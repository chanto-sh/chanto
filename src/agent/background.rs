//! Background agents: spawning a Task that keeps running while the main agent
//! continues, and messaging, resuming or stopping it later.

use super::*;

pub(crate) fn spawn_background(
    env: Arc<Env>,
    id: String,
    handle: Arc<tokio::sync::Mutex<Agent>>,
    prompt: String,
    cancel: CancellationToken,
) {
    let (description, kind) = env
        .bg_agents
        .lock()
        .unwrap()
        .iter()
        .find(|b| b.id == id)
        .map(|b| (b.description.clone(), b.kind.clone()))
        .unwrap_or_default();
    // Not locked yet: the run below takes it.
    let key = handle.try_lock().ok().and_then(|a| a.parent.clone());
    if let Some(k) = &key {
        env.live_agents
            .start(k, &id, &kind, &description, &prompt, true);
    }
    env.emit(Event::Background {
        id: id.clone(),
        description: description.clone(),
        status: "running".into(),
    });
    tokio::spawn(async move {
        let t0 = Instant::now();
        // A panic must still produce a report, or `-p` would wait forever.
        let run = std::panic::AssertUnwindSafe(async {
            // Waits here (queued) while the maximum number of agents is running.
            let _spend_slot = env.spend.slot(&cancel).await;
            let _slot = crate::resources::agent_slot().await;
            let mut a = handle.lock().await;
            let mut end = a.run_turn(&prompt, vec![], cancel.clone()).await;
            // Messages the user sent as it was finishing: answer them too.
            loop {
                let more: Vec<String> = a.followups.lock().unwrap().drain(..).collect();
                if more.is_empty() || cancel.is_cancelled() || !matches!(end, Ok(TurnEnd::Done(_)))
                {
                    // Not run: they wait for a resume.
                    a.followups.lock().unwrap().extend(more);
                    break;
                }
                end = a.run_turn(&more.join("\n\n"), vec![], cancel.clone()).await;
            }
            report(&a, end, t0.elapsed().as_secs())
        });
        let (ok, text, stats) = match futures::FutureExt::catch_unwind(run).await {
            Ok(r) => r,
            Err(_) => (
                false,
                "Subagent crashed (internal error).".to_string(),
                crate::util::duration(t0.elapsed().as_secs()),
            ),
        };
        let status = if cancel.is_cancelled() {
            "stopped"
        } else if ok {
            "completed"
        } else {
            "failed"
        }
        .to_string();
        if let Some(k) = &key {
            env.live_agents.finish(k, status == "completed");
            env.spend.agent_finished(k);
        }
        let worktree = env
            .bg_agents
            .lock()
            .unwrap()
            .iter()
            .find(|b| b.id == id)
            .and_then(|b| b.worktree.clone());
        let mut body = text.clone();
        if let Some(w) = &worktree {
            // Keep the worktree while the agent might be resumed; just describe it.
            let changes = w.changes();
            body.push_str(&if changes.is_empty() {
                "\n\n(No changes in its worktree.)".to_string()
            } else {
                format!(
                    "\n\nWorktree: {} (branch `{}`)\n{changes}",
                    w.path.display(),
                    w.branch
                )
            });
        }
        // Queue the report before marking the agent done, so a waiter that sees
        // "not running" always finds the notification.
        env.notifications.lock().unwrap().push(format!(
            "<system-reminder>Background agent {id} ({description}) {status} ({stats}). Its report:</system-reminder>\n{body}"
        ));
        // Under the list's lock, so a message sent now either made it into the
        // run above or is picked up here (see `message_background`).
        let late: Vec<String> = match env
            .bg_agents
            .lock()
            .unwrap()
            .iter_mut()
            .find(|b| b.id == id)
        {
            Some(b) => {
                b.status = status.clone();
                b.result = Some(text.clone());
                if status == "completed" {
                    b.followups.lock().unwrap().drain(..).collect()
                } else {
                    vec![]
                }
            }
            None => vec![],
        };
        env.emit(Event::Background {
            id: id.clone(),
            description,
            status,
        });
        env.bg_done.notify_waiters();
        if !late.is_empty() {
            resume_background(&env, &id, &late.join("\n\n"));
        }
    });
}

/// What became of a message the user sent a background agent.
#[derive(Debug, PartialEq)]
pub enum Sent {
    /// It's running: the message waits for its next step.
    Queued,
    /// It had finished: it was resumed with the message.
    Resumed,
}

/// Send the user's message to background agent `id`: queued while it runs,
/// or resuming it (with its context) when it has finished.
pub fn message_background(env: &Arc<Env>, id: &str, text: &str) -> Result<Sent, String> {
    {
        let list = env.bg_agents.lock().unwrap();
        let Some(b) = list.iter().find(|b| b.id == id) else {
            return Err(format!("No background agent `{id}`."));
        };
        if b.status == "running" {
            b.followups.lock().unwrap().push(text.to_string());
            drop(list);
            env.live_agents.message(id, text);
            return Ok(Sent::Queued);
        }
    }
    let out = resume_background(env, id, text);
    if out.is_error {
        Err(out.content)
    } else {
        Ok(Sent::Resumed)
    }
}

/// Give a finished background agent more instructions (it keeps its context).
pub fn resume_background(env: &Arc<Env>, id: &str, prompt: &str) -> ToolOutput {
    let handle = {
        let mut list = env.bg_agents.lock().unwrap();
        let Some(b) = list.iter_mut().find(|b| b.id == id) else {
            return ToolOutput::err(format!("No background agent `{id}`."));
        };
        if b.status == "running" {
            return ToolOutput::err(format!(
                "Agent {id} is still running; wait for its report before sending more instructions."
            ));
        }
        b.status = "running".into();
        b.result = None;
        b.cancel = CancellationToken::new();
        (b.agent.clone(), b.cancel.clone())
    };
    spawn_background(
        env.clone(),
        id.to_string(),
        handle.0,
        prompt.to_string(),
        handle.1,
    );
    ToolOutput::ok(format!(
        "Resumed background agent {id}; you'll get its report when it finishes."
    ))
    .display(format!("Resumed {id}"))
}

/// Stop a running background agent.
pub fn stop_background(env: &Env, id: &str) -> bool {
    let list = env.bg_agents.lock().unwrap();
    match list.iter().find(|b| b.id == id && b.status == "running") {
        Some(b) => {
            b.cancel.cancel();
            true
        }
        None => false,
    }
}

pub fn background_running(env: &Env) -> usize {
    env.bg_agents
        .lock()
        .unwrap()
        .iter()
        .filter(|b| b.status == "running")
        .count()
}
