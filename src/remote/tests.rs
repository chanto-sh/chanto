use super::*;

#[test]
fn link_token_rotates_on_each_start_and_clears_on_stop() {
    let a = fresh_token();
    let b = fresh_token();
    assert_ne!(
        a, b,
        "each start mints a new token, so the old link is revoked"
    );
    assert_eq!(TOKEN.read().unwrap().as_deref(), Some(b.as_str()));
    assert_eq!(stop(), "Remote control is already off.");
    assert!(TOKEN.read().unwrap().is_none(), "stop clears the token");
}

#[test]
fn streamed_text_goes_to_the_cloud_as_one_event() {
    let events = vec![
        json!({"type": "text", "text": "Hel"}),
        json!({"type": "text", "text": "lo"}),
        json!({"type": "tool_start", "id": "t1", "label": "Bash(ls)"}),
        json!({"type": "reasoning", "text": "a"}),
        json!({"type": "reasoning", "text": "b"}),
        json!({"type": "text", "text": "!"}),
    ];
    let merged = merge_text(events);
    assert_eq!(merged.len(), 4);
    assert_eq!(
        (
            merged[0]["text"].as_str(),
            merged[2]["text"].as_str(),
            merged[3]["text"].as_str()
        ),
        (Some("Hello"), Some("ab"), Some("!"))
    );
}

#[tokio::test]
async fn the_first_event_after_a_quiet_spell_is_sent_at_once() {
    let (tx, mut rx) = broadcast::channel(8);
    let closed = CancellationToken::new();
    tx.send(json!({"type": "text", "text": "hi"})).unwrap();
    let batch = tokio::time::timeout(
        Duration::from_millis(200),
        next_batch(&mut rx, &closed, Vec::new(), Some(Instant::now() - GATHER)),
    )
    .await
    .expect("the event waited for the gather window")
    .unwrap();
    assert_eq!(batch.len(), 1);
}

#[tokio::test]
async fn the_replay_is_sent_at_once() {
    let (_tx, mut rx) = broadcast::channel(8);
    let closed = CancellationToken::new();
    let batch = tokio::time::timeout(
        Duration::from_millis(200),
        next_batch(&mut rx, &closed, vec![json!({"type": "info"})], None),
    )
    .await
    .expect("the replay waited for the gather window")
    .unwrap();
    assert_eq!(batch.len(), 1);
}

#[tokio::test]
async fn a_burst_is_gathered_into_one_post() {
    let (tx, mut rx) = broadcast::channel(8);
    let closed = CancellationToken::new();
    let sender = tokio::spawn(async move {
        for text in ["a", "b", "c"] {
            tx.send(json!({"type": "text", "text": text})).unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    let batch = tokio::time::timeout(
        Duration::from_secs(1),
        next_batch(&mut rx, &closed, Vec::new(), Some(Instant::now())),
    )
    .await
    .expect("the burst wasn't gathered")
    .unwrap();
    assert_eq!(batch.len(), 3);
    sender.await.unwrap();
}

#[tokio::test]
async fn ending_remote_control_stops_the_upload() {
    let (_tx, mut rx) = broadcast::channel::<Value>(1);
    let closed = CancellationToken::new();
    closed.cancel();
    assert!(
        next_batch(&mut rx, &closed, Vec::new(), None)
            .await
            .is_none()
    );
}

#[derive(Default)]
struct FakeHost {
    inputs: Mutex<Vec<Input>>,
    /// None: "a turn holds the session".
    messages: Mutex<Option<Vec<Value>>>,
}

impl Host for FakeHost {
    fn send(&self, input: Input) -> bool {
        self.inputs.lock().unwrap().push(input);
        true
    }
    fn snapshot(&self) -> Option<Vec<Value>> {
        self.messages.lock().unwrap().clone()
    }
    fn info(&self) -> Value {
        json!({"model": "mock", "mode": "default", "todos": []})
    }
}

/// A terminal whose dialogs stay open until the test answers them.
#[derive(Default)]
struct Terminal {
    asks: Mutex<Vec<oneshot::Sender<PermissionReply>>>,
    questions: Mutex<Vec<oneshot::Sender<Option<String>>>>,
    events: Mutex<Vec<Value>>,
}

#[async_trait]
impl Frontend for Terminal {
    fn emit(&self, ev: Event) {
        if let Some(v) = event_json(&ev) {
            self.events.lock().unwrap().push(v);
        }
    }
    async fn permission(&self, _: PermissionRequest) -> PermissionReply {
        let (tx, rx) = oneshot::channel();
        self.asks.lock().unwrap().push(tx);
        rx.await.unwrap_or(PermissionReply::Deny(None))
    }
    async fn question(&self, _: &str, _: &[String]) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        self.questions.lock().unwrap().push(tx);
        rx.await.ok().flatten()
    }
}

fn req() -> PermissionRequest {
    PermissionRequest {
        tool: "Bash".into(),
        label: "Bash(rm -rf build)".into(),
        detail: "rm -rf build".into(),
        always_label: "rm commands".into(),
        reason: None,
        proposed: None,
        auto_deny: None,
    }
}

fn setup() -> (Arc<FakeHost>, Arc<Hub>, Arc<Terminal>, Mirror) {
    let host = Arc::new(FakeHost::default());
    *host.messages.lock().unwrap() = Some(vec![json!({"role": "user", "content": "hi"})]);
    let hub = Hub::new(SESSION_ID, "secret", host.clone(), None);
    let term = Arc::new(Terminal::default());
    let slot: Slot = Arc::new(RwLock::new(Some(hub.clone())));
    let mirror = Mirror::new(term.clone(), slot);
    (host, hub, term, mirror)
}

#[test]
fn phone_prompts_user_events_and_snapshots_mask_secrets() {
    let (host, hub, _, _) = setup();
    // Token-shaped, assembled at runtime so the source holds no such literal.
    let secret = &format!("{}_{}", "ghp", "abcdefghijklmnopqrstuvwxyz0123456789");
    let secret = secret.as_str();
    *host.messages.lock().unwrap() = Some(vec![json!({"content": secret})]);
    assert!(!hub.snapshot().to_string().contains(secret));
    hub.user(secret, false);
    let (_id, _answer) = hub.open(json!({"type": "permission_request", "detail": secret}));
    let (replay, _) = hub.subscribe();
    assert_eq!(replay.len(), 2);
    for event in replay {
        assert!(!event.to_string().contains(secret));
        assert!(event.to_string().contains("REDACTED"));
    }
}

async fn next_of(rx: &mut broadcast::Receiver<Value>, kind: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let v = rx.recv().await.unwrap();
            if v["type"] == kind {
                return v;
            }
        }
    })
    .await
    .expect("event")
}

