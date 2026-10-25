//! Layered configuration: built-in defaults < ~/.chanto/config.toml <
//! ./.chanto/config.toml < environment < CLI flags.

use crate::llm::ProviderKind;
use crate::permissions::Mode;
use crate::util::Canonical as _;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub updates: crate::update_check::UpdatesConfig,
    pub pricing: BTreeMap<String, crate::cost::Price>,
    pub max_cost_usd: Option<f64>,
    pub max_cost_usd_per_day: Option<f64>,
    pub telemetry: crate::cost::Telemetry,
    pub memory: crate::memory::MemoryConfig,
    pub history: crate::history::HistoryConfig,
    /// Organization policy from Claude Code managed settings: the starting mode.
    #[serde(skip)]
    pub policy_mode: Option<crate::permissions::Mode>,
    /// Organization policy: bypass-permissions mode is disabled.
    #[serde(skip)]
    pub policy_no_bypass: bool,
    #[serde(skip)]
    pub origins: std::collections::HashMap<String, PathBuf>,
    #[serde(skip)]
    pub layers: Vec<String>,
    #[serde(skip)]
    pub diagnostics: Vec<String>,
    pub provider: ProviderKind,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    /// Context window to request from the server (Ollama `num_ctx`) and to
    /// budget against for auto-compaction.
    pub num_ctx: Option<u32>,
    pub temperature: Option<f32>,
    /// Enable model "thinking" (reasoning) when supported.
    pub think: Option<bool>,
    /// Reasoning effort: low | medium | high | xhigh | max (unset: the provider's default).
    pub effort: Option<String>,
    /// Show streamed reasoning in the terminal.
    pub show_thinking: Option<bool>,
    /// Interactive view: "fullscreen" (default) or "inline" (output goes to the
    /// terminal's scrollback; see `--inline`).
    pub tui: Option<String>,
    pub mode: Option<Mode>,
    /// Model used by the auto-mode safety classifier (defaults to `model`).
    pub classifier_model: Option<String>,
    /// Model used for subagents (defaults to `model`).
    pub subagent_model: Option<String>,
    /// Model for the `oracle` subagent, a read-only second opinion on plans
    /// and diffs (defaults to `model`).
    pub oracle_model: Option<String>,
    /// When a hosted plan's usage limit (5h or weekly window) runs out, wait
    /// for the reset and continue the turn instead of failing it. The TUI
    /// asks when this is unset; `-p` runs wait only when it's true.
    pub resume_after_limit: Option<bool>,
    /// Which saved login to use (`chanto login <provider> --as <name>`), for
    /// every provider that has one by that name; `/login` switches per session.
    pub account: Option<String>,
    /// Models to switch to when the main model is overloaded or unavailable,
    /// tried in order, for the rest of that turn: a spec, a comma-separated
    /// list or an array (Claude Code's `fallbackModel`; `--fallback-model`).
    pub fallback_model: Option<ModelList>,
    /// Tool search (deferred MCP tool schemas, loaded with the ToolSearch
    /// tool): "auto" (default; defer once they'd take 10% of the context),
    /// "auto:N" (N%), "true" (always defer) or "false". `ENABLE_TOOL_SEARCH` overrides.
    pub tool_search: Option<String>,
    /// Auto-compact when context usage exceeds this fraction (0..1).
    pub compact_threshold: Option<f32>,
    /// Seconds a model response stream may go silent once it has started
    /// before the request fails (and is retried); 0 turns the limit off.
    /// The wait for the first byte (prompt processing) is never limited.
    pub stream_idle_timeout: Option<f64>,
    /// Seconds an MCP tool call may take before it fails, for servers without
    /// their own `tool_timeout_sec` (default: `MCP_TOOL_TIMEOUT` in ms, else 600).
    pub mcp_tool_timeout: Option<f64>,
    pub max_turns: Option<u32>,
    /// "native" (default, auto-falls back) or "text" to force the text tool-call protocol.
    pub tool_protocol: Option<String>,
    /// Advertise the Codex-style `apply_patch` tool (it is always accepted when
    /// a model calls it; advertising it helps models trained on Codex, e.g. gpt-oss).
    pub apply_patch_tool: Option<bool>,
    /// Shell command whose first output line is shown in the TUI footer
    /// (receives session JSON on stdin), refreshed after each turn.
    pub status_line: Option<String>,
    /// Ring the terminal bell when a turn that took over this many seconds finishes (0 = off).
    pub notify_after_secs: Option<u64>,
    /// After each turn, suggest a likely next prompt (greyed out; tab takes it).
    /// Off by default: each suggestion is one more model call.
    pub prompt_suggestions: Option<bool>,
    /// A one-line recap of the session when you come back to the TUI after
    /// a few minutes away (on by default; Claude Code's `awaySummaryEnabled`).
    pub away_summary: Option<bool>,
    /// Idle compaction (Claude Code's `idleCompaction`; off by default here): a TUI
    /// session idle for `idle_compact_minutes` (default 54) whose conversation
    /// fills at least `idle_compact_percent` of the window (default 0.5) is
    /// compacted in the background, so the next prompt starts fast.
    pub idle_compaction: Option<bool>,
    pub idle_compact_minutes: Option<u64>,
    pub idle_compact_percent: Option<f32>,
    /// Desktop notifications when a long turn ends or a prompt waits for you:
    /// "auto" (default; picks from the terminal), "osc9", "osc777", "system", "bell" or "off".
    pub notifications: Option<String>,
    /// Working/waiting/done on the terminal tab through OSC 7501 (the Program
    /// Status Protocol): "auto" (default; terminals known to support it), "on" or "off".
    pub terminal_status: Option<String>,
    /// Keep the machine from idle-sleeping: "working" (default; only while a
    /// turn, loop or background run is going), "never" or "always".
    pub keep_awake: Option<String>,
    /// Sound played when a long turn ends unwatched (same rules as the
    /// notification): "off" (default), "default", a system sound name
    /// (e.g. "Glass" on macOS) or a sound file path.
    pub notify_sound: Option<String>,
    /// Show big pastes as `[Pasted text #n +N lines]` in the input (default
    /// true; ctrl+o on the tag expands it). False inserts them as typed text.
    pub collapse_pastes: Option<bool>,
    /// Copy a mouse drag selection to the clipboard as soon as it ends
    /// (default true; Claude Code's `copyOnSelect`). False keeps the
    /// selection highlighted so `ctrl+shift+c` copies it.
    #[serde(alias = "copyOnSelect")]
    pub copy_on_select: Option<bool>,
    /// Bash commands get the user's shell setup (PATH, other exported
    /// variables, aliases, functions), captured once per session from an
    /// interactive login shell (default true).
    pub shell_snapshot: Option<bool>,
    /// After a Bash command changes files in the project, show them as a
    /// diff (default true; Claude Code's `bashEditDiffEnabled`).
    pub bash_diff: Option<bool>,
    /// TUI colors: "auto" (default; dark or light from the terminal
    /// background), a built-in (dark, light, dracula, nord, solarized-dark,
    /// gruvbox, colorblind) or a file in ~/.chanto/themes/<name>.toml.
    pub theme: Option<String>,
    /// Screen-reader mode (Claude Code's `axScreenReader`; `--ax-screen-reader`):
    /// line-based output, no animation, and a bell when a prompt waits.
    #[serde(alias = "axScreenReader")]
    pub screen_reader: Option<bool>,
    /// TUI animation (spinner, live counters). `false` skips it
    /// (`--no-animations`); unset follows the macOS "Reduce motion" setting.
    pub animations: Option<bool>,
    /// How the TUI frames boxes (input, dialogs): "block" (default; a thin
    /// frame of block glyphs that can't leave hairline gaps), "line" (rounded
    /// outline; some fonts draw its corners short) or "solid".
    pub input_border: Option<String>,
    pub permissions: PermissionsConfig,
    pub web: WebConfig,
    pub browser: BrowserConfig,
    /// Computer use (the `Computer` tool): off unless enabled.
    pub computer: crate::tools::computer::ComputerConfig,
    #[serde(alias = "mcpServers")]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    pub hooks: HooksConfig,
    /// Claude Code's `disableAllHooks`: run no hooks (hooks from managed
    /// settings keep running unless managed settings set it).
    #[serde(alias = "disableAllHooks", skip_serializing_if = "Option::is_none")]
    pub disable_all_hooks: Option<bool>,
    /// Formatters run on each file an edit changes (`[format.commands]`, `auto`).
    pub format: crate::format::FileCommands,
    /// Linters run on each file an edit changes; findings go to the model.
    pub lint: crate::format::FileCommands,
    pub sandbox: SandboxConfig,
    /// Environment variables set for the session (tools, hooks, MCP servers).
    pub env: BTreeMap<String, String>,
    /// Which other agents' settings to import (Claude Code, Codex).
    pub compat: CompatConfig,
    /// Learning from the user's corrections (signals, approved lessons).
    pub learning: LearningConfig,
    /// Keeping secrets away from the model, transcripts and shell commands.
    pub secrets: crate::secrets::SecretsConfig,
    /// Output style name (built-in Concise, Explanatory, Learning or Proactive, or a file in output-styles/).
    pub output_style: Option<String>,
    /// Command that prints the API key (Claude Code's apiKeyHelper), run at startup.
    pub api_key_helper: Option<String>,
    /// First turn: list the project's files and attach small files the
    /// request names, saving discovery round trips (default true).
    pub primer: Option<bool>,
    /// Limits on parallel agents/commands and memory use.
    pub resources: crate::resources::ResourcesConfig,
    /// Verified mode: done-gate checks and test protection.
    pub guardrails: crate::guardrails::GuardrailsConfig,
    /// Extra named providers, referenced as `name/model` (e.g. "lmstudio/qwen2.5-coder").
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Embeddings for the `SemanticSearch` tool: which model to embed with, and where.
    pub embeddings: EmbeddingsConfig,
    pub lsp: crate::lsp::LspConfig,
    /// Background runs (`chanto run --bg`): autonomy, budgets and limits.
    pub background: crate::runs::BackgroundConfig,
    /// Run servers (`[remote.<name>]`): how code reaches them.
    pub remote: BTreeMap<String, crate::runs::RemoteConfig>,
    /// Notifications when background runs finish, fail or need input.
    pub notify: crate::runs::notify::NotifyConfig,
    /// Goals (`/goal`, `chanto goal`): iteration, no-progress, cost and time limits.
    pub goal: crate::goal::GoalConfig,
    /// Teams (`/team`, `chanto team`): agents at once and the total budget.
    pub team: crate::team::TeamConfig,
    /// Spend guard: fan-out estimates, live notices, day and month budgets.
    pub spend: crate::spend::SpendConfig,
    /// Voice input (ctrl+space / `/voice`, the web UI's mic): recording and transcription.
    pub voice: crate::voice::VoiceConfig,
    /// Opt-in error reports (`share_errors`); `/feedback` works either way.
    pub feedback: crate::feedback::FeedbackConfig,
    /// Experimental speed/token optimizations (`[optimize]`), each off by
    /// default while they're being measured. `CHANTO_OPTIMIZE` overrides.
    pub optimize: OptimizeConfig,
}

/// `[embeddings]`: the model `SemanticSearch` embeds with. Defaults to a local
/// Ollama `nomic-embed-text`; point it elsewhere with
/// `provider = "openai"` for any OpenAI-compatible `/v1/embeddings`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct EmbeddingsConfig {
    /// "ollama" (default) or "openai" (an OpenAI-compatible `/v1/embeddings`).
    pub provider: Option<String>,
    /// Model name (`nomic-embed-text`, `text-embedding-3-small`, …).
    pub model: Option<String>,
    /// Endpoint root; defaults per provider.
    pub base_url: Option<String>,
    /// API key for an OpenAI-compatible endpoint (falls back to OPENAI_API_KEY).
    pub api_key: Option<String>,
    /// Texts per embedding request (default 16).
    pub batch: Option<usize>,
}

/// `[optimize]`: experimental optimizations, off unless enabled. Each is a
/// separate switch so the benchmark can measure it on its own.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct OptimizeConfig {
    /// A system-prompt instruction to keep prose replies brief (never code,
    /// tool arguments, file contents or reasoning).
    pub concise_prose: Option<bool>,
    /// Quiet mode's model side: no narration between tool calls, one short
    /// summary at the end (`--quiet` also turns on the TUI's quiet view).
    pub quiet: Option<bool>,
    /// Flag final-answer numbers and identifiers absent from this turn's tool outputs.
    pub provenance: Option<bool>,
    /// Treat a written spec as the contract, and exercise the result against it
    /// even when the project has no test for it. Opt-in and not in `all`:
    /// across 30 MBPP tasks repeated three times it moved 69/90 to 73/90
    /// (within noise) for ~20% more tokens.
    pub spec_check: Option<bool>,
    /// Ask for low reasoning effort after a reply that only called tools.
    /// On by default: it costs prompt-cache reuse (DeepSeek re-reads the
    /// prompt uncached each time the effort changes, ~18% more cost on the
    /// beyond tier) but solved more: 67/68 vs 64/68 there, and 13/15 vs 9/15
    /// (fixed high 10/15, fixed low 12/15) on the tasks that flipped.
    /// `false` keeps the effort the same on every call.
    pub adaptive_effort: Option<bool>,
    /// Send only Chanto's fixed instructions as the system message and the
    /// per-project part (environment, git status, skills, subagents, MCP
    /// servers, instruction files) at the top of the first user message, so
    /// the system message and tool list are cached across sessions and
    /// directories. On by default: on the beyond tier it cut uncached input
    /// 40% with the same pass rate (65/68 vs 64/68); `false` turns it off.
    pub stable_prefix: Option<bool>,
    /// Keep rarely used built-in tools (`tool_search::DEFERRED_BUILTINS`) out
    /// of the tool list; the model loads them with ToolSearch when needed.
    pub defer_tools: Option<bool>,
    /// Auto mode: settle ALLOW from the probabilities of a one-token answer
    /// (no JSON generated); BLOCK still gets the full verdict and its reason.
    /// Falls back to the JSON verdict where the server reports no token
    /// probabilities. 14/14 and ~25% faster on DeepSeek in classifier_eval.
    pub token_classifier: Option<bool>,
}

