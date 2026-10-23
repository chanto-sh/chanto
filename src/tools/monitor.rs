//! Monitor: run a command in the background and feed each output line back to
//! the model as it arrives. The command is a
//! background shell like `Bash` with `run_in_background`, so `/bashes`, the
//! TUI's background panel, BashOutput and KillShell all see it; a watcher turns
//! its new lines into notifications, which reach the model at its next step,
//! or start a turn when the session is idle.

use super::bash::{BgShell, kill_group, spawn_background};
use super::{Ctx, Tool, ToolKind, ToolOutput, arg_str, arg_u64};
use crate::agent::Env;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Deadline when none is given, and the most allowed (interactive sessions
/// and `-p` runs).
pub const DEFAULT_MS: u64 = 300_000;
pub const MAX_MS: u64 = 1_800_000;
pub const MAX_MS_PRINT: u64 = 600_000;
/// One undelivered notification holds at most this much output.
const NOTE_CAP: usize = 8_000;
const POLL: Duration = Duration::from_millis(250);

pub struct Monitor;

#[async_trait]
impl Tool for Monitor {
    fn name(&self) -> String {
        "Monitor".into()
    }
    fn description(&self) -> String {
        "Run a command in the background and receive each line it prints as a notification while you keep working, so you can \
react to log entries, file changes or polled status without polling yourself. Write the command to print only lines worth \
reacting to (e.g. `tail -f app.log | grep --line-buffered ERROR`, or a loop that prints when a CI status changes). The watch \
ends when the command exits or at timeout_ms (default 300000, max 1800000); you get one notice then. KillShell stops it early."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command whose output lines are the events"},
                "description": {"type": "string", "description": "Short description of what is being watched"},
                "timeout_ms": {"type": "integer", "description": "Deadline in milliseconds (default 300000, max 1800000)"}
            },
            "required": ["command"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Execute
    }
    fn rule_subjects(&self, args: &Value) -> Vec<String> {
        crate::permissions::split_shell(args["command"].as_str().unwrap_or(""))
    }
    fn label(&self, args: &Value) -> String {
        let what = args["description"]
            .as_str()
            .filter(|d| !d.trim().is_empty())
            .or(args["command"].as_str())
            .unwrap_or("");
        format!("Monitor({})", crate::util::one_line(what, 100))
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let command = arg_str(&args, "command")?.to_string();
        if args.get("ws").is_some_and(|w| !w.is_null()) {
            return Ok(ToolOutput::err(
                "WebSocket watches (`ws`) aren't supported here: pass a `command` that connects and prints one line per message instead (e.g. with websocat).",
            ));
        }
        let description = args["description"]
            .as_str()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| crate::util::one_line(&command, 60));
        let max = if ctx.env.interactive {
            MAX_MS
        } else {
            MAX_MS_PRINT
        };
        let timeout = Duration::from_millis(
            arg_u64(&args, "timeout_ms")
                .unwrap_or(DEFAULT_MS)
                .clamp(1_000, max),
        );
        if let Err(e) =
            crate::resources::wait_for_memory("this monitor", Duration::from_secs(30)).await
        {
            return Ok(ToolOutput::err(e));
        }
        let id = format!("monitor_{}", ctx.env.next_shell_id());
        // `stdbuf` would be nicer, but isn't everywhere; most watch commands
        // (tail -f, loops with echo) flush per line anyway.
        crate::shell::snapshot::ensure(&ctx.env).await;
        let shell = spawn_background(
            ctx,
            &id,
            &command,
            &ctx.cwd(),
            false,
            Some(description.clone()),
        )?;
        LIVE.lock().unwrap().push((key(&ctx.env), id.clone()));
        tokio::spawn(watch(ctx.env.clone(), shell, description.clone(), timeout));
        let secs = timeout.as_secs();
        Ok(ToolOutput::ok(format!(
            "Monitor {id} started; it ends in {} at the latest. Each line it prints reaches you as a notification while it runs, \
             so there's no need to poll or wait for it: carry on, or end your turn. KillShell with shell_id=\"{id}\" stops it.",
            crate::util::duration(secs)
        ))
        .display(format!("Monitoring in background ({id}, up to {})", crate::util::duration(secs))))
    }
}

