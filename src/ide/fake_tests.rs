//! The editor connection against a websocket that speaks the extension's
//! protocol: auth, selection, diagnostics and diffs. No editor runs.

use super::*;
use crate::agent::tests::{make_env_with, tmpdir};
use crate::permissions::Mode;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};

const TOKEN: &str = "right-token";

struct FakeIde {
    port: u16,
    /// Requests received, and the reply to each by method.
    seen: Arc<Mutex<Vec<Value>>>,
    replies: Arc<Mutex<HashMap<String, Value>>>,
    close_now: mpsc::UnboundedSender<()>,
}

/// The handshake callback's signature is tungstenite's.
#[allow(clippy::result_large_err)]
fn check_token(req: &Request, res: Response) -> std::result::Result<Response, ErrorResponse> {
    if req.headers().get(AUTH_HEADER).is_some_and(|t| t == TOKEN) {
        Ok(res)
    } else {
        Err(ErrorResponse::new(Some("bad token".into())))
    }
}

async fn fake_ide() -> FakeIde {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<Value>>> = Default::default();
    let replies: Arc<Mutex<HashMap<String, Value>>> = Default::default();
    let (close_now, mut closing) = mpsc::unbounded_channel::<()>();
    let (s2, r2) = (seen.clone(), replies.clone());
    tokio::spawn(async move {
        let Ok((sock, _)) = listener.accept().await else {
            return;
        };
        let Ok(ws) = tokio_tungstenite::accept_hdr_async(sock, check_token).await else {
            return;
        };
        let (mut sink, mut stream) = ws.split();
        // Noise a client must ignore, then a pushed selection.
        let _ = sink.send(WsMsg::text("not json")).await;
        let _ = sink.send(WsMsg::binary(vec![0u8])).await;
        let _ = sink
            .send(WsMsg::text(
                json!({"method": "selectionChanged", "params": {
                    "file": "/w/pushed.rs", "text": "pushed", "startLine": 1, "endLine": 1
                }})
                .to_string(),
            ))
            .await;
        loop {
            tokio::select! {
                _ = closing.recv() => {
                    let _ = sink.send(WsMsg::Close(None)).await;
                    return;
                }
                m = stream.next() => {
                    let Some(Ok(WsMsg::Text(t))) = m else { return };
                    let req: Value = serde_json::from_str(&t).unwrap();
                    s2.lock().unwrap().push(req.clone());
                    let method = req["method"].as_str().unwrap_or("").to_string();
                    let reply = r2.lock().unwrap().get(&method).cloned();
                    let Some(mut reply) = reply else { continue };
                    reply["id"] = req["id"].clone();
                    let _ = sink.send(WsMsg::text(reply.to_string())).await;
                }
            }
        }
    });
    FakeIde {
        port,
        seen,
        replies,
        close_now,
    }
}

impl FakeIde {
    fn lock(&self) -> LockInfo {
        LockInfo {
            port: self.port,
            pid: std::process::id(),
            workspace_folders: vec![],
            ide_name: "Fake Editor".into(),
            auth_token: TOKEN.into(),
            transport: "ws".into(),
        }
    }

    fn reply(&self, method: &str, result: Value) {
        self.replies
            .lock()
            .unwrap()
            .insert(method.into(), json!({"result": result}));
    }

    fn methods(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|r| r["method"].as_str().unwrap().to_string())
            .collect()
    }

    async fn connect(&self) -> Arc<Ide> {
        Ide::connect(&self.lock()).await.unwrap()
    }
}

fn selection_json(text: &str) -> Value {
    json!({"file": "/w/src/a.rs", "text": text, "startLine": 3, "endLine": 4, "isEmpty": text.is_empty()})
}

async fn until(what: &str, f: impl Fn() -> bool) {
    for _ in 0..600 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("never saw {what}");
}

