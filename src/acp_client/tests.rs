use super::*;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

#[tokio::test]
async fn acp_fs_is_confined_to_the_working_directory() {
    let dir = std::env::temp_dir().join(format!("acp-fs-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    // Links out of it: `..` after one leaves from its target, and writing
    // through a dangling one creates its target.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/usr/bin", dir.join("sys")).unwrap();
        std::os::unix::fs::symlink("/usr/chanto-acp-nope", dir.join("dangle")).unwrap();
    }
    // A write inside the working directory is allowed.
    write_text_file(
        &dir,
        &serde_json::json!({"path": "note.txt", "content": "hi"}),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("note.txt")).unwrap(), "hi");
    // Absolute and `..` paths that escape the working directory are refused,
    // before any read or write touches disk.
    for p in [
        "/etc/chanto-acp-nope",
        "../../etc/passwd",
        "/Users/x/.ssh/id_rsa",
        #[cfg(unix)]
        "sys/../chanto-acp-nope",
        #[cfg(unix)]
        "dangle",
    ] {
        let w = write_text_file(&dir, &serde_json::json!({"path": p, "content": "x"})).await;
        assert!(w.is_err(), "write {p} should be refused");
        assert!(
            w.unwrap_err()
                .to_string()
                .contains("outside the working directory")
        );
        assert!(
            read_text_file(&dir, &serde_json::json!({"path": p}))
                .await
                .is_err(),
            "read {p} should be refused"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

struct Fake {
    rx: UnboundedReceiver<Value>,
    sent: Arc<Mutex<Vec<Value>>>,
}

#[async_trait]
impl Transport for Fake {
    async fn send(&mut self, message: &Value) -> Result<()> {
        self.sent.lock().unwrap().push(message.clone());
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<Value>> {
        Ok(self.rx.recv().await)
    }

    async fn stop(&mut self) {}
}

struct Recording {
    events: Mutex<Vec<Event>>,
    asked: Mutex<Vec<PermissionRequest>>,
    replies: Mutex<VecDeque<PermissionReply>>,
    answers: Mutex<VecDeque<Option<String>>>,
}

impl Recording {
    fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            asked: Mutex::new(Vec::new()),
            replies: Mutex::new(VecDeque::new()),
            answers: Mutex::new(VecDeque::new()),
        }
    }

    fn reply(&self, reply: PermissionReply) {
        self.replies.lock().unwrap().push_back(reply);
    }

    fn answer(&self, answer: &str) {
        self.answers
            .lock()
            .unwrap()
            .push_back(Some(answer.to_string()));
    }

    fn cancel_answer(&self) {
        self.answers.lock().unwrap().push_back(None);
    }
}

#[async_trait]
impl Frontend for Recording {
    fn emit(&self, event: Event) {
        self.events.lock().unwrap().push(event);
    }

    async fn permission(&self, request: PermissionRequest) -> PermissionReply {
        self.asked.lock().unwrap().push(request);
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(PermissionReply::Deny(None))
    }

    async fn question(&self, _: &str, _: &[String]) -> Option<String> {
        self.answers.lock().unwrap().pop_front().unwrap_or_default()
    }

    async fn choose(&self, _: &str, _: &[String]) -> Option<String> {
        self.answers.lock().unwrap().pop_front().unwrap_or_default()
    }
}

fn agent() -> (AcpAgent, UnboundedSender<Value>, Arc<Mutex<Vec<Value>>>) {
    let (tx, rx) = unbounded_channel();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let mut agent = AcpAgent::new(vec!["fake-agent".into()]);
    agent.transport = Some(tokio::sync::Mutex::new(Box::new(Fake {
        rx,
        sent: sent.clone(),
    })));
    agent.session = Some(Session {
        id: "s1".into(),
        tools: HashMap::new(),
        text: String::new(),
    });
    (agent, tx, sent)
}

#[tokio::test]
async fn streams_text_tools_and_todos() {
    let (mut agent, tx, sent) = agent();
    let frontend = Recording::new();
    let cwd = std::env::temp_dir();
    for update in [
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hello "}}),
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "world"}}),
        json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Bash", "kind": "execute", "status": "pending", "rawInput": {"command": "ls"}, "content": [{"type": "content", "content": {"type": "text", "text": "ls"}}]}),
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed", "content": [{"type": "content", "content": {"type": "text", "text": "a.txt"}}]}),
        json!({"sessionUpdate": "plan", "entries": [{"content": "Read", "status": "completed", "activeForm": "Reading"}]}),
    ] {
        tx.send(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": update}})).unwrap();
    }
    tx.send(json!({"jsonrpc": "2.0", "id": 1, "result": {"stopReason": "end_turn"}}))
        .unwrap();
    let end = agent
        .run_with(
            &frontend,
            &cwd,
            Mode::Default,
            "do it",
            vec![],
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(done_text("hello world".into()), "hello world");
    assert!(matches!(end, TurnEnd::Done(ref t) if t == "hello world"));
    let sent = sent.lock().unwrap();
    let prompt = &sent[0];
    assert_eq!(prompt["method"], "session/prompt");
    assert_eq!(prompt["params"]["prompt"][0]["text"], "do it");
    let events = frontend.events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Text(t) if t == "hello "))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Text(t) if t == "world"))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ToolStart { id, .. } if id == "t1"))
    );
    assert!(events.iter().any(|e| matches!(e, Event::ToolEnd { id, content, is_error: false, .. } if id == "t1" && content == "a.txt")));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Todos(t) if t.len() == 1 && t[0].content == "Read"))
    );
}

