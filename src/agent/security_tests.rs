//! Restored session settings, user-scope memory approval and the
//! approve-then-swap check on path-taking calls.

use super::authorize::memory_preview;
use super::tests::{make_env, make_env_with, mock_server, tc, tmpdir};
use super::*;

fn resume_with(env: Arc<Env>, mode: &str) -> Agent {
    let mut agent = Agent::new(env.clone());
    let mut s = crate::session::Session::new(&env.cwd(), "mock");
    s.settings = json!({"mode": mode});
    agent.session = Some(s);
    agent.restore_todos();
    agent
}

#[test]
fn a_session_file_never_restores_bypass_mode() {
    let dir = tmpdir("restore-bypass");
    let env = make_env("http://localhost:1", &dir, Mode::Default, 32768);
    resume_with(env.clone(), "bypass");
    assert_eq!(env.mode(), Mode::Default);
    resume_with(env.clone(), "acceptEdits");
    assert_eq!(env.mode(), Mode::AcceptEdits);
}

#[test]
fn a_managed_policy_mode_outranks_the_restored_one() {
    let dir = tmpdir("restore-policy");
    let env = make_env_with("http://localhost:1", &dir, Mode::Default, 32768, |c| {
        c.policy_mode = Some(Mode::Plan)
    });
    resume_with(env.clone(), "acceptEdits");
    assert_eq!(env.mode(), Mode::Plan);
}

#[test]
fn user_scope_memory_saves_ask_outside_bypass() {
    let cwd = Path::new("/tmp");
    let save = |scope: Option<&str>| {
        let mut a =
            json!({"action": "save", "name": "n", "description": "d", "type": "user", "body": "b"});
        if let Some(s) = scope {
            a["scope"] = json!(s);
        }
        a
    };
    for mode in [Mode::Default, Mode::AcceptEdits, Mode::Auto] {
        for args in [save(Some("user")), save(None)] {
            assert_eq!(
                permissions::static_decision(mode, ToolKind::Edit, "Memory", &args, cwd),
                Some(Decision::Ask),
                "{mode:?} {args}"
            );
        }
        assert_eq!(
            permissions::static_decision(
                mode,
                ToolKind::Edit,
                "Memory",
                &save(Some("project")),
                cwd
            ),
            Some(Decision::Allow)
        );
    }
    assert_eq!(
        permissions::static_decision(
            Mode::Bypass,
            ToolKind::Edit,
            "Memory",
            &save(Some("user")),
            cwd
        ),
        Some(Decision::Allow)
    );
    let preview = memory_preview(&save(Some("user")));
    assert!(
        preview.contains("every project")
            && preview.contains("Description: d")
            && preview.ends_with("b")
    );
}

#[tokio::test]
async fn memory_without_a_scope_is_judged_by_the_configured_one() {
    let dir = tmpdir("memory-scope");
    let call =
        json!({"action": "save", "name": "n", "description": "d", "type": "user", "body": "b"});
    let (base, seen) = mock_server(vec![
        json!({"tool_calls": tc("Memory", call)}),
        json!({"content": "done"}),
    ])
    .await;
    let env = make_env_with(&base, &dir, Mode::AcceptEdits, 32768, |c| {
        c.memory.scope = "user".into()
    });
    let mut agent = Agent::new(env);
    agent
        .run_turn("remember", vec![], CancellationToken::new())
        .await
        .unwrap();
    let sent = seen.lock().unwrap()[1]["messages"].to_string();
    assert!(sent.contains("requires user approval"), "{sent}");
}

#[test]
fn a_cron_task_needs_approval_in_default_mode() {
    let tool = crate::tools::schedule::CronCreate;
    let args = json!({"cron": "* * * * *", "prompt": "run the tests"});
    for (mode, want) in [
        (Mode::Default, Some(Decision::Ask)),
        (Mode::AcceptEdits, Some(Decision::Ask)),
        (Mode::Bypass, Some(Decision::Allow)),
    ] {
        assert_eq!(
            permissions::static_decision(
                mode,
                tool.kind_for(&args),
                "CronCreate",
                &args,
                Path::new("/tmp")
            ),
            want,
            "{mode:?}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_swapped_in_after_approval_blocks_the_call() {
    let dir = tmpdir("swap");
    let outside = tmpdir("swap-outside");
    let target = outside.join("victim.txt");
    std::fs::write(&target, "original\n").unwrap();
    let calls = json!([
        {"index": 0, "id": "c1", "type": "function", "function": {"name": "Bash", "arguments":
            json!({"command": format!("ln -s {} notes.txt", target.display())}).to_string()}},
        {"index": 1, "id": "c2", "type": "function", "function": {"name": "Write", "arguments":
            json!({"file_path": "notes.txt", "content": "pwned\n"}).to_string()}},
    ]);
    let (base, seen) = mock_server(vec![
        json!({"tool_calls": calls}),
        json!({"content": "done"}),
    ])
    .await;
    let env = make_env_with(&base, &dir, Mode::Bypass, 32768, |c| {
        c.sandbox.enabled = false;
    });
    let mut agent = Agent::new(env);
    agent
        .run_turn("write notes", vec![], CancellationToken::new())
        .await
        .unwrap();
    assert!(dir.join("notes.txt").is_symlink(), "the command ran");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "original\n");
    let sent = seen.lock().unwrap()[1]["messages"].to_string();
    assert!(sent.contains("when this call was approved"), "{sent}");
    let _ = std::fs::remove_dir_all(&outside);
}

#[cfg(unix)]
#[tokio::test]
async fn a_command_relinked_to_a_secret_after_approval_is_blocked() {
    let dir = tmpdir("swap-exec");
    let outside = tmpdir("swap-exec-outside");
    let key = outside.join("id_rsa");
    std::fs::write(&key, "fake-key-material\n").unwrap();
    let link = dir.join("notes");
    let calls = json!([
        {"index": 0, "id": "c1", "type": "function", "function": {"name": "Bash", "arguments":
            json!({"command": format!("ln -s {} {}", key.display(), link.display())}).to_string()}},
        {"index": 1, "id": "c2", "type": "function", "function": {"name": "Bash", "arguments":
            json!({"command": format!("cat {}", link.display())}).to_string()}},
    ]);
    let (base, seen) = mock_server(vec![
        json!({"tool_calls": calls}),
        json!({"content": "done"}),
    ])
    .await;
    let env = make_env_with(&base, &dir, Mode::Default, 32768, |c| {
        c.sandbox.enabled = false;
        c.permissions.allow.push("Bash(ln:*)".into());
    });
    let mut agent = Agent::new(env);
    agent
        .run_turn("look at notes", vec![], CancellationToken::new())
        .await
        .unwrap();
    assert!(link.is_symlink(), "the ln ran");
    let sent = seen.lock().unwrap()[1]["messages"].to_string();
    assert!(!sent.contains("fake-key-material"), "{sent}");
    assert!(sent.contains("since this command was approved"), "{sent}");
    let _ = std::fs::remove_dir_all(&outside);
}
