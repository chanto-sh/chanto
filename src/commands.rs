//! Slash commands shared by the TUI and the plain REPL.

use crate::agent::{Agent, Env, Event};
use crate::permissions::{Mode, Rule};
use crate::session::Session;
use tokio_util::sync::CancellationToken;

pub enum Outcome {
    /// Show this text (markdown) to the user.
    Show(String),
    /// Send this prompt to the model.
    Prompt(String),
    /// Run a long-running mode (outcome, goal, team) that takes over the agent.
    Run(LongRun),
    Exit,
    /// Conversation was replaced (clear/resume): frontends should reset.
    Reset(Option<String>),
    Nothing,
}

/// Modes that drive the agent through many turns until they finish.
pub enum LongRun {
    /// Outcome mode: criteria → approval → build → evidence report.
    Outcome(String),
    /// A persistent goal: criteria, then iterations until they all pass.
    Goal(String),
    /// Continue a goal (a unique id prefix, or the newest unfinished one).
    GoalResume(Option<String>),
    /// A team: plan, parallel sub-task runs, review, merge, checks.
    Team(String),
}

impl LongRun {
    pub fn label(&self) -> &'static str {
        match self {
            LongRun::Outcome(_) => "Outcome mode",
            LongRun::Goal(_) | LongRun::GoalResume(_) => "Goal",
            LongRun::Team(_) => "Team",
        }
    }

    /// Run it to the end; returns the report.
    pub async fn run(self, agent: &mut Agent, cancel: CancellationToken) -> anyhow::Result<String> {
        let limits = agent.env.config.goal.limits();
        match self {
            LongRun::Outcome(intent) => crate::outcome::run(agent, &intent, cancel).await,
            LongRun::Goal(objective) => crate::goal::start(
                agent,
                &objective,
                &crate::goal::goals_dir(),
                None,
                limits,
                cancel,
            )
            .await
            .map(|r| r.1),
            LongRun::GoalResume(id) => crate::goal::resume(
                agent,
                &crate::goal::goals_dir(),
                id.as_deref(),
                Some(limits),
                cancel,
            )
            .await
            .map(|r| r.1),
            LongRun::Team(task) => crate::team::run_here(agent, &task, None, cancel).await,
        }
    }
}

pub struct CommandInfo {
    pub name: &'static str,
    pub args: &'static str,
    pub help: &'static str,
}

pub const BUILTIN: &[CommandInfo] = &[
    CommandInfo {
        name: "help",
        args: "",
        help: "Show commands and shortcuts",
    },
    CommandInfo {
        name: "clear",
        args: "",
        help: "Start a new conversation",
    },
    CommandInfo {
        name: "compact",
        args: "[instructions]",
        help: "Summarize the conversation to free context",
    },
    CommandInfo {
        name: "autocompact",
        args: "[<tokens>|auto|off]",
        help: "Show or set when this session compacts on its own (e.g. 120k)",
    },
    CommandInfo {
        name: "plan",
        args: "[task]",
        help: "Enter plan mode, and start planning the task if given",
    },
    CommandInfo {
        name: "recap",
        args: "",
        help: "One-line summary of the session so far",
    },
    CommandInfo {
        name: "cd",
        args: "<path>",
        help: "Move the session to another working directory (loads its instructions and settings)",
    },
    CommandInfo {
        name: "background",
        args: "[prompt]",
        help: "Move this session to a background run (own worktree) and free the terminal; `chanto attach` follows it",
    },
    CommandInfo {
        name: "mode",
        args: "[default|acceptEdits|plan|auto|bypass]",
        help: "Show or set the permission mode",
    },
    CommandInfo {
        name: "model",
        args: "[--session] [name]",
        help: "Show or switch the model (saved as the default; --session for this session only)",
    },
    CommandInfo {
        name: "models",
        args: "",
        help: "List models available on the server",
    },
    CommandInfo {
        name: "think",
        args: "[on|off|low|medium|high|xhigh|max]",
        help: "Toggle model reasoning, or set its effort",
    },
    CommandInfo {
        name: "effort",
        args: "[low|medium|high|xhigh|max]",
        help: "Show or set reasoning effort",
    },
    CommandInfo {
        name: "context",
        args: "[skills]",
        help: "Show context window usage; `skills`: what each skill, subagent and MCP server costs",
    },
    CommandInfo {
        name: "skill-doctor",
        args: "",
        help: "What each skill, subagent and MCP server adds to the context (same as `/context skills`)",
    },
    CommandInfo {
        name: "status",
        args: "",
        help: "Version, model and login, mode, branch, context, MCP servers, session and sandbox at a glance",
    },
    CommandInfo {
        name: "cost",
        args: "[today | cheaper [model|off] | limit [N|off]]",
        help: "Where this session's spend went, by agent, model, tool and file, with suggestions; account limits (alias: /usage)",
    },
    CommandInfo {
        name: "insights",
        args: "",
        help: "Where this session's tokens and time went, and what to change",
    },
    CommandInfo {
        name: "stats",
        args: "",
        help: "Usage across sessions, here and overall: last 7 and 30 days, by day and by model",
    },
    CommandInfo {
        name: "resume",
        args: "[id | --all | --search <text>]",
        help: "List, search or resume a previous session (the picker deletes and archives too)",
    },
    CommandInfo {
        name: "sessions",
        args: "[--all | --search <text> | delete|archive|unarchive <id>]",
        help: "List, search, delete or archive saved sessions",
    },
    CommandInfo {
        name: "rename",
        args: "<title>",
        help: "Name this session, as /resume and `chanto sessions` list it (alias: /title)",
    },
    CommandInfo {
        name: "loop",
        args: "[interval] [prompt] | status | stop",
        help: "Re-run a prompt every interval (/loop 5m check CI), or when the model chooses (/loop check CI); /loop stop or esc ends it",
    },
    CommandInfo {
        name: "rewind",
        args: "[n|undo]",
        help: "Restore files and conversation to before an earlier prompt",
    },
    CommandInfo {
        name: "redo",
        args: "",
        help: "Put back what the last /undo or /rewind took back (until the next prompt)",
    },
    CommandInfo {
        name: "init",
        args: "",
        help: "Generate an AGENTS.md for this project",
    },
    CommandInfo {
        name: "review",
        args: "[low|medium|high|xhigh|max] [--fix] [--comment] [target]",
        help: "Review the current diff (or a PR, branch, range or path) for bugs; always the bundled /code-review",
    },
    CommandInfo {
        name: "memory",
        args: "[list|open|edit|delete|forget all|undo]",
        help: "Manage durable memories outside the repo",
    },
    CommandInfo {
        name: "permissions",
        args: "",
        help: "Show permission rules",
    },
    CommandInfo {
        name: "allow",
        args: "<rule>",
        help: "Add an allow rule for this session, e.g. Bash(npm test:*)",
    },
    CommandInfo {
        name: "deny",
        args: "<rule>",
        help: "Add a deny rule for this session",
    },
    CommandInfo {
        name: "hooks",
        args: "",
        help: "List configured hooks",
    },
    CommandInfo {
        name: "verify",
        args: "[full|related]",
        help: "Show or set what verified mode tests: related tests (default) or the full suite",
    },
    CommandInfo {
        name: "outcome",
        args: "<what you want>",
        help: "Outcome mode: approve acceptance criteria, then review evidence instead of code",
    },
    CommandInfo {
        name: "goal",
        args: "<objective> | status | pause | resume [id] | stop | list",
        help: "Keep working toward an objective until its acceptance criteria pass",
    },
    CommandInfo {
        name: "team",
        args: "<task> | status | stop",
        help: "Split a task into parallel sub-task runs, then review, merge and check them",
    },
    CommandInfo {
        name: "learned",
        args: "[review|accept|reject|disable|enable|add|undo|signals]",
        help: "Lessons learned from your corrections (nothing applies until you accept it)",
    },
    CommandInfo {
        name: "tasks",
        args: "[stop <id>]",
        help: "List background agents, or stop one",
    },
    CommandInfo {
        name: "plugin",
        args: "[list|install|uninstall|enable|disable|marketplace]",
        help: "Manage plugins and marketplaces",
    },
    CommandInfo {
        name: "plugins",
        args: "",
        help: "List plugins in use",
    },
    CommandInfo {
        name: "ide",
        args: "[connect [port]|disconnect]",
        help: "Show or manage the IDE connection (VS Code or JetBrains)",
    },
    CommandInfo {
        name: "remote",
        args: "[off|status|local]",
        help: "Continue this session from your phone over Tailscale (alias: /remote-control)",
    },
    CommandInfo {
        name: "add-dir",
        args: "<path>",
        help: "Let the agent work in another directory this session",
    },
    CommandInfo {
        name: "statusline",
        args: "[description]",
        help: "Generate a status line script from a description (or your shell prompt) and set it in ~/.chanto/config.toml",
    },
    CommandInfo {
        name: "theme",
        args: "[name|auto]",
        help: "List color themes, or switch to one for this session",
    },
    CommandInfo {
        name: "login",
        args: "[provider]",
        help: "Sign in to a provider (chanto cloud, ChatGPT plan, OpenRouter) or list providers",
    },
    CommandInfo {
        name: "logout",
        args: "<provider>",
        help: "Forget a provider's saved login or key",
    },
    CommandInfo {
        name: "sandbox",
        args: "[on|off]",
        help: "Show or toggle the sandbox for shell commands",
    },
    CommandInfo {
        name: "output-style",
        args: "[name]",
        help: "List or switch output styles (Concise, Explanatory, Learning, Proactive, or your own)",
    },
    CommandInfo {
        name: "security-review",
        args: "[focus]",
        help: "Security review of the pending changes",
    },
    CommandInfo {
        name: "pr-comments",
        args: "[pr]",
        help: "Show review comments on the current pull request",
    },
    CommandInfo {
        name: "diff",
        args: "",
        help: "Show uncommitted git changes (including untracked files)",
    },
    CommandInfo {
        name: "tools",
        args: "",
        help: "List available tools",
    },
    CommandInfo {
        name: "mcp",
        args: "[reconnect|enable|disable <server>|all] [login|logout <server>]",
        help: "Show MCP servers; reconnect, enable or disable one (remembered per project); log in or out (OAuth)",
    },
    CommandInfo {
        name: "skills",
        args: "",
        help: "List skills",
    },
    CommandInfo {
        name: "customize",
        args: "<request>",
        help: "Customize chanto using its extension points",
    },
    CommandInfo {
        name: "reload",
        args: "",
        help: "Reload extensions and hooks; validate configuration",
    },
    CommandInfo {
        name: "agents",
        args: "",
        help: "List custom subagents",
    },
    CommandInfo {
        name: "peers",
        args: "",
        help: "Other chanto sessions working in this project, and their files",
    },
    CommandInfo {
        name: "todos",
        args: "",
        help: "Show the current todo list",
    },
    CommandInfo {
        name: "bashes",
        args: "",
        help: "List background shells",
    },
    CommandInfo {
        name: "fork",
        args: "[name] [--at n]",
        help: "Fork this session, optionally before a prompt",
    },
    CommandInfo {
        name: "edit-prompt",
        args: "<n> [both|conversation] [new prompt]",
        help: "Edit an earlier prompt: branch the session before it (the original stays) and send the new text",
    },
    CommandInfo {
        name: "share",
        args: "[--no-tool-output]",
        help: "Save a redacted self-contained HTML session",
    },
    CommandInfo {
        name: "export",
        args: "[file]",
        help: "Export the conversation as markdown",
    },
    CommandInfo {
        name: "copy",
        args: "",
        help: "Copy the last response to the clipboard",
    },
    CommandInfo {
        name: "btw",
        args: "<question> | fork [n]",
        help: "Ask a side question, even mid-turn, without adding it to the conversation; f in the answer forks it into a background agent",
    },
    CommandInfo {
        name: "bg",
        args: "<task>",
        help: "Run a task in the background in its own git worktree (or start a prompt with &)",
    },
    CommandInfo {
        name: "runs",
        args: "[id | attach <id> | detach | stop <id>]",
        help: "List background runs; follow one here and answer its prompts",
    },
    CommandInfo {
        name: "schedule",
        args: "[run <job>]",
        help: "Scheduled jobs (chanto runner): when they run next; start one now",
    },
    CommandInfo {
        name: "vision",
        args: "[on|off]",
        help: "Toggle image support for the model",
    },
    CommandInfo {
        name: "config",
        args: "[key value]",
        help: "Show configuration file locations, or set a session setting",
    },
    CommandInfo {
        name: "keybindings",
        args: "",
        help: "Create or show your keybindings file (Claude Code's format; edits apply at once)",
    },
    CommandInfo {
        name: "vim",
        args: "[on|off]",
        help: "Toggle vim keybindings in the prompt (TUI)",
    },
    CommandInfo {
        name: "quiet",
        args: "[on|off]",
        help: "Quiet mode (experimental): no narration between tool calls, one short summary; the TUI shows only the status line while working",
    },
    CommandInfo {
        name: "transcript",
        args: "",
        help: "Show the steps of the last turn (in quiet mode they're hidden; also ctrl+o in the TUI)",
    },
    CommandInfo {
        name: "voice",
        args: "",
        help: "Dictate a prompt: start recording, run again to transcribe (TUI; also ctrl+space)",
    },
    CommandInfo {
        name: "terminal-setup",
        args: "",
        help: "Set up shift+enter for multi-line prompts in your terminal",
    },
    CommandInfo {
        name: "feedback",
        args: "<message>",
        help: "Send feedback or a bug report to the chanto developers (shows exactly what is sent first)",
    },
    CommandInfo {
        name: "bug",
        args: "<message>",
        help: "Report a bug (same as /feedback)",
    },
    CommandInfo {
        name: "diagnostics",
        args: "[on [--hours N] [--name you]|off|status]",
        help: "Diagnostic mode: send errors and warnings to the chanto developers automatically for a while",
    },
    CommandInfo {
        name: "privacy-settings",
        args: "",
        help: "Show what data leaves this machine",
    },
    CommandInfo {
        name: "release-notes",
        args: "",
        help: "What's new in this version",
    },
    CommandInfo {
        name: "doctor",
        args: "",
        help: "Check the environment (server, tools, browser)",
    },
    CommandInfo {
        name: "upgrade",
        args: "",
        help: "Check for a newer release and how to install it",
    },
    CommandInfo {
        name: "exit",
        args: "",
        help: "Quit",
    },
];

/// Keyboard shortcuts, kept in one place so `/help` and the TUI's `?` overlay
/// can't drift apart.
pub const SHORTCUTS: &[(&str, &str)] = &[
    (
        "shift+tab",
        "cycle mode (default → accept edits → plan → auto)",
    ),
    (
        "esc",
        "interrupt · stop a /loop · esc esc to rewind to an earlier prompt",
    ),
    (
        "ctrl+c",
        "clear the input, stop the turn, or quit (press twice)",
    ),
    (
        "ctrl+o",
        "expand or collapse tool output and thinking (quiet mode: show the hidden steps) · click a block to expand it",
    ),
    ("ctrl+r", "search prompt history"),
    ("ctrl+f", "search the transcript"),
    ("ctrl+g", "edit the prompt in $EDITOR"),
    ("ctrl+t", "toggle the todo list"),
    ("drag", "select text to copy it"),
    (
        "cmd/ctrl+click",
        "open a link (needs a terminal with hyperlink support)",
    ),
    (
        "ctrl+s / F2",
        "let the terminal select text itself (or hold shift/option while dragging)",
    ),
    (
        "ctrl+v",
        "paste an image from the clipboard as an [Image #n] tag",
    ),
    (
        "ctrl+space",
        "dictate: record, then press again to transcribe into the prompt (or /voice)",
    ),
    (
        "@path / !cmd / #note",
        "attach a file · run a shell command · save a durable memory",
    ),
    (
        "enter (while working)",
        "the model gets the message at its next step",
    ),
    (
        "ctrl+x enter",
        "queue the message for after the current turn",
    ),
    (
        "ctrl+x ctrl+s / ctrl+enter",
        "send now, cutting the current turn short",
    ),
    (
        "↑ (empty prompt, while working)",
        "edit messages the model hasn't received yet",
    ),
    (
        "↓ (empty prompt)",
        "select a background agent or shell below the prompt: enter opens, x stops",
    ),
    (
        "shift+enter",
        "new line in the prompt (also alt+enter or ctrl+j)",
    ),
    (
        "↑ / ↓ · pgup / pgdn",
        "prompt history · scroll the transcript",
    ),
];

/// The `## Shortcuts` block for `/help`, built from [`SHORTCUTS`].
pub fn shortcuts_markdown() -> String {
    SHORTCUTS
        .iter()
        .map(|(k, d)| format!("- `{k}` — {d}\n"))
        .collect()
}

pub fn completions(agent: &Agent) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = BUILTIN
        .iter()
        .filter(|c| agent.acp.is_some() || !c.name.starts_with("agent-"))
        .map(|c| (format!("/{}", c.name), c.help.to_string()))
        .collect();
    for c in &*agent.env.commands.read().unwrap() {
        v.push((format!("/{}", c.name), with_hint(&c.meta, &c.description)));
    }
    // Skills run as `/name` too, replacing a command of the same name (built-ins stay).
    for s in agent
        .env
        .skills
        .read()
        .unwrap()
        .iter()
        .filter(|s| s.user_invocable && !BUILTIN.iter().any(|c| c.name == s.name))
    {
        let name = format!("/{}", s.name);
        v.retain(|(n, _)| *n != name);
        v.push((name, with_hint(&s.meta, &s.description)));
    }
    // Bundled skills, unless a skill or command of the same name replaces them.
    for b in crate::bundled::BUNDLED {
        let name = format!("/{}", b.name);
        if !v.iter().any(|(n, _)| *n == name) {
            v.push((name, b.help.to_string()));
        }
    }
    for (name, desc) in crate::mcp::prompt_commands(&agent.env.mcp) {
        v.push((format!("/{name}"), desc));
    }
    // The external agent's own commands; Chanto commands of the same name win.
    if let Some(acp) = &agent.acp {
        for (name, description) in acp.available_commands() {
            let name = format!("/{name}");
            if !v.iter().any(|(n, _)| *n == name) {
                v.push((name, description));
            }
        }
    }
    v
}