#[tokio::test]
async fn answers_permission_from_the_frontend() {
    let (mut agent, tx, sent) = agent();
    let frontend = Recording::new();
    frontend.reply(PermissionReply::Allow);
    let cwd = std::env::temp_dir();
    tx.send(json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "session/request_permission",
        "params": {
            "sessionId": "s1",
            "toolCall": {"toolCallId": "t1", "title": "Run tests", "kind": "execute", "rawInput": {"command": "cargo test"}},
            "options": [
                {"optionId": "allow", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "always", "name": "Always", "kind": "allow_always"},
                {"optionId": "no", "name": "No", "kind": "reject_once"}
            ]
        }
    }))
    .unwrap();
    tx.send(json!({"jsonrpc": "2.0", "id": 1, "result": {"stopReason": "end_turn"}}))
        .unwrap();
    agent
        .run_with(
            &frontend,
            &cwd,
            Mode::Default,
            "run",
            vec![],
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let asked = frontend.asked.lock().unwrap();
    assert_eq!(asked[0].label, "Run tests");
    assert!(asked[0].detail.contains("cargo test"));
    let sent = sent.lock().unwrap();
    let response = sent
        .iter()
        .find(|m| m["id"] == 7 && m.get("method").is_none())
        .unwrap();
    assert_eq!(response["result"]["outcome"]["optionId"], "allow");
}

