//! User-configured hooks, following Claude Code's hook protocol for the events
//! Chanto fires and its handler types: `command` (shell or exec form, also
//! `async` / `asyncRewake`), `http`, `mcp_tool` (a tool on a connected MCP
//! server; its text is read like stdout), `prompt` (one model call returning
//! `{"ok", "reason", "impossible"}`) and `agent` (a read-only subagent that
//! returns the same verdict). Hooks come from settings, from invoked skills
//! (for the rest of the session) and from the running subagent's frontmatter
//! (for its lifetime, see [`SCOPE`]).
//!
//! A command hook gets a JSON payload on stdin (`hook_event_name`,
//! `session_id`, `transcript_path`, `cwd`, `permission_mode` plus event
//! fields) and `CLAUDE_PROJECT_DIR` / `CHANTO_PROJECT_DIR` (plus, for plugin
//! hooks, `CLAUDE_PLUGIN_ROOT` / `CLAUDE_PLUGIN_DATA`) in its environment. An
//! http hook gets the same payload as a JSON POST body; a 2xx response with an
//! empty or JSON body is read like a command's exit 0 and stdout, anything else
//! is a non-blocking error.
//! - exit 2: block; stderr is fed back (to the model, or to the user for prompts)
//! - exit 0: stdout may be JSON: `decision: "block"|"approve"`, `reason`,
//!   `continue: false` + `stopReason`, `systemMessage`, and
//!   `hookSpecificOutput.{permissionDecision, permissionDecisionReason,
//!   updatedInput, additionalContext}`. Plain stdout is added as context for
//!   UserPromptSubmit and SessionStart.

use crate::agent::Env;
use crate::config::HookConfig;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, PartialEq)]
pub enum PermDecision {
    Allow,
    Deny(String),
    Ask(String),
}

#[derive(Debug, Default)]
pub struct HookResult {
    /// Block the action (exit 2 or `decision: "block"`), with feedback.
    pub block: Option<String>,
    /// Text to add to the model's context.
    pub context: Vec<String>,
    /// PreToolUse / PermissionRequest decision.
    pub decision: Option<PermDecision>,
    /// PreToolUse: replacement tool input.
    pub updated_input: Option<Value>,
    /// `continue: false`: stop the agent, with this reason.
    pub halt: Option<String>,
    /// Messages for the user.
    pub messages: Vec<String>,
    /// Every JSON object an exit-0 hook printed, for event-specific fields
    /// (`retry`, `displayContent`, `watchPaths`, `action`, ...).
    pub json: Vec<Value>,
    /// The stdout of every hook that exited 0 (WorktreeCreate reads a path from it).
    pub stdout: Vec<String>,
    /// A hook exited with a code other than 0 or 2.
    pub failed: bool,
    /// A hook didn't answer in time.
    pub timed_out: bool,
    /// PostToolUse: what the model sees instead of the tool's output
    /// (`updatedToolOutput`, or `updatedMCPToolOutput` for MCP tools).
    pub updated_output: Option<String>,
}

/// Hooks for the code running inside this scope: a subagent's frontmatter
/// hooks while it runs, or none at all inside an agent hook's verifier (so
/// it can't trigger hooks, itself included).
#[derive(Clone, Default)]
pub struct Scope {
    pub hooks: Option<Arc<crate::config::HooksConfig>>,
    pub off: bool,
}

tokio::task_local! {
    pub static SCOPE: Scope;
}

fn scope() -> Scope {
    SCOPE.try_with(|s| s.clone()).unwrap_or_default()
}

/// The hooks that run for `event`: settings, then hooks registered by
/// invoked skills, then the running subagent's own. `disableAllHooks` leaves
/// only what the settings kept (managed hooks).
fn configured(env: &Env, event: &str) -> Vec<HookConfig> {
    let sc = scope();
    if sc.off {
        return vec![];
    }
    let mut v = env
        .hooks
        .read()
        .unwrap()
        .get(event)
        .cloned()
        .unwrap_or_default();
    if env.config.disable_all_hooks == Some(true) {
        return v;
    }
    v.extend(
        env.hook_state
            .session_hooks
            .lock()
            .unwrap()
            .get(event)
            .cloned()
            .unwrap_or_default(),
    );
    if let Some(h) = sc.hooks.as_ref().and_then(|h| h.get(event)) {
        v.extend(h.iter().cloned());
    }
    v
}

/// Register an invoked skill's hooks for the rest of the session.
mod verdict;
use verdict::{apply_verdict, evaluate, supported};
mod envfile;
pub use envfile::{bash_prelude, plain_env_vars, prepare_env_file};
use envfile::{env_file_for, env_files};
mod worktree;
#[cfg(test)]
use worktree::worktree_remove_detached;
pub use worktree::{finish_hook_worktree, worktree_create, worktree_create_detached};

pub fn add_session_hooks(env: &Env, hooks: &crate::config::HooksConfig) {
    let mut all = env.hook_state.session_hooks.lock().unwrap();
    for (event, list) in hooks {
        let have = all.entry(event.clone()).or_default();
        for h in list {
            if !have.contains(h) {
                have.push(h.clone());
            }
        }
    }
}

/// A `once` skill hook ran successfully: drop it.
fn remove_once(env: &Env, event: &str, h: &HookConfig) {
    let mut all = env.hook_state.session_hooks.lock().unwrap();
    if let Some(list) = all.get_mut(event)
        && let Some(i) = list.iter().position(|x| x == h)
    {
        list.remove(i);
    }
}

impl HookResult {
    pub fn context_text(&self) -> Option<String> {
        if self.context.is_empty() {
            None
        } else {
            Some(self.context.join("\n"))
        }
    }
}

/// Default timeout, as in Claude Code: most events 60s; a few differ.
fn default_timeout(event: &str) -> u64 {
    match event {
        "MessageDisplay" => 10,
        "PreModelSwitch" => 30,
        "DirectoryAdded" => 600,
        _ => 60,
    }
}

fn timeout_for(h: &HookConfig, payload: &Value) -> Duration {
    let default = match h.kind.as_str() {
        // As in Claude Code: model hooks have their own defaults.
        "prompt" => 30,
        "agent" => 60,
        _ => default_timeout(payload["hook_event_name"].as_str().unwrap_or("")),
    };
    Duration::from_secs(h.timeout_secs.unwrap_or(default))
}

/// `${CLAUDE_PROJECT_DIR}`, `${CLAUDE_PLUGIN_ROOT}` and `${CLAUDE_PLUGIN_DATA}`
/// in an exec-form hook's command and arguments (no shell expands them).
fn placeholders(
    s: &str,
    root: &std::path::Path,
    env: &std::collections::BTreeMap<String, String>,
) -> String {
    let mut out = s.replace("${CLAUDE_PROJECT_DIR}", &root.display().to_string());
    for k in ["CLAUDE_PLUGIN_ROOT", "CLAUDE_PLUGIN_DATA"] {
        if let Some(v) = env.get(k) {
            out = out.replace(&format!("${{{k}}}"), v);
        }
    }
    out
}

/// The process a command hook runs: exec form with `args` (no shell), the
/// hook's own `shell: "powershell"`, or the session's shell.
fn hook_command(h: &HookConfig, root: &std::path::Path) -> tokio::process::Command {
    if let Some(args) = &h.args {
        let mut c = tokio::process::Command::new(placeholders(h.command.trim(), root, &h.env));
        c.args(args.iter().map(|a| placeholders(a, root, &h.env)));
        return c;
    }
    if h.shell
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("powershell"))
    {
        let exe = if cfg!(windows) || !crate::shell::command_exists("pwsh") {
            "powershell"
        } else {
            "pwsh"
        };
        let mut c = tokio::process::Command::new(exe);
        c.args(["-NoProfile", "-NonInteractive", "-Command", &h.command]);
        return c;
    }
    crate::shell::command(&h.command)
}

/// Run a command hook: `(exit code, stdout, stderr)`, None on timeout. A
/// hook that can't be started is exit code -1 with the reason as stderr.
async fn run(
    h: &HookConfig,
    payload: &Value,
    cwd: &std::path::Path,
    env_file: Option<&std::path::Path>,
) -> Option<(i32, String, String)> {
    let cwd = cwd.to_path_buf();
    let root = crate::permissions::project_root().unwrap_or_else(|| cwd.clone());
    let mut cmd = hook_command(h, &root);
    cmd.current_dir(&cwd);
    cmd.env("CLAUDE_PROJECT_DIR", &root)
        .env("CHANTO_PROJECT_DIR", &root);
    // Only SessionStart, Setup, CwdChanged and FileChanged hooks get one.
    match env_file {
        Some(f) => cmd.env("CLAUDE_ENV_FILE", f).env("CHANTO_ENV_FILE", f),
        None => cmd
            .env_remove("CLAUDE_ENV_FILE")
            .env_remove("CHANTO_ENV_FILE"),
    };
    for (k, v) in &h.env {
        cmd.env(k, v);
    }
    // A plugin's data directory is created the first time one of its hooks runs.
    if let Some(d) = h.env.get("CLAUDE_PLUGIN_DATA") {
        let _ = std::fs::create_dir_all(d);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Some((-1, String::new(), format!("failed to start: {e}"))),
    };
    if let Some(mut stdin) = child.stdin.take() {
        // Write in the background so a hook that never reads stdin can't block us.
        let data = payload.to_string();
        tokio::spawn(async move {
            let _ = stdin.write_all(data.as_bytes()).await;
        });
    }
    // Wait for the hook itself to exit, not for its pipes to close: a hook that
    // starts a background process (`server &`) leaves them open, and waiting
    // for that held up the session until the timeout, then dropped the output.
    fn drain<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
        pipe: Option<R>,
    ) -> (
        std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let b = buf.clone();
        let task = tokio::spawn(async move {
            let Some(mut p) = pipe else { return };
            let mut chunk = [0u8; 8192];
            while let Ok(n) = tokio::io::AsyncReadExt::read(&mut p, &mut chunk).await {
                if n == 0 {
                    break;
                }
                b.lock().unwrap().extend_from_slice(&chunk[..n]);
            }
        });
        (buf, task)
    }
    let (out_buf, out_task) = drain(child.stdout.take());
    let (err_buf, err_task) = drain(child.stderr.take());
    let status = tokio::time::timeout(timeout_for(h, payload), child.wait())
        .await
        .ok()?
        .ok()?;
    // What the hook wrote before exiting is in the pipe already; a background
    // process still holding it gets a moment, then is left alone.
    let _ = tokio::time::timeout(Duration::from_millis(500), async {
        let _ = out_task.await;
        let _ = err_task.await;
    })
    .await;
    let stdout = std::mem::take(&mut *out_buf.lock().unwrap());
    let stderr = std::mem::take(&mut *err_buf.lock().unwrap());
    Some((
        status.code().unwrap_or(-1),
        String::from_utf8_lossy(&stdout).trim().to_string(),
        String::from_utf8_lossy(&stderr).trim().to_string(),
    ))
}

/// Expand `$VAR` / `${VAR}` in an http hook header; only `allowed` variables
/// resolve, others become empty (as in Claude Code).
fn expand_header(value: &str, allowed: &[String]) -> String {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}|\$([A-Za-z_][A-Za-z0-9_]*)").unwrap()
    });
    RE.replace_all(value, |c: &regex::Captures| {
        let name = c
            .get(1)
            .or_else(|| c.get(2))
            .map(|m| m.as_str())
            .unwrap_or("");
        if allowed.iter().any(|a| a == name) {
            std::env::var(name).unwrap_or_default()
        } else {
            String::new()
        }
    })
    .into_owned()
}

/// POST the payload to an http hook. Maps the response onto the command
/// contract: 2xx with an empty or JSON-object body = exit 0 with that body as
/// stdout; other bodies, non-2xx and connection errors = non-blocking errors;
/// a timeout cancels the hook.
async fn run_http(h: &HookConfig, payload: &Value) -> Option<(i32, String, String)> {
    let mut rb = crate::http::client()
        .post(h.url.as_str())
        .header("content-type", "application/json")
        .timeout(timeout_for(h, payload))
        .body(payload.to_string());
    for (k, v) in &h.headers {
        rb = rb.header(k.as_str(), expand_header(v, &h.allowed_env_vars));
    }
    let resp = match rb.send().await {
        Ok(r) => r,
        Err(e) if e.is_timeout() => return None,
        Err(e) => return Some((1, String::new(), format!("POST {} failed: {e}", h.url))),
    };
    let status = resp.status();
    let body = match resp.text().await {
        Ok(b) => b.trim().to_string(),
        Err(e) if e.is_timeout() => return None,
        Err(e) => {
            return Some((
                1,
                String::new(),
                format!("reading the response from {} failed: {e}", h.url),
            ));
        }
    };
    if !status.is_success() {
        return Some((
            1,
            String::new(),
            format!("{} returned HTTP {status}", h.url),
        ));
    }
    if body.is_empty() || serde_json::from_str::<Value>(&body).is_ok_and(|v| v.is_object()) {
        return Some((0, body, String::new()));
    }
    Some((
        1,
        String::new(),
        format!("{} returned a body that is not a JSON object", h.url),
    ))
}

/// Fill `${path}` in an mcp_tool hook's string inputs from the hook input
/// (`${tool_input.file_path}`); unknown paths become empty.
fn fill_input(v: &Value, payload: &Value) -> Value {
    match v {
        Value::String(s) => {
            static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
                regex::Regex::new(r"\$\{([A-Za-z0-9_.\-]+)\}").unwrap()
            });
            Value::String(
                RE.replace_all(s, |c: &regex::Captures| {
                    let found = c[1].split('.').try_fold(payload, |v, k| v.get(k));
                    match found {
                        Some(Value::String(s)) => s.clone(),
                        Some(Value::Null) | None => String::new(),
                        Some(other) => other.to_string(),
                    }
                })
                .into_owned(),
            )
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| fill_input(x, payload)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), fill_input(x, payload)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Call an mcp_tool hook's tool. Its text is read like a command's stdout; an
/// error result, or a server that isn't there or connected, is a
/// non-blocking error. None on timeout.
async fn run_mcp(env: &Env, h: &HookConfig, payload: &Value) -> Option<(i32, String, String)> {
    let Some(c) = env.mcp.iter().find(|c| c.name == h.server) else {
        return Some((
            1,
            String::new(),
            format!("MCP server `{}` is not configured", h.server),
        ));
    };
    if !c.is_connected() {
        return Some((
            1,
            String::new(),
            format!("MCP server `{}` is not connected", h.server),
        ));
    }
    let args = fill_input(h.input.as_ref().unwrap_or(&json!({})), payload);
    match tokio::time::timeout(timeout_for(h, payload), c.call(&h.tool, args)).await {
        Err(_) => None,
        Ok(Err(e)) => Some((1, String::new(), format!("{e:#}"))),
        Ok(Ok(out)) if out.is_error => Some((1, String::new(), out.content)),
        Ok(Ok(out)) => Some((0, out.content.trim().to_string(), String::new())),
    }
}