#[tokio::test]
async fn connecting_checks_transport_and_token() {
    let ide = fake_ide().await;
    let mut lock = ide.lock();
    lock.transport = "stdio".into();
    let e = Ide::connect(&lock).await.err().unwrap();
    assert_eq!(e.to_string(), "unsupported IDE transport stdio");

    let mut lock = ide.lock();
    lock.auth_token = "bad\ntoken".into();
    let e = Ide::connect(&lock).await.err().unwrap();
    assert_eq!(e.to_string(), "bad auth token", "not sent as a header");

    let mut lock = ide.lock();
    lock.auth_token = "wrong".into();
    let e = Ide::connect(&lock).await.err().unwrap();
    assert!(e.to_string().contains("connecting to the IDE"), "{e}");

    let free = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut lock = ide.lock();
    lock.port = free.local_addr().unwrap().port();
    drop(free);
    assert!(Ide::connect(&lock).await.is_err());
}

#[tokio::test]
async fn an_unnamed_editor_is_called_ide() {
    let fake = fake_ide().await;
    let mut lock = fake.lock();
    lock.ide_name.clear();
    lock.transport.clear();
    assert_eq!(Ide::connect(&lock).await.unwrap().name, "IDE");
}

#[tokio::test]
async fn requests_report_errors_timeouts_and_disconnects() {
    let fake = fake_ide().await;
    let ide = fake.connect().await;
    assert!(ide.alive());

    fake.replies.lock().unwrap().insert(
        "boom".into(),
        json!({"error": {"message": "no such thing"}}),
    );
    let short = Duration::from_secs(5);
    let e = ide.request("boom", json!({}), short).await.unwrap_err();
    assert_eq!(e.to_string(), "IDE error: no such thing");
    fake.replies
        .lock()
        .unwrap()
        .insert("opaque".into(), json!({"error": {"code": 1}}));
    let e = ide.request("opaque", json!({}), short).await.unwrap_err();
    assert_eq!(e.to_string(), "IDE error: unknown");

    let e = ide
        .request("silent", json!({}), Duration::from_millis(20))
        .await
        .unwrap_err();
    assert_eq!(e.to_string(), "IDE did not answer in time");
    assert!(
        ide.pending.lock().unwrap().is_empty(),
        "a timeout forgets its request"
    );

    // A diff waits for the user with no time limit; losing the editor must
    // end it instead of hanging the edit forever.
    let diff = {
        let ide = ide.clone();
        tokio::spawn(async move { ide.open_diff(Path::new("/w/a.rs"), "new").await })
    };
    let seen = fake.seen.clone();
    until("openDiff", move || {
        seen.lock()
            .unwrap()
            .iter()
            .any(|r| r["method"] == "openDiff")
    })
    .await;
    fake.close_now.send(()).unwrap();
    let e = diff.await.unwrap().unwrap_err();
    assert_eq!(e.to_string(), "IDE disconnected");
    assert!(!ide.alive());
}

#[tokio::test]
async fn diffs_carry_the_change_and_return_the_users_answer() {
    let fake = fake_ide().await;
    let ide = fake.connect().await;
    fake.reply(
        "openDiff",
        json!({"accepted": true, "contents": "edited by user"}),
    );
    let out = ide
        .open_diff(Path::new("/w/lib.rs"), "proposed")
        .await
        .unwrap();
    assert_eq!(
        out,
        DiffOutcome {
            accepted: true,
            contents: Some("edited by user".into())
        }
    );
    let req = fake.seen.lock().unwrap()[0].clone();
    assert_eq!(req["params"]["newContents"], "proposed");
    assert_eq!(
        req["params"]["title"],
        "lib.rs: current ↔ proposed (chanto)"
    );

    // A malformed answer is a refusal, never an acceptance.
    fake.reply("openDiff", json!({}));
    let out = ide.open_diff(Path::new("/"), "x").await.unwrap();
    assert_eq!(
        out,
        DiffOutcome {
            accepted: false,
            contents: None
        }
    );

    ide.close_diffs(Path::new("/w/lib.rs"));
    let seen = fake.seen.clone();
    until("closeDiffs", move || {
        seen.lock()
            .unwrap()
            .iter()
            .any(|r| r["method"] == "closeDiffs")
    })
    .await;
    assert!(ide.pending.lock().unwrap().is_empty(), "fire and forget");
}