#[tokio::test]
async fn reads_and_writes_files_relative_to_the_session() {
    let dir =
        std::env::temp_dir().join(format!("chanto-acp-test-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("in.txt"), "one\ntwo\nthree\n").unwrap();
    let (mut agent, tx, sent) = agent();
    let frontend = Recording::new();
    tx.send(json!({"jsonrpc": "2.0", "id": 10, "method": "fs/read_text_file", "params": {"path": "in.txt", "line": 2, "limit": 1}})).unwrap();
    tx.send(json!({"jsonrpc": "2.0", "id": 11, "method": "fs/write_text_file", "params": {"path": "out/out.txt", "content": "written"}})).unwrap();
    tx.send(json!({"jsonrpc": "2.0", "id": 1, "result": {"stopReason": "end_turn"}}))
        .unwrap();
    agent
        .run_with(
            &frontend,
            &dir,
            Mode::Default,
            "files",
            vec![],
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let sent = sent.lock().unwrap();
    let read = sent.iter().find(|m| m["id"] == 10).unwrap();
    assert_eq!(read["result"]["content"], "two");
    assert_eq!(
        std::fs::read_to_string(dir.join("out/out.txt")).unwrap(),
        "written"
    );
    drop(sent);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn cancel_sends_session_cancel_and_stops_the_turn() {
    let (mut agent, tx, sent) = agent();
    let frontend = Recording::new();
    let cwd = std::env::temp_dir();
    let cancel = CancellationToken::new();
    let controller = async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel.cancel();
        tokio::time::sleep(Duration::from_millis(10)).await;
        tx.send(json!({"jsonrpc": "2.0", "id": 1, "result": {"stopReason": "cancelled"}}))
            .unwrap();
    };
    let (end, ()) = tokio::join!(
        agent.run_with(
            &frontend,
            &cwd,
            Mode::Default,
            "stop",
            vec![],
            &[],
            cancel.clone()
        ),
        controller
    );
    assert!(matches!(end.unwrap(), TurnEnd::Interrupted));
    let sent = sent.lock().unwrap();
    assert!(
        sent.iter()
            .any(|m| m["method"] == "session/cancel" && m["params"]["sessionId"] == "s1")
    );
}

#[test]
fn prompts_carry_image_mime_types() {
    // A one-pixel PNG header is enough to recognise the type.
    let png = base64::engine::general_purpose::STANDARD
        .encode([0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let blocks = prompt_blocks("look", &[png]);
    assert_eq!(blocks[1]["mimeType"], "image/png");
    assert_eq!(blocks[1]["type"], "image");
}

#[test]
fn content_and_diffs_are_rendered() {
    let content = json!([
        {"type": "content", "content": {"type": "text", "text": "ran"}},
        {"type": "diff", "path": "a.rs", "oldText": "old", "newText": "new"}
    ]);
    let text = render_content(&content);
    assert!(text.contains("ran"));
    assert!(text.contains("a.rs"));
    assert!(text.contains("new"));
}

/// The transport end to end: spawn a process, initialize it, open a
/// session and stream one turn back through the frontend.
#[cfg(unix)]
#[tokio::test]
async fn drives_a_process_over_stdio() {
    use std::os::unix::fs::PermissionsExt;

    let dir =
        std::env::temp_dir().join(format!("chanto-acp-e2e-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("agent.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"Fake agent","version":"1.0"},"authMethods":[]}}' ;;
    *'"method":"session/new"'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s9"}}' ;;
    *'"method":"session/prompt"'*)
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s9","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"from the agent"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}' ;;
  esac
done
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut agent = AcpAgent::new(vec![script.display().to_string()]);
    let frontend = Recording::new();
    let end = agent
        .run_with(
            &frontend,
            &dir,
            Mode::Default,
            "hello",
            vec![],
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(end, TurnEnd::Done(ref t) if t == "from the agent"));
    assert!(
        frontend
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, Event::Text(t) if t == "from the agent"))
    );
    assert!(
        frontend
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, Event::Info(t) if t.contains("Fake agent")))
    );
    agent.reset();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn capabilities_decide_how_a_session_is_restored() {
    let caps = AgentCaps::from_init(&json!({
        "agentCapabilities": {
            "loadSession": true,
            "mcpCapabilities": {"http": true}
        }
    }));
    assert_eq!(caps.restore_method(), Some("session/load"));
    assert!(caps.mcp_http);
    assert!(!caps.mcp_sse);
    assert!(!caps.close);

    let caps = AgentCaps::from_init(&json!({
        "agentCapabilities": {
            "loadSession": true,
            "sessionCapabilities": {"resume": {}, "close": {}}
        }
    }));
    assert_eq!(caps.restore_method(), Some("session/resume"));
    assert!(caps.close);

    let caps = AgentCaps::from_init(&json!({}));
    assert_eq!(caps.restore_method(), None);
}