/// A completion's description led by the `argument-hint` (`[issue-number] Fix an issue`).
fn with_hint(meta: &crate::context::SkillMeta, description: &str) -> String {
    match meta.argument_hint.as_deref().filter(|h| !h.is_empty()) {
        Some(h) => format!("{h} {description}"),
        None => description.to_string(),
    }
}

/// `/name` plus its `argument-hint`, for `/help`.
fn usage(name: &str, meta: &crate::context::SkillMeta) -> String {
    match meta.argument_hint.as_deref().filter(|h| !h.is_empty()) {
        Some(h) => format!("/{name} {h}"),
        None => format!("/{name}"),
    }
}

/// Split `/cmd rest` into the command name and its argument.
fn parse_cmd(line: &str) -> (&str, &str) {
    match line[1..].split_once(char::is_whitespace) {
        Some((c, a)) => (c, a.trim()),
        None => (&line[1..], ""),
    }
}

/// Commands that need only the environment, so they take effect immediately even
/// while a turn is in flight instead of queuing behind the model (settings like
/// `/model` take effect at once). Commands that rewrite the conversation
/// genuinely need the agent, which the running turn holds, so those still queue.
pub fn is_immediate(line: &str) -> bool {
    matches!(
        parse_cmd(line).0,
        "model"
            | "models"
            | "think"
            | "effort"
            | "mode"
            | "theme"
            | "vision"
            | "help"
            | "?"
            | "exit"
            | "quit"
            | "q"
            // Read-only inspection: seeing the mode, tools, todos or running
            // shells mid-turn is exactly when you want it.
            | "permissions"
            | "diagnostics"
            | "tools"
            | "skills"
            | "agents"
            | "peers"
            | "todos"
            | "bashes"
            // Background runs live outside this conversation.
            | "bg"
            | "runs"
            | "schedule"
            | "upgrade"
            // Turning remote control on mid-turn is how you hand a running task to your phone.
            | "remote"
            | "remote-control"
            | "keybindings"
            | "stats"
            // Listing or reconnecting a server doesn't touch the conversation.
            | "mcp"
            // A side question answers from the conversation so far, mid-turn included.
            | "btw"
            // Applies from the next turn, as `/model` does.
            | "quiet"
    ) || matches!(parse_cmd(line), ("goal" | "team", a) if a.is_empty() || matches!(a.split_whitespace().next(), Some("status" | "pause" | "stop" | "list" | "--bg")))
}

