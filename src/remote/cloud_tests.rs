//! `/remote` through Chanto cloud, against a fake relay on loopback: the CLI
//! creates the session, uploads sealed snapshots and events, and acts on the
//! commands a reader sends once it has joined with the link's keys.

use super::*;
use crate::test_support::isolated;
use chanto_proto::remote::{
    AppendRemoteEvents, CreateRemoteSession, RemoteCommand, RemoteCommandItem, Sealed,
};

struct FakeHost {
    inputs: Mutex<Vec<Input>>,
    messages: Mutex<Vec<Value>>,
}

impl Host for FakeHost {
    fn send(&self, input: Input) -> bool {
        self.inputs.lock().unwrap().push(input);
        true
    }
    fn snapshot(&self) -> Option<Vec<Value>> {
        Some(self.messages.lock().unwrap().clone())
    }
    fn info(&self) -> Value {
        json!({"model": "mock", "mode": "default", "todos": []})
    }
}

/// What the relay saw, with the bearer token each request carried.
#[derive(Default)]
struct Relay {
    created: Vec<(Option<String>, CreateRemoteSession)>,
    snapshots: Vec<(Option<String>, Sealed)>,
    events: Vec<(Option<String>, Sealed)>,
    commands: Vec<RemoteCommandItem>,
    ended: Vec<Option<String>>,
}

type Shared = Arc<Mutex<Relay>>;

fn bearer(h: &axum::http::HeaderMap) -> Option<String> {
    h.get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
}