#[test]
fn configured_mcp_servers_become_acp_entries() {
    let mut config = BTreeMap::new();
    config.insert(
        "stdio".to_string(),
        crate::config::McpServerConfig {
            command: "echo".into(),
            args: vec!["hi".into()],
            env: [("TOKEN".to_string(), "secret".to_string())].into(),
            ..Default::default()
        },
    );
    config.insert(
        "remote".to_string(),
        crate::config::McpServerConfig {
            url: Some("https://example.com/mcp".into()),
            headers: [("Authorization".to_string(), "Bearer x".to_string())].into(),
            ..Default::default()
        },
    );
    config.insert(
        "legacy".to_string(),
        crate::config::McpServerConfig {
            url: Some("https://example.com/sse".into()),
            transport: Some("sse".into()),
            ..Default::default()
        },
    );
    config.insert(
        "off".to_string(),
        crate::config::McpServerConfig {
            command: "never".into(),
            disabled: true,
            ..Default::default()
        },
    );
    let (servers, errors) = mcp_servers(&config);
    assert!(errors.is_empty());
    assert_eq!(servers.len(), 3);
    let stdio = servers.iter().find(|s| s["name"] == "stdio").unwrap();
    assert_eq!(stdio["args"][0], "hi");
    assert_eq!(stdio["env"][0]["name"], "TOKEN");
    assert!(stdio.get("type").is_none());
    let remote = servers.iter().find(|s| s["name"] == "remote").unwrap();
    assert_eq!(remote["type"], "http");
    assert_eq!(remote["headers"][0]["value"], "Bearer x");
    let legacy = servers.iter().find(|s| s["name"] == "legacy").unwrap();
    assert_eq!(legacy["type"], "sse");
}

#[test]
fn replayed_session_updates_are_not_shown_twice() {
    let (mut agent, _, _) = agent();
    let frontend = Recording::new();
    agent.replaying = true;
    agent.apply_update(
        &frontend,
        &json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "old"}}),
    );
    assert!(frontend.events.lock().unwrap().is_empty());
    agent.replaying = false;
    agent.apply_update(
        &frontend,
        &json!({"sessionUpdate": "available_commands_update", "availableCommands": [{"name": "review"}, {"name": "test"}]}),
    );
    assert!(frontend.events.lock().unwrap().iter().any(
        |e| matches!(e, Event::Info(text) if text.contains("/review") && text.contains("/test"))
    ));
}

#[tokio::test]
async fn asks_the_user_for_form_input() {
    let frontend = Recording::new();
    // Properties are offered in name order: dry_run, environment, project.
    frontend.answer("Yes");
    frontend.answer("Staging");
    frontend.answer("acme");
    let request = json!({
        "mode": "form",
        "sessionId": "s1",
        "message": "Deployment details",
        "requestedSchema": {
            "type": "object",
            "properties": {
                "project": {"type": "string", "title": "Project"},
                "dry_run": {"type": "boolean", "description": "Dry run?"},
                "environment": {"type": "string", "oneOf": [
                    {"const": "staging", "title": "Staging"},
                    {"const": "production", "title": "Production"}
                ]}
            },
            "required": ["project", "environment"]
        }
    });
    let result = elicitation(&frontend, &request, &CancellationToken::new()).await;
    assert_eq!(result["action"], "accept");
    assert_eq!(result["content"]["project"], "acme");
    assert_eq!(result["content"]["dry_run"], true);
    assert_eq!(result["content"]["environment"], "staging");
}

#[tokio::test]
async fn cancelling_a_form_declines_it() {
    let frontend = Recording::new();
    frontend.cancel_answer();
    let request = json!({
        "mode": "form",
        "sessionId": "s1",
        "message": "Need a name",
        "requestedSchema": {
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "required": ["name"]
        }
    });
    let result = elicitation(&frontend, &request, &CancellationToken::new()).await;
    assert_eq!(result["action"], "cancel");
}