/// The environment-only commands. Returns `None` for anything else, which the
/// caller then runs against the agent.
///
/// `/model` deliberately does not rebuild the system prompt here: the agent may
/// be mid-turn and holding its lock. `Agent::run_turn` refreshes the prompt when
/// it notices the model changed, so the switch is still applied before the next
/// request.
pub async fn handle_env(env: &std::sync::Arc<Env>, line: &str) -> Option<Outcome> {
    let (cmd, arg) = parse_cmd(line);
    Some(match cmd {
        "quiet" => {
            let on = match arg {
                "on" => true,
                "off" => false,
                "" => !env.quiet(),
                _ => return Some(Outcome::Show("Usage: `/quiet [on|off]`".into())),
            };
            // The agent may be mid-turn holding its lock: `run_turn` rebuilds
            // the system prompt when it sees the switch, as for `/model`.
            env.quiet.store(on, std::sync::atomic::Ordering::Relaxed);
            Outcome::Show(if on {
                "Quiet mode **on** (experimental): the model skips narration between tool calls and ends with a short summary. \
                 The TUI shows only the status line while it works; ctrl+o or `/transcript` shows the steps."
                    .into()
            } else {
                "Quiet mode **off**.".into()
            })
        }
        "keybindings" => {
            let path = crate::config::home_dir().join("keybindings.json");
            let created = !path.exists()
                && std::fs::create_dir_all(crate::config::home_dir())
                    .and_then(|_| std::fs::write(&path, crate::tui::keybindings::TEMPLATE))
                    .is_ok();
            let claude = crate::compat::claude_dir().join("keybindings.json");
            let also = if env.config.compat.claude && claude.exists() {
                format!(
                    "\n\nClaude Code's `{}` is read too; this file wins where both bind a key.",
                    crate::util::tilde(&claude)
                )
            } else {
                String::new()
            };
            Outcome::Show(format!(
                "{} `{}`. It uses Claude Code's format: blocks of `{{\"context\": \"Chat\", \"bindings\": {{\"ctrl+e\": \"chat:externalEditor\", \"ctrl+s\": null}}}}`; `null` unbinds a key, and chords are keys separated by spaces (`ctrl+k ctrl+s`). Saved changes apply right away. Actions and contexts: https://code.claude.com/docs/en/keybindings{also}",
                if created {
                    "Created"
                } else {
                    "Your keybindings:"
                },
                crate::util::tilde(&path)
            ))
        }
        "btw" if arg == "fork" || arg.starts_with("fork ") => {
            // A background fork of a side answer inherits the conversation plus
            // that question and answer, with full tool access.
            // `/btw fork <n>` picks the n-th side question (the overlay passes it).
            let btw = env.btw.lock().unwrap().clone();
            let pick = arg
                .strip_prefix("fork")
                .unwrap_or("")
                .trim()
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1));
            let Some((q, a)) = pick.map_or(btw.last(), |i| btw.get(i)).cloned() else {
                return Some(Outcome::Show(
                    "No side question to fork yet: ask one with `/btw <question>`.".into(),
                ));
            };
            let mut history = env.conversation.read().unwrap().as_ref().clone();
            history.push(crate::llm::Message::user(format!("(Side question) {q}")));
            history.push(crate::llm::Message::new(crate::llm::Role::Assistant, a));
            let ctx = crate::tools::Ctx {
                env: env.clone(),
                cancel: CancellationToken::new(),
                depth: 0,
                call_id: None,
                cwd_override: None,
                reads: None,
            };
            let opts = crate::agent::SubagentOpts {
                background: true,
                description: format!("/btw fork: {}", crate::util::one_line(&q, 50)),
                history: Some(history),
                ..Default::default()
            };
            let prompt = format!(
                "Follow up on the side question above with full tool access: {q}\nInvestigate or make the change it calls for, then report back."
            );
            return Some(Outcome::Show(
                match crate::agent::run_subagent(&ctx, "fork", &prompt, opts).await {
                    Ok(out) if !out.is_error => format!(
                        "Forked the side question into a background agent. {}",
                        out.display.as_deref().unwrap_or("")
                    ),
                    Ok(out) => format!("Couldn't fork: {}", out.content),
                    Err(e) => format!("Couldn't fork: {e:#}"),
                },
            ));
        }
        "btw" => {
            if arg.is_empty() {
                return Some(Outcome::Show(match env.btw.lock().unwrap().last() {
                    Some((q, a)) => format!("**{q}**\n\n{a}"),
                    None => "Ask a side question that doesn't change the conversation: `/btw <question>`.\nIt sees the context so far (mid-turn too) but has no tools, and neither the question nor the answer is added to the history.".into(),
                }));
            }
            let conversation = env.conversation.read().unwrap().clone();
            match crate::agent::Agent::side_question_on(env, &env.model(), &conversation, arg).await
            {
                Ok(a) => {
                    let a = a.trim().to_string();
                    env.btw.lock().unwrap().push((arg.to_string(), a.clone()));
                    Outcome::Show(format!("**/btw {arg}**\n\n{a}"))
                }
                Err(e) => Outcome::Show(format!("Side question failed: {e:#}")),
            }
        }
        "help" | "?" => {
            let mut s = String::from("## Commands\n");
            for c in BUILTIN {
                s.push_str(&format!(
                    "- `/{}{}` — {}\n",
                    c.name,
                    if c.args.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", c.args)
                    },
                    c.help
                ));
            }
            s.push_str("\n## Bundled skills\nA skill or command of your own with the same name replaces one.\n");
            for c in crate::bundled::BUNDLED {
                s.push_str(&format!(
                    "- `/{}{}` — {}\n",
                    c.name,
                    if c.args.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", c.args)
                    },
                    c.help
                ));
            }
            let prompts = crate::mcp::prompt_commands(&env.mcp);
            if !prompts.is_empty() {
                s.push_str("\n## MCP prompts\n");
                for (n, d) in prompts {
                    s.push_str(&format!("- `/{n}` — {d}\n"));
                }
            }
            let commands = env.commands.read().unwrap().clone();
            if !commands.is_empty() {
                s.push_str("\n## Custom commands\n");
                for c in &commands {
                    s.push_str(&format!(
                        "- `{}` — {}\n",
                        usage(&c.name, &c.meta),
                        c.description
                    ));
                }
            }
            let skills: Vec<_> = env
                .skills
                .read()
                .unwrap()
                .iter()
                .filter(|k| k.user_invocable)
                .cloned()
                .collect();
            if !skills.is_empty() {
                s.push_str("\n## Skills\n");
                for k in &skills {
                    s.push_str(&format!(
                        "- `{}` — {}\n",
                        usage(&k.name, &k.meta),
                        k.description
                    ));
                }
            }
            s.push_str("\n## Shortcuts\n");
            s.push_str(&shortcuts_markdown());
            Outcome::Show(s)
        }
        "exit" | "quit" | "q" => Outcome::Exit,
        "bg" => Outcome::Show(crate::runs::cli::slash_bg(env, arg).await),
        "remote" | "remote-control" => Outcome::Show(crate::remote::command(env, arg).await),
        "goal" if arg.starts_with("--bg") => {
            let objective = arg["--bg".len()..].trim();
            if objective.is_empty() {
                return Some(Outcome::Show("Usage: `/goal --bg <objective>`".into()));
            }
            Outcome::Show(crate::runs::cli::slash_bg_kind(env, objective, Some("goal")).await)
        }
        "goal"
            if arg.is_empty()
                || matches!(
                    arg.split_whitespace().next(),
                    Some("status" | "pause" | "stop" | "list")
                ) =>
        {
            Outcome::Show(crate::goal::command(&env.root, arg))
        }
        "team"
            if arg.is_empty()
                || matches!(
                    arg.split_whitespace().next(),
                    Some("status" | "stop" | "list")
                ) =>
        {
            Outcome::Show(crate::team::command(&env.root, arg).await)
        }
        "runs" => Outcome::Show(crate::runs::cli::slash_runs(env, arg).await),
        "upgrade" => Outcome::Show(crate::update::slash().await),
        "schedule" => Outcome::Show(crate::runner::slash(&env.root, arg).await),
        "stats" => Outcome::Show(crate::insights::usage(
            &Session::summaries(),
            &env.cwd(),
            chrono::Utc::now().timestamp(),
        )),
        "mode" => {
            if arg.is_empty() {
                return Some(Outcome::Show(format!("Mode: **{}**", env.mode().label())));
            }
            match Mode::parse(arg) {
                Some(m) => {
                    env.set_mode(m);
                    Outcome::Nothing
                }
                None => Outcome::Show(
                    "Unknown mode. Use default, acceptEdits, plan, auto or bypass.".into(),
                ),
            }
        }
        "model" => {
            if arg.is_empty() {
                return Some(Outcome::Show(format!(
                    "Model: **{}** (context {} tokens, vision {})",
                    env.model(),
                    env.context_max(),
                    if env.vision() { "on" } else { "off" }
                )));
            }
            // The choice becomes the default for new sessions;
            // `--session` switches this session only. (`--save` is the old spelling.)
            let (arg, save) = match arg.strip_prefix("--session ") {
                Some(m) => (m.trim(), false),
                None => (arg.strip_prefix("--save ").unwrap_or(arg).trim(), true),
            };
            if let Some(why) = crate::detect::missing_login(&env.config, arg) {
                return Some(Outcome::Show(why));
            }
            if let Err(why) = crate::hooks::switch_model(env, arg, "command").await {
                return Some(Outcome::Show(why));
            }
            if save && let Err(e) = crate::onboarding::save(arg, None, None) {
                return Some(Outcome::Show(format!("Could not save model: {e}")));
            }
            let (p, m) = env.resolve(arg);
            // OpenAI-compatible servers report no capabilities; assume vision so
            // a newly switched-to model doesn't silently drop images.
            let assumed = p.kind.assumes_vision();
            env.set_vision(p.model_info(&m).await.map(|(_, v)| v).unwrap_or(assumed));
            Outcome::Show(if save {
                format!(
                    "Switched model to **{arg}** (saved as the default; `/model --session <model>` switches this session only)"
                )
            } else {
                format!("Switched model to **{arg}** for this session")
            })
        }
        "models" => match Ok::<Vec<String>, anyhow::Error>(
            crate::onboarding::refresh_models(env, false).await,
        ) {
            Ok(ms) => Outcome::Show(format!(
                "## Models\n{}",
                ms.iter()
                    .map(|m| format!(
                        "- `{m}`{}",
                        if *m == env.model() { " (current)" } else { "" }
                    ))
                    .collect::<Vec<_>>()
                    .join("\n")
            )),
            Err(e) => Outcome::Show(format!("Could not list models: {e}")),
        },
        "verify" => {
            use crate::guardrails::Scope;
            if !arg.is_empty() {
                let Some(scope) = Scope::parse(arg) else {
                    return Some(Outcome::Show("Usage: `/verify [full|related]`".into()));
                };
                *env.verify_scope.write().unwrap() = Some(scope);
            }
            let scope = env
                .verify_scope
                .read()
                .unwrap()
                .or(env.config.guardrails.scope)
                .unwrap_or(Scope::Related);
            let state = if env.config.guardrails.enabled {
                "on"
            } else {
                "off (start with `--verify`)"
            };
            Outcome::Show(format!(
                "Verified mode: {state}. Done-gate runs: **{}**.",
                match scope {
                    Scope::Related =>
                        "tests related to the changed files (full suite when unsure); `/verify full` for everything",
                    Scope::Full => "the full test suite; `/verify related` for related tests only",
                }
            ))
        }
        "peers" => {
            let peers = crate::presence::peers();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            Outcome::Show(if peers.is_empty() {
                "No other chanto sessions are working in this project.".into()
            } else {
                format!(
                    "## Other sessions in this project\n{}\n\nEach is told about the others; an edit to a file another just changed stops once to say so.",
                    peers
                        .iter()
                        .map(|p| crate::presence::describe(p, now))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            })
        }
        "think" | "effort" => {
            if crate::llm::EFFORTS.contains(&arg) {
                env.set_think(true);
                env.set_effort(Some(arg.to_string()));
                return Some(Outcome::Show(format!("Thinking enabled, effort {arg}")));
            }
            let on = match arg {
                "on" | "true" => true,
                "off" | "false" => false,
                "" if cmd == "effort" => {
                    let e = env.effort();
                    return Some(Outcome::Show(format!(
                        "Effort: {} (set with /think low|medium|high|xhigh|max)",
                        e.as_deref().unwrap_or("the provider's default")
                    )));
                }
                "" => !env.think(),
                _ => {
                    return Some(Outcome::Show(format!(
                        "Usage: /think [on|off|{}]",
                        crate::llm::EFFORTS.join("|")
                    )));
                }
            };
            env.set_think(on);
            let effort = env
                .effort()
                .filter(|_| on)
                .map(|e| format!(", effort {e}"))
                .unwrap_or_default();
            Outcome::Show(format!(
                "Thinking {}{effort}",
                if on { "enabled" } else { "disabled" }
            ))
        }
        "theme" => {
            use crate::tui::theme;
            if arg.is_empty() {
                let cur = theme::current();
                let list: Vec<String> = theme::available()
                    .into_iter()
                    .map(|(n, user)| {
                        format!(
                            "- `{n}`{}{}",
                            if user { " (yours)" } else { "" },
                            if n == cur { " — current" } else { "" }
                        )
                    })
                    .collect();
                return Some(Outcome::Show(format!(
                    "Theme: **{cur}**. Switch with `/theme <name>` (or `auto`):\n{}\n\nAdd your own as `{}/<name>.toml`; see docs/reference.md for the keys.",
                    list.join("\n"),
                    theme::dir().display()
                )));
            }
            match theme::switch(arg) {
                Ok(name) => Outcome::Show(format!(
                    "Theme: **{name}** (set `theme = \"{arg}\"` in ~/.chanto/config.toml to keep it)"
                )),
                Err(e) => Outcome::Show(e),
            }
        }
        "vision" => {
            let on = match arg {
                "on" => true,
                "off" => false,
                _ => !env.vision(),
            };
            env.set_vision(on);
            Outcome::Show(format!(
                "Vision {}",
                if on { "enabled" } else { "disabled" }
            ))
        }
        // Read-only inspection: these touch only the environment, so they run
        // straight away instead of queuing behind an in-flight turn.
        "permissions" => {
            let r = env.rules.read().unwrap();
            let fmt = |v: &Vec<Rule>| {
                if v.is_empty() {
                    "(none)".to_string()
                } else {
                    v.iter()
                        .map(|r| format!("`{r}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            };
            Outcome::Show(format!(
                "## Permissions\nMode: **{}**\n\n- allow: {}\n- ask: {}\n- deny: {}\n\nAdd a rule with `/allow`, `/ask` or `/deny` (e.g. `/allow Bash(npm test:*)`); remove one with `/unallow`, `/unask` or `/undeny`. In the full-screen UI, `/permissions` lists the rules to pick one to remove.",
                env.mode().label(),
                fmt(&r.allow),
                fmt(&r.ask),
                fmt(&r.deny)
            ))
        }
        // Remove a session rule. Pairs with `/allow` etc. so the interactive
        // `/permissions` picker has a command to run when a rule is chosen.
        "unallow" | "unask" | "undeny" => {
            let set = cmd.trim_start_matches("un");
            if arg.is_empty() {
                Outcome::Show(format!("Usage: /{cmd} <rule>"))
            } else {
                let mut r = env.rules.write().unwrap();
                let list = match set {
                    "allow" => &mut r.allow,
                    "ask" => &mut r.ask,
                    _ => &mut r.deny,
                };
                let before = list.len();
                list.retain(|x| x.to_string() != arg);
                Outcome::Show(if list.len() < before {
                    format!("Removed {set} rule `{arg}`")
                } else {
                    format!("No {set} rule matching `{arg}`")
                })
            }
        }
        "tools" => {
            let mut s = String::from("## Tools\n");
            for t in &env.tools.tools {
                s.push_str(&format!(
                    "- **{}** — {}\n",
                    t.name(),
                    crate::util::one_line(&t.description(), 100)
                ));
            }
            for c in &env.mcp {
                for n in c.tool_names() {
                    s.push_str(&format!("- **{n}** (MCP)\n"));
                }
            }
            Outcome::Show(s)
        }
        "skills" => {
            let skills = env.skills.read().unwrap().clone();
            Outcome::Show(if skills.is_empty() {
                "No skills found (looked in ~/.claude/skills, ~/.agents/skills, ~/.codex/skills, ~/.chanto/skills, and .claude/skills, .agents/skills, .codex/skills, .chanto/skills from the git root down).".into()
            } else {
                let note = |s: &crate::context::Skill| match (s.model_invocable, s.user_invocable) {
                    (false, _) => " (only as /name)",
                    (true, false) => " (model only)",
                    _ => "",
                };
                format!(
                    "## Skills\n{}",
                    skills
                        .iter()
                        .map(|s| format!("- **{}**{} — {}", s.name, note(s), s.description))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            })
        }
        "agents" => {
            let agents = env.agents.read().unwrap().clone();
            // With a name, show the full definition (system prompt + settings) so
            // the interactive `/agents` picker has something to open.
            if !arg.is_empty() {
                if let Some(a) = agents.iter().find(|a| a.name == arg) {
                    return Some(Outcome::Show(format!(
                        "## {}\n{}\n\n**Allowed tools:** {}{}\n\n{}",
                        a.name,
                        a.description,
                        match &a.tools {
                            None => "(all)".to_string(),
                            Some(t) if t.is_empty() => "(none)".to_string(),
                            Some(t) => t.join(", "),
                        },
                        if a.disallowed_tools.is_empty() {
                            String::new()
                        } else {
                            format!(" except {}", a.disallowed_tools.join(", "))
                        },
                        if a.prompt.is_empty() {
                            "(no extra instructions)".into()
                        } else {
                            a.prompt.clone()
                        }
                    )));
                }
                return Some(Outcome::Show(format!("No subagent named `{arg}`.")));
            }
            Outcome::Show(format!(
                "## Subagents\n- **Explore** — read-only codebase exploration\n- **general-purpose** — multi-step tasks\n- **oracle** — read-only second opinion on plans and diffs (`oracle_model`)\n{}\n\nPass a name to see its definition: `/agents Explore`.",
                agents
                    .iter()
                    .map(|a| format!("- **{}** — {}", a.name, a.description))
                    .collect::<Vec<_>>()
                    .join("\n")
            ))
        }
        "todos" => {
            let t = env.todos.lock().unwrap().clone();
            if t.is_empty() {
                return Some(Outcome::Show("No todos.".into()));
            }
            Outcome::Show(
                t.iter()
                    .map(|t| {
                        format!(
                            "- [{}] {}",
                            if t.status == "completed" {
                                "x"
                            } else if t.status == "in_progress" {
                                "~"
                            } else {
                                " "
                            },
                            t.content
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
        "mcp" => mcp_command(env, arg).await,
        "bashes" => {
            let shells = env.shells.lock().unwrap();
            if shells.is_empty() {
                return Some(Outcome::Show("No background shells.".into()));
            }
            Outcome::Show(
                shells
                    .values()
                    .map(|s| {
                        let what = match &s.monitor {
                            Some(d) => format!("monitor: {d} (`{}`)", s.command),
                            None => s.command.clone(),
                        };
                        format!(
                            "- `{}` {} — {what}",
                            s.id,
                            if s.exit.lock().unwrap().is_some() {
                                "exited"
                            } else {
                                "running"
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
        _ => return None,
    })
}

/// `/transcript` in the plain REPL: the last turn's steps (the assistant's
/// text and each tool call with the first line of its result), which quiet
/// mode leaves out of the output. The session itself keeps all of it.
pub fn last_turn_steps(agent: &Agent) -> String {
    use crate::llm::Role;
    let msgs = &agent.messages;
    let start = msgs
        .iter()
        .rposition(|m| {
            m.role == Role::User && !m.content.trim_start().starts_with("<system-reminder>")
        })
        .map_or(0, |i| i + 1);
    let mut out = Vec::new();
    for m in &msgs[start..] {
        if m.role != Role::Assistant {
            continue;
        }
        if !m.content.trim().is_empty() {
            out.push(m.content.trim().to_string());
        }
        for c in &m.tool_calls {
            let label = agent
                .env
                .tools
                .get(&c.name)
                .map(|t| t.label(&c.arguments))
                .unwrap_or_else(|| c.name.clone());
            let result = msgs
                .iter()
                .find(|x| x.role == Role::Tool && x.tool_call_id.as_deref() == Some(&c.id));
            let first = result.and_then(|r| r.content.lines().next()).unwrap_or("");
            out.push(format!(
                "- `{}`: {}",
                crate::util::one_line(&label, 100),
                crate::util::one_line(first, 120)
            ));
        }
    }
    if out.is_empty() {
        "No steps in the last turn.".into()
    } else {
        format!("## Last turn\n{}", out.join("\n\n"))
    }
}

/// Load the saved session `id` for this directory and make it the live
/// conversation (shared by `/resume` and `/rewind`'s undo-a-clear). Returns its
/// title, or the load error.
async fn adopt_session(
    agent: &mut Agent,
    env: &std::sync::Arc<Env>,
    id: &str,
) -> Result<String, String> {
    let cwd = env.cwd();
    let (mut sess, msgs) = Session::load(&cwd, id).map_err(|e| format!("{e}"))?;
    sess.archived = false;
    agent.save_session();
    agent.reset();
    env.set_model(&sess.model);
    agent.messages = msgs;
    agent.refresh_system();
    let title = sess.title.clone();
    env.accounting
        .restore(sess.stats.total_cost_usd, sess.stats.price_unknown);
    agent.stats = sess.stats.clone();
    agent.session = Some(sess);
    agent.restore_todos();
    agent.refresh_system();
    agent.save_session();
    // The status line shows the resumed session's cost and context now.
    agent.emit_usage(0.0);
    env.session_start("resume").await;
    Ok(title)
}

/// The note shown after `/clear`: how to undo it, when there's something to undo.
fn cleared_hint(cleared: &Option<String>) -> Option<String> {
    cleared
        .as_ref()
        .map(|_| "Conversation cleared — `/rewind` restores it.".to_string())
}

pub async fn handle(agent: &mut Agent, line: &str) -> Outcome {
    let line = line.trim();
    let env = agent.env.clone();
    // Idle, /btw sees the conversation as it stands, final answer included.
    if parse_cmd(line).0 == "btw" {
        agent.publish_conversation();
    }
    // `/model <smaller>`: a conversation the new model can't hold is summarized
    // first, by the current model, which can still read all of it.
    if let ("model", arg) = parse_cmd(line) {
        let to = arg
            .strip_prefix("--session ")
            .or_else(|| arg.strip_prefix("--save "))
            .unwrap_or(arg)
            .trim();
        let current = agent.model();
        if !to.is_empty()
            && to != current
            && agent.window_of(to) < agent.window_of(&current)
            && let Some((used, window)) = agent.outgrows(to)
        {
            let compact = if env.interactive {
                let q = format!(
                    "The conversation (~{used} tokens) is too long for {to}'s {window}-token context. Compact it with {current} first?"
                );
                let options = vec![
                    "Compact first, then switch".to_string(),
                    "Switch now (it compacts before the next request)".to_string(),
                    "Cancel".to_string(),
                ];
                match env.frontend.choose(&q, &options).await.as_deref() {
                    Some(a) if a == options[0] => true,
                    Some(a) if a == options[1] => false,
                    _ => return Outcome::Show(format!("Kept {current}.")),
                }
            } else {
                true
            };
            if compact {
                if let Err(e) = agent
                    .compact(None, "auto", &tokio_util::sync::CancellationToken::new())
                    .await
                {
                    return Outcome::Show(format!(
                        "Couldn't compact before switching, so {current} stays: {e:#}"
                    ));
                }
                agent.save_session();
            }
        }
    }
    // `/help` in an external-agent session also lists that agent's controls and
    // advertised commands, which live on the agent.
    if matches!(parse_cmd(line).0, "help" | "?")
        && let Some(acp) = &agent.acp
        && let Some(Outcome::Show(mut text)) = handle_env(&env, line).await
    {
        let mut section = String::from(
            "\n## External agent\nThis session drives an ACP agent: `/agent-mode`, `/agent-config`, `/agent-login` and `/agent-logout` control it.\n",
        );
        let commands = acp.available_commands();
        if !commands.is_empty() {
            section.push_str("\n### Agent commands\nThese run in the external agent.\n");
            for (name, description) in commands {
                section.push_str(&format!("- `/{name}` — {description}\n"));
            }
        }
        if let Some(at) = text.find("\n## Shortcuts") {
            text.insert_str(at, &section);
        } else {
            text.push_str(&section);
        }
        return Outcome::Show(text);
    }
    // Settings commands touch only the environment, so they apply straight away
    // rather than waiting for the turn to finish.
    if let Some(o) = handle_env(&env, line).await {
        return o;
    }
    let (cmd, arg) = parse_cmd(line);
    let commands = env.commands.read().unwrap().clone();
    let skills = env.skills.read().unwrap().clone();
    let agents = env.agents.read().unwrap().clone();
    let output_styles = env.output_styles.read().unwrap().clone();
    let hooks = env.hooks.read().unwrap().clone();
    match cmd {
        "agent-mode" => {
            let Some(acp) = agent.acp.as_mut() else {
                return Outcome::Show(
                    "This session is not driving an external ACP agent.".into(),
                );
            };
            if arg.is_empty() {
                return Outcome::Show(acp.modes_summary());
            }
            match acp
                .set_mode(&env, arg, CancellationToken::new())
                .await
            {
                Ok(message) => Outcome::Show(message),
                Err(e) => Outcome::Show(format!("Could not set the agent mode: {e:#}")),
            }
        }
        "agent-config" => {
            let Some(acp) = agent.acp.as_mut() else {
                return Outcome::Show(
                    "This session is not driving an external ACP agent.".into(),
                );
            };
            let (id, value) = arg
                .split_once(char::is_whitespace)
                .map(|(id, value)| (id, Some(value.trim()).filter(|value| !value.is_empty())))
                .unwrap_or((arg, None));
            if id.is_empty() {
                return Outcome::Show(acp.settings_summary());
            }
            match acp
                .set_config_option(&env, id, value, CancellationToken::new())
                .await
            {
                Ok(message) => Outcome::Show(message),
                Err(e) => Outcome::Show(format!("Could not set the agent setting: {e:#}")),
            }
        }
        "agent-login" | "agent-auth" => {
            let Some(acp) = agent.acp.as_mut() else {
                return Outcome::Show(
                    "This session is not driving an external ACP agent.".into(),
                );
            };
            let method = (!arg.is_empty()).then_some(arg);
            match acp.login(&env, method, CancellationToken::new()).await {
                Ok(message) => Outcome::Show(message),
                Err(e) => Outcome::Show(format!("Could not sign in: {e:#}")),
            }
        }
        "agent-logout" => {
            let Some(acp) = agent.acp.as_mut() else {
                return Outcome::Show(
                    "This session is not driving an external ACP agent.".into(),
                );
            };
            match acp.logout(&env, CancellationToken::new()).await {
                Ok(message) => Outcome::Show(message),
                Err(e) => Outcome::Show(format!("Could not sign out: {e:#}")),
            }
        }
        "customize" => {
            if arg.is_empty() {
                return Outcome::Show("Usage: `/customize <request>`".into());
            }
            agent.enable_customization();
            Outcome::Prompt(format!("Customize chanto: {arg}"))
        }
        "reload" => match crate::customize::reload_checked(&env).await {
            Ok(report) => {
                agent.refresh_system_prompt();
                Outcome::Show(report)
            }
            Err(e) => Outcome::Show(format!("Reload failed (existing extensions retained): {e:#}")),
        },
        "outcome" | "build" => {
            if arg.is_empty() {
                return Outcome::Show("Usage: `/outcome <what you want built>`. chanto writes executable acceptance criteria for you to approve, builds until they all pass, then reports the evidence and the changes a human should still review.".into());
            }
            Outcome::Run(LongRun::Outcome(arg.to_string()))
        }
        "goal" => match arg.strip_prefix("resume") {
            Some(id) if id.is_empty() || id.starts_with(' ') => Outcome::Run(LongRun::GoalResume(Some(id.trim().to_string()).filter(|i| !i.is_empty()))),
            _ => Outcome::Run(LongRun::Goal(arg.to_string())),
        },
        "team" => Outcome::Run(LongRun::Team(arg.to_string())),
        "learned" | "learn" => {
            let root = env.root.clone();
            let (sub, rest) = arg.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((arg, ""));
            let show = |l: &crate::learn::Lesson| {
                let extra = if l.kind == "check" { " · check" } else { "" };
                format!("- `{}` [{}{}{}] {}\n", l.id, l.status, if l.scope == "global" { " · global" } else { "" }, extra, l.text)
            };
            match sub {
                "review" => match crate::learn::review(&env, &root).await {
                    Ok(p) if p.is_empty() => Outcome::Show("No new lessons to propose yet.".into()),
                    Ok(p) => {
                        let mut s = String::from("## Proposed lessons\nNothing applies until you accept it.\n");
                        for l in &p {
                            s.push_str(&show(l));
                            s.push_str(&format!("  evidence: {}\n", l.evidence.join(", ")));
                        }
                        s.push_str("\n`/learned accept <id>` (or `all`), `/learned reject <id>`.");
                        Outcome::Show(s)
                    }
                    Err(e) => Outcome::Show(format!("Review failed: {e:#}")),
                },
                "accept" | "reject" | "disable" | "enable" => {
                    let (from, to): (&[&str], &str) = match sub {
                        "accept" => (&["proposed"], "active"),
                        "reject" => (&["proposed"], "rejected"),
                        "disable" => (&["active"], "disabled"),
                        _ => (&["disabled", "rejected"], "active"),
                    };
                    match crate::learn::set_status(&root, if rest.is_empty() { "all" } else { rest }, from, to) {
                        Ok(v) if v.is_empty() => Outcome::Show(format!("No matching lesson for `{rest}`.")),
                        Ok(v) => Outcome::Show(format!("{} lesson(s) now {to}. Rule lessons take effect in new sessions (or after /clear).{}", v.len(), v.iter().filter(|l| l.kind != "check").map(|l| format!("\n\x1b[2m{} memory: lesson-{}\x1b[0m", if to == "active" { "Saved" } else { "Deleted" }, l.id)).collect::<String>())),
                        Err(e) => Outcome::Show(format!("{e:#}")),
                    }
                }
                "add" => {
                    let (global, text) = match rest.strip_prefix("--global") {
                        Some(t) => (true, t.trim()),
                        None => (false, rest),
                    };
                    match crate::learn::add(&root, text, global) {
                        Ok(l) => Outcome::Show(format!("Added lesson `{}`.\n\x1b[2mSaved memory: lesson-{}\x1b[0m", l.id, l.id)),
                        Err(e) => Outcome::Show(format!("{e:#}")),
                    }
                }
                "undo" => Outcome::Show(match crate::learn::undo(&root) {
                    Some(a) => format!("Undid the last change ({a})."),
                    None => "Nothing to undo.".into(),
                }),
                "signals" => {
                    let sig = crate::learn::signals(&root);
                    let mut s = format!("## Recorded signals ({} total, newest last)\n", sig.len());
                    for x in sig.iter().rev().take(20).rev() {
                        s.push_str(&format!("- `{}` {} — {}\n", x.id, x.kind, crate::util::one_line(&x.text, 100)));
                    }
                    Outcome::Show(s)
                }
                _ => {
                    let all = crate::learn::lessons(&root);
                    let sig = crate::learn::signals(&root).len();
                    if all.is_empty() {
                        return Outcome::Show(format!(
                            "No lessons yet ({sig} signals recorded). chanto notes your corrections (denials with feedback, \"no, …\" messages, edits to files it just wrote, rewinds, repeated !commands). \
                             Run `/learned review` to turn them into proposed lessons; nothing applies until you accept it."
                        ));
                    }
                    let mut s = String::from("## Lessons\n");
                    for l in all.iter().filter(|l| l.status != "rejected") {
                        s.push_str(&show(l));
                    }
                    s.push_str(&format!("\n{sig} signals recorded. `/learned review` · `accept|reject|disable|enable <id>` · `add [--global] <text>` · `undo` · `signals`"));
                    Outcome::Show(s)
                }
            }
        }
        "tasks" => {
            if let Some(id) = arg.strip_prefix("stop").map(str::trim).filter(|s| !s.is_empty()) {
                return Outcome::Show(if crate::agent::stop_background(&env, id) { format!("Stopping {id}…") } else { format!("No running agent `{id}`") });
            }
            let list = env.bg_agents.lock().unwrap();
            if list.is_empty() {
                return Outcome::Show("No background agents. The model starts them with Task(run_in_background: true).".into());
            }
            let mut s = String::from("## Background agents\n");
            for b in list.iter() {
                let wt = b.worktree.as_ref().map(|w| format!(" · worktree `{}` (branch `{}`)", w.path.display(), w.branch)).unwrap_or_default();
                s.push_str(&format!("- `{}` **{}** ({}) — {} · {}{wt}\n", b.id, b.description, b.kind, b.status, crate::util::duration(b.started.elapsed().as_secs())));
            }
            s.push_str("\nStop one with `/tasks stop <id>`.");
            Outcome::Show(s)
        }
        "plugin" => {
            let args = match shell_words::split(arg) {
                Ok(args) => args,
                Err(e) => return Outcome::Show(e.to_string()),
            };
            let frontend = env.frontend.clone();
            let interactive = env.interactive;
            let runtime = tokio::runtime::Handle::current();
            let result = tokio::task::spawn_blocking(move || {
                crate::plugin::run(&crate::plugin::root(), &args, &mut |summary| {
                    if !interactive { return false; }
                    runtime.block_on(frontend.choose(
                        &format!("{summary}\nPlugin can run code. Install?"),
                        &["yes".into(), "no".into()],
                    )).is_some_and(|answer| answer.eq_ignore_ascii_case("yes"))
                })
            }).await.unwrap_or_else(|e| Err(e.into()));
            Outcome::Show(match result {
                Ok(s) => format!("{s}\nChanges apply after /reload (Markdown and hooks) or restart (MCP/LSP)."),
                Err(e) => format!("{e:#}"),
            })
        }
        "plugins" => {
            let ps = crate::compat::active_plugins();
            if ps.is_empty() {
                return Outcome::Show("No plugins in use. Use /plugin marketplace add <source> and /plugin install <plugin>@<marketplace>.".into());
            }
            let mut s = String::from("## Plugins\n");
            for p in ps {
                let count = |items: usize, what: &str| if items > 0 { format!(" · {items} {what}") } else { String::new() };
                let prefix = format!("{}:", p.name);
                s.push_str(&format!(
                    "- **{}** `{}`{}{}{}\n",
                    p.name,
                    p.root.display(),
                    count(commands.iter().filter(|c| c.name.starts_with(&prefix)).count(), "commands"),
                    count(agents.iter().filter(|a| a.name.starts_with(&prefix)).count(), "agents"),
                    count(skills.iter().filter(|k| k.name.starts_with(&prefix)).count(), "skills"),
                ));
            }
            Outcome::Show(s)
        }
        "ide" => Outcome::Show(crate::ide::command(&env, arg).await),
        "hooks" => {
            if hooks.is_empty() {
                return Outcome::Show("No hooks configured. Add them under `[[hooks.<Event>]]` in config.toml or `hooks` in .claude/settings.json.".into());
            }
            // `/hooks test <Event> <index>` runs one hook so you can see what it
            // does before it fires for real; the interactive picker uses it.
            if let Some(rest) = arg.strip_prefix("test ") {
                let mut it = rest.rsplitn(2, char::is_whitespace);
                let idx: usize = it.next().and_then(|n| n.parse().ok()).unwrap_or(usize::MAX);
                let ev = it.next().unwrap_or("").trim();
                let Some(list) = hooks.iter().find(|(e, _)| e.eq_ignore_ascii_case(ev)).map(|(_, l)| l) else {
                    return Outcome::Show(format!("No hooks for event `{ev}`."));
                };
                let Some(h) = list.get(idx) else {
                    return Outcome::Show(format!("`{ev}` has {} hook(s).", list.len()));
                };
                if h.kind == "http" {
                    return Outcome::Show(format!("`{ev}` hook {idx} is an http hook (POST {}); only command hooks can be test-run.", h.url));
                }
                return run_hook_test(&h.command, ev).await;
            }
            let mut s = String::from("## Hooks\n");
            for (ev, list) in &hooks {
                for h in list {
                    let m = if h.matcher.is_empty() { String::new() } else { format!(" `{}`", h.matcher) };
                    s.push_str(&format!("- **{ev}**{m}: `{}`\n", h.describe()));
                }
            }
            s.push_str("\nRun one with `/hooks test <Event> <index>`.");
            Outcome::Show(s)
        }
        "clear" | "new" | "reset" => {
            agent.save_session();
            // Remember the conversation being cleared (when it had real content)
            // so `/rewind` can undo an accidental clear by resuming it.
            let cleared = agent
                .session
                .as_ref()
                .map(|s| s.id.clone())
                .filter(|_| agent.messages.iter().any(|m| m.role == crate::llm::Role::User));
            agent.reset();
            env.session_end("clear").await;
            agent.session = Some(Session::new(&env.cwd(), &env.model()));
            agent.cleared_session = cleared;
            agent.save_session();
            env.session_start("clear").await;
            Outcome::Reset(cleared_hint(&agent.cleared_session))
        }
        "compact" => {
            let cancel = CancellationToken::new();
            match agent.compact(if arg.is_empty() { None } else { Some(arg) }, "manual", &cancel).await {
                Ok(()) => Outcome::Nothing,
                Err(e) => Outcome::Show(format!("Compaction failed: {e:#}")),
            }
        }
        "add-dir" => {
            if arg.is_empty() {
                let dirs = crate::permissions::extra_dirs();
                return Outcome::Show(if dirs.is_empty() {
                    "No additional directories. Usage: `/add-dir <path>`".into()
                } else {
                    format!("Additional directories:\n{}", dirs.iter().map(|d| format!("- `{}`", d.display())).collect::<Vec<_>>().join("\n"))
                });
            }
            let p = crate::util::resolve(&env.cwd(), arg);
            if !p.is_dir() {
                return Outcome::Show(format!("Not a directory: `{}`", p.display()));
            }
            let mut dirs: Vec<String> = crate::permissions::extra_dirs().iter().map(|d| d.display().to_string()).collect();
            // Already a working directory, or inside one: DirectoryAdded doesn't fire.
            let canon = |d: &std::path::Path| std::fs::canonicalize(d).unwrap_or_else(|_| d.to_path_buf());
            let known = std::iter::once(env.root.clone()).chain(crate::permissions::extra_dirs()).any(|d| canon(&p).starts_with(canon(&d)));
            dirs.push(p.display().to_string());
            crate::permissions::set_extra_dirs(&dirs, &env.cwd());
            if !known {
                crate::hooks::directory_added(&env, &p);
            }
            Outcome::Show(format!("Added `{}` to the workspace for this session", p.display()))
        }
        "login" => {
            let say = |_: &str| {};
            let words: Vec<&str> = arg.split_whitespace().collect();
            // `/login <provider> --as <name>`: sign in as a named account.
            let account = words.iter().position(|w| *w == "--as").and_then(|i| words.get(i + 1).copied());
            if account.is_some_and(|a| !crate::auth::valid_account(a) || a == crate::auth::DEFAULT_ACCOUNT) {
                return Outcome::Show("Account names are letters, digits, `.`, `_` and `-`.".into());
            }
            // `/login <provider> <name>`: switch to a saved account for this session.
            if let [provider, name] = words.as_slice()
                && !crate::chanto_cloud::is_login_name(provider) && !name.starts_with('-') {
                    return Outcome::Show(match crate::auth::switch_account(provider, name) {
                        Ok(()) => format!("Using the `{name}` login for {provider} for the rest of this session."),
                        Err(e) => format!("{e:#}"),
                    });
                }
            let r = match words.first().copied().unwrap_or("") {
                "" => return Outcome::Show(format!(
                    "```\n{}\n{}```\n`/login chanto` (chanto cloud's models), `/login chatgpt` and `/login openrouter` sign in through your browser (add `--as <name>` for a second account); for an API key run `chanto login <provider> [--as <name>]` in a terminal or set its variable. `/login <provider> <name>` switches to a saved account for this session.",
                    crate::auth::providers_table(),
                    crate::auth::accounts_table()
                )),
                "chatgpt" => crate::auth::login_chatgpt(account, &|u| crate::mcp_auth::open_browser(u), &say).await,
                "openrouter" => crate::auth::login_openrouter(account, &|u| crate::mcp_auth::open_browser(u), &say).await,
                // Chanto cloud: `/login chanto [github|google|gitlab|bitbucket]`.
                a if crate::chanto_cloud::is_login_name(a.split_whitespace().next().unwrap_or_default()) => {
                    return Outcome::Show(match crate::runs::cloud::login_in_session(a.split_whitespace().nth(1).map(str::to_string)).await {
                        Ok(s) => s,
                        Err(e) => format!("Login failed: {e:#}"),
                    });
                }
                other => return Outcome::Show(format!("To save an API key for `{other}`, run `chanto login {other}` in a terminal (the key isn't shown as you type), or set its environment variable.")),
            };
            let provider = words.first().copied().unwrap_or("");
            if let (Ok(()), Some(a)) = (&r, account) {
                let _ = crate::auth::switch_account(provider, a);
                return Outcome::Show(format!("Signed in as `{a}`, now in use for {provider} this session. Use `{provider}/<model>` (see /models)."));
            }
            Outcome::Show(match r {
                Ok(()) => format!("Signed in. Use `{provider}/<model>`, e.g. `/model {provider}/…` (see /models)."),
                Err(e) => format!("Login failed: {e:#}"),
            })
        }
        "logout" => match crate::auth::remove(arg) {
            Ok(true) => Outcome::Show(format!("Logged out of {arg}.")),
            Ok(false) => Outcome::Show(format!("Not logged in to `{arg}`. Usage: /logout <provider>")),
            Err(e) => Outcome::Show(format!("{e:#}")),
        },
        "sandbox" => {
            use crate::tools::bash::{sandbox_available, sandbox_on, SANDBOX_ACTIVE};
            let cfg = &agent.env.config.sandbox;
            match arg {
                "on" if !sandbox_available() => return Outcome::Show("No sandbox is available on this system (needs working macOS Seatbelt, Linux Landlock or a Windows restricted token).".into()),
                "on" => SANDBOX_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed),
                "off" => SANDBOX_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed),
                "" => {}
                _ => return Outcome::Show("Usage: /sandbox [on|off]".into()),
            }
            let status = if sandbox_on() {
                "**on**"
            } else if sandbox_available() {
                "**off** (`/sandbox on` to enable)"
            } else {
                "**unavailable** on this system"
            };
            let home = dirs::home_dir().unwrap_or_default();
            let mut writable = vec!["the project".to_string(), "temp dirs".into()];
            if !cfg!(windows) { writable.push("tool caches (~/.cargo, ~/.npm, ~/.cache, …)".into()); }
            writable.extend(cfg.allow_write.iter().cloned());
            let denies: Vec<String> = crate::tools::bash::secret_read_denies(&home, &cfg.deny_read)
                .into_iter()
                .map(|(_, p)| p.replace(&home.display().to_string(), "~").replace("^", "").replace("\\", "").replace("$", ""))
                .collect();
            // Windows confines writes only (see windows_sandbox.rs).
            let network = if cfg!(windows) { format!("not restricted on Windows (configured: {})", cfg.network_description()) } else { cfg.network_description() };
            let denies = if cfg!(windows) { vec!["not enforced on Windows".to_string()] } else { denies };
            Outcome::Show(format!(
                "Sandbox for shell commands: {status}\n\n- Can write: {}\n- Network: {}\n- Can't read: {}\n\nA blocked command can be retried outside the sandbox only with your approval. \
                 Change the defaults under `[sandbox]` in config (`enabled`, `allow_write`, `deny_read`, `network`, `allowed_domains`, `denied_domains`).",
                writable.join(", "),
                network,
                denies.join(", ")
            ))
        }
        "output-style" => {
            let env = agent.env.clone();
            if arg.is_empty() {
                let cur = env.output_style.read().unwrap().clone().unwrap_or_else(|| "Default".into());
                let mut lines = vec![format!("Output style: **{cur}**. Switch with `/output-style <name>`:"), "- Default: concise engineering responses".to_string()];
                lines.extend(output_styles.iter().map(|s| format!("- {}: {}", s.name, s.description)));
                return Outcome::Show(lines.join("\n"));
            }
            let pick = if arg.eq_ignore_ascii_case("default") {
                None
            } else {
                match output_styles.iter().find(|s| s.name.eq_ignore_ascii_case(arg)) {
                    Some(s) => Some(s.name.clone()),
                    None => return Outcome::Show(format!("No output style named `{arg}`. Run `/output-style` to list them.")),
                }
            };
            *env.output_style.write().unwrap() = pick.clone();
            agent.refresh_system_prompt();
            Outcome::Show(format!(
                "Output style: **{}** (set `output_style = \"{}\"` in config to keep it)",
                pick.as_deref().unwrap_or("Default"),
                pick.as_deref().unwrap_or("default")
            ))
        }
        "status" => Outcome::Show(status(agent)),
        "context" | "skill-doctor" if cmd == "skill-doctor" || matches!(arg, "skills" | "skill" | "extensions") => {
            Outcome::Show(crate::context_cost::report(&crate::context_cost::costs(&env), env.context_max()))
        }
        "context" => {
            let used = agent.context_used();
            let max = env.context_max();
            let pct = used as f64 * 100.0 / max.max(1) as f64;
            let bar_w = 30;
            let filled = ((pct / 100.0) * bar_w as f64).round() as usize;
            let by_role = |r: crate::llm::Role| agent.messages.iter().filter(|m| m.role == r).map(|m| m.content.len()).sum::<usize>() / 4;
            let parts = [crate::llm::Role::System, crate::llm::Role::User, crate::llm::Role::Assistant, crate::llm::Role::Tool].map(by_role);
            // What the parts don't cover: tool definitions, tool-call arguments, framing.
            let rest = (used as usize).saturating_sub(parts.iter().sum());
            Outcome::Show(format!(
                "## Context\n`{}{}` {:.0}% — {} / {} tokens\n\n- system prompt: ~{} tokens\n- user: ~{} · assistant: ~{} · tool results: ~{}\n- tool definitions, call arguments and framing: ~{}\n- skills, subagents and MCP servers listed: ~{} (`/context skills` breaks it down)\n- messages: {}\n- model: {} · mode: {} · cwd: {}",
                "█".repeat(filled.min(bar_w)),
                "░".repeat(bar_w - filled.min(bar_w)),
                pct,
                used,
                max,
                parts[0],
                parts[1],
                parts[2],
                parts[3],
                rest,
                crate::context_cost::costs(&env).iter().map(|c| c.listing).sum::<u64>(),
                agent.messages.len(),
                env.model(),
                env.mode().label(),
                crate::util::tilde(&env.cwd())
            ))
        }
        "insights" => {
            let findings = crate::insights::analyze(&agent.messages, &agent.stats, env.context_max());
            Outcome::Show(crate::insights::report("this session", &findings, &agent.stats, agent.messages.len()))
        }
        "cost" | "usage" => {
            // `/cost today`, `/cost cheaper`, `/cost limit`: the spend guard's.
            if let Some(out) = crate::spend::command(&env, arg) {
                return Outcome::Show(out);
            }
            // Say which model this was billed to: claiming "local model — no cost"
            // for a hosted one was simply wrong.
            let (prov, _model_name) = env.resolve(&env.model());
            let billing = env.accounting.summary();
            let mut out = format!("Tokens this session: **{}** in · **{}** out · {billing}", agent.total_in, agent.total_out);
            for (name, spent, limit) in env.accounting.budgets() {
                out.push_str(&format!("\n- {name} budget: {} of {} ({:.0}%)", crate::util::usd(spent), crate::util::usd(limit), spent * 100.0 / limit.max(1e-9)));
            }
            let reprice = |m: &str, u: &crate::llm::Usage| crate::spend::pricer(&env, m)(u);
            let cheaper = |m: &str| crate::spend::cheaper(&env, m);
            out.push_str("\n\n");
            out.push_str(&env.spend.report(env.context_max(), &reprice, &cheaper));
            out.push_str("\n_`/cost today` adds up every session today; `/cost cheaper` and `/cost limit` act on the suggestions._");
            // A credit balance is only meaningful for providers that publish one.
            if let Some(b) = prov.balance().await {
                out.push_str(&format!("\n\n**Balance:** {b}"));
            }
            // Account limits come from the provider (ChatGPT plans report them on
            // each call); say so plainly when nothing has been reported yet.
            match env.rate_limits.lock().unwrap().clone() {
                Some(l) => out.push_str(&format!("\n\n## Account limits\n{}\n\n_Reported by the provider; resets depend on your plan._", l.detail(chrono::Utc::now().timestamp()))),
                None => out.push_str("\n\n_No account limits reported by this provider._"),
            }
            Outcome::Show(out)
        }
        "resume" | "sessions" => {
            let cwd = env.cwd();
            let (verb, rest) = arg.split_once(char::is_whitespace).map(|(v, r)| (v, r.trim())).unwrap_or((arg, ""));
            if matches!(verb, "delete" | "rm" | "archive" | "unarchive") {
                return crate::session_cmds::manage(agent, if verb == "rm" { "delete" } else { verb }, rest).await;
            }
            let (all, query) = match verb {
                "--all" | "-a" => (true, rest),
                "--search" | "-s" => (false, rest),
                _ => (false, ""),
            };
            if arg.is_empty() || verb.starts_with('-') {
                let list = Session::browse(&cwd, query, all);
                if list.is_empty() {
                    return Outcome::Show(if query.is_empty() { "No saved sessions for this directory.".into() } else { format!("No saved sessions match `{query}`.") });
                }
                let mut s = String::from("## Sessions (resume with `/resume <id>`; `/sessions delete|archive <id>`)\n");
                for l in &list {
                    let x = &l.session;
                    s.push_str(&format!("{}- `{}` {} — {}{}\n", "  ".repeat(l.depth), x.id, x.updated.get(..16).unwrap_or(""), x.title, if x.archived { " _(archived)_" } else { "" }));
                }
                return Outcome::Show(s);
            }
            match adopt_session(agent, &env, arg).await {
                Ok(title) => Outcome::Reset(Some(format!("Resumed session: {title}"))),
                Err(e) => Outcome::Show(format!("Could not resume: {e}")),
            }
        }
        "rename" | "title" => {
            let Some(sess) = agent.session.as_mut() else {
                return Outcome::Show("This session isn't saved, so it has no title.".into());
            };
            // Listed in plain text, so masked like an automatic title.
            let title = crate::util::one_line(&crate::secrets::redact_for_export(arg), 80);
            if title.is_empty() {
                return Outcome::Show(format!("Session title: **{}**. Usage: `/rename <title>`", if sess.title.is_empty() { "(untitled)" } else { sess.title.as_str() }));
            }
            sess.title = title.clone();
            agent.save_session();
            Outcome::Show(format!("Session renamed to **{title}**"))
        }
        // The TUI and the plain REPL run loops themselves; nothing else has a clock to run them on.
        "loop" => Outcome::Show("`/loop` runs in the interactive TUI (`chanto`) or the `--plain` REPL.".into()),
        "plan" => crate::session_cmds::plan(agent, arg),
        "cd" => crate::session_cmds::cd(agent, arg).await,
        "autocompact" => crate::session_cmds::autocompact(&env, arg),
        "background" => crate::session_cmds::background(agent, arg).await,
        "recap" => match crate::session_cmds::recap(&env, &agent.messages).await {
            Ok(r) => Outcome::Show(format!("※ {r}")),
            Err(e) => Outcome::Show(format!("No recap: {e:#}")),
        },
        "redo" => match agent.redo() {
            Ok((n, prompt)) => {
                agent.save_session();
                Outcome::Reset(Some(format!("⏩ Redone from \"{}\" — {n} file(s) restored", crate::util::one_line(&prompt, 60))))
            }
            Err(e) => Outcome::Show(format!("{e}")),
        },
        "rewind" | "undo" => {
            if cmd == "rewind" && arg == "undo" {
                let tree = env.rewind_undo.lock().unwrap().clone();
                if env.interactive && let Some(tree) = tree {
                    let summary = match env.shadow().and_then(|s| s.restore_summary(&tree)) {
                        Ok(summary) => summary,
                        Err(e) => return Outcome::Show(format!("Cannot preview undo: {e}")),
                    };
                    if !env.frontend.choose(&format!("{summary}\nUndo restore?"), &["yes".into(), "no".into()]).await
                        .is_some_and(|s| s.eq_ignore_ascii_case("yes")) {
                        return Outcome::Show("Undo cancelled.".into());
                    }
                }
                return match agent.rewind_undo() {
                    Ok(n) => { agent.save_session(); Outcome::Show(format!("Restored the state before the last code restore — {n} file(s) restored")) }
                    Err(e) => Outcome::Show(format!("Undo failed: {e}")),
                };
            }
            let cps: Vec<String> = env.checkpoints.lock().unwrap().iter().map(|c| c.prompt.clone()).collect();
            if cps.is_empty() {
                // A fresh session has no checkpoints, but if it was just cleared,
                // `/rewind` undoes that by resuming the conversation it replaced.
                if let Some(id) = agent.cleared_session.take() {
                    return match adopt_session(agent, &env, &id).await {
                        Ok(title) => Outcome::Reset(Some(format!("↩ Restored the conversation you cleared: {title}"))),
                        Err(_) => Outcome::Show("Nothing to rewind (the cleared conversation is no longer available).".into()),
                    };
                }
                return Outcome::Show("Nothing to rewind.".into());
            }
            let idx = if arg.is_empty() {
                if cmd == "undo" { cps.len().saturating_sub(1) } else {
                    let mut s = String::from("## Checkpoints (rewind with `/rewind <n>`)\n");
                    for (i, p) in cps.iter().enumerate() {
                        s.push_str(&format!("{}. {}\n", i, crate::util::one_line(p, 80)));
                    }
                    return Outcome::Show(s);
                }
            } else {
                match arg.split_whitespace().next().unwrap_or("").parse::<usize>() {
                    Ok(i) => i,
                    Err(_) => return Outcome::Show("Usage: /rewind <n>".into()),
                }
            };
            let mode = arg.split_whitespace().nth(1).unwrap_or("both");
            if !matches!(mode, "both" | "conversation" | "code") { return Outcome::Show("Usage: /rewind <n> [both|conversation|code]".into()); }
            if mode != "conversation" && env.interactive {
                let tree = env.checkpoints.lock().unwrap().get(idx).and_then(|c| c.tree.clone());
                if let Some(tree) = tree {
                    let summary = match env.shadow().and_then(|s| s.restore_summary(&tree)) {
                        Ok(summary) => summary,
                        Err(e) => return Outcome::Show(format!("Cannot preview restore: {e}")),
                    };
                    if !env.frontend.choose(&format!("{summary}\nRestore?"), &["yes".into(), "no".into()]).await
                        .is_some_and(|s| s.eq_ignore_ascii_case("yes")) {
                        return Outcome::Show("Restore cancelled.".into());
                    }
                }
            }
            match agent.rewind_mode(idx, mode) {
                Ok((n, prompt)) => {
                    agent.save_session();
                    let undo = if mode != "conversation" && env.rewind_undo.lock().unwrap().is_some() { "; previous code state saved. Use /rewind undo to restore it." } else { "" };
                    Outcome::Reset(Some(format!("⏪ Rewound to before \"{}\" — {n} file(s) restored{undo}", crate::util::one_line(&prompt, 60))))
                }
                Err(e) => Outcome::Show(format!("Rewind failed: {e}")),
            }
        }
        "init" => Outcome::Prompt(crate::context::INIT_PROMPT.into()),
        // `/review` is the bundled `/code-review`'s alias: it keeps running the
        // bundled review even when a skill named `code-review` replaces it.
        "review" => crate::bundled::code_review(agent, arg).await,
        "statusline" => crate::bundled::statusline(&env, arg),
        "security-review" => Outcome::Prompt(format!(
            "Do a security review of the pending changes on this branch{}. Use `git status`, `git diff` and `git diff --staged` \
             (and `git log`/`git diff <base>...HEAD` for committed work not yet merged) to see them, and read surrounding code as needed. \
             Look for vulnerabilities the changes introduce: injection (SQL, command, path traversal, template), authentication and \
             authorization gaps, secrets or credentials in code or logs, unsafe deserialization, SSRF, XSS/CSRF, weak cryptography, \
             race conditions, and unvalidated input reaching dangerous sinks. Only report issues you are confident are real and \
             exploitable, each with severity, file:line, an exploit scenario and a concrete fix; say so plainly if there are none. \
             Do not modify files.",
            if arg.is_empty() { String::new() } else { format!(" ({arg})") }
        )),
        "pr-comments" => Outcome::Prompt(format!(
            "Show the review comments on {}. Use the gh CLI: `gh pr view --json number,url,title` to find the PR, then \
             `gh api repos/{{owner}}/{{repo}}/pulls/<number>/comments` for line comments and `gh api repos/{{owner}}/{{repo}}/issues/<number>/comments` \
             for discussion. Group them by file with file:line, author and the comment text, note which look unresolved, \
             and summarize what is being asked. Do not change any code.",
            if arg.is_empty() { "the current branch's pull request".to_string() } else { format!("pull request {arg}") }
        )),
        "memory" | "remember" => {
            add_memory(agent, arg)
        }
        "allow" | "deny" | "ask" => {
            if arg.is_empty() {
                return Outcome::Show(format!("Usage: /{cmd} <rule>, e.g. Bash(npm test:*)"));
            }
            let mut r = env.rules.write().unwrap();
            match cmd {
                "allow" => r.allow.push(Rule::parse(arg)),
                "ask" => r.ask.push(Rule::parse(arg)),
                _ => r.deny.push(Rule::parse(arg)),
            }
            Outcome::Show(format!("Added {cmd} rule `{arg}` for this session"))
        }
        "edit-prompt" => {
            // `/edit-prompt <n> [both|conversation] [new prompt]`
            let mut parts = arg.splitn(2, char::is_whitespace);
            let Some(idx) = parts.next().and_then(|n| n.parse::<usize>().ok()) else {
                return Outcome::Show("Usage: `/edit-prompt <checkpoint> [both|conversation] [new prompt]`: branch the session before that prompt (numbers as `/rewind` lists them) and send the new one. `both` restores the files too (the default), `conversation` leaves them.".into());
            };
            let rest = parts.next().unwrap_or("").trim_start();
            let (code, text) = match rest.split_once(char::is_whitespace).map(|(m, t)| (m, t.trim())).unwrap_or((rest, "")) {
                ("conversation", t) => (false, t),
                ("both" | "code", t) => (true, t),
                _ => (true, rest.trim()),
            };
            match agent.branch_at(idx, code) {
                Ok((id, original, files)) => {
                    let parent = agent.session.as_ref().and_then(|s| s.parent_id.clone()).unwrap_or_default();
                    let files = if code { format!(", {files} file(s) restored") } else { String::new() };
                    let note = format!("Branched before \"{}\" into session {id}{files}. The original conversation is kept: `/resume {parent}`.", crate::util::one_line(&original, 60));
                    if text.is_empty() { Outcome::Reset(Some(note)) } else {
                        env.emit(Event::Info(note));
                        Outcome::Prompt(text.to_string())
                    }
                }
                Err(e) => Outcome::Show(format!("Couldn't branch: {e}")),
            }
        }
        "fork" => {
            let (name, point) = match arg.split_once("--at ") {
                Some((name, n)) => match n.trim().parse::<usize>() {
                    Ok(n) => (name.trim(), Some(n)),
                    Err(_) => return Outcome::Show("Usage: /fork [name] --at <checkpoint index>".into()),
                },
                None => (if arg == "--at-current" { "" } else { arg }, None),
            };
            match agent.fork_session(Some(name), point) {
                Ok(id) => Outcome::Reset(Some(format!("Forked session: {id}"))),
                Err(e) => Outcome::Show(format!("Fork failed: {e}")),
            }
        }
        "share" => {
            if !arg.is_empty() && arg != "--no-tool-output" { return Outcome::Show("Usage: /share [--no-tool-output]".into()); }
            agent.save_session();
            match agent.session.as_ref().ok_or_else(|| anyhow::anyhow!("no current session"))
                .and_then(|s| crate::share::export(s, &agent.messages, arg == "--no-tool-output")) {
                Ok(path) => Outcome::Show(format!("Shared to `{}`", path.display())),
                Err(e) => Outcome::Show(format!("Share failed: {e}")),
            }
        }
        "export" => {
            let md = export_markdown(agent);
            let path = if arg.is_empty() { format!("chanto-conversation-{}.md", chrono::Local::now().format("%Y%m%d-%H%M%S")) } else { arg.to_string() };
            let full = crate::util::resolve(&env.cwd(), &path);
            match crate::util::write_private(&full, md.as_bytes()) {
                Ok(()) => Outcome::Show(format!("Exported to `{}`", full.display())),
                Err(e) => Outcome::Show(format!("Export failed: {e}")),
            }
        }
        // The TUI handles `/transcript` itself (it shows the hidden steps in place).
        "transcript" => Outcome::Show(last_turn_steps(agent)),
        "copy" => {
            let last = agent.messages.iter().rev().find(|m| m.role == crate::llm::Role::Assistant && !m.content.trim().is_empty()).map(|m| m.content.clone());
            match last {
                Some(t) => match copy_to_clipboard(&t) {
                    Ok(()) => Outcome::Show("Copied last response to clipboard.".into()),
                    Err(e) => Outcome::Show(format!("Copy failed: {e}")),
                },
                None => Outcome::Show("Nothing to copy.".into()),
            }
        }
        "doctor" => {
            let r = crate::doctor::report(&env.config, Some(&env.model())).await;
            // A code block keeps the report's columns and indentation in the TUI.
            Outcome::Show(format!("```text\n{}\nMCP connected: {}/{}\nSkills: {} · custom commands: {} · subagents: {}\nContext: {} · vision: {}\n```",
                crate::doctor::render(&r), env.mcp.len(), env.config.mcp_servers.len(), skills.len(), commands.len(), agents.len(), env.context_max(), env.vision()))
        }
        "config" if !arg.is_empty() => {
            // `/config <key> <value>` sets a session setting through its own command.
            const KEYS: [&str; 8] = ["model", "mode", "theme", "effort", "think", "vision", "sandbox", "output-style"];
            let (key, value) = arg.split_once(char::is_whitespace).map(|(k, v)| (k, v.trim())).unwrap_or((arg, ""));
            let key = key.replace('_', "-");
            if !KEYS.contains(&key.as_str()) {
                return Outcome::Show(format!("No session setting `{key}`. Settable: {}. Other settings go in `~/.chanto/config.toml`.", KEYS.map(|k| format!("`{k}`")).join(", ")));
            }
            if value.is_empty() {
                return Outcome::Show(format!("Usage: `/config {key} <value>`"));
            }
            Box::pin(handle(agent, &format!("/{key} {value}"))).await
        }
        "config" => {
            let cwd = env.cwd();
            let home = crate::compat::home();
            let codex = crate::compat::codex_dir();
            let candidates = [
                (env.config.compat.codex, codex.join("config.toml")),
                (env.config.compat.claude, home.join(".claude.json")),
                (env.config.compat.claude, home.join(".claude/settings.json")),
                (true, crate::config::home_dir().join("config.toml")),
                (env.config.compat.codex, cwd.join(".codex/config.toml")),
                (env.config.compat.claude, cwd.join(".claude/settings.json")),
                (env.config.compat.claude, cwd.join(".claude/settings.local.json")),
                (true, cwd.join(".chanto/config.toml")),
                (true, cwd.join(".chanto/config.local.toml")),
                (true, cwd.join(".mcp.json")),
            ];
            let mut s = String::from("## Config files (lowest → highest precedence)\n");
            let mut any = false;
            for (on, p) in candidates {
                if p.exists() {
                    any = true;
                    s.push_str(&format!("- {} `{}`\n", if on { "✓" } else { "✗ (compat off)" }, crate::util::tilde(&p)));
                }
            }
            if !any {
                s.push_str("- none yet: built-in defaults (`chanto setup` writes `~/.chanto/config.toml`)\n");
            }
            let instr = crate::context::instruction_files(&cwd);
            if !instr.is_empty() {
                s.push_str("\n## Instruction files\n");
                for (p, _) in instr {
                    s.push_str(&format!("- `{}`\n", p.display()));
                }
            }
            let scoped = crate::context::scoped_foreign_rules(&cwd);
            if !scoped.is_empty() {
                s.push_str("\n## Path-scoped rules (Cursor, Copilot; loaded when a matching file is touched)\n");
                for (p, globs) in scoped {
                    s.push_str(&format!("- `{}`: {}\n", p.display(), globs.join(", ")));
                }
            }
            s.push_str(&format!(
                "\n- provider: {} at `{}`\n- sessions: `{}`\n\nSet a session setting with `/config <key> <value>` (model, mode, theme, effort, …). Example config file: `chanto --example-config`",
                env.config.provider.label(),
                env.provider().base_url,
                crate::util::tilde(&crate::config::home_dir().join("sessions"))
            ));
            Outcome::Show(s)
        }
        // Vim mode is a TUI editor state, so the TUI intercepts `/vim` before it
        // reaches here; this is the fallback for the plain REPL.
        "vim" => Outcome::Show("Vim keybindings apply to the interactive TUI (`chanto`), not the plain REPL.".into()),
        // Like `/vim`, the TUI handles `/voice` itself.
        "voice" => Outcome::Show("Voice input works in the interactive TUI (ctrl+space or `/voice`) and the web UI's mic button.".into()),
        "terminal-setup" => terminal_setup(),
        "bug" | "feedback" => crate::feedback::command(&env, arg).await,
        "privacy-settings" => Outcome::Show(privacy_settings(&env)),
        "diagnostics" => {
            let args: Vec<String> = arg.split_whitespace().map(str::to_string).collect();
            match crate::feedback::diagnostics_command(&args) {
                Ok(s) => Outcome::Show(s),
                Err(e) => Outcome::Show(format!("{e:#}")),
            }
        }
        "release-notes" => Outcome::Show(release_notes()),
        "diff" => {
            let cwd = env.cwd();
            let git = |args: &[&str]| {
                std::process::Command::new("git").args(args).current_dir(&cwd).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            };
            let Some(diff) = git(&["diff", "HEAD"]).or_else(|| git(&["diff"])) else {
                return Outcome::Show("Not a git repository.".into());
            };
            let untracked = git(&["ls-files", "--others", "--exclude-standard"]).unwrap_or_default();
            if diff.trim().is_empty() && untracked.trim().is_empty() {
                return Outcome::Show("No changes.".into());
            }
            let mut s = String::new();
            if let Some(stat) = git(&["diff", "HEAD", "--stat"]) {
                s.push_str(&format!("```\n{}```\n", stat));
            }
            if !untracked.trim().is_empty() {
                s.push_str(&format!("\nUntracked:\n```\n{untracked}```\n"));
            }
            if !diff.trim().is_empty() {
                s.push_str(&format!("\n```diff\n{}\n```", crate::util::truncate(&diff, 60_000)));
            }
            Outcome::Show(s)
        }
        _ => {
            if let Some(r) = crate::mcp::run_prompt_command(&env.mcp, cmd, arg).await {
                return match r {
                    Ok(text) => match crate::hooks::user_prompt_expansion(&env, "mcp_prompt", cmd, arg, "mcp").await {
                        Ok(()) => Outcome::Prompt(text),
                        Err(why) => Outcome::Show(format!("/{cmd} was blocked by a UserPromptExpansion hook: {why}")),
                    },
                    Err(e) => Outcome::Show(format!("/{cmd}: {e}")),
                };
            }
            // Codex invokes prompts as /prompts:<name>.
            let name = cmd.strip_prefix("prompts:").unwrap_or(cmd);
            // Skills and commands share `/name`; a skill wins a clash. Plugin
            // and subfolder names are `/ns:name`; the bare
            // `/name` works when unambiguous.
            let mut skill = skills.iter().find(|s| s.user_invocable && s.name == name);
            let mut found = commands.iter().find(|c| c.name == name);
            if skill.is_none() && found.is_none() {
                // Bundled skills give way only to a skill or command of exactly their name.
                if let Some(o) = crate::bundled::run(agent, name, arg).await {
                    return o;
                }
                let suffix = format!(":{name}");
                let sk: Vec<_> = skills.iter().filter(|s| s.user_invocable && s.name.ends_with(&suffix)).collect();
                let cm: Vec<_> = commands.iter().filter(|c| c.name.ends_with(&suffix)).collect();
                if sk.len() + cm.len() == 1 {
                    skill = sk.first().copied();
                    found = cm.first().copied();
                }
            }
            if let Some((name, project)) = skill.map(|s| (&s.name, s.project)).or(found.map(|c| (&c.name, c.project))) {
                let source = if project { "project" } else { "user" };
                if let Err(why) = crate::hooks::user_prompt_expansion(&env, "slash_command", name, arg, source).await {
                    return Outcome::Show(format!("/{name} was blocked by a UserPromptExpansion hook: {why}"));
                }
            }
            if let Some(sk) = skill {
                let body = match render_skill(&env, sk, arg).await {
                    Ok(b) => b,
                    Err(e) => return Outcome::Show(format!("/{name}: {e}")),
                };
                activate_skill(&env, &sk.name, &sk.meta, sk.project, true, sk.fork.is_some());
                let Some(kind) = &sk.fork else { return Outcome::Prompt(body) };
                // `context: fork`: the skill runs in a subagent; its report comes back here.
                let ctx = crate::tools::Ctx { env: env.clone(), cancel: CancellationToken::new(), depth: agent.depth, call_id: None, cwd_override: agent.cwd_override.clone(), reads: None };
                let background = sk.meta.background.unwrap_or(true) && env.interactive;
                return match run_forked_skill(&ctx, sk, kind, &body, format!("/{}", sk.name)).await {
                    Ok(out) if !out.is_error && background => Outcome::Show(format!("/{} is running in a background {kind} subagent; its report arrives when it finishes.", sk.name)),
                    Ok(out) if !out.is_error => Outcome::Prompt(format!("The /{} skill ran in a {kind} subagent. Its report:\n\n{}", sk.name, out.content)),
                    Ok(out) => Outcome::Show(format!("/{name}: {}", out.content)),
                    Err(e) => Outcome::Show(format!("/{name}: {e:#}")),
                };
            }
            if let Some(c) = found {
                activate_skill(&env, &c.name, &c.meta, c.project, true, false);
                // The !`cmd` blocks run before the arguments go in, so an
                // argument can't add a command (a block reads them from $ARGUMENTS).
                let body = inline_bash(&env, c.body.clone(), c.project, name, c.meta.shell.as_deref(), arg, true).await;
                return Outcome::Prompt(expand_args(&body, arg));
            }
            // A command the external agent advertised runs there, with its
            // arguments, as an ordinary prompt.
            if agent.acp.as_ref().is_some_and(|acp| acp.has_command(cmd)) {
                return Outcome::Prompt(line.to_string());
            }
            let names = BUILTIN.iter().chain(crate::bundled::BUNDLED).map(|c| c.name.to_string()).chain(commands.iter().map(|c| c.name.clone())).chain(skills.iter().map(|s| s.name.clone()));
            match crate::util::closest(cmd, names) {
                Some(guess) => env.emit(Event::Warn(format!("Unknown command /{cmd}. Did you mean /{guess}?"))),
                None => env.emit(Event::Warn(format!("Unknown command /{cmd} — try /help"))),
            }
            Outcome::Nothing
        }
    }
}

/// What invoking a skill or command changes for the rest of its turn
/// (Claude Code's `allowed-tools`, `model` and `effort`). Cleared when the
/// user sends their next message.
#[derive(Debug, Default)]
pub struct SkillTurn {
    /// Permission rules pre-approved for the turn.
    pub allow: Vec<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Set by a typed `/name`: the turn it applies to is the one about to start.
    armed: bool,
}

impl SkillTurn {
    /// A user turn starts: keep what a typed `/name` just set, drop the rest.
    pub fn begin_turn(&mut self) {
        if self.armed {
            self.armed = false;
        } else {
            *self = SkillTurn::default();
        }
    }
}

/// Apply a skill's or command's frontmatter as it's invoked: its hooks for
/// the rest of the session (a project one's only in a trusted folder) and,
/// unless it runs forked (where they go to the subagent), its `allowed-tools`,
/// `model` and `effort` for the rest of the turn. `typed`: invoked as `/name`,
/// so the turn is the one about to start.
pub fn activate_skill(
    env: &Env,
    name: &str,
    meta: &crate::context::SkillMeta,
    project: bool,
    typed: bool,
    forked: bool,
) {
    if let Some(h) = &meta.hooks {
        if project && !crate::config::is_trusted(&env.cwd()) {
            env.emit(Event::Warn(format!("/{name} is defined by this project, which isn't trusted; its hooks were not registered (chanto --trust to allow).")));
        } else {
            crate::hooks::add_session_hooks(env, h);
        }
    }
    // Pre-approved tools skip the permission checks, so a project's skills
    // (which arrive with a cloned repository) only grant them once the
    // folder is trusted, like their hooks.
    let grants = if project
        && !meta.allowed_tools.is_empty()
        && !crate::config::is_trusted(&env.cwd())
    {
        env.emit(Event::Warn(format!("/{name} is defined by this project, which isn't trusted; its allowed-tools were not pre-approved (chanto --trust to allow).")));
        &[][..]
    } else {
        &meta.allowed_tools[..]
    };
    let mut turn = env.skill_turn.lock().unwrap();
    for r in grants {
        if !turn.allow.contains(r) {
            turn.allow.push(r.clone());
        }
    }
    if !forked {
        if let Some(m) = &meta.model {
            turn.model = Some(crate::agent::subagent_model(env, &env.model(), m));
        }
        match &meta.effort {
            Some(e) if crate::llm::EFFORTS.contains(&e.as_str()) => turn.effort = Some(e.clone()),
            Some(e) => env.emit(Event::Warn(format!(
                "/{name}: ignoring effort `{e}`: use one of {}.",
                crate::llm::EFFORTS.join(", ")
            ))),
            None => {}
        }
    }
    turn.armed |= typed;
}

/// Run a forked skill (`context: fork`) in a subagent of type `kind`: in the
/// background unless it sets `background: false` or nobody is there to wait
/// for (non-interactive runs).
pub async fn run_forked_skill(
    ctx: &crate::tools::Ctx,
    sk: &crate::context::Skill,
    kind: &str,
    body: &str,
    description: String,
) -> anyhow::Result<crate::tools::ToolOutput> {
    let background = sk.meta.background.unwrap_or(true) && ctx.env.interactive;
    let opts = crate::agent::SubagentOpts {
        description,
        background,
        model: sk.meta.model.clone(),
        effort: sk.meta.effort.clone(),
        ..Default::default()
    };
    crate::agent::run_subagent(ctx, kind, body, opts).await
}

/// Fill a command template: `$ARGUMENTS` (all), `$1`..`$9` (positional) and
/// `$NAME` for `NAME=value` arguments (Codex prompts). Without placeholders
/// the arguments are appended.
pub fn expand_args(body: &str, arg: &str) -> String {
    let words = shell_words::split(arg)
        .unwrap_or_else(|_| arg.split_whitespace().map(str::to_string).collect());
    let mut out = body.replace("$ARGUMENTS", arg);
    let mut used = out != body;
    for (i, w) in words.iter().enumerate().take(9).rev() {
        let ph = format!("${}", i + 1);
        if out.contains(&ph) {
            out = out.replace(&ph, w);
            used = true;
        }
    }
    for w in &words {
        if let Some((k, v)) = w.split_once('=') {
            let ph = format!("${k}");
            if !k.is_empty()
                && k.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                && out.contains(&ph)
            {
                out = out.replace(&ph, v);
                used = true;
            }
        }
    }
    if !used && !arg.is_empty() {
        out = format!("{out}\n\n{arg}");
    }
    out
}

/// Fill a skill's `$ARGUMENTS`, `$ARGUMENTS[N]` and `$N`. Unlike command
/// templates, `N` counts from 0 (`$0` is the first argument), as in Claude
/// Code skills. Without placeholders the arguments are appended.
pub fn expand_skill_args(body: &str, arg: &str) -> String {
    let words = shell_words::split(arg)
        .unwrap_or_else(|_| arg.split_whitespace().map(str::to_string).collect());
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\$ARGUMENTS\[(\d+)\]|\$ARGUMENTS\b|\$(\d)\b").unwrap()
    });
    let mut used = false;
    let out = RE
        .replace_all(body, |c: &regex::Captures| {
            used = true;
            match c.get(1).or(c.get(2)) {
                Some(n) => n
                    .as_str()
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| words.get(i))
                    .cloned()
                    .unwrap_or_default(),
                None => arg.to_string(),
            }
        })
        .into_owned();
    if used || arg.is_empty() {
        out
    } else {
        format!("{out}\n\nARGUMENTS: {arg}")
    }
}

/// A skill's instructions, ready to run: !`cmd` blocks run (project skills
/// only in a trusted folder), then `${CLAUDE_SKILL_DIR}` and the arguments
/// filled in. Arguments go in last so they can't add commands.
pub async fn render_skill(
    env: &Env,
    sk: &crate::context::Skill,
    arg: &str,
) -> std::io::Result<String> {
    let text = std::fs::read_to_string(&sk.path)?;
    let dir = sk
        .path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let body = inline_bash(
        env,
        crate::context::strip_frontmatter(&text).to_string(),
        sk.project,
        &sk.name,
        sk.meta.shell.as_deref(),
        arg,
        false,
    )
    .await;
    let body = expand_skill_args(&body.replace("${CLAUDE_SKILL_DIR}", &dir), arg);
    Ok(format!("Base directory for this skill: {dir}\n\n{body}"))
}

/// Run a template's !`cmd` blocks; a project-defined one's only in a trusted
/// folder. `args` reach the blocks as data, never as code: see
/// [`expand_inline_bash`]. `positional`: a command file's `$1`.. and `$NAME`.
async fn inline_bash(
    env: &Env,
    body: String,
    project: bool,
    name: &str,
    shell: Option<&str>,
    args: &str,
    positional: bool,
) -> String {
    if !body.contains("!`") {
        return body;
    }
    if project && !crate::config::is_trusted(&env.cwd()) {
        env.emit(Event::Warn(format!("/{name} is defined by this project, which isn't trusted; its !`command` blocks were not run (chanto --trust to allow).")));
        return body;
    }
    expand_inline_bash(&body, &env.cwd(), shell, args, positional).await
}

/// The process for one !`cmd` block: the session's shell, or PowerShell for
/// `shell: powershell`.
fn inline_command(cmd: &str, shell: Option<&str>) -> tokio::process::Command {
    if shell.is_some_and(|s| s.eq_ignore_ascii_case("powershell")) {
        let exe = if cfg!(windows) || !crate::shell::command_exists("pwsh") {
            "powershell"
        } else {
            "pwsh"
        };
        let mut c = tokio::process::Command::new(exe);
        c.args(["-NoProfile", "-NonInteractive", "-Command", cmd]);
        return c;
    }
    crate::shell::command(cmd)
}

/// Replace !`cmd` occurrences (at the start of a line or after whitespace)
/// in command templates with the command's output. The invocation's
/// arguments are in the `ARGUMENTS` environment variable, so the shell never
/// parses them as code. With `positional` (command files) a POSIX shell also
/// gets the words as `$1`, `$2`.., and each `NAME=value` argument whose
/// `$NAME` a block uses becomes that environment variable, unless the
/// environment already has one (so `PATH=` can't redirect the command).
async fn expand_inline_bash(
    body: &str,
    cwd: &std::path::Path,
    shell: Option<&str>,
    args: &str,
    positional: bool,
) -> String {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(^|\s)!`([^`]+)`").unwrap());
    let words = if positional {
        shell_words::split(args)
            .unwrap_or_else(|_| args.split_whitespace().map(str::to_string).collect())
    } else {
        Vec::new()
    };
    let posix = !shell.is_some_and(|s| s.eq_ignore_ascii_case("powershell"))
        && crate::shell::flavor() == crate::shell::Flavor::Posix;
    let mut out = String::new();
    let mut last = 0;
    for cap in RE.captures_iter(body) {
        let m = cap.get(0).unwrap();
        out.push_str(&body[last..m.start()]);
        out.push_str(&cap[1]);
        let cmd = &cap[2];
        let mut c = inline_command(cmd, shell);
        c.current_dir(cwd).env("ARGUMENTS", args);
        if positional && posix {
            // `sh -c script $0 $1 ...`
            c.arg("chanto").args(&words);
        }
        for w in &words {
            if let Some((k, v)) = w.split_once('=')
                && !k.is_empty()
                && k.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                && (cmd.contains(&format!("${k}")) || cmd.contains(&format!("${{{k}}}")))
                && std::env::var_os(k).is_none()
            {
                c.env(k, v);
            }
        }
        let res = c.output().await;
        let text = match res {
            Ok(o) => format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
            Err(e) => format!("(failed to run `{cmd}`: {e})"),
        };
        out.push_str(crate::util::truncate(text.trim_end(), 20_000).as_str());
        last = m.end();
    }
    out.push_str(&body[last..]);
    out
}

pub fn add_memory(agent: &Agent, note: &str) -> Outcome {
    let Some(note) = note.strip_prefix("--agents ") else {
        return Outcome::Show(
            match crate::memory::command(&agent.env.cwd(), note, &agent.env.config.memory.scope) {
                Ok(s) => s,
                Err(e) => format!("Memory: {e}"),
            },
        );
    };
    if crate::secrets::contains_secret(note) {
        return Outcome::Show("Refusing to store a secret".into());
    }
    let p = agent.env.cwd().join("AGENTS.md");
    let mut text = std::fs::read_to_string(&p).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("- {}\n", note.trim()));
    match std::fs::write(&p, text) {
        Ok(()) => Outcome::Show(format!(
            "Saved to `{}` (applies from the next session or /clear)",
            p.display()
        )),
        Err(e) => Outcome::Show(format!("Could not write AGENTS.md: {e}")),
    }
}

pub fn copy_to_clipboard(text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    // Try each tool in turn: a tool that exists but fails (no display, broken
    // pbcopy, ...) must not stop the ones after it.
    for (cmd, args) in [
        ("pbcopy", vec![]),
        ("wl-copy", vec![]),
        ("xclip", vec!["-selection", "clipboard"]),
        ("clip", vec![]),
    ] {
        let Ok(mut c) = std::process::Command::new(cmd)
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .spawn()
        else {
            continue;
        };
        let wrote = c
            .stdin
            .take()
            .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
        if wrote && c.wait().is_ok() {
            return Ok(());
        }
    }
    // OSC 52 fallback. Must be flushed: stdout is line-buffered and this escape
    // has no newline, so without it the sequence would sit in the buffer and be
    // emitted later, tangled up with the next frame.
    use base64::Engine;
    let mut out = std::io::stdout();
    out.write_all(
        format!(
            "\x1b]52;c;{}\x07",
            base64::engine::general_purpose::STANDARD.encode(text)
        )
        .as_bytes(),
    )?;
    out.flush()?;
    Ok(())
}

pub fn export_markdown(agent: &Agent) -> String {
    use crate::llm::Role;
    let mut s = String::from("# Conversation\n\n");
    for m in agent.messages.iter().skip(1) {
        match m.role {
            Role::User => s.push_str(&format!(
                "## User\n\n{}\n\n",
                m.content
                    .split("<system-reminder>")
                    .next()
                    .unwrap_or("")
                    .trim()
            )),
            Role::Assistant => {
                if !m.content.trim().is_empty() {
                    s.push_str(&format!("## Assistant\n\n{}\n\n", m.content.trim()));
                }
                for c in &m.tool_calls {
                    s.push_str(&format!(
                        "**Tool call:** `{}` `{}`\n\n",
                        c.name,
                        crate::util::one_line(&c.arguments.to_string(), 200)
                    ));
                }
            }
            Role::Tool => s.push_str(&format!(
                "<details><summary>Result</summary>\n\n```\n{}\n```\n</details>\n\n",
                crate::util::truncate(&m.content, 3000)
            )),
            Role::System => {}
        }
    }
    // An export goes to disk (and often into a repo or a ticket), so mask
    // secrets exactly as the model and the session log see them.
    crate::secrets::redact(&s).into_owned()
}

/// `/terminal-setup`: make shift+enter insert a newline instead of submitting.
/// Terminals send the same byte for enter and shift+enter unless the emulator
/// is told otherwise, so this either writes the VS Code keybinding or prints
/// the manual steps for the terminal in use.
fn terminal_setup() -> Outcome {
    let term = std::env::var("TERM_PROGRAM").unwrap_or_default();
    if term == "vscode" {
        let path =
            crate::compat::home().join("Library/Application Support/Code/User/keybindings.json");
        let entry = serde_json::json!({
            "key": "shift+enter",
            "command": "workbench.action.terminal.sendSequence",
            "args": { "text": "\n" },
            "when": "terminalFocus"
        });
        let mut arr: Vec<serde_json::Value> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&strip_json_comments(&s)).ok())
            .unwrap_or_default();
        if arr.iter().any(|e| e["key"] == "shift+enter") {
            return Outcome::Show("shift+enter is already bound in VS Code.".into());
        }
        arr.push(entry);
        let write = std::fs::create_dir_all(path.parent().unwrap()).and_then(|_| {
            std::fs::write(
                &path,
                serde_json::to_string_pretty(&arr).unwrap_or_default(),
            )
        });
        return Outcome::Show(match write {
            Ok(()) => format!(
                "Added the shift+enter binding to `{}`.\n\nReload the window for it to take effect.",
                path.display()
            ),
            Err(e) => format!(
                "Could not write `{}`: {e}\n\nAdd this to your keybindings.json manually:\n```json\n{{\"key\":\"shift+enter\",\"command\":\"workbench.action.terminal.sendSequence\",\"args\":{{\"text\":\"\\u000a\"}},\"when\":\"terminalFocus\"}}\n```",
                path.display()
            ),
        });
    }
    let hint = match term.as_str() {
        "iTerm.app" => {
            "iTerm2 → Settings → Profiles → Keys → add a mapping: shift+enter → Send Text → `\\n`"
        }
        "Apple_Terminal" => {
            "Terminal.app can't bind shift+enter; use option+enter, which chanto already accepts."
        }
        "WezTerm" => {
            "WezTerm already sends shift+enter as a distinct key, so multi-line input works as-is."
        }
        _ => "Add a shift+enter → send `\\n` mapping in your terminal's key settings.",
    };
    Outcome::Show(format!(
        "**Terminal setup ({})\n\n{hint}\n\nThis session: alt+enter and ctrl+j also insert a newline.",
        if term.is_empty() { "unknown" } else { &term }
    ))
}

/// Strip `//` line comments so VS Code's JSONC keybindings file parses.
fn strip_json_comments(s: &str) -> String {
    s.lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `/privacy-settings`: Chanto is local-first, so this answers the only
/// question that matters — what leaves the machine, and to whom.
fn privacy_settings(env: &std::sync::Arc<crate::agent::Env>) -> String {
    let hosted = env.provider().kind.is_hosted();
    let base = &env.provider().base_url;
    let mut s = String::from(
        "## Privacy\n\nchanto keeps sessions, checkpoints and history on disk. Telemetry is **off by default**.\n\n### What leaves this machine\n",
    );
    if env.config.telemetry.otlp_endpoint.is_some() {
        s.push_str("\nOpt-in OTLP export is enabled: model/provider identifiers, token counts, USD cost, duration, status, tool names, service name, timestamps and random trace/span IDs are sent to your configured collector. No prompts, responses, tool arguments, paths or credentials are sent.\n");
    }
    if base.contains("localhost") || base.contains("127.0.0.1") || base.contains("[::1]") {
        s.push_str(&format!(
            "- Nothing: the model runs at `{base}` on this machine (Ollama or similar).\n"
        ));
    } else {
        s.push_str(&format!(
            "- Your prompts, attached files and tool results go to `{base}`{}.\n",
            if hosted { " (a hosted API)" } else { "" }
        ));
        s.push_str("- Web search/fetch and Browser send the URLs you visit.\n");
    }
    s.push_str("\n### Secrets\n- Files matching secret patterns (`.env`, `~/.ssh/id_*`, credential stores) are withheld from the model and from sandboxed shell reads.\n- MCP stdio servers are spawned without secret-named environment variables.\n");
    s.push_str(&format!("\n### Feedback and error reports\n- `/feedback` sends only what it shows you, after you confirm.\n- Automatic error reports: {}. They carry the error kind, a redacted message, version, OS, provider type and a random install id; never prompts, code or keys.\n", crate::feedback::status(&env.config)));
    s.push_str("\n### Local data\n");
    s.push_str(&format!(
        "- Config and sessions: `{}`\n",
        crate::config::home_dir().display()
    ));
    s.push_str(
        "- Run `/clear` to start fresh; delete the directory above to remove all local history.\n",
    );
    s
}

/// `/release-notes`: prefer a CHANGELOG, else summarize recent commits.
fn release_notes() -> String {
    const CHANGELOG: &str = include_str!("../CHANGELOG.md");
    let version = env!("CARGO_PKG_VERSION");
    // This version's section of the changelog shipped with the binary.
    let notes = CHANGELOG
        .split("\n## ")
        .find(|s| s.starts_with(&format!("v{version} ")))
        .map(|s| format!("## {}", s.trim_end()));
    let footer = format!(
        "All releases: {}\n\n`chanto upgrade --check` shows whether a newer version is available.",
        crate::update::RELEASES
    );
    match notes {
        Some(n) => format!("{n}\n\n{footer}"),
        None => format!("This is chanto {version}.\n\n{footer}"),
    }
}

/// Run one hook command and show its output, for `/hooks test`.
async fn run_hook_test(command: &str, event: &str) -> Outcome {
    let out = crate::shell::command(command).output().await;
    match out {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);
            let mut s = format!(
                "## {event} → `{command}`\n\nexit: {}\n",
                o.status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "killed".into())
            );
            if !stdout.trim().is_empty() {
                s.push_str(&format!("\n**stdout**\n```\n{}```\n", stdout));
            }
            if !stderr.trim().is_empty() {
                s.push_str(&format!("\n**stderr**\n```\n{}```\n", stderr));
            }
            Outcome::Show(s)
        }
        Err(e) => Outcome::Show(format!("Could not run hook `{command}`: {e}")),
    }
}

/// How a configured MCP server currently stands. A stable, short set so the
/// `/mcp` panel, `/status` and the markdown listing can share one description.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum McpState {
    Connected,
    NeedsLogin,
    Failed,
    Off,
    /// In `.mcp.json` but not approved for this project yet.
    Pending,
    /// Blocked by `disabledMcpjsonServers`.
    Rejected,
}

impl McpState {
    /// The word after the server name, e.g. "connected".
    pub fn label(self) -> &'static str {
        match self {
            McpState::Connected => "connected",
            McpState::NeedsLogin => "needs login",
            McpState::Failed => "failed",
            McpState::Off => "disabled",
            McpState::Pending => "pending approval",
            McpState::Rejected => "rejected",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            McpState::Connected => "✓",
            McpState::NeedsLogin => "!",
            McpState::Failed => "✗",
            McpState::Off => "⊘",
            McpState::Pending => "⏸",
            McpState::Rejected => "✘",
        }
    }
}

/// One server as `/mcp` and the TUI panel show it.
pub struct McpRow {
    pub name: String,
    pub state: McpState,
    /// Plain description: tools and extras, a failure reason, or why it is off.
    pub detail: String,
    /// Remote server, so signing in is possible.
    pub http: bool,
    /// Off only because this project disabled it, so `/mcp enable` turns it back
    /// on. A server off for any other reason can't be switched on from here.
    pub can_enable: bool,
}

impl McpRow {
    /// The row in the markdown `/mcp` listing, kept in step with the panel.
    pub fn markdown_line(&self) -> String {
        let name = &self.name;
        let mut line = match self.state {
            McpState::Off => format!("- **{name}** ⊘ {}: {}", self.state.label(), self.detail),
            McpState::Connected => {
                format!("- **{name}** ✓ {} — {}", self.state.label(), self.detail)
            }
            McpState::Failed => {
                format!("- **{name}** ✗ {}: {}", self.state.label(), self.detail)
            }
            McpState::NeedsLogin => format!("- **{name}** ! {}", self.state.label()),
            McpState::Pending => format!("- **{name}** ⏸ {} · {}", self.state.label(), self.detail),
            McpState::Rejected => {
                format!("- **{name}** ✘ {} · {}", self.state.label(), self.detail)
            }
        };
        match self.state {
            McpState::NeedsLogin => {
                line.push_str(&format!(" — run `/mcp login {name}`"));
            }
            McpState::Failed => {
                line.push_str(&format!(" — `/mcp reconnect {name}`"));
            }
            McpState::Pending => {
                line.push_str(&format!(
                    " — `/mcp enable {name}` approves it for this project"
                ));
            }
            _ => {}
        }
        line.push('\n');
        line
    }
}

/// A failure reason for a row that already names the server: drop the
/// "MCP server `name`" the transport errors lead with.
fn failed_detail(name: &str, why: &str) -> String {
    let rest = why
        .strip_prefix(&format!("MCP server `{name}`: "))
        .or_else(|| why.strip_prefix(&format!("MCP server `{name}` ")))
        .unwrap_or(why);
    match rest {
        "exited" => "the process exited".to_string(),
        "is not running" => "the process isn't running".to_string(),
        _ => rest.to_string(),
    }
}

/// Every server `/mcp` knows about: the configured ones and any `.mcp.json`
/// entries still waiting on approval or rejected by settings.
pub fn mcp_rows(env: &crate::agent::Env) -> Vec<McpRow> {
    use crate::mcp::Status;
    let root = &env.root;
    let approvals = crate::mcp_policy::Approvals::load(
        root,
        &env.config.compat,
        crate::config::is_trusted_here(root),
        crate::config::is_trusted(root),
    );
    let unloaded = crate::mcp_policy::unloaded_mcpjson(root, &env.config.mcp_servers, &approvals);
    let mut rows: Vec<McpRow> = env
        .config
        .mcp_servers
        .iter()
        .map(|(name, cfg)| {
            let client = env.mcp.iter().find(|c| &c.name == name);
            let http = client
                .map(|c| c.is_http())
                .unwrap_or_else(|| cfg.url.is_some());
            let (state, detail, can_enable) = match (cfg.disabled, client.map(|c| (c, c.status())))
            {
                (true, _) => {
                    // Name the file it was turned off in: settings and the
                    // project config both carry `disabled`, and the fix differs.
                    let file = env
                        .config
                        .origins
                        .get(&format!("mcp_servers.{name}"))
                        .map(|p| format!(" in `{}`", crate::util::tilde(p)))
                        .unwrap_or_default();
                    (McpState::Off, format!("`disabled = true`{file}"), false)
                }
                (_, Some((c, Status::Connected))) => {
                    let tools: Vec<String> = c.tools().into_iter().map(|t| t.name).collect();
                    let mut extra = Vec::new();
                    if !tools.is_empty() {
                        extra.push(crate::util::plural(tools.len(), "tool"));
                    }
                    if !c.prompts().is_empty() {
                        extra.push(crate::util::plural(c.prompts().len(), "prompt"));
                    }
                    if c.has_resources() {
                        extra.push("resources".to_string());
                    }
                    if extra.is_empty() {
                        extra.push("no tools".to_string());
                    }
                    (McpState::Connected, extra.join(" · "), false)
                }
                (_, Some((_, Status::NeedsLogin))) => (McpState::NeedsLogin, String::new(), false),
                (_, Some((_, Status::Failed(why)))) => {
                    (McpState::Failed, failed_detail(name, &why), false)
                }
                (_, Some((_, Status::Off(why)))) => {
                    // Only a project disable is ours to undo; a gate in the
                    // user's settings has to be changed where it was set.
                    let enable = why == crate::mcp_policy::Gate::Disabled.describe();
                    // The panel and `/mcp` list the command that undoes it, so
                    // the detail says what happened, not how to reverse it.
                    let detail = if enable {
                        "disabled for this project".to_string()
                    } else {
                        why
                    };
                    (McpState::Off, detail, enable)
                }
                (_, None) => (McpState::Failed, "not connected".into(), false),
            };
            McpRow {
                name: name.clone(),
                state,
                detail,
                http,
                can_enable,
            }
        })
        .collect();
    for (name, approval) in unloaded {
        let (state, detail) = match approval {
            crate::mcp_policy::Approval::Rejected => (
                McpState::Rejected,
                ".mcp.json (disabledMcpjsonServers)".to_string(),
            ),
            _ => (McpState::Pending, ".mcp.json".to_string()),
        };
        rows.push(McpRow {
            name,
            state,
            detail,
            http: false,
            can_enable: false,
        });
    }
    rows
}

/// Connected / configured counts for the footer, without reading approval
/// files on every frame.
pub fn mcp_summary(env: &crate::agent::Env) -> (usize, usize, bool) {
    use crate::mcp::Status;
    // A server turned off on purpose isn't a connection to miss, so it stays
    // out of the ratio; it still shows in the `/mcp` panel.
    let off = |name: &String| {
        env.mcp
            .iter()
            .any(|c| &c.name == name && matches!(c.status(), Status::Off(_)))
    };
    let total = env
        .config
        .mcp_servers
        .iter()
        .filter(|(name, cfg)| !cfg.disabled && !off(name))
        .count();
    let mut connected = 0;
    let mut attention = false;
    for c in &env.mcp {
        match c.status() {
            Status::Connected => connected += 1,
            Status::NeedsLogin | Status::Failed(_) => attention = true,
            Status::Off(_) => {}
        }
    }
    (connected, total, attention)
}

/// Expand `@path` mentions: attach file contents (or images) to the prompt.
/// `/mcp`, `/mcp login|logout <server>`, `/mcp reconnect|enable|disable <server>|all`.
async fn mcp_command(env: &std::sync::Arc<crate::agent::Env>, arg: &str) -> Outcome {
    let (sub, name) = arg
        .split_once(char::is_whitespace)
        .map(|(a, b)| (a, b.trim()))
        .unwrap_or((arg, ""));
    if matches!(sub, "login" | "logout" | "auth") {
        let candidates: Vec<&str> = env
            .mcp
            .iter()
            .filter(|c| c.is_http())
            .map(|c| c.name.as_str())
            .collect();
        let Some(c) = env.mcp.iter().find(|c| c.name == name) else {
            return Outcome::Show(if name.is_empty() {
                format!(
                    "Usage: /mcp {sub} <server> (remote servers: {})",
                    if candidates.is_empty() {
                        "none".into()
                    } else {
                        candidates.join(", ")
                    }
                )
            } else if env.config.mcp_servers.contains_key(name) {
                format!(
                    "`{name}` didn't connect at startup (unreachable or misconfigured), so there is nothing to log in to. Check `/mcp`."
                )
            } else {
                format!("No MCP server named `{name}`.")
            });
        };
        if sub == "logout" {
            return Outcome::Show(if c.logout() {
                format!("Logged out of `{name}` and deleted its stored token.")
            } else {
                format!("`{name}` had no stored token.")
            });
        }
        let env2 = env.clone();
        let server = name.to_string();
        let open = move |url: &str| {
            env2.emit(Event::Info(format!(
                "Opening your browser to log in to `{server}`. If it doesn't open, visit:\n{url}"
            )));
            crate::mcp_auth::open_browser(url);
        };
        env.emit(Event::Busy(Some(format!(
            "Waiting for browser login to {name}…"
        ))));
        let r = c.login(&open).await;
        env.emit(Event::Busy(None));
        return Outcome::Show(match r {
            Ok(()) => {
                let tools = c.tools();
                let prompts = c.prompts();
                format!(
                    "Logged in to `{name}`: {}{}{}. Token saved under `~/.chanto/mcp-auth`.",
                    crate::util::plural(tools.len(), "tool"),
                    if prompts.is_empty() {
                        String::new()
                    } else {
                        format!(
                            ", {} (see /help)",
                            crate::util::plural(prompts.len(), "prompt")
                        )
                    },
                    if c.has_resources() {
                        ", resources (ListMcpResources / @server:uri)"
                    } else {
                        ""
                    }
                )
            }
            Err(e) => format!("Login to `{name}` failed: {e:#}"),
        });
    }
    if matches!(sub, "reconnect" | "enable" | "disable") {
        return Outcome::Show(mcp_toggle(env, sub, name).await);
    }
    let rows = mcp_rows(env);
    if rows.is_empty() {
        return Outcome::Show("No MCP servers configured. Add them with `chanto mcp add`, under [mcp_servers.<name>] in ~/.chanto/config.toml, or in .mcp.json.".into());
    }
    let mut s = String::from("## MCP servers\n");
    for row in &rows {
        s.push_str(&row.markdown_line());
    }
    s.push_str("\n`/mcp reconnect <server>|all` · `/mcp enable|disable <server>|all` · `/mcp login|logout <server>`");
    Outcome::Show(s)
}

/// `/mcp reconnect|enable|disable <server>|all`. Turning a server off or on
/// is remembered for this project (`disabledMcpServers` in
/// `~/.chanto/mcp-projects.json`).
async fn mcp_toggle(env: &std::sync::Arc<crate::agent::Env>, sub: &str, name: &str) -> String {
    use crate::mcp::Status;
    use crate::mcp_policy::Gate;
    if name.is_empty() {
        return format!("Usage: `/mcp {sub} <server>|all`");
    }
    let root = env.root.clone();
    let all = name == "all";
    let targets: Vec<_> = env
        .mcp
        .iter()
        .filter(|c| match (all, sub) {
            (false, _) => c.name == name,
            // `reconnect all` retries what failed or needs authentication.
            (true, "reconnect") => matches!(c.status(), Status::Failed(_) | Status::NeedsLogin),
            (true, "enable") => matches!(c.status(), Status::Off(_)),
            (true, _) => !matches!(c.status(), Status::Off(_)),
        })
        .cloned()
        .collect();
    if targets.is_empty() {
        if !all && sub != "reconnect" {
            // A project `.mcp.json` server that isn't approved yet: approve or reject it.
            let approvals = crate::mcp_policy::Approvals::load(
                &root,
                &env.config.compat,
                crate::config::is_trusted_here(&root),
                crate::config::is_trusted(&root),
            );
            if crate::mcp_policy::unloaded_mcpjson(&root, &env.config.mcp_servers, &approvals)
                .iter()
                .any(|(n, _)| n == name)
            {
                let approve = sub == "enable";
                return match crate::mcp_policy::set_mcpjson_approval(&root, name, approve) {
                    Ok(()) if approve => format!(
                        "Approved `{name}` from .mcp.json for this project; it connects from your next session."
                    ),
                    Ok(()) => format!("Rejected `{name}` from .mcp.json for this project."),
                    Err(e) => format!("Couldn't save the choice: {e:#}"),
                };
            }
        }
        return if all {
            format!("No MCP servers to {sub}.")
        } else {
            format!("No MCP server named `{name}`.")
        };
    }
    let mut out = Vec::new();
    for c in targets {
        let n = c.name.clone();
        if sub == "disable" {
            if let Err(e) = crate::mcp_policy::set_disabled(&root, &n, true) {
                out.push(format!("Couldn't save the choice for `{n}`: {e:#}"));
                continue;
            }
            c.shutdown(Status::Off(Gate::Disabled.describe()));
            out.push(format!("Disabled `{n}` for this project."));
            continue;
        }
        let gate = env.config.mcp_servers.get(&n).and_then(|s| s.gate.clone());
        match gate {
            Some(g @ (Gate::Policy(_) | Gate::DisabledInClaude)) => {
                out.push(format!("`{n}` stays off: {}.", g.describe()));
                continue;
            }
            _ if c.config().disabled => {
                let file = env
                    .config
                    .origins
                    .get(&format!("mcp_servers.{n}"))
                    .map(|p| format!(" in `{}`", crate::util::tilde(p)))
                    .unwrap_or_default();
                out.push(format!(
                    "`{n}` has `disabled = true`{file}; remove that to turn it on."
                ));
                continue;
            }
            _ => {}
        }
        if sub == "reconnect" && matches!(c.status(), Status::Off(_)) {
            out.push(format!("`{n}` is disabled; `/mcp enable {n}` turns it on."));
            continue;
        }
        if sub == "enable"
            && let Err(e) = crate::mcp_policy::set_disabled(&root, &n, false)
        {
            out.push(format!("Couldn't save the choice for `{n}`: {e:#}"));
            continue;
        }
        env.emit(Event::Busy(Some(format!("Connecting to {n}…"))));
        let r = c.reconnect().await;
        env.emit(Event::Busy(None));
        let verb = if sub == "enable" {
            "Enabled"
        } else {
            "Reconnected"
        };
        out.push(match r {
            Ok(()) if c.status() == Status::NeedsLogin => {
                format!("{verb} `{n}`; it needs you to log in: `/mcp login {n}`.")
            }
            Ok(()) => format!(
                "{verb} `{n}`: {}.",
                crate::util::plural(c.tools().len(), "tool")
            ),
            Err(e) => format!(
                "`{n}` failed to connect: {}",
                crate::util::one_line(&format!("{e:#}"), 300)
            ),
        });
    }
    out.join("\n")
}

/// `/status`: one screen of what this session is running with. Everything
/// here is already known locally; `/doctor` is the one that probes servers.
fn status(agent: &Agent) -> String {
    let env = &agent.env;
    let model = env.model();
    let (p, _) = env.resolve(&model);
    let provider = crate::doctor::resolve(&env.config, &model).2;
    let auth = match crate::doctor::credential(&env.config, &model) {
        Some(c) => c,
        None if p.kind.is_cloud() => "the cloud's own credentials (`/doctor` checks them)".into(),
        None => "none".into(),
    };
    let account = model
        .split_once('/')
        .and_then(|(prefix, _)| crate::auth::account_label(prefix))
        .map(|a| format!(" · account: **{a}**"))
        .unwrap_or_default();
    let cwd = env.cwd();
    let branch = crate::tui::git_branch(&cwd)
        .map(|b| format!(" · branch `{b}`"))
        .unwrap_or_default();
    let used = agent.context_used();
    let max = env.context_max();
    let sandbox = if crate::tools::bash::sandbox_on() {
        "on"
    } else if crate::tools::bash::sandbox_available() {
        "off"
    } else {
        "unavailable"
    };
    let session = match &agent.session {
        Some(s) => format!(
            "`{}`{}",
            s.id,
            if s.title.is_empty() {
                String::new()
            } else {
                format!(" — {}", crate::util::one_line(&s.title, 60))
            }
        ),
        None => "not saved".into(),
    };
    let mcp: Vec<String> = mcp_rows(env)
        .into_iter()
        .map(|r| {
            // One line each; the detail only when it adds something to the state.
            let detail = match r.state {
                McpState::NeedsLogin | McpState::Rejected => String::new(),
                _ => format!(" ({})", r.detail),
            };
            format!("{} {} {}{detail}", r.name, r.state.glyph(), r.state.label())
        })
        .collect();
    format!(
        "## Status\n- chanto **v{}**\n- model: **{model}** via {provider} · credential: {auth}{account}\n- mode: {} · sandbox: {sandbox}\n- cwd: `{}`{branch}\n- context: {} / {} tokens ({:.0}%)\n- session: {session}\n- tokens this session: {} in · {} out · {}\n- MCP: {}\n\n`/context` breaks down the context, `/cost` the spend, `/doctor` checks the setup.",
        env!("CARGO_PKG_VERSION"),
        env.mode().label(),
        cwd.display(),
        crate::util::short_num(used),
        crate::util::short_num(max),
        used as f64 * 100.0 / max.max(1) as f64,
        crate::util::short_num(agent.total_in),
        crate::util::short_num(agent.total_out),
        env.accounting.summary(),
        if mcp.is_empty() {
            "none configured".to_string()
        } else {
            mcp.join(", ")
        },
    )
}

pub async fn expand_mentions(agent: &Agent, input: &str) -> (String, Vec<String>) {
    use base64::Engine;
    let cwd = agent.env.cwd();
    let mut attachments = String::new();
    let mut images = Vec::new();
    let escaped = input.replace("\\ ", "\u{1}");
    for tok in escaped.split_whitespace() {
        let tok = tok.replace('\u{1}', " ");
        // A dragged-in file arrives as a bare path, so accept image paths either
        // way; only an explicit `@` attaches text files, since a bare path in
        // prose shouldn't silently pull a whole file into the prompt.
        let (p, explicit) = match tok.strip_prefix('@') {
            Some(p) => (p, true),
            None => (tok.as_str(), false),
        };
        let p = p.trim_end_matches([',', '.', ';', ':', ')', '?', '!']);
        if p.is_empty() {
            continue;
        }
        // `@server:uri` reads an MCP resource.
        if let Some(r) = crate::mcp::read_mention(&agent.env.mcp, p).await {
            match r {
                Ok((server, uri, text)) => attachments.push_str(&format!(
                    "\n\n<resource server=\"{server}\" uri=\"{uri}\">\n{}\n</resource>",
                    crate::util::truncate(&text, 100_000)
                )),
                Err(e) => agent
                    .env
                    .emit(Event::Warn(format!("Could not read @{p}: {e}"))),
            }
            continue;
        }
        let path = crate::util::resolve(&cwd, p);
        if path.is_dir() {
            if !explicit {
                continue;
            }
            let listing: Vec<String> = std::fs::read_dir(&path)
                .map(|rd| {
                    rd.flatten()
                        .take(200)
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect()
                })
                .unwrap_or_default();
            attachments.push_str(&format!(
                "\n\n<directory path=\"{}\">\n{}\n</directory>",
                path.display(),
                listing.join("\n")
            ));
        } else if path.is_file() {
            if crate::util::is_image_path(&path) {
                match std::fs::read(&path) {
                    Ok(b) => images.push(base64::engine::general_purpose::STANDARD.encode(b)),
                    // Never drop an attachment silently: a transient screenshot
                    // path is the usual cause, and the user should hear about it.
                    Err(e) => agent
                        .env
                        .emit(Event::Warn(format!("Could not read image {p}: {e}"))),
                }
                continue;
            }
            if !explicit {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    agent.env.mark_read(&path);
                    attachments.push_str(&format!(
                        "\n\n<file path=\"{}\">\n{}\n</file>",
                        path.display(),
                        crate::util::truncate(&text, 100_000)
                    ));
                }
                Err(e) => agent
                    .env
                    .emit(Event::Warn(format!("Could not read {p}: {e}"))),
            }
        } else if explicit {
            // A mention the user can see but the model never receives must not be
            // silent: say so, whether the file is missing or was cleaned up (as
            // happens to screenshots).
            agent
                .env
                .emit(Event::Warn(format!("Could not attach {p}: no such file")));
        }
    }
    (format!("{input}{attachments}"), images)
}