/// Watchers that haven't sent their final notice yet, by session (the
/// `Env`'s address) and monitor id.
static LIVE: std::sync::Mutex<Vec<(usize, String)>> = std::sync::Mutex::new(Vec::new());

fn key(env: &Env) -> usize {
    env as *const Env as usize
}

/// Watches still running: until a watch's final notice is queued, so a run
/// that waits for them doesn't miss its last lines.
pub fn active(env: &Env) -> usize {
    LIVE.lock()
        .unwrap()
        .iter()
        .filter(|(e, _)| *e == key(env))
        .count()
}

fn header(id: &str, description: &str) -> String {
    format!("<system-reminder>Monitor {id} ({description}) output:</system-reminder>\n")
}

/// Queue `text` for the model: appended to this monitor's notification if
/// that hasn't been delivered yet (so a burst becomes one message), else a
/// new one. Wakes anything waiting on background work.
fn deliver(env: &Env, id: &str, description: &str, text: &str) {
    let head = header(id, description);
    let marker = format!(
        "[more lines not shown here; BashOutput with bash_id=\"{id}\" has the full output]"
    );
    {
        let mut queue = env.notifications.lock().unwrap();
        match queue.iter_mut().rev().find(|m| m.starts_with(&head)) {
            Some(m) if m.len() + text.len() > NOTE_CAP => {
                if !m.ends_with(&marker) {
                    m.push('\n');
                    m.push_str(&marker);
                }
            }
            Some(m) => {
                m.push('\n');
                m.push_str(text);
            }
            None => {
                let mut body = text.to_string();
                if body.len() > NOTE_CAP {
                    let mut cut = NOTE_CAP;
                    while !body.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    body.truncate(cut);
                    body.push('\n');
                    body.push_str(&marker);
                }
                queue.push(format!("{head}{body}"));
            }
        }
    }
    env.bg_done.notify_waiters();
}

/// A final notice: always its own message, after any output still queued.
fn finish(env: &Env, text: String) {
    env.notifications
        .lock()
        .unwrap()
        .push(format!("<system-reminder>{text}</system-reminder>"));
    env.bg_done.notify_waiters();
}

async fn watch(env: Arc<Env>, shell: Arc<BgShell>, description: String, timeout: Duration) {
    let id = shell.id.clone();
    watch_until_done(&env, &shell, &description, timeout).await;
    LIVE.lock()
        .unwrap()
        .retain(|(e, i)| !(*e == key(&env) && *i == id));
    env.bg_done.notify_waiters();
}

async fn watch_until_done(
    env: &Arc<Env>,
    shell: &Arc<BgShell>,
    description: &str,
    timeout: Duration,
) {
    let id = shell.id.clone();
    let started = Instant::now();
    loop {
        tokio::time::sleep(POLL).await;
        let exit = *shell.exit.lock().unwrap();
        // Give the readers a moment to drain the pipes after the exit.
        if exit.is_some() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let fresh = take_lines(shell, exit.is_some());
        if !fresh.is_empty() {
            deliver(env, &id, description, &fresh);
        }
        if let Some(code) = exit {
            let removed = !env.shells.lock().unwrap().contains_key(&id);
            finish(
                env,
                if removed {
                    format!("Monitor {id} ({description}) was stopped.")
                } else {
                    format!(
                        "Monitor {id} ({description}) ended: the command exited with code {code}."
                    )
                },
            );
            return;
        }
        if started.elapsed() >= timeout {
            env.shells.lock().unwrap().remove(&id);
            if let Some(pid) = shell.pid {
                kill_group(pid);
            }
            finish(
                env,
                format!(
                    "Monitor {id} ({description}) reached its {} deadline and was stopped. Start it again if you still need it.",
                    crate::util::duration(timeout.as_secs())
                ),
            );
            return;
        }
    }
}

/// Complete lines printed since the last call (and the unterminated tail
/// once the command has exited), blank ones dropped.
fn take_lines(shell: &BgShell, all: bool) -> String {
    let out = shell.output.lock().unwrap();
    let mut pos = shell.watch_pos.lock().unwrap();
    let start = (*pos).min(out.len());
    let fresh = &out[start..];
    let end = if all {
        fresh.len()
    } else {
        fresh.rfind('\n').map_or(0, |i| i + 1)
    };
    *pos = start + end;
    fresh[..end]
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}