#[tokio::test]
async fn the_selection_falls_back_to_the_last_push() {
    let fake = fake_ide().await;
    let ide = fake.connect().await;
    let held = ide.selection.clone();
    until("the pushed selection", move || {
        held.lock().unwrap().is_some()
    })
    .await;
    // No answer to getSelection: use what the editor last pushed.
    let got = ide.selection().await.unwrap();
    assert_eq!(got.file, "/w/pushed.rs");
    fake.reply("getSelection", selection_json("fresh"));
    assert_eq!(ide.selection().await.unwrap().text, "fresh");
    fake.reply("getSelection", Value::Null);
    assert_eq!(ide.selection().await, None);
}

#[tokio::test]
async fn turn_context_reports_a_selection_once() {
    let dir = tmpdir("ide-turn");
    let env = make_env_with("http://127.0.0.1:9", &dir, Mode::Bypass, 32768, |_| {});
    assert_eq!(turn_context(&env).await, None, "no editor connected");
    let fake = fake_ide().await;
    env.set_ide(Some(fake.connect().await));

    fake.reply("getSelection", selection_json("let x = 1;"));
    let ctx = turn_context(&env).await.unwrap();
    assert!(ctx.contains("selected lines 3 to 4"), "{ctx}");
    assert!(ctx.contains("let x = 1;") && ctx.contains("may or may not be related"));
    assert_eq!(turn_context(&env).await, None, "unchanged selection");

    fake.reply("getSelection", selection_json(""));
    let ctx = turn_context(&env).await.unwrap();
    assert!(ctx.starts_with("The user opened the file"), "{ctx}");
    fake.reply("getSelection", selection_json("   "));
    let ctx = turn_context(&env).await.unwrap();
    assert!(
        ctx.starts_with("The user opened the file"),
        "whitespace only: {ctx}"
    );
}

#[tokio::test]
async fn edits_pick_up_the_editors_errors() {
    let dir = tmpdir("ide-diags");
    let env = make_env_with("http://127.0.0.1:9", &dir, Mode::Bypass, 32768, |_| {});
    let fake = fake_ide().await;
    env.set_ide(Some(fake.connect().await));
    fake.reply(
        "getDiagnostics",
        json!([{"diagnostics": [
            {"severity": "warning", "line": 1, "character": 1, "message": "ignored"},
            {"severity": "error", "line": 2, "character": 5, "message": "bad", "source": "tsc"},
            {"severity": "error", "line": 3, "character": 1, "message": "worse"}
        ]}]),
    );
    let mut out = crate::tools::ToolOutput::ok("wrote");
    let args = json!({"file_path": "a.ts"});
    after_tool(&env, "Write", &args, &mut out).await;
    assert!(
        out.content.contains("source=\"Fake Editor\""),
        "{}",
        out.content
    );
    assert!(
        out.content
            .contains("L2:5 error: bad [tsc]\nL3:1 error: worse"),
        "{}",
        out.content
    );
    assert!(!out.content.contains("ignored"));

    // Nothing is asked for failures, other tools, or files with results already.
    let asked = fake.methods().len();
    for (tool, mut o) in [
        ("Write", crate::tools::ToolOutput::err("failed")),
        ("Read", crate::tools::ToolOutput::ok("x")),
        ("Edit", crate::tools::ToolOutput::ok("<diagnostics>x")),
    ] {
        let before = o.content.clone();
        after_tool(&env, tool, &args, &mut o).await;
        assert_eq!(o.content, before, "{tool}");
    }
    assert_eq!(fake.methods().len(), asked);

    // A patch names its file in the input; one with no files asks nothing.
    let patch = json!({"input": "*** Begin Patch\n*** Add File: b.ts\n+x\n*** End Patch"});
    let mut out = crate::tools::ToolOutput::ok("patched");
    after_tool(&env, "apply_patch", &patch, &mut out).await;
    assert!(out.content.contains("file=\"b.ts\""), "{}", out.content);
    let asked = fake.methods().len();
    let mut out = crate::tools::ToolOutput::ok("patched");
    after_tool(&env, "apply_patch", &json!({"input": ""}), &mut out).await;
    assert_eq!(fake.methods().len(), asked);

    // A clean file adds nothing, nor does a silent editor.
    fake.reply("getDiagnostics", json!([]));
    let mut out = crate::tools::ToolOutput::ok("wrote");
    after_tool(&env, "Write", &args, &mut out).await;
    assert_eq!(out.content, "wrote");
}

