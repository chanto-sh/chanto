//! Simple line-based interactive mode (`--plain`), built on reedline.

use crate::agent::TurnEnd;
use crate::commands::{self, Outcome};
use crate::plain::{OutputFormat, PlainFrontend};
use crate::{Cli, Setup, setup, ui};
use anyhow::Result;
use reedline::{
    DefaultPrompt, DefaultPromptSegment, EditCommand, Emacs, FileBackedHistory, KeyCode,
    KeyModifiers, Reedline, ReedlineEvent, Signal, default_emacs_keybindings,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub async fn run(cli: Cli) -> Result<()> {
    let fe = Arc::new(PlainFrontend::new(OutputFormat::Live, true));
    // Between prompts the REPL fires /loop iterations and scheduled tasks.
    crate::cron::set_frontend_drives();
    let Setup {
        env,
        mut agent,
        warnings,
    } = setup(&cli, fe.clone(), true).await?;
    env.scheduler.set_driven();
    ui::banner(
        &env.model(),
        &env.cwd().display().to_string(),
        env.mode().label(),
    );
    for w in warnings {
        ui::warn(&w);
    }
    let mut kb = default_emacs_keybindings();
    kb.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::BackTab,
        ReedlineEvent::ExecuteHostCommand("__cycle_mode".into()),
    );
    kb.add_binding(
        KeyModifiers::ALT,
        KeyCode::Enter,
        ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
    );
    kb.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::Enter,
        ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
    );
    let hist = FileBackedHistory::with_file(1000, crate::config::home_dir().join("history.txt"))?;
    let mut ed = Reedline::create()
        .with_edit_mode(Box::new(Emacs::new(kb)))
        .with_history(Box::new(hist));
    let mut pending: Option<String> = if cli.prompt.is_empty() {
        None
    } else {
        Some(cli.prompt.join(" "))
    };
    // The last turn was a loop iteration; whether to wait for loops and monitors.
    let (mut loop_turn, mut wait) = (false, true);
    let mut turn_ended = false;
    // SIGTERM/SIGHUP: a running turn is interrupted (it saves the session as it
    // ends) and the loop exits; at the prompt, everything is already saved.
    let running: Arc<std::sync::Mutex<Option<CancellationToken>>> = Default::default();
    {
        let running = running.clone();
        crate::util::on_termination(move |code| match running.lock().unwrap().as_ref() {
            Some(c) if !c.is_cancelled() => c.cancel(),
            _ => {
                let _ = crossterm::terminal::disable_raw_mode();
                std::process::exit(code);
            }
        });
    }
    loop {
        if std::mem::take(&mut loop_turn)
            && let Some(n) = env.scheduler.iteration_done()
        {
            ui::line(&ui::dim(&n));
        }
        let scheduled = if pending.is_none() {
            next_scheduled(&env, &mut loop_turn, &mut wait).await
        } else {
            None
        };
        let line = match pending.take().or(scheduled) {
            Some(p) => p,
            None => {
                let prompt = DefaultPrompt::new(
                    DefaultPromptSegment::Basic(format!("[{}] ", env.mode().label())),
                    DefaultPromptSegment::Empty,
                );
                // The `idle_prompt` notification: a minute after the
                // last turn ended with nothing typed since.
                let idle = std::mem::take(&mut turn_ended).then(|| {
                    let env = env.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                        if crate::agent::background_running(&env) == 0 && crate::tools::monitor::active(&env) == 0 {
                            let msg = "Chanto is waiting for your input";
                            crate::hooks::notify(&env, "Notification", Some("idle_prompt"), serde_json::json!({"message": msg, "notification_type": "idle_prompt"})).await;
                        }
                    })
                });
                let read = ed.read_line(&prompt);
                if let Some(t) = idle {
                    t.abort();
                }
                wait = true;
                match read? {
                    Signal::Success(s) => s,
                    Signal::HostCommand(c) if c == "__cycle_mode" => {
                        env.set_mode(env.mode().next());
                        continue;
                    }
                    Signal::CtrlC => continue,
                    Signal::CtrlD => break,
                    _ => continue,
                }
            }
        };
        let prompt = match handle_line(&env, &mut agent, line.trim()).await? {
            Next::Continue => continue,
            Next::Exit => break,
            Next::Turn(p) => p,
        };
        let (text, images) = commands::expand_mentions(&agent, &prompt).await;
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        let sig = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                c2.cancel();
            }
        });
        *running.lock().unwrap() = Some(cancel.clone());
        // Quiet mode (`--quiet`, `/quiet`): the steps stay out of the output
        // (`/transcript` shows them) and the final summary prints at the end.
        let quiet = env.quiet();
        fe.set_quiet(quiet);
        let end = agent.run_turn(&text, images, cancel).await;
        *running.lock().unwrap() = None;
        if let Some(code) = crate::util::terminated() {
            agent.save_session();
            env.shutdown().await;
            std::process::exit(code);
        }
        match end {
            Ok(TurnEnd::Done(t)) => {
                if quiet {
                    fe.print_final(&t);
                }
            }
            Ok(TurnEnd::Interrupted) => ui::line(&ui::dim("↳ Interrupted")),
            Err(e) => ui::error(&format!("Error: {e:#}")),
        }
        sig.abort();
        agent.save_session();
        turn_ended = true;
        ui::out("\n");
    }
    agent.save_session();
    if let Some(note) = crate::session_cmds::take_exit_note() {
        ui::line(&note);
    }
    env.shutdown().await;
    Ok(())
}

