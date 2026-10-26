//! The agent loop: streams model output, gates tool calls through the
//! permission system, executes tools (in parallel when safe), and manages the
//! context window (auto-compaction).

use crate::config::Config;
use crate::context::{AgentDef, CustomCommand, Skill};
use crate::llm::{self, ChatRequest, Delta, Message, Provider, Role, ToolCall};
use crate::permissions::{self, Decision, Mode, Rule, Rules};
use crate::tools::bash::BgShell;
use crate::tools::browser::BrowserSession;
use crate::tools::misc::Todo;
use crate::tools::{Ctx, Registry, Tool, ToolKind, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};
use tokio_util::sync::CancellationToken;

mod authorize;
mod background;
mod calls;
mod compact;
mod model;
mod protocol;
mod session;
mod subagent;
mod turn;
mod verify;

pub mod live;

pub(crate) use background::spawn_background;
pub use background::{
    Sent, background_running, message_background, resume_background, stop_background,
};
use protocol::*;
pub(crate) use subagent::*;

/// Warn the model once disk space gets this low.
const DISK_LOW: u64 = 2 << 30;
/// Refuse writes below this: running out mid-write corrupts files and breaks
/// every tool in confusing ways, so stop before that.
const DISK_CRITICAL: u64 = 256 << 20;

// ------------------------------------------------------------------ events

#[derive(Debug, Clone)]
pub enum Event {
    /// Model started generating (main agent only).
    AssistantStart,
    Text(String),
    Reasoning(String),
    AssistantEnd,
    ToolStart {
        id: String,
        label: String,
        parent: Option<String>,
    },
    ToolEnd {
        id: String,
        display: String,
        content: String,
        is_error: bool,
        /// Base64 images the tool produced (a screenshot, an image read), for
        /// frontends that can show them.
        images: Vec<String>,
        parent: Option<String>,
    },
    /// Live output tail from a running tool.
    ToolProgress {
        id: String,
        text: String,
    },
    /// Spinner label while waiting (None = idle).
    Busy(Option<String>),
    Info(String),
    Warn(String),
    Error(String),
    Todos(Vec<Todo>),
    Usage {
        context_used: u64,
        context_max: u64,
        total_in: u64,
        total_out: u64,
        total_cost_usd: f64,
        price_unknown: bool,
        tps: f64,
    },
    ModeChanged(Mode),
    Compacted {
        before: u64,
        after: u64,
    },
    /// A background agent started or changed state ("running", "completed", "failed", "stopped").
    Background {
        id: String,
        description: String,
        status: String,
    },
    /// Account rate limits reported by the provider (ChatGPT plans).
    Limits(crate::llm::limits::Snapshot),
    /// Live status of a long-running mode, by key ("goal", "team"): the first
    /// line is a compact summary for the status line, the rest a panel (a
    /// team's sub-tasks). No lines clears it.
    Status {
        key: String,
        lines: Vec<String>,
    },
}

/// JSON form of an event, for stream-json output and the HTTP server.
/// Seconds from the Bash tool's "[command timed out after Ns ...]" note.
fn timed_out_after(output: &str) -> Option<u64> {
    let rest = output.rsplit_once("[command timed out after ")?.1;
    rest.split_once('s')?.0.parse().ok()
}

/// The last check the agent ran since its most recent edit, if that run
/// failed. Anything edited after the run makes it stale, so no nudge.
fn last_failed_check(runs: &[(String, u64, bool, String)], edit_seq: u64) -> Option<(&str, &str)> {
    runs.iter()
        .rev()
        .find(|(_, seq, _, _)| *seq == edit_seq)
        .and_then(|(cmd, _, ok, out)| (!ok).then_some((cmd.as_str(), out.as_str())))
}

/// The target a failing test name belongs to, and the selector within it: the
/// file for pytest-style names (`tests/test_a.py::test_x`), the module for
/// unittest/doctest-style ones (`test_x (pkg.test_a.T.test_x)`). `None` when
/// the name carries no location, e.g. a bare `Class.test_method` header.
fn test_target(name: &str) -> Option<(String, Option<String>)> {
    if let Some((path, selector)) = name.split_once("::") {
        return Some((path.to_string(), Some(selector.to_string())));
    }
    let inside = name.split_once(" (")?.1.trim_end_matches(')');
    let parts: Vec<&str> = inside.split('.').collect();
    let module = parts.get(..parts.len().saturating_sub(2))?;
    if module.is_empty() {
        return None;
    }
    let selector = parts[module.len()..].join(".");
    Some((module.join("/"), Some(selector)))
}

/// The most recent failed check that nothing after it re-covered. A run
/// covers a failure when it stands for at least the same tests, or names the
/// failing test's own file, module, class or the test itself. This is the
/// "ran the suite, saw it fail, then ran a narrower thing that passed" finish,
/// which no single-run guard catches.
fn unverified_failure(runs: &[(String, u64, bool, String)]) -> Option<(&str, &str)> {
    let (idx, cmd, out) = runs
        .iter()
        .enumerate()
        .rev()
        .find(|(_, (_, _, ok, _))| !ok)
        .map(|(i, (cmd, _, _, out))| (i, cmd, out))?;
    let failing = crate::guardrails::failing_tests(out)?;
    let covered = runs[idx + 1..].iter().any(|(run, _, ok, _)| {
        *ok && (crate::guardrails::covers(run, cmd)
            || failing.iter().all(|test| runs_test(run, test)))
    });
    (!covered).then_some((cmd.as_str(), out.as_str()))
}

/// Whether `run` executes the failing test `test`. Only used when `covers`
/// can't compare the two commands: the run must name the test's file, module,
/// class or the test itself, and must not narrow within that target or filter
/// the selection.
fn runs_test(run: &str, test: &str) -> bool {
    let Some((target, selector)) = test_target(test) else {
        return false;
    };
    let words: Vec<&str> = run
        .split_whitespace()
        .map(|w| w.trim_matches(|c| c == '\'' || c == '"'))
        .collect();
    // Flags that pick part of a target: naming the target doesn't show the
    // failing test was among what ran.
    if words.iter().any(|w| {
        matches!(
            *w,
            "-k" | "--deselect" | "--ignore" | "--ignore-glob" | "--lf" | "--last-failed"
        ) || w.starts_with("--deselect=")
            || w.starts_with("--ignore=")
    }) {
        return false;
    }
    // A selected test is part of what the selector names; a sibling isn't.
    let selects = |selected: &str| {
        selector.as_deref().is_some_and(|full| {
            full.strip_prefix(selected).is_some_and(|rest| {
                rest.is_empty() || !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_')
            })
        })
    };
    words.iter().any(|w| {
        let w = w.trim_start_matches("./");
        if let Some((file, selected)) = w.split_once("::") {
            return file == target && selects(selected);
        }
        if w == target {
            return true;
        }
        if target.contains('/') || target.ends_with(".py") {
            // A directory holding the file (`pytest tests/`), never a
            // different file or a narrower selection inside it.
            return target
                .strip_prefix(w.trim_end_matches('/'))
                .is_some_and(|rest| rest.starts_with('/'));
        }
        // A module: `test_a` or `test_a.Class` runs the test's whole scope.
        w.strip_prefix(&target)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(selects)
    })
}

/// Whether a check command names specific test files. `python3 -m unittest
/// discover` and bare `pytest` run everything, so they cover any narrower
/// selection and don't need a "run the related tests" nudge.
fn scoped_to_tests(cmd: &str) -> bool {
    cmd.split_whitespace().any(|w| {
        let w = w.trim_matches(|c| c == '\'' || c == '"');
        !w.starts_with('-')
            && w != "./..."
            && !w.ends_with("/...")
            && w.rsplit('/').next().is_some_and(|f| f.contains('.'))
    })
}

/// Tools that can add data to the disk.
fn writes_to_disk(call: &ToolCall, tool: &dyn Tool) -> bool {
    match tool.kind_for(&call.arguments) {
        ToolKind::Edit | ToolKind::External => true,
        ToolKind::Execute => {
            !permissions::is_safe_command(call.arguments["command"].as_str().unwrap_or(""))
        }
        _ => call.name == "Task",
    }
}

/// Commands that recover space. These must still run when the disk is full,
/// otherwise the agent cannot clean up what filled it.
fn frees_space(call: &ToolCall) -> bool {
    let Some(cmd) = call.arguments["command"].as_str() else {
        return false;
    };
    let first = cmd.split_whitespace().next().unwrap_or("");
    matches!(first, "rm" | "rmdir" | "unlink" | "truncate" | "shred")
        || [
            "cargo clean",
            "git clean",
            "go clean",
            "docker system prune",
            "docker image prune",
            "npm cache clean",
            "yarn cache clean",
            "pip cache purge",
        ]
        .iter()
        .any(|p| cmd.contains(p))
}