#[test]
fn diagnostics_keep_only_twenty_errors() {
    let many: Vec<Value> = (0..30)
        .map(|i| json!({"severity": "error", "line": i, "character": 0, "message": "m"}))
        .collect();
    let text = format_diagnostics(&json!([{"diagnostics": many}])).unwrap();
    assert_eq!(text.lines().count(), 20);
    assert_eq!(format_diagnostics(&json!("junk")), None);
}

/// Lock files live under the config home, so these run in a child process.
#[tokio::test]
async fn the_ide_command_connects_through_lock_files() {
    if crate::test_support::isolated("ide::fake_tests::the_ide_command_connects_through_lock_files")
    {
        return;
    }
    let dir = crate::util::canonical(tmpdir("ide-cmd")).unwrap();
    let env = make_env_with("http://127.0.0.1:9", &dir, Mode::Bypass, 32768, |_| {});

    let status = command(&env, "").await;
    assert!(status.contains("- not connected"), "{status}");
    assert!(!status.contains("Available for this folder"), "{status}");
    let msg = command(&env, "connect").await;
    assert!(msg.starts_with("No running IDE found"), "{msg}");
    assert_eq!(auto_connect(&env).await, None);

    let fake = fake_ide().await;
    std::fs::create_dir_all(lock_dir()).unwrap();
    let write_lock = |port: u16, token: &str| {
        std::fs::write(
            lock_dir().join(format!("{port}.lock")),
            json!({
                "pid": std::process::id(),
                "workspaceFolders": [dir],
                "ideName": "Fake Editor",
                "authToken": token,
                "transport": "ws"
            })
            .to_string(),
        )
        .unwrap();
    };
    write_lock(fake.port, TOKEN);

    let status = command(&env, "status").await;
    assert!(
        status.contains(&format!("- Fake Editor (port {}", fake.port)),
        "{status}"
    );
    let msg = command(&env, "connect 1").await;
    assert!(
        msg.starts_with("No running IDE found"),
        "another port: {msg}"
    );
    let msg = command(&env, &format!("connect {}", fake.port)).await;
    assert_eq!(
        msg,
        format!("Connected to Fake Editor on port {}.", fake.port)
    );
    let status = command(&env, "").await;
    assert!(
        status.contains("- connected to **Fake Editor**"),
        "{status}"
    );
    assert_eq!(
        command(&env, "disconnect").await,
        "Disconnected from the IDE."
    );
    assert!(env.ide().is_none());

    // The fake takes one connection: the next attempt fails and says so.
    let msg = command(&env, "connect").await;
    assert!(msg.starts_with("Could not connect: port"), "{msg}");

    let second = fake_ide().await;
    std::fs::remove_file(lock_dir().join(format!("{}.lock", fake.port))).unwrap();
    write_lock(second.port, TOKEN);
    let msg = auto_connect(&env).await.unwrap();
    assert!(msg.starts_with("Connected to Fake Editor"), "{msg}");
    assert!(env.ide().is_some());
}
