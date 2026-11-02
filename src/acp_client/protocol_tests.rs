//! What an external agent can send: turn endings, requests for permission,
//! input and terminals, and answers the client has to refuse. The agent is a
//! scripted channel, so everything here is under the test's control.

use super::*;
use crate::agent::tests::tmpdir;
use async_trait::async_trait;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

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

#[derive(Default)]
struct Scripted {
    events: Mutex<Vec<String>>,
    asked: Mutex<Vec<PermissionRequest>>,
    replies: Mutex<VecDeque<PermissionReply>>,
    answers: Mutex<VecDeque<Option<String>>>,
}

impl Scripted {
    fn answer(&self, a: &str) {
        self.answers.lock().unwrap().push_back(Some(a.into()));
    }
    fn dismiss(&self) {
        self.answers.lock().unwrap().push_back(None);
    }
    fn saw(&self, needle: &str) -> bool {
        self.events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.contains(needle))
    }
}

#[async_trait]
impl Frontend for Scripted {
    fn emit(&self, event: Event) {
        self.events.lock().unwrap().push(match event {
            Event::Warn(t) => format!("warn: {t}"),
            Event::Info(t) => format!("info: {t}"),
            other => format!("{other:?}"),
        });
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

type Sent = Arc<Mutex<Vec<Value>>>;

fn agent() -> (AcpAgent, UnboundedSender<Value>, Sent) {
    let (tx, rx) = unbounded_channel();
    let sent: Sent = Default::default();
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

async fn turn(
    agent: &mut AcpAgent,
    frontend: &Scripted,
    cancel: CancellationToken,
) -> Result<TurnEnd> {
    agent
        .run_with(
            frontend,
            &std::env::temp_dir(),
            Mode::Default,
            "go",
            vec![],
            &[],
            cancel,
        )
        .await
}

fn done(end: TurnEnd) -> String {
    match end {
        TurnEnd::Done(text) => text,
        TurnEnd::Interrupted => panic!("interrupted"),
    }
}

#[tokio::test]
async fn each_stop_reason_is_reported() {
    for (reason, warning) in [
        ("max_tokens", Some("token limit")),
        ("refusal", Some("refused the request")),
        ("max_turn_requests", Some("request limit")),
        ("end_turn", None),
        ("something_new", None),
    ] {
        let (mut agent, tx, _) = agent();
        let f = Scripted::default();
        tx.send(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {
            "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "partial"}
        }}})).unwrap();
        tx.send(json!({"id": 1, "result": {"stopReason": reason}}))
            .unwrap();
        let text = done(
            turn(&mut agent, &f, CancellationToken::new())
                .await
                .unwrap(),
        );
        assert_eq!(text, "partial", "{reason}");
        if let Some(w) = warning {
            assert!(f.saw(w), "{reason}: {:?}", f.events.lock().unwrap());
        }
    }
    let (mut a, tx, _) = agent();
    let f = Scripted::default();
    tx.send(json!({"id": 1, "result": {"stopReason": "cancelled"}}))
        .unwrap();
    assert!(matches!(
        turn(&mut a, &f, CancellationToken::new()).await.unwrap(),
        TurnEnd::Interrupted
    ));
}