/// The JSON object an async hook printed: all of stdout, or a line of its own.
fn async_json(stdout: &str) -> Option<Value> {
    if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(stdout.trim()) {
        return Some(v);
    }
    stdout.lines().rev().find_map(|l| {
        serde_json::from_str::<Value>(l.trim())
            .ok()
            .filter(Value::is_object)
    })
}

/// Start an `async` / `asyncRewake` command hook in the background. When it
/// exits, its `additionalContext` and `systemMessage` reach the model on its
/// next turn; with `asyncRewake`, exit 2 also wakes an idle session with the
/// hook's stderr (or stdout).
fn spawn_async(
    env: &Env,
    h: &HookConfig,
    event: &str,
    payload: Value,
    cwd: PathBuf,
    env_file: Option<PathBuf>,
) {
    let Some(env) = session(env) else { return };
    let (event, mut h) = (event.to_string(), h.clone());
    // Only asyncRewake hooks keep their timeout.
    if !h.async_rewake {
        h.timeout_secs = Some(7 * 86_400);
    }
    tokio::spawn(async move {
        let Some((code, stdout, stderr)) = run(&h, &payload, &cwd, env_file.as_deref()).await
        else {
            return;
        };
        let mut notes = Vec::new();
        if code == 0
            && let Some(v) = async_json(&stdout)
        {
            notes.extend(
                v["hookSpecificOutput"]["additionalContext"]
                    .as_str()
                    .map(str::to_string),
            );
            notes.extend(v["systemMessage"].as_str().map(str::to_string));
        }
        let rewake = code == 2 && h.async_rewake;
        if rewake {
            notes.push(if stderr.is_empty() { stdout } else { stderr });
        }
        notes.retain(|n| !n.trim().is_empty());
        if notes.is_empty() {
            return;
        }
        let what = crate::util::one_line(&h.describe(), 60);
        env.notifications.lock().unwrap().push(format!(
            "<system-reminder>\nBackground {event} hook `{what}` {}:\n{}\n</system-reminder>",
            if rewake {
                "exited with code 2"
            } else {
                "finished"
            },
            notes.join("\n")
        ));
        if rewake {
            // Wakes an idle session the way a finished background agent does.
            env.emit(crate::agent::Event::Background {
                id: "hook".into(),
                description: format!("{event} hook `{what}`"),
                status: "exited with code 2".into(),
            });
            env.bg_done.notify_waiters();
        }
    });
}

/// What `updatedToolOutput` replaces the model's view of a tool result with:
/// a value shaped like that tool's `tool_response` (Bash's `stdout`/`stderr`,
/// Read's `file.content`, others' `output`); MCP tools also take text or
/// content blocks. None when the shape doesn't fit, as in Claude Code.
fn replaced_output(tool: &str, v: &Value, mcp_only: bool) -> Option<String> {
    let mcp = tool.starts_with("mcp__");
    if mcp_only && !mcp {
        return None;
    }
    if mcp {
        let blocks = v.as_array().or_else(|| v["content"].as_array());
        return match (v, blocks) {
            (Value::String(s), _) => Some(s.clone()),
            (_, Some(b)) => Some(
                b.iter()
                    .map(|c| {
                        c["text"]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| c.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => Some(v.to_string()),
        };
    }
    match tool {
        "Bash" => {
            let out = v["stdout"].as_str()?;
            let err = v["stderr"].as_str().unwrap_or("");
            Some(if err.is_empty() {
                out.to_string()
            } else if out.is_empty() {
                err.to_string()
            } else {
                format!("{out}\n{err}")
            })
        }
        "Read" => v["file"]["content"].as_str().map(str::to_string),
        _ => v["output"].as_str().map(str::to_string),
    }
}

/// Claude Code matcher semantics: empty or `*` matches everything; a value of
/// only letters, digits, `_`, `-`, spaces, `,` and `|` is a list of exact
/// names separated by `|` or `,`; anything else is an unanchored regex.
fn matcher_hits(matcher: &str, s: &str) -> bool {
    let m = matcher.trim();
    if m.is_empty() || m == "*" {
        return true;
    }
    // apply_patch edits files, so Claude-style `Edit|Write` matchers apply to it.
    let patch = s == "apply_patch";
    if m.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ' ' | ',' | '|'))
    {
        return m.split(['|', ',']).map(str::trim).any(|n| {
            !n.is_empty() && (n == s || (patch && matches!(n, "Edit" | "Write" | "MultiEdit")))
        });
    }
    let Ok(re) = regex::Regex::new(m) else {
        return false;
    };
    re.is_match(s)
        || (patch
            && ["Edit", "Write", "MultiEdit"]
                .iter()
                .any(|n| re.is_match(n)))
}

fn hook_matches(h: &HookConfig, subject: Option<&str>) -> bool {
    match subject {
        Some(s) => matcher_hits(&h.matcher, s),
        None => true,
    }
}

/// Events whose hooks may carry an `if` condition (Claude Code's tool events).
const TOOL_EVENTS: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "PermissionDenied",
];

/// Whether `cond` is one permission rule: `Tool` or `Tool(pattern)`. There
/// is no syntax for combining rules.
pub fn condition_is_valid(cond: &str) -> bool {
    let c = cond.trim();
    let name_end = c.find('(').unwrap_or(c.len());
    let name = &c[..name_end];
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '*'))
        && (name_end == c.len() || (c.ends_with(')') && c.len() >= name_end + 2))
        && ![") &&", ") ||", "),"].iter().any(|j| c.contains(j))
}

/// Drop leading `VAR=value` assignments from a simple command.
fn strip_assignments(cmd: &str) -> String {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"^(?:[A-Za-z_][A-Za-z0-9_]*=(?:'[^']*'|"[^"]*"|\S*)\s+)+"#).unwrap()
    });
    RE.replace(cmd.trim(), "").into_owned()
}