pub fn event_json(ev: &Event) -> Option<Value> {
    Some(match ev {
        Event::AssistantStart => json!({"type": "assistant_start"}),
        Event::AssistantEnd => json!({"type": "assistant_end"}),
        Event::Text(t) => json!({"type": "text", "text": t}),
        Event::Reasoning(t) => json!({"type": "reasoning", "text": t}),
        Event::ToolStart { id, label, parent } => {
            json!({"type": "tool_start", "id": id, "label": label, "parent": parent})
        }
        Event::ToolEnd {
            id,
            display,
            content,
            is_error,
            images,
            parent,
        } => {
            json!({"type": "tool_end", "id": id, "is_error": is_error, "output": crate::util::truncate(content, 2000), "display": display, "images": image_data_urls(images), "parent": parent})
        }
        Event::ToolProgress { id, text } => {
            json!({"type": "tool_progress", "id": id, "text": text})
        }
        Event::Busy(label) => json!({"type": "busy", "label": label}),
        Event::Usage {
            context_used,
            context_max,
            total_in,
            total_out,
            total_cost_usd,
            price_unknown,
            tps,
        } => {
            json!({"type": "usage", "context_used": context_used, "context_max": context_max, "total_in": total_in, "total_out": total_out, "tps": tps, "total_cost_usd": total_cost_usd, "price_unknown": price_unknown})
        }
        Event::Warn(w) => json!({"type": "warning", "message": w}),
        Event::Error(e) => json!({"type": "error", "message": e}),
        Event::Info(i) => json!({"type": "info", "message": i}),
        Event::Todos(t) => json!({"type": "todos", "todos": t}),
        Event::ModeChanged(m) => json!({"type": "mode", "mode": m}),
        Event::Compacted { before, after } => {
            json!({"type": "compacted", "before": before, "after": after})
        }
        Event::Background {
            id,
            description,
            status,
        } => {
            json!({"type": "background_agent", "id": id, "description": description, "status": status})
        }
        Event::Limits(s) => {
            json!({"type": "rate_limits", "summary": s.summary(), "plan": s.plan_type, "primary": s.primary, "secondary": s.secondary})
        }
        Event::Status { key, lines } => json!({"type": "status", "key": key, "lines": lines}),
    })
}

/// Base64 images as data URLs a browser can show. A tool result or message
/// holds at most a few; two is enough to show what the model saw.
pub(crate) fn image_data_urls(images: &[String]) -> Vec<String> {
    images
        .iter()
        .take(2)
        .map(|img| format!("data:{};base64,{img}", crate::llm::sniff_mime(img)))
        .collect()
}

#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub tool: String,
    pub label: String,
    /// Detail shown in the dialog (command, diff preview, args).
    pub detail: String,
    /// Description of what "always allow" will do.
    pub always_label: String,
    pub reason: Option<String>,
    /// For a file edit: the file's full proposed content, which the user may
    /// edit before approving (`PermissionReply::AllowEdited`).
    pub proposed: Option<String>,
    /// How long this prompt waits before it is auto-denied (auto mode only);
    /// `None` means it waits indefinitely. Frontends may show it as a countdown.
    pub auto_deny: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PermissionReply {
    Allow,
    AllowAlways,
    /// Allow a file edit, but write this content (the user edited the proposal).
    AllowEdited(String),
    /// Deny; with feedback the model continues, without it the turn stops.
    Deny(Option<String>),
    /// Auto mode denied an unanswered prompt after its countdown. The note is
    /// addressed to the model as-is; no user made this decision.
    AutoDenied(String),
}

#[async_trait]
pub trait Frontend: Send + Sync {
    fn emit(&self, ev: Event);
    async fn permission(&self, req: PermissionRequest) -> PermissionReply;
    async fn question(&self, question: &str, options: &[String]) -> Option<String>;
    /// A question whose answer must be one of `options` (yes/no, Allow/Deny):
    /// frontends don't offer a free-text answer.
    async fn choose(&self, question: &str, options: &[String]) -> Option<String> {
        self.question(question, options).await
    }
    async fn tool_call(&self, _id: &str, _name: &str, _args: &Value, _cwd: &Path) {}
    fn file_restored(&self, _path: &Path, _text: &str) {}
    fn file_diff(&self, _id: &str, _path: &Path, _old: Option<&str>, _new: &str) {}
    async fn read_text_file(&self, _path: &Path) -> Option<Result<String>> {
        None
    }
    async fn write_text_file(&self, _path: &Path, _text: &str) -> Option<Result<()>> {
        None
    }
    /// Edits go to the client's buffers (ACP), where disk snapshots can't see them.
    fn writes_via_client(&self) -> bool {
        false
    }
}

/// A prompt typed while a turn was running, waiting for the model's next step.
/// The id lets a client edit or withdraw it until the agent takes it.
#[derive(Debug, Clone)]
pub struct Queued {
    pub id: String,
    pub text: String,
}

impl Queued {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().simple().to_string(),
            text: text.into(),
        }
    }
}

// --------------------------------------------------------------------- env

pub struct Env {
    pub accounting: Arc<crate::cost::Accounting>,
    /// Spend guard: fan-out estimates, burn-rate notices, the `/cost` breakdown.
    pub spend: crate::spend::Guard,
    pub config: Config,
    pub hooks: RwLock<crate::config::HooksConfig>,
    /// The auto-mode classifier's rules and fast-path switch
    /// (`permissions.auto_mode_rules`, `permissions.fast_path`): `config`'s at
    /// start, the new project's after `/cd`.
    pub auto_mode: RwLock<(Vec<String>, bool)>,
    pub provider: RwLock<Provider>,
    model: RwLock<String>,
    mode: RwLock<Mode>,
    /// The mode an edit's "always allow" switched away from to accept edits.
    /// `/cd` goes back to it; any other mode change clears it.
    mode_before_grant: Mutex<Option<Mode>>,
    think: AtomicBool,
    effort: RwLock<Option<String>>,
    vision: AtomicBool,
    /// Use the text tool-call protocol (model lacks native tool calling).
    pub text_tools: AtomicBool,
    cwd: RwLock<PathBuf>,
    read_files: Mutex<HashMap<PathBuf, SystemTime>>,
    pub todos: Mutex<Vec<Todo>>,
    pub shells: Mutex<HashMap<String, Arc<BgShell>>>,
    shell_counter: AtomicU64,
    pub browser: tokio::sync::Mutex<Option<BrowserSession>>,
    pub tools: Registry,
    pub session_tool_policy: RwLock<(Option<Vec<String>>, Vec<String>)>,
    pub mcp: Vec<Arc<crate::mcp::McpClient>>,
    pub frontend: Arc<dyn Frontend>,
    pub rules: RwLock<Rules>,
    pub interactive: bool,
    pub skills: RwLock<Vec<Skill>>,
    pub output_styles: RwLock<Vec<crate::context::OutputStyle>>,
    /// Selected output style (name), switchable with /output-style.
    pub output_style: RwLock<Option<String>>,
    /// Quiet mode (`optimize.quiet`, `--quiet`, `/quiet`): the model is told not
    /// to narrate between tool calls and to end with one short summary.
    pub quiet: AtomicBool,
    pub agents: RwLock<Vec<AgentDef>>,
    pub commands: RwLock<Vec<CustomCommand>>,
    /// File snapshots per user turn, for /rewind.
    pub checkpoints: Mutex<Vec<Checkpoint>>,
    /// The per-prompt working-tree snapshot, taken in the background; tools
    /// that change files wait for it first.
    pub pending_snapshot: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Where this session's shadow snapshots live, fixed when the session starts.
    pub history_dir: PathBuf,
    /// The user's shell setup for Bash commands, captured on first use.
    pub shell_snapshot: crate::shell::snapshot::Slot,
    pub rewind_undo: Mutex<Option<String>>,
    /// What /undo and /rewind took back, newest last, for /redo. Cleared by
    /// the next prompt.
    pub redo: Mutex<Vec<Redo>>,
    history_warned: AtomicBool,
    #[cfg(test)]
    snapshot_attempts: AtomicU64,
    /// Test-only free space for the disk warning, in bytes.
    #[cfg(test)]
    pub(crate) disk_free_override: AtomicU64,
    /// Messages the user typed while a turn was running.
    pub inbox: Mutex<Vec<Queued>>,
    /// The conversation as of the main agent's latest model call, so `/btw`
    /// can answer mid-turn without waiting for the agent.
    pub conversation: RwLock<Arc<Vec<Message>>>,
    /// `/btw` questions and answers this session (kept out of the conversation).
    pub btw: Mutex<Vec<(String, String)>>,
    /// Latest account rate limits reported by the provider (ChatGPT plans).
    pub rate_limits: Mutex<Option<crate::llm::limits::Snapshot>>,
    /// Context to add to the next user message (e.g. SessionStart hook output).
    pub pending_context: Mutex<Vec<String>>,
    /// What the human typed this session, newest last: the auto-mode
    /// classifier's only source of the user's intent, for the main agent and
    /// its subagents alike. See [`Env::record_user_request`].
    user_requests: Mutex<Vec<String>>,
    /// Connected editor (VS Code extension), if any.
    ide: RwLock<Option<Arc<crate::ide::Ide>>>,
    /// Current session id and transcript path (for hooks).
    pub session_info: Mutex<Option<(String, PathBuf)>>,
    /// Directories whose nested instruction files were already loaded.
    instruction_dirs: Mutex<std::collections::HashSet<PathBuf>>,
    /// Directory the session started in (the project root).
    pub root: PathBuf,
    /// The project the session works in now: `root`, or where `/cd` moved it.
    /// Bash's cwd is kept inside it (or an added directory).
    project: RwLock<PathBuf>,
    /// Subagents running (or finished) in the background.
    pub bg_agents: Mutex<Vec<BgAgent>>,
    /// Subagents' live activity and transcripts, for the agents panel.
    pub live_agents: live::Registry,
    /// Background-agent reports waiting to be delivered to the main agent.
    pub notifications: Mutex<Vec<String>>,
    /// Signalled whenever a background agent finishes.
    pub bg_done: tokio::sync::Notify,
    /// Set by a hook returning `continue: false`; ends the current turn.
    pub halt: Mutex<Option<String>>,
    /// `/verify full|related`: overrides `[guardrails] scope` for this session.
    pub verify_scope: RwLock<Option<crate::guardrails::Scope>>,
    /// State for hook events that run outside a turn (watchers, model switches, ...).
    pub hook_state: crate::hooks::HookState,
    /// What invoked skills changed for the current turn (`allowed-tools`, `model`, `effort`).
    pub skill_turn: Mutex<crate::commands::SkillTurn>,
    /// `/loop`, ScheduleWakeup and CronCreate tasks for this session.
    pub scheduler: crate::cron::Scheduler,
    /// `/autocompact`: 0 = the configured threshold, `u64::MAX` = off,
    /// otherwise compact once the context reaches this many tokens.
    pub autocompact: AtomicU64,
}