/// What `/loop` was asked to do.
#[derive(Debug, PartialEq)]
pub enum LoopCmd {
    /// Run now, then every `every`; without one the loop is self-paced (the
    /// model picks each next run with ScheduleWakeup). Without a prompt it
    /// runs `loop.md` or the built-in maintenance prompt.
    Start {
        every: Option<std::time::Duration>,
        prompt: Option<String>,
    },
    Stop,
    Status,
}

/// How long a loop runs before it stops on its own.
pub const LOOP_EXPIRY: std::time::Duration = std::time::Duration::from_secs(7 * 86_400);

pub const LOOP_USAGE: &str = "Usage: `/loop [interval] [prompt]` (e.g. `/loop 5m check the CI and fix failures`, `/loop check the deploy every 2h`; without an interval the model picks when to run next; without a prompt it runs `loop.md` or the maintenance prompt) · `/loop status` · `/loop stop`";

/// Parse `/loop` arguments: the interval leads
/// as a bare token (`5m`) or trails as `every 2 hours`; units are s, m, h and
/// d. Seconds round up to whole minutes, the shortest interval. Without an
/// interval the loop is self-paced; without a prompt it runs the default one.
pub fn parse_loop(arg: &str) -> Result<LoopCmd, String> {
    let arg = arg.trim();
    match arg {
        "status" | "list" => return Ok(LoopCmd::Status),
        "stop" | "off" | "cancel" => return Ok(LoopCmd::Stop),
        _ => {}
    }
    let (first, rest) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
    let (every, prompt) = if first.starts_with(|c: char| c.is_ascii_digit()) {
        let digits = first
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(first.len());
        match loop_interval(&first[..digits], &first[digits..]) {
            Some(d) => (Some(d), rest.trim()),
            None => {
                return Err(format!(
                    "`{first}` isn't an interval: use 1m to 7d in s, m, h or d, e.g. `5m`. {LOOP_USAGE}"
                ));
            }
        }
    } else {
        static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"(?i)(^|\s+)every\s+(\d*)\s*([a-z]+)\s*$").unwrap()
        });
        match RE.captures(arg).and_then(|c| {
            Some((
                c.get(0)?.start(),
                loop_interval(c.get(2)?.as_str(), c.get(3)?.as_str())?,
            ))
        }) {
            Some((at, d)) => (Some(d), arg[..at].trim()),
            None => (None, arg),
        }
    };
    Ok(LoopCmd::Start {
        every,
        prompt: Some(prompt.to_string()).filter(|p| !p.is_empty()),
    })
}