/// What the loop does after a line.
#[derive(Debug, PartialEq)]
enum Next {
    /// Handled here (a command, `!shell`, `#note`, `/loop`): read the next line.
    Continue,
    Exit,
    /// Send this prompt to the model.
    Turn(String),
}

/// Handle one line typed at the prompt (or a scheduled prompt), short of
/// running a model turn: slash commands, `/loop`, `!shell` and `#memory` run
/// here; anything else is the prompt for the next turn.
async fn handle_line(
    env: &Arc<crate::agent::Env>,
    agent: &mut crate::agent::Agent,
    line: &str,
) -> Result<Next> {
    if line.is_empty() {
        return Ok(Next::Continue);
    }
    if let Some(arg) = line
        .strip_prefix("/loop")
        .filter(|a| a.is_empty() || a.starts_with(char::is_whitespace))
    {
        match commands::parse_loop(arg) {
            Err(e) => ui::warn(&e),
            Ok(commands::LoopCmd::Stop) => ui::line(&ui::dim(if env.scheduler.stop_loop() {
                "Loop stopped."
            } else {
                "No loop is running."
            })),
            Ok(commands::LoopCmd::Status) => {
                ui::line(&ui::dim(&env.scheduler.loop_status().unwrap_or_else(
                    || format!("No loop is running. {}", commands::LOOP_USAGE),
                )))
            }
            Ok(commands::LoopCmd::Start { every, prompt }) => {
                ui::line(&ui::dim(
                    &commands::start_loop(env, every, prompt)
                        .replace("or esc", "or ctrl+c while it waits"),
                ));
            }
        }
        return Ok(Next::Continue);
    }
    let prompt = if line.starts_with('/') {
        match commands::handle(agent, line).await {
            Outcome::Show(s) => {
                ui::out(&ui::render_markdown(&s));
                ui::out("\n");
                return Ok(Next::Continue);
            }
            Outcome::Prompt(p) => p,
            Outcome::Run(job) => {
                let cancel = tokio_util::sync::CancellationToken::new();
                let c2 = cancel.clone();
                let sig = tokio::spawn(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    c2.cancel();
                });
                let label = job.label();
                match job.run(agent, cancel).await {
                    Ok(r) => ui::out(&ui::render_markdown(&r)),
                    Err(e) => ui::error(&format!("{label} failed: {e:#}")),
                }
                sig.abort();
                agent.save_session();
                return Ok(Next::Continue);
            }
            Outcome::Exit => return Ok(Next::Exit),
            Outcome::Reset(msg) => {
                ui::line(&ui::dim(
                    &msg.unwrap_or_else(|| "(new conversation)".into()),
                ));
                return Ok(Next::Continue);
            }
            Outcome::Nothing => return Ok(Next::Continue),
        }
    } else if let Some(cmd) = line.strip_prefix('!') {
        if env.config.learning.capture() {
            crate::learn::record(&env.root, "shell", cmd.trim(), "");
        }
        crate::shell::snapshot::ensure(env).await;
        let out =
            crate::shell::std_command(&format!("{}{cmd}", crate::shell::snapshot::prelude(env)))
                .current_dir(env.cwd())
                .output()?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        ui::out(&text);
        agent.messages.push(crate::llm::Message::user(format!(
            "<bash-input>{cmd}</bash-input>\n<bash-output>{}</bash-output>",
            crate::util::truncate(&text, 20_000)
        )));
        return Ok(Next::Continue);
    } else if let Some(note) = line.strip_prefix('#') {
        if let Outcome::Show(s) = commands::add_memory(agent, note) {
            ui::line(&ui::dim(&s));
        }
        return Ok(Next::Continue);
    } else {
        line.to_string()
    };
    Ok(Next::Turn(prompt))
}