/// One /undo or /rewind, kept so /redo can put it back.
#[derive(Debug, Clone)]
pub struct Redo {
    /// Checkpoints the rewind removed (none for a code-only rewind).
    checkpoints: Vec<Checkpoint>,
    /// Messages the rewind removed (none for a code-only rewind).
    messages: Vec<Message>,
    /// The conversation length right after the rewind; /redo needs it unchanged.
    msg_len: usize,
    /// Shadow snapshot of the files as they were before the rewind.
    tree: Option<String>,
    /// Without a shadow snapshot: each restored file's prior contents (None = absent).
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
    prompt: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Checkpoint {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub timestamp: String,
    #[serde(default)]
    pub tree: Option<String>,
    /// Index in the message list where this turn's user message starts.
    pub msg_index: usize,
    pub prompt: String,
    /// Original contents of files changed during the turn (None = didn't exist).
    #[serde(with = "checkpoint_files")]
    pub files: HashMap<PathBuf, Option<Vec<u8>>>,
}

/// Checkpoint file contents in the session file: base64 rather than a JSON
/// number per byte, and nothing over 2 MB (kept in memory for this session;
/// after a resume, rewind falls back to the working-tree snapshot).
mod checkpoint_files {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::HashMap;
    use std::path::PathBuf;

    const MAX_PERSISTED: usize = 2 * 1024 * 1024;

    #[derive(Serialize, Deserialize)]
    #[serde(untagged)]
    enum Stored {
        B64(String),
        Bytes(Vec<u8>),
    }

    pub fn serialize<S: Serializer>(
        files: &HashMap<PathBuf, Option<Vec<u8>>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        let b64 = base64::engine::general_purpose::STANDARD;
        let out: HashMap<&PathBuf, Option<Stored>> = files
            .iter()
            .filter(|(_, v)| v.as_ref().is_none_or(|b| b.len() <= MAX_PERSISTED))
            .map(|(k, v)| (k, v.as_ref().map(|b| Stored::B64(b64.encode(b)))))
            .collect();
        out.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<HashMap<PathBuf, Option<Vec<u8>>>, D::Error> {
        let b64 = base64::engine::general_purpose::STANDARD;
        let raw: HashMap<PathBuf, Option<Stored>> = HashMap::deserialize(d)?;
        Ok(raw
            .into_iter()
            .filter_map(|(k, v)| match v {
                None => Some((k, None)),
                Some(Stored::B64(s)) => b64.decode(s).ok().map(|b| (k, Some(b))),
                // Sessions saved before this format.
                Some(Stored::Bytes(b)) => Some((k, Some(b))),
            })
            .collect())
    }
}

pub struct EnvInit {
    pub config: Config,
    pub provider: Provider,
    pub model: String,
    pub mode: Mode,
    pub vision: bool,
    pub cwd: PathBuf,
    pub tools: Registry,
    pub mcp: Vec<Arc<crate::mcp::McpClient>>,
    pub frontend: Arc<dyn Frontend>,
    pub interactive: bool,
}

/// Runs MCP servers' sampling requests on the session's current model: no
/// tools, counted against the session's budget like any other call.
struct EnvSampler(std::sync::Weak<Env>);

#[async_trait::async_trait]
impl crate::mcp::Sampler for EnvSampler {
    async fn sample(&self, r: crate::mcp::SampleRequest) -> Result<(String, String)> {
        let env = self
            .0
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("the session has ended"))?;
        env.accounting.check()?;
        let model = env.model();
        let (provider, name) = env.resolve(&model);
        let mut messages: Vec<llm::Message> = r
            .system
            .into_iter()
            .map(|s| llm::Message::system(&s))
            .collect();
        messages.extend(r.messages.into_iter().map(|(role, text)| {
            if role == "assistant" {
                llm::Message::assistant(&text)
            } else {
                llm::Message::user(&text)
            }
        }));
        let req = ChatRequest {
            model: name,
            messages,
            tools: vec![],
            think: Some(false),
            effort: None,
            temperature: r.temperature,
            max_tokens: Some(r.max_tokens),
            json: false,
        };
        let resp = provider.chat(&req, &mut |_| {}).await?;
        Ok((resp.text, model))
    }
}

impl Env {
    pub fn new(i: EnvInit) -> Arc<Env> {
        // Organization policy can rule bypass out, however the session was started.
        let start_mode = if i.mode == Mode::Bypass && i.config.policy_no_bypass {
            Mode::Default
        } else {
            i.mode
        };
        crate::permissions::set_project_root(&i.cwd);
        let fe = i.frontend.clone();
        crate::resources::init(&i.config.resources, move |msg| fe.emit(Event::Warn(msg)));
        crate::compat::set_active_plugins(crate::compat::plugins_with_compat(
            &i.cwd,
            crate::config::is_trusted(&i.cwd),
            i.config.compat.claude,
        ));
        crate::permissions::set_extra_dirs(&i.config.permissions.additional_directories, &i.cwd);
        // A verify comparison run that Chanto was killed during left this
        // turn's changes saved aside; put them back.
        crate::auth::set_config_account(i.config.account.clone());
        let restored = crate::guardrails::recover_put_backs(
            &crate::config::home_dir().join("history"),
            &i.cwd,
        );
        if !restored.is_empty() {
            i.frontend.emit(Event::Warn(format!(
                "Restored {} file(s) that an interrupted verify run had swapped for their earlier versions.",
                restored.len()
            )));
        }
        // MCP servers may ask the user for input (elicitation); in print mode
        // nobody is there to answer, so they're declined.
        if i.interactive {
            for c in &i.mcp {
                c.set_frontend(i.frontend.clone());
            }
        }
        let rules = Rules::new(
            &i.config.permissions.allow,
            &i.config.permissions.deny,
            &i.config.permissions.ask,
        );
        let think = i.config.think.unwrap_or(true);
        let effort = i.config.effort.clone().filter(|e| {
            let ok = crate::llm::EFFORTS.contains(&e.as_str());
            if !ok {
                i.frontend.emit(Event::Warn(format!(
                    "Ignoring effort = \"{e}\": use one of {}.",
                    crate::llm::EFFORTS.join(", ")
                )));
            }
            ok
        });
        // Independent directory scans (project, home and plugin folders).
        let (skills, output_styles, agents, commands) = std::thread::scope(|sc| {
            let skills = sc.spawn(|| crate::context::load_skills(&i.cwd));
            let styles = sc.spawn(|| crate::context::load_output_styles(&i.cwd));
            let agents = sc.spawn(|| crate::context::load_agents(&i.cwd));
            let commands = crate::context::load_commands(&i.cwd);
            (
                skills.join().unwrap_or_default(),
                styles.join().unwrap_or_default(),
                agents.join().unwrap_or_default(),
                commands,
            )
        });
        let env = Arc::new(Env {
            accounting: Arc::new(crate::cost::Accounting::new(&i.config)),
            spend: crate::spend::Guard::new(&i.config.spend),
            skills: RwLock::new(skills),
            output_styles: RwLock::new(output_styles),
            output_style: RwLock::new(
                i.config
                    .output_style
                    .clone()
                    .filter(|s| !s.eq_ignore_ascii_case("default")),
            ),
            quiet: AtomicBool::new(i.config.optimize.quiet()),
            agents: RwLock::new(agents),
            commands: RwLock::new(commands),
            checkpoints: Default::default(),
            pending_snapshot: Default::default(),
            history_dir: crate::config::home_dir().join("history"),
            shell_snapshot: Default::default(),
            rewind_undo: Default::default(),
            redo: Default::default(),
            history_warned: AtomicBool::new(false),
            #[cfg(test)]
            snapshot_attempts: AtomicU64::new(0),
            #[cfg(test)]
            disk_free_override: AtomicU64::new(0),
            inbox: Default::default(),
            conversation: Default::default(),
            btw: Default::default(),
            rate_limits: Default::default(),
            pending_context: Default::default(),
            user_requests: Default::default(),
            ide: Default::default(),
            session_info: Default::default(),
            halt: Default::default(),
            verify_scope: Default::default(),
            root: i.cwd.clone(),
            project: RwLock::new(crate::permissions::normalize(&i.cwd)),
            bg_agents: Default::default(),
            live_agents: Default::default(),
            notifications: Default::default(),
            bg_done: tokio::sync::Notify::new(),
            instruction_dirs: Default::default(),
            hooks: RwLock::new(i.config.hooks.clone()),
            auto_mode: RwLock::new((
                i.config.permissions.auto_mode_rules.clone(),
                i.config.permissions.fast_path,
            )),
            config: i.config,
            provider: RwLock::new(i.provider),
            hook_state: crate::hooks::HookState::new(&i.model),
            skill_turn: Mutex::new(Default::default()),
            scheduler: Default::default(),
            autocompact: AtomicU64::new(0),
            model: RwLock::new(i.model),
            mode: RwLock::new(start_mode),
            mode_before_grant: Mutex::new(None),
            think: AtomicBool::new(think),
            effort: RwLock::new(effort),
            vision: AtomicBool::new(i.vision),
            text_tools: AtomicBool::new(false),
            cwd: RwLock::new(i.cwd),
            read_files: Default::default(),
            todos: Default::default(),
            shells: Default::default(),
            shell_counter: AtomicU64::new(1),
            browser: tokio::sync::Mutex::new(None),
            tools: i.tools,
            session_tool_policy: RwLock::new((None, vec![])),
            mcp: i.mcp,
            frontend: i.frontend,
            rules: RwLock::new(rules),
            interactive: i.interactive,
        });
        // MCP sampling runs on the user's model, after they approve each request.
        if env.interactive {
            for c in &env.mcp {
                c.set_sampler(Arc::new(EnvSampler(Arc::downgrade(&env))));
            }
        }
        env
    }