#[tokio::test]
async fn an_error_answer_or_a_lost_agent_fails_the_turn() {
    let (mut a, tx, _) = agent();
    let f = Scripted::default();
    tx.send(json!({"id": 1, "error": {"code": -1, "message": "model overloaded"}}))
        .unwrap();
    let e = turn(&mut a, &f, CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert_eq!(e.to_string(), "fake-agent: model overloaded");

    let (mut a, tx, _) = agent();
    tx.send(json!({"id": 1, "error": {"code": 7}})).unwrap();
    let e = turn(&mut a, &f, CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert!(e.to_string().contains("\"code\":7"), "{e}");

    // The process died: the next turn must start over, and stderr explains it.
    let (mut a, tx, _) = agent();
    a.stderr_tail
        .lock()
        .unwrap()
        .push_back("panic: out of memory".into());
    drop(tx);
    let e = turn(&mut a, &f, CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert_eq!(
        e.to_string(),
        "fake-agent exited before finishing the turn\npanic: out of memory"
    );
    assert!(a.transport.is_none() && a.session.is_none());
    assert_eq!(a.session_id(), None);
}

#[tokio::test]
async fn cancelling_a_turn_tells_the_agent_and_waits_for_it() {
    let (mut a, tx, sent) = agent();
    let f = Scripted::default();
    let cancel = CancellationToken::new();
    cancel.cancel();
    // Answer only once the cancel has arrived, as an agent would.
    let seen = sent.clone();
    tokio::spawn(async move {
        loop {
            let cancelled = seen
                .lock()
                .unwrap()
                .iter()
                .any(|m| m["method"] == "session/cancel" && m["params"]["sessionId"] == "s1");
            if cancelled {
                let _ = tx.send(json!({"id": 1, "result": {"stopReason": "cancelled"}}));
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    });
    assert!(matches!(
        turn(&mut a, &f, cancel).await.unwrap(),
        TurnEnd::Interrupted
    ));
}

/// Answers a request through `handle` and returns what the agent would read.
async fn handled(a: &mut AcpAgent, sent: &Sent, f: &Scripted, mode: Mode, message: Value) -> Value {
    let cwd = crate::util::canonical(std::env::temp_dir()).unwrap();
    a.handle(f, &cwd, mode, &CancellationToken::new(), &message)
        .await
        .unwrap();
    sent.lock().unwrap().last().cloned().unwrap_or(Value::Null)
}

fn permission_request(options: Value) -> Value {
    json!({"id": 5, "method": "session/request_permission", "params": {
        "sessionId": "s1",
        "toolCall": {"title": "Run tests", "kind": "execute", "rawInput": {"cmd": "ls"}},
        "options": options
    }})
}

#[tokio::test]
async fn permission_replies_pick_the_matching_option() {
    let all = json!([
        {"optionId": "once", "kind": "allow_once"},
        {"optionId": "always", "kind": "allow_always"},
        {"optionId": "no", "kind": "reject_once"}
    ]);
    let cases = [
        (PermissionReply::Allow, all.clone(), json!("once")),
        (PermissionReply::AllowAlways, all.clone(), json!("always")),
        (PermissionReply::Deny(None), all.clone(), json!("no")),
        (
            PermissionReply::AutoDenied("x".into()),
            all.clone(),
            json!("no"),
        ),
        (
            PermissionReply::Allow,
            json!([{"optionId": "always", "kind": "allow_always"}]),
            json!("always"),
        ),
        (
            PermissionReply::AllowAlways,
            json!([{"optionId": "once", "kind": "allow_once"}]),
            json!("once"),
        ),
        (
            PermissionReply::Deny(None),
            json!([{"optionId": "never", "kind": "reject_always"}]),
            json!("never"),
        ),
    ];
    for (reply, options, want) in cases {
        let (mut a, _tx, sent) = agent();
        let f = Scripted::default();
        f.replies.lock().unwrap().push_back(reply);
        let r = handled(
            &mut a,
            &sent,
            &f,
            Mode::Default,
            permission_request(options),
        )
        .await;
        assert_eq!(r["result"]["outcome"]["optionId"], want, "{r}");
        assert_eq!(r["id"], 5);
        let asked = f.asked.lock().unwrap();
        assert_eq!(asked[0].label, "Run tests");
        assert!(
            asked[0].detail.contains("\"cmd\": \"ls\""),
            "{}",
            asked[0].detail
        );
    }

    // Denied with no reject option offered: cancelled, never allowed.
    let (mut a, _tx, sent) = agent();
    let f = Scripted::default();
    let r = handled(
        &mut a,
        &sent,
        &f,
        Mode::Default,
        permission_request(json!([{"optionId": "once", "kind": "allow_once"}])),
    )
    .await;
    assert_eq!(r["result"]["outcome"]["outcome"], "cancelled", "{r}");

    // Bypass answers without asking; edited content can't be passed on.
    let (mut a, _tx, sent) = agent();
    let f = Scripted::default();
    let r = handled(
        &mut a,
        &sent,
        &f,
        Mode::Bypass,
        permission_request(all.clone()),
    )
    .await;
    assert_eq!(r["result"]["outcome"]["optionId"], "always");
    assert!(f.asked.lock().unwrap().is_empty());
    let (mut a, _tx, sent) = agent();
    let f = Scripted::default();
    f.replies
        .lock()
        .unwrap()
        .push_back(PermissionReply::AllowEdited(String::new()));
    let r = handled(&mut a, &sent, &f, Mode::Default, permission_request(all)).await;
    assert_eq!(r["result"]["outcome"]["optionId"], "once");
    assert!(f.saw("Edited content cannot be passed"));
}

#[tokio::test]
async fn elicitation_takes_urls_and_typed_forms() {
    let (mut a, _tx, sent) = agent();
    let ask = |params: Value| json!({"id": 9, "method": "elicitation/create", "params": params});
    let url = |u: &str| ask(json!({"mode": "url", "message": "Sign in", "url": u}));

    let f = Scripted::default();
    let r = handled(&mut a, &sent, &f, Mode::Default, url("")).await;
    assert_eq!(r["result"]["action"], "decline", "no URL to open");
    f.answer("I opened it");
    let r = handled(&mut a, &sent, &f, Mode::Default, url("https://x.test")).await;
    assert_eq!(r["result"]["action"], "accept");
    assert!(f.saw("https://x.test"));
    f.answer("Decline");
    let r = handled(&mut a, &sent, &f, Mode::Default, url("https://x.test")).await;
    assert_eq!(r["result"]["action"], "decline");
    f.dismiss();
    let r = handled(&mut a, &sent, &f, Mode::Default, url("https://x.test")).await;
    assert_eq!(r["result"]["action"], "cancel");
    let r = handled(
        &mut a,
        &sent,
        &f,
        Mode::Default,
        ask(json!({"mode": "other"})),
    )
    .await;
    assert_eq!(r["result"]["action"], "decline");

    let schema = json!({
        "required": ["count"],
        "properties": {
            "color": {"type": "string", "enum": ["red", "blue"]},
            "count": {"type": "integer"},
            "flag": {"type": "boolean"},
            "kind": {"oneOf": [{"const": "a", "title": "Type A"}, {"const": "b"}]},
            "name": {"type": "string"},
            "ratio": {"type": "number"}
        }
    });
    let form = || ask(json!({"mode": "form", "message": "Fill", "requestedSchema": schema}));
    let f = Scripted::default();
    for answer in ["blue", "12", "Yes", "Type A", "", "0.5"] {
        f.answer(answer);
    }
    let r = handled(&mut a, &sent, &f, Mode::Default, form()).await;
    assert_eq!(r["result"]["action"], "accept", "{r}");
    assert_eq!(
        r["result"]["content"],
        json!({"color": "blue", "count": 12, "flag": true, "kind": "a", "ratio": 0.5})
    );

    // A bad number in a required field cancels; in an optional one it's left out.
    let f = Scripted::default();
    for answer in ["red", "many"] {
        f.answer(answer);
    }
    let r = handled(&mut a, &sent, &f, Mode::Default, form()).await;
    assert_eq!(r["result"]["action"], "cancel");
    assert!(f.saw("is not a whole number"));
    let f = Scripted::default();
    for answer in ["red", "1", "No", "b", "n", "lots"] {
        f.answer(answer);
    }
    let r = handled(&mut a, &sent, &f, Mode::Default, form()).await;
    assert_eq!(r["result"]["content"]["flag"], false);
    assert_eq!(r["result"]["content"]["name"], "n");
    assert!(r["result"]["content"].get("ratio").is_none(), "{r}");
    assert!(f.saw("is not a number"));

    // Dismissing at any kind of field cancels the form.
    for prefix in [vec![], vec!["red", "1"], vec!["red", "1", "Yes", "b"]] {
        let f = Scripted::default();
        for answer in prefix {
            f.answer(answer);
        }
        f.dismiss();
        let r = handled(&mut a, &sent, &f, Mode::Default, form()).await;
        assert_eq!(r["result"]["action"], "cancel", "{r}");
    }

    let r = handled(
        &mut a,
        &sent,
        &Scripted::default(),
        Mode::Default,
        ask(json!({"mode": "form", "requestedSchema": {}})),
    )
    .await;
    assert_eq!(r["result"], json!({"action": "accept", "content": {}}));
}

#[tokio::test]
async fn requests_the_client_cannot_serve_are_errors() {
    let (mut a, _tx, sent) = agent();
    let f = Scripted::default();
    for (method, params, needle) in [
        ("made/up", json!({}), "unknown method made/up"),
        ("terminal/output", json!({}), "terminalId"),
        ("terminal/kill", json!({"terminalId": "nope"}), "nope"),
        (
            "terminal/wait_for_exit",
            json!({"terminalId": "nope"}),
            "nope",
        ),
        ("terminal/release", json!({}), "terminalId is required"),
        ("terminal/create", json!({}), "command"),
    ] {
        let r = handled(
            &mut a,
            &sent,
            &f,
            Mode::Default,
            json!({"id": 3, "method": method, "params": params}),
        )
        .await;
        let e = r["error"]["message"].as_str().unwrap_or("");
        assert!(e.contains(needle), "{method}: {r}");
        assert_eq!(r["error"]["code"], -32603);
    }
    // Releasing what's already gone is fine, and notifications get no answer.
    let r = handled(
        &mut a,
        &sent,
        &f,
        Mode::Default,
        json!({"id": 4, "method": "terminal/release", "params": {"terminalId": "nope"}}),
    )
    .await;
    assert_eq!(r["result"], json!({}));
    let before = sent.lock().unwrap().len();
    for method in [
        "made/up",
        "terminal/kill",
        "terminal/release",
        "terminal/output",
    ] {
        handled(
            &mut a,
            &sent,
            &f,
            Mode::Default,
            json!({"method": method, "params": {}}),
        )
        .await;
    }
    assert_eq!(sent.lock().unwrap().len(), before);
}

#[cfg(unix)]
#[tokio::test]
async fn a_terminal_can_be_killed_and_reports_its_signal() {
    let (mut a, _tx, sent) = agent();
    let f = Scripted::default();
    let call = |method: &str, params: Value| json!({"id": 1, "method": method, "params": params});
    let r = handled(
        &mut a,
        &sent,
        &f,
        Mode::Default,
        call(
            "terminal/create",
            json!({"command": "sleep", "args": ["30"], "cwd": "/"}),
        ),
    )
    .await;
    let id = r["result"]["terminalId"].as_str().unwrap().to_string();
    let params = json!({"terminalId": id});
    let r = handled(
        &mut a,
        &sent,
        &f,
        Mode::Default,
        call("terminal/kill", params.clone()),
    )
    .await;
    assert_eq!(r["result"], json!({}));
    let r = handled(
        &mut a,
        &sent,
        &f,
        Mode::Default,
        call("terminal/wait_for_exit", params.clone()),
    )
    .await;
    assert!(
        r["result"]["signal"].as_str().unwrap().starts_with("SIG"),
        "{r}"
    );
    let r = handled(
        &mut a,
        &sent,
        &f,
        Mode::Default,
        call("terminal/release", params.clone()),
    )
    .await;
    assert_eq!(r["result"], json!({}));
    let r = handled(
        &mut a,
        &sent,
        &f,
        Mode::Default,
        call("terminal/output", params),
    )
    .await;
    assert!(r["error"].is_object(), "released: {r}");
}

fn env_for(frontend: Arc<Scripted>) -> Arc<Env> {
    let dir = tmpdir("acp-client");
    Env::new(crate::agent::EnvInit {
        config: crate::config::Config::default(),
        provider: crate::llm::Provider::new(
            crate::llm::ProviderKind::Openai,
            "http://127.0.0.1:9",
            None,
            32768,
        ),
        model: "mock".into(),
        mode: Mode::Default,
        vision: false,
        cwd: dir,
        tools: crate::tools::Registry::builtin(),
        mcp: vec![],
        frontend,
        interactive: false,
    })
}

fn option_select() -> Value {
    json!({"id": "model", "name": "Model", "type": "select", "currentValue": "o",
    "options": [
        {"group": "g", "name": "Best", "options": [{"value": "o", "name": "Opus"}]},
        {"value": "s", "name": "Sonnet"},
        {"value": 1}
    ]})
}

#[tokio::test]
async fn settings_and_modes_refuse_what_the_agent_did_not_offer() {
    let (mut a, tx, sent) = agent();
    let f = Arc::new(Scripted::default());
    let env = env_for(f.clone());
    let c = CancellationToken::new;

    assert_eq!(a.modes_summary(), "The agent has not advertised any modes.");
    assert_eq!(
        a.settings_summary(),
        "The agent has not advertised any settings."
    );
    let e = a
        .set_config_option(&env, "model", None, c())
        .await
        .unwrap_err();
    assert_eq!(e.to_string(), "The agent has not advertised any settings.");

    a.config_options = vec![
        option_select(),
        json!({"id": "think", "name": "Think", "type": "boolean", "currentValue": true}),
    ];
    let e = a
        .set_config_option(&env, "nope", None, c())
        .await
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "The agent has no setting `nope`. Available: model, think."
    );
    let e = a
        .set_config_option(&env, "think", Some("maybe"), c())
        .await
        .unwrap_err();
    assert_eq!(e.to_string(), "`maybe` is not on or off for `think`.");
    let e = a
        .set_config_option(&env, "think", None, c())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("Usage"), "not interactive: {e}");
    let e = a
        .set_config_option(&env, "model", None, c())
        .await
        .unwrap_err();
    assert_eq!(e.to_string(), "Usage: `/agent-config model <o|s>`.");
    let e = a
        .set_config_option(&env, "model", Some("gpt"), c())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("gpt"), "{e}");

    let summary = a.settings_summary();
    assert!(
        summary.contains("`model` — Model: **Best / Opus**"),
        "{summary}"
    );
    assert!(summary.contains("`think` — Think: **on**"), "{summary}");
    assert_eq!(
        config_value_label(&json!({"type": "select", "currentValue": "x"})),
        "x"
    );

    // An agent's own update replaces the list and is shown.
    a.apply_update(
        f.as_ref(),
        &json!({"sessionUpdate": "config_option_update", "configOptions": [
            {"id": "think", "name": "Think", "type": "boolean", "currentValue": false}
        ]}),
    );
    assert!(f.saw("Agent settings — Think: off."));
    a.apply_update(f.as_ref(), &json!({"sessionUpdate": "user_message_chunk"}));
    a.apply_update(f.as_ref(), &json!({"sessionUpdate": "from_the_future"}));

    a.modes = Some(json!({"currentModeId": "ask", "availableModes": [
        {"id": "ask", "name": "Ask"}, {"id": "code"}
    ]}));
    let e = a.set_mode(&env, "plan", c()).await.unwrap_err();
    assert_eq!(
        e.to_string(),
        "The agent has no mode `plan`. Available: ask, code."
    );
    let shown = a.modes_summary();
    assert!(
        shown.contains("`ask` — Ask **(current)**") && shown.contains("`code` — code\n"),
        "{shown}"
    );

    // An error from the agent leaves the current mode alone.
    tx.send(json!({"id": 1, "error": {"code": 1, "message": "busy"}}))
        .unwrap();
    let e = a.set_mode(&env, "code", c()).await.unwrap_err();
    assert!(format!("{e:#}").contains("busy"), "{e:#}");
    assert_eq!(a.modes.as_ref().unwrap()["currentModeId"], "ask");
    assert!(
        sent.lock()
            .unwrap()
            .iter()
            .any(|m| m["method"] == "session/set_mode")
    );

    assert!(!a.has_command("x"));
    a.commands = vec![
        json!({"name": "x", "description": "d"}),
        json!({"nameless": true}),
    ];
    assert!(a.has_command("x"));
    assert_eq!(a.available_commands(), [("x".to_string(), "d".to_string())]);
}

#[tokio::test]
async fn sign_in_offers_the_agents_methods() {
    let (mut a, tx, sent) = agent();
    let f = Arc::new(Scripted::default());
    let env = env_for(f.clone());
    let c = CancellationToken::new;

    let e = a.login(&env, None, c()).await.unwrap_err();
    assert!(
        e.to_string().contains("does not offer a sign-in method"),
        "{e}"
    );
    a.auth_methods = vec![
        json!({"id": "web", "name": "Browser"}),
        json!({"id": "term", "name": "Terminal", "type": "terminal"}),
    ];
    let e = a.login(&env, Some("nope"), c()).await.unwrap_err();
    assert!(
        e.to_string()
            .starts_with("The agent has no sign-in method `nope`."),
        "{e}"
    );
    // Not interactive: the list, with how to use it.
    let list = a.login(&env, None, c()).await.unwrap();
    assert!(
        list.contains("- `web` — Browser") && list.contains("/agent-login <id>"),
        "{list}"
    );

    tx.send(json!({"id": 1, "result": {}})).unwrap();
    assert_eq!(
        a.login(&env, Some("web"), c()).await.unwrap(),
        "The agent accepted the `web` sign-in."
    );
    assert_eq!(
        sent.lock().unwrap().last().unwrap()["params"]["methodId"],
        "web"
    );

    let e = a.logout(&env, c()).await.unwrap_err();
    assert_eq!(e.to_string(), "fake-agent does not support logout.");
    a.caps.logout = true;
    tx.send(json!({"id": 2, "result": {}})).unwrap();
    assert_eq!(
        a.logout(&env, c()).await.unwrap(),
        "Signed out of the agent."
    );
    assert_eq!(sent.lock().unwrap().last().unwrap()["method"], "logout");
}

#[test]
fn capabilities_choose_how_to_restore_a_session() {
    let caps = |v: Value| AgentCaps::from_init(&json!({"agentCapabilities": v}));
    assert_eq!(caps(json!({})).restore_method(), None);
    assert_eq!(
        caps(json!({"loadSession": true})).restore_method(),
        Some("session/load")
    );
    let both = caps(json!({
        "loadSession": true,
        "sessionCapabilities": {"resume": {}, "close": {}},
        "mcpCapabilities": {"http": true, "sse": true},
        "auth": {"logout": {}}
    }));
    assert_eq!(
        both.restore_method(),
        Some("session/resume"),
        "no replay is preferred"
    );
    assert!(both.close && both.mcp_http && both.mcp_sse && both.logout);
}

#[test]
fn small_formatters() {
    assert_eq!(signal_name(9), "SIGKILL");
    assert_eq!(signal_name(15), "SIGTERM");
    assert_eq!(signal_name(64), "SIG64");
    for (s, n) in [
        (1, "SIGHUP"),
        (2, "SIGINT"),
        (3, "SIGQUIT"),
        (6, "SIGABRT"),
        (11, "SIGSEGV"),
        (13, "SIGPIPE"),
        (14, "SIGALRM"),
    ] {
        assert_eq!(signal_name(s), n);
    }
    use base64::Engine;
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    assert_eq!(image_mime(&b64(b"\x89PNG....")), "image/png");
    assert_eq!(image_mime(&b64(&[0xFF, 0xD8, 0xFF, 0])), "image/jpeg");
    assert_eq!(image_mime(&b64(b"GIF89a")), "image/gif");
    assert_eq!(image_mime(&b64(b"RIFF....WEBPxx")), "image/webp");
    assert_eq!(image_mime(&b64(b"RIFF")), "image/png", "too short for WebP");
    assert_eq!(image_mime("not base64!"), "image/png");

    assert_eq!(
        render_content(&json!([
            {"type": "content", "content": {"text": "a"}},
            {"type": "diff", "path": "f.rs", "newText": "new"},
            {"type": "text", "text": "b"},
            {"type": "other"},
            {"type": "content", "content": {}}
        ])),
        "a\nf.rs:\nnew\nb"
    );
    assert_eq!(render_content(&json!("nope")), "");
    assert_eq!(permission_detail(&json!({"title": "T"})), "T");
    assert_eq!(permission_detail(&json!({})), "");
    let long = permission_detail(&json!({"content": [{"type": "text", "text": "x".repeat(5000)}]}));
    assert!(
        long.ends_with("\n…") && long.chars().count() == 4002,
        "{}",
        long.len()
    );
    assert_eq!(truncate("héllo".into(), 5), "héllo");
    assert_eq!(truncate("héllo!".into(), 5), "héllo\n…");
    assert!(command_path("/abs/path") == "/abs/path");
    assert_eq!(
        command_path("definitely-not-on-path-xyz"),
        "definitely-not-on-path-xyz"
    );
    assert_eq!(error_text(&json!({"error": {"message": "m"}})), "m");
    assert_eq!(choices(&json!({"enum": [1]})), None);
    assert_eq!(choices(&json!({"oneOf": [{"title": "x"}]})), None);
}