/// Serves the relay on `listener` (bound before the runtime starts, so the
/// test can name its URL in the environment first).
async fn fake_cloud(listener: std::net::TcpListener) -> (String, Shared) {
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let relay: Shared = Default::default();
    let page = format!("{base}/remote/rs1");
    let app = Router::new()
        .route(
            "/v1/remote/sessions",
            post({
                let relay = relay.clone();
                move |h: axum::http::HeaderMap, Json(b): Json<CreateRemoteSession>| async move {
                    relay.lock().unwrap().created.push((bearer(&h), b));
                    Json(json!({"id": "rs1", "token": "session-token", "url": page}))
                }
            }),
        )
        .route(
            "/v1/remote/sessions/rs1/snapshot",
            axum::routing::put({
                let relay = relay.clone();
                move |h: axum::http::HeaderMap, Json(b): Json<Sealed>| async move {
                    relay.lock().unwrap().snapshots.push((bearer(&h), b));
                    Json(json!({}))
                }
            }),
        )
        .route(
            "/v1/remote/sessions/rs1/events",
            post({
                let relay = relay.clone();
                move |h: axum::http::HeaderMap, Json(b): Json<AppendRemoteEvents>| async move {
                    let who = bearer(&h);
                    let mut r = relay.lock().unwrap();
                    for e in b.events {
                        r.events.push((who.clone(), e));
                    }
                    Json(json!({}))
                }
            }),
        )
        .route(
            "/v1/remote/sessions/rs1/commands",
            get({
                let relay = relay.clone();
                move |axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>| async move {
                    let after: u64 = q["after"].parse().unwrap();
                    // A short long-poll: answer as soon as something is queued.
                    for _ in 0..50 {
                        let ready: Vec<_> = relay
                            .lock()
                            .unwrap()
                            .commands
                            .iter()
                            .filter(|c| c.seq > after)
                            .cloned()
                            .collect();
                        if !ready.is_empty() {
                            return Json(json!({"commands": ready}));
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Json(json!({"commands": []}))
                }
            }),
        )
        .route(
            "/v1/remote/sessions/rs1/end",
            post({
                let relay = relay.clone();
                move |h: axum::http::HeaderMap| async move {
                    relay.lock().unwrap().ended.push(bearer(&h));
                    Json(json!({}))
                }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, relay)
}

async fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(v) = f() {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

fn fragment_value(link: &str, name: &str) -> String {
    let (_, frag) = link.split_once('#').expect("a fragment");
    frag.split('&')
        .find_map(|p| p.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("no {name} in the link's fragment"))
        .to_string()
}

fn queue(relay: &Shared, seq: u64, command: Sealed) {
    relay
        .lock()
        .unwrap()
        .commands
        .push(RemoteCommandItem { seq, command });
}

#[test]
fn a_reader_joins_through_the_cloud_and_drives_the_session() {
    if isolated("remote::cloud_tests::a_reader_joins_through_the_cloud_and_drives_the_session") {
        return;
    }
    // The clipboard and the machine's name come from fake programs, so the
    // test neither touches the real clipboard nor depends on the host.
    let bin = std::env::temp_dir().join(format!("chanto-remote-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&bin).unwrap();
    let clip = bin.join("clip");
    crate::test_support::shim(&bin.join("pbcopy"), &format!("cat > '{}'", clip.display()));
    crate::test_support::shim(&bin.join("hostname"), "echo test-host");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let cloud_url = format!("http://{}", listener.local_addr().unwrap());
    // Before the runtime starts any threads.
    unsafe {
        std::env::set_var("PATH", &path);
        std::env::set_var("CHANTO_CLOUD_TOKEN", "account-token");
        std::env::set_var("CHANTO_CLOUD_URL", &cloud_url);
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (base, relay) = fake_cloud(listener).await;
        assert_eq!(base, cloud_url);
        let env = crate::agent::tests::make_env(
            "http://127.0.0.1:1",
            &bin,
            crate::permissions::Mode::Default,
            32768,
        );
        // Outside the interactive terminal there is no session to share.
        assert_eq!(command(&env, "cloud").await, NOT_HERE);
        assert_eq!(command(&env, "local").await, NOT_HERE);

        let host = Arc::new(FakeHost {
            inputs: Mutex::new(Vec::new()),
            messages: Mutex::new(vec![json!({"role": "user", "content": "hi"})]),
        });
        assert!(HOST.set(host.clone()).is_ok());
        SLOT.get_or_init(Default::default);

        // Signed in, so plain `/remote` goes through the cloud.
        let out = command(&env, "").await;
        assert!(
            out.starts_with("**Remote control is on, through chanto cloud.**"),
            "{out}"
        );
        let link = std::fs::read_to_string(&clip).expect("the full link went to the clipboard");
        assert!(
            link.starts_with(&format!("{base}/remote/rs1#k=")),
            "the clipboard holds the session's link"
        );
        let key = crate::remote_crypto::SessionKey::from_link(&link).expect("a key in the link");
        let cli_public = fragment_value(&link, "p");
        assert!(
            !out.contains(&key.token()),
            "the screen shows the link without its key"
        );
        assert!(out.contains(&format!("{base}/remote/rs1#k=••••••")));
        assert!(out.contains(&format!("Key fingerprint `{}`", key.fingerprint())));
        assert!(out.contains("The full link, with its key, is on your clipboard"));
        assert!(out.contains("/#token=••••••"), "the local page is masked");

        // The session was created with the account's token, its metadata
        // sealed with the link's key.
        let (who, created) = relay.lock().unwrap().created.pop().expect("a create");
        assert_eq!(who.as_deref(), Some("account-token"));
        assert_eq!(created.title, "", "the title only travels sealed");
        let meta: Value = key
            .open(Purpose::Meta, created.sealed_meta.as_ref().unwrap())
            .unwrap();
        assert_eq!(meta["machine"], "test-host");
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(meta["cwd"], cwd.display().to_string());
        assert_eq!(
            meta["title"],
            cwd.file_name().unwrap().to_string_lossy().as_ref()
        );

        // Before anyone joins, the snapshot is under the link's key and
        // goes up with the session's own token.
        let (who, snap) = wait_for("the first snapshot", || {
            relay.lock().unwrap().snapshots.first().cloned()
        })
        .await;
        assert_eq!(who.as_deref(), Some("session-token"));
        let snap: Value = key.open(Purpose::Snapshot, &snap).unwrap();
        assert_eq!(snap["messages"][0]["content"], "hi");
        assert_eq!(snap["id"], SESSION_ID);

        // A command for a reader that hasn't joined is ignored.
        let stranger = crate::remote_crypto::Handshake::generate().unwrap();
        let stranger_key = stranger.complete("stranger", &cli_public, "rs1").unwrap();
        queue(
            &relay,
            1,
            stranger_key
                .seal(Purpose::Commands, &RemoteCommand::Interrupt)
                .unwrap(),
        );
        // The reader joins with the link's key and its own handshake half.
        let reader = crate::remote_crypto::Handshake::generate().unwrap();
        let join = RemoteCommand::Join {
            viewer: "reader-1".into(),
            public: reader.public_token(),
        };
        queue(&relay, 2, key.seal(Purpose::Commands, &join).unwrap());
        let view = reader.complete("reader-1", &cli_public, "rs1").unwrap();
        let opened_events = |relay: &Shared| -> Vec<Value> {
            relay
                .lock()
                .unwrap()
                .events
                .iter()
                .filter_map(|(_, e)| view.open::<Value>(Purpose::Events, e).ok())
                .collect()
        };
        wait_for("the ready marker", || {
            opened_events(&relay)
                .iter()
                .any(|e| e["type"] == "ready")
                .then_some(())
        })
        .await;
        // The snapshot is now sealed for the reader.
        let joined_snap = relay.lock().unwrap().snapshots.last().unwrap().1.clone();
        let joined_snap: Value = view.open(Purpose::Snapshot, &joined_snap).unwrap();
        assert_eq!(joined_snap["messages"][0]["content"], "hi");

        // A second join, even with the link's key, is refused: no snapshot
        // or events are sealed for it.
        let snapshots_before_late = relay.lock().unwrap().snapshots.len();
        let late = crate::remote_crypto::Handshake::generate().unwrap();
        let late_join = RemoteCommand::Join {
            viewer: "reader-2".into(),
            public: late.public_token(),
        };
        queue(&relay, 3, key.seal(Purpose::Commands, &late_join).unwrap());

        // The reader's commands reach the session, once each.
        let message = view
            .seal(
                Purpose::Commands,
                &RemoteCommand::Message {
                    text: "from phone".into(),
                },
            )
            .unwrap();
        queue(&relay, 4, message.clone());
        queue(&relay, 5, message);
        let mut tampered = view
            .seal(
                Purpose::Commands,
                &RemoteCommand::Message {
                    text: "tampered".into(),
                },
            )
            .unwrap();
        tampered.c = tampered.c.chars().rev().collect();
        queue(&relay, 6, tampered);
        queue(
            &relay,
            7,
            view.seal(Purpose::Commands, &RemoteCommand::Interrupt)
                .unwrap(),
        );
        wait_for("the interrupt", || {
            (host.inputs.lock().unwrap().len() >= 2).then_some(())
        })
        .await;
        assert_eq!(
            *host.inputs.lock().unwrap(),
            vec![Input::Prompt("from phone".into()), Input::Interrupt],
            "the replay, the tampered command and the stranger's were dropped"
        );
        // Commands are handled in order, so the late join (seq 3) was seen.
        assert_eq!(
            relay.lock().unwrap().snapshots.len(),
            snapshots_before_late,
            "the late join got no snapshot"
        );
        assert_eq!(
            opened_events(&relay)
                .iter()
                .filter(|e| e["type"] == "ready")
                .count(),
            1,
            "only the first reader was told it's ready"
        );
        let late_view = late.complete("reader-2", &cli_public, "rs1").unwrap();
        {
            let r = relay.lock().unwrap();
            assert!(
                r.snapshots
                    .iter()
                    .all(|(_, s)| late_view.open::<Value>(Purpose::Snapshot, s).is_err()),
                "the late reader can't open any snapshot"
            );
            assert!(
                r.events
                    .iter()
                    .all(|(_, e)| late_view.open::<Value>(Purpose::Events, e).is_err()),
                "the late reader can't open any event"
            );
        }

        // An answer to an open prompt settles it.
        let hub = active().expect("the hub is live");
        let (id, answer) = hub.open(json!({"type": "permission_request"}));
        queue(
            &relay,
            8,
            view.seal(
                Purpose::Commands,
                &RemoteCommand::Respond {
                    request_id: id.clone(),
                    decision: Some("allow".into()),
                    feedback: None,
                    answer: None,
                },
            )
            .unwrap(),
        );
        let body = tokio::time::timeout(Duration::from_secs(10), answer)
            .await
            .expect("the answer arrives")
            .unwrap();
        assert_eq!(body["request_id"], id.as_str());
        assert_eq!(body["decision"], "allow");

        // What happens in the terminal goes up sealed for the reader, and
        // the end of a turn refreshes the snapshot.
        user("typed at the terminal");
        show("slash command output");
        // The conversation the turn leaves behind.
        *host.messages.lock().unwrap() = vec![json!({"role": "user", "content": "after the turn"})];
        turn_done(Some("boom"));
        let evs = wait_for("the turn's events", || {
            let evs = opened_events(&relay);
            let done = evs
                .iter()
                .any(|e| e["type"] == "turn_done" && e["error"] == "boom");
            done.then_some(evs)
        })
        .await;
        assert!(
            evs.iter()
                .any(|e| e["type"] == "user" && e["text"] == "typed at the terminal"),
            "the prompt went up: {evs:?}"
        );
        assert!(
            evs.iter()
                .any(|e| e["type"] == "info" && e["message"] == "slash command output"),
            "the command output went up: {evs:?}"
        );
        wait_for("the snapshot after the turn", || {
            relay
                .lock()
                .unwrap()
                .snapshots
                .iter()
                .filter_map(|(_, s)| view.open::<Value>(Purpose::Snapshot, s).ok())
                .any(|s| s["messages"][0]["content"] == "after the turn")
                .then_some(())
        })
        .await;
        assert!(
            relay
                .lock()
                .unwrap()
                .events
                .iter()
                .all(|(who, _)| who.as_deref() == Some("session-token"))
        );

        // `status` shows it again, masked; `link` prints the key; a second
        // start doesn't make a second session.
        let status = command(&env, "status").await;
        assert!(status.contains("#k=••••••") && !status.contains(&key.token()));
        let full = command(&env, "link").await;
        assert!(full.contains(&key.token()));
        assert!(full.contains("/#token="));
        assert!(!full.contains("••••••"));
        command(&env, "cloud").await;
        assert!(relay.lock().unwrap().created.is_empty());

        // Off: the relay hears the session end, with the session's token.
        assert_eq!(
            command(&env, "off").await,
            "Remote control is off; the dashboard no longer reaches this session."
        );
        let ended = wait_for("the end", || relay.lock().unwrap().ended.first().cloned()).await;
        assert_eq!(ended.as_deref(), Some("session-token"));
        assert!(active().is_none());
        assert_eq!(
            command(&env, "status").await,
            "Remote control is off. `/remote` turns it on."
        );
        assert_eq!(command(&env, "off").await, "Remote control is already off.");
        assert!(
            command(&env, "sideways")
                .await
                .starts_with("Usage: `/remote`")
        );

        // `/remote local` serves the session on loopback behind a fresh token.
        let out = command(&env, "local").await;
        assert!(
            out.starts_with("**Remote control is on.** Listening on this machine only (127.0.0.1)"),
            "{out}"
        );
        let local = std::fs::read_to_string(&clip).unwrap();
        let token = fragment_value(&local, "token");
        assert!(!out.contains(&token), "the screen hides the token");
        let base = local.split("/#").next().unwrap().to_string();
        assert!(base.starts_with("http://127.0.0.1:"), "{base}");
        let client = crate::http::client();
        let list: Value = client
            .get(format!("{base}/sessions"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list["sessions"][0]["id"], SESSION_ID);
        assert!(command(&env, "link").await.contains(&token));
        assert_eq!(
            command(&env, "stop").await,
            "Remote control is off; the phone's link no longer works."
        );
    });
    let _ = std::fs::remove_dir_all(&bin);
}