    /// Estimated cost of writing `tokens` of prompt cache on `spec`, and how
    /// it was priced (`configured`, `catalog` or `default`, then 0).
    pub fn estimate_cache_write(&self, spec: &str, tokens: u64) -> (f64, &'static str) {
        let (p, model) = self.resolve_untracked(spec);
        let usage = llm::Usage {
            prompt_tokens: tokens,
            cache_write_tokens: tokens,
            ..Default::default()
        };
        let (cost, kind) = self.accounting.estimate(&p.billing_id(), &model, &usage);
        (cost.unwrap_or(0.0), kind)
    }

    /// Record text the human typed (a prompt, a message to a running agent),
    /// without the files it mentions or the context appended to it, which are
    /// data and could carry instructions. Kept for the auto-mode classifier.
    pub fn record_user_request(&self, typed: &str) {
        let text = crate::permissions::without_context(typed).trim();
        if text.is_empty() {
            return;
        }
        let mut v = self.user_requests.lock().unwrap();
        v.push(text.to_string());
        let n = v.len();
        v.drain(..n.saturating_sub(8));
    }

    /// What the human typed this session, oldest first (the last few).
    pub fn user_requests(&self) -> Vec<String> {
        self.user_requests.lock().unwrap().clone()
    }

    pub fn emit(&self, e: Event) {
        match &e {
            Event::ToolStart {
                id,
                label,
                parent: Some(p),
            } => self.live_agents.tool_start(p, id, label),
            Event::ToolEnd {
                id,
                display,
                is_error,
                parent: Some(p),
                ..
            } => self.live_agents.tool_end(p, id, display, *is_error),
            _ => {}
        }
        if let Event::Error(_) = &e {
            self.accounting
                .log("error", "error", serde_json::json!({"status": "error"}));
        }
        self.frontend.emit(e)
    }