#[cfg(unix)]
#[tokio::test]
async fn runs_terminal_commands_for_the_agent() {
    let (mut agent, tx, sent) = agent();
    let frontend = Recording::new();
    tx.send(json!({
        "jsonrpc": "2.0",
        "id": 20,
        "method": "terminal/create",
        "params": {
            "sessionId": "s1",
            "command": "/bin/sh",
            "args": ["-c", "printf 'hello\\n'"],
            "outputByteLimit": 1024
        }
    }))
    .unwrap();
    tx.send(json!({
        "jsonrpc": "2.0",
        "id": 21,
        "method": "terminal/wait_for_exit",
        "params": {"sessionId": "s1", "terminalId": "term_1"}
    }))
    .unwrap();
    tx.send(json!({
        "jsonrpc": "2.0",
        "id": 22,
        "method": "terminal/output",
        "params": {"sessionId": "s1", "terminalId": "term_1"}
    }))
    .unwrap();
    tx.send(json!({"jsonrpc": "2.0", "id": 1, "result": {"stopReason": "end_turn"}}))
        .unwrap();
    agent
        .run_with(
            &frontend,
            &std::env::temp_dir(),
            Mode::Default,
            "run",
            vec![],
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let sent = sent.lock().unwrap();
    let created = sent.iter().find(|m| m["id"] == 20).unwrap();
    assert_eq!(created["result"]["terminalId"], "term_1");
    let waited = sent.iter().find(|m| m["id"] == 21).unwrap();
    assert_eq!(waited["result"]["exitCode"], 0);
    let output = sent.iter().find(|m| m["id"] == 22).unwrap();
    assert_eq!(output["result"]["output"], "hello\n");
    assert_eq!(output["result"]["truncated"], false);
    drop(sent);
    agent.reset();
}

#[test]
fn learns_modes_and_initial_settings() {
    let (mut agent, _tx, _sent) = agent();
    let frontend = Recording::new();
    agent.learn_session(
        &frontend,
        &json!({
            "modes": {
                "currentModeId": "ask",
                "availableModes": [
                    {"id": "ask", "name": "Ask"},
                    {"id": "plan", "name": "Plan"}
                ]
            },
            "configOptions": [
                {
                    "id": "model", "name": "Model", "type": "select",
                    "currentValue": "sonnet",
                    "options": [
                        {"value": "sonnet", "name": "Sonnet"},
                        {"value": "opus", "name": "Opus"}
                    ]
                },
                {
                    "id": "thinking", "name": "Thinking", "type": "boolean",
                    "currentValue": true
                }
            ]
        }),
    );
    let modes = agent.modes_summary();
    assert!(modes.contains("Ask **(current)**"), "{modes}");
    assert!(modes.contains("Plan"), "{modes}");
    let settings = agent.settings_summary();
    assert!(settings.contains("Model: **Sonnet**"), "{settings}");
    assert!(settings.contains("Thinking: **on**"), "{settings}");
    assert!(settings.contains("`model` — Model"), "{settings}");
    let events = frontend.events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Info(text) if text.contains("Agent modes")))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Info(text) if text.contains("Model: Sonnet")))
    );
}

