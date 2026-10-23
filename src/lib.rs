//! Chanto — a fast, local-first agentic coding CLI.

// Tests serialize on a process-wide std Mutex (so they can mutate `$HOME`) and
// build fixtures with `Default::default()` then set fields; both are fine there.
#![cfg_attr(
    test,
    allow(clippy::await_holding_lock, clippy::field_reassign_with_default)
)]

#[macro_use]
mod site;
mod a11y;
mod acp;
mod acp_client;
pub mod agent;
mod auth;
mod awake;
mod bundled;
mod chanto_cloud;
mod cloud;
mod commands;
mod compat;
pub mod config;
mod context;
mod context_cost;
mod cost;
mod cron;
mod customize;
mod detect;
mod doctor;
mod feedback;
mod format;
mod github;
mod goal;
mod guardrails;
mod history;
mod hooks;
mod http;
mod ide;
mod insights;
mod launch;
mod learn;
pub mod llm;
mod loopwatch;
mod lsp;
mod mcp;
mod mcp_auth;
mod mcp_cli;
mod mcp_policy;
mod mcp_server;
mod memory;
mod migrate;
mod network;
mod onboarding;
mod outcome;
pub mod permissions;
mod plain;
mod plugin;
mod presence;
mod primer;
mod provenance;
mod providers;
mod remote;
mod remote_crypto;
mod repl;
mod resources;
mod risk;
mod runner;
mod runs;
mod secrets;
mod server;
pub mod session;
mod session_cmds;
mod share;
mod shell;
mod spend;
mod structured;
mod team;
pub mod tools;
mod tui;
mod ui;
mod update;
mod update_check;
mod util;
mod voice;
#[cfg(windows)]
mod windows_sandbox;
mod worktree;

use agent::{Agent, Env, EnvInit, Frontend, TurnEnd};
use anyhow::{Result, bail};
use clap::Parser;
use config::Config;
use llm::{Provider, ProviderKind};
use permissions::Mode;
use std::sync::Arc;

const COMMANDS_HELP: &str = concat!(
    "\
Commands:
  setup [--model <M>]           Pick a default model
  doctor [--json]               Check the setup; show where each setting comes from
  models [<provider>]           List available models
  providers                     List providers and whether you're logged in
  login | logout <provider>     Store or remove credentials (`login cloud`: chanto cloud)
  exec <prompt>                 Run non-interactively (same as -p)
  sessions                      List saved sessions as a tree
  share [<id>] [--gist]         Export a session as self-contained HTML
  import-session <file>         Import a shared session
  memory [<command>]            Show or edit durable memory
  mcp add | list | get | remove Manage MCP servers
  mcp-serve                     Serve chanto to other agents over MCP (stdio)
  acp                           Agent Client Protocol server (Zed, JetBrains)
  acp-client <command...>       Drive an external ACP agent (Claude Code, OpenCode, …)
  plugin <command>              Install and manage plugins and marketplaces
  customize <request>           Have chanto write a command, skill, hook or agent
  check-extensions              Validate custom commands, skills, hooks and agents
  goal <objective>              Keep working until acceptance criteria pass
  team <task>                   Split a task across parallel agents in worktrees
  run --bg <task>               Run a task in the background in its own worktree
  runs | attach <id>            List background runs, or follow one
  run --cloud <task>            Run a task on chanto cloud; runs --cloud lists them
  cloud login | status | logout Sign in to chanto cloud (its models: chanto/<model>)
  agents <command>              chanto cloud agents: list, create, edit, run --wait
  cloud run --agent <name>      Start an agent's run (CI: CHANTO_CLOUD_TOKEN)
  daemon start | stop | status  The local daemon that hosts background runs
  remote add | list | rm        Servers that run background tasks (--on <server>)
  runner [list]                 Scheduled and triggered jobs (.chanto/runner.toml)
  serve | web                   HTTP API, or the browser UI
  github install                Set up the GitHub review bot in this repository
  insights | logs [--tail]      Usage report, or the diagnostic log
  feedback <message>            Send feedback or a bug report (shows what is sent first)
  upgrade [--check]             Update chanto

Most commands accept --help. Docs: ",
    site_url!(),
    "/docs"
);

#[derive(Parser, Debug, Clone)]
#[command(name = "chanto", version, about = "Fast, verification-first coding agent for local and hosted models", after_help = COMMANDS_HELP)]
pub struct Cli {
    /// Initial prompt (interactive) or the task (with -p)
    pub prompt: Vec<String>,
    /// Non-interactive: run the prompt, print the result and exit
    #[arg(short, long)]
    pub print: bool,
    #[arg(skip)]
    pub customizing: bool,
    /// Set by `acp-client`: the program to drive over ACP.
    #[arg(skip)]
    pub acp_command: Option<Vec<String>>,
    /// Run the Setup hooks (trigger `init`) and SessionStart hooks, then exit
    #[arg(long, conflicts_with_all = ["init", "maintenance"])]
    pub init_only: bool,
    /// With -p: run the Setup hooks (trigger `init`) before the prompt
    #[arg(long, conflicts_with = "maintenance")]
    pub init: bool,
    /// With -p: run the Setup hooks (trigger `maintenance`) before the prompt
    #[arg(long)]
    pub maintenance: bool,
    /// Model to use, as `model` or `provider/model` (e.g. anthropic/claude-opus-5-5)
    #[arg(short, long)]
    pub model: Option<String>,
    /// Models to switch to when the model is overloaded or unavailable, tried
    /// in order for the rest of that turn (comma-separated specs; overrides `fallback_model`)
    #[arg(long, value_name = "SPECS")]
    pub fallback_model: Option<String>,
    /// Wire protocol for --base-url: ollama | openai | openai-responses | anthropic | bedrock | vertex | azure | foundry
    #[arg(long)]
    pub provider: Option<String>,
    /// Endpoint for the model server (e.g. http://localhost:8080/v1)
    #[arg(long)]
    pub base_url: Option<String>,
    /// default | acceptEdits | plan | auto | bypassPermissions
    /// (`--permission-mode` is accepted as an alias for Claude Code scripts)
    #[arg(long, visible_alias = "permission-mode")]
    pub mode: Option<String>,
    /// Shorthand for --mode bypassPermissions
    #[arg(
        long,
        alias = "yolo",
        alias = "dangerously-bypass-approvals-and-sandbox"
    )]
    pub dangerously_skip_permissions: bool,
    /// Verified mode: the agent can't finish until the project's checks pass and existing tests are intact
    #[arg(long)]
    pub verify: bool,
    /// Verified mode, checking with the full test suite rather than the tests related to the change
    #[arg(long)]
    pub verify_full: bool,
    /// Outcome mode for this prompt (-p): write acceptance criteria, build until they pass, print the evidence report
    #[arg(long)]
    pub outcome: bool,
    /// Quiet mode (experimental): the model doesn't narrate between tool calls
    /// and ends with a short summary; the TUI shows only the status line while
    /// it works, and -p text prints only the summary. Auto mode unless --mode is given
    #[arg(long)]
    pub quiet: bool,
    /// Don't add learned lessons to the prompt (e.g. to A/B them in benchmarks)
    #[arg(long)]
    pub no_lessons: bool,
    /// Codex-style shorthand: auto mode with the Bash sandbox on
    #[arg(long)]
    pub full_auto: bool,
    /// Codex-style sandbox policy: read-only (persistent) | workspace-write (sandboxed Bash) | danger-full-access
    #[arg(short = 's', long, value_name = "POLICY")]
    pub sandbox: Option<String>,
    /// Codex-style alias for --output-format stream-json in print mode
    #[arg(long, hide = true)]
    pub json: bool,
    /// Continue the most recent session in this directory
    #[arg(short, long)]
    pub r#continue: bool,
    /// Resume a session by id (without an id: pick from a list)
    #[arg(short, long, num_args = 0..=1, default_missing_value = "")]
    pub resume: Option<String>,
    /// Resume the session that opened this pull request (number or URL)
    #[arg(long, value_name = "PR")]
    pub from_pr: Option<String>,
    /// Fork the resumed session before continuing
    #[arg(long, requires = "resume")]
    pub fork_session: bool,
    /// text | json | stream-json (print mode)
    #[arg(long, default_value = "text", value_parser = ["text", "json", "stream-json"])]
    pub output_format: String,
    /// text | stream-json: with stream-json (needs -p and --output-format stream-json),
    /// stdin carries one JSON user message per line, each run as a turn of one session
    #[arg(long, default_value = "text", value_parser = ["text", "stream-json"])]
    pub input_format: String,
    /// Show the full transcript in print mode
    #[arg(short, long)]
    pub verbose: bool,
    /// Context window size to request
    #[arg(long)]
    pub num_ctx: Option<u32>,
    /// Disable model reasoning
    #[arg(long)]
    pub no_think: bool,
    /// Reasoning effort: low | medium | high | xhigh | max
    #[arg(long, value_name = "LEVEL", value_parser = clap::builder::PossibleValuesParser::new(llm::EFFORTS), conflicts_with = "no_think")]
    pub effort: Option<String>,
    /// Use the simple line-based interface instead of the full-screen TUI
    #[arg(long)]
    pub plain: bool,
    /// Screen-reader mode: line-based output with no in-place redraws, an
    /// audible cue when a prompt waits, and no animation (also `screen_reader`)
    #[arg(long)]
    pub ax_screen_reader: bool,
    /// Turn off TUI animation (spinner, live counters); also `animations = false`
    #[arg(long)]
    pub no_animations: bool,
    /// Work in a new git worktree and branch (`chanto/<NAME>`), leaving your
    /// checkout untouched; for parallel sessions on the same code
    #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
    pub worktree: Option<String>,
    /// Draw inline instead of full screen: finished output goes to the terminal's
    /// scrollback, so its own scrolling, search and copy work (also `tui = "inline"`)
    #[arg(long, conflicts_with = "plain")]
    pub inline: bool,
    /// Session budget in USD
    #[arg(long)]
    pub max_cost: Option<f64>,
    /// Mirror structured diagnostics to stderr
    #[arg(long)]
    pub debug: bool,
    /// Comma-separated tool allowlist (e.g. "Read,Grep,Glob")
    #[arg(long)]
    pub allowed_tools: Option<String>,
    /// Comma-separated tools to disable
    #[arg(long)]
    pub disallowed_tools: Option<String>,
    /// Stop after this many model turns
    #[arg(long)]
    pub max_turns: Option<u32>,
    /// Extra text appended to the system prompt
    #[arg(long)]
    pub append_system_prompt: Option<String>,
    /// Working directory
    #[arg(long, alias = "cd", short = 'C')]
    pub cwd: Option<std::path::PathBuf>,
    /// Additional directories the agent may work in (repeatable)
    #[arg(long = "add-dir", value_name = "DIR")]
    pub add_dir: Vec<String>,
    /// Print an example config file and exit
    #[arg(long)]
    pub example_config: bool,
    /// Customize the Chanto source checkout (with `customize`)
    #[arg(long)]
    pub source: bool,
    /// Trust this folder's project settings (hooks, MCP servers, permissions) and remember it
    #[arg(long)]
    pub trust: bool,
    /// Print shell completions (bash, zsh, fish, elvish, powershell) and exit
    #[arg(long, value_name = "SHELL")]
    pub completions: Option<clap_complete::Shell>,
    /// Debug: run a single tool directly, e.g. --run-tool WebSearch '{"query":"rust"}'
    #[arg(long, hide = true, num_args = 2, value_names = ["TOOL", "JSON"])]
    pub run_tool: Option<Vec<String>>,
    /// Use a named [profiles.<name>] section of the config (like Codex's --profile)
    #[arg(long)]
    pub profile: Option<String>,
    /// Debug: show the auto-mode fast path's features and verdict for a command
    #[arg(long, hide = true)]
    pub risk: Option<String>,
    /// Train the auto-mode fast path: IN.jsonl OUT.json (see classifier/README.md)
    #[arg(long, hide = true, num_args = 2, value_names = ["IN", "OUT"])]
    pub train_risk: Option<Vec<String>>,
    /// Evaluate the auto-mode fast path on a JSONL file of {"cmd", "risky"?}
    /// examples; with OUT, also write each verdict
    #[arg(long, hide = true, num_args = 1..=2, value_names = ["IN", "OUT"])]
    pub eval_risk: Option<Vec<String>>,
    /// Debug: triage a JSON array of commands through auto mode's static checks
    /// and fast path; prints counts and writes the remaining ones to OUT
    #[arg(long, hide = true, num_args = 2, value_names = ["IN", "OUT"])]
    pub triage_commands: Option<Vec<String>>,
    /// Debug: ask the auto-mode classifier about an action: TOOL JSON USER_REQUEST
    #[arg(long, hide = true, num_args = 3, value_names = ["TOOL", "JSON", "REQUEST"])]
    pub classify: Option<Vec<String>>,
    /// Debug: print the system prompt and tool schemas with size estimates
    #[arg(long, hide = true)]
    pub dump_prompt: bool,
    /// `chanto serve` / `chanto web`: port to listen on (0 = random)
    #[arg(long, default_value_t = 4141)]
    pub port: u16,
    /// `chanto serve` / `chanto web`: address to bind (keep it local unless you know what you're doing)
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    /// `chanto serve` / `chanto web`: bind to this machine's Tailscale address so
    /// your other devices can reach it, and nothing on the LAN can (overrides --host)
    #[arg(long)]
    pub tailscale: bool,
    /// `chanto web`: do not open the browser
    #[arg(long)]
    pub no_open: bool,
    /// Run the task in the background (`chanto run --bg "<task>"`): in its own
    /// git worktree, hosted by the local daemon; returns at once with the run id
    #[arg(long)]
    pub bg: bool,
    /// Background runs: run on this server (added with `chanto remote add`)
    #[arg(long, value_name = "SERVER")]
    pub on: Option<String>,
    /// Background runs: turn verified mode off for this run
    #[arg(long)]
    pub no_verify: bool,
    /// `chanto goal`: stop after this many work iterations (default from `[goal]`, 20)
    #[arg(long, value_name = "N")]
    pub max_iterations: Option<u32>,
    /// A number: `chanto team`'s sub-task agents that run at once (default from
    /// `[team]`, 3). JSON (or a JSON file): subagent definitions for this session,
    /// as in Claude Code: '{"reviewer":{"description":"…","prompt":"…","tools":["Read"]}}'
    #[arg(long, value_name = "N|JSON")]
    pub agents: Option<String>,
    /// Run the main conversation as this subagent (its prompt, tools and model)
    #[arg(long, value_name = "NAME")]
    pub agent: Option<String>,
    /// Replace Chanto's system prompt (tools, environment and instruction files are still added)
    #[arg(long, value_name = "TEXT", conflicts_with = "system_prompt_file")]
    pub system_prompt: Option<String>,
    /// Replace Chanto's system prompt with a file's contents
    #[arg(long, value_name = "PATH")]
    pub system_prompt_file: Option<String>,
    /// Append a file's contents to the system prompt
    #[arg(long, value_name = "PATH")]
    pub append_system_prompt_file: Option<String>,
    /// Load MCP servers from a JSON file or string ({"mcpServers":{…}}); repeat for more
    #[arg(long, value_name = "FILE|JSON")]
    pub mcp_config: Vec<String>,
    /// Only use the servers from --mcp-config, ignoring every other MCP configuration
    #[arg(long)]
    pub strict_mcp_config: bool,
    /// Settings for this session: a JSON file or string (Claude Code settings.json
    /// format) or a Chanto .toml file; overrides the same keys in your config files
    #[arg(long, value_name = "FILE|JSON")]
    pub settings: Option<String>,
    /// Which config files to read: comma-separated user, project, local
    #[arg(long, value_name = "SOURCES")]
    pub setting_sources: Option<String>,
    /// Use this session id (a UUID) for a new session
    #[arg(long, value_name = "UUID")]
    pub session_id: Option<String>,
    /// Name the session (shown in the session list; `--resume <name>` finds it)
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: Option<String>,
    /// Don't save the session to disk (it can't be resumed)
    #[arg(long)]
    pub no_session_persistence: bool,
    /// Print mode: finish with JSON matching this JSON Schema (`structured_output`
    /// in json output); the answer is checked, and repaired once if it doesn't match
    #[arg(long, value_name = "SCHEMA")]
    pub json_schema: Option<String>,
    /// Restrict the built-in tools: "Bash,Edit,Read", "" for none, "default" for all
    #[arg(long, value_name = "TOOLS")]
    pub tools: Option<String>,
    /// Load a plugin (or a folder of plugins) for this session only; repeatable
    #[arg(long, value_name = "PATH")]
    pub plugin_dir: Vec<String>,
    /// Minimal mode: skip auto-discovery of hooks, skills, commands, subagents,
    /// plugins, MCP servers, memory and instruction files; tools Bash, Read, Edit
    #[arg(long)]
    pub bare: bool,
    /// Troubleshooting start with every customization off (instructions,
    /// skills, plugins, hooks, MCP servers, commands, agents, output styles,
    /// custom themes and keybindings, status line, language servers, memory);
    /// model, tools and permissions work as usual. Also CLAUDE_CODE_SAFE_MODE=1
    #[arg(long)]
    pub safe_mode: bool,
    /// No tools that run commands or code (nor WebFetch) unless --tools names
    /// them, file tools kept to the working directories, only managed settings
    /// and --settings, no bypass mode, no cloud runs. Also CLAUDE_CODE_RESTRICTED=1
    #[arg(long)]
    pub restricted: bool,
    /// Accepted for Claude Code scripts: Chanto's stream-json output already
    /// streams text as it arrives
    #[arg(long)]
    pub include_partial_messages: bool,
    /// Print mode: an MCP tool (mcp__server__tool) that answers permission prompts
    #[arg(long, value_name = "TOOL")]
    pub permission_prompt_tool: Option<String>,
    /// `chanto serve`: host background runs for clients beyond this machine
    /// (servers bound to loopback always host them)
    #[arg(long)]
    pub runs: bool,
    /// Skip the interactive folder-trust prompt (server sessions)
    #[arg(skip)]
    pub no_trust_prompt: bool,
    /// Offer the Computer tool (screen, mouse and keyboard; each action asks first)
    #[arg(long)]
    pub computer: bool,
    /// A background run's worker (computer use only with `[computer] background`)
    #[arg(skip)]
    pub background_run: bool,
    /// Debug: print the effective configuration (after importing Claude Code / Codex settings)
    #[arg(long, hide = true)]
    pub dump_config: bool,
}