    /// Kill background shells and any browser we launched.
    pub async fn shutdown(&self) {
        self.session_end("exit").await;
        // Stop background agents; drop worktrees they left unchanged.
        let worktrees: Vec<crate::worktree::Worktree> = self
            .bg_agents
            .lock()
            .unwrap()
            .iter()
            .inspect(|b| b.cancel.cancel())
            .filter_map(|b| b.worktree.clone())
            .collect();
        for w in worktrees {
            w.cleanup_if_unchanged();
        }
        crate::tools::bash::kill_all(self);
        if let Some(sess) = self.browser.lock().await.take() {
            // Clear the "controlled by Chanto" badge we put on its dock icon.
            let _ = crate::tools::browser::set_dock_badge(sess.port, "").await;
        }
        crate::tools::browser::kill_launched();
        crate::lsp::shutdown();
    }
    pub fn cwd(&self) -> PathBuf {
        self.cwd.read().unwrap().clone()
    }
    /// AGENTS.md / CLAUDE.md files in subdirectories the tool call touches
    /// that haven't been shown to the model yet.
    pub fn nested_instructions(&self, args: &serde_json::Value) -> Vec<(PathBuf, String)> {
        let Some(p) = ["file_path", "path", "notebook_path"]
            .iter()
            .find_map(|k| args.get(*k).and_then(|v| v.as_str()))
        else {
            return vec![];
        };
        let root = crate::permissions::project_root().unwrap_or_else(|| self.cwd());
        let file = crate::util::resolve(&self.cwd(), p);
        // A directory argument (Ls, Glob, Grep) counts as touching files inside it.
        let file = if file.is_dir() { file.join("_") } else { file };
        crate::context::nested_instructions(
            &root,
            &file,
            &mut self.instruction_dirs.lock().unwrap(),
        )
    }
    pub fn ide(&self) -> Option<Arc<crate::ide::Ide>> {
        self.ide.read().unwrap().clone().filter(|i| i.alive())
    }
    pub fn set_ide(&self, ide: Option<Arc<crate::ide::Ide>>) {
        *self.ide.write().unwrap() = ide;
    }
    pub fn session_id(&self) -> String {
        self.session_info
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.0.clone())
            .unwrap_or_default()
    }
    pub fn transcript_path(&self) -> Option<PathBuf> {
        self.session_info
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.1.clone())
    }
    /// Run SessionStart hooks; their output is added to the next prompt.
    pub async fn session_start(&self, source: &str) {
        crate::hooks::prepare_env_file(self);
        if source == "resume" {
            crate::hooks::model_restored(self).await;
        }
        if crate::hooks::has(self, "InstructionsLoaded") {
            let reason = if source == "compact" {
                "compact"
            } else {
                "session_start"
            };
            let files = crate::context::instruction_files(&self.cwd())
                .into_iter()
                .map(|(p, _)| (p, reason))
                .collect();
            crate::hooks::instructions_loaded(self, files, None);
        }
        let r = crate::hooks::notify(
            self,
            "SessionStart",
            Some(source),
            serde_json::json!({"source": source}),
        )
        .await;
        if let Some(c) = r.context_text() {
            self.pending_context.lock().unwrap().push(c);
        }
        crate::hooks::set_watch_paths(self, &r);
        crate::hooks::start_file_watcher(self);
    }
    pub async fn session_end(&self, reason: &str) {
        crate::hooks::notify(
            self,
            "SessionEnd",
            Some(reason),
            serde_json::json!({"reason": reason}),
        )
        .await;
    }
    /// The project root the session works in now (normalized).
    pub fn project(&self) -> PathBuf {
        self.project.read().unwrap().clone()
    }
    /// `/cd`: the session's project is now `p`.
    pub fn set_project(&self, p: &Path) {
        *self.project.write().unwrap() = crate::permissions::normalize(p);
        crate::permissions::set_project_root(p);
    }
    pub fn set_cwd(&self, p: PathBuf) {
        let _ = std::env::set_current_dir(&p);
        *self.cwd.write().unwrap() = p;
    }
    pub fn model(&self) -> String {
        self.model.read().unwrap().clone()
    }
    pub fn set_model(&self, m: &str) {
        *self.model.write().unwrap() = m.to_string();
    }
    pub fn provider(&self) -> Provider {
        let mut p = self.provider.read().unwrap().clone();
        p.accounting = Some(self.accounting.clone());
        p
    }

    /// Resolve a model spec ("model" or "provider/model") to a provider and model name.
    pub fn resolve(&self, spec: &str) -> (Provider, String) {
        let (mut p, model) = self.resolve_untracked(spec);
        p.accounting = Some(self.accounting.clone());
        p.stream_idle = self.config.stream_idle();
        if let Some((prefix, _)) = spec.split_once('/') {
            // Cloud platforms bill by platform, whatever the section is called.
            if let Some(id) = p.kind.cloud_id() {
                p.billing_provider = Some(id.into());
            } else if self.config.providers.contains_key(prefix)
                || crate::providers::find(prefix).is_some()
                || matches!(prefix, "ollama" | "lmstudio" | "llamacpp")
            {
                p.billing_provider = Some(prefix.into());
            }
        }
        (p, model)
    }

    fn resolve_untracked(&self, spec: &str) -> (Provider, String) {
        if let Some((prefix, rest)) = spec.split_once('/') {
            if let Some(pc) = self.config.providers.get(prefix) {
                let ctx = if pc.kind.is_hosted() {
                    llm::hosted_context(pc.kind, rest)
                } else {
                    self.provider().num_ctx
                };
                return (pc.provider(ctx), rest.to_string());
            }
            // Catalog providers (`groq/<model>`, `chatgpt/<model>`, …) work with just a key or login.
            // anthropic/ and openai/ stay local when the default server is a custom one that
            // names its models that way (LM Studio, vLLM).
            if (self.config.allows_builtin_hosted() || !matches!(prefix, "openai" | "anthropic"))
                && let Some(p) = llm::builtin_provider(prefix, rest)
            {
                return (p, rest.to_string());
            }
        }
        if let Some((p, _)) = spec.split_once('/')
            && matches!(p, "ollama" | "lmstudio" | "llamacpp")
        {
            let (provider, model, _) = crate::doctor::resolve(&self.config, spec);
            return (provider, model);
        }
        (self.provider(), spec.to_string())
    }

    pub fn mode(&self) -> Mode {
        *self.mode.read().unwrap()
    }
    /// Quiet mode is on (see [`Env::quiet`]).
    pub fn quiet(&self) -> bool {
        self.quiet.load(Ordering::Relaxed)
    }
    pub fn set_mode(&self, m: Mode) {
        if m == Mode::Bypass && self.config.policy_no_bypass {
            self.emit(Event::Warn(
                "Bypass permissions mode is disabled by your organization's managed settings."
                    .into(),
            ));
            return;
        }
        *self.mode.write().unwrap() = m;
        *self.mode_before_grant.lock().unwrap() = None;
        self.emit(Event::ModeChanged(m));
    }
    /// Accept edits because the user said "always allow" to an edit, which
    /// (unlike choosing the mode) is a grant for this project only.
    pub fn grant_accept_edits(&self) {
        let before = self.mode();
        if before == Mode::AcceptEdits {
            return;
        }
        self.set_mode(Mode::AcceptEdits);
        if self.mode() == Mode::AcceptEdits {
            *self.mode_before_grant.lock().unwrap() = Some(before);
        }
    }
    /// For `/cd`: undo [`Self::grant_accept_edits`] if it's what put the
    /// session in accept-edits mode. Returns the mode restored.
    pub fn revoke_accept_edits_grant(&self) -> Option<Mode> {
        let before = self.mode_before_grant.lock().unwrap().take()?;
        if self.mode() != Mode::AcceptEdits {
            return None;
        }
        self.set_mode(before);
        Some(before)
    }
    pub fn think(&self) -> bool {
        self.think.load(Ordering::Relaxed)
    }
    pub fn set_think(&self, b: bool) {
        self.think.store(b, Ordering::Relaxed)
    }
    /// Reasoning effort for the main model; `None` leaves it to the provider.
    pub fn effort(&self) -> Option<String> {
        self.effort.read().unwrap().clone()
    }
    pub fn set_effort(&self, e: Option<String>) {
        *self.effort.write().unwrap() = e;
    }
    pub fn vision(&self) -> bool {
        self.vision.load(Ordering::Relaxed)
    }
    pub fn set_vision(&self, b: bool) {
        self.vision.store(b, Ordering::Relaxed)
    }
    pub fn next_shell_id(&self) -> u64 {
        self.shell_counter.fetch_add(1, Ordering::Relaxed)
    }
    pub fn mark_read(&self, p: &Path) {
        if let Ok(m) = std::fs::metadata(p).and_then(|m| m.modified()) {
            self.read_files
                .lock()
                .unwrap()
                .insert(permissions::normalize(p), m);
        }
    }
    pub fn read_time(&self, p: &Path) -> Option<SystemTime> {
        self.read_files
            .lock()
            .unwrap()
            .get(&permissions::normalize(p))
            .copied()
    }
    /// Record a file's current contents before the first modification this turn.
    pub fn snapshot_file(&self, p: &Path) {
        let mut cps = self.checkpoints.lock().unwrap();
        if let Some(cp) = cps.last_mut() {
            let key = permissions::normalize(p);
            cp.files.entry(key).or_insert_with(|| std::fs::read(p).ok());
        }
    }

    /// Wait for the background per-prompt snapshot, if one is running.
    pub async fn wait_snapshot(&self) {
        let h = self.pending_snapshot.lock().unwrap().take();
        if let Some(h) = h {
            let _ = h.await;
        }
    }

    /// The shadow history for this session's project.
    pub fn shadow(&self) -> Result<crate::history::Shadow> {
        crate::history::Shadow::in_dir(&self.history_dir, &self.root, &self.config.history)
    }

    pub fn shadow_snapshot(&self) -> Option<String> {
        #[cfg(test)]
        self.snapshot_attempts.fetch_add(1, Ordering::Relaxed);
        match self.shadow().and_then(|s| s.snapshot()) {
            Ok(tree) => Some(tree),
            Err(e) => {
                if !self.history_warned.swap(true, Ordering::Relaxed) {
                    let h = &self.config.history;
                    let e = e.to_string();
                    let (why, fix) = if e.contains("time budget") {
                        (
                            format!(
                                "snapshotting this project took longer than {} ms",
                                h.timeout_ms
                            ),
                            format!("raise `[history] timeout_ms` (now {})", h.timeout_ms),
                        )
                    } else if e.contains("size budget") {
                        (
                            format!(
                                "this project is larger than {} MB",
                                h.max_snapshot_bytes / (1024 * 1024)
                            ),
                            "raise `[history] max_snapshot_bytes`".to_string(),
                        )
                    } else {
                        (format!("it couldn't be snapshotted ({e})"), String::new())
                    };
                    let fix = if fix.is_empty() {
                        String::new()
                    } else {
                        format!(" To include those, {fix} in ~/.chanto/config.toml.")
                    };
                    self.emit(Event::Warn(format!("/rewind here covers files Chanto edits, but not changes made by shell commands: {why}.{fix}")));
                }
                None
            }
        }
    }

    pub fn context_max(&self) -> u64 {
        self.resolve(&self.model()).0.num_ctx as u64
    }

    /// Condense long content against a question with a single model call.
    pub async fn condense(&self, content: &str, prompt: &str, max_chars: usize) -> Result<String> {
        let budget = (self.context_max() as usize * 3)
            .saturating_sub(8000)
            .max(8000);
        let content = crate::util::truncate(content, budget);
        let req = ChatRequest {
            model: self.model(),
            messages: vec![
                Message::system(
                    "You extract information from web page content. Answer the request using only the content provided. Quote code, commands, and API details verbatim. Be concise but complete; include relevant URLs.",
                ),
                Message::user(format!(
                    "<content>\n{content}\n</content>\n\nRequest: {prompt}"
                )),
            ],
            tools: vec![],
            think: Some(false),
            effort: None,
            temperature: Some(0.2),
            max_tokens: Some((max_chars / 3) as u32),
            json: false,
        };
        let (provider, model) = self.resolve(&self.model());
        let mut req = req;
        req.model = model;
        let r = provider.chat(&req, &mut |_| {}).await?;
        Ok(r.text)
    }
}

// ------------------------------------------------------------------- agent