/// Start a `/loop` on the session's scheduler; its first iteration runs as
/// soon as the session is idle. Returns the note to show.
pub fn start_loop(env: &Env, every: Option<std::time::Duration>, prompt: Option<String>) -> String {
    let what = prompt
        .clone()
        .unwrap_or_else(|| "the default loop prompt (loop.md, or maintenance)".into());
    env.scheduler.start_loop(every, prompt);
    // What it may cost if it runs all day (nothing for local models).
    let spend = crate::spend::loop_note(env, every)
        .map(|(n, _)| format!("\n{n}"))
        .unwrap_or_default();
    match every {
        Some(d) => format!(
            "Looping every {}: {what} · `/loop stop` or esc ends it{spend}",
            crate::util::duration(d.as_secs())
        ),
        None => format!(
            "Self-paced loop: {what} · the model picks when each next run is (1m to 1h) · `/loop stop` or esc ends it{spend}"
        ),
    }
}

/// `5` + `m`, `2` + `hours`; an empty count is one (`every hour`).
fn loop_interval(count: &str, unit: &str) -> Option<std::time::Duration> {
    let n: u64 = if count.is_empty() {
        1
    } else {
        count.parse().ok()?
    };
    let secs = match unit.to_ascii_lowercase().as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => n,
        "m" | "min" | "mins" | "minute" | "minutes" => n.checked_mul(60)?,
        "h" | "hr" | "hrs" | "hour" | "hours" => n.checked_mul(3600)?,
        "d" | "day" | "days" => n.checked_mul(86_400)?,
        _ => return None,
    };
    // Loops end after a week, so a longer interval would never fire.
    if secs == 0 || secs > LOOP_EXPIRY.as_secs() {
        return None;
    }
    Some(std::time::Duration::from_secs(secs.div_ceil(60) * 60))
}

