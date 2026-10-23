//! The agent side of a background run. In the daemon each run is its own
//! process (`chanto __run-worker <id>`): project root, sandbox and secret
//! masking are process-wide, and every run has a different root.
//!
//! Protocol, one JSON object per line. Worker → daemon (stdout): the agent's
//! events (`event_json`), plus `run_started {session_id, model}`,
//! `permission_request` / `question` (with `request_id`), `request_done
//! {request_id}` and finally `run_end {state, result, error, verify,
//! cost_usd}`. Daemon → worker (stdin): `respond {request_id, decision |
//! answer, feedback}` and `stop`. End of stdin means the daemon went away:
//! the worker saves the session and ends as `interrupted`.

use super::{Run, RunState, VerifyResult};
use crate::Cli;
use crate::agent::{Event, Frontend, PermissionReply, PermissionRequest, TurnEnd, event_json};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

struct WorkerFrontend {
    out: mpsc::UnboundedSender<Value>,
    pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    /// Requests waiting for an answer (the wall clock pauses meanwhile).
    waiting: AtomicUsize,
    input_timeout: Duration,
}

/// Ends a request even when the agent drops the wait (stop, interrupt).
struct AskGuard<'a> {
    fe: &'a WorkerFrontend,
    id: String,
    answered: bool,
}

impl Drop for AskGuard<'_> {
    fn drop(&mut self) {
        self.fe.waiting.fetch_sub(1, Ordering::SeqCst);
        self.fe.pending.lock().unwrap().remove(&self.id);
        let _ = self.fe.out.send(
            json!({"type": "request_done", "request_id": self.id, "answered": self.answered}),
        );
    }
}