pub struct Agent {
    pub env: Arc<Env>,
    pub messages: Vec<Message>,
    pub depth: usize,
    /// Tool call id of the Task that spawned this subagent.
    pub parent: Option<String>,
    /// Messages the user sent this (background) subagent while it runs; it
    /// reads them between steps.
    pub followups: Arc<Mutex<Vec<String>>>,
    /// The next `run_turn` prompt is what the human typed (in the TUI, the
    /// REPL, the web UI or phone, an ACP client's prompt, `-p`). Set only by
    /// those frontends, right before the turn, and taken by it: any other
    /// prompt (a goal, team or outcome step, a run's task or report, an
    /// expanded slash command, a repair prompt, an MCP client's) is the
    /// agent's own task to the auto-mode classifier, never the user's word.
    pub typed_input: bool,
    /// The latest prompt this agent got that the user didn't type (a Task
    /// prompt, a scheduled task): shown to the auto-mode classifier as the
    /// agent's task, never as the user's request.
    task_prompt: Option<String>,
    tool_filter: Option<Vec<String>>,
    /// What this agent has read, for "unchanged since last read" (see [`crate::tools::fs::ReadLog`]).
    reads: Arc<crate::tools::fs::ReadLog>,
    /// Tools removed even when `tool_filter` allows them (`disallowedTools`).
    tool_deny: Vec<String>,
    model_override: Option<String>,
    pub total_in: u64,
    pub total_out: u64,
    /// Prompt tokens the provider served from cache (a subset of `total_in`).
    pub total_cached: u64,
    last_prompt_tokens: u64,
    msgs_at_last_usage: usize,
    consecutive_blocks: u32,
    /// `additionalContext` from the PreToolUse hook for the action being
    /// authorized; delivered with that tool's result.
    hook_context: Option<String>,
    /// The model the system prompt was last rendered for. `/model` can run while
    /// a turn holds the agent lock, so the prompt is rebuilt when they diverge.
    system_for_model: Option<String>,
    /// Whether that prompt has quiet mode's instruction (`/quiet` also runs mid-turn).
    system_quiet: bool,
    /// Edits or non-read-only commands run during the current turn (verified mode).
    turn_mutations: u32,
    /// Session saves: when the last one started, a counter for ordering
    /// background writes, and the newest generation written.
    last_save: Option<Instant>,
    save_gen: Arc<std::sync::atomic::AtomicU64>,
    save_written: Arc<Mutex<u64>>,
    /// Bumped on every change to files (edits, non-read-only commands).
    edit_seq: u64,
    /// Extra room for the output cap after a response was cut off at it with
    /// nothing usable in it (the whole budget went to thinking). Doubles per
    /// consecutive truncation; reset as soon as a real response arrives.
    output_cap_boost: u32,
    /// How much less to think after responses spent the whole output cap on
    /// reasoning: 1 asks for low effort, 2 turns thinking off for the call, so
    /// the model has to write or act. Reset when a real response arrives.
    think_less: u8,
    /// `optimize.defer_tools`: deferred built-ins sent upfront anyway, fixed
    /// for the session once its first turn starts (`tool_search::upfront_builtins`).
    upfront_builtins: Option<Vec<String>>,
    /// Deferred built-ins already recorded as used in this project.
    recorded_builtins: std::collections::HashSet<String>,
    /// The agent's own shell commands that looked like checks, with edit_seq at
    /// the time: (command, seq, passed, output). The verify gate accepts a
    /// covering *passing* run with no edits since, and reuses a covering failing
    /// run instead of repeating it (its failures are then split against the
    /// baseline like a fresh run's).
    check_runs: Vec<(String, u64, bool, String)>,
    /// Low disk space was already reported this turn.
    disk_warned: bool,
    /// Read-only commands of this turn that hit their time limit: command,
    /// edit sequence, seconds.
    check_timeouts: Vec<(String, u64, u64)>,
    /// Verified mode: the checks run before any change, and their results.
    baseline_task: Option<tokio::task::JoinHandle<Vec<crate::guardrails::CheckResult>>>,
    baseline: Option<Vec<crate::guardrails::CheckResult>>,
    /// This turn changed files in ways the checkpoint doesn't record (shell
    /// commands, external tools, subagents), and when the turn started.
    turn_untracked_writes: bool,
    turn_started: Option<SystemTime>,
    /// How long each full check took when it last ran in this session.
    full_secs: HashMap<String, f32>,
    pub(crate) system_extra: Option<String>,
    /// `--system-prompt` / `--agent`: replaces Chanto's base instructions.
    system_base: Option<String>,
    pub session: Option<crate::session::Session>,
    /// When set, turns are run by an external ACP agent (`chanto acp-client`)
    /// instead of the model loop. Events and permission prompts are the same,
    /// so the TUI, `/remote` and the HTTP server all work unchanged.
    pub acp: Option<crate::acp_client::AcpAgent>,
    /// Working directory when isolated in a git worktree.
    pub cwd_override: Option<PathBuf>,
    /// (main checkout root, worktree root): absolute paths into the main
    /// checkout are redirected into the worktree.
    worktree_map: Option<(PathBuf, PathBuf)>,
    /// Cancellation token of the turn in progress.
    cur_cancel: Option<CancellationToken>,
    /// Contents of files this agent wrote last turn (to notice the user's fix-ups).
    agent_wrote: HashMap<PathBuf, String>,
    /// Tool results from this turn, used by the optional provenance check.
    turn_evidence: String,
    /// Outcome mode: verify this turn with these extra checks and locked dirs.
    forced_verify: Option<(Vec<String>, Vec<PathBuf>)>,
    /// Usage counters for /insights.
    pub stats: crate::insights::Stats,
    /// Verified mode's verdict for the last turn: (passed, evidence or the
    /// first line of the failure). None when the gate didn't run.
    pub last_verify: Option<(bool, String)>,
    /// The active goal's directory: its objective, criteria and progress are
    /// re-injected when the conversation is compacted.
    pub goal: Option<PathBuf>,
    message_redactor: crate::secrets::MessageRedactor,
    /// The `fallback_model` entry serving the rest of this turn, after the
    /// model was overloaded or unavailable (None: the session's model).
    turn_fallback: Option<usize>,
    /// The model that served the last request: the conversation fit its
    /// window, so it can summarize before a smaller one takes over.
    last_model: Option<String>,
    /// Settings from a subagent definition's frontmatter.
    pub(crate) sub: SubagentSettings,
    /// The id of the session `/clear` most recently archived, so `/rewind` can
    /// undo an accidental clear by resuming it. Taken when restored.
    pub(crate) cleared_session: Option<String>,
}

/// What a subagent definition's frontmatter sets for the agent running it.
#[derive(Default)]
pub(crate) struct SubagentSettings {
    /// `permissionMode` (see [`Agent::mode`] for when it applies).
    pub mode: Option<Mode>,
    /// `permissionMode: dontAsk`: what would prompt is denied.
    pub dont_ask: bool,
    pub effort: Option<String>,
    /// `maxTurns`: model calls before it stops with a partial report.
    pub max_turns: Option<u32>,
    /// Inline `mcpServers`, connected for this agent only.
    pub mcp: Vec<Arc<crate::mcp::McpClient>>,
    /// Frontmatter `hooks`, active while this agent runs.
    pub hooks: Option<Arc<crate::config::HooksConfig>>,
    /// `omitClaudeMd`: no instruction files in its prompt.
    pub omit_instructions: bool,
}

/// Files changed during a turn (verified mode).
struct TurnChanges {
    files: Vec<crate::guardrails::Changed>,
    /// Contents when the turn started (None: the file didn't exist).
    originals: HashMap<PathBuf, Option<Vec<u8>>>,
    /// Every changed file's original is known.
    complete: bool,
}

#[derive(Debug)]
pub enum TurnEnd {
    Done(String),
    Interrupted,
}

impl Agent {
    pub fn new(env: Arc<Env>) -> Agent {
        Self::new_with_system_extra(env, None)
    }

    pub fn new_with_system_extra(env: Arc<Env>, system_extra: Option<String>) -> Agent {
        crate::hooks::attach(&env);
        let mut a = Agent {
            env,
            messages: vec![],
            depth: 0,
            parent: None,
            followups: Default::default(),
            typed_input: false,
            task_prompt: None,
            tool_filter: None,
            reads: Arc::default(),
            tool_deny: Vec::new(),
            model_override: None,
            total_in: 0,
            total_out: 0,
            total_cached: 0,
            last_prompt_tokens: 0,
            msgs_at_last_usage: 0,
            consecutive_blocks: 0,
            turn_mutations: 0,
            last_save: None,
            save_gen: Default::default(),
            save_written: Default::default(),
            edit_seq: 0,
            output_cap_boost: 0,
            think_less: 0,
            upfront_builtins: None,
            recorded_builtins: Default::default(),
            check_runs: Vec::new(),
            disk_warned: false,
            check_timeouts: Vec::new(),
            baseline_task: None,
            baseline: None,
            turn_untracked_writes: false,
            turn_started: None,
            full_secs: HashMap::new(),
            last_verify: None,
            system_extra,
            system_base: None,
            session: None,
            acp: None,
            cwd_override: None,
            worktree_map: None,
            cur_cancel: None,
            agent_wrote: HashMap::new(),
            turn_evidence: String::new(),
            hook_context: None,
            system_for_model: None,
            system_quiet: false,
            forced_verify: None,
            stats: Default::default(),
            goal: None,
            message_redactor: Default::default(),
            turn_fallback: None,
            last_model: None,
            sub: Default::default(),
            cleared_session: None,
        };
        a.messages = vec![Message::system(a.system_prompt())];
        // Built for this model, so the first turn doesn't build it again.
        a.system_for_model = Some(a.env.model());
        a.system_quiet = a.env.quiet();
        a
    }