#[cfg(test)]
mod arg_tests {
    use super::{
        BUILTIN, LoopCmd, McpRow, McpState, SHORTCUTS, expand_args, expand_skill_args,
        failed_detail, is_immediate, parse_cmd, parse_loop, shortcuts_markdown,
    };

    #[test]
    fn suggests_the_closest_command() {
        let names = || BUILTIN.iter().map(|c| c.name.to_string());
        use crate::util::closest;
        assert_eq!(closest("comapct", names()).as_deref(), Some("compact"));
        assert_eq!(closest("reivew", names()).as_deref(), Some("review"));
        assert_eq!(closest("Doctr", names()).as_deref(), Some("doctor"));
        assert_eq!(closest("xyzzy", names()), None);
    }

    #[test]
    fn skill_args_count_from_zero() {
        assert_eq!(
            expand_skill_args("Migrate $0 from $1 to $2.", "Button react vue"),
            "Migrate Button from react to vue."
        );
        assert_eq!(
            expand_skill_args("All: $ARGUMENTS; first: $ARGUMENTS[0]", "a \"b c\""),
            "All: a \"b c\"; first: a"
        );
        assert_eq!(
            expand_skill_args("Second: $ARGUMENTS[1]", "a \"b c\""),
            "Second: b c"
        );
        assert_eq!(
            expand_skill_args("Review", "src/"),
            "Review\n\nARGUMENTS: src/"
        );
        assert_eq!(expand_skill_args("Review", ""), "Review");
    }