impl Cli {
    /// `--agents N`: `chanto team`'s parallelism (JSON there means subagents).
    pub fn team_size(&self) -> Option<usize> {
        match self.agents.as_deref().map(launch::parse_agents) {
            Some(Ok(launch::AgentsFlag::Team(n))) => Some(n),
            _ => None,
        }
    }

    /// The process-wide launch flags (`--settings`, `--mcp-config`, `--bare`, …),
    /// checked before anything starts.
    fn launch(&self) -> Result<launch::Launch> {
        let mut l = launch::Launch {
            strict_mcp: self.strict_mcp_config,
            bare: self.bare,
            safe_mode: self.safe_mode || launch::env_flag("CLAUDE_CODE_SAFE_MODE"),
            restricted: self.restricted || launch::env_flag("CLAUDE_CODE_RESTRICTED"),
            add_dirs: self.add_dir.clone(),
            ..Default::default()
        };
        if let Some(s) = &self.settings {
            l.settings.push(launch::parse_settings(s)?);
        }
        if let Some(s) = &self.setting_sources {
            l.sources = Some(launch::parse_sources(s)?);
        }
        // `--restricted` reads no user, project or local settings files.
        if l.restricted {
            l.sources = Some(Vec::new());
        }
        if l.restricted && self.dangerously_skip_permissions {
            bail!("--restricted refuses bypass permissions mode (--dangerously-skip-permissions)");
        }
        for m in &self.mcp_config {
            l.mcp.push(launch::parse_mcp_config(m)?);
        }
        for d in &self.plugin_dir {
            l.plugins.extend(launch::parse_plugin_dir(d)?);
        }
        if let Some(t) = &self.permission_prompt_tool {
            if !t.starts_with("mcp__") {
                bail!(
                    "--permission-prompt-tool takes an MCP tool name (mcp__<server>__<tool>), got {t:?}"
                );
            }
            l.permission_prompt_tool = Some(t.clone());
        }
        if let Some(a) = &self.agents {
            launch::parse_agents(a)?;
        }
        if let Some(id) = &self.session_id {
            launch::parse_session_id(id)?;
        }
        if let Some(s) = &self.json_schema {
            let v: serde_json::Value = serde_json::from_str(s)
                .map_err(|e| anyhow::anyhow!("--json-schema is not a valid JSON Schema: {e}"))?;
            structured::check_schema(&v)
                .map_err(|e| anyhow::anyhow!("--json-schema is not a valid JSON Schema: {e}"))?;
        }
        Ok(l)
    }

    /// The Setup hooks' trigger this run asked for, if any.
    fn setup_trigger(&self) -> Option<&'static str> {
        if self.init_only || self.init {
            Some("init")
        } else if self.maintenance {
            Some("maintenance")
        } else {
            None
        }
    }
}

pub struct Setup {
    pub env: Arc<Env>,
    pub agent: Agent,
    pub warnings: Vec<String>,
}

pub async fn setup(cli: &Cli, frontend: Arc<dyn Frontend>, interactive: bool) -> Result<Setup> {
    if let Some(p) = &cli.cwd {
        std::env::set_current_dir(p).map_err(|e| anyhow::anyhow!("--cwd {}: {e}", p.display()))?;
    }
    setup_session(
        cli,
        frontend,
        interactive,
        std::env::current_dir()?,
        vec![],
        None,
    )
    .await
}

/// The permission mode a session starts in, from the command line and the
/// config's `mode`. `--quiet` presets auto mode (few interruptions, with auto
/// mode's safety: risky actions still ask, and the sandbox still applies);
/// any explicit mode wins over it.
fn start_mode(cli: &Cli, configured: Option<Mode>) -> Result<Mode> {
    Ok(if cli.dangerously_skip_permissions {
        Mode::Bypass
    } else if cli.sandbox.as_deref() == Some("read-only") {
        Mode::Plan
    } else if cli.full_auto {
        Mode::Auto
    } else if let Some(m) = &cli.mode {
        Mode::parse(m).ok_or_else(|| anyhow::anyhow!("unknown mode {m}"))?
    } else if cli.quiet {
        Mode::Auto
    } else {
        configured.unwrap_or_default()
    })
}