    /// `/btw`: answer a question from the context so far without adding the
    /// question or its answer to the conversation, and without tools. The model
    /// still sees the current messages (redacted), so it can use what it already
    /// read, but the exchange costs no context on later turns.
    pub async fn side_question(&self, question: &str) -> Result<String> {
        Self::side_question_on(&self.env, &self.model(), &self.messages, question).await
    }

    /// Let `/btw` see the conversation as it is now (the main agent only).
    pub fn publish_conversation(&self) {
        if self.depth == 0 {
            let mut slot = self.env.conversation.write().unwrap();
            // Reuse the previous snapshot's buffers when no reader holds it
            // (the common case): `clone_from` refills the same Vec and its
            // Strings instead of allocating a fresh copy on every model call.
            // A reader mid-read still gets a stable copy — `make_mut` clones
            // first only when the Arc is shared.
            Arc::make_mut(&mut slot).clone_from(&self.messages);
        }
    }

    pub fn system_prompt(&self) -> String {
        let env = &self.env;
        let mcp_note = if !env.mcp.iter().any(|c| c.is_connected()) {
            None
        } else {
            Some(
                env.mcp
                    .iter()
                    .filter(|c| c.is_connected())
                    .map(|c| match &c.instructions() {
                        Some(i) => format!("## {}\n{}", c.name, crate::util::truncate(i, 2000)),
                        None => format!("## {} ({} tools)", c.name, c.tools().len()),
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        };
        let agents = env.agents.read().unwrap();
        let mut s = crate::context::system_prompt(&crate::context::PromptInputs {
            cwd: &self.cwd_override.clone().unwrap_or_else(|| env.cwd()),
            model: &self.model(),
            skills: &env.skills.read().unwrap(),
            agents: if self.depth == 0 { &agents } else { &[] },
            mcp_note,
            instructions: !self.sub.omit_instructions,
        });
        // Chanto's own instructions first and the per-project context last,
        // so everything before it is the same in every session
        // (`optimize.stable_prefix` sends the two apart).
        let project = s.split_off(s.find(crate::context::PROJECT_CONTEXT).unwrap_or(s.len()));
        if self.depth == 0 {
            let selected = env.output_style.read().unwrap().clone();
            if let Some(style) = selected.and_then(|n| {
                env.output_styles
                    .read()
                    .unwrap()
                    .iter()
                    .find(|s| s.name.eq_ignore_ascii_case(&n))
                    .cloned()
            }) {
                s.push_str(&format!(
                    "\n\n# Output style: {}\n{}\n",
                    style.name, style.body
                ));
            }
        }
        match &self.system_base {
            // `--system-prompt`: replaces Chanto's own instructions. The
            // environment, skills, subagents, MCP servers and instruction files
            // stay, since they're sent separately from the system prompt.
            Some(base) => s = s.replacen(crate::context::BASE_PROMPT, base.trim(), 1),
            None => s.push_str("\n\nUse Memory to remember durable preferences, corrections, and non-obvious project facts. Read indexed memories when relevant; correct or delete wrong memories. Never store secrets, transient task state, or information readily derived from repository files."),
        }
        // `optimize.concise_prose`: config is fixed for the session, so the
        // prefix stays stable. The Concise style already says the same.
        let concise_style = self.depth == 0
            && env
                .output_style
                .read()
                .unwrap()
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case("concise"));
        if env.config.optimize.concise_prose() && !concise_style {
            s.push_str("\n\n");
            s.push_str(crate::context::CONCISE_PROSE);
        }
        // Quiet mode: fixed text that changes only when `/quiet` switches it
        // (which rebuilds the prompt once, as `/output-style` does).
        if env.quiet() {
            s.push_str("\n\n");
            s.push_str(crate::context::QUIET);
        }
        // `optimize.spec_check`: fixed for the session, so the prefix is stable.
        if env.config.optimize.spec_check() {
            s.push_str("\n\n");
            s.push_str(crate::context::SPEC_CHECK);
        }
        if let Some(x) = &self.system_extra {
            s.push_str("\n\n");
            s.push_str(x);
        }
        s.push_str(&project);
        s
    }

    pub fn enable_customization(&mut self) {
        let guide = crate::customize::extension_guide();
        let extra = self.system_extra.get_or_insert_with(String::new);
        // Projects carrying the guide under its earlier heading keep it.
        if !extra.contains("# chanto extension guide")
            && !extra.contains("# Chanto extension guide")
        {
            extra.push_str("\n\n");
            extra.push_str(&guide);
        }
        self.refresh_system_prompt();
    }

    /// `--system-prompt`: replace Chanto's base instructions (tools keep working:
    /// their definitions are sent separately, or added by the text protocol).
    pub fn set_system_base(&mut self, base: Option<String>) {
        self.system_base = base;
        self.refresh_system();
    }

    /// `--agent <name>`: run the main conversation as that subagent
    /// definition: its prompt replaces Chanto's base instructions, and its
    /// tools and model apply (an explicit `--model` still wins).
    pub fn adopt_definition(&mut self, def: &AgentDef, keep_model: bool) {
        self.tool_filter = def.tools.clone();
        self.tool_deny = def.disallowed_tools.clone();
        if let Some(m) = def.model.as_deref().filter(|_| !keep_model) {
            let m = subagent_model(&self.env, &self.env.model(), m);
            self.env.set_model(&m);
        }
        // permissionMode, maxTurns, effort, hooks, memory, skills (inline MCP
        // servers connect in `connect_agent_mcp`).
        let prompt = self.adopt_frontmatter(def, false);
        if self.system_base.is_none() {
            self.system_base = Some(prompt);
        } else {
            let extra = self.system_extra.get_or_insert_with(String::new);
            *extra = format!("{}\n\n{}", prompt.trim(), extra).trim().to_string();
        }
        self.refresh_system();
    }

    /// Forget files under `dir` that this agent wrote, so Chanto restoring
    /// them (locked goal criteria) isn't taken for the user's own fix-up.
    pub fn forget_written(&mut self, dir: &Path) {
        self.agent_wrote.retain(|p, _| !p.starts_with(dir));
    }

    /// Clear the conversation (keeps settings).
    pub fn reset(&mut self) {
        if let Some(acp) = &mut self.acp {
            acp.reset();
        }
        self.messages = vec![Message::system(self.system_prompt())];
        self.reads.clear();
        self.last_prompt_tokens = 0;
        self.msgs_at_last_usage = 0;
        self.stats = Default::default();
        self.cleared_session = None;
        self.task_prompt = None;
        if self.depth == 0 {
            self.env.user_requests.lock().unwrap().clear();
            self.env.todos.lock().unwrap().clear();
            self.env.checkpoints.lock().unwrap().clear();
            *self.env.rewind_undo.lock().unwrap() = None;
            self.env.redo.lock().unwrap().clear();
        }
    }

    pub fn model(&self) -> String {
        if self.depth == 0
            && let Some(m) = self.env.skill_turn.lock().unwrap().model.clone()
        {
            return m;
        }
        self.model_override
            .clone()
            .unwrap_or_else(|| self.env.model())
    }

    /// The permission mode this agent's tool calls are checked in. A
    /// subagent's `permissionMode` applies unless the main conversation is in
    /// bypass, acceptEdits or auto mode, and it can't ask for bypass.
    pub fn mode(&self) -> Mode {
        let main = self.env.mode();
        match self.sub.mode {
            Some(m)
                if !matches!(main, Mode::Bypass | Mode::AcceptEdits | Mode::Auto)
                    && m != Mode::Bypass =>
            {
                m
            }
            _ => main,
        }
    }

    /// Effort for the next model call: an invoked skill's for the main agent,
    /// a subagent definition's, the session's, or — when none is set — an
    /// adaptive default for the kind of turn this is.
    fn effort(&self) -> Option<String> {
        let skill = if self.depth == 0 {
            self.env.skill_turn.lock().unwrap().effort.clone()
        } else {
            None
        };
        skill
            .or_else(|| self.sub.effort.clone())
            .or_else(|| self.env.effort())
            .or_else(|| self.adaptive_effort())
    }

    /// `optimize.adaptive_effort` (on by default): when nothing pins an effort,
    /// spend less on mechanical follow-ups (turns whose previous assistant
    /// message only called tools and said nothing). The effort then changes
    /// between calls, which costs a provider's prompt cache at each change,
    /// but it solved more benchmark tasks than any fixed effort. Turned off
    /// by an explicit session/skill effort.
    fn adaptive_effort(&self) -> Option<String> {
        if !self.env.think() || !self.env.config.optimize.adaptive_effort() {
            return None;
        }
        let last = self
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)?;
        let mechanical = !last.tool_calls.is_empty() && last.content.trim().is_empty();
        mechanical.then(|| "low".to_string())
    }

    fn tool_allowed(&self, name: &str) -> bool {
        let policy = self.env.session_tool_policy.read().unwrap();
        if policy
            .0
            .as_ref()
            .is_some_and(|a| !a.iter().any(|n| n.eq_ignore_ascii_case(name)))
            || policy.1.iter().any(|n| {
                n.eq_ignore_ascii_case(name)
                    || (name == "apply_patch" && (n == "Edit" || n == "Write"))
            })
        {
            return false;
        }
        if self.depth > 0
            && matches!(
                name,
                "Task" | "ExitPlanMode" | "AskUserQuestion" | "TodoWrite"
            )
        {
            return false;
        }
        // With no user to answer, these only ever return their "carry on"
        // fallback; advertising them costs prompt tokens and invites a wasted
        // turn.
        if !self.env.interactive && matches!(name, "AskUserQuestion" | "ExitPlanMode") {
            return false;
        }
        let hit = |t: &str| {
            t == name
                || (t.ends_with('*') && name.starts_with(t.trim_end_matches('*')))
                || (name == "apply_patch" && t == "Edit")
        };
        // A deny entry with a specifier (`Bash(git push *)`) removes the whole tool.
        if self
            .tool_deny
            .iter()
            .any(|t| hit(t.split('(').next().unwrap_or(t).trim()))
        {
            return false;
        }
        match &self.tool_filter {
            Some(f) => f.iter().any(|t| hit(t)),
            None => true,
        }
    }

    pub fn context_used(&self) -> u64 {
        let new = &self.messages[self.msgs_at_last_usage.min(self.messages.len())..];
        if self.last_prompt_tokens == 0 {
            llm::estimate_tokens(&self.messages) + 3000
        } else {
            self.last_prompt_tokens + llm::estimate_tokens(new)
        }
    }

    fn emit(&self, e: Event) {
        if let (
            true,
            Some(p),
            Event::Usage {
                total_in,
                total_out,
                ..
            },
        ) = (self.depth > 0, &self.parent, &e)
        {
            self.env.live_agents.tokens(p, total_in + total_out);
        }
        // Subagents only surface their tool activity.
        if self.depth > 0
            && !matches!(
                e,
                Event::ToolStart { .. } | Event::ToolEnd { .. } | Event::Warn(_) | Event::Error(_)
            )
        {
            return;
        }
        self.env.emit(e)
    }

    pub(crate) fn emit_usage(&self, tps: f64) {
        self.emit(Event::Usage {
            context_used: self.context_used(),
            context_max: self.env.context_max(),
            total_in: self.total_in,
            total_out: self.total_out,
            total_cost_usd: self.env.accounting.total(),
            price_unknown: self.env.accounting.unknown(),
            tps,
        });
    }

    /// Free bytes on the filesystem the agent writes to.
    fn disk_free(&self) -> Option<u64> {
        #[cfg(test)]
        {
            let free = self.env.disk_free_override.load(Ordering::Relaxed);
            if free > 0 {
                return Some(free);
            }
        }
        let root = self
            .cwd_override
            .clone()
            .unwrap_or_else(|| self.env.cwd().to_path_buf());
        crate::util::free_bytes(&root)
    }

    /// Warn once per low-space episode, so the model can clean up before a
    /// write fails halfway through.
    fn check_disk(&mut self) {
        let Some(free) = self.disk_free() else {
            return;
        };
        if free >= DISK_LOW {
            self.disk_warned = false;
            return;
        }
        if self.disk_warned {
            return;
        }
        self.disk_warned = true;
        let gb = free as f64 / (1u64 << 30) as f64;
        self.emit(Event::Warn(format!("Low disk space: {gb:.1} GB free")));
        self.messages.push(Message::user(format!(
            "<system-reminder>Only {gb:.1} GB of disk space is free. A build, download or large file could fill it; \
             writes and commands are refused once it drops below {} MB. Remove build artifacts or caches you created \
             (for example `cargo clean`, or deleting a temporary build directory) before continuing.</system-reminder>",
            DISK_CRITICAL >> 20
        )));
    }

    /// Model replies and tool calls so far in this session (`num_turns`,
    /// `tool_calls` in print-mode results). Counted as they happen, since
    /// compaction drops the messages; a session saved before the counters
    /// existed falls back to counting its messages.
    pub fn run_counts(&self) -> (u64, u64) {
        let turns = self
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count() as u64;
        let calls: u64 = self
            .messages
            .iter()
            .map(|m| m.tool_calls.len() as u64)
            .sum();
        (
            self.stats.turns.max(turns),
            self.stats.tool_calls.max(calls),
        )
    }
}

const COMPACT_PROMPT: &str = r#"Write a handoff summary of this conversation so another agent can pick up exactly where it stopped. Do not call any tools.

Cover, in this order:
1. What the user wants, including every constraint and correction they gave.
2. Decisions made and the technical facts that drove them.
3. Every file examined, changed or created, why it matters, and the essential code (snippets where they matter).
4. What went wrong and how it was resolved, plus feedback received.
5. What has been worked out so far.
6. Each user message (excluding tool results), in their words or a faithful paraphrase.
7. What still has to be done.
8. The exact task in progress at this moment, with paths and code.
9. The immediate next action, quoting the latest instruction.

Be complete on essentials, not long for its own sake."#;

// --------------------------------------------------------------- subagents

/// The `oracle` subagent: a read-only second opinion.
const ORACLE_PROMPT: &str = "You are the oracle: a senior engineer giving a second opinion. You review plans, designs and diffs \
and find what is wrong with them before it ships: bugs, wrong assumptions, missed edge cases, broken callers, security \
problems, and simpler alternatives to complicated plans. You are read-only: read the code you need (Read, Grep, Glob, RepoMap \
for an unfamiliar tree, and read-only Bash such as `git diff`), never modify anything. Check claims against the code rather \
than trusting the description. Be concrete and brief: list each problem with file:line, why it is wrong and how to fix it, \
most important first. Don't pad with praise or style nits. If you find nothing that needs changing, say exactly `LGTM` and \
nothing else.";

const EXPLORE_TOOLS: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "Ls",
    "RepoMap",
    "SemanticSearch",
    "WebFetch",
    "WebSearch",
    "Bash",
    "BashOutput",
    "Skill",
];