impl WorkerFrontend {
    async fn ask(&self, mut ev: Value) -> Option<Value> {
        let id = format!("req-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let (tx, rx) = oneshot::channel();
        ev["request_id"] = json!(id);
        self.pending.lock().unwrap().insert(id.clone(), tx);
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let mut guard = AskGuard {
            fe: self,
            id,
            answered: false,
        };
        let _ = self.out.send(ev);
        let answer = tokio::time::timeout(self.input_timeout, rx)
            .await
            .ok()
            .and_then(|r| r.ok());
        guard.answered = answer.is_some();
        answer
    }

    fn resolve(&self, msg: Value) -> bool {
        let id = msg["request_id"].as_str().unwrap_or_default().to_string();
        match self.pending.lock().unwrap().remove(&id) {
            Some(tx) => tx.send(msg).is_ok(),
            None => false,
        }
    }
}

#[async_trait]
impl Frontend for WorkerFrontend {
    fn emit(&self, ev: Event) {
        if let Some(v) = event_json(&ev) {
            let _ = self.out.send(v);
        }
    }
    async fn permission(&self, req: PermissionRequest) -> PermissionReply {
        let ev = json!({"type": "permission_request", "tool": req.tool, "label": req.label, "detail": req.detail,
            "reason": req.reason, "always_label": req.always_label});
        let Some(a) = self.ask(ev).await else {
            return PermissionReply::Deny(Some(format!(
                "Nobody answered this permission request within {} minutes, so it was denied. Continue without it if you can; otherwise finish and explain what is blocked.",
                self.input_timeout.as_secs() / 60
            )));
        };
        match a["decision"].as_str() {
            Some("allow") => PermissionReply::Allow,
            Some("allow_always") => PermissionReply::AllowAlways,
            _ => PermissionReply::Deny(Some(
                a["feedback"].as_str().filter(|f| !f.is_empty()).unwrap_or("The user denied this action. Find another way, or finish and explain what is blocked.").to_string(),
            )),
        }
    }
    async fn question(&self, question: &str, options: &[String]) -> Option<String> {
        let a = self
            .ask(json!({"type": "question", "question": question, "options": options}))
            .await?;
        a["answer"].as_str().map(str::to_string)
    }
}

fn run_prompt(run: &Run) -> String {
    if run.from_session.is_some() && !run.resume {
        return format!(
            "{}\n\n<system-reminder>The user moved this session to a background run (/background): the conversation above continues here, \
             in a separate git worktree at {} (branch `{}`) instead of {}. Paths under {} are now under the worktree. \
             Work autonomously until the task is complete; don't ask for confirmation, and ask a question only if you are truly blocked. \
             When you finish, your changes are committed to the branch for the user to review and apply.</system-reminder>",
            run.task,
            run.worktree.display(),
            run.branch,
            run.source.path.display(),
            run.source.path.display()
        );
    }
    if run.resume {
        return "<system-reminder>This background run was interrupted (the chanto daemon stopped) and is now resumed. \
                Continue the task from where you left off; check the working tree to see what is already done.</system-reminder>"
            .into();
    }
    format!(
        "{}\n\n<system-reminder>This is a background run: you work alone in a separate git worktree (branch `{}`) while the user does something else. \
         Work autonomously until the task is complete; don't ask for confirmation, and ask a question only if you are truly blocked. \
         When you finish, your changes are committed to the branch for the user to review and apply.</system-reminder>",
        run.task, run.branch
    )
}

/// Run the task; events and the final `run_end` go to `out`.
pub async fn run(
    run: Run,
    out: mpsc::UnboundedSender<Value>,
    input: mpsc::UnboundedReceiver<Value>,
) {
    let end = match work(&run, &out, input).await {
        Ok(v) => v,
        Err(e) => json!({"type": "run_end", "state": RunState::Failed, "error": format!("{e:#}")}),
    };
    let _ = out.send(end);
}

async fn work(
    run: &Run,
    out: &mpsc::UnboundedSender<Value>,
    input: mpsc::UnboundedReceiver<Value>,
) -> anyhow::Result<Value> {
    let s = &run.settings;
    let mut cli = <Cli as clap::Parser>::parse_from(["chanto"]);
    cli.no_trust_prompt = true;
    cli.background_run = true;
    // Print-style startup (no pickers, no update prompt), but an interactive
    // session: permission prompts reach the frontend instead of being denied.
    cli.print = true;
    cli.model = s.model.clone();
    cli.provider = s.provider.clone();
    cli.base_url = s.base_url.clone();
    cli.mode = Some(s.mode.clone());
    cli.sandbox = Some(
        if s.sandbox {
            "workspace-write"
        } else {
            "danger-full-access"
        }
        .into(),
    );
    cli.max_cost = Some(s.max_cost_usd);
    if run.resume {
        cli.resume = run.session_id.clone();
    }
    let fe = Arc::new(WorkerFrontend {
        out: out.clone(),
        pending: Default::default(),
        waiting: AtomicUsize::new(0),
        input_timeout: Duration::from_secs(s.input_timeout_minutes.max(1) * 60),
    });
    // Container isolation: shell commands from the worktree go into the run's
    // container, created before the agent can run any.
    let container = match &s.isolate {
        Some(iso) => {
            let note_out = out.clone();
            let note = move |m: String| {
                let _ = note_out.send(json!({"type": "info", "message": m}));
            };
            Some(super::container::start(run, iso, s.allow_sandboxed_commands, &note).await?)
        }
        None => None,
    };
    let res = work_in(run, out, input, cli, fe, container.clone()).await;
    if let Some(c) = container {
        super::container::unregister(&c.name);
    }
    res
}

async fn work_in(
    run: &Run,
    out: &mpsc::UnboundedSender<Value>,
    mut input: mpsc::UnboundedReceiver<Value>,
    cli: Cli,
    fe: Arc<WorkerFrontend>,
    container: Option<Arc<super::container::Container>>,
) -> anyhow::Result<Value> {
    let s = &run.settings;
    let setup = crate::setup_session(
        &cli,
        fe.clone(),
        true,
        run.worktree.clone(),
        vec![],
        Some(s.verify),
    )
    .await?;
    if let Some(c) = &container {
        let _ = out.send(json!({"type": "info", "message": format!("Shell commands run in container {} ({})", c.name, s.isolate.as_ref().map(|i| i.label()).unwrap_or_default())}));
    }
    if s.sandbox && s.allow_sandboxed_commands && crate::tools::bash::sandbox_on() {
        crate::permissions::BACKGROUND_SANDBOX_ALLOW.store(true, Ordering::Relaxed);
    }
    let env = setup.env;
    let mut agent = setup.agent;
    agent.cwd_override = Some(run.worktree.clone());
    // `/background`: carry the conversation over (a resumed run already has it).
    if let (Some(p), false) = (&run.from_session, run.resume) {
        match crate::session::Session::load_file(p) {
            Ok((_, messages)) => {
                agent.messages = messages;
                agent.map_worktree(run.source.path.clone(), run.worktree.clone());
            }
            Err(e) => {
                let _ = out.send(json!({"type": "warning", "message": format!("Couldn't load the session to continue ({e:#}); starting from the task alone.")}));
            }
        }
    }
    agent.refresh_system_prompt();
    for w in setup.warnings {
        let _ = out.send(json!({"type": "warning", "message": w}));
    }
    let session_id = agent.session.as_ref().map(|s| s.id.clone());
    let _ =
        out.send(json!({"type": "run_started", "session_id": session_id, "model": env.model()}));

    let cancel = CancellationToken::new();
    // Why the turn was cancelled: stop, time limit, or the daemon went away.
    let reason: Arc<Mutex<Option<(RunState, String)>>> = Default::default();
    let listener = {
        let (fe, cancel, reason) = (fe.clone(), cancel.clone(), reason.clone());
        tokio::spawn(async move {
            while let Some(msg) = input.recv().await {
                match msg["type"].as_str() {
                    Some("respond") => {
                        fe.resolve(msg);
                    }
                    Some("stop") => {
                        reason
                            .lock()
                            .unwrap()
                            .get_or_insert((RunState::Cancelled, "stopped".into()));
                        cancel.cancel();
                    }
                    _ => {}
                }
            }
            reason.lock().unwrap().get_or_insert((
                RunState::Interrupted,
                "the chanto daemon stopped while the run was active".into(),
            ));
            cancel.cancel();
        })
    };
    let clock = {
        let (fe, cancel, reason) = (fe.clone(), cancel.clone(), reason.clone());
        let limit = s.max_minutes.max(1) * 60;
        tokio::spawn(async move {
            let mut active = 0u64;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
                if fe.waiting.load(Ordering::SeqCst) == 0 {
                    active += 1;
                }
                if active >= limit {
                    reason.lock().unwrap().get_or_insert((
                        RunState::Failed,
                        format!("wall-clock limit of {} minutes reached", limit / 60),
                    ));
                    cancel.cancel();
                    break;
                }
            }
        })
    };

