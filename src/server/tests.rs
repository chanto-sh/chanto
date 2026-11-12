use super::*;
use crate::agent::EnvInit;
use crate::agent::tests::{mock_server, tc};
use crate::llm::{Provider, ProviderKind};
use crate::tools::{Ctx, Registry, Tool};

#[test]
fn private_server_leaves_the_users_server_file_alone() {
    let dir = std::env::temp_dir().join(format!("chanto-server-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("server.json");
    std::fs::write(&path, "the user's server").unwrap();

    write_server_file(&path, &json!({"url": "private"}), true).unwrap();
    remove_server_file(&path, true);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "the user's server");

    // A normal server replaces the file, then removes it on shutdown.
    let normal = json!({"url": "normal", "pid": std::process::id()});
    write_server_file(&path, &normal, false).unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(written["url"], "normal");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    remove_server_file(&path, false);
    assert!(!path.exists());

    // One another server wrote since (the daemon) stays.
    let daemon = json!({"url": "daemon", "pid": std::process::id() + 1});
    write_server_file(&path, &daemon, false).unwrap();
    remove_server_file(&path, false);
    assert!(path.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn only_one_server_hosts_the_runs_store() {
    let dir = std::env::temp_dir().join(format!("chanto-host-lock-{}", uuid::Uuid::new_v4()));
    let store = crate::runs::Store::new(dir.clone());
    assert!(runs_lock(&store, false, false).unwrap().is_none());
    assert!(!store.hosted_elsewhere());

    let first = runs_lock(&store, true, false).unwrap();
    assert!(first.is_some());
    assert!(store.hosted_elsewhere());
    // A plain loopback server serves without runs; one started with --runs
    // refuses to start.
    assert!(runs_lock(&store, true, false).unwrap().is_none());
    let e = runs_lock(&store, true, true).unwrap_err().to_string();
    assert!(e.contains("already hosts background runs"), "{e}");

    // Released with the first host: the next one takes over. On Linux a
    // child process another test forks at this moment shares the lock file
    // until it execs, so give the release a moment to be seen.
    drop(first);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while store.hosted_elsewhere() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(!store.hosted_elsewhere());
    assert!(runs_lock(&store, true, true).unwrap().is_some());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn request_events_and_replay_mask_secrets() {
    let (tx, mut rx) = broadcast::channel(8);
    let frontend = ServerFrontend {
        tx,
        pending: Default::default(),
    };
    // Token-shaped, assembled at runtime so the source holds no such literal.
    let secret = &format!("{}_{}", "ghp", "abcdefghijklmnopqrstuvwxyz0123456789");
    let secret = secret.as_str();
    for kind in ["permission_request", "question"] {
        let ask = frontend
            .ask(|request_id| json!({"type": kind, "request_id": request_id, "detail": secret}));
        tokio::pin!(ask);
        tokio::select! {
            _ = &mut ask => panic!("request should wait for an answer"),
            ev = rx.recv() => {
                let ev = ev.unwrap();
                assert!(!ev.to_string().contains(secret));
                assert!(ev.to_string().contains("REDACTED"));
                let pending = frontend.pending.lock().unwrap();
                assert!(!pending.values().next().unwrap().1.to_string().contains(secret));
            }
        }
    }
}

#[tokio::test]
async fn permission_requests_and_questions_reach_the_client_and_come_back() {
    let (tx, mut rx) = broadcast::channel(16);
    let frontend = Arc::new(ServerFrontend {
        tx,
        pending: Default::default(),
    });
    // The client: answer each request by its id, as POST /respond does.
    let answer = |frontend: Arc<ServerFrontend>, reply: Value| async move {
        let ev = rx_next(&frontend).await;
        let id = ev["request_id"].as_str().unwrap().to_string();
        let (sender, _) = frontend.pending.lock().unwrap().remove(&id).unwrap();
        sender.send(reply).unwrap();
        ev
    };
    async fn rx_next(frontend: &ServerFrontend) -> Value {
        loop {
            if let Some((_, (_, ev))) = frontend.pending.lock().unwrap().iter().next() {
                return ev.clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    let req = PermissionRequest {
        tool: "Bash".into(),
        label: "Bash(ls)".into(),
        detail: "ls".into(),
        always_label: "ls".into(),
        reason: Some("lists files".into()),
        proposed: None,
        auto_deny: None,
    };
    let client = tokio::spawn(answer(
        frontend.clone(),
        json!({"decision": "allow_always"}),
    ));
    assert_eq!(frontend.permission(req).await, PermissionReply::AllowAlways);
    let ev = client.await.unwrap();
    assert_eq!(ev["type"], "permission_request");
    assert_eq!(ev["tool"], "Bash");
    assert_eq!(ev["reason"], "lists files");
    assert_eq!(ev["always_label"], "ls");
    assert_eq!(rx.recv().await.unwrap()["type"], "permission_request");

    let options = vec!["red".to_string(), "blue".to_string()];
    let client = tokio::spawn(answer(frontend.clone(), json!({"answer": "blue"})));
    assert_eq!(
        frontend.question("Which?", &options).await.as_deref(),
        Some("blue")
    );
    let ev = client.await.unwrap();
    assert_eq!(ev["type"], "question");
    assert_eq!(ev["options"], json!(["red", "blue"]));
}

#[test]
fn the_documented_session_body_passes_validation() {
    // `CreateSession` is what sdk/schema.json and the SDKs' types say the
    // route takes: every field of it, filled in, has to be accepted.
    let body = crate::wire::CreateSession {
        model: Some("m".into()),
        mode: Some(crate::wire::Mode::AcceptEdits),
        resume: Some("abc".into()),
        cwd: Some("/w".into()),
        system_prompt_append: Some("".into()),
        allowed_tools: Some(vec!["Read".into()]),
        disallowed_tools: Some(vec!["Bash".into()]),
        max_turns: Some(3),
        verify: Some(true),
        tools: Some(vec![crate::wire::ToolSpec {
            name: "lookup".into(),
            description: "Look something up.".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }]),
    };
    let body = serde_json::to_value(body).unwrap();
    assert_eq!(validate_options(&body), Ok(()), "{body}");
}

#[test]
fn bearer_scheme_is_case_insensitive() {
    for h in [
        "Bearer abc",
        "bearer abc",
        "BEARER abc",
        "Bearer  abc ",
        " bEaReR abc",
    ] {
        assert_eq!(bearer_token(h), Some("abc"), "{h:?}");
    }
    for h in ["Basic abc", "Bearer", "Bearer ", "Bearerabc", "abc"] {
        assert_eq!(bearer_token(h), None, "{h:?}");
    }
}

#[test]
fn server_token_prefers_env_then_reads_file() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let path = std::env::temp_dir().join(format!("chanto-server-token-{}", std::process::id()));
    std::fs::write(&path, "file-token\n").unwrap();
    let prev_t = std::env::var_os("CHANTO_SERVER_TOKEN");
    let prev_f = std::env::var_os("CHANTO_SERVER_TOKEN_FILE");
    unsafe {
        std::env::remove_var("CHANTO_SERVER_TOKEN");
        std::env::set_var("CHANTO_SERVER_TOKEN_FILE", &path);
    }
    // Trailing whitespace from the file is trimmed.
    assert_eq!(server_token().as_deref(), Some("file-token"));
    unsafe { std::env::set_var("CHANTO_SERVER_TOKEN", "env-token") };
    assert_eq!(server_token().as_deref(), Some("env-token"));
    unsafe {
        match prev_t {
            Some(v) => std::env::set_var("CHANTO_SERVER_TOKEN", v),
            None => std::env::remove_var("CHANTO_SERVER_TOKEN"),
        }
        match prev_f {
            Some(v) => std::env::set_var("CHANTO_SERVER_TOKEN_FILE", v),
            None => std::env::remove_var("CHANTO_SERVER_TOKEN_FILE"),
        }
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn tailscale_ip_is_read_past_cli_noise() {
    // `tailscale ip -4` prints only the address when up.
    assert_eq!(
        parse_first_ipv4("100.101.102.103\n"),
        Some(Ipv4Addr::new(100, 101, 102, 103))
    );
    // Version mismatches are printed with the answer; the address still wins.
    assert_eq!(
        parse_first_ipv4("Warning: client version \"x\" != \"y\"\n100.64.0.5\n"),
        Some(Ipv4Addr::new(100, 64, 0, 5))
    );
    // No address when logged out.
    assert_eq!(
        parse_first_ipv4("Warning: client version \"1.102\" != \"1.98\"\n"),
        None
    );
    assert_eq!(parse_first_ipv4(""), None);
}

#[test]
fn tailscale_states_get_actionable_reasons() {
    assert!(tailscale_reason("NeedsLogin").contains("tailscale up"));
    assert!(tailscale_reason("Stopped").contains("tailscale up"));
    assert!(tailscale_reason("NoState").contains("tailscale up"));
    assert!(tailscale_reason("SomethingElse").contains("SomethingElse"));
}

#[test]
fn artifacts_keep_the_newest_previews() {
    let mut a = Artifacts::default();
    let first = a.insert("one".into());
    for i in 0..Artifacts::CAPACITY {
        a.insert(format!("{i}"));
    }
    assert_eq!(a.pages.len(), Artifacts::CAPACITY);
    assert!(!a.pages.contains_key(&first));
    let last = a.insert("last".into());
    assert_eq!(a.pages.len(), Artifacts::CAPACITY);
    assert_eq!(a.pages[&last], "last");
}

/// The transcript, checkpoint and queue endpoints show what the web UI
/// shows, so secrets in the history are masked there too.
#[tokio::test]
async fn transcript_checkpoint_and_queue_endpoints_mask_secrets() {
    use crate::llm::{Message, ToolCall};
    let _g = crate::secrets::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = crate::util::canonical(crate::agent::tests::tmpdir("server-mask")).unwrap();
    crate::secrets::init(&crate::config::Config::default(), &dir);
    // Token-shaped, assembled at runtime so the source holds no such literal.
    let secret = &format!("{}_{}", "ghp", "abcdefghijklmnopqrstuvwxyz0123456789");
    let secret = secret.as_str();
    let (tx, _) = broadcast::channel(8);
    let pending: Pending = Default::default();
    let env = Env::new(EnvInit {
        config: crate::config::Config::default(),
        provider: Provider::new(ProviderKind::Openai, "http://127.0.0.1:9", None, 32768),
        model: "mock".into(),
        mode: crate::permissions::Mode::Bypass,
        vision: false,
        cwd: dir,
        tools: Registry::builtin(),
        mcp: vec![],
        frontend: Arc::new(ServerFrontend {
            tx: tx.clone(),
            pending: pending.clone(),
        }),
        interactive: false,
    });
    let mut agent = Agent::new(env.clone());
    let mut call = Message::assistant("");
    call.tool_calls.push(ToolCall {
        id: "c1".into(),
        name: "Bash".into(),
        arguments: json!({"command": format!("gh auth login --with-token {secret}")}),
    });
    agent.messages = vec![
        Message::system("sys"),
        Message::user(format!("use {secret}")),
        call,
    ];
    *env.checkpoints.lock().unwrap() = vec![crate::agent::Checkpoint {
        id: "a".into(),
        timestamp: "t1".into(),
        tree: None,
        msg_index: 1,
        prompt: format!("use {secret}"),
        files: Default::default(),
    }];
    env.inbox
        .lock()
        .unwrap()
        .push(Queued::new(format!("later {secret}")));
    let s = Arc::new(Session {
        options: json!({}),
        agent: Arc::new(AsyncMutex::new(agent)),
        env: env.clone(),
        tx,
        pending,
        cancel: Mutex::new(None),
    });
    let st = AppState {
        cli: Arc::new(<Cli as clap::Parser>::parse_from(["chanto"])),
        token: Arc::new("test".into()),
        sessions: Arc::new(Mutex::new(HashMap::from([("test".into(), s)]))),
        artifacts: Arc::new(Mutex::new(Artifacts::default())),
    };
    let id = || Path("test".to_string());
    for (what, body) in [
        (
            "transcript",
            body_json(get_session(State(st.clone()), id()).await).await,
        ),
        (
            "checkpoints",
            body_json(checkpoints(State(st.clone()), id()).await).await,
        ),
        (
            "queue",
            body_json(get_queue(State(st.clone()), id()).await).await,
        ),
    ] {
        let text = body.to_string();
        assert!(
            !text.contains(secret) && text.contains("[REDACTED:GITHUB_TOKEN]"),
            "{what}: {text}"
        );
    }
}

#[tokio::test]
async fn queued_messages_can_be_listed_edited_and_removed() {
    let dir = crate::util::canonical(crate::agent::tests::tmpdir("server-queue")).unwrap();
    let (tx, _) = broadcast::channel(8);
    let pending: Pending = Default::default();
    let env = Env::new(EnvInit {
        config: crate::config::Config::default(),
        provider: Provider::new(ProviderKind::Openai, "http://127.0.0.1:9", None, 32768),
        model: "mock".into(),
        mode: crate::permissions::Mode::Bypass,
        vision: false,
        cwd: dir,
        tools: Registry::builtin(),
        mcp: vec![],
        frontend: Arc::new(ServerFrontend {
            tx: tx.clone(),
            pending: pending.clone(),
        }),
        interactive: false,
    });
    let s = Arc::new(Session {
        options: json!({}),
        agent: Arc::new(AsyncMutex::new(Agent::new(env.clone()))),
        env: env.clone(),
        tx,
        pending,
        cancel: Mutex::new(None),
    });
    let st = AppState {
        cli: Arc::new(<Cli as clap::Parser>::parse_from(["chanto"])),
        token: Arc::new("test".into()),
        sessions: Arc::new(Mutex::new(HashMap::from([("test".into(), s)]))),
        artifacts: Arc::new(Mutex::new(Artifacts::default())),
    };
    let path = |qid: &str| Path(("test".to_string(), qid.to_string()));
    let id = || Path("test".to_string());
    env.inbox.lock().unwrap().push(Queued::new("first"));
    env.inbox.lock().unwrap().push(Queued::new("second"));
    let queue = body_json(get_queue(State(st.clone()), id()).await).await;
    assert_eq!(queue["queued"][0]["text"], "first");
    assert_eq!(queue["queued"][1]["text"], "second");
    let qid = queue["queued"][0]["id"].as_str().unwrap().to_string();
    let edited = body_json(
        edit_queued(
            State(st.clone()),
            path(&qid),
            Json(json!({"text": "first, edited"})),
        )
        .await,
    )
    .await;
    assert_eq!(edited["queued"][0]["text"], "first, edited");
    assert_eq!(
        edit_queued(State(st.clone()), path(&qid), Json(json!({"text": "  "})),)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        edit_queued(State(st.clone()), path("gone"), Json(json!({"text": "x"})),)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    // Withdrawing is idempotent: taking one the model already has changes nothing.
    let left = body_json(remove_queued(State(st.clone()), path(&qid)).await).await;
    assert_eq!(left["queued"].as_array().unwrap().len(), 1);
    assert_eq!(left["queued"][0]["text"], "second");
    let again = body_json(remove_queued(State(st), path(&qid)).await).await;
    assert_eq!(again["queued"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn immediate_commands_run_while_a_turn_is_running() {
    let dir = crate::util::canonical(crate::agent::tests::tmpdir("server-immediate")).unwrap();
    let (tx, _) = broadcast::channel(8);
    let pending: Pending = Default::default();
    let env = Env::new(EnvInit {
        config: crate::config::Config::default(),
        provider: Provider::new(ProviderKind::Openai, "http://127.0.0.1:9", None, 32768),
        model: "mock".into(),
        mode: crate::permissions::Mode::Bypass,
        vision: false,
        cwd: dir,
        tools: Registry::builtin(),
        mcp: vec![],
        frontend: Arc::new(ServerFrontend {
            tx: tx.clone(),
            pending: pending.clone(),
        }),
        interactive: false,
    });
    let s = Arc::new(Session {
        options: json!({}),
        agent: Arc::new(AsyncMutex::new(Agent::new(env.clone()))),
        env: env.clone(),
        tx,
        pending,
        cancel: Mutex::new(Some(CancellationToken::new())),
    });
    let st = AppState {
        cli: Arc::new(<Cli as clap::Parser>::parse_from(["chanto"])),
        token: Arc::new("test".into()),
        sessions: Arc::new(Mutex::new(HashMap::from([("test".into(), s)]))),
        artifacts: Arc::new(Mutex::new(Artifacts::default())),
    };
    let id = || Path("test".to_string());
    // `/mcp` lists now instead of waiting behind the running turn.
    let out =
        body_json(send_message(State(st.clone()), id(), Json(json!({"text": "/mcp"}))).await).await;
    assert!(out["output"].as_str().unwrap().contains("No MCP servers"));
    assert!(env.inbox.lock().unwrap().is_empty());
    // A prompt still queues for the model's next step.
    let out =
        body_json(send_message(State(st), id(), Json(json!({"text": "keep going"}))).await).await;
    assert_eq!(out["queued"], true);
    assert_eq!(env.inbox.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn web_page_and_api_auth() {
    let cli = <Cli as clap::Parser>::parse_from(["chanto", "web", "--no-open"]);
    let store_dir = std::env::temp_dir().join(format!(
        "chanto-runs-auth-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let runs = crate::runs::manager::Manager::open(
        crate::runs::Store::new(store_dir.clone()),
        crate::runs::manager::Launcher::InProcess,
        1,
    );
    let app = web_router(router(
        cli,
        "secret".into(),
        Default::default(),
        Some(crate::runner::Scheduler::new(runs)),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = crate::http::client();
    let page = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(page.headers()["content-security-policy"], WEB_CSP);
    let html = page.text().await.unwrap();
    assert!(html.contains("/app.js"));
    // The project picker and the mic button; the desktop shell's hooks.
    assert!(html.contains("id=\"open-project\"") && html.contains("id=\"mic\""));
    let js = client
        .get(format!("{base}/app.js"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(js.contains("window.chanto = {") && js.contains("'/voice/transcribe'"));
    for asset in ["app.js", "style.css"] {
        assert_eq!(
            client
                .get(format!("{base}/{asset}"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
    for (method, path) in [
        (reqwest::Method::GET, "/sessions"),
        (reqwest::Method::POST, "/sessions"),
        (reqwest::Method::GET, "/sessions/missing"),
        (reqwest::Method::GET, "/sessions/missing/events"),
        (reqwest::Method::POST, "/sessions/missing/messages"),
        (reqwest::Method::POST, "/sessions/missing/fork"),
        (reqwest::Method::GET, "/sessions/missing/checkpoints"),
        (reqwest::Method::POST, "/sessions/missing/rewind"),
        (reqwest::Method::POST, "/sessions/missing/respond"),
        (reqwest::Method::POST, "/sessions/missing/interrupt"),
        (reqwest::Method::GET, "/sessions/missing/queue"),
        (reqwest::Method::PATCH, "/sessions/missing/queue/q1"),
        (reqwest::Method::DELETE, "/sessions/missing/queue/q1"),
        (reqwest::Method::DELETE, "/sessions/missing"),
        (reqwest::Method::GET, "/runs"),
        (reqwest::Method::POST, "/runs"),
        (reqwest::Method::GET, "/runs/missing"),
        (reqwest::Method::GET, "/runs/missing/events"),
        (reqwest::Method::POST, "/runs/missing/respond"),
        (reqwest::Method::POST, "/runs/missing/stop"),
        (reqwest::Method::POST, "/runs/missing/resume"),
        (reqwest::Method::DELETE, "/runs/missing"),
        (reqwest::Method::GET, "/runs/jobs"),
        (reqwest::Method::POST, "/runs/jobs/due"),
        (reqwest::Method::POST, "/runs/jobs/missing/trigger"),
        (reqwest::Method::GET, "/voice"),
        (reqwest::Method::POST, "/voice/transcribe"),
        (reqwest::Method::POST, "/artifacts"),
    ] {
        for token in [None, Some("wrong")] {
            let mut req = client.request(method.clone(), format!("{base}{path}"));
            if let Some(token) = token {
                req = req.bearer_auth(token);
            }
            assert_eq!(
                req.send().await.unwrap().status(),
                StatusCode::UNAUTHORIZED,
                "{path}"
            );
        }
    }
    assert_eq!(
        client
            .get(format!("{base}/sessions/missing"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    // Voice uploads need the token and a body; unauthenticated audio is never read.
    let audio = client
        .post(format!("{base}/voice/transcribe"))
        .header("content-type", "audio/webm")
        .body(vec![0u8; 64])
        .send()
        .await
        .unwrap();
    assert_eq!(audio.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        client
            .post(format!("{base}/voice/transcribe"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    // The runs API, with the token.
    let health: Value = client
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["runs"], true);
    let list = client
        .get(format!("{base}/runs"))
        .bearer_auth("secret")
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    assert_eq!(list.json::<Value>().await.unwrap()["runs"], json!([]));
    assert_eq!(
        client
            .get(format!("{base}/runs/missing"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let bad = client
        .post(format!("{base}/runs"))
        .bearer_auth("secret")
        .json(&json!({"task": ""}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    // Visual previews: stored with the token, fetched by the frame without
    // one, sandboxed and offline, and gone once the store forgets them.
    let created = client
        .post(format!("{base}/artifacts"))
        .bearer_auth("secret")
        .json(&json!({"html": "<h1>chart</h1><script>document.title='x'</script>"}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::OK);
    let created: Value = created.json().await.unwrap();
    let url = created["url"].as_str().unwrap().to_string();
    assert_eq!(
        url,
        format!("/artifacts/{}", created["id"].as_str().unwrap())
    );
    let piece = client.get(format!("{base}{url}")).send().await.unwrap();
    assert_eq!(piece.status(), StatusCode::OK);
    assert_eq!(piece.headers()["content-security-policy"], ARTIFACT_CSP);
    assert_eq!(piece.headers()["cache-control"], "no-store");
    assert!(piece.text().await.unwrap().contains("<h1>chart</h1>"));
    let empty = client
        .post(format!("{base}/artifacts"))
        .bearer_auth("secret")
        .json(&json!({"html": "  "}))
        .send()
        .await
        .unwrap();
    assert_eq!(empty.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        client
            .get(format!("{base}/artifacts/missing"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    task.abort();
    let _ = std::fs::remove_dir_all(store_dir);
}

/// The app mark is served for the tab icon and the welcome badge.
#[tokio::test]
async fn web_page_serves_the_app_mark() {
    let app = web_router(Router::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let res = reqwest::get(format!("{base}/logo.svg")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-type"], "image/svg+xml");
    assert!(res.text().await.unwrap().contains("<svg"));
    task.abort();
}

/// The web UI's script parses (when node is installed to check it).
#[test]
fn web_app_script_parses() {
    let Some(node) = crate::shell::find_on_path("node") else {
        return;
    };
    let out = std::process::Command::new(node)
        .args([
            "--check",
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/web/app.js"),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn upload_types_map_to_extensions() {
    assert_eq!(audio_ext("audio/webm;codecs=opus"), "webm");
    assert_eq!(audio_ext("audio/mp4"), "m4a");
    assert_eq!(audio_ext("audio/ogg; codecs=opus"), "ogg");
    assert_eq!(audio_ext(""), "wav");
}

#[tokio::test]
async fn uploads_are_transcribed_and_deleted() {
    // The hosted backend against a local mock that records the file name.
    let names = Arc::new(Mutex::new(Vec::<String>::new()));
    let n2 = names.clone();
    let app = Router::new().route(
        "/v1/audio/transcriptions",
        post(move |body: axum::body::Bytes| {
            let names = n2.clone();
            async move {
                let text = String::from_utf8_lossy(&body).to_string();
                names.lock().unwrap().push(
                    text.split("filename=\"")
                        .nth(1)
                        .and_then(|r| r.split('"').next())
                        .unwrap_or("")
                        .to_string(),
                );
                Json(json!({"text": "run the tests"}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let backend = crate::voice::Backend::Hosted(crate::voice::HostedBackend {
        url: format!("{base}/v1/audio/transcriptions"),
        key: crate::voice::Key::None,
        model: "m".into(),
        language: None,
    });
    assert_eq!(
        transcribe_upload(&backend, b"OggS", "audio/ogg")
            .await
            .unwrap(),
        "run the tests"
    );
    assert_eq!(names.lock().unwrap().as_slice(), ["audio.ogg"]);
    // A local backend sees the temp file, which is gone afterwards.
    #[cfg(unix)]
    {
        let dir = std::env::temp_dir().join(format!(
            "chanto-voice-bin-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("whisper-cli");
        std::fs::write(&bin, "#!/bin/sh\ntest -s \"$4\" && echo \"$4\"\n").unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        // A file just written can briefly be "busy" for exec while another
        // test's fork still holds it open (ETXTBSY on Linux).
        for _ in 0..50 {
            match std::process::Command::new(&bin).status() {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(std::time::Duration::from_millis(20))
                }
                _ => break,
            }
        }
        let local = crate::voice::Backend::Local {
            bin,
            model: "m.bin".into(),
            python: false,
            language: None,
        };
        let path = transcribe_upload(&local, b"RIFF....WAVE", "audio/wav")
            .await
            .unwrap();
        let name = std::path::Path::new(&path).file_name().unwrap();
        assert!(name.to_string_lossy().starts_with("chanto-voice-"));
        assert_eq!(
            std::path::Path::new(&path).extension(),
            Some("wav".as_ref())
        );
        assert!(!std::path::Path::new(&path).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
    task.abort();
}

#[test]
fn session_option_validation() {
    for body in [
        json!({"verify": "yes"}),
        json!({"max_turns": 0}),
        json!({"mode": "wrong"}),
        json!({"allowed_tools": "Read"}),
        json!({"tools": [{"name": "read"}]}),
        json!({"cwd": false}),
    ] {
        assert!(validate_options(&body).is_err(), "{body}");
    }
    assert!(
        validate_options(&json!({"verify": false, "allowed_tools": [], "max_turns": 1})).is_ok()
    );
}

async fn body_json(r: Response) -> Value {
    serde_json::from_slice(
        &axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn checkpoints_list_and_rewind() {
    use crate::agent::Checkpoint;
    use crate::llm::Message;
    let dir = crate::util::canonical(crate::agent::tests::tmpdir("server-rewind")).unwrap();
    let file = dir.join("f.txt");
    std::fs::write(&file, "v2\n").unwrap();
    let (tx, _) = broadcast::channel(8);
    let pending: Pending = Default::default();
    let env = Env::new(EnvInit {
        config: crate::config::Config::default(),
        provider: Provider::new(ProviderKind::Openai, "http://127.0.0.1:9", None, 32768),
        model: "mock".into(),
        mode: crate::permissions::Mode::Bypass,
        vision: false,
        cwd: dir.clone(),
        tools: Registry::builtin(),
        mcp: vec![],
        frontend: Arc::new(ServerFrontend {
            tx: tx.clone(),
            pending: pending.clone(),
        }),
        interactive: false,
    });
    let mut agent = Agent::new(env.clone());
    agent.messages = vec![
        Message::system("sys"),
        Message::user("one"),
        Message::assistant("first"),
        Message::user("two"),
        Message::assistant("second"),
    ];
    *env.checkpoints.lock().unwrap() = vec![
        Checkpoint {
            id: "a".into(),
            timestamp: "t1".into(),
            tree: None,
            msg_index: 1,
            prompt: "one".into(),
            files: Default::default(),
        },
        Checkpoint {
            id: "b".into(),
            timestamp: "t2".into(),
            tree: None,
            msg_index: 3,
            prompt: "two".into(),
            files: HashMap::from([(file.clone(), Some(b"v1\n".to_vec()))]),
        },
    ];
    let s = Arc::new(Session {
        options: json!({}),
        agent: Arc::new(AsyncMutex::new(agent)),
        env: env.clone(),
        tx,
        pending,
        cancel: Mutex::new(None),
    });
    let st = AppState {
        cli: Arc::new(<Cli as clap::Parser>::parse_from(["chanto"])),
        token: Arc::new("test".into()),
        sessions: Arc::new(Mutex::new(HashMap::from([("test".into(), s.clone())]))),
        artifacts: Arc::new(Mutex::new(Artifacts::default())),
    };
    let id = || Path("test".to_string());
    let list = body_json(checkpoints(State(st.clone()), id()).await).await;
    assert_eq!(list["checkpoints"][1]["prompt"], "two");
    assert_eq!(
        (
            list["checkpoints"][0]["code"].clone(),
            list["checkpoints"][1]["code"].clone()
        ),
        (json!(false), json!(true))
    );
    assert_eq!(list["checkpoints"][1]["conversation"], true);
    assert_eq!(list["undo"], false);
    // The transcript marks the prompts that start a checkpoint.
    let transcript = body_json(get_session(State(st.clone()), id()).await).await;
    assert_eq!(transcript["messages"][0]["checkpoint"], 0);
    assert!(transcript["messages"][1]["checkpoint"].is_null());
    assert_eq!(transcript["messages"][2]["checkpoint"], 1);
    for bad in [
        json!({}),
        json!({"checkpoint": 1, "mode": "files"}),
        json!({"checkpoint": 5}),
    ] {
        assert_eq!(
            rewind_session(State(st.clone()), id(), Json(bad.clone()))
                .await
                .status(),
            StatusCode::BAD_REQUEST,
            "{bad}"
        );
    }
    // Nothing changes while a turn runs.
    *s.cancel.lock().unwrap() = Some(CancellationToken::new());
    assert_eq!(
        rewind_session(State(st.clone()), id(), Json(json!({"checkpoint": 1})))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        checkpoints(State(st.clone()), id()).await.status(),
        StatusCode::CONFLICT
    );
    *s.cancel.lock().unwrap() = None;
    // A preview changes nothing; without a shadow snapshot there's no summary.
    let preview = body_json(
        rewind_session(
            State(st.clone()),
            id(),
            Json(json!({"checkpoint": 1, "preview": true})),
        )
        .await,
    )
    .await;
    assert!(preview["summary"].is_null());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "v2\n");
    // Code only: the conversation and its checkpoints stay.
    let code = body_json(
        rewind_session(
            State(st.clone()),
            id(),
            Json(json!({"checkpoint": 1, "mode": "code"})),
        )
        .await,
    )
    .await;
    assert_eq!(
        (code["files_restored"].clone(), code["prompt"].clone()),
        (json!(1), json!("two"))
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1\n");
    assert_eq!(code["checkpoints"].as_array().unwrap().len(), 2);
    assert_eq!(s.agent.lock().await.messages.len(), 5);
    // Conversation only: files stay, the prompt and everything after it go.
    std::fs::write(&file, "v3\n").unwrap();
    let conversation = body_json(
        rewind_session(
            State(st.clone()),
            id(),
            Json(json!({"checkpoint": 1, "mode": "conversation"})),
        )
        .await,
    )
    .await;
    assert_eq!(conversation["files_restored"], 0);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "v3\n");
    assert_eq!(conversation["checkpoints"].as_array().unwrap().len(), 1);
    assert_eq!(s.agent.lock().await.messages.len(), 3);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn remote_tool_round_trip() {
    let (base, seen) = mock_server(vec![
        json!({"tool_calls": tc("lookup_ticket", json!({"id": 42}))}),
        json!({"content": "Ticket found."}),
    ])
    .await;
    let (tx, mut rx) = broadcast::channel(32);
    let pending: Pending = Default::default();
    let fe = Arc::new(ServerFrontend {
        tx: tx.clone(),
        pending: pending.clone(),
    });
    let tool = Arc::new(RemoteTool {
        name: "lookup_ticket".into(),
        description: "Find ticket".into(),
        schema: json!({"type": "object", "properties": {"id": {"type": "integer"}}}),
        tx: tx.clone(),
        pending: pending.clone(),
        timeout: std::time::Duration::from_secs(2),
    });
    let mut registry = Registry::builtin();
    registry.tools.push(tool.clone());
    // Its own empty project: the shared temp directory can hold gigabytes
    // that the turn's secret scan and snapshot would otherwise walk.
    let cwd = std::env::temp_dir().join(format!(
        "chanto-remote-tool-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&cwd).unwrap();
    let env = Env::new(EnvInit {
        config: crate::config::Config::default(),
        provider: Provider::new(ProviderKind::Openai, &base, None, 32768),
        model: "mock".into(),
        mode: crate::permissions::Mode::Bypass,
        vision: false,
        cwd: cwd.clone(),
        tools: registry,
        mcp: vec![],
        frontend: fe,
        interactive: false,
    });
    let mut agent = Agent::new(env.clone());
    let s = Arc::new(Session {
        options: json!({}),
        agent: Arc::new(AsyncMutex::new(Agent::new(env.clone()))),
        env: env.clone(),
        tx,
        pending: pending.clone(),
        cancel: Mutex::new(None),
    });
    let st = AppState {
        cli: Arc::new(<Cli as clap::Parser>::parse_from(["chanto"])),
        token: Arc::new("test".into()),
        sessions: Arc::new(Mutex::new(HashMap::from([("test".into(), s)]))),
        artifacts: Arc::new(Mutex::new(Artifacts::default())),
    };
    let client = tokio::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            if ev["type"] == "tool_call" {
                assert_eq!(ev["name"], "lookup_ticket");
                assert_eq!(ev["input"]["id"], 42);
                let response = respond(
                    State(st),
                    Path("test".into()),
                    Json(json!({"id": ev["id"],
                    "result": {"content": "Ticket 42: fix tests", "is_error": false}})),
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK);
                break;
            }
        }
    });
    let end = agent
        .run_turn("look up ticket 42", vec![], CancellationToken::new())
        .await
        .unwrap();
    client.await.unwrap();
    assert!(matches!(end, TurnEnd::Done(_)));
    assert!(
        seen.lock().unwrap()[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool"
                && m["content"]
                    .as_str()
                    .unwrap()
                    .contains("Ticket 42: fix tests"))
    );
    assert!(pending.lock().unwrap().is_empty());

    let cancel = CancellationToken::new();
    cancel.cancel();
    let ctx = Ctx {
        env,
        cancel,
        depth: 0,
        call_id: None,
        cwd_override: None,
        reads: None,
    };
    assert!(tool.run(json!({}), &ctx).await.unwrap().is_error);
    assert!(pending.lock().unwrap().is_empty());
    let short = RemoteTool {
        name: "timeout".into(),
        description: "".into(),
        schema: json!({}),
        tx: tool.tx.clone(),
        pending: pending.clone(),
        timeout: std::time::Duration::from_millis(1),
    };
    let ctx = Ctx {
        cancel: CancellationToken::new(),
        ..ctx
    };
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(1),
            tool.run(json!({}), &ctx)
        )
        .await
        .is_err()
    );
    assert!(pending.lock().unwrap().is_empty());
    assert!(short.run(json!({}), &ctx).await.unwrap().is_error);
    assert!(pending.lock().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&cwd);
}