impl OptimizeConfig {
    pub fn concise_prose(&self) -> bool {
        self.concise_prose.unwrap_or(false)
    }
    pub fn quiet(&self) -> bool {
        self.quiet.unwrap_or(false)
    }
    pub fn provenance(&self) -> bool {
        self.provenance.unwrap_or(false)
    }
    pub fn spec_check(&self) -> bool {
        self.spec_check.unwrap_or(false)
    }
    pub fn adaptive_effort(&self) -> bool {
        self.adaptive_effort.unwrap_or(true)
    }
    pub fn stable_prefix(&self) -> bool {
        self.stable_prefix.unwrap_or(true)
    }
    pub fn defer_tools(&self) -> bool {
        self.defer_tools.unwrap_or(false)
    }
    pub fn token_classifier(&self) -> bool {
        self.token_classifier.unwrap_or(false)
    }
    fn merge(&mut self, o: OptimizeConfig) {
        macro_rules! take {
            ($($f:ident),*) => { $(if o.$f.is_some() { self.$f = o.$f; })* };
        }
        take!(
            concise_prose,
            quiet,
            provenance,
            spec_check,
            adaptive_effort,
            stable_prefix,
            defer_tools,
            token_classifier
        );
    }
    /// `CHANTO_OPTIMIZE=concise,quiet,provenance,spec,defer,token,noprefix,noadaptive` (or `all` for the
    /// first three, or the full key names) turns those on, for benchmark runs;
    /// unknown names are returned.
    pub fn apply_env(&mut self, v: &str) -> Vec<String> {
        let mut unknown = Vec::new();
        for name in v.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            match name {
                "all" => {
                    self.concise_prose = Some(true);
                    self.quiet = Some(true);
                    self.provenance = Some(true);
                }
                "concise" | "concise_prose" => self.concise_prose = Some(true),
                "quiet" => self.quiet = Some(true),
                "provenance" => self.provenance = Some(true),
                "spec" | "spec_check" => self.spec_check = Some(true),
                "adaptive" | "adaptive_effort" => self.adaptive_effort = Some(true),
                "noadaptive" | "no_adaptive_effort" => self.adaptive_effort = Some(false),
                "prefix" | "stable_prefix" => self.stable_prefix = Some(true),
                "noprefix" | "no_stable_prefix" => self.stable_prefix = Some(false),
                "defer" | "defer_tools" => self.defer_tools = Some(true),
                "token" | "token_classifier" => self.token_classifier = Some(true),
                "none" | "off" => *self = OptimizeConfig::default(),
                o => unknown.push(o.to_string()),
            }
        }
        unknown
    }
    /// The switches that are on, by key name (for `/status` and bench metadata).
    pub fn enabled(&self) -> Vec<&'static str> {
        [
            ("concise_prose", self.concise_prose()),
            ("quiet", self.quiet()),
            ("provenance", self.provenance()),
            ("spec_check", self.spec_check()),
            ("no_adaptive_effort", !self.adaptive_effort()),
            // On by default, so listed when turned off.
            ("no_stable_prefix", !self.stable_prefix()),
            ("defer_tools", self.defer_tools()),
            ("token_classifier", self.token_classifier()),
        ]
        .into_iter()
        .filter(|(_, on)| *on)
        .map(|(k, _)| k)
        .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NetworkPolicy {
    #[default]
    Allow,
    Deny,
    Allowlist,
}

impl std::fmt::Display for NetworkPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Allowlist => "allowlist",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    /// Run Bash commands under the OS sandbox (macOS Seatbelt, Linux
    /// Landlock): writes are limited to the project, temp dirs and tool
    /// caches, and private keys can't be read. On Windows (a write-restricted
    /// token) only writes are confined. On by default.
    pub enabled: bool,
    /// Explicit read-only policy, independent of the mutable permission mode
    /// and whether the OS shell sandbox is enabled.
    pub read_only: bool,
    /// In auto mode, run sandboxed commands without consulting the classifier.
    pub auto_allow: bool,
    /// Extra writable paths.
    pub allow_write: Vec<String>,
    /// Block network access from sandboxed commands.
    pub deny_network: bool,
    /// Explicit policy takes precedence over the legacy deny_network flag.
    pub network: Option<NetworkPolicy>,
    /// None selects package registry defaults; an explicit empty list allows nothing.
    pub allowed_domains: Option<Vec<String>>,
    pub denied_domains: Vec<String>,
    /// Extra paths sandboxed commands can't read (private keys and
    /// credential files are always included).
    pub deny_read: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            read_only: false,
            auto_allow: false,
            allow_write: vec![],
            deny_network: false,
            network: None,
            allowed_domains: None,
            denied_domains: vec![],
            deny_read: vec![],
        }
    }
}

impl SandboxConfig {
    pub fn network_policy(&self) -> NetworkPolicy {
        self.network.unwrap_or(if self.deny_network {
            NetworkPolicy::Deny
        } else {
            NetworkPolicy::Allow
        })
    }

    pub fn network_domains(&self) -> Vec<String> {
        self.allowed_domains.clone().unwrap_or_else(|| {
            [
                "github.com",
                "api.github.com",
                "codeload.github.com",
                "objects.githubusercontent.com",
                "registry.npmjs.org",
                "*.npmjs.org",
                "pypi.org",
                "files.pythonhosted.org",
                "crates.io",
                "index.crates.io",
                "static.crates.io",
                "proxy.golang.org",
                "sum.golang.org",
                "repo.maven.apache.org",
                "rubygems.org",
                "registry.yarnpkg.com",
            ]
            .into_iter()
            .map(String::from)
            .collect()
        })
    }

    pub fn network_description(&self) -> String {
        match self.network_policy() {
            NetworkPolicy::Allowlist => format!(
                "allowlist (allowed: {}; denied: {})",
                self.network_domains().join(", "),
                self.denied_domains.join(", ")
            ),
            p => p.to_string(),
        }
    }

    pub fn set_policy(&mut self, policy: &str) -> Result<()> {
        match policy {
            "read-only" => self.read_only = true,
            "workspace-write" => {
                self.read_only = false;
                self.enabled = true;
            }
            "danger-full-access" => {
                self.read_only = false;
                self.enabled = false;
            }
            other => anyhow::bail!(
                "unknown sandbox policy {other} (read-only | workspace-write | danger-full-access)"
            ),
        }
        Ok(())
    }