/// What came of a usage limit: waited it out, didn't wait, or the user stopped the wait.
enum LimitWait {
    Resumed,
    No,
    Cancelled,
}

/// A reset time as the local clock shows it: `14:05`, or `Fri 09:00` on another day.
fn reset_clock(at: i64, now: i64) -> String {
    use chrono::{Local, TimeZone};
    let (Some(t), Some(n)) = (
        Local.timestamp_opt(at, 0).single(),
        Local.timestamp_opt(now, 0).single(),
    ) else {
        return "the reset".into();
    };
    if t.date_naive() == n.date_naive() {
        t.format("%H:%M").to_string()
    } else {
        t.format("%a %H:%M").to_string()
    }
}

/// A unified diff of a turn's changes, for the oracle.
fn turn_diff(root: &Path, changes: &TurnChanges) -> String {
    let root = &permissions::normalize(root);
    let mut files: Vec<&PathBuf> = changes.originals.keys().collect();
    files.sort();
    let mut out = String::new();
    for p in files {
        let rel = crate::util::rel(root, p);
        let old = changes.originals[p]
            .as_deref()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        let new = std::fs::read(p)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        if old == new {
            continue;
        }
        let d = similar::TextDiff::from_lines(&old, &new);
        out.push_str(
            &d.unified_diff()
                .context_radius(3)
                .header(&format!("a/{rel}"), &format!("b/{rel}"))
                .to_string(),
        );
    }
    for c in changes
        .files
        .iter()
        .filter(|c| !changes.originals.contains_key(&c.path))
    {
        out.push_str(&format!(
            "(changed by a command; read it to see the change: {})\n",
            crate::util::rel(root, &c.path)
        ));
    }
    out
}

/// The oracle's "nothing to fix" answer.
fn oracle_approves(text: &str) -> bool {
    let t = text.trim().trim_matches(|c: char| !c.is_alphanumeric());
    t.eq_ignore_ascii_case("lgtm") || (t.len() < 60 && t.to_ascii_lowercase().starts_with("lgtm"))
}

fn worktree_note(w: &crate::worktree::Worktree) -> String {
    if w.cleanup_if_unchanged() {
        "\n\n(The agent made no changes; its worktree was removed.)".into()
    } else {
        format!(
            "\n\nWorktree: {} (branch `{}`)\n{}",
            w.path.display(),
            w.branch,
            w.changes()
        )
    }
}

/// Where to set the test command the guardrails run: a Chanto cloud run
/// takes it from its run or agent (the runner hands over a run-tools token),
/// a local session from the project's config.
fn checks_hint() -> &'static str {
    if std::env::var_os("CHANTO_RUN_TOKEN").is_some() {
        "in the run's or agent's Checks (one command per line)"
    } else {
        "with `checks = [\"…\"]` under `[guardrails]` in `.chanto/config.toml`"
    }
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
mod codex_call_tests;

#[cfg(test)]
mod oracle_limit_tests;

#[cfg(test)]
mod protocol_tests;

#[cfg(test)]
mod security_tests;