#[tokio::test]
async fn changes_modes_and_settings_over_the_protocol() {
    use crate::agent::tests::{make_env, tmpdir};
    let dir = tmpdir("acp-settings");
    let env = make_env("http://127.0.0.1:1", &dir, Mode::Default, 32768);
    let (mut agent, tx, sent) = agent();
    agent.modes = Some(json!({
        "currentModeId": "ask",
        "availableModes": [
            {"id": "ask", "name": "Ask"},
            {"id": "plan", "name": "Plan"}
        ]
    }));
    agent.config_options = vec![
        json!({
            "id": "model", "name": "Model", "type": "select",
            "currentValue": "sonnet",
            "options": [
                {"value": "sonnet", "name": "Sonnet"},
                {"value": "opus", "name": "Opus"}
            ]
        }),
        json!({
            "id": "thinking", "name": "Thinking", "type": "boolean",
            "currentValue": false
        }),
    ];

    tx.send(json!({"jsonrpc": "2.0", "id": 1, "result": {}}))
        .unwrap();
    let message = agent
        .set_mode(&env, "plan", CancellationToken::new())
        .await
        .unwrap();
    assert!(message.contains("Plan"), "{message}");
    assert_eq!(agent.modes.as_ref().unwrap()["currentModeId"], "plan");

    tx.send(json!({
        "jsonrpc": "2.0", "id": 2,
        "result": {"configOptions": [
            {
                "id": "model", "name": "Model", "type": "select",
                "currentValue": "opus",
                "options": [
                    {"value": "sonnet", "name": "Sonnet"},
                    {"value": "opus", "name": "Opus"}
                ]
            },
            {
                "id": "thinking", "name": "Thinking", "type": "boolean",
                "currentValue": false
            }
        ]}
    }))
    .unwrap();
    // Accepts the option's name, not just its value id.
    let message = agent
        .set_config_option(&env, "model", Some("Opus"), CancellationToken::new())
        .await
        .unwrap();
    assert!(message.contains("Opus"), "{message}");
    assert_eq!(agent.config_options[0]["currentValue"], "opus");

    tx.send(json!({
        "jsonrpc": "2.0", "id": 3,
        "result": {"configOptions": [
            {
                "id": "model", "name": "Model", "type": "select",
                "currentValue": "opus",
                "options": [
                    {"value": "sonnet", "name": "Sonnet"},
                    {"value": "opus", "name": "Opus"}
                ]
            },
            {
                "id": "thinking", "name": "Thinking", "type": "boolean",
                "currentValue": true
            }
        ]}
    }))
    .unwrap();
    agent
        .set_config_option(&env, "thinking", Some("on"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(agent.config_options[1]["currentValue"], true);

    let sent = sent.lock().unwrap();
    let set_mode = sent
        .iter()
        .find(|message| message["method"] == "session/set_mode")
        .unwrap();
    assert_eq!(set_mode["params"]["modeId"], "plan");
    let set_option = sent.iter().find(|message| message["id"] == 3).unwrap();
    assert_eq!(set_option["params"]["configId"], "thinking");
    assert_eq!(set_option["params"]["type"], "boolean");
    assert_eq!(set_option["params"]["value"], true);
}

#[tokio::test]
async fn signs_in_with_an_advertised_method() {
    use crate::agent::tests::{make_env, tmpdir};
    let dir = tmpdir("acp-login");
    let env = make_env("http://127.0.0.1:1", &dir, Mode::Default, 32768);
    let (mut agent, tx, sent) = agent();
    agent.auth_methods = vec![
        json!({"id": "oauth", "name": "Sign in with GitHub"}),
        json!({"id": "shell", "name": "Run a login command"}),
    ];
    tx.send(json!({"jsonrpc": "2.0", "id": 1, "result": {}}))
        .unwrap();
    let message = agent
        .login(&env, Some("oauth"), CancellationToken::new())
        .await
        .unwrap();
    assert!(message.contains("oauth"), "{message}");
    let sent = sent.lock().unwrap();
    let authenticate = sent
        .iter()
        .find(|message| message["method"] == "authenticate")
        .unwrap();
    assert_eq!(authenticate["params"]["methodId"], "oauth");
}

#[test]
fn adopts_the_saved_agent_session_only_before_one_starts() {
    let (mut saved, _tx, _sent) = agent();
    saved.transport = None;
    saved.adopt(Some("s9"));
    assert_eq!(saved.session_id(), Some("s9"));
    // A live session is already the conversation and stays put.
    let (mut live, _tx, _sent) = agent();
    live.adopt(Some("other"));
    assert_eq!(live.session_id(), None);
}

#[test]
fn remembers_agent_commands_for_passthrough() {
    let (mut agent, _tx, _sent) = agent();
    let frontend = Recording::new();
    agent.apply_update(
        &frontend,
        &json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                {"name": "review", "description": "Review the diff"}
            ]
        }),
    );
    assert!(agent.has_command("review"));
    assert_eq!(
        agent.available_commands(),
        vec![("review".to_string(), "Review the diff".to_string())]
    );
    agent.apply_update(
        &frontend,
        &json!({"sessionUpdate": "available_commands_update", "availableCommands": []}),
    );
    assert!(!agent.has_command("review"));
}
