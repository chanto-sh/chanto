//! The oracle subagent and waiting out usage limits.

use super::tests::{make_env, make_env_with, mock_server, tmpdir};
#[cfg(unix)]
use super::tests::{mock_server_routed, tc};
use super::*;

#[test]
fn oracle_is_read_only_and_uses_its_own_model() {
    let dir = tmpdir("oracle-build");
    let env = make_env("http://localhost:1", &dir, Mode::Default, 32768);
    let a = build_subagent(&env, 1, "oracle");
    assert!(
        a.tool_allowed("Read")
            && a.tool_allowed("Grep")
            && !a.tool_allowed("Write")
            && !a.tool_allowed("Edit")
    );
    assert_eq!(a.model(), "mock", "falls back to the main model");
    assert!(a.system_extra.as_deref().unwrap().contains("LGTM"));
    let env = make_env_with("http://localhost:1", &dir, Mode::Default, 32768, |c| {
        c.oracle_model = Some("ollama/big".into());
        c.subagent_model = Some("ollama/small".into());
    });
    assert_eq!(build_subagent(&env, 1, "Oracle").model(), "ollama/big");
    assert_eq!(
        build_subagent(&env, 1, "general-purpose").model(),
        "ollama/small"
    );
    // A custom agent named oracle replaces the built-in.
    env.agents.write().unwrap().push(AgentDef {
        name: "oracle".into(),
        description: String::new(),
        tools: None,
        disallowed_tools: vec![],
        model: None,
        prompt: "mine".into(),
        ..Default::default()
    });
    assert!(
        build_subagent(&env, 1, "oracle")
            .system_extra
            .unwrap()
            .starts_with("mine")
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn oracle_approval_is_recognized() {
    assert!(
        oracle_approves("LGTM")
            && oracle_approves("**LGTM**")
            && oracle_approves("lgtm.")
            && oracle_approves("LGTM — nothing to fix")
    );
    assert!(!oracle_approves(
        "src/a.rs:3: off by one; LGTM otherwise but this needs fixing before it ships, really"
    ));
    assert!(!oracle_approves("Bug in a.txt"));
}

#[cfg(unix)]
#[tokio::test]
async fn verified_mode_asks_the_oracle_once_before_the_gate() {
    let dir = tmpdir("oracle-verify");
    let (base, seen) = mock_server_routed(vec![
        ("Review this change", vec![json!({"content": "a.txt:1: the greeting is misspelled (helo)."})]),
        (
            "write a greeting",
            vec![
                json!({"tool_calls": tc("Write", json!({"file_path": "a.txt", "content": "helo\n"}))}),
                json!({"content": "Done."}),
                json!({"tool_calls": tc("Edit", json!({"file_path": "a.txt", "old_string": "helo", "new_string": "hello"}))}),
                json!({"content": "Fixed the typo."}),
            ],
        ),
    ])
    .await;
    let env = make_env_with(&base, &dir, Mode::AcceptEdits, 32768, |c| {
        c.guardrails.enabled = true;
        c.guardrails.checks = vec!["true".into()];
        c.guardrails.protect_tests = Some(false);
        c.guardrails.oracle = Some(true);
    });
    let mut agent = Agent::new(env);
    let end = agent
        .run_turn("write a greeting", vec![], CancellationToken::new())
        .await
        .unwrap();
    let TurnEnd::Done(text) = end else {
        panic!("interrupted")
    };
    assert!(
        text.starts_with("Fixed the typo.") && text.contains("Verified"),
        "{text}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "hello\n"
    );
    let reqs = seen.lock().unwrap();
    let oracle: Vec<&Value> = reqs
        .iter()
        .filter(|r| {
            r.to_string().contains("Review this change")
                && r["messages"][1]["content"]
                    .as_str()
                    .is_some_and(|c| c.starts_with("Review this change"))
        })
        .collect();
    assert_eq!(oracle.len(), 1, "one review per turn");
    let prompt = oracle[0]["messages"][1]["content"].as_str().unwrap();
    assert!(
        prompt.contains("+helo") && prompt.contains("b/a.txt"),
        "{prompt}"
    );
    assert!(
        reqs.iter()
            .any(|r| r.to_string().contains("misspelled (helo)")
                && r.to_string().contains("the oracle")),
        "findings reached the model"
    );
    drop(reqs);
    let _ = std::fs::remove_dir_all(dir);
}

fn usage_limit(resets_in: u64) -> Value {
    let body = json!({"error": {"type": "usage_limit_reached", "message": "You've hit your usage limit", "resets_in_seconds": resets_in}});
    json!({"http_status": 429, "body": body.to_string()})
}

#[tokio::test]
async fn a_usage_limit_is_waited_out_when_resume_after_limit_is_on() {
    let dir = tmpdir("limit-resume");
    let (base, seen) = mock_server(vec![usage_limit(2), json!({"content": "Back at it."})]).await;
    let env = make_env_with(&base, &dir, Mode::Default, 32768, |c| {
        c.resume_after_limit = Some(true)
    });
    let mut agent = Agent::new(env);
    let t0 = Instant::now();
    let end = agent
        .run_turn("hello", vec![], CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(end, TurnEnd::Done(ref t) if t == "Back at it."));
    assert!(
        t0.elapsed() >= Duration::from_millis(900),
        "waited for the reset"
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        2,
        "the same request again after the reset"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn without_the_setting_a_non_interactive_run_fails_on_the_limit() {
    let dir = tmpdir("limit-fail");
    let (base, seen) = mock_server(vec![usage_limit(2), json!({"content": "unreachable"})]).await;
    let env = make_env(&base, &dir, Mode::Default, 32768);
    let mut agent = Agent::new(env);
    let err = agent
        .run_turn("hello", vec![], CancellationToken::new())
        .await
        .expect_err("fails");
    assert!(
        format!("{err:#}").contains("usage limit reached"),
        "{err:#}"
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn stopping_the_wait_interrupts_the_turn() {
    let dir = tmpdir("limit-cancel");
    let (base, _) = mock_server(vec![usage_limit(600)]).await;
    let env = make_env_with(&base, &dir, Mode::Default, 32768, |c| {
        c.resume_after_limit = Some(true)
    });
    let mut agent = Agent::new(env);
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        c2.cancel();
    });
    let end = agent.run_turn("hello", vec![], cancel).await.unwrap();
    assert!(matches!(end, TurnEnd::Interrupted));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn reset_times_read_as_a_clock() {
    let now = chrono::Utc::now().timestamp();
    let c = reset_clock(now + 60, now);
    assert!(c.len() == 5 && c.contains(':'), "{c}");
    assert!(
        reset_clock(now + 3 * 86_400, now).len() > 5,
        "another day names the day"
    );
}