    // A goal run: criteria (approved through needs-input), then iterations.
    let goal = if run.kind.as_deref() == Some("goal") {
        let base = crate::goal::goals_dir();
        let limits = env.config.goal.limits();
        Some(
            if run.resume && base.join(&run.id).join("goal.json").exists() {
                crate::goal::resume(
                    &mut agent,
                    &base,
                    Some(&run.id),
                    Some(limits),
                    cancel.clone(),
                )
                .await
            } else {
                crate::goal::start(
                    &mut agent,
                    &run.task,
                    &base,
                    Some(run.id.clone()),
                    limits,
                    cancel.clone(),
                )
                .await
            },
        )
    } else {
        None
    };
    let mut res = match &goal {
        Some(Ok(_)) => Ok(TurnEnd::Done(String::new())),
        Some(Err(e)) => Err(anyhow::anyhow!("{e:#}")),
        None => {
            agent
                .run_turn(&run_prompt(run), vec![], cancel.clone())
                .await
        }
    };
    agent.save_session();
    // Background agents the run started report back before it ends.
    while goal.is_none() && matches!(res, Ok(TurnEnd::Done(_))) && !cancel.is_cancelled() {
        let waiting = |env: &crate::agent::Env| {
            crate::agent::background_running(env) + crate::tools::monitor::active(env)
        };
        if env.notifications.lock().unwrap().is_empty() && waiting(&env) > 0 {
            let notified = env.bg_done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if waiting(&env) > 0 && env.notifications.lock().unwrap().is_empty() {
                tokio::select! {
                    _ = notified => {}
                    _ = cancel.cancelled() => break,
                }
            }
            continue;
        }
        let reports: Vec<String> = env.notifications.lock().unwrap().drain(..).collect();
        if reports.is_empty() {
            break;
        }
        res = agent
            .run_turn(&reports.join("\n\n"), vec![], cancel.clone())
            .await;
        agent.save_session();
    }
    listener.abort();
    clock.abort();
    let cost = env.accounting.total();
    let verify = match (s.verify, &agent.last_verify) {
        (false, _) => VerifyResult {
            status: "off".into(),
            detail: String::new(),
        },
        (true, Some((true, d))) => VerifyResult {
            status: "passed".into(),
            detail: d.clone(),
        },
        (true, Some((false, d))) => VerifyResult {
            status: "failed".into(),
            detail: d.clone(),
        },
        (true, None) => VerifyResult {
            status: "skipped".into(),
            detail: "no changes to verify".into(),
        },
    };
    let (state, result, error) = match (goal, res) {
        (Some(Ok((crate::goal::GoalState::Done, report))), _) => {
            (RunState::Done, Some(report), None)
        }
        (Some(Ok((_, _))), _) if cancel.is_cancelled() => {
            let (st, why) = reason
                .lock()
                .unwrap()
                .clone()
                .unwrap_or((RunState::Cancelled, "interrupted".into()));
            (st, None, Some(why))
        }
        // Stopped, paused or blocked: the report says why; resume continues it.
        (Some(Ok((st, report))), _) => (
            RunState::Failed,
            Some(report),
            Some(format!("goal {}", st.label())),
        ),
        (_, res) => match res {
            Ok(TurnEnd::Done(text)) if text.starts_with("USD budget reached") => (
                RunState::Failed,
                None,
                Some(format!("cost budget of ${:.2} reached", s.max_cost_usd)),
            ),
            Ok(TurnEnd::Done(text)) => (RunState::Done, Some(text), None),
            Ok(TurnEnd::Interrupted) => {
                let (st, why) = reason
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or((RunState::Cancelled, "interrupted".into()));
                (st, None, Some(why))
            }
            Err(e) => (RunState::Failed, None, Some(format!("{e:#}"))),
        },
    };
    env.shutdown().await;
    Ok(
        json!({"type": "run_end", "state": state, "result": result, "error": error, "verify": verify,
        "cost_usd": cost, "session_id": session_id}),
    )
}

/// `chanto __run-worker <id> [<runs dir>]`: the daemon's child process for
/// one run. The directory is passed when the run lives outside the default
/// store (`chanto runner connect`).
pub async fn main(args: &[String]) -> i32 {
    let Some(id) = args.first() else {
        eprintln!("usage: chanto __run-worker <run-id> [<runs dir>]");
        return 2;
    };
    let dir = args
        .get(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(super::Store::default_dir);
    let store = super::Store::new(dir);
    let run = match store.load(id) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e:#}");
            return 1;
        }
    };
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let (in_tx, in_rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && in_tx.send(v).is_err()
            {
                break;
            }
        }
        // Dropping in_tx tells the worker the daemon is gone.
    });
    let writer = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut stdout = tokio::io::stdout();
        while let Some(v) = out_rx.recv().await {
            let line = format!("{v}\n");
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
            // Background tasks may still hold the frontend; the end is the end.
            if v["type"] == "run_end" {
                break;
            }
        }
    });
    self::run(run, out_tx, in_rx).await;
    let _ = writer.await;
    0
}