async fn setup_session(
    cli: &Cli,
    frontend: Arc<dyn Frontend>,
    interactive: bool,
    cwd: std::path::PathBuf,
    remote_tools: Vec<Arc<dyn tools::Tool>>,
    verify: Option<bool>,
) -> Result<Setup> {
    if cli.trust {
        config::trust(&cwd)?;
    }
    // Let other Chanto sessions in this project know this one is working here.
    presence::start(&cwd);
    let mut warnings = Vec::new();
    let (mut cfg, skipped) = Config::load(&cwd, config::is_trusted(&cwd))?;
    crate::util::timing("config loaded");
    if !skipped.is_empty() {
        let ask = !cli.no_trust_prompt
            && interactive
            && std::env::var_os("CI").is_none()
            && ui::is_tty()
            && atty_stdin();
        let mut accepted = false;
        if ask {
            let q = format!(
                "\nThis folder has project settings that can run commands or change permissions:\n  - {}\n\nOnly trust folders whose contents you trust. Trust {}? [y/N] ",
                skipped.join("\n  - "),
                cwd.display()
            );
            eprintln!("{}", ui::word_wrap(&q, ui::width().saturating_sub(1)));
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            accepted = matches!(line.trim(), "y" | "Y" | "yes");
        }
        if accepted {
            config::trust(&cwd)?;
            cfg = Config::load(&cwd, true)?.0;
        } else {
            warnings.push(format!(
                "Ignored untrusted project settings: {}. Run `chanto --trust` here to enable them.",
                skipped.join(", ")
            ));
        }
    }
    // The skipped settings were just reported in one line; `doctor` lists them one by one.
    warnings.extend(
        cfg.diagnostics
            .iter()
            .filter(|d| !d.starts_with("Untrusted: "))
            .cloned(),
    );
    // Resolve accessibility before the onboarding picker or any spinner runs, so
    // screen-reader mode reaches the frontend choice below. A flag beats config.
    let (screen_reader, animations) = (
        cli.ax_screen_reader.then_some(true).or(cfg.screen_reader),
        cli.no_animations.then_some(false).or(cfg.animations),
    );
    crate::a11y::init(screen_reader, animations);
    if let Some(p) = &cli.provider {
        cfg.provider = ProviderKind::parse(p).ok_or_else(|| anyhow::anyhow!("unknown provider {p} (ollama | openai | anthropic | openai-responses | bedrock | vertex | azure | foundry)"))?;
    }
    if let Some(u) = &cli.base_url {
        cfg.base_url = Some(u.clone());
    }
    if query_explicit_models(
        cli.base_url.is_some() || std::env::var_os("CHANTO_BASE_URL").is_some(),
        cli.model.is_some() || std::env::var_os("CHANTO_MODEL").is_some(),
    ) {
        cfg.model = None;
    }
    let explicit_connection = cli.provider.is_some()
        || cli.base_url.is_some()
        || std::env::var_os("CHANTO_PROVIDER").is_some()
        || std::env::var_os("CHANTO_BASE_URL").is_some();
    if cli.model.is_none()
        && cfg.model.is_none()
        && !cli.dump_prompt
        && cli.run_tool.is_none()
        && cli.classify.is_none()
    {
        if explicit_connection {
            let url = cfg.base_url();
            let kind = if cfg.provider == ProviderKind::Ollama {
                "ollama"
            } else {
                "openai"
            };
            let mut candidates = detect::probe(kind, &url).await;
            // An explicit URL can speak either local protocol.
            if candidates.is_empty()
                && cli.provider.is_none()
                && std::env::var_os("CHANTO_PROVIDER").is_none()
                && (cli.base_url.is_some() || std::env::var_os("CHANTO_BASE_URL").is_some())
            {
                let other = if kind == "ollama" { "openai" } else { "ollama" };
                candidates = detect::probe(other, &url).await;
                if !candidates.is_empty() {
                    cfg.provider = if other == "ollama" {
                        ProviderKind::Ollama
                    } else {
                        ProviderKind::Openai
                    };
                }
            }
            if cfg.provider == ProviderKind::Openai && !url.trim_end_matches('/').ends_with("/v1") {
                cfg.base_url = Some(format!("{}/v1", url.trim_end_matches('/')));
            }
            cfg.model = Some(
                candidates
                    .first()
                    .and_then(|c| c.model.clone())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "No usable models returned by {url}; specify --model <model>"
                        )
                    })?,
            );
        } else if interactive
            && !cli.print
            && std::env::var_os("CI").is_none()
            && ui::is_tty()
            && atty_stdin()
        {
            onboarding::picker(cli.plain).await?;
            cfg = Config::load(&cwd, config::is_trusted(&cwd))?.0;
        } else {
            let candidates = detect::detect().await;
            let chosen = candidates
                .iter()
                .find(|c| c.model.is_some())
                .ok_or_else(|| anyhow::anyhow!("{}", detect::no_candidate(&candidates)))?;
            detect::apply(&mut cfg, chosen);
            eprintln!("{}", detect::selection_notice(chosen));
        }
    }
    if let Some(n) = cli.num_ctx {
        cfg.num_ctx = Some(n);
    }
    if cli.no_think {
        cfg.think = Some(false);
    }
    if let Some(e) = &cli.effort {
        cfg.effort = Some(e.clone());
        cfg.think = Some(true);
    }
    if let Some(m) = cli.max_cost {
        cfg.max_cost_usd = Some(m);
    }
    crate::cost::set_debug(cli.debug);
    if let Some(m) = cli.max_turns {
        cfg.max_turns = Some(m);
    }
    if cfg.api_key.is_none()
        && let Some(helper) = cfg.api_key_helper.clone()
    {
        // Claude Code's apiKeyHelper: a command that prints the key.
        match crate::shell::std_command(&helper).output() {
            Ok(o) if o.status.success() && !o.stdout.is_empty() => {
                cfg.api_key = Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            }
            Ok(o) => warnings.push(format!(
                "api_key_helper failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            Err(e) => warnings.push(format!("api_key_helper failed: {e}")),
        }
    }
    crate::util::timing("connection resolved");
    context::prefetch_git(&cwd);
    // MCP servers connect while the rest of startup runs.
    let mcp_task = {
        let servers = cfg.mcp_servers.clone();
        tokio::spawn(async move { mcp::connect_all(&servers).await })
    };
    // Language-server detection may start a process (rustup); run it alongside.
    lsp::set_trusted(config::is_trusted(&cwd));
    let lsp_task = {
        let (l, c) = (cfg.lsp.clone(), cwd.clone());
        tokio::task::spawn_blocking(move || lsp::available(&l, &c))
    };

    if cli.print || interactive {
        update_check::start(&cfg.updates, interactive && !cli.print);
        cost::start_price_refresh(cfg.updates.check, interactive && !cli.print);
    }
    match cfg
        .keep_awake
        .as_deref()
        .map(|v| (v, awake::Policy::parse(v)))
    {
        None => awake::init(awake::Policy::Working),
        Some((_, Some(p))) => awake::init(p),
        Some((v, None)) => ui::warn(&format!(
            "keep_awake = \"{v}\" isn't one of working, never, always; using working"
        )),
    }
    feedback::init(
        &cfg,
        interactive
            && !cli.print
            && std::env::var_os("CI").is_none()
            && ui::is_tty()
            && atty_stdin(),
    );
    let provider = Provider::new(
        cfg.provider,
        &cfg.base_url(),
        cfg.api_key.clone(),
        cfg.num_ctx(),
    );
    let model = cli
        .model
        .clone()
        .or(cfg.model.clone())
        .unwrap_or_else(|| "qwen3-coder".into());
    for (set, key) in [
        (cli.model.is_some(), "model"),
        (cli.provider.is_some(), "provider"),
        (cli.base_url.is_some(), "base_url"),
    ] {
        if set {
            cfg.origins
                .insert(key.into(), std::path::PathBuf::from("CLI"));
        }
    }
    cfg.model = Some(model.clone());
    if let Some(f) = &cli.fallback_model {
        cfg.fallback_model = Some(config::ModelList::One(f.clone()));
        cfg.origins
            .insert("fallback_model".into(), std::path::PathBuf::from("CLI"));
    }
    // `--base-url` sets the default server; a `name/model` spec for a named or
    // built-in provider goes to that provider's own endpoint instead, so the
    // URL (and the request's key) would silently go elsewhere.
    if let (Some(url), Some((prefix, _))) = (cli.base_url.as_deref(), model.split_once('/')) {
        let named = cfg.providers.contains_key(prefix)
            || prefix == crate::chanto_cloud::PROVIDER_ID
            || (crate::providers::find(prefix).is_some()
                && (cfg.allows_builtin_hosted() || !matches!(prefix, "openai" | "anthropic")));
        if named {
            bail!(
                "--base-url {url} doesn't apply to {model}: `{prefix}/` models use the {prefix} provider's own endpoint. Drop the `{prefix}/` prefix to send the model name to {url} (add --provider openai for an OpenAI-compatible server), or drop --base-url."
            );
        }
    }
    if let Some(why) = detect::missing_login(&cfg, &model) {
        // One-shot runs can't continue; interactive ones can still pick another model.
        if !interactive {
            bail!("{why}");
        }
        warnings.push(format!("{why} Use /model to pick another."));
    }
    // Which provider actually serves this model: an entry under `providers`, a
    // built-in catalog provider, or the default. Its kind decides whether the
    // server can report image support.
    let serving_kind = match model.split_once('/') {
        Some((p, m)) => cfg
            .providers
            .get(p)
            .map(|pc| pc.kind)
            .or(match p {
                "ollama" => Some(ProviderKind::Ollama),
                "lmstudio" | "llamacpp" => Some(ProviderKind::Openai),
                _ => None,
            })
            .or_else(|| {
                cfg.allows_builtin_hosted()
                    .then(|| llm::builtin_provider(p, m))
                    .flatten()
                    .map(|b| b.kind)
            })
            .unwrap_or(cfg.provider),
        None => cfg.provider,
    };
    // Ollama's `/api/show` advertises a `vision` capability; OpenAI-compatible
    // servers (DeepSeek, OpenRouter, llama.cpp, vLLM, …) don't, so assume images
    // are supported and let the endpoint reject them rather than silently
    // dropping a screenshot the user just pasted.
    let mut vision = serving_kind.assumes_vision();
    if cfg.provider.is_hosted() && cfg.num_ctx.is_none() {
        cfg.num_ctx = Some(llm::hosted_context(cfg.provider, &model));
    }
    crate::util::timing("before model_info");
    // `--run-tool` and `--dump-prompt` exit without a model turn, and model_info
    // only feeds `vision`/`num_ctx` (and dump-prompt's output is independent of
    // both). Skip its network round-trip so these paths stay fast — a hosted
    // provider's model_info otherwise blocks startup ~600ms fetching `/models`.
    let info = if cli.run_tool.is_some() || cli.dump_prompt {
        None
    } else {
        match model.split_once('/') {
            Some((p, m)) if cfg.providers.contains_key(p) => {
                cfg.providers[p].provider(cfg.num_ctx()).model_info(m).await
            }
            // A built-in hosted provider manages its own context budget; just read vision support.
            Some((p, m))
                if cfg.allows_builtin_hosted() && llm::builtin_provider(p, m).is_some() =>
            {
                llm::builtin_provider(p, m)
                    .unwrap()
                    .model_info(m)
                    .await
                    .map(|(_, v)| (0, v))
            }
            Some(("ollama" | "lmstudio" | "llamacpp", _)) => {
                let (p, m, _) = doctor::resolve(&cfg, &model);
                p.model_info(&m).await
            }
            _ => provider.model_info(&model).await,
        }
    };
    crate::util::timing("model_info done");
    if let Some((ctx_len, v)) = info {
        vision = v;
        // An OpenAI-compatible server that reports its window gets all of it
        // (it only sets when to compact). Ollama keeps the default: there,
        // num_ctx is memory the server allocates.
        if cfg.num_ctx.is_none()
            && serving_kind == ProviderKind::Openai
            && ctx_len > cfg.num_ctx() as u64
        {
            cfg.num_ctx = Some(ctx_len.min(u32::MAX as u64) as u32);
        }
        if ctx_len > 0 && (cfg.num_ctx() as u64) > ctx_len {
            warnings.push(format!(
                "num_ctx {} exceeds the model's trained context {}; clamping",
                cfg.num_ctx(),
                ctx_len
            ));
            cfg.num_ctx = Some(ctx_len as u32);
        }
    }
    let provider = Provider::new(
        cfg.provider,
        &cfg.base_url(),
        cfg.api_key.clone(),
        cfg.num_ctx(),
    );
    cfg.permissions
        .additional_directories
        .extend(cli.add_dir.iter().cloned());
    if cli.verify || cli.verify_full {
        cfg.guardrails.enabled = true;
    }
    if cli.verify_full {
        cfg.guardrails.scope = Some(crate::guardrails::Scope::Full);
    }
    if cli.quiet {
        cfg.optimize.quiet = Some(true);
    }
    if let Some(enabled) = verify {
        cfg.guardrails.enabled = enabled;
    }
    // A cloud run told it has no checks has nothing to verify against.
    if std::env::var(chanto_proto::runs::CHECKS_ENV).is_ok_and(|v| v.trim().is_empty()) {
        cfg.guardrails.enabled = false;
    }
    if cli.no_lessons || launch::get().no_customizations() {
        cfg.learning.lessons = Some(false);
    }
    if launch::get().safe_mode {
        // Customizations that aren't discovery: they come from your settings.
        cfg.status_line = None;
        cfg.output_style = None;
        cfg.lsp.enabled = Some(false);
        if cfg.theme.as_deref().is_some_and(|t| {
            crate::tui::theme::available()
                .iter()
                .any(|(n, user)| *user && n == t)
        }) {
            cfg.theme = None;
        }
    }
    if launch::get().restricted {
        cfg.policy_no_bypass = true;
        if cfg.mode == Some(Mode::Bypass) {
            cfg.mode = None;
        }
    }
    if let Some(policy) = cli.sandbox.as_deref() {
        cfg.sandbox.set_policy(policy)?;
    }
    if cli.full_auto {
        cfg.sandbox.enabled = true;
        cfg.sandbox.auto_allow = true;
    }
    let mode = start_mode(cli, cfg.mode)?;
    // Organization policy (Claude Code managed settings) outranks the command line.
    let mode = match cfg.policy_mode {
        Some(p) if p != mode => {
            warnings.push(format!(
                "Your organization's managed settings start sessions in {} mode.",
                p.label()
            ));
            p
        }
        _ => mode,
    };
    let mode = if cfg.policy_no_bypass && mode == Mode::Bypass {
        warnings.push("Bypass permissions mode is disabled by your organization's managed settings; starting in default mode.".into());
        Mode::Default
    } else {
        mode
    };
    cfg.mode = Some(mode);
    crate::tools::bash::prime_sandbox_caches(&cfg.sandbox);
    for (key, overridden) in [
        ("model", cli.model.is_some()),
        ("provider", cli.provider.is_some()),
        ("base_url", cli.base_url.is_some()),
        (
            "mode",
            cli.mode.is_some()
                || cli.quiet
                || cli.full_auto
                || cli.dangerously_skip_permissions
                || cli.sandbox.as_deref() == Some("read-only"),
        ),
        ("sandbox", cli.sandbox.is_some() || cli.full_auto),
    ] {
        if overridden {
            cfg.origins
                .insert(key.into(), std::path::PathBuf::from("CLI"));
        }
    }
    for ev in cfg
        .hooks
        .keys()
        .filter(|k| !config::HOOK_EVENTS.contains(&k.as_str()))
    {
        warnings.push(format!(
            "Unknown hook event `{ev}` (known: {})",
            config::HOOK_EVENTS.join(", ")
        ));
    }
    for (k, v) in &cfg.env {
        // SAFETY: called during startup before any other threads read the environment.
        unsafe { std::env::set_var(k, v) };
    }
    let (mcp, errs) = mcp_task.await.unwrap_or_default();
    crate::util::timing("mcp connected");
    warnings.extend(errs);
    let mut tools = tools::Registry::builtin();
    // In-session scheduling, where a frontend fires the tasks (TUI, REPL).
    if crate::cron::available() {
        let (sched, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut tools.hidden)
            .into_iter()
            .partition(|t| crate::cron::TOOLS.contains(&t.name().as_str()));
        tools.hidden = rest;
        tools.tools.extend(sched);
    }
    // Signed in to Chanto cloud: the agent can see and start its runs and agents.
    if tools::cloud::available() {
        tools.tools.push(Arc::new(tools::cloud::ChantoCloud));
    }
    crate::util::timing("registry");
    if cfg.apply_patch_tool.unwrap_or(false) {
        tools.show_hidden();
    }
    // MCP resource tools: advertised when a connected server has resources,
    // otherwise callable but hidden (e.g. a server logged into later).
    let resource_tools: [Arc<dyn tools::Tool>; 2] = [
        Arc::new(mcp::ListMcpResources),
        Arc::new(mcp::ReadMcpResource),
    ];
    if mcp.iter().any(|c| c.has_resources()) {
        tools.tools.extend(resource_tools);
    } else {
        tools.hidden.extend(resource_tools);
    }
    tools.tools.extend(remote_tools);
    let list = |s: &Option<String>| -> Option<Vec<String>> {
        s.as_ref().map(|s| {
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
    };
    if let Some(allow) = list(&cli.allowed_tools) {
        tools
            .tools
            .retain(|t| allow.iter().any(|a| a.eq_ignore_ascii_case(&t.name())));
        tools
            .hidden
            .retain(|t| allow.iter().any(|a| a.eq_ignore_ascii_case(&t.name())));
    }
    if let Some(deny) = list(&cli.disallowed_tools) {
        tools
            .tools
            .retain(|t| !deny.iter().any(|a| a.eq_ignore_ascii_case(&t.name())));
        // apply_patch writes files, so denying Edit or Write denies it too.
        let edits_denied = deny.iter().any(|d| d == "Edit" || d == "Write");
        tools.hidden.retain(|t| {
            !deny.iter().any(|a| a.eq_ignore_ascii_case(&t.name()))
                && !(edits_denied && t.name() == "apply_patch")
        });
    }
    lsp::set_trusted(config::is_trusted(&cwd));
    crate::util::timing("trust checked");
    secrets::init(&cfg, &cwd);
    // Do the first prompt's secret-file scan in the background.
    {
        let root = cwd.clone();
        std::thread::spawn(move || secrets::warm(&root));
    }
    crate::util::timing("secrets init");
    permissions::PROTECT_SECRET_FILES.store(
        cfg.secrets.protect_files,
        std::sync::atomic::Ordering::Relaxed,
    );
    let lsp_ok = lsp_task.await.unwrap_or(false);
    crate::util::timing("lsp checked");
    if lsp_ok {
        tools.tools.push(Arc::new(tools::lsp::Lsp));
    }
    // Computer use: only when asked for, and in background runs only when
    // configured for them too.
    let computer = (cli.computer || cfg.computer.enabled == Some(true))
        && (!cli.background_run || cfg.computer.background == Some(true));
    if computer && !tools.tools.iter().any(|t| t.name() == "Computer") {
        let allowed = list(&cli.allowed_tools)
            .is_none_or(|a| a.iter().any(|t| t.eq_ignore_ascii_case("Computer")));
        let denied = list(&cli.disallowed_tools)
            .is_some_and(|d| d.iter().any(|t| t.eq_ignore_ascii_case("Computer")));
        if allowed && !denied {
            tools
                .tools
                .push(Arc::new(tools::computer::Computer::new(&cfg.computer)));
            tools::computer::ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
            warnings.push(match tools::computer::support() {
                Ok(()) => "Computer use is on: the model can ask to see your screen and use your mouse and keyboard. You approve each action (except in auto/bypass mode). Anything on screen can contain instructions meant to hijack it, so watch what it does.".into(),
                Err(e) => format!("Computer use is on but unavailable here: {e}"),
            });
        }
    }
    tools::bash::SANDBOX_ACTIVE.store(
        cfg.sandbox.enabled && tools::bash::sandbox_available(),
        std::sync::atomic::Ordering::Relaxed,
    );
    if cfg.sandbox.enabled {
        tools::bash::start_sandbox_probe();
    }
    if cfg.sandbox.enabled && !tools::bash::sandbox_available() {
        warnings.push("Shell commands aren't sandboxed on this system (chanto needs working macOS Seatbelt, Linux Landlock or a Windows restricted token). Set `[sandbox] enabled = false` to hide this.".into());
    }
    // The Windows sandbox confines writes only; say so where config expects more.
    if cfg!(windows) && cfg.sandbox.enabled && tools::bash::sandbox_available() {
        if cfg.sandbox.network_policy() == crate::config::NetworkPolicy::Deny {
            warnings.push("sandbox.network = \"deny\" isn't enforced on Windows: sandboxed commands can still reach the network.".into());
        }
        if cfg.sandbox.auto_allow {
            warnings.push("sandbox.auto_allow is on, but on Windows the sandbox only confines writes: auto-allowed commands can still read credential files and use the network.".into());
        }
    }
    if cfg.sandbox.enabled && cfg.sandbox.auto_allow && tools::bash::sandbox_available() {
        permissions::SANDBOX_AUTO_ALLOW.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    // `--tools` (and `--bare`'s or `--restricted`'s default set) limits the built-in tools; MCP tools stay.
    let keep = launch::kept_tools(
        cli.tools.as_deref(),
        cli.bare,
        launch::get().restricted,
        &tools.names(),
    );
    permissions::RESTRICTED.store(
        launch::get().restricted,
        std::sync::atomic::Ordering::Relaxed,
    );
    if let Some(keep) = keep {
        let known = tools.names();
        for name in keep
            .iter()
            .filter(|k| !known.iter().any(|n| n.eq_ignore_ascii_case(k)))
        {
            warnings.push(format!("--tools: no built-in tool named {name}"));
        }
        tools
            .tools
            .retain(|t| keep.iter().any(|k| k.eq_ignore_ascii_case(&t.name())));
        tools
            .hidden
            .retain(|t| keep.iter().any(|k| k.eq_ignore_ascii_case(&t.name())));
    }
    crate::util::timing("before Env::new");
    let env = Env::new(EnvInit {
        config: cfg,
        provider,
        model: model.clone(),
        mode,
        vision,
        cwd: cwd.clone(),
        tools,
        mcp,
        frontend,
        interactive,
    });
    *env.session_tool_policy.write().unwrap() = (
        list(&cli.allowed_tools),
        list(&cli.disallowed_tools).unwrap_or_default(),
    );
    if env.config.tool_protocol.as_deref() == Some("text") {
        env.text_tools
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    // `--agents '<json>'`: subagents for this session, replacing same-named ones.
    if let Some(launch::AgentsFlag::Defs(defs)) = cli
        .agents
        .as_deref()
        .map(launch::parse_agents)
        .transpose()?
    {
        let mut agents = env.agents.write().unwrap();
        for d in defs {
            agents.retain(|a| !a.name.eq_ignore_ascii_case(&d.name));
            agents.push(d);
        }
    }
    let append = match &cli.append_system_prompt_file {
        Some(p) => Some(
            format!(
                "{}\n\n{}",
                cli.append_system_prompt.clone().unwrap_or_default(),
                launch::read_prompt_file("--append-system-prompt-file", p)?
            )
            .trim()
            .to_string(),
        ),
        None => cli.append_system_prompt.clone(),
    };
    let base = match &cli.system_prompt_file {
        Some(p) => Some(launch::read_prompt_file("--system-prompt-file", p)?),
        None => cli.system_prompt.clone(),
    };
    let definition = match &cli.agent {
        Some(name) => Some(
            crate::util::find_named(&env.agents.read().unwrap(), name, |a| &a.name)
                .cloned()
                .ok_or_else(|| {
                    let known: Vec<String> = env
                        .agents
                        .read()
                        .unwrap()
                        .iter()
                        .map(|a| a.name.clone())
                        .collect();
                    anyhow::anyhow!(
                        "--agent: no subagent named {name} (known: {})",
                        if known.is_empty() {
                            "none".into()
                        } else {
                            known.join(", ")
                        }
                    )
                })?,
        ),
        None => None,
    };
    crate::util::timing("before Agent::new");
    let mut agent = Agent::new_with_system_extra(env.clone(), append);
    if let Some(command) = &cli.acp_command {
        agent.acp = Some(crate::acp_client::AcpAgent::new(command.clone()));
    }
    if base.is_some() {
        agent.set_system_base(base);
    }
    crate::util::timing("Agent::new done");
    if cli.customizing {
        agent.enable_customization();
    }
    // Sessions.
    //
    // `--resume` without an id (the flag's default_missing_value is "") means
    // "pick one": interactively the TUI opens a picker, but print mode has no UI,
    // so it resumes the most recent session rather than silently starting fresh.
    let resume_id: Option<String> = match cli.resume.as_deref() {
        _ if cli.from_pr.is_some() => {
            let pr = cli.from_pr.as_deref().unwrap_or_default();
            match session::Session::for_pr(&cwd, pr) {
                Some(s) => Some(s.id),
                None => bail!(
                    "No session is linked to pull request {pr}. A session is linked when a command in it prints the pull request's URL (as `gh pr create` does)."
                ),
            }
        }
        Some("") | None if cli.r#continue => {
            let (latest, unreadable) = session::Session::latest(&cwd);
            if let Some((path, why)) = unreadable.first() {
                let more = match unreadable.len() {
                    1 => String::new(),
                    n => format!(" ({} newer session files couldn't be read either)", n - 1),
                };
                warnings.push(match &latest {
                    Some(s) => format!("The latest session {} couldn't be read: {why}{more}. Resuming the newest readable one instead: {} ({}).", path.display(), s.id, crate::util::one_line(&s.title, 60)),
                    None => format!("The latest session {} couldn't be read: {why}{more}. No other session to resume; starting a new one.", path.display()),
                });
            }
            latest.map(|s| s.id)
        }
        Some("") if !interactive => session::Session::list(&cwd)
            .into_iter()
            .find(|s| !s.archived)
            .map(|s| s.id),
        Some("") => None,
        // An id, or a session's name (`--name`, `/rename`).
        Some(id) => Some(session::Session::id_for(&cwd, id)),
        None => None,
    };
    if let Some(id) = resume_id {
        match session::Session::load(&cwd, &id) {
            Ok((mut s, msgs)) => {
                // Resuming an archived session brings it back into the lists.
                s.archived = false;
                if cli.model.is_none() {
                    env.set_model(&s.model);
                }
                agent.messages = msgs;
                agent.refresh_system();
                env.accounting
                    .restore(s.stats.total_cost_usd, s.stats.price_unknown);
                agent.stats = s.stats.clone();
                // A marker left beside the session means the process died while
                // tools were running. Say so rather than resuming as if the turn
                // had ended cleanly.
                if let Some(n) = s.interrupted() {
                    warnings.push(n);
                }
                agent.session = Some(s);
                agent.restore_todos();
                if let Some(mode) = cli.mode.as_deref().and_then(Mode::parse) {
                    env.set_mode(mode);
                } else if cli.quiet
                    && !cli.full_auto
                    && !cli.dangerously_skip_permissions
                    && cli.sandbox.is_none()
                {
                    env.set_mode(Mode::Auto);
                }
                if cli.no_think {
                    env.set_think(false);
                }
                if let Some(e) = &cli.effort {
                    env.set_think(true);
                    env.set_effort(Some(e.clone()));
                }
                agent.refresh_system();
            }
            // A script asked for that session: running the prompt in a fresh one would be wrong.
            Err(e) if !interactive => bail!("Could not resume session {id}: {e}"),
            Err(e) => warnings.push(format!("Could not resume session {id}: {e}")),
        }
    } else if (cli.r#continue || cli.resume.is_some())
        && !interactive
        && !warnings
            .iter()
            .any(|w| w.starts_with("The latest session "))
    {
        warnings.push("No previous session to resume.".into());
    }
    if let Some(def) = &definition {
        agent.adopt_definition(def, cli.model.is_some());
        agent::connect_agent_mcp(&mut agent, def).await;
    }
    if cli.fork_session {
        agent.fork_session(None, None)?;
    }
    let source = if agent.session.is_some() {
        "resume"
    } else {
        "startup"
    };
    if agent.session.is_none() {
        let mut s = session::Session::new(&cwd, &env.model());
        if let Some(id) = &cli.session_id {
            s.id = launch::parse_session_id(id)?;
            if session::Session::load(&cwd, &s.id).is_ok()
                || session::Session::find_dir(&s.id).is_some()
            {
                bail!(
                    "--session-id {}: a session with that id already exists (use --resume to continue it)",
                    s.id
                );
            }
        }
        agent.session = Some(s);
    }
    if let Some(s) = agent.session.as_mut() {
        if let Some(n) = cli.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            s.title = n.to_string();
        }
        s.ephemeral = cli.no_session_persistence;
    }
    if let Some(s) = &agent.session {
        env.accounting.set_session(&s.id);
        *env.session_info.lock().unwrap() = Some((s.id.clone(), s.path()));
    }
    if let Some(trigger) = cli.setup_trigger() {
        crate::hooks::prepare_env_file(&env);
        crate::hooks::setup(&env, trigger).await;
    }
    env.session_start(source).await;
    crate::util::timing("setup done");
    Ok(Setup {
        env,
        agent,
        warnings,
    })
}

async fn run_print(cli: Cli) -> Result<i32> {
    use plain::{OutputFormat, PlainFrontend};
    let mut prompt = cli.prompt.join(" ");
    let stream_input = cli.input_format == "stream-json";
    if stream_input && !prompt.trim().is_empty() {
        bail!(
            "--input-format stream-json reads its messages from stdin; don't pass a prompt argument"
        );
    }
    // Read piped input, but don't block on a non-terminal stdin that nothing
    // writes to (another program's socket, cron), unless there is no prompt.
    if !stream_input && !atty_stdin() && (prompt.trim().is_empty() || stdin_is_pipe_or_file()) {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
        if !s.trim().is_empty() {
            prompt = if prompt.is_empty() {
                s
            } else {
                format!("{prompt}\n\n{s}")
            };
        }
    }
    if !stream_input && prompt.trim().is_empty() {
        bail!("no prompt given");
    }
    let format = print_format(&cli);
    let fe = Arc::new(PlainFrontend::new(format, cli.verbose));
    let Setup {
        env,
        mut agent,
        warnings,
    } = setup(&cli, fe, false).await?;
    for w in warnings {
        ui::warn(&w);
    }
    let cancel = tokio_util::sync::CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        // 128 + SIGINT, the shell convention; the turn is saved and ends next.
        crate::util::set_terminated(130);
        c2.cancel();
    });
    // SIGTERM/SIGHUP interrupt like ctrl+c: the turn ends, the session (with
    // the prompt) is saved, and the exit code says which signal. A second one
    // exits at once.
    let c3 = cancel.clone();
    crate::util::on_termination(move |code| {
        if c3.is_cancelled() {
            std::process::exit(code);
        }
        c3.cancel();
    });
    if stream_input {
        let res = run_stream_input(&mut agent, cancel).await;
        env.shutdown().await;
        let ok = res?;
        if let Some(code) = crate::util::terminated() {
            return Ok(code);
        }
        if let Some(notice) = update_check::take_notice() {
            eprintln!("\x1b[2m{notice}\x1b[0m");
        }
        return Ok(if ok { 0 } else { 1 });
    }
    let t0 = std::time::Instant::now();
    // `--json-schema`: ask for JSON matching the schema (checked in main()).
    let schema: Option<serde_json::Value> = cli
        .json_schema
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());
    let (text, ok, structured_output) = if cli.outcome {
        let (text, ok) = turn_text(
            crate::outcome::run(&mut agent, &prompt, cancel)
                .await
                .map(TurnEnd::Done),
        );
        (text, ok, None)
    } else {
        run_structured(&mut agent, &prompt, schema.as_ref(), cancel).await
    };
    env.shutdown().await;
    match format {
        OutputFormat::Json | OutputFormat::StreamJson => {
            let mut r = result_json(&agent, &text, ok, t0.elapsed());
            if let Some(v) = &structured_output {
                r["structured_output"] = v.clone();
            }
            println!("{r}");
        }
        OutputFormat::Text => match &structured_output {
            Some(v) => println!("{v}"),
            // A failed turn isn't an answer: keep stdout for the result and
            // put the reason on stderr, like the warnings above.
            None if !ok => ui::error(text.trim()),
            // Terminal escapes from the model are dropped whether or not
            // stdout is a terminal (a pipe often ends up in one: `| less -R`, logs).
            None => println!("{}", crate::util::strip_terminal_escapes(text.trim())),
        },
        // The transcript showed the answer; the structured value is what scripts want.
        OutputFormat::Live => {
            if let Some(v) = &structured_output {
                println!("{v}");
            }
        }
    }
    if let Some(notice) = update_check::take_notice() {
        eprintln!("\x1b[2m{notice}\x1b[0m");
    }
    // Verified mode finished without passing checks: still an answer, but
    // scripts and CI must be able to tell it from a verified one.
    let unverified = matches!(agent.last_verify, Some((false, _)));
    if let Some(code) = crate::util::terminated() {
        return Ok(code);
    }
    Ok(if !ok {
        1
    } else if unverified {
        3
    } else {
        0
    })
}

/// `-p`'s output format. Quiet text prints only the final summary, even with
/// `--verbose`; JSON and stream-json are the same either way.
fn print_format(cli: &Cli) -> plain::OutputFormat {
    use plain::OutputFormat;
    match (cli.output_format.as_str(), cli.verbose && !cli.quiet) {
        ("stream-json", _) => OutputFormat::StreamJson,
        ("json", _) => OutputFormat::Json,
        (_, true) => OutputFormat::Live,
        _ => OutputFormat::Text,
    }
}

/// Run the print-mode prompt. With a `--json-schema`, the model is asked to
/// finish with matching JSON, and gets one repair turn when it doesn't.
/// Returns the answer text, success, and the structured value.
async fn run_structured(
    agent: &mut Agent,
    prompt: &str,
    schema: Option<&serde_json::Value>,
    cancel: tokio_util::sync::CancellationToken,
) -> (String, bool, Option<serde_json::Value>) {
    let Some(schema) = schema else {
        let (text, ok) = turn_text(agent.run_turn(prompt, vec![], cancel).await);
        return (text, ok, None);
    };
    let first = format!("{prompt}\n\n{}", structured::instruction(schema));
    let (text, ok) = turn_text(agent.run_turn(&first, vec![], cancel.clone()).await);
    if !ok {
        return (text, ok, None);
    }
    let problems = match structured::parse_answer(schema, &text) {
        Ok(v) => return (text, true, Some(v)),
        Err(p) => p,
    };
    let (again, ok) = turn_text(
        agent
            .run_turn(
                &structured::repair_prompt(schema, &problems),
                vec![],
                cancel,
            )
            .await,
    );
    if !ok {
        return (again, false, None);
    }
    match structured::parse_answer(schema, &again) {
        Ok(v) => (again, true, Some(v)),
        Err(problems) => (
            format!(
                "The answer doesn't match --json-schema after a retry:\n- {}\n\n{}",
                problems.join("\n- "),
                again.trim()
            ),
            false,
            None,
        ),
    }
}

/// A finished turn's answer, and whether it succeeded.
fn turn_text(res: Result<TurnEnd>) -> (String, bool) {
    match res {
        Ok(TurnEnd::Done(t)) => (t, true),
        Ok(TurnEnd::Interrupted) => ("[interrupted]".to_string(), false),
        Err(e) => (format!("Error: {e:#}"), false),
    }
}

/// The `result` event printed after a print-mode turn (json and stream-json).
fn result_json(
    agent: &Agent,
    text: &str,
    ok: bool,
    elapsed: std::time::Duration,
) -> serde_json::Value {
    let (num_turns, tool_calls) = agent.run_counts();
    serde_json::json!({
        "type": "result",
        "is_error": !ok,
        "result": text,
        "duration_ms": elapsed.as_millis() as u64,
        "num_turns": num_turns,
        "tool_calls": tool_calls,
        "total_cost_usd": agent.env.accounting.total(),
        "price_unknown": agent.env.accounting.unknown(),
        "usage": {"input_tokens": agent.total_in, "output_tokens": agent.total_out, "cached_tokens": agent.total_cached},
        "session_id": agent.session.as_ref().map(|s| s.id.clone()),
        "model": agent.env.model(),
    })
}

/// `--input-format stream-json`: run each JSON user message on stdin as a turn
/// of the same session, printing a `result` event after each, until EOF or
/// Ctrl-C. A line that isn't a valid user message gets an `error` event and is
/// skipped. Returns whether every line was accepted and every turn succeeded.
async fn run_stream_input(
    agent: &mut Agent,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<bool> {
    use tokio::io::AsyncBufReadExt;
    let mut input = tokio::io::BufReader::new(tokio::io::stdin());
    let mut all_ok = true;
    loop {
        // Raw bytes: a line that isn't UTF-8 is one bad message, not the end of the session.
        let mut buf = Vec::new();
        let read = tokio::select! {
            n = input.read_until(b'\n', &mut buf) => n?,
            _ = cancel.cancelled() => 0,
        };
        if read == 0 {
            break;
        }
        let parsed = match String::from_utf8(buf) {
            Ok(line) => {
                // Claude Code SDK clients also send control requests (initialize,
                // interrupt, …). Answer that they're unsupported rather than
                // counting them as bad input.
                let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap_or_default();
                if v["type"] == "control_request" {
                    println!(
                        "{}",
                        serde_json::json!({"type": "control_response", "response": {
                        "subtype": "error", "request_id": v["request_id"], "error": "chanto doesn't support control requests; send user messages only"}})
                    );
                    continue;
                }
                parse_stream_input(line.trim_end_matches(['\n', '\r']))
            }
            Err(_) => Err("not UTF-8 text".to_string()),
        };
        let (prompt, images) = match parsed {
            Ok(Some(m)) => m,
            Ok(None) => continue,
            Err(e) => {
                println!(
                    "{}",
                    serde_json::json!({"type": "error", "message": format!("Invalid input message: {e}")})
                );
                all_ok = false;
                continue;
            }
        };
        let t0 = std::time::Instant::now();
        let (text, ok) = turn_text(agent.run_turn(&prompt, images, cancel.clone()).await);
        all_ok &= ok;
        println!("{}", result_json(agent, &text, ok, t0.elapsed()));
        if cancel.is_cancelled() {
            break;
        }
    }
    Ok(all_ok)
}

/// One line of `--input-format stream-json`, in Claude Code's shape:
/// `{"type":"user","message":{"role":"user","content":…}}`, where content is a
/// string or an array of `text` and base64 `image` blocks. Returns the text and
/// the images (base64), or None for a blank line.
fn parse_stream_input(line: &str) -> std::result::Result<Option<(String, Vec<String>)>, String> {
    if line.trim().is_empty() {
        return Ok(None);
    }
    let v: serde_json::Value = serde_json::from_str(line).map_err(|e| format!("not JSON: {e}"))?;
    match v["type"].as_str() {
        Some("user") => {}
        Some(t) => {
            return Err(format!(
                "unsupported message type {t:?} (expected \"user\")"
            ));
        }
        None => return Err("missing \"type\": \"user\"".into()),
    }
    let msg = &v["message"];
    if let Some(role) = msg["role"].as_str().filter(|r| *r != "user") {
        return Err(format!("unsupported role {role:?} (expected \"user\")"));
    }
    let mut texts: Vec<String> = Vec::new();
    let mut images: Vec<String> = Vec::new();
    match &msg["content"] {
        serde_json::Value::String(s) => texts.push(s.clone()),
        serde_json::Value::Array(blocks) => {
            for b in blocks {
                match b["type"].as_str() {
                    Some("text") => texts.push(
                        b["text"]
                            .as_str()
                            .ok_or("text block without \"text\"")?
                            .to_string(),
                    ),
                    Some("image") => {
                        let src = &b["source"];
                        if src["type"].as_str() != Some("base64") {
                            return Err("image blocks need a base64 source".into());
                        }
                        images.push(
                            src["data"]
                                .as_str()
                                .ok_or("image source without \"data\"")?
                                .to_string(),
                        );
                    }
                    t => {
                        return Err(format!(
                            "unsupported content block type {:?}",
                            t.unwrap_or("")
                        ));
                    }
                }
            }
        }
        _ => return Err("message.content must be a string or an array of content blocks".into()),
    }
    let text = texts.join("\n");
    if text.trim().is_empty() && images.is_empty() {
        return Err("empty message".into());
    }
    Ok(Some((text, images)))
}

/// Flags that need print mode, or that don't combine.
fn print_only_error(cli: &Cli, print: bool) -> Option<&'static str> {
    if cli.json_schema.is_some() && !print {
        return Some("--json-schema only works with --print (-p)");
    }
    if cli.json_schema.is_some() && (cli.outcome || cli.input_format == "stream-json") {
        return Some(
            "--json-schema can't be combined with --outcome or --input-format stream-json",
        );
    }
    if cli.session_id.is_some() && (cli.r#continue || cli.resume.is_some() || cli.from_pr.is_some())
    {
        return Some(
            "--session-id names a new session; it can't be combined with --continue, --resume or --from-pr",
        );
    }
    None
}

/// Why `--input-format` can't be used with these flags (Claude Code's rules).
fn stream_input_error(
    input_format: &str,
    print: bool,
    output_format: &str,
    outcome: bool,
) -> Option<&'static str> {
    if input_format != "stream-json" {
        return None;
    }
    if !print {
        return Some("--input-format stream-json only works with --print (-p)");
    }
    if output_format != "stream-json" {
        return Some("--input-format stream-json requires --output-format stream-json");
    }
    if outcome {
        return Some("--input-format stream-json can't be combined with --outcome");
    }
    None
}

async fn run_tool(cli: &Cli, name: &str, json: &str) -> Result<()> {
    let fe = Arc::new(plain::PlainFrontend::new(plain::OutputFormat::Live, false));
    let Setup { env, .. } = setup(cli, fe, false).await?;
    let tool = env
        .tools
        .get(name)
        .or_else(|| env.mcp.iter().find_map(|c| c.tool(name)))
        .ok_or_else(|| anyhow::anyhow!("no tool {name}"))?;
    let args: serde_json::Value = serde_json::from_str(json)?;
    let ctx = tools::Ctx {
        env: env.clone(),
        cancel: tokio_util::sync::CancellationToken::new(),
        depth: 0,
        call_id: None,
        cwd_override: None,
        reads: None,
    };
    // An array runs several calls in one session (useful for stateful tools like Browser).
    let calls = match args {
        serde_json::Value::Array(a) => a,
        a => vec![a],
    };
    for args in calls {
        if let Some(reason) = env.config.sandbox.tool_denial(tool.as_ref(), &args) {
            anyhow::bail!(reason);
        }
        let mut out = tool.run(args.clone(), &ctx).await?;
        lsp::after_tool(&env, name, &args, &mut out).await;
        println!(
            "is_error: {}\nimages: {}\ndisplay: {}\n---\n{}\n",
            out.is_error,
            out.images.len(),
            out.display.unwrap_or_default(),
            crate::util::truncate(&out.content, 3000)
        );
    }
    env.shutdown().await;
    Ok(())
}

fn stdin_is_pipe_or_file() -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata("/dev/stdin").is_ok_and(|m| m.file_type().is_fifo() || m.is_file())
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn atty_stdin() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

pub async fn run() {
    crate::util::timing("start");
    feedback::install_panic_hook();
    let session_args: Vec<String> = std::env::args().collect();
    if matches!(
        session_args.get(1).map(String::as_str),
        Some("share" | "sessions" | "import-session")
    ) {
        match share::cli(&session_args[1..]) {
            Ok(out) => println!("{out}"),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
        return;
    }
    // `chanto mcp add|list|get|remove` has its own flags (`-e`, `-H`, `--`).
    let argv: Vec<String> = std::env::args().collect();
    // Clap would print the main help for these.
    if argv.get(1).map(String::as_str) == Some("memory")
        && matches!(argv.get(2).map(String::as_str), Some("--help" | "-h"))
    {
        println!(
            "{}",
            memory::command(std::path::Path::new("."), "help", "project").unwrap_or_default()
        );
        return;
    }
    if argv.get(1).map(String::as_str) == Some(guardrails::GUARD_FLAG) {
        if let (Some(journal), Some(pid)) = (argv.get(2), argv.get(3).and_then(|p| p.parse().ok()))
        {
            guardrails::guard(std::path::Path::new(journal), pid);
        }
        return;
    }
    #[cfg(windows)]
    if argv.get(1).map(String::as_str) == Some(windows_sandbox::LAUNCH_FLAG) {
        windows_sandbox::dispatch(argv.get(2).map(String::as_str));
    }
    if argv.get(1).map(String::as_str) == Some("github") {
        let result = if argv.get(2).map(String::as_str) == Some("install") && argv.len() == 3 {
            std::env::current_dir()
                .map_err(anyhow::Error::from)
                .and_then(|root| github::install(&root))
        } else {
            Err(anyhow::anyhow!("usage: chanto github install"))
        };
        match result {
            Ok(out) => println!("{out}"),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
        return;
    }
    if matches!(
        argv.get(1).map(String::as_str),
        Some("setup" | "doctor" | "models")
    ) && matches!(
        argv.get(2).map(String::as_str),
        Some("--help" | "-h" | "help")
    ) {
        println!(
            "{}",
            match argv[1].as_str() {
                "setup" =>
                    "Usage: chanto setup [--model <provider/model>]\n\nPick a default model and write it to ~/.chanto/config.toml.",
                "doctor" =>
                    "Usage: chanto doctor [--json]\n\nCheck the setup: config layers and where each setting comes from, the model and its server, the sandbox, MCP servers and tools.",
                _ =>
                    "Usage: chanto models [<provider>]\n\nList the models you can use, as provider/model. `chanto providers` lists the providers.",
            }
        );
        return;
    }
    if matches!(
        argv.get(1).map(String::as_str),
        Some("setup" | "doctor" | "models")
    ) {
        let result = match argv[1].as_str() {
            "setup" => onboarding::cli(&argv[2..]).await,
            "doctor" => doctor::cli(&argv[2..]).await,
            _ => doctor::models_cli(argv.get(2).map(String::as_str)).await,
        };
        match result {
            Ok(out) => println!("{out}"),
            Err(e) => {
                if e.is::<onboarding::Cancelled>() {
                    eprintln!("{e}");
                    return;
                }
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
        return;
    }

    if argv.get(1).map(String::as_str) == Some("migrate") {
        match migrate::cli(&argv[2..]).await {
            Ok(out) => println!("{out}"),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
        return;
    }

    if argv.get(1).map(String::as_str) == Some("mcp")
        && matches!(
            argv.get(2).map(String::as_str),
            Some(
                "add"
                    | "add-json"
                    | "add-from-claude-desktop"
                    | "remove"
                    | "rm"
                    | "list"
                    | "ls"
                    | "get"
                    | "help"
                    | "--help"
                    | "-h"
            ) | None
        )
    {
        match mcp_cli::run(&argv[2..]) {
            Ok(out) => println!("{out}"),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
        return;
    }
    // `chanto goal|team` with no task, `help`, `-h` or `--help`: their own usage.
    if let Some(cmd @ ("goal" | "team")) = argv.get(1).map(String::as_str)
        && (argv.len() == 2
            || argv.get(2).is_some_and(|a| a == "help")
            || argv[2..].iter().any(|a| a == "-h" || a == "--help"))
    {
        println!(
            "{}",
            if cmd == "goal" {
                goal::USAGE
            } else {
                team::USAGE
            }
        );
        return;
    }
    // Background runs: the daemon's per-run worker, and the client commands.
    if argv.get(1).map(String::as_str) == Some("__run-worker") {
        std::process::exit(runs::worker::main(&argv[2..]).await);
    }
    // `chanto agents ...` and `chanto cloud run --agent`: Chanto cloud agents.
    if argv.get(1).map(String::as_str) == Some("agents")
        || (argv.get(1).map(String::as_str) == Some("cloud")
            && argv.get(2).map(String::as_str) == Some("run"))
    {
        let result = if argv[1] == "agents" {
            chanto_cloud::agents::cli(&argv[2..]).await
        } else {
            chanto_cloud::agents::cloud_run(&argv[3..]).await
        };
        match result {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
    }
    // `chanto login cloud` / `chanto logout cloud` (or `chanto`) are `chanto cloud login|logout`.
    let cloud_login = matches!(argv.get(1).map(String::as_str), Some("login" | "logout"))
        && argv.get(2).is_some_and(|a| chanto_cloud::is_login_name(a));
    let argv: Vec<String> = if cloud_login {
        [
            &["chanto".into(), "cloud".into(), argv[1].clone()][..],
            &argv[3..],
        ]
        .concat()
    } else {
        argv
    };
    // `--restricted` refuses cloud sessions, as in Claude Code.
    let cloud = argv.get(1).map(String::as_str) == Some("cloud")
        || (matches!(
            argv.get(1).map(String::as_str),
            Some("run" | "runs" | "attach")
        ) && argv[2..].iter().any(|a| a == "--cloud"));
    if cloud
        && (argv.iter().any(|a| a == "--restricted") || launch::env_flag("CLAUDE_CODE_RESTRICTED"))
    {
        ui::error("--restricted refuses cloud runs and chanto cloud commands.");
        std::process::exit(1);
    }
    // Chanto cloud: `chanto cloud login|status|logout|org`, and runs queued
    // there (`chanto run --cloud`, `chanto runs --cloud`, `chanto attach <id> --cloud`).
    if argv.get(1).map(String::as_str) == Some("cloud") {
        match runs::cloud::cloud_cmd(&argv[2..]).await {
            Ok(out) => println!("{out}"),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
        return;
    }
    if matches!(
        argv.get(1).map(String::as_str),
        Some("run" | "runs" | "attach")
    ) && argv[2..].iter().any(|a| a == "--cloud")
    {
        let result = match argv[1].as_str() {
            "run" => runs::cloud::run_cmd(&argv[2..]).await,
            "attach" => {
                runs::cloud::runs_cmd(&[&["attach".to_string()][..], &argv[2..]].concat()).await
            }
            _ => runs::cloud::runs_cmd(&argv[2..]).await,
        };
        match result {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
    }
    if matches!(
        argv.get(1).map(String::as_str),
        Some("runs" | "attach" | "daemon" | "remote")
    ) {
        let result = match argv[1].as_str() {
            "runs" => runs::cli::runs_cmd(&argv[2..]).await,
            "attach" => runs::cli::attach_cmd(&argv[2..]).await,
            "remote" => runs::cli::remote_cmd(&argv[2..]).await,
            _ => runs::cli::daemon_cmd(&argv[2..]).await,
        };
        if let Err(e) = result {
            ui::error(&format!("{e:#}"));
            std::process::exit(1);
        }
        return;
    }
    // `chanto runner ...`: scheduled and triggered jobs.
    if argv.get(1).map(String::as_str) == Some("runner") {
        if let Err(e) = runner::cli(&argv[2..]).await {
            ui::error(&format!("{e:#}"));
            std::process::exit(1);
        }
        return;
    }
    // `chanto upgrade [--check] [--yes] [--force]`.
    if matches!(
        argv.get(1).map(String::as_str),
        Some("upgrade" | "--upgrade")
    ) {
        match update::cli(&argv[2..]).await {
            Ok(out) => println!("{out}"),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
        return;
    }
    if argv.get(1).map(String::as_str) == Some("feedback") {
        match feedback::cli(&argv[2..]).await {
            Ok(out) => println!("{out}"),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
        return;
    }
    if argv.get(1).map(String::as_str) == Some("diagnostics") {
        match feedback::diagnostics_command(&argv[2..]) {
            Ok(out) => println!("{out}"),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
        return;
    }
    if argv.get(1).map(String::as_str) == Some("logs") {
        if argv[2..]
            .iter()
            .any(|a| matches!(a.as_str(), "help" | "-h" | "--help"))
        {
            println!(
                "usage: chanto logs [--tail]\n\n\
                 Print the diagnostic log's path, or follow it with --tail."
            );
            return;
        }
        if let Some(extra) = argv[2..]
            .iter()
            .find(|a| !matches!(a.as_str(), "--tail" | "help" | "-h" | "--help"))
        {
            ui::error(&format!(
                "unknown argument `{extra}`\nusage: chanto logs [--tail]"
            ));
            std::process::exit(1);
        }
        if let Err(e) = crate::cost::logs(argv.iter().any(|a| a == "--tail")).await {
            ui::error(&e.to_string());
            std::process::exit(1);
        }
        return;
    }
    // `chanto insights [--session <id>] [--last N]`: usage report for saved sessions.
    if argv.get(1).map(String::as_str) == Some("insights") {
        match insights::run_cli(&argv[2..]) {
            // The report is Markdown: render it for a terminal, leave it raw when piped.
            Ok(out) if ui::is_tty() && !argv.iter().any(|a| a == "--json") => {
                println!("{}", ui::render_markdown(&out).trim_end())
            }
            Ok(out) => println!("{out}"),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
        return;
    }
    // `chanto login|logout|providers`.
    if matches!(
        argv.get(1).map(String::as_str),
        Some("login" | "logout" | "providers")
    ) {
        match auth::cli(&argv[1..]).await {
            Ok(out) => println!("{out}"),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
        return;
    }
    if argv.get(1).map(String::as_str) == Some("plugin") {
        use std::io::{IsTerminal, Write};
        let mut confirm = |summary: &str| {
            println!("{summary}");
            if !std::io::stdin().is_terminal() {
                return false;
            }
            print!("Plugin can run code. Install? [y/N] ");
            let _ = std::io::stdout().flush();
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer).is_ok()
                && matches!(answer.trim(), "y" | "Y" | "yes")
        };
        match plugin::run(&plugin::root(), &argv[2..], &mut confirm) {
            Ok(out) => println!("{out}"),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
        return;
    }
    // `chanto help` alone prints help rather than sending "help" to the model.
    if argv.len() == 2 && argv[1] == "help" {
        use clap::CommandFactory;
        let _ = Cli::command().print_long_help();
        return;
    }
    let mut cli = Cli::parse();
    // Reject bad flag values before probing for a model, so a typo is reported
    // at once instead of after a network round trip.
    if let Some(m) = &cli.mode
        && Mode::parse(m).is_none()
    {
        ui::error(&format!("Error: unknown mode {m}"));
        std::process::exit(1);
    }
    if let Some(p) = &cli.sandbox
        && let Err(e) = config::SandboxConfig::default().set_policy(p)
    {
        ui::error(&format!("Error: {e:#}"));
        std::process::exit(1);
    }
    if cli.prompt.first().is_some_and(|p| p == "memory") {
        let root = cli
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let result = Config::load(&root, config::is_trusted(&root)).and_then(|(cfg, _)| {
            secrets::init(&cfg, &root);
            memory::command(&root, &cli.prompt[1..].join(" "), &cfg.memory.scope)
        });
        match result {
            Ok(s) => println!("{s}"),
            Err(e) => {
                ui::error(&format!("{e}"));
                std::process::exit(1);
            }
        }
        return;
    }
    if let Some(p) = &cli.profile {
        // SAFETY: set once at startup, before any threads read the environment.
        unsafe { std::env::set_var("CHANTO_PROFILE", p) };
    }
    // Claude Code's launch flags: checked up front, as Claude Code does.
    match cli.launch() {
        Ok(l) => launch::set(l),
        Err(e) => {
            ui::error(&format!("Error: {e:#}"));
            std::process::exit(1);
        }
    }
    if cli.bare {
        // SAFETY: set once at startup, before any threads read the environment.
        unsafe { std::env::set_var("CLAUDE_CODE_SIMPLE", "1") };
    }
    // Hooks, commands and subagents' processes see the modes, as in Claude Code.
    for (on, var) in [
        (launch::get().safe_mode, "CLAUDE_CODE_SAFE_MODE"),
        (launch::get().restricted, "CLAUDE_CODE_RESTRICTED"),
    ] {
        if on {
            // SAFETY: set once at startup, before any threads read the environment.
            unsafe { std::env::set_var(var, "1") };
        }
    }
    if cli.prompt.first().map(String::as_str) == Some("check-extensions") {
        let cwd = cli
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        match customize::check_extensions(&cwd) {
            Ok((_, report)) => println!("{report}"),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
        return;
    }
    if cli.prompt.first().map(String::as_str) == Some("customize") {
        cli.prompt.remove(0);
        if cli.source {
            if let Err(e) = customize::run_source(cli).await {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
            return;
        }
        if cli.prompt.is_empty() {
            ui::error("Usage: chanto customize <request>");
            std::process::exit(1);
        }
        cli.customizing = true;
    } else if cli.source {
        ui::error("--source requires chanto customize");
        std::process::exit(1);
    }
    // `chanto goal "<objective>"` and `chanto team "<task>"` (also with --bg).
    if !cli.customizing
        && matches!(
            cli.prompt.first().map(String::as_str),
            Some("goal" | "team")
        )
        && cli.prompt.len() > 1
    {
        let res = if cli.prompt[0] == "goal" {
            goal::cli(cli).await
        } else {
            team::cli(cli).await
        };
        match res {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
    }
    // `chanto run --bg "<task>"`: hand the task to the daemon and return.
    if cli.bg && !cli.customizing {
        if let Err(e) = runs::cli::run_bg(&cli).await {
            ui::error(&format!("{e:#}"));
            std::process::exit(1);
        }
        return;
    }
    if !cli.customizing && cli.prompt.first().map(String::as_str) == Some("run") {
        ui::error(
            "`chanto run` needs --bg to run in the background (`chanto run --bg \"<task>\"`) or --cloud to run on chanto cloud; use `chanto -p \"<task>\"` to run in the foreground.",
        );
        std::process::exit(1);
    }
    // `chanto serve`: HTTP API for other programs.
    if !cli.customizing
        && cli
            .prompt
            .first()
            .map(String::as_str)
            .is_some_and(|s| s == "serve" || s == "web")
    {
        let web = cli.prompt.remove(0) == "web";
        let (host, port) = (cli.host.clone(), cli.port);
        let tailscale = cli.tailscale;
        if let Err(e) = server::run(cli, &host, port, web, tailscale).await {
            ui::error(&format!("Error: {e:#}"));
            std::process::exit(1);
        }
        return;
    }
    if !cli.customizing && cli.prompt.first().map(String::as_str) == Some("acp") {
        if let Err(e) = acp::run(cli).await {
            eprintln!("{e:#}");
            std::process::exit(1);
        }
        return;
    }
    // `chanto acp-client <command>`: the TUI runs, but turns go to an
    // external ACP agent. Remote control works because it wraps the TUI.
    if !cli.customizing && cli.prompt.first().map(String::as_str) == Some("acp-client") {
        cli.prompt.remove(0);
        if cli.prompt.is_empty() {
            eprintln!(
                "Usage: chanto acp-client <command> [args...]\n\nThe command must speak ACP v1 on stdio (for example `claude-agent-acp`)."
            );
            std::process::exit(1);
        }
        if cli.print {
            eprintln!("chanto acp-client is interactive; remove -p.");
            std::process::exit(1);
        }
        cli.acp_command = Some(std::mem::take(&mut cli.prompt));
    }

    // `chanto mcp-serve`: MCP server over stdio for other agents and editors.
    if !cli.customizing && cli.prompt.first().map(String::as_str) == Some("mcp-serve") {
        cli.prompt.remove(0);
        if let Err(e) = mcp_server::run(cli).await {
            ui::error(&format!("Error: {e:#}"));
            std::process::exit(1);
        }
        return;
    }
    // `chanto exec <prompt>` (Codex) is print mode.
    if !cli.customizing && cli.prompt.first().map(String::as_str) == Some("exec") {
        cli.prompt.remove(0);
        cli.print = true;
    }
    if cli.json {
        cli.output_format = "stream-json".into();
    }
    if let Some(sh) = cli.completions {
        use clap::CommandFactory;
        clap_complete::generate(sh, &mut Cli::command(), "chanto", &mut std::io::stdout());
        return;
    }
    // `--resume <id>` from another directory: resume it in its own project.
    if let Some(id) = cli.resume.clone().filter(|id| !id.is_empty()) {
        let cwd = cli
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        if session::Session::load(&cwd, &id).is_err()
            && let Some(dir) = session::Session::find_dir(&id).filter(|d| d.is_dir())
        {
            eprintln!(
                "Session {id} belongs to {}; resuming it there.",
                dir.display()
            );
            cli.cwd = Some(dir);
        }
    }
    // `--worktree [name]`: the session works in its own checkout and branch.
    if let Some(name) = cli.worktree.clone() {
        match start_worktree(&cli, &name) {
            Ok(dir) => cli.cwd = Some(dir),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
    }
    if cli.example_config {
        print!("{}", config::EXAMPLE_CONFIG);
        return;
    }
    if let Some(cmd) = &cli.risk {
        println!(
            "features: {:?}\nverdict: {:?}",
            risk::features(cmd),
            risk::assess(cmd)
        );
        return;
    }
    if let Some(a) = &cli.train_risk {
        let run = || -> anyhow::Result<String> {
            let (m, report) = risk::train(&std::fs::read_to_string(&a[0])?)?;
            std::fs::write(&a[1], serde_json::to_string_pretty(&m)? + "\n")?;
            Ok(report)
        };
        match run() {
            Ok(r) => println!("{r}"),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
        return;
    }
    if let Some(a) = &cli.eval_risk {
        let text = std::fs::read_to_string(&a[0]).unwrap_or_default();
        let (mut n, mut allowed, mut routine, mut routine_allowed, mut leaked) =
            (0, 0, 0, 0, Vec::new());
        let mut out = String::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let cmd = v["cmd"].as_str().unwrap_or("");
            let verdict = risk::assess_with(risk::builtin(), cmd, &Default::default());
            let ok = matches!(verdict, risk::Verdict::Allow { .. });
            n += 1;
            allowed += ok as usize;
            match v["risky"].as_u64() {
                Some(0) => {
                    routine += 1;
                    routine_allowed += ok as usize;
                }
                Some(_) if ok => leaked.push(cmd.to_string()),
                _ => {}
            }
            let mut v = v.clone();
            v["fast_allow"] = serde_json::json!(ok);
            out.push_str(&format!("{v}\n"));
        }
        println!(
            "commands: {n}\nfast-allowed: {allowed} ({:.1}%)",
            100.0 * allowed as f64 / n.max(1) as f64
        );
        if routine > 0 || !leaked.is_empty() {
            println!(
                "labelled routine fast-allowed: {routine_allowed}/{routine} ({:.1}%)",
                100.0 * routine_allowed as f64 / routine.max(1) as f64
            );
            println!("labelled risky fast-allowed: {}", leaked.len());
            for l in &leaked {
                println!("  {}", l.replace('\n', "\\n"));
            }
        }
        if let Some(o) = a.get(1) {
            let _ = std::fs::write(o, out);
        }
        return;
    }
    if let Some(a) = &cli.triage_commands {
        // Either ["cmd", ...] or [{"cmd": ..., "cwd": ...}, ...].
        let raw: Vec<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(&a[0]).unwrap_or_default())
                .unwrap_or_default();
        let here = std::env::current_dir().unwrap_or_default();
        let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
        let mut rest = Vec::new();
        for v in &raw {
            let (c, cwd) = match v {
                serde_json::Value::String(c) => (c.clone(), here.clone()),
                v => (
                    v["cmd"].as_str().unwrap_or("").to_string(),
                    v["cwd"]
                        .as_str()
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|| here.clone()),
                ),
            };
            let c = &c;
            permissions::set_project_root(&cwd);
            let _ = std::env::set_current_dir(&cwd);
            let args = serde_json::json!({"command": c});
            let stage = match permissions::static_decision(
                permissions::Mode::Auto,
                tools::ToolKind::Execute,
                "Bash",
                &args,
                &cwd,
            ) {
                Some(permissions::Decision::Allow) => "static allow",
                Some(_) => "static block",
                None => match risk::assess(c) {
                    risk::Verdict::Allow { .. } => {
                        if std::env::var_os("CHANTO_TRIAGE_FAST").is_some() {
                            println!("FAST\t{}", serde_json::to_string(c).unwrap_or_default());
                        }
                        "fast allow"
                    }
                    risk::Verdict::Defer { .. } => {
                        rest.push(c.clone());
                        "model classifier"
                    }
                },
            };
            *counts.entry(stage).or_default() += 1;
        }
        for (k, v) in &counts {
            println!(
                "{k:>16}: {v:>6} ({:.1}%)",
                100.0 * *v as f64 / raw.len().max(1) as f64
            );
        }
        let _ = std::fs::write(&a[1], serde_json::to_string(&rest).unwrap_or_default());
        return;
    }
    if let Some(c) = cli.classify.clone() {
        let fe = Arc::new(plain::PlainFrontend::new(plain::OutputFormat::Text, false));
        let Setup { env, .. } = match setup(&cli, fe, false).await {
            Ok(s) => s,
            Err(e) => {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        };
        let args: serde_json::Value = serde_json::from_str(&c[1]).unwrap_or_default();
        let model = env
            .config
            .classifier_model
            .clone()
            .unwrap_or_else(|| env.model());
        let (p, m) = env.resolve(&model);
        let t0 = std::time::Instant::now();
        let d = permissions::classify(
            &p,
            &m,
            &[llm::Message::user(c[2].clone())],
            &c[0],
            &args,
            &env.config.permissions.auto_mode_rules,
            env.config.optimize.token_classifier(),
        )
        .await;
        println!("{:?} ({:.1}s)", d, t0.elapsed().as_secs_f32());
        return;
    }
    if cli.dump_config {
        let cwd = std::env::current_dir().unwrap_or_default();
        match Config::load(&cwd, config::is_trusted(&cwd)) {
            Ok((mut cfg, skipped)) => {
                cfg.api_key = cfg.api_key.map(|_| "<redacted>".into());
                for p in cfg.providers.values_mut() {
                    p.api_key = p.api_key.take().map(|_| "<redacted>".into());
                }
                for s in cfg.mcp_servers.values_mut() {
                    for v in s.env.values_mut().chain(s.headers.values_mut()) {
                        *v = "<redacted>".into();
                    }
                }
                cfg.web.brave_api_key = cfg.web.brave_api_key.map(|_| "<redacted>".into());
                cfg.web.tavily_api_key = cfg.web.tavily_api_key.map(|_| "<redacted>".into());
                println!("{}", toml::to_string_pretty(&cfg).unwrap_or_default());
                for s in skipped {
                    println!("# skipped (untrusted): {s}");
                }
            }
            Err(e) => {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
        return;
    }
    if cli.dump_prompt {
        let fe = Arc::new(plain::PlainFrontend::new(plain::OutputFormat::Text, false));
        match setup(&cli, fe, false).await {
            Ok(Setup { env, agent, .. }) => {
                let sys = &agent.messages[0].content;
                let specs = serde_json::to_string(&env.tools.specs(&|_| true)).unwrap_or_default();
                println!(
                    "{sys}\n\n--- system: {} chars (~{} tok) · tools: {} chars (~{} tok)",
                    sys.len(),
                    sys.len() / 4,
                    specs.len(),
                    specs.len() / 4
                );
            }
            Err(e) => {
                ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
        return;
    }
    if let Some(rt) = cli.run_tool.clone() {
        std::process::exit(match run_tool(&cli, &rt[0], &rt[1]).await {
            Ok(()) => 0,
            Err(e) => {
                if e.is::<onboarding::Cancelled>() {
                    eprintln!("{e}");
                    return;
                }
                ui::error(&format!("Error: {e:#}"));
                1
            }
        });
    }
    if cli.init_only {
        // Setup hooks (trigger `init`) and SessionStart hooks, then exit.
        let fe = Arc::new(plain::PlainFrontend::new(plain::OutputFormat::Text, false));
        let code = match setup(&cli, fe, false).await {
            Ok(Setup { env, .. }) => {
                env.shutdown().await;
                0
            }
            Err(e) => {
                ui::error(&format!("Error: {e:#}"));
                1
            }
        };
        presence::stop();
        finish_worktree();
        std::process::exit(code);
    }
    if (cli.init || cli.maintenance) && !cli.print {
        ui::error(
            "Error: --init and --maintenance only work with -p (to run the Setup hooks without a prompt, use --init-only)",
        );
        std::process::exit(2);
    }
    if let Some(e) = stream_input_error(
        &cli.input_format,
        cli.print,
        &cli.output_format,
        cli.outcome,
    ) {
        ui::error(&format!("Error: {e}"));
        std::process::exit(1);
    }
    if let Some(e) = print_only_error(&cli, cli.print || !ui::is_tty()) {
        ui::error(&format!("Error: {e}"));
        std::process::exit(1);
    }
    if cli.include_partial_messages && !(cli.print && cli.output_format == "stream-json") {
        ui::warn(
            "--include-partial-messages only applies with -p --output-format stream-json (whose text events already stream as they arrive)",
        );
    }
    // `--agent`: its `initialPrompt` is the first user turn of a new session,
    // ahead of any prompt given.
    if let Some(name) = cli
        .agent
        .clone()
        .filter(|_| !cli.r#continue && cli.resume.is_none())
    {
        let cwd = cli
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        if let Some(p) = launch::initial_prompt(&cwd, cli.agents.as_deref(), &name) {
            cli.prompt.insert(0, p);
        }
    }
    let code = if cli.print || !ui::is_tty() {
        match run_print(cli).await {
            Ok(c) => c,
            Err(e) => {
                if e.is::<onboarding::Cancelled>() {
                    eprintln!("{e}");
                    return;
                }
                ui::error(&format!("Error: {e:#}"));
                1
            }
        }
    } else if cli.plain || crate::a11y::screen_reader() {
        match repl::run(cli).await {
            Ok(()) => 0,
            Err(e) => {
                if e.is::<onboarding::Cancelled>() {
                    eprintln!("{e}");
                    return;
                }
                ui::error(&format!("Error: {e:#}"));
                1
            }
        }
    } else {
        match tui::run(cli).await {
            Ok(()) => 0,
            Err(e) => {
                if e.is::<onboarding::Cancelled>() {
                    eprintln!("{e}");
                    return;
                }
                ui::error(&format!("Error: {e:#}"));
                1
            }
        }
    };
    presence::stop();
    finish_worktree();
    std::process::exit(code);
}

static WORKTREE: std::sync::OnceLock<worktree::Worktree> = std::sync::OnceLock::new();
/// A `--worktree` made by a WorktreeCreate hook: (path, where Chanto started, hooks).
static HOOK_WORKTREE: std::sync::OnceLock<(
    std::path::PathBuf,
    std::path::PathBuf,
    config::HooksConfig,
)> = std::sync::OnceLock::new();

/// Create the session's worktree; returns the directory to work in (the same
/// subdirectory of the repository the user started from).
fn start_worktree(cli: &Cli, name: &str) -> Result<std::path::PathBuf> {
    let cwd = match &cli.cwd {
        Some(p) => p.clone(),
        None => std::env::current_dir()?,
    };
    let name = if name.is_empty() {
        format!(
            "session-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..6]
        )
    } else {
        name.to_string()
    };
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        || name.starts_with('.')
    {
        bail!("--worktree names can use letters, digits, '-', '_' and '.'");
    }
    // A WorktreeCreate hook replaces git: the session runs in the path it returns.
    let hooks = Config::load(&cwd, config::is_trusted(&cwd))
        .map(|(c, _)| c.hooks)
        .unwrap_or_default();
    if let Some(made) = hooks::worktree_create_detached(&hooks, &cwd, &name) {
        let dir = made.map_err(|e| anyhow::anyhow!(e))?;
        eprintln!(
            "Working in {} (made by a WorktreeCreate hook).",
            dir.display()
        );
        let _ = HOOK_WORKTREE.set((dir.clone(), cwd, hooks));
        return Ok(dir);
    }
    let wt = worktree::create(&cwd, &name)?;
    let rel = std::fs::canonicalize(&cwd)
        .ok()
        .and_then(|c| {
            c.strip_prefix(std::fs::canonicalize(&wt.repo).ok()?)
                .ok()
                .map(|r| r.to_path_buf())
        })
        .unwrap_or_default();
    eprintln!(
        "Working in {} on branch {} (your checkout is untouched).",
        wt.path.display(),
        wt.branch
    );
    let dir = wt.path.join(rel);
    let _ = WORKTREE.set(wt);
    Ok(dir)
}

/// On exit: remove a worktree with no changes, or say how to take them.
fn finish_worktree() {
    if let Some((dir, cwd, hooks)) = HOOK_WORKTREE.get() {
        hooks::finish_hook_worktree(hooks, cwd, dir);
        return;
    }
    let Some(wt) = WORKTREE.get() else { return };
    if wt.cleanup_if_unchanged() {
        eprintln!("No changes in {}; removed it.", wt.branch);
    } else {
        eprintln!(
            "Changes are on branch {} in {}.\n  Review: git -C {} diff {}\n  Take them: commit there, then `git merge {}` in your checkout\n  Discard: git worktree remove --force {} && git branch -D {}",
            wt.branch,
            wt.path.display(),
            wt.path.display(),
            &wt.base[..wt.base.len().min(12)],
            wt.branch,
            wt.path.display(),
            wt.branch
        );
    }
}

fn query_explicit_models(explicit_url: bool, explicit_model: bool) -> bool {
    explicit_url && !explicit_model
}

#[cfg(test)]
mod onboarding_flag_tests {
    #[test]
    fn explicit_endpoint_queries_models_unless_model_is_explicit() {
        assert!(super::query_explicit_models(true, false));
        assert!(!super::query_explicit_models(true, true));
        assert!(!super::query_explicit_models(false, false));
        assert!(!super::query_explicit_models(false, true));
    }
}

#[cfg(test)]
mod claude_flag_tests {
    use super::{Cli, print_only_error, run_structured};
    use crate::agent::Agent;
    use crate::agent::tests::{make_env, mock_server, tmpdir};
    use crate::permissions::Mode;
    use clap::Parser;
    use serde_json::json;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from([&["chanto"][..], args].concat()).unwrap()
    }

    #[test]
    fn safe_mode_and_restricted_launch_flags() {
        let l = parse(&["--safe-mode"]).launch().unwrap();
        assert!(l.safe_mode && l.no_customizations() && !l.bare && l.wants("user"));
        let l = parse(&[
            "--restricted",
            "--setting-sources",
            "user,project",
            "--settings",
            "{}",
        ])
        .launch()
        .unwrap();
        // Only managed settings and --settings.
        assert!(
            l.restricted
                && !l.wants("user")
                && !l.wants("project")
                && !l.wants("local")
                && l.settings.len() == 1
        );
        let e = parse(&["--restricted", "--dangerously-skip-permissions"])
            .launch()
            .unwrap_err();
        assert!(e.to_string().contains("bypass"), "{e}");
    }

    #[test]
    fn claude_code_flags_parse() {
        let c = parse(&[
            "-p",
            "--system-prompt",
            "S",
            "--append-system-prompt-file",
            "a.txt",
            "--mcp-config",
            "a.json",
            "--mcp-config",
            "{\"mcpServers\":{}}",
            "--strict-mcp-config",
            "--settings",
            "{}",
            "--setting-sources",
            "user,project",
            "--session-id",
            "550e8400-e29b-41d4-a716-446655440000",
            "-n",
            "my-work",
            "--no-session-persistence",
            "--json-schema",
            "{\"type\":\"object\"}",
            "--agent",
            "reviewer",
            "--tools",
            "",
            "--plugin-dir",
            "p1",
            "--plugin-dir",
            "p2",
            "--bare",
            "--include-partial-messages",
            "--permission-prompt-tool",
            "mcp__auth__ok",
            "do it",
        ]);
        assert_eq!(c.system_prompt.as_deref(), Some("S"));
        assert_eq!(c.mcp_config.len(), 2);
        assert_eq!(c.plugin_dir, vec!["p1", "p2"]);
        assert_eq!(c.tools.as_deref(), Some(""));
        assert_eq!(c.name.as_deref(), Some("my-work"));
        assert_eq!(c.prompt, vec!["do it"]);
        assert!(
            c.strict_mcp_config && c.bare && c.no_session_persistence && c.include_partial_messages
        );
        assert!(
            Cli::try_parse_from([
                "chanto",
                "--system-prompt",
                "a",
                "--system-prompt-file",
                "b"
            ])
            .is_err()
        );
    }

    #[test]
    fn agents_is_a_team_size_or_subagent_json() {
        let c = parse(&["team", "x", "--agents", "4"]);
        assert_eq!(c.team_size(), Some(4));
        assert!(c.launch().is_ok());
        let c = parse(&["--agents", r#"{"r":{"description":"d","prompt":"p"}}"#]);
        assert_eq!(c.team_size(), None);
        assert!(c.launch().is_ok());
        assert!(
            parse(&["--agents", r#"{"r":{"prompt":"p"}}"#])
                .launch()
                .is_err()
        );
    }

    #[test]
    fn launch_flags_are_checked_up_front() {
        assert!(
            parse(&["--json-schema", "{\"type\":\"objekt\"}"])
                .launch()
                .unwrap_err()
                .to_string()
                .contains("not a valid JSON Schema")
        );
        assert!(parse(&["--json-schema", "nope"]).launch().is_err());
        assert!(parse(&["--session-id", "not-a-uuid"]).launch().is_err());
        assert!(
            parse(&["--permission-prompt-tool", "approve"])
                .launch()
                .is_err()
        );
        assert!(
            parse(&["--setting-sources", "everything"])
                .launch()
                .is_err()
        );
        assert!(
            parse(&["--mcp-config", "/no/such/file.json"])
                .launch()
                .is_err()
        );
        let l = parse(&[
            "--strict-mcp-config",
            "--mcp-config",
            r#"{"mcpServers":{"a":{"command":"x"}}}"#,
            "--bare",
        ])
        .launch()
        .unwrap();
        assert!(l.strict_mcp && l.bare && l.mcp[0].1.contains_key("a"));
        assert!(print_only_error(&parse(&["--json-schema", "{}"]), false).is_some());
        assert!(print_only_error(&parse(&["-p", "--json-schema", "{}"]), true).is_none());
        assert!(
            print_only_error(
                &parse(&["--session-id", "550e8400-e29b-41d4-a716-446655440000", "-c"]),
                true
            )
            .is_some()
        );
    }

    #[test]
    fn quiet_presets_auto_mode_unless_a_mode_is_given() {
        let mode = |args: &[&str], configured| super::start_mode(&parse(args), configured).unwrap();
        assert_eq!(mode(&["--quiet"], None), Mode::Auto);
        // The preset beats the config's default mode; anything explicit beats the preset.
        assert_eq!(mode(&["--quiet"], Some(Mode::Default)), Mode::Auto);
        assert_eq!(mode(&["--quiet", "--mode", "plan"], None), Mode::Plan);
        assert_eq!(
            mode(&["--quiet", "--permission-mode", "default"], None),
            Mode::Default
        );
        assert_eq!(
            mode(&["--quiet", "--dangerously-skip-permissions"], None),
            Mode::Bypass
        );
        assert_eq!(
            mode(&["--quiet", "--sandbox", "read-only"], None),
            Mode::Plan
        );
        // Without --quiet nothing changes.
        assert_eq!(mode(&[], Some(Mode::AcceptEdits)), Mode::AcceptEdits);
        assert_eq!(mode(&[], None), Mode::Default);
    }

    #[test]
    fn quiet_print_text_is_the_summary_only_and_json_is_unchanged() {
        use crate::plain::OutputFormat;
        let fmt = |args: &[&str]| super::print_format(&parse(args));
        assert!(fmt(&["-p", "--quiet", "--verbose", "x"]) == OutputFormat::Text);
        assert!(fmt(&["-p", "--verbose", "x"]) == OutputFormat::Live);
        assert!(fmt(&["-p", "--quiet", "--output-format", "json", "x"]) == OutputFormat::Json);
        assert!(
            fmt(&[
                "-p",
                "--quiet",
                "--output-format",
                "stream-json",
                "--verbose",
                "x"
            ]) == OutputFormat::StreamJson
        );
    }

    #[tokio::test]
    async fn quiet_print_mode_answers_with_the_final_summary() {
        let dir = tmpdir("quiet-print");
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        let (base, _) = mock_server(vec![
            json!({"content": "Let me look.", "tool_calls": crate::agent::tests::tc("Read", json!({"file_path": "a.txt"}))}),
            json!({"content": "Read a.txt. Nothing changed."}),
        ])
        .await;
        let env = crate::agent::tests::make_env_with(&base, &dir, Mode::Bypass, 32768, |c| {
            c.optimize.quiet = Some(true)
        });
        let mut agent = Agent::new(env);
        // What `-p` text prints: the turn's answer, without the narration before the tool call.
        let (text, ok, _) = run_structured(
            &mut agent,
            "read a.txt",
            None,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(ok);
        assert_eq!(text, "Read a.txt. Nothing changed.");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn json_schema_output_is_validated_and_repaired_once() {
        let schema =
            json!({"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]});
        // Valid on the first try (inside prose).
        let (base, seen) = mock_server(vec![json!({"content": "Done: {\"n\": 2}"})]).await;
        let dir = tmpdir("schema-ok");
        let mut agent = Agent::new(make_env(&base, &dir, Mode::Bypass, 32768));
        let (_, ok, v) = run_structured(
            &mut agent,
            "count",
            Some(&schema),
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(ok);
        assert_eq!(v, Some(json!({"n": 2})));
        assert!(seen.lock().unwrap()[0].to_string().contains("JSON Schema"));
        let _ = std::fs::remove_dir_all(dir);

        // Wrong, then repaired.
        let (base, seen) = mock_server(vec![
            json!({"content": "{\"n\": \"two\"}"}),
            json!({"content": "{\"n\": 2}"}),
        ])
        .await;
        let dir = tmpdir("schema-fix");
        let mut agent = Agent::new(make_env(&base, &dir, Mode::Bypass, 32768));
        let (_, ok, v) = run_structured(
            &mut agent,
            "count",
            Some(&schema),
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(ok);
        assert_eq!(v, Some(json!({"n": 2})));
        assert!(
            seen.lock().unwrap()[1]
                .to_string()
                .contains("expected integer")
        );
        let _ = std::fs::remove_dir_all(dir);

        // Wrong twice: an error.
        let (base, _) = mock_server(vec![
            json!({"content": "no"}),
            json!({"content": "still no"}),
        ])
        .await;
        let dir = tmpdir("schema-bad");
        let mut agent = Agent::new(make_env(&base, &dir, Mode::Bypass, 32768));
        let (text, ok, v) = run_structured(
            &mut agent,
            "count",
            Some(&schema),
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(!ok && v.is_none());
        assert!(text.contains("doesn't match --json-schema"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod stream_input_tests {
    use super::{parse_stream_input, stream_input_error};

    #[test]
    fn string_content_is_the_prompt() {
        let m = parse_stream_input(
            r#"{"type":"user","message":{"role":"user","content":"fix the tests"}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(m, ("fix the tests".to_string(), vec![]));
    }

    #[test]
    fn content_blocks_join_text_and_collect_base64_images() {
        let line = r#"{"type":"user","session_id":"x","message":{"role":"user","content":[{"type":"text","text":"what is this?"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0K"}},{"type":"text","text":"be brief"}]}}"#;
        let (text, images) = parse_stream_input(line).unwrap().unwrap();
        assert_eq!(text, "what is this?\nbe brief");
        assert_eq!(images, vec!["iVBORw0K".to_string()]);
    }

    #[test]
    fn blank_lines_are_skipped() {
        assert_eq!(parse_stream_input("").unwrap(), None);
        assert_eq!(parse_stream_input("   \r").unwrap(), None);
    }

    #[test]
    fn malformed_messages_are_rejected() {
        for line in [
            "not json",
            r#"{"message":{"content":"hi"}}"#,
            r#"{"type":"control_request","request":{}}"#,
            r#"{"type":"user","message":{"role":"assistant","content":"hi"}}"#,
            r#"{"type":"user","message":{"role":"user"}}"#,
            r#"{"type":"user","message":{"role":"user","content":"  "}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"x"}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"image","source":{"type":"url","url":"https://x/y.png"}}]}}"#,
        ] {
            assert!(parse_stream_input(line).is_err(), "{line}");
        }
    }

    #[test]
    fn stream_input_needs_print_and_stream_json_output() {
        assert_eq!(stream_input_error("text", false, "text", false), None);
        assert_eq!(
            stream_input_error("stream-json", true, "stream-json", false),
            None
        );
        assert!(stream_input_error("stream-json", false, "stream-json", false).is_some());
        assert!(stream_input_error("stream-json", true, "json", false).is_some());
        assert!(stream_input_error("stream-json", true, "stream-json", true).is_some());
    }
}

#[cfg(test)]
mod version_tests {
    /// Everything released from this repository reports the CLI's version: the
    /// desktop app (About, `--version`, the bundle) and both editor plugins.
    /// A stale manifest makes an installed copy look like no update arrived.
    #[test]
    fn shipped_manifests_agree_on_the_version() {
        let version = env!("CARGO_PKG_VERSION");
        let field = |text: &str, prefix: &str| {
            text.lines()
                .find_map(|l| l.trim().strip_prefix(prefix))
                .and_then(|v| v.trim_end_matches(',').strip_suffix('"'))
                .unwrap_or_else(|| panic!("no {prefix} field"))
                .to_string()
        };
        assert_eq!(
            field(include_str!("../desktop/Cargo.toml"), "version = \""),
            version,
            "desktop/Cargo.toml"
        );
        assert_eq!(
            field(
                include_str!("../ide/vscode/package.json"),
                "\"version\": \""
            ),
            version,
            "ide/vscode/package.json"
        );
        assert_eq!(
            field(
                include_str!("../ide/jetbrains/build.gradle.kts"),
                "version = \""
            ),
            version,
            "ide/jetbrains/build.gradle.kts"
        );
    }

    /// Every surface that can be installed on its own carries the app mark:
    /// the VS Code extension's store icon and the JetBrains plugin icon. A
    /// missing or malformed one ships as a blank square.
    #[test]
    fn shipped_marks_are_present() {
        let png = include_bytes!("../ide/vscode/icon.png");
        assert_eq!(
            &png[..8],
            b"\x89PNG\r\n\x1a\n",
            "ide/vscode/icon.png is a PNG"
        );
        // Width and height lead the IHDR chunk, big-endian.
        assert_eq!(
            &png[16..24],
            &[0, 0, 0, 128, 0, 0, 0, 128],
            "ide/vscode/icon.png is 128×128"
        );
        assert!(include_str!("../ide/vscode/package.json").contains("\"icon\": \"icon.png\""));
        assert!(
            include_str!("../ide/jetbrains/src/main/resources/META-INF/pluginIcon.svg")
                .contains("<svg")
        );
        assert!(include_str!("../assets/logo.svg").contains("<svg"));
    }
}