    #[test]
    fn template_args() {
        assert_eq!(expand_args("Fix $ARGUMENTS", "bug 1"), "Fix bug 1");
        assert_eq!(expand_args("a=$1 b=$2", "x \"y z\""), "a=x b=y z");
        assert_eq!(
            expand_args("ticket $TICKET", "TICKET=ABC-1"),
            "ticket ABC-1"
        );
        assert_eq!(expand_args("Review", "src/"), "Review\n\nsrc/");
        assert_eq!(expand_args("Review", ""), "Review");
    }

    #[test]
    fn parse_cmd_splits_name_and_args() {
        assert_eq!(parse_cmd("/model gpt-5"), ("model", "gpt-5"));
        assert_eq!(parse_cmd("/help"), ("help", ""));
        assert_eq!(parse_cmd("/model   gpt-5  "), ("model", "gpt-5"));
    }

    #[test]
    fn mcp_rows_describe_each_state() {
        let row = |state, detail: &str| {
            McpRow {
                name: "srv".into(),
                state,
                detail: detail.into(),
                http: false,
                can_enable: false,
            }
            .markdown_line()
        };
        assert_eq!(
            row(McpState::Connected, "3 tools"),
            "- **srv** ✓ connected — 3 tools\n"
        );
        assert_eq!(
            row(McpState::NeedsLogin, "not signed in"),
            "- **srv** ! needs login — run `/mcp login srv`\n"
        );
        assert_eq!(
            row(McpState::Failed, "401"),
            "- **srv** ✗ failed: 401 — `/mcp reconnect srv`\n"
        );
        assert_eq!(
            row(McpState::Off, "turned off"),
            "- **srv** ⊘ disabled: turned off\n"
        );
        assert_eq!(
            row(McpState::Pending, ".mcp.json"),
            "- **srv** ⏸ pending approval · .mcp.json — `/mcp enable srv` approves it for this project\n"
        );
        assert_eq!(
            row(McpState::Rejected, ".mcp.json"),
            "- **srv** ✘ rejected · .mcp.json\n"
        );
    }

