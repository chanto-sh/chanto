use super::*;
use crate::agent::tests::{mock_server, tc, tmpdir};
use crate::agent::{Agent, EnvInit, Event, Frontend, PermissionReply, PermissionRequest, TurnEnd};
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
        <crate::Cli as clap::Parser>::try_parse_from(["chanto", "--init-only", "--init"]).is_err()
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
        serde_json::from_str(&std::fs::read_to_string(dir.join("create.json")).unwrap()).unwrap();
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
        serde_json::from_str(&std::fs::read_to_string(dir.join("remove.json")).unwrap()).unwrap();
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
        tools.contains(&"Read".to_string()) && !tools.iter().any(|t| t == "Bash" || t == "Write"),
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