    pub fn tool_denial(
        &self,
        tool: &dyn crate::tools::Tool,
        args: &serde_json::Value,
    ) -> Option<String> {
        use crate::tools::ToolKind;
        if !self.read_only {
            return None;
        }
        let allowed = match tool.kind_for(args) {
            ToolKind::ReadOnly | ToolKind::Network => true,
            ToolKind::External if tool.read_only_hint() => true,
            ToolKind::Interactive => {
                !(tool.name() == "Task" && args["isolation"].as_str() == Some("worktree"))
            }
            ToolKind::Execute => {
                !crate::tools::arg_bool(args, "dangerouslyDisableSandbox")
                    && crate::permissions::is_safe_command(args["command"].as_str().unwrap_or(""))
            }
            _ => false,
        };
        (!allowed).then(|| "Blocked by the explicit read-only sandbox policy. Changing permission mode or approving a plan cannot enable writes.".into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: Option<String>,
    pub num_ctx: Option<u32>,
    /// Cloud platforms: `region`, `profile`, `project`, `resource`, `api_version`, `api`.
    #[serde(flatten)]
    pub cloud: crate::cloud::Settings,
}

/// A base URL ending in `/v1` speaks the OpenAI chat API.
/// Top-level keys a config file may set.
fn known_keys() -> Vec<String> {
    let mut keys: Vec<String> = serde_json::to_value(Config::default())
        .ok()
        .and_then(|v| v.as_object().map(|o| o.keys().cloned().collect()))
        .unwrap_or_default();
    keys.extend(["mcpServers", "profile", "profiles", "axScreenReader"].map(String::from));
    keys
}

fn unknown_keys(raw: &toml::Table) -> Vec<String> {
    let known = known_keys();
    raw.keys().filter(|k| !known.contains(k)).cloned().collect()
}

fn openai_style_url(url: &str) -> bool {
    url.trim_end_matches('/').ends_with("/v1")
}

impl ProviderConfig {
    /// A client for this provider; `ctx` is used when `num_ctx` isn't set.
    pub fn provider(&self, ctx: u32) -> crate::llm::Provider {
        let url = if self.base_url.is_empty() {
            self.kind.default_base_url()
        } else {
            &self.base_url
        };
        crate::llm::Provider::new(
            self.kind,
            url,
            self.api_key.clone(),
            self.num_ctx.unwrap_or(ctx),
        )
        .with_cloud(self.cloud.clone())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PermissionsConfig {
    /// Rules like `Bash(git status:*)`, `Read`, `WebFetch(domain:docs.rs)`.
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub ask: Vec<String>,
    /// Extra plain-English guidance for the auto-mode classifier.
    pub auto_mode_rules: Vec<String>,
    /// Auto mode: clear routine shell commands with the built-in fast model
    /// instead of the model classifier (see classifier/README.md). Off by default.
    pub fast_path: bool,
    /// Auto mode: if a permission prompt goes unanswered for this many seconds,
    /// deny it so the turn can keep moving. Default 75; 0 waits indefinitely.
    pub auto_deny_after_secs: Option<u64>,
    /// Directories outside the project treated as part of the workspace.
    #[serde(alias = "additionalDirectories")]
    pub additional_directories: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WebConfig {
    /// auto | duckduckgo | brave | tavily | searxng
    pub search_backend: Option<String>,
    pub brave_api_key: Option<String>,
    pub tavily_api_key: Option<String>,
    pub searxng_url: Option<String>,
    /// Max characters of page content returned by WebFetch.
    pub fetch_max_chars: Option<usize>,
    /// Use the model to condense long pages against the prompt (slow on local models).
    pub condense_with_model: Option<bool>,
    /// Minimum gap between key-less searches to one engine, in ms (default 1500; 0 disables).
    pub search_min_gap_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrowserConfig {
    pub chrome_path: Option<String>,
    /// Remote debugging port. If something is already listening, attach to it.
    pub port: Option<u16>,
    pub headless: Option<bool>,
    /// Profile dir for the automation browser (keeps logins between runs).
    pub user_data_dir: Option<String>,
    /// Badge shown on the browser's dock/taskbar icon while Chanto is
    /// controlling it (`Browser.setDockTile`), so it's clear it isn't being
    /// driven by hand. Defaults to "AI"; set to "" to turn it off.
    pub badge: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct McpServerConfig {
    /// stdio transport: command to spawn
    pub command: String,
    /// HTTP transport: server URL (streamable HTTP)
    pub url: Option<String>,
    /// "sse" for the legacy HTTP+SSE transport (MCP 2024-11-05). Unset or
    /// "http": streamable HTTP, falling back to SSE when the server refuses it.
    /// Claude Code's `.mcp.json` calls this `type`.
    #[serde(alias = "type", skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub disabled: bool,
    /// Variables passed through from Chanto's own environment even when
    /// their names look secret (Codex's `env_vars`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub env_vars: Vec<String>,
    /// stdio transport: working directory for the server process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// How long the server may take to start (default 30s; 10s for imported servers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_timeout_sec: Option<f64>,
    /// Per tool call time limit (default: `mcp_tool_timeout`, `MCP_TOOL_TIMEOUT` or 600s).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_timeout_sec: Option<f64>,
    /// Only expose these tools (by the server's own tool names).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled_tools: Option<Vec<String>>,
    /// Hide these tools (applied after `enabled_tools`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub disabled_tools: Vec<String>,
    /// OAuth client settings for HTTP servers without dynamic registration.
    pub oauth: Option<crate::mcp_auth::OAuthConfig>,
    /// Set for servers imported from another agent's config ("Claude Code", "Codex").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imported_from: Option<String>,
    /// Tool search: true loads this server's tools upfront, false defers all
    /// of them (even ones the server marks always-load). Claude Code's `alwaysLoad`.
    #[serde(alias = "alwaysLoad", skip_serializing_if = "Option::is_none")]
    pub always_load: Option<bool>,
    /// Why this server must not start (policy, or turned off for this
    /// project); set while loading, never read from a file.
    #[serde(skip)]
    pub gate: Option<crate::mcp_policy::Gate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CompatConfig {
    /// Read Claude Code settings, instructions, skills, commands and agents.
    pub claude: bool,
    /// Read Codex config, AGENTS.md overrides, skills and prompts.
    pub codex: bool,
    /// Import Codex `prefix_rule` approvals from ~/.codex/rules as Bash permission rules.
    pub codex_rules: bool,
    /// Read Gemini CLI's GEMINI.md files (~/.gemini/GEMINI.md and per directory).
    pub gemini: bool,
    /// Read Cursor rules (`.cursor/rules/*.mdc`) and a legacy `.cursorrules`.
    pub cursor: bool,
    /// Read `.github/copilot-instructions.md` and `.github/instructions/*.instructions.md`.
    pub copilot: bool,
    /// Read the files `.aider.conf.yml` lists under `read:` (e.g. CONVENTIONS.md).
    pub aider: bool,
    /// Read opencode's `opencode.json` `instructions` and `.opencode/agent/*.md` agents.
    pub opencode: bool,
}

impl Default for CompatConfig {
    fn default() -> Self {
        CompatConfig {
            claude: true,
            codex: true,
            codex_rules: false,
            gemini: true,
            cursor: true,
            copilot: true,
            aider: true,
            opencode: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LearningConfig {
    /// Record correction signals locally (default true). Nothing is applied without approval.
    pub enabled: Option<bool>,
    /// Add approved lessons to the prompt (default true; `--no-lessons` turns it off).
    pub lessons: Option<bool>,
}

impl LearningConfig {
    pub fn capture(&self) -> bool {
        self.enabled != Some(false)
    }
    pub fn lessons_enabled(&self) -> bool {
        self.capture() && self.lessons != Some(false)
    }
}

/// Hooks by event name (one of `HOOK_EVENTS`).
pub type HooksConfig = BTreeMap<String, Vec<HookConfig>>;

/// Claude Code's hook events. All are accepted; `Setup` never fires (Chanto
/// has no `--init` / `--maintenance` runs).
pub const HOOK_EVENTS: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "UserPromptSubmit",
    "Stop",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "SessionStart",
    "SessionEnd",
    "Notification",
    "PermissionRequest",
    "Setup",
    "UserPromptExpansion",
    "PermissionDenied",
    "PostToolBatch",
    "MessageDisplay",
    "TaskCreated",
    "TaskCompleted",
    "StopFailure",
    "TeammateIdle",
    "InstructionsLoaded",
    "ConfigChange",
    "CwdChanged",
    "DirectoryAdded",
    "FileChanged",
    "WorktreeCreate",
    "WorktreeRemove",
    "PostCompact",
    "PreModelSwitch",
    "PostModelSwitch",
    "Elicitation",
    "ElicitationResult",
];

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct HookConfig {
    /// Matched against the tool name, or the event's subject (e.g. the
    /// SessionStart source, the PreCompact trigger), as in Claude Code: empty
    /// or `*` = all; only letters, digits, `_`, `-`, spaces, `,` and `|` = exact
    /// names separated by `|` or `,`; anything else = unanchored regex.
    pub matcher: String,
    /// Handler type: `command` (default), `http`, `mcp_tool`, `prompt` or `agent`.
    #[serde(rename = "type")]
    pub kind: String,
    /// `command` hooks: the shell command, or with `args` the executable.
    pub command: String,
    /// `command` hooks: exec form. `command` is spawned directly with these
    /// arguments, no shell involved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// `command` hooks: `bash` (default) or `powershell`. Ignored with `args`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// `command` hooks: run in the background without blocking; results
    /// reach the model on its next turn.
    #[serde(rename = "async", skip_serializing_if = "std::ops::Not::not")]
    pub run_async: bool,
    /// `command` hooks: run in the background; exit 2 wakes the model with the hook's output.
    #[serde(alias = "asyncRewake", skip_serializing_if = "std::ops::Not::not")]
    pub async_rewake: bool,
    /// `command` and `http` hooks: `continue` (default) or `block`: what a
    /// failing hook (can't start, bad exit, timeout, invalid output) does to the action.
    #[serde(alias = "onFailure", skip_serializing_if = "Option::is_none")]
    pub on_failure: Option<String>,
    /// `http` hooks: URL the JSON payload is POSTed to.
    pub url: String,
    /// `http` hooks: request headers; `$VAR`/`${VAR}` expand only for `allowed_env_vars`.
    pub headers: BTreeMap<String, String>,
    #[serde(alias = "allowedEnvVars")]
    pub allowed_env_vars: Vec<String>,
    /// `mcp_tool` hooks: the configured MCP server and the tool to call.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub server: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tool: String,
    /// `mcp_tool` hooks: the tool's arguments; `${path}` in strings is filled
    /// from the hook input (`${tool_input.file_path}`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
    /// `prompt` and `agent` hooks: what the model evaluates (`$ARGUMENTS` is the hook input).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub prompt: String,
    /// `prompt` and `agent` hooks: the model (default: the session's).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `prompt` hooks: on `ok: false`, feed the reason back and keep going
    /// instead of ending the turn (PreToolUse, PostToolUse, TeammateIdle).
    #[serde(alias = "continueOnBlock", skip_serializing_if = "std::ops::Not::not")]
    pub continue_on_block: bool,
    /// Shown while the hook runs.
    #[serde(alias = "statusMessage", skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    /// Skill hooks: removed after their first successful run.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub once: bool,
    /// Claude Code `if`: one permission rule (e.g. `Bash(git *)`) the tool call
    /// must match for the hook to run. Tool events only.
    #[serde(rename = "if", skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// Extra environment for command hooks (plugin hooks get
    /// CLAUDE_PLUGIN_ROOT and CLAUDE_PLUGIN_DATA here).
    pub env: BTreeMap<String, String>,
    #[serde(alias = "timeout")]
    pub timeout_secs: Option<u64>,
    /// Claude Code layout: `{"matcher": "Bash", "hooks": [{"type": "command", "command": "..."}]}`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<HookConfig>,
}

impl HookConfig {
    /// What the hook runs, for listings: the command, `POST <url>`, the MCP
    /// tool or the prompt, plus its `if` condition.
    pub fn describe(&self) -> String {
        let what = match self.kind.as_str() {
            "http" => format!("POST {}", self.url),
            "mcp_tool" => format!("mcp {}/{}", self.server, self.tool),
            "prompt" | "agent" => {
                format!("{}: {}", self.kind, crate::util::one_line(&self.prompt, 60))
            }
            _ => match &self.args {
                Some(a) => format!("{} {}", self.command, shell_words::join(a)),
                None => self.command.clone(),
            },
        };
        match &self.condition {
            Some(c) => format!("{what} (if {c})"),
            None => what,
        }
    }
}

/// Whether Chanto can run a hook handler of this type with these fields.
pub fn runnable_hook(h: &HookConfig) -> bool {
    match h.kind.as_str() {
        "" | "command" => !h.command.trim().is_empty(),
        "http" => !h.url.trim().is_empty(),
        "mcp_tool" => !h.server.trim().is_empty() && !h.tool.trim().is_empty(),
        "prompt" | "agent" => !h.prompt.trim().is_empty(),
        _ => false,
    }
}

/// Expand Claude-style nested hook groups into one entry per handler.
/// Returns a description of each handler that was dropped because Chanto
/// can't run its type, or it lacks the fields its type needs.
pub fn flatten_hooks(h: &mut HooksConfig) -> Vec<String> {
    let mut dropped = Vec::new();
    for (event, list) in h.iter_mut() {
        let mut out = Vec::new();
        for mut e in list.drain(..) {
            let nested = std::mem::take(&mut e.hooks);
            if runnable_hook(&e) {
                if e.kind.is_empty() {
                    e.kind = "command".into();
                }
                out.push(e.clone());
            } else if !e.kind.is_empty() && e.kind != "command" {
                dropped.push(format!("{event} hook of type `{}`", e.kind));
            }
            for mut c in nested {
                c.hooks.clear();
                if !runnable_hook(&c) {
                    if !c.kind.is_empty() && c.kind != "command" {
                        dropped.push(format!("{event} hook of type `{}`", c.kind));
                    }
                    continue;
                }
                if c.kind.is_empty() {
                    c.kind = "command".into();
                }
                c.matcher = e.matcher.clone();
                c.condition = c.condition.or_else(|| e.condition.clone());
                c.timeout_secs = c.timeout_secs.or(e.timeout_secs);
                out.push(c);
            }
        }
        *list = out;
    }
    h.retain(|_, v| !v.is_empty());
    dropped
}

/// Per-project allow rules chosen by the user ("don't ask again"). Stored
/// under ~/.chanto (not in the repo) so a cloned project can't forge them.
fn project_rules_file() -> PathBuf {
    home_dir().join("project-permissions.json")
}

fn project_key(cwd: &Path) -> String {
    cwd.canonical()
        .unwrap_or_else(|_| cwd.to_path_buf())
        .display()
        .to_string()
}

pub fn persist_allow_rule(cwd: &Path, rule: &str) -> Result<PathBuf> {
    let path = project_rules_file();
    let mut map: BTreeMap<String, Vec<String>> = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let list = map.entry(project_key(cwd)).or_default();
    if !list.iter().any(|r| r == rule) {
        list.push(rule.to_string());
    }
    std::fs::create_dir_all(home_dir())?;
    std::fs::write(&path, serde_json::to_vec_pretty(&map)?)?;
    Ok(path)
}

fn saved_project_rules(cwd: &Path) -> Vec<String> {
    let map: BTreeMap<String, Vec<String>> = std::fs::read(project_rules_file())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    map.get(&project_key(cwd)).cloned().unwrap_or_default()
}

/// Parameter count in billions from a model name (`qwen3:4b`,
/// `llama-3.1-8b-instruct`, `gpt-oss-20b`), when it states one.
pub fn model_billions(name: &str) -> Option<f64> {
    let n = name.to_ascii_lowercase();
    let b = n.as_bytes();
    (0..b.len())
        .filter(|&i| b[i] == b'b' && i > 0 && b[i - 1].is_ascii_digit())
        .filter(|&i| b.get(i + 1).is_none_or(|c| !c.is_ascii_alphanumeric()))
        .find_map(|i| {
            let start = n[..i]
                .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
                .map_or(0, |p| p + 1);
            // `e2b`-style names (an expert count) aren't sizes.
            let prev = n[..start].chars().last();
            (prev.is_none_or(|c| !c.is_ascii_alphabetic()))
                .then(|| n[start..i].parse().ok())
                .flatten()
        })
}

pub fn home_dir() -> PathBuf {
    if let Ok(h) = std::env::var("CHANTO_HOME") {
        return PathBuf::from(h);
    }
    // Tests never touch the real ~/.chanto (logs, sessions, error-report state).
    #[cfg(test)]
    {
        static TEST_HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        return TEST_HOME
            .get_or_init(|| {
                std::env::temp_dir().join(format!("chanto-test-home-{}", std::process::id()))
            })
            .clone();
    }
    #[allow(unreachable_code)]
    resolve_home(
        dirs::home_dir().as_deref(),
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .as_deref(),
    )
}

/// Whether two paths name the same existing file.
fn same_file(a: &Path, b: &Path) -> bool {
    matches!((a.canonical(), b.canonical()), (Ok(x), Ok(y)) if x == y)
}

/// Where Chanto keeps its files when `CHANTO_HOME` is unset: an existing
/// `~/.chanto` always wins (upgrading never moves anyone's data); otherwise
/// `$XDG_CONFIG_HOME/chanto` when that is set to an absolute path (the XDG
/// spec says to ignore relative ones); otherwise `~/.chanto`. One root keeps
/// config, sessions and logs together, as `~/.chanto` does.
fn resolve_home(home: Option<&Path>, xdg_config: Option<&Path>) -> PathBuf {
    let legacy = home.unwrap_or(Path::new(".")).join(".chanto");
    if legacy.exists() {
        return legacy;
    }
    match xdg_config {
        Some(x) if x.is_absolute() => x.join("chanto"),
        _ => legacy,
    }
}

/// Settings a project directory could use to run code or weaken safety.
/// They are ignored until the user trusts the directory.
///
/// `browser`/`web` belong here because a project could point `chrome_path` at an
/// arbitrary binary (executed when the agent browses) or redirect every search
/// through an attacker's endpoint; `output_style` can inject instructions into
/// the system prompt.
const RISKY_KEYS: &[&str] = &[
    "telemetry",
    "pricing",
    "mcp_servers",
    "hooks",
    "format",
    "lint",
    "account",
    "disable_all_hooks",
    "disableAllHooks",
    "mcpServers",
    "status_line",
    "mode",
    "base_url",
    "api_key",
    "providers",
    "sandbox",
    "provider",
    "env",
    "compat",
    "lsp",
    "guardrails",
    "secrets",
    "api_key_helper",
    "browser",
    "computer",
    "web",
    "output_style",
    "background",
    "remote",
    "notify",
    "voice",
    "feedback",
    "embeddings",
    // The model that judges auto-mode actions: a project could name a weak
    // or permissive one.
    "classifier_model",
];

fn trust_file() -> PathBuf {
    home_dir().join("trusted.json")
}

/// Is `dir` (or one of its ancestors) in the trusted list?
pub fn is_trusted(dir: &Path) -> bool {
    // A background run's worktree inherits its source repository's trust.
    if let Some((root, _)) = crate::runs::split_worktree_path(dir) {
        return crate::runs::source_repo_of(&root)
            .is_some_and(|src| !src.starts_with(crate::runs::worktrees_dir()) && is_trusted(&src));
    }
    let list: Vec<PathBuf> = std::fs::read(trust_file())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let dir = dir.canonical().unwrap_or_else(|_| dir.to_path_buf());
    trusted_by_chanto(&list, &dir) || crate::compat::trusted_elsewhere(&dir)
}

fn trusted_by_chanto(list: &[PathBuf], dir: &Path) -> bool {
    list.iter().any(|t| trust_covers(t, dir))
}

/// Whether trusting `t` covers `dir`: an exact match always, and a descendant
/// unless `t` is so broad that trusting it would sweep in unrelated repos —
/// the filesystem root, or the home directory and its ancestors. Trusting
/// `~/code/app` still covers `~/code/app/sub`; trusting `~` does not cover
/// `~/code/app`.
pub(crate) fn trust_covers(t: &Path, dir: &Path) -> bool {
    if dir == t {
        return true;
    }
    if !dir.starts_with(t) {
        return false;
    }
    let broad = t.parent().is_none() || home_dir_real().is_some_and(|h| h.starts_with(t));
    !broad
}

fn home_dir_real() -> Option<PathBuf> {
    let h = dirs::home_dir()?;
    Some(h.canonical().unwrap_or(h))
}

/// Trusted through Chanto itself (not only through Claude Code / Codex).
pub fn is_trusted_here(dir: &Path) -> bool {
    let list: Vec<PathBuf> = std::fs::read(trust_file())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    trusted_by_chanto(
        &list,
        &dir.canonical().unwrap_or_else(|_| dir.to_path_buf()),
    )
}

pub fn trust(dir: &Path) -> Result<()> {
    let mut list: Vec<PathBuf> = std::fs::read(trust_file())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let dir = dir.canonical().unwrap_or_else(|_| dir.to_path_buf());
    if !list.contains(&dir) {
        list.push(dir);
    }
    std::fs::create_dir_all(home_dir())?;
    std::fs::write(trust_file(), serde_json::to_vec_pretty(&list)?)?;
    Ok(())
}

impl Config {
    #[cfg(test)]
    pub fn flatten_hooks_for_test(&mut self) {
        let _ = flatten_hooks(&mut self.hooks);
    }

    /// `disableAllHooks: true` turns hooks off; hooks from managed settings
    /// (`managed`) stay unless managed settings themselves set it.
    fn apply_disable_all_hooks(&mut self, managed: &HooksConfig, managed_disables: bool) {
        if self.disable_all_hooks != Some(true) {
            return;
        }
        if managed_disables {
            self.hooks.clear();
            return;
        }
        for (event, list) in self.hooks.iter_mut() {
            let keep = managed.get(event).cloned().unwrap_or_default();
            list.retain(|h| keep.contains(h));
        }
        self.hooks.retain(|_, v| !v.is_empty());
    }

    /// Load configuration. When `trusted` is false, project-level settings
    /// that can execute code or loosen permissions are skipped and returned
    /// in the second element so the caller can ask the user.
    pub fn load(cwd: &Path, trusted: bool) -> Result<(Config, Vec<String>)> {
        Self::load_inner(cwd, trusted, false, crate::launch::get())
    }

    pub fn load_for_doctor(cwd: &Path, trusted: bool) -> Result<(Config, Vec<String>)> {
        Self::load_inner(cwd, trusted, true, crate::launch::get())
    }

    /// `load` with explicit launch flags (`--settings`, `--mcp-config`, …).
    #[cfg(test)]
    pub fn load_with(
        cwd: &Path,
        trusted: bool,
        launch: &crate::launch::Launch,
    ) -> Result<(Config, Vec<String>)> {
        Self::load_inner(cwd, trusted, false, launch)
    }

    fn load_inner(
        cwd: &Path,
        trusted: bool,
        diagnostic: bool,
        launch: &crate::launch::Launch,
    ) -> Result<(Config, Vec<String>)> {
        let mut cfg = Config::default();
        let mut skipped: Vec<String> = Vec::new();
        let parse_errors = std::cell::RefCell::new(Vec::new());
        let read_toml = |p: &Path| -> Result<Option<toml::Table>> {
            match std::fs::read_to_string(p) {
                Ok(text) => match toml::from_str(&text) {
                    Ok(table) => Ok(Some(table)),
                    Err(e) if diagnostic => {
                        let pos = e.span().map(|s| s.start).unwrap_or(0).min(text.len());
                        let prefix = &text[..text.floor_char_boundary(pos)];
                        let message = format!(
                            "Skipped {}: parse error at line {}, column {}; fix the file",
                            p.display(),
                            prefix.bytes().filter(|b| *b == b'\n').count() + 1,
                            prefix.rsplit('\n').next().unwrap_or("").len() + 1
                        );
                        if !parse_errors.borrow().contains(&message) {
                            parse_errors.borrow_mut().push(message);
                        }
                        Ok(None)
                    }
                    Err(e) => Err(e).with_context(|| format!("parsing {}", p.display())),
                },
                Err(_) => Ok(None),
            }
        };
        // Which compat sources to read is decided by the user's own config only.
        let global = home_dir().join("config.toml");
        let mut compat: CompatConfig = read_toml(&global)?
            .and_then(|t| t.get("compat").cloned())
            .and_then(|v| v.try_into().ok())
            .unwrap_or_default();
        if std::env::var("CHANTO_NO_CLAUDE_CONFIG").as_deref() == Ok("1") {
            compat.claude = false;
        }
        if std::env::var("CHANTO_NO_CODEX_CONFIG").as_deref() == Ok("1") {
            compat.codex = false;
        }
        cfg.diagnostics = crate::compat::diagnostics(cwd, &compat);
        // Layers, lowest precedence first: (source, is_project, table).
        // `--setting-sources` picks among user, project and local files.
        let mut layers: Vec<(String, bool, toml::Table)> = Vec::new();
        if launch.wants("user") {
            layers.extend(crate::compat::user_layers(cwd, &compat));
        }
        layers.extend(crate::compat::plugin_layers(cwd, &compat, trusted));
        if launch.wants("user")
            && let Some(t) = read_toml(&global)?
        {
            layers.push((global.display().to_string(), false, t));
        }
        let local_file =
            |src: &str| src.ends_with("settings.local.json") || src.ends_with("config.local.toml");
        layers.extend(
            crate::compat::project_layers(cwd, &compat)
                .into_iter()
                .filter(|(src, _, _)| {
                    launch.wants(if local_file(src) { "local" } else { "project" })
                }),
        );
        for p in [
            cwd.join(".chanto").join("config.toml"),
            cwd.join(".chanto").join("config.local.toml"),
        ] {
            if !launch.wants(if local_file(&p.to_string_lossy()) {
                "local"
            } else {
                "project"
            }) {
                continue;
            }
            // Started in the home directory: ~/.chanto/config.toml is the
            // user's own config (read above), not an untrusted project's.
            if same_file(&p, &global) {
                continue;
            }
            if let Some(t) = read_toml(&p)? {
                layers.push((p.display().to_string(), true, t));
            }
        }
        // `--bare` and `--strict-mcp-config`: no hooks or MCP servers found on disk.
        for (_, _, t) in layers.iter_mut() {
            if launch.no_customizations() {
                t.remove("hooks");
            }
            if launch.skip_discovered_mcp() {
                t.remove("mcp_servers");
            }
        }
        // [profiles.<name>] tables, applied after every layer when selected.
        let mut profiles: BTreeMap<String, Vec<(String, bool, toml::Table)>> = BTreeMap::new();
        let mut layers: Vec<(String, bool, toml::Table)> = layers;
        for (src, project, raw) in layers.iter_mut() {
            if let Some(toml::Value::Table(p)) = raw.remove("profiles") {
                for (name, v) in p {
                    if let toml::Value::Table(t) = v {
                        profiles.entry(name).or_default().push((
                            format!("{src} [profiles.{}]", "…"),
                            *project,
                            t,
                        ));
                    }
                }
            }
        }
        let selected = std::env::var("CHANTO_PROFILE")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                layers.iter().rev().find_map(|(_, project, raw)| {
                    (!*project || trusted)
                        .then(|| {
                            raw.get("profile")
                                .and_then(|v| v.as_str())
                                .map(str::to_string)
                        })
                        .flatten()
                })
            });
        if let Some(name) = &selected {
            match profiles.remove(name) {
                Some(p) => layers.extend(
                    p.into_iter()
                        .map(|(src, project, t)| (src.replace("…", name), project, t)),
                ),
                None => anyhow::bail!(
                    "unknown profile {name:?} (known: {})",
                    if profiles.is_empty() {
                        "none".to_string()
                    } else {
                        profiles.keys().cloned().collect::<Vec<_>>().join(", ")
                    }
                ),
            }
        }
        // `--settings`: above every file, below managed settings (as in Claude Code).
        layers.extend(
            launch
                .settings
                .iter()
                .map(|(src, t)| (src.clone(), false, t.clone())),
        );
        // Claude Code managed settings (organization policy): above every other
        // layer, profiles included, whatever `[compat]` says. Rule lists merge, so
        // nothing lower can drop a managed deny; managed `env` wins.
        let managed = crate::compat::managed_settings(&crate::compat::managed_dir());
        if !managed.errors.is_empty() {
            if !diagnostic {
                anyhow::bail!("{}", managed.errors.join("\n"));
            }
            cfg.diagnostics.extend(managed.errors.iter().cloned());
        }
        let managed_start = layers.len();
        layers.extend(
            managed
                .layers
                .iter()
                .map(|(src, t)| (src.clone(), false, t.clone())),
        );
        // Hooks and `disableAllHooks` from managed settings, which a lower layer can't turn off.
        let (mut managed_hooks, mut managed_disables) = (HooksConfig::new(), false);
        for (i, (src, project, mut raw)) in layers.into_iter().enumerate() {
            cfg.layers.push(src.clone());
            raw.remove("profile");
            if project && !trusted {
                for k in RISKY_KEYS {
                    if raw.remove(*k).is_some() {
                        skipped.push(format!("{k} ({src})"));
                    }
                }
                // The one-token classifier settles ALLOW without the JSON
                // verdict: the user's choice, not a repository's.
                if let Some(toml::Value::Table(opt)) = raw.get_mut("optimize")
                    && opt.remove("token_classifier").is_some()
                {
                    skipped.push(format!("optimize.token_classifier ({src})"));
                }
                if let Some(toml::Value::Table(perm)) = raw.get_mut("permissions") {
                    // Everything here can loosen the agent's safety, so an
                    // untrusted project must not be able to set it. `auto_mode_rules`
                    // is the dangerous one: its text is fed straight to the
                    // auto-mode classifier as "additional rules from the user", so
                    // a repo could instruct the model to approve everything.
                    for k in [
                        "allow",
                        "additional_directories",
                        "additionalDirectories",
                        "auto_mode_rules",
                        "autoModeRules",
                        "fast_path",
                        "fastPath",
                        // `0` would make an unanswered auto-mode prompt wait
                        // forever, wedging an unattended run in a repo the user
                        // never chose to trust.
                        "auto_deny_after_secs",
                        "autoDenyAfterSecs",
                    ] {
                        if perm.remove(k).is_some() {
                            skipped.push(format!("permissions.{k} ({src})"));
                        }
                    }
                }
            }
            let mut layer: Config = match toml::Value::Table(raw.clone()).try_into() {
                Ok(layer) => layer,
                Err(_) if diagnostic || !src.contains(".chanto") => {
                    cfg.diagnostics.push(format!(
                        "Ignored {src}: invalid settings; fix file or disable compat"
                    ));
                    continue;
                }
                Err(e) => return Err(anyhow::anyhow!("parsing {src}: {e}")),
            };
            for key in raw.keys() {
                cfg.origins.insert(key.clone(), PathBuf::from(&src));
            }
            // A misspelt key would otherwise be dropped without a word.
            if src.contains(".chanto") {
                for key in unknown_keys(&raw) {
                    let hint = crate::util::closest(&key, known_keys().into_iter())
                        .map(|k| format!(" (did you mean `{k}`?)"))
                        .unwrap_or_default();
                    cfg.diagnostics.push(format!(
                        "Unknown setting `{key}` in {src}{hint}; it was ignored"
                    ));
                }
            }
            // Servers merge across layers, so each one remembers its own file.
            for name in raw
                .get("mcp_servers")
                .and_then(|s| s.as_table())
                .into_iter()
                .flat_map(|t| t.keys())
            {
                cfg.origins
                    .insert(format!("mcp_servers.{name}"), PathBuf::from(&src));
            }
            for d in flatten_hooks(&mut layer.hooks) {
                cfg.diagnostics.push(format!("Ignored {d} ({src}): chanto runs command, http, mcp_tool, prompt and agent hooks"));
            }
            if i >= managed_start {
                for (ev, list) in &layer.hooks {
                    managed_hooks
                        .entry(ev.clone())
                        .or_default()
                        .extend(list.iter().cloned());
                }
                managed_disables |= layer.disable_all_hooks == Some(true);
            }
            cfg.merge(layer, &raw);
        }
        cfg.apply_disable_all_hooks(&managed_hooks, managed_disables);
        // Claude-compatible MCP config files.
        // A project's `.mcp.json` servers need approval, as in Claude Code: trusting
        // the folder in Chanto approves them all; otherwise `enabledMcpjsonServers`
        // / `enableAllProjectMcpServers` (from outside the repo, or from its own
        // settings once trusted in Claude Code) or `/mcp enable`. `disabledMcpjsonServers`
        // always rejects.
        let borrowed = trusted
            && !is_trusted_here(cwd)
            && crate::compat::trusted_elsewhere(
                &cwd.canonical().unwrap_or_else(|_| cwd.to_path_buf()),
            );
        let approvals =
            crate::mcp_policy::Approvals::load(cwd, &compat, trusted && !borrowed, trusted);
        for (i, p) in [cwd.join(".mcp.json"), home_dir().join("mcp.json")]
            .into_iter()
            .enumerate()
        {
            if launch.skip_discovered_mcp()
                || !launch.wants(if i == 0 { "project" } else { "user" })
            {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&p) {
                cfg.layers.push(p.display().to_string());
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
                    && let Some(servers) = v.get("mcpServers").and_then(|s| s.as_object())
                {
                    for (name, s) in servers {
                        match if i == 0 {
                            approvals.decide(name)
                        } else {
                            crate::mcp_policy::Approval::Approved
                        } {
                            crate::mcp_policy::Approval::Approved => {}
                            crate::mcp_policy::Approval::Rejected => {
                                cfg.diagnostics.push(format!(
                                    "MCP server {name} ({}) is rejected by disabledMcpjsonServers",
                                    p.display()
                                ));
                                continue;
                            }
                            crate::mcp_policy::Approval::Pending => {
                                skipped.push(format!("MCP server {name} ({}; not approved: trust the folder or run /mcp enable {name})", p.display()));
                                continue;
                            }
                        }
                        if let Ok(sc) = serde_json::from_value::<McpServerConfig>(s.clone())
                            && !cfg.mcp_servers.contains_key(name)
                        {
                            cfg.origins.entry("mcp_servers".into()).or_insert(p.clone());
                            cfg.origins.insert(format!("mcp_servers.{name}"), p.clone());
                            cfg.mcp_servers.insert(name.clone(), sc);
                        }
                    }
                }
            }
        }
        // `--mcp-config` servers win a name clash with any file's.
        for (src, servers) in &launch.mcp {
            cfg.layers.push(src.clone());
            for (name, sc) in servers {
                cfg.origins
                    .insert(format!("mcp_servers.{name}"), PathBuf::from(src));
                cfg.mcp_servers.insert(name.clone(), sc.clone());
            }
        }
        cfg.diagnostics
            .extend(skipped.iter().map(|s| format!("Untrusted: {s}")));
        // allowedMcpServers / deniedMcpServers, and servers turned off for this project.
        crate::mcp_policy::apply(&mut cfg.mcp_servers, cwd, &compat, trusted);
        if let Some(t) = cfg.mcp_tool_timeout.filter(|t| t.is_finite() && *t > 0.0) {
            for s in cfg.mcp_servers.values_mut() {
                s.tool_timeout_sec.get_or_insert(t);
            }
        }
        cfg.compat = compat;
        cfg.permissions.allow.extend(saved_project_rules(cwd));
        // `allowManagedPermissionRulesOnly`: only the policy's own allow rules count.
        if managed.rules_only {
            cfg.permissions.allow = managed.allow_rules();
        }
        cfg.policy_no_bypass = managed.disable_bypass;
        cfg.policy_mode = managed
            .default_mode
            .as_deref()
            .and_then(crate::permissions::Mode::parse)
            .filter(|m| !(cfg.policy_no_bypass && *m == crate::permissions::Mode::Bypass));
        if let Some(m) = cfg.policy_mode {
            cfg.mode = Some(m);
            cfg.origins
                .insert("mode".into(), PathBuf::from("managed settings"));
        }
        if cfg.policy_no_bypass && cfg.mode == Some(crate::permissions::Mode::Bypass) {
            cfg.mode = Some(crate::permissions::Mode::Default);
            cfg.diagnostics.push("Bypass permissions mode is disabled by your organization's managed settings; starting in default mode".into());
        }
        if let Ok(v) = std::env::var("CHANTO_MODEL") {
            cfg.model = Some(v);
        }
        if let Ok(v) = std::env::var("CHANTO_BASE_URL") {
            cfg.base_url = Some(v);
        }
        if let Ok(v) = std::env::var("CHANTO_API_KEY") {
            cfg.api_key = Some(v);
        }
        if let Ok(v) = std::env::var("CHANTO_PROVIDER")
            && let Some(k) = ProviderKind::parse(&v)
        {
            cfg.provider = k;
        }
        // A cloud run's own checks (`checks` on the run or its agent), one per
        // line; set but blank turns verified mode off (`setup_session`).
        if let Ok(v) = std::env::var(chanto_proto::runs::CHECKS_ENV) {
            cfg.guardrails.checks = v
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect();
            cfg.origins.insert(
                "guardrails.checks".into(),
                PathBuf::from(format!("env:{}", chanto_proto::runs::CHECKS_ENV)),
            );
        }
        if let Ok(v) = std::env::var("CHANTO_OPTIMIZE") {
            for name in cfg.optimize.apply_env(&v) {
                cfg.diagnostics.push(format!("CHANTO_OPTIMIZE: unknown optimization `{name}` (use concise, quiet, provenance, spec or all)"));
            }
            cfg.origins
                .insert("optimize".into(), PathBuf::from("env:CHANTO_OPTIMIZE"));
        }
        if let Some(size) = cfg.classifier_model.as_deref().and_then(model_billions)
            && size < 7.0
        {
            cfg.diagnostics.push(format!(
                "classifier_model `{}` looks like a {size}B model: models under ~7B let dangerous actions through \
                 in auto mode (a 4B model allowed 22 of 96 dangerous test cases in our benchmark; 8B and up allowed none)",
                cfg.classifier_model.as_deref().unwrap_or_default()
            ));
        }
        if cfg.web.brave_api_key.is_none() {
            cfg.web.brave_api_key = std::env::var("BRAVE_API_KEY").ok();
        }
        if cfg.web.tavily_api_key.is_none() {
            cfg.web.tavily_api_key = std::env::var("TAVILY_API_KEY").ok();
        }
        for (var, key) in [
            ("CHANTO_MODEL", "model"),
            ("CHANTO_BASE_URL", "base_url"),
            ("CHANTO_PROVIDER", "provider"),
            ("CHANTO_API_KEY", "api_key"),
        ] {
            if std::env::var_os(var).is_some() {
                cfg.origins
                    .insert(key.into(), PathBuf::from(format!("env:{var}")));
            }
        }
        // A top-level `/v1` endpoint speaks OpenAI's API (LiteLLM, vLLM, LM
        // Studio, Ollama's own /v1) unless a provider was named explicitly.
        if cfg.provider == ProviderKind::Ollama
            && cfg.base_url.as_deref().is_some_and(openai_style_url)
            && !cfg.origins.contains_key("provider")
        {
            cfg.provider = ProviderKind::Openai;
        }
        // `[providers.bedrock]` (and vertex, azure, foundry) without a `kind`
        // configures that cloud platform.
        for (name, pc) in cfg.providers.iter_mut() {
            if let Some(h) = crate::providers::find(name)
                .filter(|h| h.kind.is_cloud() && pc.kind == ProviderKind::Ollama)
            {
                pc.kind = h.kind;
            } else if pc.kind == ProviderKind::Ollama && openai_style_url(&pc.base_url) {
                // No `kind`, but a `/v1` URL: an OpenAI-compatible server (LM Studio,
                // vLLM, LiteLLM, Ollama's own /v1), never Ollama's native API.
                pc.kind = ProviderKind::Openai;
            }
        }
        cfg.diagnostics.extend(parse_errors.into_inner());
        Ok((cfg, skipped))
    }

    /// Merge a layer; `raw` tells which keys the layer actually set.
    fn merge(&mut self, o: Config, raw: &toml::Table) {
        macro_rules! opt {
            ($($f:ident),*) => { $( if o.$f.is_some() { self.$f = o.$f; } )* };
        }
        opt!(
            base_url,
            api_key,
            model,
            num_ctx,
            temperature,
            think,
            effort,
            show_thinking,
            tui,
            mode,
            classifier_model,
            subagent_model,
            oracle_model,
            resume_after_limit,
            account,
            fallback_model,
            tool_search,
            compact_threshold,
            stream_idle_timeout,
            mcp_tool_timeout,
            max_turns,
            max_cost_usd,
            max_cost_usd_per_day,
            tool_protocol,
            status_line,
            notify_after_secs,
            prompt_suggestions,
            away_summary,
            idle_compaction,
            idle_compact_minutes,
            idle_compact_percent,
            notifications,
            terminal_status,
            keep_awake,
            notify_sound,
            collapse_pastes,
            copy_on_select,
            apply_patch_tool,
            shell_snapshot,
            bash_diff,
            theme,
            input_border,
            output_style,
            api_key_helper,
            primer,
            disable_all_hooks,
            screen_reader,
            animations
        );
        if let Some(toml::Value::Table(h)) = raw.get("history") {
            if h.contains_key("max_file_bytes") {
                self.history.max_file_bytes = o.history.max_file_bytes;
            }
            if h.contains_key("max_snapshot_bytes") {
                self.history.max_snapshot_bytes = o.history.max_snapshot_bytes;
            }
            if h.contains_key("timeout_ms") {
                self.history.timeout_ms = o.history.timeout_ms;
            }
            if h.contains_key("retention_days") {
                self.history.retention_days = o.history.retention_days;
            }
        }
        if let Some(toml::Value::Table(m)) = raw.get("memory") {
            if m.contains_key("auto") {
                self.memory.auto = o.memory.auto;
            }
            if m.contains_key("scope") {
                self.memory.scope = o.memory.scope;
            }
        }
        if raw.contains_key("provider") {
            self.provider = o.provider;
        }
        self.pricing.extend(o.pricing);
        if let Some(toml::Value::Table(u)) = raw.get("updates")
            && u.contains_key("check")
        {
            self.updates.check = o.updates.check;
        }
        if raw.contains_key("telemetry") {
            self.telemetry = o.telemetry;
        }
        if o.feedback.share_errors.is_some() {
            self.feedback.share_errors = o.feedback.share_errors;
        }
        self.permissions.allow.extend(o.permissions.allow);
        self.permissions.deny.extend(o.permissions.deny);
        self.permissions.ask.extend(o.permissions.ask);
        self.permissions
            .auto_mode_rules
            .extend(o.permissions.auto_mode_rules);
        self.permissions
            .additional_directories
            .extend(o.permissions.additional_directories);
        macro_rules! sub {
            ($s:ident: $($f:ident),*) => { $( if o.$s.$f.is_some() { self.$s.$f = o.$s.$f; } )* };
        }
        sub!(web: search_backend, brave_api_key, tavily_api_key, searxng_url, fetch_max_chars, condense_with_model, search_min_gap_ms);
        sub!(browser: chrome_path, port, headless, user_data_dir, badge);
        self.mcp_servers.extend(o.mcp_servers);
        self.providers.extend(o.providers);
        if let Some(toml::Value::Table(sb)) = raw.get("sandbox") {
            if sb.contains_key("enabled") {
                self.sandbox.enabled = o.sandbox.enabled;
            }
            if sb.contains_key("read_only") {
                self.sandbox.read_only = o.sandbox.read_only;
            }
            if sb.contains_key("auto_allow") {
                self.sandbox.auto_allow = o.sandbox.auto_allow;
            }
            if sb.contains_key("deny_network") {
                self.sandbox.deny_network = o.sandbox.deny_network;
            }
            if sb.contains_key("network") {
                self.sandbox.network = o.sandbox.network;
            }
            if sb.contains_key("allowed_domains") {
                self.sandbox.allowed_domains = o.sandbox.allowed_domains;
            }
            self.sandbox.denied_domains.extend(o.sandbox.denied_domains);
            self.sandbox.allow_write.extend(o.sandbox.allow_write);
            self.sandbox.deny_read.extend(o.sandbox.deny_read);
        }
        if raw.contains_key("format") {
            self.format.merge(o.format);
        }
        if raw.contains_key("lint") {
            self.lint.merge(o.lint);
        }
        for (ev, list) in o.hooks {
            self.hooks.entry(ev).or_default().extend(list);
        }
        self.env.extend(o.env);
        if raw.contains_key("learning") {
            if o.learning.enabled.is_some() {
                self.learning.enabled = o.learning.enabled;
            }
            if o.learning.lessons.is_some() {
                self.learning.lessons = o.learning.lessons;
            }
        }
        if raw.contains_key("resources") {
            self.resources.merge(o.resources);
        }
        if let Some(toml::Value::Table(g)) = raw.get("guardrails") {
            self.guardrails.merge(o.guardrails, g);
        }
        if raw.contains_key("lsp") {
            self.lsp.merge(o.lsp);
        }
        if raw.contains_key("background") {
            self.background.merge(o.background);
        }
        if raw.contains_key("computer") {
            self.computer.merge(o.computer);
        }
        if raw.contains_key("remote") {
            self.remote.extend(o.remote);
        }
        if raw.contains_key("notify") {
            self.notify.merge(o.notify);
        }
        if raw.contains_key("goal") {
            self.goal.merge(o.goal);
        }
        if raw.contains_key("team") {
            self.team.merge(o.team);
        }
        if raw.contains_key("spend") {
            self.spend.merge(o.spend);
        }
        if raw.contains_key("optimize") {
            self.optimize.merge(o.optimize);
        }
        if raw.contains_key("voice") {
            self.voice.merge(o.voice);
        }
        if let Some(toml::Value::Table(p)) = raw.get("permissions") {
            if p.contains_key("fast_path") {
                self.permissions.fast_path = o.permissions.fast_path;
            }
            if p.contains_key("auto_deny_after_secs") {
                self.permissions.auto_deny_after_secs = o.permissions.auto_deny_after_secs;
            }
        }
        if let Some(toml::Value::Table(sc)) = raw.get("secrets") {
            if sc.contains_key("redact") {
                self.secrets.redact = o.secrets.redact;
            }
            if sc.contains_key("scrub_env") {
                self.secrets.scrub_env = o.secrets.scrub_env;
            }
            if sc.contains_key("protect_files") {
                self.secrets.protect_files = o.secrets.protect_files;
            }
            self.secrets.pass_env.extend(o.secrets.pass_env);
        }
    }

    pub fn base_url(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(|| self.provider.default_base_url().into())
    }

    /// The `stream_idle_timeout` limit: the default when unset, none when 0.
    pub fn stream_idle(&self) -> Option<std::time::Duration> {
        match self.stream_idle_timeout {
            None => Some(crate::llm::STREAM_IDLE_DEFAULT),
            Some(s) if s.is_finite() && s > 0.0 => {
                Some(std::time::Duration::from_secs_f64(s.min(86_400.0)))
            }
            Some(_) => None,
        }
    }

    /// May `anthropic/<model>` / `openai/<model>` go to the hosted APIs? Not
    /// when the default server is a custom OpenAI-compatible one: LM Studio and
    /// vLLM name local models like `openai/gpt-oss-20b`, and those must stay local.
    pub fn allows_builtin_hosted(&self) -> bool {
        self.base_url.is_none() && self.provider != ProviderKind::Openai
    }

    pub fn num_ctx(&self) -> u32 {
        self.num_ctx.unwrap_or(32768)
    }
}

/// One model spec, a comma-separated list, or an array of them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ModelList {
    One(String),
    Many(Vec<String>),
}

impl ModelList {
    /// The specs in order, without blanks or duplicates, at most three (as Claude Code).
    pub fn specs(&self) -> Vec<String> {
        let all: Vec<&str> = match self {
            ModelList::One(s) => s.split(',').collect(),
            ModelList::Many(v) => v.iter().flat_map(|s| s.split(',')).collect(),
        };
        let mut out: Vec<String> = Vec::new();
        for s in all.into_iter().map(str::trim).filter(|s| !s.is_empty()) {
            if !out.iter().any(|o| o == s) {
                out.push(s.to_string());
            }
        }
        out.truncate(3);
        out
    }
}

pub const EXAMPLE_CONFIG: &str = r#"# chanto configuration (~/.chanto/config.toml or ./.chanto/config.toml)

provider = "ollama"             # "ollama" (native API), "openai" (any OpenAI-compatible server),
                                # "anthropic" or "openai-responses" (hosted)
# base_url = "http://localhost:11434"
# model = "qwen3-coder:30b"     # default: first model the server reports
num_ctx = 32768                 # context window requested from the server
think = true                    # enable reasoning for models that support it
# effort = "high"               # reasoning effort: low | medium | high | xhigh | max
show_thinking = true
# tui = "inline"                # draw inline: output goes to the terminal's scrollback (default: fullscreen)
mode = "default"                # default | acceptEdits | plan | auto | bypassPermissions
# input_border = "block"        # TUI frames: block (default; can't gap) | line (rounded) | solid
# theme = "auto"                # auto | dark | light | dracula | nord | solarized-dark | gruvbox | colorblind | ~/.chanto/themes/<name>.toml
# screen_reader = false         # screen-reader mode: line-based output, no animation, a bell when a prompt waits (also --ax-screen-reader)
# animations = true             # TUI animation (spinner, live counters); false skips it (also --no-animations); unset follows macOS Reduce motion
# notifications = "auto"        # when a long turn ends or a prompt waits: auto | osc9 | osc777 | system | bell | off
# terminal_status = "auto"      # working/waiting/done on the tab (OSC 7501): auto (terminals known to support it) | on | off
# keep_awake = "working"        # stop idle sleep while chanto works: working | never | always (a closed lid still sleeps)
# notify_sound = "off"          # sound when a long turn ends unwatched: off | default | a system sound (Glass, Ping…) | a file path
# collapse_pastes = true        # big pastes show as [Pasted text #n +N lines] (ctrl+o expands); false pastes them as text
# copy_on_select = true         # a mouse drag copies its selection when released; false keeps it highlighted for ctrl+shift+c
# away_summary = true           # one-line session recap when you return after 3+ minutes away (TUI)
# shell_snapshot = true         # Bash gets your shell's PATH, exported variables, aliases and functions (from your rc files)
# bash_diff = true              # show files a Bash command changed as a diff (ctrl+o expands)
# idle_compaction = false       # compact in the background after idle_compact_minutes (54) idle at idle_compact_percent (0.5) of the window (TUI)
# classifier_model = "qwen3:8b" # faster model for auto-mode safety checks (7B or larger: smaller ones miss dangerous actions)
# oracle_model = "anthropic/claude-opus-5-5" # the `oracle` subagent (second opinion on plans/diffs); default: model
# resume_after_limit = true     # usage limit hit: wait for the reset, then continue the turn (the TUI asks when unset)
# account = "work"              # saved login to use (`chanto login <provider> --as work`)
# fallback_model = ["ollama/qwen3:8b"] # tried in order when the model is overloaded/unavailable (that turn only)
# tool_search = "auto"          # defer MCP tool schemas behind ToolSearch: auto | auto:N (% of context) | true | false
# stream_idle_timeout = 120     # seconds a started model response may go silent before it fails and is retried (0: no limit)
# mcp_tool_timeout = 600        # seconds an MCP tool call may take (a server's tool_timeout_sec wins; else MCP_TOOL_TIMEOUT in ms)
compact_threshold = 0.85

# Other agents' files read in place (all on by default; only your own config can change these).
# [compat]
# claude = true                 # Claude Code settings, CLAUDE.md, skills, commands, agents
# codex = true                  # Codex config, AGENTS.override.md, skills, prompts
# gemini = true                 # GEMINI.md
# cursor = true                 # .cursor/rules/*.mdc, .cursorrules
# copilot = true                # .github/copilot-instructions.md, .github/instructions/*.instructions.md
# aider = true                  # files .aider.conf.yml lists under `read:`
# opencode = true               # opencode.json `instructions`, .opencode/agent/*.md

[permissions]
# Examples; uncomment to use.
# allow = ["Bash(git status:*)", "Bash(cargo check:*)"]
# deny  = ["Bash(rm -rf /:*)"]
# auto_mode_rules = ["Never push to the main branch"]

[web]
# search_backend = "auto"       # auto | duckduckgo | brave | tavily | searxng
# search_min_gap_ms = 1500      # pacing for key-less searches; 0 disables
# brave_api_key = "..."
# searxng_url = "http://localhost:8888"

[browser]
# chrome_path = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
# port = 9222
# headless = false
# Badge on the browser's dock/taskbar icon while chanto drives it ("AI" by
# default; "" turns it off).
# badge = "AI"

# Error reports: when something breaks (crash, failed model request, auto-mode
# classifier or sandbox not starting), send a short redacted report. Off unless
# you say yes when asked; CHANTO_NO_ERROR_REPORTS=1 forces it off.
# [feedback]
# share_errors = false

# Computer use: the Computer tool sees the screen and drives mouse and keyboard
# (macOS; Linux X11 with xdotool + ImageMagick). Off by default; `--computer`
# turns it on for one session. Each action asks first unless in auto/bypass mode.
# [computer]
# enabled = false
# background = false              # also offer it to background runs
# max_width = 1280                # screenshot size the model sees
# max_height = 800

# Extra providers, used as "<name>/<model>" in model, subagent_model, classifier_model or /model
# [providers.lmstudio]
# kind = "openai"
# base_url = "http://localhost:1234/v1"
# [providers.llamacpp]
# kind = "openai"
# base_url = "http://localhost:8080/v1"
# Hosted APIs. With ANTHROPIC_API_KEY / OPENAI_API_KEY set, "anthropic/<model>" and
# "openai/<model>" also work without a section here.
# [providers.claude]
# kind = "anthropic"            # Anthropic Messages API (prompt caching, thinking)
# api_key = "..."               # default: $ANTHROPIC_API_KEY
# [providers.gpt]
# kind = "openai-responses"     # OpenAI Responses API (encrypted reasoning)
# api_key = "..."               # default: $OPENAI_API_KEY
# Coding plans ("zai/<model>", "kimi-coding/kimi-for-coding",
# "minimax-coding/MiniMax-M3.1-Flash-Preview") need only `chanto login <plan>`.
# With a mainland-China account, point a section at the China endpoint:
# [providers.kimi-cn]
# kind = "openai"
# base_url = "https://api.kimi.com/coding/v1"   # MiniMax: https://api.minimax.cn/v1
# api_key = "..."               # the plan's key, not a pay-as-you-go one
# num_ctx = 1048576
# Cloud platforms ("bedrock/<model-id>", "vertex/<model>", "azure/<deployment>",
# "foundry/<model>") use the cloud's own credentials; these sections are optional.
# [providers.bedrock]
# region = "us-east-1"          # default: $AWS_REGION, then the profile's region
# profile = "dev"               # default: $AWS_PROFILE, then "default"
# [providers.vertex]
# project = "my-project"        # default: $GOOGLE_CLOUD_PROJECT, then gcloud config
# region = "global"             # default: $CLOUD_ML_REGION, then "global"
# [providers.azure]
# resource = "my-resource"      # https://my-resource.openai.azure.com; default: $AZURE_OPENAI_ENDPOINT
# [providers.foundry]
# resource = "my-resource"      # https://my-resource.services.ai.azure.com

[memory]
auto = true
scope = "project"               # "user" for preferences shared across projects

[sandbox]
enabled = false                 # macOS: run Bash under Seatbelt (writes limited to project + temp/caches)
auto_allow = false              # auto mode: sandboxed commands skip the classifier
# allow_write = ["~/.local/share/myapp"]
# network = "allow"             # allow | deny | allowlist (deny_network is a legacy alias)
# allowed_domains = ["github.com", "*.npmjs.org", "pypi.org"]
# denied_domains = []

# Background runs (`chanto run --bg`, `&task` in the TUI): each in its own git worktree.
# [background]
# mode = "acceptEdits"            # permission mode for runs
# allow_sandboxed_commands = true # acceptEdits: commands that stay in the sandbox run without asking
# verify = true                   # verified mode (`--no-verify` per run)
# max_cost_usd = 2.0              # per-run budget
# max_minutes = 60                # wall-clock limit (time waiting for input doesn't count)
# input_timeout_minutes = 1440    # unanswered permission prompts are denied after this
# max_parallel = 2                # runs at once; the rest wait as queued
# isolate = "none"                # "docker" or "podman": shell commands run in a per-run container
# image = "rust:1-bookworm"       # container image (default: picked by project type)
# network = "none"                # container network: none | allowlist ([sandbox] allowed_domains) | full
# cpus = 2                        # container CPU limit
# memory = "4g"                   # container memory limit

# Goals (`/goal <objective>`, `chanto goal`): limits per run (from the start or a resume).
# [goal]
# max_iterations = 20             # work turns before stopping
# max_stale_iterations = 3        # stop after this many turns without a new passing criterion
# max_cost_usd = 5.0              # default: no goal budget (the session budget still applies)
# max_minutes = 120

# Teams (`/team <task>`, `chanto team`): sub-tasks run as background runs.
# [team]
# agents = 3                      # sub-task runs at once (`--agents N`)
# max_cost_usd = 6.0              # total budget, split evenly across sub-tasks

# Spend guard: estimates before fan-out (several subagents, /team, /loop),
# quiet notices when spend runs hot, and budgets. Unknown prices show tokens
# and plan-window % instead of dollars.
# [spend]
# confirm_over_usd = 2.0          # ask before fan-out estimated above this
# confirm_over_plan_percent = 10  # plans with usage windows: % of the window
# confirm_over_tokens = 5000000   # price and plan share both unknown
# non_interactive = "warn"        # -p and background runs over the threshold: warn | stop
# calls_per_agent = 15            # default: learned from your past subagents
# cheap_model = "anthropic/claude-haiku-5-5"  # offered by "use a cheaper model"
# limit_agents = 2                # offered by "limit parallelism"
# notices = true                  # one-line notices while spend runs hot
# burn_usd_per_min = 0.5
# burn_plan_percent_per_hour = 25
# parallel_agents = 4
# repeated_reads = 3              # one large file read this many times
# resent_context_percent = 50     # context resent on every call above this % of the window
# session_usd = 5.0               # budgets: notes at 50% and 80%, stop and ask at 100%
# day_usd = 20.0                  # (max_cost_usd / max_cost_usd_per_day win when set)
# month_usd = 200.0

# Experimental optimizations, each off by default while they're measured
# (CHANTO_OPTIMIZE=concise,quiet,provenance,spec,defer,token or all turns them on for a run,
# `noprefix` and `noadaptive` turn the default ones off; `spec`, `defer` and `token` are selected by name while they are being measured).
# [optimize]
# concise_prose = true            # brief prose replies (never code, tool args or reasoning)
# quiet = true                    # no narration between tool calls, one short summary (also `--quiet`)
# provenance = true               # flag unsupported numbers and identifiers in final answers
# spec_check = true               # treat a written spec as the contract and test against it
# adaptive_effort = false         # same effort on every call: better caching, fewer hard tasks solved
# stable_prefix = false           # keep project context in the system message (on by default)
# defer_tools = true              # rarely used built-in tools load through ToolSearch
# token_classifier = true         # auto mode: one-token ALLOW, full verdict only on BLOCK

# Run servers added with `chanto remote add <name> <url> --token <t>`:
# [remote.myserver]
# git_remote = "origin"           # push the branch there instead of uploading a bundle

# Notifications for background runs (done, failed, needs input). Bodies carry
# only the run id, its state and a link: never the task, code or output.
# [notify]
# ntfy = "https://ntfy.sh/my-private-topic"
# webhook = "https://example.com/chanto-hook"   # JSON POST
# desktop = true                  # default: on for the local daemon
# link = "http://my-mac.tailnet.ts.net:4141"     # base URL for links to the web UI

# Voice input (ctrl+space or `/voice` in the TUI, the mic button in the web UI).
# Local whisper.cpp by default; audio goes to a hosted endpoint only if you set one.
# [voice]
# backend = "auto"                # auto (local, else hosted if configured) | local | hosted | off
# whisper_model = "~/.chanto/voice/ggml-base.en.bin"
# provider = "groq"               # hosted: openai, groq, or a [providers.<name>] entry
# model = "whisper-large-v3-turbo"
# language = "en"
# silence_secs = 2.0              # stop after this much silence following speech (0 = key only)

# [mcp_servers.filesystem]
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

# Language servers (used when installed): rust-analyzer, typescript-language-server,
# pyright/basedpyright/pylsp, gopls, clangd. Errors are reported to the model after edits.
# [lsp]
# enabled = true
# diagnostics_wait_ms = 2500     # default 2500 (8000 for rust-analyzer)
# [lsp.servers.zls]
# command = "zls"
# extensions = ["zig"]

# Verified mode (`--verify`): the done-gate runs the project's checks before "done".
# [guardrails]
# enabled = true
# checks = ["cargo test"]         # default: detected
# oracle = true                   # first, the oracle subagent reviews the diff (once per turn)

# Format and lint each file an edit changes. The model sees what the formatter
# changed and the linter's findings. `{file}` is the file's path (else appended).
# [format]
# auto = true                     # use the project's own: rustfmt, prettier, ruff/black, gofmt, clang-format
# timeout_secs = 15
# [format.commands]
# "*.rs" = "rustfmt --edition 2021"
# "*.{ts,tsx}" = "npx prettier --write {file}"
# [lint]
# auto = true                     # ruff (when configured), eslint (when in package.json)
# timeout_secs = 60
# [lint.commands]
# py = "ruff check --quiet"

# [[hooks.PostToolUse]]
# matcher = "Edit|Write"
# command = "cargo fmt"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_does_not_cascade_from_broad_roots() {
        let home = home_dir_real().unwrap_or_else(|| PathBuf::from("/home/u"));
        let project = home.join("code/app");
        // A trusted project covers itself and its subtree.
        assert!(trust_covers(&project, &project));
        assert!(trust_covers(&project, &project.join("src/lib.rs")));
        // Trusting the home directory (or an ancestor, or /) does not.
        assert!(!trust_covers(&home, &project));
        assert!(!trust_covers(Path::new("/"), &project));
        if let Some(parent) = home.parent() {
            assert!(!trust_covers(parent, &project));
        }
        // But the broad path itself is still trusted when it is the cwd.
        assert!(trust_covers(&home, &home));
    }

    #[test]
    fn home_dir_follows_xdg_unless_legacy_exists() {
        let h = std::env::temp_dir().join(format!("chanto-xdg-{}", uuid::Uuid::new_v4().simple()));
        let xdg = h.join("xdg");
        assert_eq!(resolve_home(Some(&h), None), h.join(".chanto"));
        assert_eq!(resolve_home(Some(&h), Some(&xdg)), xdg.join("chanto"));
        // Relative XDG paths are ignored, per the spec.
        assert_eq!(
            resolve_home(Some(&h), Some(Path::new("rel"))),
            h.join(".chanto")
        );
        // An existing ~/.chanto keeps working.
        std::fs::create_dir_all(h.join(".chanto")).unwrap();
        assert_eq!(resolve_home(Some(&h), Some(&xdg)), h.join(".chanto"));
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn updates_merge_preserves_opt_out_in_unrelated_layers() {
        let mut cfg = super::Config::default();
        let raw: toml::Table = toml::from_str("[updates]\ncheck = false").unwrap();
        let layer = toml::from_str("[updates]\ncheck = false").unwrap();
        cfg.merge(layer, &raw);
        cfg.merge(super::Config::default(), &toml::Table::new());
        assert!(!cfg.updates.check);
    }

    #[test]
    fn copy_on_select_accepts_both_spellings() {
        let camel: Config = toml::from_str("copyOnSelect = false").unwrap();
        assert_eq!(camel.copy_on_select, Some(false));
        let snake: Config = toml::from_str("copy_on_select = true").unwrap();
        assert_eq!(snake.copy_on_select, Some(true));
    }

    #[test]
    fn optimizations_are_off_by_default_and_layer() {
        let mut cfg = Config::default();
        assert!(cfg.optimize.enabled().is_empty());
        for text in [
            "[optimize]\nconcise_prose = true\n",
            "[optimize]\nquiet = true\n",
            "[optimize]\nprovenance = true\n",
            "model = 'x'\n",
        ] {
            let raw: toml::Table = toml::from_str(text).unwrap();
            cfg.merge(toml::Value::Table(raw.clone()).try_into().unwrap(), &raw);
        }
        assert_eq!(
            cfg.optimize.enabled(),
            ["concise_prose", "quiet", "provenance"]
        );
        let mut o = OptimizeConfig::default();
        assert_eq!(o.apply_env("concise, provenance,bogus"), ["bogus"]);
        assert_eq!(o.enabled(), ["concise_prose", "provenance"]);
        o.apply_env("none");
        assert!(o.enabled().is_empty());
        o.apply_env("all");
        assert_eq!(o.enabled().len(), 3);
        o.apply_env("none");
        assert_eq!(o.apply_env("quiet"), Vec::<String>::new());
        assert_eq!(o.enabled(), ["quiet"]);
        assert_eq!(o.apply_env("provenance"), Vec::<String>::new());
        assert_eq!(o.enabled(), ["quiet", "provenance"]);
    }

    #[test]
    fn memory_and_cost_settings_survive_layering() {
        let mut cfg = Config::default();
        for text in [
            "max_cost_usd = 2.0\n[memory]\nauto = false\nscope = 'user'\n[pricing.custom]\ninput = 1.0\noutput = 2.0\n",
            "max_cost_usd_per_day = 5.0\n[memory]\nscope = 'project'\n",
        ] {
            let raw: toml::Table = toml::from_str(text).unwrap();
            cfg.merge(toml::Value::Table(raw.clone()).try_into().unwrap(), &raw);
        }
        assert!(!cfg.memory.auto);
        assert_eq!(cfg.memory.scope, "project");
        assert_eq!(cfg.max_cost_usd, Some(2.0));
        assert_eq!(cfg.max_cost_usd_per_day, Some(5.0));
        assert!(cfg.pricing.contains_key("custom"));
    }

    #[test]
    fn newer_settings_merge_from_config_files() {
        let d =
            std::env::temp_dir().join(format!("chanto-merge-{}", uuid::Uuid::new_v4().simple()));
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
        std::fs::create_dir_all(d.join(".chanto")).unwrap();
        std::fs::write(
            d.join(".chanto/config.toml"),
            "output_style = \"Learning\"\napi_key_helper = \"echo k\"\nprimer = false\n[permissions]\nfast_path = true\n[secrets]\nredact = false\nscrub_env = false\nprotect_files = false\npass_env = [\"GH_TOKEN\"]\n",
        )
        .unwrap();
        let (cfg, _) = Config::load(&d, true).unwrap();
        assert_eq!(cfg.output_style.as_deref(), Some("Learning"));
        assert_eq!(cfg.api_key_helper.as_deref(), Some("echo k"));
        assert_eq!(cfg.primer, Some(false));
        assert!(cfg.permissions.fast_path);
        assert!(!cfg.secrets.redact && !cfg.secrets.scrub_env && !cfg.secrets.protect_files);
        assert_eq!(cfg.secrets.pass_env, vec!["GH_TOKEN".to_string()]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn misspelt_keys_are_reported() {
        let d = std::env::temp_dir().join(format!("chanto-keys-{}", uuid::Uuid::new_v4().simple()));
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
        std::fs::create_dir_all(d.join(".chanto")).unwrap();
        std::fs::write(
            d.join(".chanto/config.toml"),
            "modle = \"x\"\nmodel = \"y\"\n[guardrails]\nenabled = true\n",
        )
        .unwrap();
        let (cfg, _) = Config::load(&d, true).unwrap();
        let notes = cfg.diagnostics.join("\n");
        assert!(
            notes.contains("Unknown setting `modle`") && notes.contains("did you mean `model`"),
            "{notes}"
        );
        assert!(
            !notes.contains("`guardrails`") && !notes.contains("Unknown setting `model`"),
            "{notes}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn provider_without_kind_and_a_v1_url_is_openai() {
        let d = std::env::temp_dir().join(format!("chanto-kind-{}", uuid::Uuid::new_v4().simple()));
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
        std::fs::create_dir_all(d.join(".chanto")).unwrap();
        std::fs::write(
            d.join(".chanto/config.toml"),
            "[providers.proxy]\nbase_url = \"http://localhost:4000/v1/\"\n[providers.box]\nbase_url = \"http://gpu-box:11434\"\n[providers.vllm]\nkind = \"anthropic\"\nbase_url = \"http://h/v1\"\n",
        )
        .unwrap();
        let (cfg, _) = Config::load(&d, true).unwrap();
        assert_eq!(cfg.providers["proxy"].kind, ProviderKind::Openai);
        assert_eq!(cfg.providers["box"].kind, ProviderKind::Ollama);
        assert_eq!(cfg.providers["vllm"].kind, ProviderKind::Anthropic);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_top_level_v1_url_is_openai_unless_a_provider_is_named() {
        let d =
            std::env::temp_dir().join(format!("chanto-top-kind-{}", uuid::Uuid::new_v4().simple()));
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
        std::fs::create_dir_all(d.join(".chanto")).unwrap();
        std::fs::write(
            d.join(".chanto/config.toml"),
            "base_url = \"http://localhost:4000/v1/\"\nmodel = \"bench-model\"\n",
        )
        .unwrap();
        let (cfg, _) = Config::load(&d, true).unwrap();
        assert_eq!(cfg.provider, ProviderKind::Openai);
        // An explicitly named provider keeps its native API.
        std::fs::write(
            d.join(".chanto/config.toml"),
            "provider = \"ollama\"\nbase_url = \"http://localhost:4000/v1/\"\nmodel = \"bench-model\"\n",
        )
        .unwrap();
        let (cfg, _) = Config::load(&d, true).unwrap();
        assert_eq!(cfg.provider, ProviderKind::Ollama);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn profiles_apply_on_top_and_respect_trust() {
        let d = std::env::temp_dir().join(format!("chanto-prof-{}", uuid::Uuid::new_v4().simple()));
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
        std::fs::create_dir_all(d.join(".chanto")).unwrap();
        std::fs::write(
            d.join(".chanto/config.toml"),
            "model = \"base\"\nprofile = \"fast\"\n[profiles.fast]\nmodel = \"small\"\nnum_ctx = 4096\n[profiles.yolo]\nmode = \"bypassPermissions\"\n",
        )
        .unwrap();
        // The project selects "fast" (trusted project).
        let (cfg, _) = Config::load(&d, true).unwrap();
        assert_eq!(cfg.model.as_deref(), Some("small"));
        assert_eq!(cfg.num_ctx, Some(4096));
        // --profile overrides; an untrusted project's profile can't set risky keys.
        unsafe { std::env::set_var("CHANTO_PROFILE", "yolo") };
        let (cfg, skipped) = Config::load(&d, false).unwrap();
        assert!(cfg.mode.is_none(), "untrusted profile set mode");
        assert!(
            skipped
                .iter()
                .any(|s| s.starts_with("mode") && s.contains("profiles.yolo")),
            "{skipped:?}"
        );
        unsafe { std::env::set_var("CHANTO_PROFILE", "nope") };
        assert!(
            Config::load(&d, true)
                .unwrap_err()
                .to_string()
                .contains("unknown profile")
        );
        unsafe { std::env::remove_var("CHANTO_PROFILE") };
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_home_directory_is_not_its_own_untrusted_project() {
        // Started in ~, ~/.chanto/config.toml is the user's config. It was also
        // read as an untrusted project's, with a warning to `--trust` all of ~.
        let d =
            std::env::temp_dir().join(format!("chanto-cfg-home-{}", uuid::Uuid::new_v4().simple()));
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
        std::fs::create_dir_all(d.join(".chanto")).unwrap();
        std::fs::write(d.join(".chanto/config.toml"), "mode = \"acceptEdits\"\n").unwrap();
        let saved = std::env::var_os("CHANTO_HOME");
        unsafe { std::env::set_var("CHANTO_HOME", d.join(".chanto")) };
        let loaded = Config::load(&d, false);
        unsafe {
            match saved {
                Some(h) => std::env::set_var("CHANTO_HOME", h),
                None => std::env::remove_var("CHANTO_HOME"),
            }
        }
        let (cfg, skipped) = loaded.unwrap();
        assert_eq!(cfg.mode, Some(Mode::AcceptEdits));
        assert!(skipped.is_empty(), "{skipped:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn untrusted_project_settings_are_skipped() {
        let d = std::env::temp_dir().join(format!("chanto-cfg-{}", uuid::Uuid::new_v4().simple()));
        // Don't pick up the real ~/.claude or ~/.codex.
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
        std::fs::create_dir_all(d.join(".chanto")).unwrap();
        std::fs::write(
            d.join(".chanto/config.toml"),
            "mode = \"bypassPermissions\"\nnum_ctx = 1234\nclassifier_model = \"tiny:1b\"\n[optimize]\ntoken_classifier = true\nconcise_prose = true\n[[hooks.PreToolUse]]\ncommand = \"touch /tmp/x\"\n[permissions]\nallow = [\"Bash\"]\ndeny = [\"Bash(rm:*)\"]\nauto_deny_after_secs = 0\n[sandbox]\nenabled = false\n",
        )
        .unwrap();
        std::fs::write(
            d.join(".mcp.json"),
            r#"{"mcpServers":{"evil":{"command":"sh"}}}"#,
        )
        .unwrap();
        let (cfg, skipped) = Config::load(&d, false).unwrap();
        assert!(cfg.hooks.is_empty());
        assert!(cfg.mode.is_none());
        assert!(!cfg.mcp_servers.contains_key("evil"));
        assert!(!cfg.permissions.allow.iter().any(|r| r == "Bash"));
        assert!(cfg.permissions.deny.iter().any(|r| r == "Bash(rm:*)"));
        assert_eq!(cfg.num_ctx, Some(1234));
        // Nor pick the model that judges auto-mode actions, or let a
        // one-token answer settle them.
        assert_eq!(cfg.classifier_model, None);
        assert_eq!(cfg.optimize.token_classifier, None);
        assert_eq!(cfg.optimize.concise_prose, Some(true));
        // A repo must not be able to make an unanswered prompt wait forever.
        assert_eq!(cfg.permissions.auto_deny_after_secs, None);
        assert!(skipped.len() >= 5);
        let (cfg, skipped) = Config::load(&d, true).unwrap();
        assert_eq!(cfg.hooks["PreToolUse"].len(), 1);
        assert!(cfg.mcp_servers.contains_key("evil"));
        assert_eq!(cfg.permissions.auto_deny_after_secs, Some(0));
        assert_eq!(cfg.classifier_model.as_deref(), Some("tiny:1b"));
        assert_eq!(cfg.optimize.token_classifier, Some(true));
        assert!(skipped.is_empty());
    }

    #[test]
    fn launch_flags_add_settings_and_servers_and_pick_sources() {
        use crate::launch::{Launch, parse_mcp_config, parse_settings};
        let d =
            std::env::temp_dir().join(format!("chanto-launch-{}", uuid::Uuid::new_v4().simple()));
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
        std::fs::create_dir_all(d.join(".chanto")).unwrap();
        std::fs::write(d.join(".chanto/config.toml"), "num_ctx = 1000\nmodel = \"project\"\n[[hooks.PreToolUse]]\ncommand = \"true\"\n[mcp_servers.proj]\ncommand = \"p\"\n").unwrap();
        std::fs::write(d.join(".chanto/config.local.toml"), "max_turns = 7\n").unwrap();
        std::fs::write(
            d.join(".mcp.json"),
            r#"{"mcpServers":{"fromfile":{"command":"sh"}}}"#,
        )
        .unwrap();
        let settings =
            parse_settings(r#"{"model":"cli","permissions":{"deny":["Bash(rm:*)"]}}"#).unwrap();
        let mcp = parse_mcp_config(r#"{"mcpServers":{"cli":{"command":"c"}}}"#).unwrap();

        // --settings wins over project files; --mcp-config adds to the discovered servers.
        let l = Launch {
            settings: vec![settings.clone()],
            mcp: vec![mcp.clone()],
            ..Default::default()
        };
        let (cfg, _) = Config::load_with(&d, true, &l).unwrap();
        assert_eq!(cfg.model.as_deref(), Some("cli"));
        assert_eq!(cfg.num_ctx, Some(1000));
        assert!(cfg.permissions.deny.iter().any(|r| r == "Bash(rm:*)"));
        assert!(
            ["proj", "fromfile", "cli"]
                .iter()
                .all(|s| cfg.mcp_servers.contains_key(*s)),
            "{:?}",
            cfg.mcp_servers.keys()
        );

        // --strict-mcp-config: only the command line's servers.
        let l = Launch {
            mcp: vec![mcp.clone()],
            strict_mcp: true,
            ..Default::default()
        };
        let (cfg, _) = Config::load_with(&d, true, &l).unwrap();
        assert_eq!(cfg.mcp_servers.keys().collect::<Vec<_>>(), vec!["cli"]);
        assert_eq!(cfg.hooks["PreToolUse"].len(), 1);

        // --bare: no hooks or discovered servers, but other settings still load.
        let l = Launch {
            bare: true,
            ..Default::default()
        };
        let (cfg, _) = Config::load_with(&d, true, &l).unwrap();
        assert!(cfg.hooks.is_empty() && cfg.mcp_servers.is_empty());
        assert_eq!(cfg.num_ctx, Some(1000));

        // --setting-sources local: no project files.
        let l = Launch {
            sources: Some(vec!["local".into()]),
            ..Default::default()
        };
        let (cfg, _) = Config::load_with(&d, true, &l).unwrap();
        assert_eq!(cfg.num_ctx, None);
        assert_eq!(cfg.max_turns, Some(7));
        assert!(!cfg.mcp_servers.contains_key("fromfile"));
        let l = Launch {
            sources: Some(vec!["project".into()]),
            ..Default::default()
        };
        let (cfg, _) = Config::load_with(&d, true, &l).unwrap();
        assert_eq!((cfg.num_ctx, cfg.max_turns), (Some(1000), None));
        unsafe { std::env::remove_var("CHANTO_COMPAT_HOME") };
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn hook_types_and_conditions_flatten() {
        let mut cfg: Config = toml::from_str(
            r#"
[[hooks.PreToolUse]]
matcher = "Bash"
[[hooks.PreToolUse.hooks]]
type = "command"
command = "check"
if = "Bash(git *)"
[[hooks.PreToolUse.hooks]]
type = "http"
url = "http://localhost:9/hook"
allowedEnvVars = ["TOKEN"]
headers = { Authorization = "Bearer $TOKEN" }
timeout = 5
onFailure = "block"
[[hooks.PreToolUse.hooks]]
type = "prompt"
prompt = "is this ok?"
model = "small"
continueOnBlock = true
[[hooks.PreToolUse.hooks]]
type = "mcp_tool"
server = "sec"
tool = "scan"
input = { path = "${tool_input.file_path}" }
[[hooks.PreToolUse.hooks]]
type = "mcp_tool"
server = "no-tool"
[[hooks.PostToolUse]]
[[hooks.PostToolUse.hooks]]
command = "node"
args = ["x.js", "--fix"]
async = true
asyncRewake = true
shell = "powershell"
[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "agent"
prompt = "verify"
[[hooks.Stop.hooks]]
type = "wasm"
"#,
        )
        .unwrap();
        let dropped = flatten_hooks(&mut cfg.hooks);
        assert_eq!(
            dropped,
            vec![
                "PreToolUse hook of type `mcp_tool`".to_string(),
                "Stop hook of type `wasm`".to_string()
            ]
        );
        let pre = &cfg.hooks["PreToolUse"];
        assert_eq!(pre.len(), 4);
        assert_eq!(
            (pre[0].kind.as_str(), pre[0].command.as_str()),
            ("command", "check")
        );
        assert_eq!(pre[0].condition.as_deref(), Some("Bash(git *)"));
        assert_eq!(pre[1].kind, "http");
        assert_eq!(pre[1].matcher, "Bash");
        assert_eq!(pre[1].url, "http://localhost:9/hook");
        assert_eq!(pre[1].allowed_env_vars, vec!["TOKEN".to_string()]);
        assert_eq!(pre[1].headers["Authorization"], "Bearer $TOKEN");
        assert_eq!(pre[1].timeout_secs, Some(5));
        assert_eq!(pre[1].on_failure.as_deref(), Some("block"));
        assert_eq!(
            (
                pre[2].kind.as_str(),
                pre[2].prompt.as_str(),
                pre[2].model.as_deref(),
                pre[2].continue_on_block
            ),
            ("prompt", "is this ok?", Some("small"), true)
        );
        assert_eq!(
            (pre[3].server.as_str(), pre[3].tool.as_str()),
            ("sec", "scan")
        );
        assert_eq!(
            pre[3].input.as_ref().unwrap()["path"],
            "${tool_input.file_path}"
        );
        let post = &cfg.hooks["PostToolUse"][0];
        assert_eq!(
            (
                post.kind.as_str(),
                post.args.clone().unwrap(),
                post.run_async,
                post.async_rewake,
                post.shell.as_deref()
            ),
            (
                "command",
                vec!["x.js".to_string(), "--fix".to_string()],
                true,
                true,
                Some("powershell")
            )
        );
        // `/hooks` lists what each one runs: an http hook has no command.
        assert_eq!(pre[0].describe(), "check (if Bash(git *))");
        assert_eq!(pre[1].describe(), "POST http://localhost:9/hook");
        assert_eq!(pre[3].describe(), "mcp sec/scan");
        assert_eq!(post.describe(), "node x.js --fix");
        assert_eq!(cfg.hooks["Stop"].len(), 1);
        assert_eq!(cfg.hooks["Stop"][0].kind, "agent");
        // Native flat entries still work, including http ones.
        let mut native: Config = toml::from_str("[[hooks.PostToolUse]]\ncommand = \"fmt\"\n[[hooks.PostToolUse]]\ntype = \"http\"\nurl = \"http://x/h\"\n").unwrap();
        assert!(flatten_hooks(&mut native.hooks).is_empty());
        assert_eq!(native.hooks["PostToolUse"].len(), 2);
        // Claude's JSON layout, as settings.json writes it.
        let json: HooksConfig = serde_json::from_str(r#"{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"x","onFailure":"block","statusMessage":"checking"}]}]}"#).unwrap();
        assert_eq!(
            json["PreToolUse"][0].hooks[0].on_failure.as_deref(),
            Some("block")
        );
    }

    #[test]
    fn disable_all_hooks_spares_managed_hooks() {
        let hook = |c: &str| HookConfig {
            kind: "command".into(),
            command: c.into(),
            ..Default::default()
        };
        let mut cfg = Config {
            disable_all_hooks: Some(true),
            ..Default::default()
        };
        cfg.hooks
            .insert("Stop".into(), vec![hook("user"), hook("managed")]);
        let mut managed = HooksConfig::new();
        managed.insert("Stop".into(), vec![hook("managed")]);
        cfg.apply_disable_all_hooks(&managed, false);
        assert_eq!(
            cfg.hooks["Stop"],
            vec![hook("managed")],
            "only managed settings can turn managed hooks off"
        );
        cfg.apply_disable_all_hooks(&managed, true);
        assert!(cfg.hooks.is_empty());
        // Claude Code settings carry it as `disableAllHooks`.
        let t = toml::Table::try_from(serde_json::json!({"disableAllHooks": true})).unwrap();
        let c: Config = toml::Value::Table(t).try_into().unwrap();
        assert_eq!(c.disable_all_hooks, Some(true));
    }

    #[test]
    fn model_sizes_come_from_the_name() {
        assert_eq!(model_billions("qwen3:4b"), Some(4.0));
        assert_eq!(model_billions("google/gemma-3-4b-it"), Some(4.0));
        assert_eq!(
            model_billions("meta-llama/llama-3.1-8b-instruct"),
            Some(8.0)
        );
        assert_eq!(model_billions("openai/gpt-oss-20b"), Some(20.0));
        assert_eq!(model_billions("qwen3.5-0.8b"), Some(0.8));
        assert_eq!(model_billions("deepseek-flash"), None);
        assert_eq!(model_billions("gemma-3n-e2b"), None);
        assert_eq!(model_billions("qwen3-30b-a3b"), Some(30.0));
    }
}