    #[test]
    fn failure_reasons_drop_the_servers_own_name() {
        assert_eq!(
            failed_detail("srv", "MCP server `srv` exited"),
            "the process exited"
        );
        assert_eq!(
            failed_detail("srv", "MCP server `srv`: bad handshake"),
            "bad handshake"
        );
        // Another server's name is part of the reason, so it stays.
        assert_eq!(
            failed_detail("srv", "MCP server `proxy` is not running"),
            "MCP server `proxy` is not running"
        );
        assert_eq!(
            failed_detail("srv", "timed out after 30s during startup"),
            "timed out after 30s during startup"
        );
    }

    #[test]
    fn only_env_commands_are_immediate() {
        // These are safe to run while a turn holds the agent lock.
        for c in [
            "/model gpt-5",
            "/models",
            "/think off",
            "/mode plan",
            "/theme light",
            "/help",
            "/exit",
            "/quiet on",
            "/mcp",
        ] {
            assert!(is_immediate(c), "{c} should apply immediately");
        }
        // These rewrite the conversation and must wait for the agent. Queuing
        // them is what keeps history consistent; they must never be treated as
        // env-only, or they would silently run against a busy agent.
        for c in [
            "/compact",
            "/clear",
            "/rewind",
            "/resume",
            "/undo",
            "/context",
            "/cost",
            "/export",
            "/transcript",
        ] {
            assert!(!is_immediate(c), "{c} must not bypass the agent lock");
        }
    }

    #[test]
    fn immediate_commands_are_all_handled_by_handle_env() {
        // Guards against is_immediate() listing something handle_env doesn't
        // serve, which would run it immediately and then do nothing at all.
        for c in [
            "/model", "/models", "/think", "/mode", "/theme", "/vision", "/help", "/exit", "/quit",
            "/?",
        ] {
            assert!(is_immediate(c), "{c}");
        }
        assert!(!is_immediate("/compact"));
    }

    #[test]
    fn shortcuts_are_unique_and_documented() {
        // The TUI's `?` overlay and `/help` both render this list, so a blank or
        // duplicated row would show up in both places.
        let md = shortcuts_markdown();
        let mut seen = std::collections::HashSet::new();
        for (k, d) in SHORTCUTS {
            assert!(
                !k.trim().is_empty() && !d.trim().is_empty(),
                "empty shortcut row"
            );
            assert!(seen.insert(*k), "duplicate shortcut key: {k}");
            assert!(md.contains(*k), "{k} missing from the markdown form");
        }
    }

    #[test]
    fn loop_intervals_lead_or_trail_the_prompt() {
        let start = |secs: u64, prompt: &str| {
            Ok(LoopCmd::Start {
                every: Some(std::time::Duration::from_secs(secs)),
                prompt: Some(prompt.into()),
            })
        };
        assert_eq!(
            parse_loop("5m check the CI and fix failures"),
            start(300, "check the CI and fix failures")
        );
        assert_eq!(parse_loop("2h /review-pr 12"), start(7200, "/review-pr 12"));
        assert_eq!(parse_loop("1d  summarize"), start(86_400, "summarize"));
        assert_eq!(
            parse_loop("check the deploy every 2 hours"),
            start(7200, "check the deploy")
        );
        assert_eq!(
            parse_loop("check the deploy every 15m"),
            start(900, "check the deploy")
        );
        assert_eq!(
            parse_loop("check the deploy every hour"),
            start(3600, "check the deploy")
        );
        // Seconds round up to whole minutes.
        assert_eq!(parse_loop("30s ping"), start(60, "ping"));
        assert_eq!(parse_loop("90s ping"), start(120, "ping"));
        // No interval: self-paced, and "every" mid-sentence is just words.
        assert_eq!(
            parse_loop("look at every file"),
            Ok(LoopCmd::Start {
                every: None,
                prompt: Some("look at every file".into())
            })
        );
        assert!(
            parse_loop("3 bugs to fix")
                .unwrap_err()
                .contains("isn't an interval")
        );
    }

    #[test]
    fn loop_stop_status_and_bad_input() {
        assert_eq!(
            parse_loop(""),
            Ok(LoopCmd::Start {
                every: None,
                prompt: None
            })
        );
        assert_eq!(parse_loop("status"), Ok(LoopCmd::Status));
        assert_eq!(parse_loop("stop"), Ok(LoopCmd::Stop));
        assert_eq!(parse_loop(" cancel "), Ok(LoopCmd::Stop));
        assert!(
            parse_loop("0m ping")
                .unwrap_err()
                .contains("isn't an interval")
        );
        assert!(parse_loop("5x ping").is_err());
        assert!(parse_loop("8d ping").is_err());
        assert_eq!(
            parse_loop("7d ping"),
            Ok(LoopCmd::Start {
                every: Some(std::time::Duration::from_secs(7 * 86_400)),
                prompt: Some("ping".into())
            })
        );
        // An interval alone runs the default prompt on that schedule.
        assert_eq!(
            parse_loop("5m"),
            Ok(LoopCmd::Start {
                every: Some(std::time::Duration::from_secs(300)),
                prompt: None
            })
        );
        assert_eq!(
            parse_loop("every 5m"),
            Ok(LoopCmd::Start {
                every: Some(std::time::Duration::from_secs(300)),
                prompt: None
            })
        );
    }
}

#[cfg(test)]
mod tests;
