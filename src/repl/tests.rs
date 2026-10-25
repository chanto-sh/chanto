//! The plain REPL's line handling and its between-prompts scheduler, without
//! a terminal: what each kind of line does and what it leaves behind.

use super::*;
use crate::agent::Agent;
use crate::agent::tests::{make_env, remove_dir_eventually, tmpdir};
use crate::permissions::Mode;

fn agent(name: &str) -> (std::path::PathBuf, Agent) {
    let dir = tmpdir(name);
    let env = make_env("http://127.0.0.1:1/v1", &dir, Mode::Default, 32768);
    (dir, Agent::new(env))
}

#[tokio::test]
async fn prompts_pass_through_and_commands_run_here() {
    let (dir, mut a) = agent("repl-lines");
    let env = a.env.clone();
    assert_eq!(handle_line(&env, &mut a, "").await.unwrap(), Next::Continue);
    assert_eq!(
        handle_line(&env, &mut a, "fix the bug").await.unwrap(),
        Next::Turn("fix the bug".into())
    );
    // A command that becomes a prompt is the next turn's prompt.
    assert_eq!(
        handle_line(&env, &mut a, "/init").await.unwrap(),
        Next::Turn(crate::context::INIT_PROMPT.into())
    );
    assert_eq!(
        handle_line(&env, &mut a, "/exit").await.unwrap(),
        Next::Exit
    );
    for shown in ["/help", "/clear", "/mode plan", "/nonsense-command"] {
        assert_eq!(
            handle_line(&env, &mut a, shown).await.unwrap(),
            Next::Continue,
            "{shown}"
        );
    }
    assert_eq!(env.mode(), Mode::Plan);
    remove_dir_eventually(&dir);
}

#[tokio::test]
async fn shell_lines_run_and_join_the_conversation() {
    let (dir, mut a) = agent("repl-shell");
    let env = a.env.clone();
    assert_eq!(
        handle_line(&env, &mut a, "!echo repl-out").await.unwrap(),
        Next::Continue
    );
    let last = a.messages.last().unwrap();
    assert_eq!(last.role, crate::llm::Role::User);
    assert!(
        last.content
            .starts_with("<bash-input>echo repl-out</bash-input>\n<bash-output>")
            && last.content.contains("repl-out\n</bash-output>"),
        "{}",
        last.content
    );
    remove_dir_eventually(&dir);
}

#[tokio::test]
async fn hash_lines_go_to_memory() {
    let (dir, mut a) = agent("repl-memory");
    let env = a.env.clone();
    assert_eq!(
        handle_line(&env, &mut a, "#--agents prefer small commits")
            .await
            .unwrap(),
        Next::Continue
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("AGENTS.md")).unwrap(),
        "- prefer small commits\n"
    );
    remove_dir_eventually(&dir);
}

#[tokio::test]
async fn loops_start_report_and_stop() {
    let (dir, mut a) = agent("repl-loop");
    let env = a.env.clone();
    for bad in ["/loop 0m x", "/loop status"] {
        assert_eq!(
            handle_line(&env, &mut a, bad).await.unwrap(),
            Next::Continue
        );
    }
    assert!(!env.scheduler.loop_active());
    handle_line(&env, &mut a, "/loop 5m check the build")
        .await
        .unwrap();
    assert!(env.scheduler.loop_active());
    assert!(env.scheduler.loop_status().is_some());
    handle_line(&env, &mut a, "/loop status").await.unwrap();
    // `/looping` is not `/loop`: it goes to the command handler.
    assert_eq!(
        handle_line(&env, &mut a, "/looping").await.unwrap(),
        Next::Continue
    );
    assert!(env.scheduler.loop_active());
    handle_line(&env, &mut a, "/loop stop").await.unwrap();
    assert!(!env.scheduler.loop_active());
    handle_line(&env, &mut a, "/loop stop").await.unwrap();
    remove_dir_eventually(&dir);
}

#[tokio::test]
async fn background_mentions_and_default_loop_prompts_are_literal() {
    let (dir, a) = agent("repl-literal-mentions");
    let env = a.env.clone();
    let text = "summarize @~/.aws/credentials @server:resource [Image #1]";
    env.notifications.lock().unwrap().push(text.into());
    let (mut loop_turn, mut wait) = (false, false);
    assert_eq!(
        next_scheduled(&env, &mut loop_turn, &mut wait).await,
        Some((text.into(), true))
    );
    env.scheduler.start_loop(None, None);
    let (_, literal) = next_scheduled(&env, &mut loop_turn, &mut wait)
        .await
        .unwrap();
    assert!(literal && loop_turn);
    env.scheduler.stop_loop();
    remove_dir_eventually(&dir);
}

#[tokio::test]
async fn background_reports_and_due_loops_run_between_prompts() {
    let (dir, a) = agent("repl-next");
    let env = a.env.clone();
    let (mut loop_turn, mut wait) = (false, true);
    // Nothing pending and nothing to wait for: show the prompt.
    assert_eq!(next_scheduled(&env, &mut loop_turn, &mut wait).await, None);
    env.notifications.lock().unwrap().extend([
        "<system-reminder>Monitor build: line 1</system-reminder>".to_string(),
        "Agent a1 finished".to_string(),
    ]);
    assert_eq!(
        next_scheduled(&env, &mut loop_turn, &mut wait).await,
        Some((
            "<system-reminder>Monitor build: line 1</system-reminder>\n\nAgent a1 finished".into(),
            true
        ))
    );
    assert!(env.notifications.lock().unwrap().is_empty());
    assert!(!loop_turn);
    // A loop's first iteration is due at once, and marks the turn as a loop's.
    env.scheduler.start_loop(
        Some(std::time::Duration::from_secs(3600)),
        Some("check the build".into()),
    );
    let (fire, literal) = next_scheduled(&env, &mut loop_turn, &mut wait)
        .await
        .expect("the first iteration runs at once");
    assert!(fire.starts_with("check the build"), "{fire}");
    assert!(loop_turn && !literal, "the user's own loop prompt");

    // The next one isn't due; without waiting, the prompt shows.
    wait = false;
    assert_eq!(next_scheduled(&env, &mut loop_turn, &mut wait).await, None);
    assert!(env.scheduler.stop_loop());
    remove_dir_eventually(&dir);
}