async fn until(f: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !f() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("condition");
}

#[test]
fn events_are_mirrored_and_replayed_to_late_phones() {
    let (host, hub, term, mirror) = setup();
    let mut rx = hub.tx.subscribe();
    hub.user("fix the build", false);
    mirror.emit(Event::Text("Look".into()));
    mirror.emit(Event::Text("ing.".into()));
    mirror.emit(Event::ToolStart {
        id: "t1".into(),
        label: "Bash(make)".into(),
        parent: None,
    });
    // The terminal still gets everything.
    assert_eq!(term.events.lock().unwrap().len(), 3);
    // Live: each event as it happens.
    let live: Vec<Value> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert_eq!(
        live.iter()
            .map(|v| v["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["user", "text", "text", "tool_start"]
    );
    // A phone connecting now: one merged text, then the tool.
    let (replay, _) = hub.subscribe();
    assert_eq!(replay.len(), 3);
    assert_eq!(replay[0], json!({"type": "user", "text": "fix the build"}));
    assert_eq!(replay[1]["text"], "Looking.");
    // Mid-turn the snapshot isn't re-read, so snapshot + log stays whole.
    *host.messages.lock().unwrap() = Some(vec![]);
    let snap = hub.snapshot();
    assert_eq!(snap["busy"], true);
    assert_eq!(snap["messages"][0]["content"], "hi");
    assert_eq!(hub.subscribe().0.len(), 3);
    // After the turn it is, and the log starts over.
    hub.turn_done(None);
    *host.messages.lock().unwrap() = Some(vec![
        json!({"role": "user", "content": "fix the build"}),
        json!({"role": "assistant", "content": "Looking."}),
    ]);
    let snap = hub.snapshot();
    assert_eq!(snap["busy"], false);
    assert_eq!(snap["messages"].as_array().unwrap().len(), 2);
    assert!(hub.subscribe().0.is_empty());
    // ...unless the session is still locked: then the log is kept.
    hub.turn_done(Some("boom"));
    *host.messages.lock().unwrap() = None;
    hub.snapshot();
    assert_eq!(
        hub.subscribe().0,
        vec![json!({"type": "turn_done", "error": "boom"})]
    );
    // Quiet prompts mark the turn without showing the text.
    hub.user("<task-notification>", true);
    assert!(hub.is_busy());
    assert_eq!(hub.subscribe().0.len(), 1);
}

#[tokio::test]
async fn phone_answer_wins_and_closes_the_terminal_dialog() {
    let (_host, hub, term, mirror) = setup();
    let mut rx = hub.tx.subscribe();
    let mirror = Arc::new(mirror);
    let m = mirror.clone();
    let ask = tokio::spawn(async move { m.permission(req()).await });
    let ev = next_of(&mut rx, "permission_request").await;
    assert_eq!(ev["label"], "Bash(rm -rf build)");
    let id = ev["request_id"].as_str().unwrap().to_string();
    until(|| !term.asks.lock().unwrap().is_empty()).await;
    // A phone connecting now sees the open prompt.
    assert!(
        hub.subscribe()
            .0
            .iter()
            .any(|v| v["request_id"] == id.as_str())
    );
    assert!(hub.answer(&id, json!({"request_id": id, "decision": "allow_always"})));
    assert_eq!(ask.await.unwrap(), PermissionReply::AllowAlways);
    // The terminal's dialog sees its answer channel closed and goes away.
    assert!(term.asks.lock().unwrap()[0].is_closed());
    assert_eq!(
        next_of(&mut rx, "request_done").await["request_id"],
        id.as_str()
    );
    assert!(hub.pending.lock().unwrap().is_empty());
    assert!(term.events.lock().unwrap().iter().any(|e| {
        e["message"]
            .as_str()
            .is_some_and(|m| m.contains("always allowed"))
    }));
    // Answering again is refused.
    assert!(!hub.answer(&id, json!({"decision": "deny"})));
}

#[tokio::test]
async fn terminal_answer_wins_and_closes_the_phone_dialog() {
    let (_host, hub, term, mirror) = setup();
    let mut rx = hub.tx.subscribe();
    let mirror = Arc::new(mirror);
    let m = mirror.clone();
    let ask = tokio::spawn(async move {
        m.question(
            "Which database?",
            &["Postgres".to_string(), "SQLite".to_string()],
        )
        .await
    });
    let ev = next_of(&mut rx, "question").await;
    assert_eq!(ev["options"][1], "SQLite");
    let id = ev["request_id"].as_str().unwrap().to_string();
    until(|| !term.questions.lock().unwrap().is_empty()).await;
    let tx = term.questions.lock().unwrap().pop().unwrap();
    tx.send(Some("SQLite".into())).unwrap();
    assert_eq!(ask.await.unwrap().as_deref(), Some("SQLite"));
    assert_eq!(
        next_of(&mut rx, "request_done").await["request_id"],
        id.as_str()
    );
    // The phone's late answer finds nothing to answer.
    assert!(!hub.answer(&id, json!({"answer": "Postgres"})));
    assert!(hub.subscribe().0.iter().all(|v| v["type"] != "question"));
}

#[tokio::test]
async fn turning_remote_off_leaves_the_prompt_to_the_terminal() {
    let (_host, hub, term, mirror) = setup();
    let mut rx = hub.tx.subscribe();
    let mirror = Arc::new(mirror);
    let m = mirror.clone();
    let ask = tokio::spawn(async move { m.permission(req()).await });
    next_of(&mut rx, "permission_request").await;
    until(|| !term.asks.lock().unwrap().is_empty()).await;
    hub.close();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!ask.is_finished());
    let tx = term.asks.lock().unwrap().pop().unwrap();
    tx.send(PermissionReply::Deny(Some("not now".into())))
        .unwrap();
    assert_eq!(
        ask.await.unwrap(),
        PermissionReply::Deny(Some("not now".into()))
    );
    // With no hub in the slot, the mirror is the terminal alone.
    let (_host, _hub, term, _) = setup();
    let off = Mirror::new(term.clone(), Arc::new(RwLock::new(None)));
    off.emit(Event::Info("hello".into()));
    assert_eq!(term.events.lock().unwrap().len(), 1);
}

#[test]
fn phone_decisions_map_to_replies() {
    assert_eq!(
        permission_reply(&json!({"decision": "allow"})),
        PermissionReply::Allow
    );
    assert_eq!(
        permission_reply(&json!({"decision": "allow_always"})),
        PermissionReply::AllowAlways
    );
    assert_eq!(
        permission_reply(&json!({"decision": "deny", "feedback": ""})),
        PermissionReply::Deny(None)
    );
    assert_eq!(
        permission_reply(&json!({"decision": "deny", "feedback": "use make"})),
        PermissionReply::Deny(Some("use make".into()))
    );
    assert_eq!(permission_reply(&json!({})), PermissionReply::Deny(None));
}

#[tokio::test]
async fn http_api_routes_phone_input_to_the_terminal() {
    let (host, hub, _term, mirror) = setup();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let closed = hub.closed.clone();
    let app = router(hub.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(closed.cancelled_owned())
            .await
            .unwrap()
    });
    let client = crate::http::client();
    // The page is public; the API needs the token.
    assert_eq!(
        client
            .get(format!("{base}/"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let health: Value = client
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (health["remote"].clone(), health["session"].clone()),
        (json!(true), json!(SESSION_ID))
    );
    for path in [
        "/sessions",
        "/sessions/terminal",
        "/sessions/terminal/events",
        "/voice",
    ] {
        for token in [None, Some("wrong")] {
            let mut r = client.get(format!("{base}{path}"));
            if let Some(t) = token {
                r = r.bearer_auth(t);
            }
            assert_eq!(
                r.send().await.unwrap().status(),
                StatusCode::UNAUTHORIZED,
                "{path}"
            );
        }
    }
    let post = |path: &str, body: Value| {
        client
            .post(format!("{base}{path}"))
            .bearer_auth("secret")
            .json(&body)
            .send()
    };
    assert_eq!(
        post("/sessions/terminal/messages", json!({"text": "hi"}))
            .await
            .unwrap()
            .status(),
        StatusCode::ACCEPTED
    );
    let list: Value = client
        .get(format!("{base}/sessions"))
        .bearer_auth("secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["sessions"][0]["id"], SESSION_ID);
    assert_eq!(
        post("/sessions/terminal/messages", json!({"text": "  "}))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post("/sessions/other/messages", json!({"text": "hi"}))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post("/sessions", json!({})).await.unwrap().status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        post("/sessions/terminal/interrupt", json!({}))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        *host.inputs.lock().unwrap(),
        vec![Input::Prompt("hi".into()), Input::Interrupt]
    );
    // A phone connecting mid-turn gets the snapshot, then the turn so far.
    hub.user("hi", false);
    mirror.emit(Event::Text("Working on it".into()));
    let snap: Value = client
        .get(format!("{base}/sessions/terminal"))
        .bearer_auth("secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (snap["busy"].clone(), snap["messages"][0]["content"].clone()),
        (json!(true), json!("hi"))
    );
    let mut stream = client
        .get(format!("{base}/sessions/terminal/events"))
        .bearer_auth("secret")
        .send()
        .await
        .unwrap();
    let mut seen = String::new();
    while !seen.contains("Working on it") {
        let chunk = tokio::time::timeout(Duration::from_secs(5), stream.chunk())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(seen.contains("\"type\":\"user\""));
    // Answers route to the open prompt; unknown ones are 404.
    let m = Arc::new(mirror);
    let m2 = m.clone();
    let ask = tokio::spawn(async move { m2.permission(req()).await });
    until(|| !hub.pending.lock().unwrap().is_empty()).await;
    let id = hub.pending.lock().unwrap().keys().next().unwrap().clone();
    assert_eq!(
        post(
            "/sessions/terminal/respond",
            json!({"request_id": "req-nope", "decision": "allow"})
        )
        .await
        .unwrap()
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post(
            "/sessions/terminal/respond",
            json!({"request_id": id, "decision": "deny", "feedback": "use make"})
        )
        .await
        .unwrap()
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        ask.await.unwrap(),
        PermissionReply::Deny(Some("use make".into()))
    );
    // Off: the stream ends and the API refuses.
    hub.close();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Ok(Some(_)) = stream.chunk().await {}
    })
    .await
    .expect("stream ends");
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server stops")
        .unwrap();
}

#[tokio::test]
async fn the_page_cloud_mode_serves_locally_comes_from_this_binary() {
    let (_host, hub, _term, _mirror) = setup();
    let base = serve_locally(hub.clone()).await.expect("loopback listener");
    let client = crate::http::client();
    // The same page `chanto web` serves, with the headers that keep it
    // from leaking the link: no referrer, no caching.
    let page = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(page.headers()["referrer-policy"], "no-referrer");
    assert_eq!(page.headers()["cache-control"], "no-store");
    assert!(page.text().await.unwrap().contains("/app.js"));
    // The API behind it still needs this session's token.
    assert_eq!(
        client
            .get(format!("{base}/sessions"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let list: Value = client
        .get(format!("{base}/sessions"))
        .bearer_auth("secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["sessions"][0]["id"], SESSION_ID);
    // Turning remote control off stops it too.
    hub.close();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if client.get(format!("{base}/health")).send().await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the local page stops");
}