/// The commands a Bash input runs, for `if` checks: each simple command plus
/// the ones inside `$(...)` and backticks. `None` when that can't be told
/// (nested or process substitution).
fn bash_commands(command: &str) -> Option<Vec<String>> {
    static INNER: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\$\(([^()]*)\)|`([^`]*)`").unwrap());
    let inner = &*INNER;
    let mut out: Vec<String> = crate::permissions::split_shell(command)
        .iter()
        .map(|p| strip_assignments(p))
        .collect();
    for c in inner.captures_iter(command) {
        let body = c
            .get(1)
            .or_else(|| c.get(2))
            .map(|m| m.as_str())
            .unwrap_or("");
        out.extend(
            crate::permissions::split_shell(body)
                .iter()
                .map(|p| strip_assignments(p)),
        );
    }
    if crate::permissions::has_dynamic_parts(&inner.replace_all(command, "")) {
        return None;
    }
    Some(out)
}

/// Evaluate a hook's `if` rule against a tool call (`subjects` are the
/// tool's permission-rule subjects). `None` when it can't be evaluated: an
/// invalid rule, or an event that isn't about a tool call.
fn condition_holds(
    cond: &str,
    event: &str,
    tool: Option<&str>,
    input: &Value,
    subjects: &[String],
) -> Option<bool> {
    if !TOOL_EVENTS.contains(&event) || !condition_is_valid(cond) {
        return None;
    }
    let tool = tool?;
    let cond = cond.trim();
    let rule = crate::permissions::Rule::parse(cond);
    let Some(open) = cond.find('(') else {
        return Some(rule.matches(tool, ""));
    };
    if !crate::permissions::Rule::parse(&cond[..open]).matches(tool, "") {
        return Some(false);
    }
    let subjects = if tool == "Bash" {
        match bash_commands(input["command"].as_str().unwrap_or("")) {
            Some(s) => s,
            // As in Claude Code: when it's unclear what the command runs, the hook runs.
            None => return Some(true),
        }
    } else {
        subjects.to_vec()
    };
    if subjects.is_empty() {
        return Some(rule.matches(tool, ""));
    }
    Some(subjects.iter().any(|s| rule.matches(tool, s)))
}

/// Permission-rule subjects (commands, paths, domains) of a tool call.
fn rule_subjects(env: &Env, tool: &str, input: &Value) -> Vec<String> {
    if let Some(t) = env
        .tools
        .get(tool)
        .or_else(|| env.mcp.iter().find_map(|c| c.tool(tool)))
    {
        return crate::tools::Tool::rule_subjects(t.as_ref(), input);
    }
    if tool == "apply_patch" {
        return crate::tools::patch::paths(input["input"].as_str().unwrap_or(""));
    }
    vec![]
}

fn warn_once(env: &Env, msg: String) {
    static WARNED: std::sync::Mutex<std::collections::BTreeSet<String>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    if WARNED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(msg.clone())
    {
        env.emit(crate::agent::Event::Warn(msg));
    }
}

fn describe(h: &HookConfig) -> String {
    if h.kind == "http" {
        h.url.clone()
    } else {
        crate::util::one_line(&h.describe(), 60)
    }
}

/// `tool_response` for PostToolUse, shaped like Claude Code's where Chanto
/// has the data; other tools get `{"output": ...}`.
fn tool_response(tool: &str, args: &Value, output: &str) -> Value {
    let path = args["file_path"].clone();
    match tool {
        "Bash" => {
            let stdout = if output == "(no output)" { "" } else { output };
            json!({"stdout": stdout, "stderr": "", "interrupted": output.contains("[interrupted by user]"), "isImage": false})
        }
        "Write" => {
            let kind = if output.starts_with("File created") {
                "create"
            } else {
                "update"
            };
            json!({"type": kind, "filePath": path, "content": args["content"]})
        }
        "Edit" => json!({
            "filePath": path,
            "oldString": args["old_string"],
            "newString": args["new_string"],
            "replaceAll": args["replace_all"].as_bool().unwrap_or(false),
        }),
        "MultiEdit" => json!({"filePath": path, "edits": args["edits"]}),
        "Read" => json!({"type": "text", "file": {"filePath": path, "content": output}}),
        "apply_patch" => {
            let paths = crate::tools::patch::paths(args["input"].as_str().unwrap_or(""));
            json!({"filePath": paths.first().cloned().unwrap_or_default(), "filePaths": paths, "output": output})
        }
        _ => json!({"output": output}),
    }
}

/// Tool input as hooks see it: patches also expose `file_path` / `file_paths`
/// like Edit, so existing Claude Code hooks can act on them.
fn hook_input(tool: &str, args: &Value) -> Value {
    if tool != "apply_patch" {
        return args.clone();
    }
    let paths = crate::tools::patch::paths(args["input"].as_str().unwrap_or(""));
    let mut v = args.clone();
    if let Some(o) = v.as_object_mut() {
        o.insert(
            "file_path".into(),
            json!(paths.first().cloned().unwrap_or_default()),
        );
        o.insert("file_paths".into(), json!(paths));
    }
    v
}

/// Whether any hook is configured for `event`.
pub fn has(env: &Env, event: &str) -> bool {
    !configured(env, event).is_empty()
}

/// Events whose `systemMessage` Claude Code discards (DirectoryAdded hands it
/// to the model instead).
const NO_SYSTEM_MESSAGE: &[&str] = &[
    "Setup",
    "InstructionsLoaded",
    "ConfigChange",
    "DirectoryAdded",
    "WorktreeCreate",
    "WorktreeRemove",
    "MessageDisplay",
    "Elicitation",
    "ElicitationResult",
    "PreCompact",
    "PostCompact",
    "StopFailure",
];

/// Events whose `continue: false` Claude Code ignores.
const NO_CONTINUE: &[&str] = &[
    "Setup",
    "InstructionsLoaded",
    "ConfigChange",
    "CwdChanged",
    "FileChanged",
    "DirectoryAdded",
    "WorktreeCreate",
    "WorktreeRemove",
    "MessageDisplay",
    "Elicitation",
    "ElicitationResult",
    "PreCompact",
    "PostCompact",
    "StopFailure",
    "TaskCreated",
];

/// Run every hook for `event` whose matcher fits `subject` and fold the results.
pub async fn fire(env: &Env, event: &str, subject: Option<&str>, fields: Value) -> HookResult {
    let hooks = configured(env, event);
    if hooks.is_empty() {
        return HookResult::default();
    }
    let mut payload = json!({
        "hook_event_name": event,
        "session_id": env.session_id(),
        "transcript_path": env.transcript_path().map(|p| p.display().to_string()),
        "cwd": env.cwd(),
        "permission_mode": env.mode(),
    });
    if let (Some(p), Value::Object(f)) = (payload.as_object_mut(), fields) {
        p.extend(f);
    }
    let env_file = env_file_for(env, event);
    let res = fire_hooks(
        &hooks,
        event,
        subject,
        &payload,
        &env.cwd(),
        Some(env),
        env_file.as_deref(),
    )
    .await;
    for m in &res.messages {
        env.emit(crate::agent::Event::Info(m.clone()));
    }
    res
}

/// Run hooks outside a session (before one exists, or after it ended), for
/// `--worktree`'s WorktreeCreate / WorktreeRemove. Blocks the calling thread.
pub fn fire_detached(
    hooks: &crate::config::HooksConfig,
    event: &str,
    cwd: &std::path::Path,
    fields: Value,
) -> HookResult {
    let Some(hooks) = hooks.get(event).filter(|h| !h.is_empty()).cloned() else {
        return HookResult::default();
    };
    let mut payload =
        json!({"hook_event_name": event, "session_id": "", "transcript_path": null, "cwd": cwd});
    if let (Some(p), Value::Object(f)) = (payload.as_object_mut(), fields) {
        p.extend(f);
    }
    let (event, cwd) = (event.to_string(), cwd.to_path_buf());
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        Some(rt.block_on(fire_hooks(&hooks, &event, None, &payload, &cwd, None, None)))
    })
    .join()
    .ok()
    .flatten()
    .unwrap_or_else(|| HookResult {
        timed_out: true,
        ..Default::default()
    })
}

async fn fire_hooks(
    hooks: &[HookConfig],
    event: &str,
    subject: Option<&str>,
    payload: &Value,
    cwd: &std::path::Path,
    env: Option<&Env>,
    env_file: Option<&std::path::Path>,
) -> HookResult {
    let mut res = HookResult::default();
    let plain_is_context = matches!(
        event,
        "UserPromptSubmit" | "SessionStart" | "PostModelSwitch"
    );
    let tool = payload["tool_name"].as_str().map(str::to_string);
    for h in hooks.iter().filter(|h| hook_matches(h, subject)) {
        if let Some(cond) = &h.condition {
            let subjects = match (tool.as_deref(), env) {
                (Some(t), Some(env)) => rule_subjects(env, t, &payload["tool_input"]),
                _ => vec![],
            };
            match condition_holds(
                cond,
                event,
                tool.as_deref(),
                &payload["tool_input"],
                &subjects,
            ) {
                Some(true) => {}
                Some(false) => continue,
                None => {
                    if let Some(env) = env {
                        warn_once(
                            env,
                            format!(
                                "Skipped {event} hook `{}`: its `if` condition {cond:?} can't be evaluated (it must be one permission rule, on a tool event)",
                                describe(h)
                            ),
                        );
                    }
                    continue;
                }
            }
        }
        if !supported(&h.kind, event, payload) {
            if let Some(env) = env {
                warn_once(
                    env,
                    format!(
                        "Skipped {event} hook `{}`: Claude Code doesn't run `{}` hooks on {event}",
                        describe(h),
                        h.kind
                    ),
                );
            }
            continue;
        }
        let command = matches!(h.kind.as_str(), "" | "command");
        if command && (h.run_async || h.async_rewake) {
            if let Some(env) = env {
                spawn_async(
                    env,
                    h,
                    event,
                    payload.clone(),
                    cwd.to_path_buf(),
                    env_file.map(|p| p.to_path_buf()),
                );
            }
            continue;
        }
        let status = env.zip(h.status_message.as_ref());
        if let Some((e, m)) = status {
            e.emit(crate::agent::Event::Busy(Some(m.clone())));
        }
        if matches!(h.kind.as_str(), "prompt" | "agent") {
            let Some(env) = env else { continue };
            let verdict = evaluate(env, h, payload).await;
            if status.is_some() {
                env.emit(crate::agent::Event::Busy(None));
            }
            match verdict {
                Ok(v) => {
                    let ok = v.ok;
                    apply_verdict(&mut res, event, h, v);
                    if ok && h.once {
                        remove_once(env, event, h);
                    }
                }
                // As for other hooks, an error doesn't block.
                Err(e) => {
                    res.failed = true;
                    res.messages.push(format!(
                        "{event} {} hook failed: {}",
                        h.kind,
                        crate::util::one_line(&e, 200)
                    ));
                }
            }
            if res.block.is_some()
                || res.halt.is_some()
                || matches!(res.decision, Some(PermDecision::Deny(_)))
            {
                break;
            }
            continue;
        }
        let ran = match h.kind.as_str() {
            "http" => run_http(h, payload).await,
            "mcp_tool" => match env {
                Some(env) => run_mcp(env, h, payload).await,
                None => continue,
            },
            _ => run(h, payload, cwd, env_file).await,
        };
        if let Some((e, _)) = status {
            e.emit(crate::agent::Event::Busy(None));
        }
        // `onFailure: "block"`: a failing command or http hook does what exit 2
        // would (denies on PermissionRequest). Not for events where exit 2
        // means "keep working".
        let block_on_failure = h.on_failure.as_deref() == Some("block")
            && (command || h.kind == "http")
            && !matches!(
                event,
                "Stop" | "SubagentStop" | "TaskCompleted" | "TeammateIdle" | "Setup"
            );
        let fail = |why: &str, detail: &str| {
            let mut m = format!(
                "{event} hook error: [{}]: {why}; blocking because onFailure is \"block\"",
                h.describe()
            );
            if !detail.trim().is_empty() {
                m.push('\n');
                m.push_str(detail.trim());
            }
            m
        };
        let block_with = |res: &mut HookResult, msg: String| {
            if event == "PermissionRequest" {
                res.decision = Some(PermDecision::Deny(msg));
            } else {
                res.block = Some(msg);
            }
        };
        let Some((code, stdout, stderr)) = ran else {
            if block_on_failure {
                block_with(&mut res, fail("timed out", ""));
                break;
            }
            res.timed_out = true;
            continue;
        };
        if block_on_failure && code != 0 && code != 2 {
            block_with(&mut res, fail("failed", &stderr));
            break;
        }
        if block_on_failure
            && code == 0
            && stdout.starts_with('{')
            && serde_json::from_str::<Value>(&stdout).is_err()
        {
            block_with(
                &mut res,
                fail(
                    "failed",
                    &format!(
                        "invalid JSON output: {}",
                        crate::util::one_line(&stdout, 200)
                    ),
                ),
            );
            break;
        }
        if code == 0
            && h.once
            && let Some(env) = env
        {
            remove_once(env, event, h);
        }
        // Setup can't block, and its JSON output is discarded.
        if event == "Setup" {
            if code != 0 {
                res.failed = true;
            }
            res.stdout.push(stdout);
            continue;
        }
        if code == 2 {
            let said = if stderr.trim().is_empty() {
                stdout
            } else {
                stderr
            };
            // Without a reason, say which hook it was so the block is traceable.
            res.block = Some(if said.trim().is_empty() {
                format!(
                    "hook [{}] exited with code 2 and gave no reason",
                    h.describe()
                )
            } else {
                said
            });
            break;
        }
        if code != 0 {
            res.failed = true;
            if !stderr.is_empty() {
                res.messages.push(format!(
                    "{event} hook failed: {}",
                    crate::util::one_line(&stderr, 200)
                ));
            }
            continue;
        }
        res.stdout.push(stdout.clone());
        let Some(v) = stdout
            .starts_with('{')
            .then(|| serde_json::from_str::<Value>(&stdout).ok())
            .flatten()
        else {
            if plain_is_context && !stdout.is_empty() {
                res.context.push(stdout);
            }
            continue;
        };
        res.json.push(v.clone());
        let reason = v["reason"].as_str().unwrap_or("").to_string();
        if let Some(m) = v["systemMessage"]
            .as_str()
            .filter(|_| !NO_SYSTEM_MESSAGE.contains(&event))
        {
            res.messages.push(m.to_string());
        }
        if v["continue"] == json!(false) && !NO_CONTINUE.contains(&event) {
            res.halt = Some(
                v["stopReason"]
                    .as_str()
                    .unwrap_or("stopped by hook")
                    .to_string(),
            );
            break;
        }
        let hso = &v["hookSpecificOutput"];
        if let Some(c) = hso["additionalContext"].as_str() {
            res.context.push(c.to_string());
        }
        if !hso["updatedInput"].is_null() {
            res.updated_input = Some(hso["updatedInput"].clone());
        }
        if event == "PostToolUse" {
            let tool = tool.as_deref().unwrap_or("");
            let out = hso
                .get("updatedToolOutput")
                .filter(|v| !v.is_null())
                .and_then(|v| replaced_output(tool, v, false));
            let mcp = hso
                .get("updatedMCPToolOutput")
                .filter(|v| !v.is_null())
                .and_then(|v| replaced_output(tool, v, true));
            if let Some(o) = out.or(mcp) {
                res.updated_output = Some(o);
            }
        }
        let why = hso["permissionDecisionReason"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| reason.clone());
        // PermissionRequest uses `decision.behavior`; PreToolUse `permissionDecision`.
        let pd = hso["permissionDecision"]
            .as_str()
            .or_else(|| hso["decision"]["behavior"].as_str());
        match pd {
            Some("allow") => res.decision = Some(PermDecision::Allow),
            Some("deny") => {
                res.decision = Some(PermDecision::Deny(if why.is_empty() {
                    format!("hook [{}] denied it and gave no reason", h.describe())
                } else {
                    why
                }));
                break;
            }
            Some("ask") => res.decision = Some(PermDecision::Ask(why)),
            _ => match v["decision"].as_str() {
                Some("block") => {
                    res.block = Some(if reason.is_empty() {
                        format!("hook [{}] blocked it and gave no reason", h.describe())
                    } else {
                        reason
                    });
                    break;
                }
                Some("approve") => res.decision = Some(PermDecision::Allow),
                _ => {}
            },
        }
    }
    res
}

pub async fn pre_tool_use(env: &Env, tool: &str, args: &Value) -> HookResult {
    if !has(env, "PreToolUse") {
        return HookResult::default();
    }
    let mut r = fire(
        env,
        "PreToolUse",
        Some(tool),
        json!({"tool_name": tool, "tool_input": hook_input(tool, args)}),
    )
    .await;
    if let Some(b) = r.block.take().filter(|_| r.halt.is_none()) {
        r.decision = Some(PermDecision::Deny(b));
    }
    r
}

pub async fn post_tool_use(
    env: &Env,
    tool: &str,
    args: &Value,
    output: &str,
    is_error: bool,
) -> HookResult {
    let event = if is_error && has(env, "PostToolUseFailure") {
        "PostToolUseFailure"
    } else {
        "PostToolUse"
    };
    if !has(env, event) {
        return HookResult::default();
    }
    let out = crate::util::truncate(output, 20_000);
    let input = hook_input(tool, args);
    let fields = if event == "PostToolUseFailure" {
        json!({"tool_name": tool, "tool_input": input, "error": out, "is_interrupt": output.contains("[interrupted by user]")})
    } else {
        // `tool_response` is an object as in Claude Code; `tool_output` keeps
        // the plain text the model saw.
        json!({"tool_name": tool, "tool_input": input, "tool_response": tool_response(tool, args, &out), "tool_output": out})
    };
    fire(env, event, Some(tool), fields).await
}

pub async fn user_prompt_submit(env: &Env, prompt: &str) -> HookResult {
    if !has(env, "UserPromptSubmit") {
        return HookResult::default();
    }
    fire(env, "UserPromptSubmit", None, json!({"prompt": prompt})).await
}

/// Stop / SubagentStop. A block asks the agent to keep going.
pub async fn stop(env: &Env, event: &str, last: &str, active: bool) -> HookResult {
    if !has(env, event) {
        return HookResult::default();
    }
    let last = crate::util::truncate(last, 10_000);
    // `last_assistant_message` is Claude Code's name; `last_message` is kept for older hooks.
    fire(
        env,
        event,
        None,
        json!({"last_assistant_message": last, "last_message": last, "stop_hook_active": active}),
    )
    .await
}

/// Fire-and-collect for events without a decision (SessionStart, SessionEnd,
/// PreCompact, SubagentStart, Notification).
pub async fn notify(env: &Env, event: &str, subject: Option<&str>, fields: Value) -> HookResult {
    if !has(env, event) {
        return HookResult::default();
    }
    fire(env, event, subject, fields).await
}

/// Setup (`--init-only`, or `-p` with `--init` / `--maintenance`): command
/// hooks only, matched on `trigger`; it can't block and its output is ignored.
pub async fn setup(env: &Env, trigger: &str) {
    notify(env, "Setup", Some(trigger), json!({"trigger": trigger})).await;
}

/// A watched settings file: its source, path, and last-seen state.
type ConfigStamp = (&'static str, PathBuf, Option<(SystemTime, u64)>);

/// Per-session state the newer events need.
pub struct HookState {
    /// The session's `Env`, for hooks that run in the background.
    me: OnceLock<Weak<Env>>,
    /// FileChanged: absolute paths hooks asked to watch (`watchPaths`).
    watch_paths: Mutex<Vec<PathBuf>>,
    watcher: AtomicBool,
    /// Settings files as of the last (re)load, for ConfigChange.
    config_stamps: Mutex<Vec<ConfigStamp>>,
    /// The model PostModelSwitch hooks were last told about.
    model_seen: Mutex<String>,
    /// When the last model response arrived (is the prompt cache warm?).
    pub last_call: Mutex<Option<Instant>>,
    /// MessageDisplay: the turn being displayed.
    pub turn_id: Mutex<String>,
    /// Where `CLAUDE_ENV_FILE`s live (None: `<chanto home>/session-env`).
    pub env_dir: Mutex<Option<PathBuf>>,
    /// Hooks registered by invoked skills, kept for the rest of the session.
    pub session_hooks: Mutex<crate::config::HooksConfig>,
}

impl HookState {
    pub fn new(model: &str) -> HookState {
        HookState {
            me: OnceLock::new(),
            watch_paths: Mutex::new(vec![]),
            watcher: AtomicBool::new(false),
            config_stamps: Mutex::new(vec![]),
            model_seen: Mutex::new(model.to_string()),
            last_call: Mutex::new(None),
            turn_id: Mutex::new(uuid::Uuid::new_v4().to_string()),
            env_dir: Mutex::new(None),
            session_hooks: Mutex::new(Default::default()),
        }
    }
}

/// Wire the session up for hooks that run outside a turn: MCP elicitation,
/// the FileChanged watcher, ConfigChange's baseline. Done once, when the
/// session's first agent is made (after that the `Env` can't be changed).
pub fn attach(env: &Arc<Env>) {
    if env.hook_state.me.set(Arc::downgrade(env)).is_err() {
        return;
    }
    for c in &env.mcp {
        c.set_elicit_hooks(Arc::new(ElicitHooks(Arc::downgrade(env))));
    }
    *env.hook_state.config_stamps.lock().unwrap() = config_stamps(&env.root);
}

fn session(env: &Env) -> Option<Arc<Env>> {
    env.hook_state.me.get().and_then(Weak::upgrade)
}

/// Run `event` in the background (events that don't wait for their hooks).
fn fire_in_background(
    env: &Env,
    event: &'static str,
    subject: Option<String>,
    fields: Value,
    then: impl FnOnce(&Env, HookResult) + Send + 'static,
) {
    if !has(env, event) {
        return;
    }
    let Some(env) = session(env) else { return };
    tokio::spawn(async move {
        let r = fire(&env, event, subject.as_deref(), fields).await;
        then(&env, r);
    });
}

/// UserPromptExpansion: a typed `/command` (custom command, skill or MCP
/// prompt) is about to expand into a prompt. `Err(reason)` blocks it; hook
/// context rides along with the expanded prompt.
pub async fn user_prompt_expansion(
    env: &Env,
    expansion_type: &str,
    command_name: &str,
    command_args: &str,
    command_source: &str,
) -> Result<(), String> {
    if !has(env, "UserPromptExpansion") {
        return Ok(());
    }
    let prompt = if command_args.is_empty() {
        format!("/{command_name}")
    } else {
        format!("/{command_name} {command_args}")
    };
    let fields = json!({
        "expansion_type": expansion_type,
        "command_name": command_name,
        "command_args": command_args,
        "command_source": command_source,
        "prompt": prompt,
    });
    let r = fire(env, "UserPromptExpansion", Some(command_name), fields).await;
    if let Some(reason) = r.block.clone().or(r.halt.clone()) {
        return Err(reason);
    }
    if let Some(c) = r.context_text() {
        env.pending_context.lock().unwrap().push(c);
    }
    Ok(())
}

/// PermissionDenied: auto mode refused a tool call. True when a hook says
/// the model may retry it.
pub async fn permission_denied(
    env: &Env,
    tool: &str,
    input: &Value,
    tool_use_id: &str,
    reason: &str,
) -> bool {
    if !has(env, "PermissionDenied") {
        return false;
    }
    let fields = json!({"tool_name": tool, "tool_input": hook_input(tool, input), "tool_use_id": tool_use_id, "reason": reason});
    let r = fire(env, "PermissionDenied", Some(tool), fields).await;
    r.json
        .iter()
        .any(|v| v["hookSpecificOutput"]["retry"] == json!(true))
}

/// PostToolBatch: every call of a batch has its result, before the next
/// model call. `tool_calls` items carry tool_name, tool_input, tool_use_id
/// and tool_response (the text the model receives).
pub async fn post_tool_batch(env: &Env, tool_calls: Vec<Value>) -> HookResult {
    if !has(env, "PostToolBatch") {
        return HookResult::default();
    }
    fire(
        env,
        "PostToolBatch",
        None,
        json!({"tool_calls": tool_calls}),
    )
    .await
}

/// Claude Code's StopFailure error types, from a model request's error text.
pub fn error_kind(msg: &str) -> &'static str {
    let m = msg.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| m.contains(w));
    if has(&["429", "rate limit", "rate_limit", "too many requests"]) {
        "rate_limit"
    } else if has(&["overloaded", "529"]) {
        "overloaded"
    } else if has(&[
        "401",
        "unauthorized",
        "authentication",
        "invalid api key",
        "invalid x-api-key",
        "api key",
    ]) {
        "authentication_failed"
    } else if has(&["402", "billing", "credit", "quota", "payment"]) {
        "billing_error"
    } else if has(&[
        "model not found",
        "model_not_found",
        "no such model",
        "unknown model",
        "does not exist",
    ]) || (m.contains("404") && m.contains("model"))
    {
        "model_not_found"
    } else if has(&["maximum context", "max_tokens", "output token limit"]) {
        "max_output_tokens"
    } else if has(&["400", "invalid_request", "bad request"]) {
        "invalid_request"
    } else if has(&["500", "502", "503", "504", "server error", "internal error"]) {
        "server_error"
    } else {
        "unknown"
    }
}

/// StopFailure: the turn ended on a model request error. Output is ignored.
pub async fn stop_failure(env: &Env, msg: &str) {
    if !has(env, "StopFailure") {
        return;
    }
    let kind = error_kind(msg);
    let details = crate::llm::root_cause(msg);
    let fields = json!({"error": kind, "error_details": details, "last_assistant_message": format!("API Error: {}", crate::util::one_line(msg, 500))});
    fire(env, "StopFailure", Some(kind), fields).await;
}

/// MessageDisplay: show the assistant's text through the hooks. Text sent to
/// the returned channel is shown in batches of whole lines (the whole message
/// at once in non-interactive runs), each replaced by the hooks'
/// `displayContent` when they return one; await the handle after closing the
/// channel. Display-only: the conversation keeps the original.
pub fn message_display(
    env: &Arc<Env>,
) -> (
    tokio::sync::mpsc::UnboundedSender<String>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let env = env.clone();
    let turn_id = env.hook_state.turn_id.lock().unwrap().clone();
    let message_id = uuid::Uuid::new_v4().to_string();
    let handle = tokio::spawn(async move {
        let (mut buf, mut index, mut any) = (String::new(), 0u64, false);
        let show = |delta: String, index: u64, last: bool| {
            let (env, turn_id, message_id) = (env.clone(), turn_id.clone(), message_id.clone());
            async move {
                let fields = json!({"turn_id": turn_id, "message_id": message_id, "index": index, "final": last, "delta": delta});
                let r = fire(&env, "MessageDisplay", None, fields).await;
                let shown = r
                    .json
                    .iter()
                    .rev()
                    .find_map(|v| {
                        v["hookSpecificOutput"]["displayContent"]
                            .as_str()
                            .map(str::to_string)
                    })
                    .unwrap_or(delta);
                if !shown.is_empty() {
                    env.emit(crate::agent::Event::Text(shown));
                }
            }
        };
        while let Some(t) = rx.recv().await {
            any |= !t.is_empty();
            buf.push_str(&t);
            // Gather whatever else arrived while the last batch was out.
            while let Ok(t) = rx.try_recv() {
                buf.push_str(&t);
            }
            if env.interactive
                && let Some(i) = buf.rfind('\n')
            {
                let rest = buf.split_off(i + 1);
                let delta = std::mem::replace(&mut buf, rest);
                show(delta, index, false).await;
                index += 1;
            }
        }
        if any {
            show(buf, index, true).await;
        }
    });
    (tx, handle)
}

/// TaskCreated / TaskCompleted for TodoWrite: new items are tasks being
/// created, items turning `completed` tasks being completed. Returns the list
/// with blocked changes undone, and the hooks' messages for the model.
pub async fn todo_changes(
    env: &Env,
    old: &[crate::tools::misc::Todo],
    mut new: Vec<crate::tools::misc::Todo>,
) -> (Vec<crate::tools::misc::Todo>, Vec<String>) {
    let mut feedback = Vec::new();
    if !has(env, "TaskCreated") && !has(env, "TaskCompleted") {
        return (new, feedback);
    }
    let mut keep = vec![true; new.len()];
    for (i, t) in new.iter_mut().enumerate() {
        let before = old.iter().find(|o| o.content == t.content);
        let fields = |t: &crate::tools::misc::Todo| {
            let mut f = json!({"task_id": (i + 1).to_string(), "task_subject": t.content});
            if !t.active_form.is_empty() && t.active_form != t.content {
                f["task_description"] = json!(t.active_form);
            }
            f
        };
        if before.is_none()
            && has(env, "TaskCreated")
            && let Some(why) = fire(env, "TaskCreated", None, fields(t)).await.block
        {
            keep[i] = false;
            feedback.push(format!(
                "Task \"{}\" was not created (TaskCreated hook): {why}",
                t.content
            ));
            continue;
        }
        if t.status == "completed"
            && before.is_none_or(|b| b.status != "completed")
            && has(env, "TaskCompleted")
            && let Some(why) = fire(env, "TaskCompleted", None, fields(t)).await.block
        {
            t.status = before
                .map(|b| b.status.clone())
                .filter(|s| s != "completed")
                .unwrap_or_else(|| "in_progress".into());
            feedback.push(format!(
                "Task \"{}\" was not marked completed (TaskCompleted hook): {why}",
                t.content
            ));
        }
    }
    let mut keep = keep.into_iter();
    new.retain(|_| keep.next().unwrap_or(true));
    (new, feedback)
}

/// TeammateIdle: a team sub-task's run finished its work. `block` sends it
/// back to work with that feedback; `halt` stops it.
pub async fn teammate_idle(env: &Env, teammate: &str, team: &str) -> HookResult {
    if !has(env, "TeammateIdle") {
        return HookResult::default();
    }
    fire(
        env,
        "TeammateIdle",
        None,
        json!({"teammate_name": teammate, "team_name": team}),
    )
    .await
}

/// `memory_type` of an instruction file.
fn memory_type(root: &std::path::Path, p: &std::path::Path) -> &'static str {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if name.contains(".local.") {
        "Local"
    } else if p.starts_with(crate::compat::managed_dir()) {
        "Managed"
    } else if p.starts_with(root)
        || !(p.starts_with(crate::compat::home()) || p.starts_with(crate::config::home_dir()))
    {
        "Project"
    } else if p.starts_with(crate::compat::home().join(".claude"))
        || p.starts_with(crate::config::home_dir())
        || p.starts_with(crate::compat::codex_dir())
    {
        "User"
    } else {
        "Project"
    }
}

/// InstructionsLoaded for instruction files the session loaded. Runs in the
/// background; output is ignored. `reason` is `session_start`, `compact`,
/// `nested_traversal` or `path_glob_match`.
pub fn instructions_loaded(
    env: &Env,
    files: Vec<(PathBuf, &'static str)>,
    trigger: Option<PathBuf>,
) {
    if files.is_empty() || !has(env, "InstructionsLoaded") {
        return;
    }
    let root = env.root.clone();
    let Some(env) = session(env) else { return };
    tokio::spawn(async move {
        for (p, reason) in files {
            let mut f = json!({"file_path": p, "memory_type": memory_type(&root, &p), "load_reason": reason});
            if reason == "path_glob_match" {
                let globs = std::fs::read_to_string(&p)
                    .map(|t| {
                        crate::context::fm_list(&crate::context::frontmatter(&t), "paths")
                            .and_then(Result::ok)
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                f["globs"] = json!(globs);
            }
            if let Some(t) = &trigger {
                f["trigger_file_path"] = json!(t);
            }
            fire(&env, "InstructionsLoaded", Some(reason), f).await;
        }
    });
}

fn stamp(p: &std::path::Path) -> Option<(SystemTime, u64)> {
    let m = std::fs::metadata(p).ok()?;
    if m.is_dir() {
        // A directory (skills): the newest file under it, and how many there are.
        let mut newest = (m.modified().ok()?, 0u64);
        let mut stack = vec![p.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let Ok(md) = e.metadata() else { continue };
                if md.is_dir() {
                    if stack.len() < 200 {
                        stack.push(e.path());
                    }
                } else {
                    newest.1 += 1;
                    if let Ok(t) = md.modified() {
                        newest.0 = newest.0.max(t);
                    }
                }
            }
        }
        return Some(newest);
    }
    Some((m.modified().ok()?, m.len()))
}

/// The settings files ConfigChange watches, by source, with their state now.
fn config_stamps(root: &std::path::Path) -> Vec<ConfigStamp> {
    let home = crate::compat::home();
    let mut files: Vec<(&'static str, PathBuf)> = vec![
        ("user_settings", home.join(".claude/settings.json")),
        (
            "user_settings",
            crate::config::home_dir().join("config.toml"),
        ),
        ("project_settings", root.join(".claude/settings.json")),
        ("project_settings", root.join(".chanto/config.toml")),
        ("local_settings", root.join(".claude/settings.local.json")),
        ("local_settings", root.join(".chanto/config.local.toml")),
        (
            "policy_settings",
            crate::compat::managed_dir().join("managed-settings.json"),
        ),
        ("skills", home.join(".claude/skills")),
        ("skills", root.join(".claude/skills")),
    ];
    let mut d: Vec<PathBuf> =
        std::fs::read_dir(crate::compat::managed_dir().join("managed-settings.d"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect();
    d.sort();
    files.extend(d.into_iter().map(|p| ("policy_settings", p)));
    files
        .into_iter()
        .map(|(s, p)| {
            let st = stamp(&p);
            (s, p, st)
        })
        .collect()
}

/// ConfigChange, before `/reload` applies changed settings: runs for each
/// settings file that changed since they were last loaded. `Err` names the
/// change a hook blocked (policy changes can't be blocked).
pub async fn config_change(env: &Env) -> Result<(), String> {
    let now = config_stamps(&env.root);
    let before = env.hook_state.config_stamps.lock().unwrap().clone();
    let mut changed: Vec<(&'static str, PathBuf)> = now
        .iter()
        .filter(|(_, p, s)| before.iter().find(|(_, q, _)| q == p).map(|(_, _, t)| t) != Some(s))
        .map(|(src, p, _)| (*src, p.clone()))
        .collect();
    // Removed files changed too.
    changed.extend(
        before
            .iter()
            .filter(|(_, p, s)| s.is_some() && !now.iter().any(|(_, q, _)| q == p))
            .map(|(src, p, _)| (*src, p.clone())),
    );
    if has(env, "ConfigChange") {
        for (source, path) in changed {
            let r = fire(
                env,
                "ConfigChange",
                Some(source),
                json!({"source": source, "file_path": path}),
            )
            .await;
            if r.block.is_some() && source != "policy_settings" {
                return Err(format!(
                    "A ConfigChange hook blocked the change to {}; kept the current settings.",
                    path.display()
                ));
            }
        }
    }
    *env.hook_state.config_stamps.lock().unwrap() = now;
    Ok(())
}

/// CwdChanged: a shell command moved the session's working directory.
pub async fn cwd_changed(env: &Env, old: &std::path::Path, new: &std::path::Path) {
    if !has(env, "CwdChanged") {
        return;
    }
    // What CwdChanged / FileChanged hooks persisted lasts until the next CwdChanged.
    if let Some((_, f)) = env_files(env)
        && f.exists()
    {
        let _ = std::fs::write(&f, "");
    }
    let r = fire(
        env,
        "CwdChanged",
        None,
        json!({"old_cwd": old, "new_cwd": new}),
    )
    .await;
    set_watch_paths(env, &r);
}

/// `watchPaths` from a hook replaces FileChanged's dynamic watch list.
pub fn set_watch_paths(env: &Env, r: &HookResult) {
    let Some(paths) = r.json.iter().rev().find_map(|v| {
        v["hookSpecificOutput"]["watchPaths"]
            .as_array()
            .or_else(|| v["watchPaths"].as_array())
            .cloned()
    }) else {
        return;
    };
    *env.hook_state.watch_paths.lock().unwrap() = paths
        .iter()
        .filter_map(|p| p.as_str())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .collect();
    start_file_watcher(env);
}

/// DirectoryAdded: `/add-dir` added a working directory. Runs in the
/// background; a `systemMessage` reaches the model with the next prompt.
pub fn directory_added(env: &Env, dir: &std::path::Path) {
    let fields = json!({"directory": dir, "source": "slash_command"});
    fire_in_background(
        env,
        "DirectoryAdded",
        Some("slash_command".into()),
        fields,
        |env, r| {
            let msgs: Vec<String> = r
                .json
                .iter()
                .filter_map(|v| v["systemMessage"].as_str().map(str::to_string))
                .collect();
            if !msgs.is_empty() {
                env.pending_context.lock().unwrap().push(msgs.join("\n"));
            }
            if r.failed || r.block.is_some() || r.timed_out {
                env.emit(crate::agent::Event::Warn(
                    "1 DirectoryAdded hook failed".into(),
                ));
            }
        },
    );
}

/// FileChanged's watch list: the files named by the hooks' matchers (in the
/// working directory) plus the paths hooks returned as `watchPaths`.
fn watch_list(env: &Env) -> Vec<PathBuf> {
    let cwd = env.cwd();
    let mut out: Vec<PathBuf> = env
        .hooks
        .read()
        .unwrap()
        .get("FileChanged")
        .into_iter()
        .flatten()
        .flat_map(|h| {
            h.matcher
                .split('|')
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(|n| cwd.join(n))
                .collect::<Vec<_>>()
        })
        .collect();
    out.extend(env.hook_state.watch_paths.lock().unwrap().iter().cloned());
    out.sort();
    out.dedup();
    out
}

/// Start polling the watch list for changes (once per session), when there
/// is something to watch.
pub fn start_file_watcher(env: &Env) {
    if !has(env, "FileChanged")
        || watch_list(env).is_empty()
        || env.hook_state.watcher.swap(true, Ordering::SeqCst)
    {
        return;
    }
    let Some(me) = env.hook_state.me.get().cloned() else {
        return;
    };
    let tick = if cfg!(test) {
        Duration::from_millis(100)
    } else {
        Duration::from_millis(500)
    };
    tokio::spawn(async move {
        let mut seen: std::collections::HashMap<PathBuf, Option<(SystemTime, u64)>> =
            Default::default();
        loop {
            let Some(env) = me.upgrade() else { return };
            for p in watch_list(&env) {
                let now = std::fs::metadata(&p)
                    .ok()
                    .filter(|m| m.is_file())
                    .map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
                let Some(before) = seen.insert(p.clone(), now) else {
                    continue;
                };
                let event = match (before, now) {
                    (None, Some(_)) => "add",
                    (Some(_), None) => "unlink",
                    (Some(a), Some(b)) if a != b => "change",
                    _ => continue,
                };
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let r = fire(
                    &env,
                    "FileChanged",
                    Some(&name),
                    json!({"file_path": p, "event": event}),
                )
                .await;
                set_watch_paths(&env, &r);
            }
            drop(env);
            tokio::time::sleep(tick).await;
        }
    });
}

/// PostCompact: compaction finished. Output is ignored (stderr shown).
pub async fn post_compact(env: &Env, trigger: &str, summary: &str) {
    if has(env, "PostCompact") {
        fire(
            env,
            "PostCompact",
            Some(trigger),
            json!({"trigger": trigger, "compact_summary": summary}),
        )
        .await;
    }
}

/// The name model-switch matchers compare against: the model without a
/// `provider/` prefix or a `[1m]` suffix.
fn canonical_model(spec: &str) -> String {
    let m = spec.trim_end_matches("[1m]");
    m.rsplit_once('/').map(|(_, m)| m).unwrap_or(m).to_string()
}

/// Fields PreModelSwitch / PostModelSwitch hooks get.
fn model_switch_fields(
    env: &Env,
    from: &str,
    to: &str,
    requested: Option<&str>,
    source: &str,
) -> Value {
    let context_tokens = crate::llm::estimate_tokens(&env.conversation.read().unwrap());
    let warm = context_tokens > 0
        && env
            .hook_state
            .last_call
            .lock()
            .unwrap()
            .is_some_and(|t| t.elapsed() < Duration::from_secs(300));
    let (cost, pricing) = env.estimate_cache_write(to, context_tokens);
    json!({
        "from_model": from,
        "to_model": to,
        "requested_model": requested,
        "source": source,
        "context_tokens": context_tokens,
        "prompt_cache_warm": warm,
        "cache_ttl": "5m",
        "estimated_cache_write_usd": (cost * 10_000.0).round() / 10_000.0,
        "pricing": pricing,
    })
}

/// Switch the session's model the user (or a client) asked for, through
/// PreModelSwitch hooks (which can refuse it, or ask the user to confirm)
/// and then PostModelSwitch hooks. `source`: `command`, `picker` or `sdk`.
pub async fn switch_model(env: &Env, requested: &str, source: &str) -> Result<(), String> {
    let from = env.model();
    if has(env, "PreModelSwitch") {
        let fields = model_switch_fields(env, &from, requested, Some(requested), source);
        let r = fire(
            env,
            "PreModelSwitch",
            Some(&canonical_model(requested)),
            fields,
        )
        .await;
        let refused = |why: String| {
            Err(if why.is_empty() {
                "A PreModelSwitch hook blocked the switch.".to_string()
            } else {
                format!("A PreModelSwitch hook blocked the switch: {why}")
            })
        };
        if let Some(why) = r.block {
            return refused(why);
        }
        if r.timed_out {
            return refused("it didn't answer in time".into());
        }
        match r.decision {
            Some(PermDecision::Deny(why)) => return refused(why),
            Some(PermDecision::Ask(why)) => {
                let q = if why.is_empty() {
                    format!("Switch the model to {requested}?")
                } else {
                    why
                };
                // Only an interactive `/model` can ask; elsewhere `ask` refuses.
                let yes = env.interactive
                    && source == "command"
                    && env
                        .frontend
                        .choose(&q, &["Switch".into(), "Cancel".into()])
                        .await
                        .as_deref()
                        == Some("Switch");
                if !yes {
                    return Err(format!(
                        "Kept {from}: the switch to {requested} was not confirmed."
                    ));
                }
            }
            _ => {}
        }
    }
    env.set_model(requested);
    model_switched(env, &from, Some(requested), source).await;
    Ok(())
}

/// PostModelSwitch after the session's model changed; its output reaches the
/// model with the next prompt. Nothing fires if the model is unchanged.
pub async fn model_switched(env: &Env, from: &str, requested: Option<&str>, source: &str) {
    let to = env.model();
    *env.hook_state.model_seen.lock().unwrap() = to.clone();
    if from == to || !has(env, "PostModelSwitch") {
        return;
    }
    let fields = model_switch_fields(env, from, &to, requested, source);
    let r = fire(env, "PostModelSwitch", Some(&canonical_model(&to)), fields).await;
    if let Some(c) = r.context_text() {
        env.pending_context.lock().unwrap().push(c);
    }
}

/// A resumed session restored its saved model.
pub async fn model_restored(env: &Env) {
    let seen = env.hook_state.model_seen.lock().unwrap().clone();
    let cur = env.model();
    model_switched(env, &seen, Some(&cur), "resume").await;
}

/// MCP elicitation through Elicitation / ElicitationResult hooks.
struct ElicitHooks(Weak<Env>);

/// The `action` a set of hook results settles on: a block (exit 2,
/// `decision: "block"`) or any `decline` declines; else the first
/// `accept` / `cancel`, with its `content`.
fn elicit_action(r: &HookResult) -> Option<Value> {
    if r.block.is_some() {
        return Some(json!({"action": "decline"}));
    }
    let actions: Vec<&Value> = r
        .json
        .iter()
        .map(|v| &v["hookSpecificOutput"])
        .filter(|h| h["action"].is_string())
        .collect();
    if actions.iter().any(|h| h["action"] == "decline") {
        return Some(json!({"action": "decline"}));
    }
    let h = actions.first()?;
    let action = h["action"].as_str().unwrap_or("");
    if !matches!(action, "accept" | "cancel") {
        return None;
    }
    let mut out = json!({"action": action});
    if action == "accept" {
        out["content"] = if h["content"].is_object() {
            h["content"].clone()
        } else {
            json!({})
        };
    }
    Some(out)
}

#[async_trait::async_trait]
impl crate::mcp::ElicitHooks for ElicitHooks {
    async fn before(&self, server: &str, params: &Value) -> Option<Value> {
        let env = self.0.upgrade()?;
        if !has(&env, "Elicitation") {
            return None;
        }
        let mut f = json!({"mcp_server_name": server, "message": params["message"], "mode": params["mode"].as_str().unwrap_or("form")});
        for (k, from) in [
            ("url", "url"),
            ("elicitation_id", "elicitationId"),
            ("requested_schema", "requestedSchema"),
        ] {
            if !params[from].is_null() {
                f[k] = params[from].clone();
            }
        }
        let r = fire(&env, "Elicitation", Some(server), f).await;
        elicit_action(&r)
    }
    async fn after(&self, server: &str, params: &Value, result: Value) -> Value {
        let Some(env) = self.0.upgrade() else {
            return result;
        };
        if !has(&env, "ElicitationResult") {
            return result;
        }
        let mut f = json!({"mcp_server_name": server, "action": result["action"], "mode": params["mode"].as_str().unwrap_or("form")});
        if !result["content"].is_null() {
            f["content"] = result["content"].clone();
        }
        if !params["elicitationId"].is_null() {
            f["elicitation_id"] = params["elicitationId"].clone();
        }
        let r = fire(&env, "ElicitationResult", Some(server), f).await;
        match elicit_action(&r) {
            // `cancel` still carries what the user submitted.
            Some(v) if v["action"] == "cancel" && !result["content"].is_null() => {
                json!({"action": "cancel", "content": result["content"]})
            }
            Some(v) => v,
            None => result,
        }
    }
}

#[cfg(test)]
mod tests;

/// The newer hook events, end to end: command hooks log their stdin to a
/// file (one JSON payload per line) and answer as Claude Code's docs say.
#[cfg(test)]
mod handler_tests;

#[cfg(all(test, unix))]
mod event_tests {
    use super::*;
    use crate::agent::tests::{mock_server, tc, tmpdir};
    use crate::agent::{
        Agent, EnvInit, Event, Frontend, PermissionReply, PermissionRequest, TurnEnd,
    };
    use crate::config::Config;
    use crate::llm::{Provider, ProviderKind};
    use crate::permissions::Mode;
    use std::path::Path;
    use tokio_util::sync::CancellationToken;

    /// Records shown text; answers every choice with `answer`.
    #[derive(Default)]
    struct Rec {
        text: Mutex<Vec<String>>,
        answer: Option<String>,
    }

    #[async_trait::async_trait]
    impl Frontend for Rec {
        fn emit(&self, ev: Event) {
            if let Event::Text(t) = ev {
                self.text.lock().unwrap().push(t);
            }
        }
        async fn permission(&self, _: PermissionRequest) -> PermissionReply {
            PermissionReply::Deny(None)
        }
        async fn question(&self, _: &str, _: &[String]) -> Option<String> {
            self.answer.clone()
        }
    }

    fn env_with(
        base: &str,
        dir: &Path,
        mode: Mode,
        fe: Arc<Rec>,
        interactive: bool,
        hooks: &[(&str, &str, String)],
    ) -> Arc<Env> {
        let mut config = Config::default();
        config.provider = ProviderKind::Openai;
        for (event, matcher, command) in hooks {
            config
                .hooks
                .entry(event.to_string())
                .or_default()
                .push(HookConfig {
                    matcher: matcher.to_string(),
                    command: command.clone(),
                    timeout_secs: Some(20),
                    ..Default::default()
                });
        }
        let env = Env::new(EnvInit {
            config,
            provider: Provider::new(ProviderKind::Openai, base, None, 32768),
            model: "mock".into(),
            mode,
            vision: false,
            cwd: dir.to_path_buf(),
            tools: crate::tools::Registry::builtin(),
            mcp: vec![],
            frontend: fe,
            interactive,
        });
        attach(&env);
        env
    }

    fn env(base: &str, dir: &Path, mode: Mode, hooks: &[(&str, &str, String)]) -> Arc<Env> {
        env_with(base, dir, mode, Arc::new(Rec::default()), false, hooks)
    }

    /// A hook command that appends its stdin to `log`, then runs `then`.
    fn log_then(log: &Path, then: &str) -> String {
        format!(
            "cat >> '{}'; echo >> '{}'; {then}",
            log.display(),
            log.display()
        )
    }

    fn logged(log: &Path) -> Vec<Value> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Wait for background hooks to log `n` payloads.
    async fn logged_n(log: &Path, n: usize) -> Vec<Value> {
        for _ in 0..100 {
            let v = logged(log);
            if v.len() >= n {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        logged(log)
    }

    #[test]
    fn every_claude_code_event_is_known() {
        for e in [
            "SessionStart",
            "Setup",
            "UserPromptSubmit",
            "UserPromptExpansion",
            "PreToolUse",
            "PermissionRequest",
            "PermissionDenied",
            "PostToolUse",
            "PostToolUseFailure",
            "PostToolBatch",
            "Notification",
            "MessageDisplay",
            "SubagentStart",
            "SubagentStop",
            "TaskCreated",
            "TaskCompleted",
            "Stop",
            "StopFailure",
            "TeammateIdle",
            "InstructionsLoaded",
            "ConfigChange",
            "CwdChanged",
            "DirectoryAdded",
            "FileChanged",
            "WorktreeCreate",
            "WorktreeRemove",
            "PreCompact",
            "PostCompact",
            "PreModelSwitch",
            "PostModelSwitch",
            "Elicitation",
            "ElicitationResult",
            "SessionEnd",
        ] {
            assert!(crate::config::HOOK_EVENTS.contains(&e), "{e}");
        }
    }

    #[test]
    fn stop_failure_error_types() {
        assert_eq!(error_kind("HTTP 429 Too Many Requests"), "rate_limit");
        assert_eq!(error_kind("overloaded_error: Overloaded"), "overloaded");
        assert_eq!(
            error_kind("401 Unauthorized: invalid api key"),
            "authentication_failed"
        );
        assert_eq!(
            error_kind("402 Payment Required: insufficient credits"),
            "billing_error"
        );
        assert_eq!(
            error_kind("404: model not found: llama9"),
            "model_not_found"
        );
        assert_eq!(error_kind("400 Bad Request"), "invalid_request");
        assert_eq!(error_kind("503 Service Unavailable"), "server_error");
        assert_eq!(error_kind("something odd"), "unknown");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_hook_that_starts_a_background_process_returns_when_it_exits() {
        // The background `sleep` keeps the hook's stdout open; the session used
        // to wait for it (up to the timeout) and then lose the hook's output.
        let dir = tmpdir("hook-bg");
        let h = HookConfig {
            command: "sleep 30 & echo started; echo warn >&2".into(),
            timeout_secs: Some(20),
            ..Default::default()
        };
        let t = Instant::now();
        let r = run(&h, &json!({"hook_event_name": "SessionStart"}), &dir, None).await;
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        assert_eq!(r, Some((0, "started".to_string(), "warn".to_string())));
        // A hook that really hangs still times out.
        let h = HookConfig {
            command: "sleep 30".into(),
            timeout_secs: Some(1),
            ..Default::default()
        };
        assert_eq!(
            run(&h, &json!({"hook_event_name": "PreToolUse"}), &dir, None).await,
            None
        );
    }

    #[tokio::test]
    async fn post_tool_batch_sees_every_result_and_adds_context() {
        let dir = tmpdir("ptb");
        let log = dir.join("log");
        let (base, seen) = mock_server(vec![
            json!({"tool_calls": tc("Bash", json!({"command": "echo batch-out"}))}),
            json!({"content": "done"}),
        ])
        .await;
        let hook = log_then(
            &log,
            r#"echo '{"hookSpecificOutput": {"hookEventName": "PostToolBatch", "additionalContext": "BATCH-CONTEXT"}}'"#,
        );
        let env = env(&base, &dir, Mode::Bypass, &[("PostToolBatch", "", hook)]);
        let mut agent = Agent::new(env);
        let end = agent
            .run_turn("go", vec![], CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(end, TurnEnd::Done(t) if t == "done"));
        let p = &logged(&log)[0];
        assert_eq!(p["hook_event_name"], "PostToolBatch");
        assert_eq!(p["tool_calls"][0]["tool_name"], "Bash");
        assert_eq!(p["tool_calls"][0]["tool_use_id"], "c1");
        assert_eq!(
            p["tool_calls"][0]["tool_input"]["command"],
            "echo batch-out"
        );
        assert!(
            p["tool_calls"][0]["tool_response"]
                .as_str()
                .unwrap()
                .contains("batch-out"),
            "{p}"
        );
        assert!(
            seen.lock().unwrap()[1]["messages"]
                .to_string()
                .contains("BATCH-CONTEXT")
        );
    }

    #[tokio::test]
    async fn post_tool_batch_block_ends_the_turn() {
        let dir = tmpdir("ptb-block");
        let (base, seen) = mock_server(vec![
            json!({"tool_calls": tc("Bash", json!({"command": "true"}))}),
            json!({"content": "never asked"}),
        ])
        .await;
        let env = env(
            &base,
            &dir,
            Mode::Bypass,
            &[(
                "PostToolBatch",
                "",
                "echo 'enough for now' >&2; exit 2".into(),
            )],
        );
        let mut agent = Agent::new(env);
        agent
            .run_turn("go", vec![], CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "no model call after the block"
        );
        assert!(
            agent
                .messages
                .last()
                .unwrap()
                .content
                .contains("enough for now"),
            "the reason stays in the conversation"
        );
    }

    #[tokio::test]
    async fn permission_denied_hook_can_let_the_model_retry() {
        let dir = tmpdir("permdenied");
        let log = dir.join("log");
        let (base, seen) = mock_server(vec![
            json!({"tool_calls": tc("Bash", json!({"command": "echo hi > out.txt"}))}),
            json!({"content": "{\"decision\":\"block\",\"reason\":\"not requested\"}"}),
            json!({"content": "ok"}),
        ])
        .await;
        let hook = log_then(
            &log,
            r#"echo '{"hookSpecificOutput": {"hookEventName": "PermissionDenied", "retry": true}}'"#,
        );
        let env = env(
            &base,
            &dir,
            Mode::Auto,
            &[("PermissionDenied", "Bash", hook)],
        );
        let mut agent = Agent::new(env);
        agent
            .run_turn("write some notes", vec![], CancellationToken::new())
            .await
            .unwrap();
        assert!(!dir.join("out.txt").exists());
        let p = &logged(&log)[0];
        assert_eq!(p["tool_name"], "Bash");
        assert_eq!(p["tool_use_id"], "c1");
        assert_eq!(p["permission_mode"], "auto");
        assert!(
            p["reason"].as_str().unwrap().contains("not requested"),
            "{p}"
        );
        assert!(
            seen.lock().unwrap()[2]["messages"]
                .to_string()
                .contains("may retry this tool call")
        );
    }

    #[tokio::test]
    async fn stop_failure_fires_when_the_model_request_fails() {
        let dir = tmpdir("stopfail");
        let log = dir.join("log");
        // Nothing listens on this port.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let env = env(
            &format!("http://127.0.0.1:{port}/v1"),
            &dir,
            Mode::Bypass,
            &[
                ("StopFailure", "", log_then(&log, "exit 2")),
                ("Stop", "", log_then(&log, "true")),
            ],
        );
        let mut agent = Agent::new(env);
        assert!(
            agent
                .run_turn("hi", vec![], CancellationToken::new())
                .await
                .is_err()
        );
        let all = logged(&log);
        assert_eq!(all.len(), 1, "Stop doesn't fire: {all:?}");
        assert_eq!(all[0]["hook_event_name"], "StopFailure");
        assert!(all[0]["error"].is_string());
        assert!(
            all[0]["last_assistant_message"]
                .as_str()
                .unwrap()
                .starts_with("API Error")
        );
    }

    #[tokio::test]
    async fn message_display_replaces_shown_text_only() {
        let dir = tmpdir("msgdisplay");
        let log = dir.join("log");
        let (base, _) = mock_server(vec![
            json!({"content": "line one\nline two"}),
            json!({"content": "line one\nline two"}),
        ])
        .await;
        let hook = log_then(
            &log,
            r#"echo '{"hookSpecificOutput": {"hookEventName": "MessageDisplay", "displayContent": "SHOWN "}}'"#,
        );
        // Non-interactive: one call per message, with the whole text.
        let fe = Arc::new(Rec::default());
        let env1 = env_with(
            &base,
            &dir,
            Mode::Bypass,
            fe.clone(),
            false,
            &[("MessageDisplay", "", hook.clone())],
        );
        let mut agent = Agent::new(env1);
        let end = agent
            .run_turn("hi", vec![], CancellationToken::new())
            .await
            .unwrap();
        assert!(
            matches!(end, TurnEnd::Done(t) if t == "line one\nline two"),
            "the conversation keeps the original"
        );
        assert_eq!(fe.text.lock().unwrap().join(""), "SHOWN ");
        let p = logged(&log);
        assert_eq!(p.len(), 1);
        assert_eq!(
            (
                p[0]["index"].as_u64(),
                p[0]["final"].as_bool(),
                p[0]["delta"].as_str()
            ),
            (Some(0), Some(true), Some("line one\nline two"))
        );
        assert!(p[0]["turn_id"].is_string() && p[0]["message_id"].is_string());
        // Interactive: whole lines as they complete, then the rest.
        std::fs::remove_file(&log).unwrap();
        let fe = Arc::new(Rec::default());
        let env2 = env_with(
            &base,
            &dir,
            Mode::Bypass,
            fe.clone(),
            true,
            &[("MessageDisplay", "", hook)],
        );
        let mut agent = Agent::new(env2);
        agent
            .run_turn("hi", vec![], CancellationToken::new())
            .await
            .unwrap();
        let p = logged(&log);
        assert_eq!(p.len(), 2, "{p:?}");
        assert_eq!(
            (p[0]["delta"].as_str(), p[0]["final"].as_bool()),
            (Some("line one\n"), Some(false))
        );
        assert_eq!(
            (
                p[1]["delta"].as_str(),
                p[1]["final"].as_bool(),
                p[1]["index"].as_u64()
            ),
            (Some("line two"), Some(true), Some(1))
        );
        assert_eq!(p[0]["message_id"], p[1]["message_id"]);
        assert_eq!(fe.text.lock().unwrap().join(""), "SHOWN SHOWN ");
    }

    #[tokio::test]
    async fn task_hooks_guard_todo_items() {
        let dir = tmpdir("tasks");
        let log = dir.join("log");
        let todos1 = json!({"todos": [{"content": "[T-1] add login", "status": "pending"}, {"content": "misc", "status": "pending"}]});
        let todos2 = json!({"todos": [{"content": "[T-1] add login", "status": "completed"}]});
        let (base, seen) = mock_server(vec![
            json!({"tool_calls": tc("TodoWrite", todos1)}),
            json!({"tool_calls": tc("TodoWrite", todos2)}),
            json!({"content": "done"}),
        ])
        .await;
        let created = log_then(
            &log,
            &format!(
                r#"tail -1 '{}' | grep -q '"task_subject":"\[T-' || {{ echo 'Task subjects need a ticket' >&2; exit 2; }}"#,
                log.display()
            ),
        );
        let completed = log_then(&log, "echo 'tests are failing' >&2; exit 2");
        let env = env(
            &base,
            &dir,
            Mode::Bypass,
            &[
                ("TaskCreated", "", created),
                ("TaskCompleted", "", completed),
            ],
        );
        let mut agent = Agent::new(env.clone());
        agent
            .run_turn("plan it", vec![], CancellationToken::new())
            .await
            .unwrap();
        let todos = env.todos.lock().unwrap().clone();
        assert_eq!(
            todos.len(),
            1,
            "the blocked task was not created: {todos:?}"
        );
        assert_eq!(todos[0].content, "[T-1] add login");
        assert_eq!(todos[0].status, "pending", "completion was blocked");
        let reqs = seen.lock().unwrap();
        assert!(
            reqs[1]["messages"]
                .to_string()
                .contains("Task subjects need a ticket")
        );
        assert!(
            reqs[2]["messages"]
                .to_string()
                .contains("tests are failing")
        );
        let p = logged(&log);
        assert_eq!(p[0]["hook_event_name"], "TaskCreated");
        assert_eq!(
            (p[0]["task_id"].as_str(), p[0]["task_subject"].as_str()),
            (Some("1"), Some("[T-1] add login"))
        );
        assert_eq!(p.last().unwrap()["hook_event_name"], "TaskCompleted");
    }

    #[tokio::test]
    async fn pre_compact_can_block_and_post_compact_gets_the_summary() {
        let dir = tmpdir("compact");
        let log = dir.join("log");
        let marker = dir.join("blocked-once");
        let (base, _) = mock_server(vec![json!({"content": "THE SUMMARY"})]).await;
        let pre = format!(
            "[ -f '{m}' ] && exit 0; touch '{m}'; echo 'not now' >&2; exit 2",
            m = marker.display()
        );
        let env = env(
            &base,
            &dir,
            Mode::Bypass,
            &[
                ("PreCompact", "manual", pre),
                ("PostCompact", "manual", log_then(&log, "")),
            ],
        );
        let mut agent = Agent::new(env);
        agent.messages.push(crate::llm::Message::user("hello"));
        agent.messages.push(crate::llm::Message::assistant("hi"));
        let n = agent.messages.len();
        agent
            .compact(None, "manual", &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(agent.messages.len(), n, "blocked: nothing compacted");
        assert!(logged(&log).is_empty());
        agent
            .compact(None, "manual", &CancellationToken::new())
            .await
            .unwrap();
        let p = &logged(&log)[0];
        assert_eq!(
            (p["trigger"].as_str(), p["compact_summary"].as_str()),
            (Some("manual"), Some("THE SUMMARY"))
        );
    }

    #[tokio::test]
    async fn model_switch_hooks() {
        let dir = tmpdir("modelswitch");
        let log = dir.join("log");
        let hooks = [
            ("PreModelSwitch", "retired-model", "echo 'retired' >&2; exit 2".to_string()),
            ("PreModelSwitch", "careful-model", r#"echo '{"hookSpecificOutput": {"hookEventName": "PreModelSwitch", "permissionDecision": "ask", "permissionDecisionReason": "Costly. Continue?"}}'"#.to_string()),
            ("PreModelSwitch", "", log_then(&log, "")),
            ("PostModelSwitch", "claude-x", log_then(&log, "echo 'On claude-x, plan first.'")),
        ];
        let env = env("http://127.0.0.1:1/v1", &dir, Mode::Default, &hooks);
        let mut agent = Agent::new(env.clone());
        match crate::commands::handle(&mut agent, "/model retired-model").await {
            crate::commands::Outcome::Show(s) => {
                assert!(s.contains("blocked the switch: retired"), "{s}")
            }
            _ => panic!("expected a refusal"),
        }
        assert_eq!(env.model(), "mock");
        // `ask` refuses when nobody can confirm.
        assert!(
            switch_model(&env, "careful-model", "command")
                .await
                .is_err()
        );
        assert_eq!(env.model(), "mock");
        crate::commands::handle(&mut agent, "/model other/claude-x").await;
        assert_eq!(env.model(), "other/claude-x");
        let p = logged(&log);
        let pre = p
            .iter()
            .find(|v| v["hook_event_name"] == "PreModelSwitch" && v["to_model"] == "other/claude-x")
            .unwrap();
        assert_eq!(
            (
                pre["from_model"].as_str(),
                pre["source"].as_str(),
                pre["requested_model"].as_str()
            ),
            (Some("mock"), Some("command"), Some("other/claude-x"))
        );
        assert!(
            pre["context_tokens"].is_u64()
                && pre["prompt_cache_warm"] == false
                && pre["cache_ttl"] == "5m"
                && pre["pricing"].is_string(),
            "{pre}"
        );
        let post = p
            .iter()
            .find(|v| v["hook_event_name"] == "PostModelSwitch")
            .unwrap();
        assert_eq!(post["source"], "command");
        assert!(
            env.pending_context
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.contains("plan first"))
        );
        // Resuming a session that used another model restores it: PostModelSwitch, source resume.
        env.set_model("again/claude-x");
        env.session_start("resume").await;
        let resumed = logged(&log)
            .into_iter()
            .find(|v| v["source"] == "resume")
            .unwrap();
        assert_eq!(
            (resumed["from_model"].as_str(), resumed["to_model"].as_str()),
            (Some("other/claude-x"), Some("again/claude-x"))
        );
    }

    #[tokio::test]
    async fn interactive_model_switch_can_be_confirmed() {
        let dir = tmpdir("modelask");
        let hook = r#"echo '{"hookSpecificOutput": {"permissionDecision": "ask", "permissionDecisionReason": "Sure?"}}'"#.to_string();
        let fe = Arc::new(Rec {
            answer: Some("Switch".into()),
            ..Default::default()
        });
        let env = env_with(
            "http://127.0.0.1:1/v1",
            &dir,
            Mode::Default,
            fe,
            true,
            &[("PreModelSwitch", "", hook)],
        );
        switch_model(&env, "big-model", "command").await.unwrap();
        assert_eq!(env.model(), "big-model");
        // A client (ACP) can't be asked: `ask` refuses.
        assert!(switch_model(&env, "other", "sdk").await.is_err());
    }

    #[tokio::test]
    async fn user_prompt_expansion_can_block_or_add_context() {
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tmpdir("expansion");
        let log = dir.join("log");
        std::fs::create_dir_all(dir.join(".chanto/commands")).unwrap();
        std::fs::write(dir.join(".chanto/commands/deploy.md"), "Deploy $ARGUMENTS").unwrap();
        let hook = log_then(
            &log,
            &format!(
                r#"tail -1 '{}' | grep -q '"command_args":"prod"' && {{ echo 'needs approval' >&2; exit 2; }}; echo '{{"hookSpecificOutput": {{"hookEventName": "UserPromptExpansion", "additionalContext": "staging is shared"}}}}'"#,
                log.display()
            ),
        );
        let env = env(
            "http://127.0.0.1:1/v1",
            &dir,
            Mode::Default,
            &[("UserPromptExpansion", "deploy", hook)],
        );
        let mut agent = Agent::new(env.clone());
        match crate::commands::handle(&mut agent, "/deploy prod").await {
            crate::commands::Outcome::Show(s) => assert!(s.contains("needs approval"), "{s}"),
            _ => panic!("expected the expansion to be blocked"),
        }
        assert!(
            matches!(crate::commands::handle(&mut agent, "/deploy staging").await, crate::commands::Outcome::Prompt(s) if s == "Deploy staging")
        );
        assert!(
            env.pending_context
                .lock()
                .unwrap()
                .iter()
                .any(|c| c == "staging is shared")
        );
        let p = &logged(&log)[1];
        assert_eq!(
            (
                p["expansion_type"].as_str(),
                p["command_name"].as_str(),
                p["command_args"].as_str()
            ),
            (Some("slash_command"), Some("deploy"), Some("staging"))
        );
        assert_eq!(
            (p["command_source"].as_str(), p["prompt"].as_str()),
            (Some("project"), Some("/deploy staging"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn config_change_runs_on_reload_and_can_block_it() {
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tmpdir("configchange");
        let log = dir.join("log");
        let env = env(
            "http://127.0.0.1:1/v1",
            &dir,
            Mode::Default,
            &[("ConfigChange", "project_settings", log_then(&log, "exit 2"))],
        );
        let mut agent = Agent::new(env.clone());
        std::fs::create_dir_all(dir.join(".chanto")).unwrap();
        std::fs::write(dir.join(".chanto/config.toml"), "max_turns = 7\n").unwrap();
        match crate::commands::handle(&mut agent, "/reload").await {
            crate::commands::Outcome::Show(s) => {
                assert!(s.contains("ConfigChange hook blocked"), "{s}")
            }
            _ => panic!("expected a report"),
        }
        assert!(has(&env, "ConfigChange"), "blocked: the old hooks stay");
        let p = &logged(&log)[0];
        assert_eq!(p["source"], "project_settings");
        assert!(p["file_path"].as_str().unwrap().ends_with("config.toml"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn instructions_loaded_at_session_start() {
        let dir = tmpdir("instructions");
        let log = dir.join("log");
        std::fs::write(dir.join("AGENTS.md"), "Be brief.").unwrap();
        let env = env(
            "http://127.0.0.1:1/v1",
            &dir,
            Mode::Default,
            &[("InstructionsLoaded", "session_start", log_then(&log, ""))],
        );
        env.session_start("startup").await;
        let want = crate::permissions::normalize(&dir.join("AGENTS.md"));
        let mut found = None;
        for _ in 0..100 {
            found = logged(&log).into_iter().find(|v| {
                v["file_path"]
                    .as_str()
                    .is_some_and(|p| crate::permissions::normalize(Path::new(p)) == want)
            });
            if found.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let p = found.expect("AGENTS.md reported");
        assert_eq!(
            (p["load_reason"].as_str(), p["memory_type"].as_str()),
            (Some("session_start"), Some("Project"))
        );
    }

    #[tokio::test]
    async fn cwd_changed_sets_watch_paths_and_file_changed_fires() {
        let dir = tmpdir("cwdchanged");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let log = dir.join("log");
        let watched = dir.join("sub").join("watched.txt");
        let (base, _) = mock_server(vec![
            json!({"tool_calls": tc("Bash", json!({"command": "cd sub"}))}),
            json!({"content": "moved"}),
        ])
        .await;
        let cwd_hook = log_then(
            &log,
            &format!(
                r#"echo '{{"hookSpecificOutput": {{"hookEventName": "CwdChanged", "watchPaths": ["{}"]}}}}'"#,
                watched.display()
            ),
        );
        let env = env(
            &base,
            &dir,
            Mode::Bypass,
            &[
                ("CwdChanged", "", cwd_hook),
                ("FileChanged", "", log_then(&log, "")),
            ],
        );
        let mut agent = Agent::new(env.clone());
        agent
            .run_turn("go to sub", vec![], CancellationToken::new())
            .await
            .unwrap();
        let p = &logged(&log)[0];
        assert_eq!(p["hook_event_name"], "CwdChanged");
        assert_eq!(
            crate::permissions::normalize(Path::new(p["new_cwd"].as_str().unwrap())),
            crate::permissions::normalize(&dir.join("sub"))
        );
        // The watcher has seen the file missing; now create it.
        tokio::time::sleep(Duration::from_millis(400)).await;
        std::fs::write(&watched, "x").unwrap();
        let all = logged_n(&log, 2).await;
        let f = all
            .iter()
            .find(|v| v["hook_event_name"] == "FileChanged")
            .expect("FileChanged fired");
        assert_eq!(
            (f["event"].as_str(), f["file_path"].as_str()),
            (Some("add"), Some(watched.to_str().unwrap()))
        );
        env.set_cwd(dir.clone());
    }

    /// Give `env` a session id and keep its env files in `dir`.
    fn with_session(env: &Env, dir: &Path, id: &str) -> PathBuf {
        *env.session_info.lock().unwrap() = Some((id.into(), dir.join(format!("{id}.jsonl"))));
        *env.hook_state.env_dir.lock().unwrap() = Some(dir.join("session-env"));
        dir.join("session-env").join(format!("{id}.sh"))
    }

    #[tokio::test]
    async fn env_file_from_session_start_reaches_later_bash_commands() {
        let dir = tmpdir("envfile");
        let (out, pre) = (dir.join("out"), dir.join("pre"));
        let cmd = format!("echo \"$HX_ENV_TEST|$HX_ENV_TEST2\" > '{}'", out.display());
        let (base, _) = mock_server(vec![
            json!({"tool_calls": tc("Bash", json!({"command": cmd}))}),
            json!({"content": "done"}),
        ])
        .await;
        let start = r#"[ -n "$CLAUDE_ENV_FILE" ] && [ "$CLAUDE_ENV_FILE" = "$CHANTO_ENV_FILE" ] && echo 'export HX_ENV_TEST="from hook"' >> "$CLAUDE_ENV_FILE""#;
        let pre_hook = format!(
            "echo \"${{CLAUDE_ENV_FILE:-none}} ${{CHANTO_ENV_FILE:-none}}\" > '{}'",
            pre.display()
        );
        let env = env(
            &base,
            &dir,
            Mode::Bypass,
            &[
                ("SessionStart", "", start.into()),
                ("PreToolUse", "", pre_hook),
            ],
        );
        let file = with_session(&env, &dir, "envfile-test");
        env.session_start("startup").await;
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("HX_ENV_TEST")
        );
        // Resuming keeps what the file already has.
        std::fs::write(
            &file,
            format!(
                "{}export HX_ENV_TEST2=two\n",
                std::fs::read_to_string(&file).unwrap()
            ),
        )
        .unwrap();
        crate::hooks::prepare_env_file(&env);
        let mut agent = Agent::new(env.clone());
        agent
            .run_turn("show it", vec![], CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&out).unwrap().trim(),
            "from hook|two"
        );
        // Other events don't get the variable.
        assert_eq!(std::fs::read_to_string(&pre).unwrap().trim(), "none none");
    }

    #[tokio::test]
    async fn bash_without_env_file_content_is_unchanged() {
        let dir = tmpdir("envfile-empty");
        let env = env(
            "http://127.0.0.1:1/v1",
            &dir,
            Mode::Bypass,
            &[("SessionStart", "", "true".into())],
        );
        let file = with_session(&env, &dir, "envfile-empty");
        env.session_start("startup").await;
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "");
        // (A CLAUDE_ENV_FILE set when the tests started may add its own line.)
        let line = format!(". '{}' >/dev/null 2>&1\n", file.display());
        assert!(!bash_prelude(&env).contains(&line));
        std::fs::write(&file, "export A=1\n").unwrap();
        assert!(bash_prelude(&env).ends_with(&line));
    }

    #[tokio::test]
    async fn setup_fires_for_init_only_with_trigger_init() {
        let cli = <crate::Cli as clap::Parser>::parse_from(["chanto", "--init-only"]);
        assert_eq!(cli.setup_trigger(), Some("init"));
        assert_eq!(
            <crate::Cli as clap::Parser>::parse_from(["chanto", "-p", "--maintenance", "x"])
                .setup_trigger(),
            Some("maintenance")
        );
        assert_eq!(
            <crate::Cli as clap::Parser>::parse_from(["chanto", "-p", "x"]).setup_trigger(),
            None
        );
        assert!(
            <crate::Cli as clap::Parser>::try_parse_from(["chanto", "--init-only", "--init"])
                .is_err()
        );
        let dir = tmpdir("setup-hook");
        let log = dir.join("log");
        // Setup can't block: exit 2 neither stops later hooks nor blocks.
        let hooks = [
            (
                "Setup",
                "init",
                log_then(
                    &log,
                    r#"echo 'export HX_SETUP=1' >> "$CLAUDE_ENV_FILE"; echo '{"continue": false}'; exit 2"#,
                ),
            ),
            ("Setup", "init", log_then(&log, "")),
            ("Setup", "maintenance", log_then(&log, "")),
        ];
        let env = env("http://127.0.0.1:1/v1", &dir, Mode::Default, &hooks);
        let file = with_session(&env, &dir, "setup-test");
        crate::hooks::setup(&env, cli.setup_trigger().unwrap()).await;
        let p = logged(&log);
        assert_eq!(p.len(), 2);
        assert_eq!(
            (p[0]["hook_event_name"].as_str(), p[0]["trigger"].as_str()),
            (Some("Setup"), Some("init"))
        );
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("HX_SETUP=1")
        );
    }

    #[tokio::test]
    async fn file_changed_watches_matcher_files() {
        let dir = tmpdir("filechanged");
        let log = dir.join("log");
        std::fs::write(dir.join(".envrc"), "a").unwrap();
        let env = env(
            "http://127.0.0.1:1/v1",
            &dir,
            Mode::Default,
            &[("FileChanged", ".envrc", log_then(&log, ""))],
        );
        env.session_start("startup").await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        std::fs::write(dir.join(".envrc"), "changed").unwrap();
        let p = logged_n(&log, 1).await;
        assert_eq!(p[0]["event"], "change");
        assert!(p[0]["file_path"].as_str().unwrap().ends_with(".envrc"));
        std::fs::remove_file(dir.join(".envrc")).unwrap();
        let p = logged_n(&log, 2).await;
        assert_eq!(p[1]["event"], "unlink");
    }

    #[tokio::test]
    async fn directory_added_runs_for_add_dir() {
        let dir = tmpdir("diradded");
        let other = tmpdir("diradded-other");
        let log = dir.join("log");
        let hook = log_then(&log, r#"echo '{"systemMessage": "deps installed"}'"#);
        let env = env(
            "http://127.0.0.1:1/v1",
            &dir,
            Mode::Default,
            &[("DirectoryAdded", "slash_command", hook)],
        );
        let mut agent = Agent::new(env.clone());
        crate::commands::handle(&mut agent, &format!("/add-dir {}", other.display())).await;
        let p = logged_n(&log, 1).await;
        assert_eq!(
            (p[0]["source"].as_str(), p[0]["directory"].as_str()),
            (Some("slash_command"), Some(other.to_str().unwrap()))
        );
        for _ in 0..50 {
            if env
                .pending_context
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.contains("deps installed"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            env.pending_context
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.contains("deps installed"))
        );
        // Already part of the workspace: no event.
        std::fs::create_dir_all(dir.join("inside")).unwrap();
        crate::commands::handle(
            &mut agent,
            &format!("/add-dir {}", dir.join("inside").display()),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(logged(&log).len(), 1);
        let dirs: Vec<String> = crate::permissions::extra_dirs()
            .iter()
            .map(|d| d.display().to_string())
            .filter(|d| !d.contains("diradded"))
            .collect();
        crate::permissions::set_extra_dirs(&dirs, &dir);
    }

    #[tokio::test]
    async fn worktree_create_hook_replaces_git_for_isolated_subagents() {
        let dir = tmpdir("wtcreate");
        let log = dir.join("log");
        let wt = dir.join("copies").join("made-by-hook");
        let (base, seen) = mock_server(vec![json!({"content": "sub done"})]).await;
        let hook = log_then(
            &log,
            &format!(
                "mkdir -p '{w}' && echo 'noise' && echo '{w}'",
                w = wt.display()
            ),
        );
        let env1 = env(&base, &dir, Mode::Bypass, &[("WorktreeCreate", "", hook)]);
        let ctx = crate::tools::Ctx {
            env: env1,
            cancel: CancellationToken::new(),
            depth: 0,
            call_id: None,
            cwd_override: None,
            reads: None,
        };
        let opts = crate::agent::SubagentOpts {
            worktree: true,
            description: "x".into(),
            ..Default::default()
        };
        let out = crate::agent::run_subagent(&ctx, "general-purpose", "do it", opts)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            logged(&log)[0]["name"]
                .as_str()
                .unwrap()
                .starts_with("agent-")
        );
        assert!(
            seen.lock().unwrap()[0]["messages"]
                .to_string()
                .contains("made-by-hook"),
            "the subagent works in the hook's directory"
        );
        assert!(wt.is_dir(), "kept afterwards");
        // A failing hook fails the creation.
        let env2 = env(
            &base,
            &dir,
            Mode::Bypass,
            &[("WorktreeCreate", "", "exit 1".into())],
        );
        let ctx = crate::tools::Ctx {
            env: env2,
            cancel: CancellationToken::new(),
            depth: 0,
            call_id: None,
            cwd_override: None,
            reads: None,
        };
        let opts = crate::agent::SubagentOpts {
            worktree: true,
            ..Default::default()
        };
        let out = crate::agent::run_subagent(&ctx, "general-purpose", "do it", opts)
            .await
            .unwrap();
        assert!(
            out.is_error && out.content.contains("Could not create a worktree"),
            "{}",
            out.content
        );
    }

    #[test]
    fn worktree_hooks_for_worktree_sessions() {
        let dir = tmpdir("wtsession");
        let wt = dir.join("wt");
        let add = |hooks: &mut crate::config::HooksConfig, ev: &str, cmd: String| {
            hooks.entry(ev.into()).or_default().push(HookConfig {
                command: cmd,
                ..Default::default()
            })
        };
        let mut hooks = crate::config::HooksConfig::new();
        add(
            &mut hooks,
            "WorktreeCreate",
            format!(
                "cat > '{}/create.json'; mkdir -p wt && echo wt",
                dir.display()
            ),
        );
        add(
            &mut hooks,
            "WorktreeRemove",
            format!(
                "cat > '{}/remove.json'; rm -rf '{}'",
                dir.display(),
                wt.display()
            ),
        );
        // A relative path resolves against the directory the hook ran in.
        let made = worktree_create_detached(&hooks, &dir, "feature-auth")
            .unwrap()
            .unwrap();
        assert_eq!(made, crate::permissions::normalize(&wt));
        let created: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("create.json")).unwrap())
                .unwrap();
        assert_eq!(
            (
                created["hook_event_name"].as_str(),
                created["name"].as_str()
            ),
            (Some("WorktreeCreate"), Some("feature-auth"))
        );
        finish_hook_worktree(&hooks, &dir, &made);
        assert!(!wt.exists(), "removed by the WorktreeRemove hook");
        let removed: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("remove.json")).unwrap())
                .unwrap();
        assert_eq!(removed["worktree_path"].as_str(), made.to_str());
        // A failing remove hook keeps the directory.
        std::fs::create_dir_all(&wt).unwrap();
        let mut failing = crate::config::HooksConfig::new();
        add(&mut failing, "WorktreeRemove", "exit 1".into());
        assert_eq!(worktree_remove_detached(&failing, &dir, &wt), Some(false));
        assert!(wt.exists());
        // No path printed, or an absolute path with `..`: creation fails.
        let mut bad = crate::config::HooksConfig::new();
        add(&mut bad, "WorktreeCreate", "true".into());
        assert!(worktree_create_detached(&bad, &dir, "x").unwrap().is_err());
        let mut bad = crate::config::HooksConfig::new();
        add(
            &mut bad,
            "WorktreeCreate",
            format!("echo '{}/../x'", dir.display()),
        );
        assert!(
            worktree_create_detached(&bad, &dir, "x")
                .unwrap()
                .unwrap_err()
                .contains("..")
        );
        assert!(worktree_create_detached(&crate::config::HooksConfig::new(), &dir, "x").is_none());
    }

    #[tokio::test]
    async fn elicitation_hooks_answer_or_rewrite() {
        let dir = tmpdir("elicit");
        let log = dir.join("log");
        let params = json!({"message": "Project key?", "requestedSchema": {"type": "object", "properties": {"project_key": {"type": "string"}}}});
        // An Elicitation hook answers for the user: no ElicitationResult.
        let answer = log_then(
            &log,
            r#"echo '{"hookSpecificOutput": {"hookEventName": "Elicitation", "action": "accept", "content": {"project_key": "DOCS"}}}'"#,
        );
        let rewrite = log_then(
            &log,
            r#"echo '{"hookSpecificOutput": {"hookEventName": "ElicitationResult", "action": "accept", "content": {"project_key": "OPS"}}}'"#,
        );
        let env1 = env(
            "http://127.0.0.1:1/v1",
            &dir,
            Mode::Default,
            &[
                ("Elicitation", "tracker", answer),
                ("ElicitationResult", "", rewrite),
            ],
        );
        let hooks = ElicitHooks(Arc::downgrade(&env1));
        let r = crate::mcp::elicit_with_hooks("tracker", &params, None, Some(&hooks)).await;
        assert_eq!(
            r,
            json!({"action": "accept", "content": {"project_key": "DOCS"}})
        );
        let p = logged(&log);
        assert_eq!(p.len(), 1);
        assert_eq!(
            (p[0]["mcp_server_name"].as_str(), p[0]["mode"].as_str()),
            (Some("tracker"), Some("form"))
        );
        assert_eq!(
            p[0]["requested_schema"]["properties"]["project_key"]["type"],
            "string"
        );
        // Another server: nobody answers (declined), and ElicitationResult rewrites the answer.
        let r = crate::mcp::elicit_with_hooks("other", &params, None, Some(&hooks)).await;
        assert_eq!(
            r,
            json!({"action": "accept", "content": {"project_key": "OPS"}})
        );
        let p = logged(&log);
        assert_eq!(
            (p[1]["hook_event_name"].as_str(), p[1]["action"].as_str()),
            (Some("ElicitationResult"), Some("decline"))
        );
        // Exit 2 declines.
        let env2 = env(
            "http://127.0.0.1:1/v1",
            &dir,
            Mode::Default,
            &[("Elicitation", "", "exit 2".into())],
        );
        let r = crate::mcp::elicit_with_hooks(
            "x",
            &params,
            None,
            Some(&ElicitHooks(Arc::downgrade(&env2))),
        )
        .await;
        assert_eq!(r, json!({"action": "decline"}));
    }

    fn env_hooks(base: &str, dir: &Path, mode: Mode, hooks: Vec<(&str, HookConfig)>) -> Arc<Env> {
        let e = env(base, dir, mode, &[]);
        let mut all = e.hooks.write().unwrap();
        for (event, h) in hooks {
            all.entry(event.to_string()).or_default().push(h);
        }
        drop(all);
        e
    }

    fn cmd(c: &str) -> HookConfig {
        HookConfig {
            kind: "command".into(),
            command: c.into(),
            timeout_secs: Some(20),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn exec_form_passes_arguments_verbatim() {
        let dir = tmpdir("exec-form");
        let out = dir.join("out");
        let h = HookConfig {
            args: Some(vec![
                "-c".into(),
                "printf '%s|%s' \"$1\" \"$2\" > \"$3\"".into(),
                "x".into(),
                "a b$HOME'`".into(),
                "${CLAUDE_PROJECT_DIR}".into(),
                out.display().to_string(),
            ]),
            ..cmd("sh")
        };
        let env = env_hooks(
            "http://127.0.0.1:1/v1",
            &dir,
            Mode::Default,
            vec![("PreToolUse", h)],
        );
        let r = pre_tool_use(&env, "Bash", &json!({"command": "ls"})).await;
        assert!(r.decision.is_none() && !r.failed, "{r:?}");
        let text = std::fs::read_to_string(&out).unwrap();
        let (first, second) = text.split_once('|').unwrap();
        assert_eq!(first, "a b$HOME'`", "no shell expands exec-form arguments");
        assert!(
            !second.contains("${"),
            "placeholders are substituted: {second}"
        );
    }

    #[tokio::test]
    async fn a_block_without_a_reason_names_the_hook() {
        let dir = tmpdir("silent-block");
        let base = "http://127.0.0.1:1/v1";
        for (c, want) in [
            ("exit 2", "exited with code 2"),
            (r#"echo '{"decision": "block"}'"#, "blocked it"),
            (
                r#"echo '{"hookSpecificOutput": {"permissionDecision": "deny"}}'"#,
                "denied it",
            ),
        ] {
            let e = env_hooks(base, &dir, Mode::Default, vec![("PreToolUse", cmd(c))]);
            match pre_tool_use(&e, "Bash", &json!({"command": "ls"}))
                .await
                .decision
            {
                Some(PermDecision::Deny(m)) => assert!(
                    m.contains(want)
                        && m.contains(&format!("hook [{}", &c[..c.len().min(30)]))
                        && m.contains("no reason"),
                    "{c}: {m}"
                ),
                d => panic!("{c}: expected a block, got {d:?}"),
            }
        }
    }

    #[tokio::test]
    async fn on_failure_block_turns_failures_into_blocks() {
        let dir = tmpdir("on-failure");
        let missing = HookConfig {
            args: Some(vec![]),
            ..cmd(&dir.join("missing-script").display().to_string())
        };
        let blocking = HookConfig {
            on_failure: Some("block".into()),
            ..missing.clone()
        };
        let base = "http://127.0.0.1:1/v1";
        // Default: a hook that can't start is a non-blocking error.
        let env1 = env_hooks(base, &dir, Mode::Default, vec![("PreToolUse", missing)]);
        let r = pre_tool_use(&env1, "Bash", &json!({"command": "ls"})).await;
        assert!(r.decision.is_none() && r.failed);
        let env2 = env_hooks(
            base,
            &dir,
            Mode::Default,
            vec![("PreToolUse", blocking.clone())],
        );
        match pre_tool_use(&env2, "Bash", &json!({"command": "ls"}))
            .await
            .decision
        {
            Some(PermDecision::Deny(m)) => assert!(
                m.contains("failed; blocking because onFailure is \"block\""),
                "{m}"
            ),
            d => panic!("expected a block, got {d:?}"),
        }
        // Exit 1, a timeout and unparsable JSON count too.
        for (c, why) in [
            ("exit 1", "failed"),
            ("sleep 5", "timed out"),
            ("echo '{not json'", "failed"),
        ] {
            let h = HookConfig {
                on_failure: Some("block".into()),
                timeout_secs: Some(1),
                ..cmd(c)
            };
            let e = env_hooks(base, &dir, Mode::Default, vec![("UserPromptSubmit", h)]);
            let r = user_prompt_submit(&e, "hi").await;
            assert!(
                r.block.as_deref().is_some_and(|b| b.contains(why)),
                "{c}: {r:?}"
            );
        }
        // PermissionRequest denies; Stop ignores it.
        let e = env_hooks(
            base,
            &dir,
            Mode::Default,
            vec![("PermissionRequest", blocking.clone()), ("Stop", blocking)],
        );
        assert!(matches!(
            notify(
                &e,
                "PermissionRequest",
                Some("Bash"),
                json!({"tool_name": "Bash"})
            )
            .await
            .decision,
            Some(PermDecision::Deny(_))
        ));
        assert!(stop(&e, "Stop", "done", false).await.block.is_none());
    }

    #[tokio::test]
    async fn async_hooks_report_on_the_next_turn_and_rewake() {
        let dir = tmpdir("async-hooks");
        let quiet = HookConfig {
            run_async: true,
            ..cmd(
                r#"sleep 1; echo '{"hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": "TESTS PASSED"}}'"#,
            )
        };
        let rewake = HookConfig {
            async_rewake: true,
            ..cmd("echo 'build broke' >&2; exit 2")
        };
        let env = env_hooks(
            "http://127.0.0.1:1/v1",
            &dir,
            Mode::Default,
            vec![("PostToolUse", quiet), ("PostToolUse", rewake)],
        );
        let t0 = Instant::now();
        let r = post_tool_use(
            &env,
            "Write",
            &json!({"file_path": "a"}),
            "File created",
            false,
        )
        .await;
        assert!(
            t0.elapsed() < Duration::from_millis(900),
            "async hooks don't block"
        );
        assert!(r.context.is_empty() && r.block.is_none());
        let mut notes = String::new();
        for _ in 0..100 {
            notes = env.notifications.lock().unwrap().join("\n");
            if notes.contains("TESTS PASSED") && notes.contains("build broke") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            notes.contains("TESTS PASSED")
                && notes.contains("build broke")
                && notes.contains("exited with code 2"),
            "{notes}"
        );
    }

    #[tokio::test]
    async fn updated_tool_output_replaces_what_the_model_sees() {
        let dir = tmpdir("updated-output");
        let (base, seen) = mock_server(vec![
            json!({"tool_calls": tc("Bash", json!({"command": "echo secret-value"}))}),
            json!({"content": "done"}),
        ])
        .await;
        let h = cmd(
            r#"echo '{"hookSpecificOutput": {"hookEventName": "PostToolUse", "updatedToolOutput": {"stdout": "[redacted]", "stderr": "", "interrupted": false, "isImage": false}}}'"#,
        );
        let env = env_hooks(
            &base,
            &dir,
            Mode::Bypass,
            vec![(
                "PostToolUse",
                HookConfig {
                    matcher: "Bash".into(),
                    ..h
                },
            )],
        );
        let mut agent = Agent::new(env);
        agent
            .run_turn("go", vec![], CancellationToken::new())
            .await
            .unwrap();
        let sent = seen.lock().unwrap()[1]["messages"].clone();
        let result = sent
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool")
            .unwrap()["content"]
            .clone();
        assert_eq!(result, "[redacted]", "{sent}");
    }

    #[tokio::test]
    async fn prompt_hooks_keep_the_agent_going_or_end_the_turn() {
        let dir = tmpdir("prompt-hook");
        // Stop: `ok: false` is the next instruction; `ok: true` lets it stop.
        let (base, seen) = mock_server(vec![
            json!({"content": "done"}),
            json!({"content": "{\"ok\": false, \"reason\": \"run the tests first\"}"}),
            json!({"content": "tests pass, done"}),
            json!({"content": "{\"ok\": true}"}),
        ])
        .await;
        let h = HookConfig {
            kind: "prompt".into(),
            prompt: "Did Claude finish? $ARGUMENTS".into(),
            ..Default::default()
        };
        let env = env_hooks(&base, &dir, Mode::Bypass, vec![("Stop", h)]);
        let mut agent = Agent::new(env);
        let end = agent
            .run_turn("go", vec![], CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(end, TurnEnd::Done(t) if t == "tests pass, done"));
        let seen = seen.lock().unwrap().clone();
        let hook_req = seen[1]["messages"].to_string();
        assert!(
            hook_req.contains("Did Claude finish?") && hook_req.contains("last_assistant_message"),
            "{hook_req}"
        );
        assert!(
            seen[2]["messages"]
                .to_string()
                .contains("run the tests first")
        );
        // PreToolUse: ends the turn by default, the tool doesn't run.
        let marker = dir.join("ran");
        let (base, _) = mock_server(vec![json!({"tool_calls": tc("Bash", json!({"command": format!("touch {}", marker.display())}))}), json!({"content": "{\"ok\": false, \"reason\": \"no shell\"}"})]).await;
        let h = HookConfig {
            kind: "prompt".into(),
            prompt: "Allow? $ARGUMENTS".into(),
            ..Default::default()
        };
        let env = env_hooks(&base, &dir, Mode::Bypass, vec![("PreToolUse", h.clone())]);
        let end = Agent::new(env)
            .run_turn("go", vec![], CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(end, TurnEnd::Done(t) if t.is_empty()) && !marker.exists());
        // With continueOnBlock the reason goes back to the model as the tool's error.
        let (base, seen) = mock_server(vec![
            json!({"tool_calls": tc("Bash", json!({"command": format!("touch {}", marker.display())}))}),
            json!({"content": "{\"ok\": false, \"reason\": \"no shell\"}"}),
            json!({"content": "ok, no shell"}),
        ])
        .await;
        let env = env_hooks(
            &base,
            &dir,
            Mode::Bypass,
            vec![(
                "PreToolUse",
                HookConfig {
                    continue_on_block: true,
                    ..h
                },
            )],
        );
        let end = Agent::new(env)
            .run_turn("go", vec![], CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(end, TurnEnd::Done(t) if t == "ok, no shell") && !marker.exists());
        assert!(
            seen.lock().unwrap()[2]["messages"]
                .to_string()
                .contains("no shell")
        );
    }

    #[tokio::test]
    async fn agent_hooks_verify_with_a_read_only_subagent() {
        let dir = tmpdir("agent-hook");
        std::fs::write(dir.join("STATUS"), "green").unwrap();
        let (base, seen) = mock_server(vec![
            json!({"content": "done"}),
            json!({"tool_calls": tc("Read", json!({"file_path": dir.join("STATUS").display().to_string()}))}),
            json!({"content": "{\"ok\": true}"}),
        ])
        .await;
        let h = HookConfig {
            kind: "agent".into(),
            prompt: "Check STATUS is green".into(),
            ..Default::default()
        };
        let env = env_hooks(&base, &dir, Mode::Bypass, vec![("Stop", h)]);
        let end = Agent::new(env)
            .run_turn("go", vec![], CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(end, TurnEnd::Done(t) if t == "done"));
        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen.len(),
            3,
            "the verifier ran once, without re-triggering the Stop hook"
        );
        let tools: Vec<String> = seen[1]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
            .collect();
        assert!(
            tools.contains(&"Read".to_string())
                && !tools.iter().any(|t| t == "Bash" || t == "Write"),
            "{tools:?}"
        );
        assert!(seen[2]["messages"].to_string().contains("green"));
    }

    #[tokio::test]
    async fn skill_hooks_last_the_session_and_once_runs_once() {
        let dir = tmpdir("skill-hooks");
        let log = dir.join("log");
        let env = env("http://127.0.0.1:1/v1", &dir, Mode::Default, &[]);
        let mut h = crate::config::HooksConfig::new();
        h.insert(
            "UserPromptSubmit".into(),
            vec![HookConfig {
                once: true,
                ..cmd(&log_then(&log, "true"))
            }],
        );
        assert!(!has(&env, "UserPromptSubmit"));
        add_session_hooks(&env, &h);
        add_session_hooks(&env, &h);
        user_prompt_submit(&env, "one").await;
        user_prompt_submit(&env, "two").await;
        assert_eq!(
            logged(&log).len(),
            1,
            "registered once, removed after its first success"
        );
        assert!(!has(&env, "UserPromptSubmit"));
    }

    #[tokio::test]
    async fn scoped_hooks_apply_only_inside_the_scope() {
        let dir = tmpdir("scoped-hooks");
        let log = dir.join("log");
        let env = env("http://127.0.0.1:1/v1", &dir, Mode::Default, &[]);
        let mut h = crate::config::HooksConfig::new();
        h.insert(
            "UserPromptSubmit".into(),
            vec![cmd(&log_then(&log, "true"))],
        );
        user_prompt_submit(&env, "outside").await;
        SCOPE
            .scope(
                Scope {
                    hooks: Some(Arc::new(h)),
                    off: false,
                },
                user_prompt_submit(&env, "inside"),
            )
            .await;
        let l = logged(&log);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0]["prompt"], "inside");
    }
}