/// What to run next without the user: background reports (Monitor lines,
/// finished agents), then a due `/loop` iteration or scheduled task. While a
/// loop or a monitor is active this waits for them instead of showing the
/// prompt (reedline can't be interrupted); ctrl+c stops the loop, or stops
/// waiting for monitors, whose events then arrive with your next prompt.
/// Scheduled tasks alone don't hold the prompt: they fire between prompts.
async fn next_scheduled(
    env: &Arc<crate::agent::Env>,
    loop_turn: &mut bool,
    wait: &mut bool,
) -> Option<String> {
    let mut announced = false;
    loop {
        let reports: Vec<String> = env.notifications.lock().unwrap().drain(..).collect();
        if !reports.is_empty() {
            for r in &reports {
                if let Some(rest) = r.strip_prefix("<system-reminder>Monitor ") {
                    ui::line(&ui::dim(&format!(
                        "◉ Monitor {}",
                        rest.split("</system-reminder>")
                            .next()
                            .unwrap_or("")
                            .trim_end_matches(':')
                    )));
                }
            }
            return Some(reports.join("\n\n"));
        }
        if let Some(fire) = env.scheduler.take_due(chrono::Local::now(), &env.cwd()) {
            let shown = fire
                .prompt
                .split("\n\n<system-reminder>")
                .next()
                .unwrap_or("")
                .to_string();
            let label = if fire.note.is_empty() {
                "↻ Loop iteration".to_string()
            } else {
                fire.note.clone()
            };
            ui::line(&ui::dim(&format!(
                "{label}: {}",
                crate::util::one_line(&shown, 100)
            )));
            *loop_turn = fire.source != crate::cron::Source::Cron;
            return Some(fire.prompt);
        }
        let looping = env.scheduler.loop_active();
        let monitors = crate::tools::monitor::active(env);
        if !*wait || (!looping && monitors == 0) {
            return None;
        }
        if !announced {
            announced = true;
            let what = match (looping, env.scheduler.loop_next()) {
                (true, Some(next)) => format!("Next loop iteration at {}", crate::cron::at(next)),
                (true, None) => "Loop running".into(),
                _ => format!("Waiting for {monitors} monitor(s)"),
            };
            ui::line(&ui::dim(&format!(
                "{what} · ctrl+c to {}",
                if looping {
                    "stop the loop"
                } else {
                    "stop waiting"
                }
            )));
        }
        let notified = env.bg_done.notified();
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
            _ = notified => {}
            _ = tokio::signal::ctrl_c() => {
                *wait = false;
                if looping && env.scheduler.stop_loop() {
                    ui::line(&ui::dim("Loop stopped."));
                } else {
                    ui::line(&ui::dim("Stopped waiting; monitor events arrive with your next prompt."));
                }
                return None;
            }
        }
    }
}
