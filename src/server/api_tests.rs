//! The session API over real HTTP against a scripted model: every request an
//! SDK or web client can send, including the ones it should not.

use super::*;
use crate::agent::tests::{mock_server, tc, tmpdir};
use crate::test_support::isolated;
use std::time::Duration;

struct Api {
    base: String,
    dir: std::path::PathBuf,
    client: reqwest::Client,
}

impl Api {
    async fn start(model: &str) -> Api {
        let dir = crate::util::canonical(tmpdir("server-api")).unwrap();
        let cli = <Cli as clap::Parser>::parse_from([
            "chanto",
            "--provider",
            "openai",
            "--base-url",
            model,
            "--model",
            "mock",
            "--cwd",
            dir.to_str().unwrap(),
            "serve",
        ]);
        let app = router(cli, "secret".into(), Default::default(), None);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Api {
            base,
            dir,
            client: crate::http::client(),
        }
    }

    async fn finish(res: reqwest::RequestBuilder) -> (StatusCode, Value) {
        let res = res.bearer_auth("secret").send().await.unwrap();
        let status = res.status();
        let text = res.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(json!(text)))
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        Self::finish(self.client.get(format!("{}{path}", self.base))).await
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        Self::finish(self.client.post(format!("{}{path}", self.base)).json(&body)).await
    }

    async fn delete(&self, path: &str) -> (StatusCode, Value) {
        Self::finish(self.client.delete(format!("{}{path}", self.base))).await
    }

    /// A new session and its event stream, open before any turn starts.
    async fn session(&self, options: Value) -> (String, Events) {
        let (status, body) = self.post("/sessions", options).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        let events = self.events(&id).await;
        (id, events)
    }

    async fn events(&self, id: &str) -> Events {
        let res = self
            .client
            .get(format!("{}/sessions/{id}/events", self.base))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["content-type"], "text/event-stream");
        Events {
            res,
            buf: String::new(),
        }
    }
}

struct Events {
    res: reqwest::Response,
    buf: String,
}

impl Events {
    /// The next event of the given type; earlier ones are skipped.
    async fn until(&mut self, kind: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                while let Some(end) = self.buf.find("\n\n") {
                    let frame: String = self.buf.drain(..end + 2).collect();
                    let Some(data) = frame.strip_prefix("data: ") else {
                        continue;
                    };
                    let event: Value = serde_json::from_str(data.trim()).unwrap();
                    if event["type"] == kind {
                        return event;
                    }
                    assert_ne!(
                        event["type"], "turn_done",
                        "the turn ended waiting for {kind}: {event}"
                    );
                }
                let chunk = self.res.chunk().await.unwrap().expect("stream ended");
                self.buf.push_str(&String::from_utf8_lossy(&chunk));
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {kind} event"))
    }
}

#[tokio::test]
async fn create_session_applies_options_and_reports_bad_ones() {
    if isolated("server::api_tests::create_session_applies_options_and_reports_bad_ones") {
        return;
    }
    let (model, seen) = mock_server(vec![json!({"content": "hello"})]).await;
    let api = Api::start(&model).await;

    // Bodies a client can get wrong are refused before anything starts.
    for (body, needle) in [
        (json!([1]), "must be an object"),
        (json!({"model": 1}), "`model` must be a string"),
        (json!({"cwd": "  "}), "`cwd` must not be empty"),
        (json!({"mode": "nope"}), "unknown `mode`"),
        (json!({"verify": "yes"}), "`verify` must be a boolean"),
        (json!({"max_turns": 0}), "`max_turns`"),
        (json!({"max_turns": 5_000_000_000u64}), "`max_turns`"),
        (json!({"allowed_tools": ["a,b"]}), "`allowed_tools`"),
        (json!({"disallowed_tools": [""]}), "`disallowed_tools`"),
        (json!({"tools": "x"}), "`tools` must be a list"),
        (json!({"tools": [{}]}), "tool `name` must be a string"),
        (json!({"tools": [{"name": "a b"}]}), "invalid tool name"),
        (
            json!({"tools": [{"name": "x".repeat(65)}]}),
            "invalid tool name",
        ),
        (json!({"tools": [{"name": "bash"}]}), "already registered"),
        (
            json!({"tools": [{"name": "t", "description": "d", "input_schema": {"type": "string"}}]}),
            "requires a description",
        ),
        (
            json!({"cwd": api.dir.join("missing")}),
            "must be an existing directory",
        ),
    ] {
        let (status, reply) = api.post("/sessions", body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(reply["error"].as_str().unwrap().contains(needle), "{reply}");
    }
    let file = api.dir.join("file.txt");
    std::fs::write(&file, "x").unwrap();
    let (status, _) = api.post("/sessions", json!({"cwd": file})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (id, mut events) = api
        .session(json!({"system_prompt_append": "MARKER-APPENDED", "disallowed_tools": ["Bash"]}))
        .await;
    // One server serves one project, and one live session owns a transcript.
    let other = crate::util::canonical(tmpdir("server-other")).unwrap();
    let (status, reply) = api.post("/sessions", json!({"cwd": other})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(reply["error"].as_str().unwrap().contains("serving"));
    let (status, _) = api.post("/sessions", json!({"resume": id[..8]})).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, reply) = api
        .post(&format!("/sessions/{id}/messages"), json!({"text": "hi"}))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(reply["started"], true);
    assert_eq!(events.until("turn_done").await["result"], "hello");

    // The options reached the model's request.
    let request = seen.lock().unwrap()[0].clone();
    assert!(
        request["messages"][0]["content"]
            .to_string()
            .contains("MARKER-APPENDED")
    );
    let tools = request["tools"].to_string();
    assert!(
        !tools.contains("\"Bash\"") && tools.contains("\"Read\""),
        "{tools}"
    );

    let (status, body) = api.get(&format!("/sessions/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("hi")
    );
    assert_eq!(body["messages"][0]["checkpoint"], 0);
    assert_eq!(body["messages"][1]["content"], "hello");
    assert_eq!(body["model"], "mock");
}

#[tokio::test]
async fn requests_for_unknown_sessions_are_not_found() {
    if isolated("server::api_tests::requests_for_unknown_sessions_are_not_found") {
        return;
    }
    let (model, _) = mock_server(vec![]).await;
    let api = Api::start(&model).await;
    let (status, reply) = api.get("/sessions/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(reply["error"], "no session nope");
    for path in ["checkpoints", "queue", "events"] {
        let (status, _) = api.get(&format!("/sessions/nope/{path}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    for (path, body) in [
        ("messages", json!({"text": "x"})),
        ("respond", json!({"request_id": "r"})),
        ("rewind", json!({"checkpoint": 0})),
        ("interrupt", json!({})),
        ("fork", json!({})),
    ] {
        let (status, _) = api.post(&format!("/sessions/nope/{path}"), body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    let (status, _) = api.delete("/sessions/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = api.delete("/sessions/nope/queue/q").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_message_needs_text_and_a_command_answers_without_the_model() {
    if isolated("server::api_tests::a_message_needs_text_and_a_command_answers_without_the_model") {
        return;
    }
    let (model, seen) = mock_server(vec![]).await;
    let api = Api::start(&model).await;
    let (id, mut events) = api.session(json!({})).await;
    let path = format!("/sessions/{id}/messages");
    for body in [json!({}), json!({"text": 3})] {
        let (status, reply) = api.post(&path, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(reply["error"], "`text` is required");
    }
    let (status, _) = api.post(&path, json!({"text": "/help"})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let done = events.until("turn_done").await;
    assert!(done["output"].as_str().unwrap().contains("/help"), "{done}");
    assert!(seen.lock().unwrap().is_empty(), "no model call for /help");
    // The turn released the session, so the next message starts a new one.
    let (_, reply) = api.post(&path, json!({"text": "/help"})).await;
    assert_eq!(reply["started"], true);
    events.until("turn_done").await;
}

#[tokio::test]
async fn a_remote_tool_round_trips_through_the_client() {
    if isolated("server::api_tests::a_remote_tool_round_trips_through_the_client") {
        return;
    }
    let (model, seen) = mock_server(vec![
        json!({"tool_calls": tc("lookup", json!({"q": 1}))}),
        json!({"content": "done"}),
    ])
    .await;
    let api = Api::start(&model).await;
    let (id, mut events) = api
        .session(json!({
            "mode": "bypass",
            "tools": [{
                "name": "lookup",
                "description": "Look something up",
                "input_schema": {"type": "object", "properties": {}}
            }]
        }))
        .await;
    api.post(&format!("/sessions/{id}/messages"), json!({"text": "go"}))
        .await;
    let call = events.until("tool_call").await;
    assert_eq!(call["name"], "lookup");
    assert_eq!(call["input"]["q"], 1);
    let call_id = call["id"].as_str().unwrap();

    // A client reconnecting now sees the call it still owes an answer to.
    let mut late = api.events(&id).await;
    assert_eq!(late.until("tool_call").await["id"], call_id);

    let respond = format!("/sessions/{id}/respond");
    let (status, reply) = api.post(&respond, json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(reply["error"].as_str().unwrap().contains("is required"));
    let (status, _) = api.post(&respond, json!({"id": "other"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for bad in [
        json!({"id": call_id}),
        json!({"id": call_id, "result": {"content": 1, "is_error": false}}),
        json!({"id": call_id, "result": {"content": "x", "is_error": "no"}}),
    ] {
        let (status, _) = api.post(&respond, bad).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let (status, _) = api
        .post(
            &respond,
            json!({"id": call_id, "result": {"content": "FOUND-IT", "is_error": false}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(events.until("turn_done").await["result"], "done");
    assert!(
        seen.lock().unwrap()[1]["messages"]
            .to_string()
            .contains("FOUND-IT")
    );
    // Answered once; a second answer finds nothing.
    let (status, _) = api
        .post(
            &respond,
            json!({"id": call_id, "result": {"content": "x", "is_error": false}}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn interrupting_a_pending_tool_call_ends_the_turn_and_clears_it() {
    if isolated("server::api_tests::interrupting_a_pending_tool_call_ends_the_turn_and_clears_it") {
        return;
    }
    let (model, _) = mock_server(vec![json!({"tool_calls": tc("lookup", json!({}))})]).await;
    let api = Api::start(&model).await;
    let (id, mut events) = api
        .session(json!({
            "mode": "bypass",
            "tools": [{"name": "lookup", "description": "d", "input_schema": {"type": "object"}}]
        }))
        .await;
    let path = format!("/sessions/{id}/messages");
    api.post(&path, json!({"text": "go"})).await;
    let call = events.until("tool_call").await;

    // While the tool waits the session is busy: reads that need the agent say
    // so, a list still answers, and a new prompt queues instead of starting.
    let (status, _) = api.get(&format!("/sessions/{id}")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = api.get(&format!("/sessions/{id}/checkpoints")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = api
        .post(&format!("/sessions/{id}/rewind"), json!({"checkpoint": 0}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = api.post(&format!("/sessions/{id}/fork"), json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (_, list) = api.get("/sessions").await;
    let live = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == id.as_str())
        .unwrap()
        .clone();
    assert_eq!(live["busy"], true);
    // An immediate command answers now; a prompt goes in the queue.
    let (status, reply) = api.post(&path, json!({"text": "/help"})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(reply["output"].is_string(), "{reply}");
    let (_, queued) = api.post(&path, json!({"text": "later"})).await;
    assert_eq!(queued["queued"], true);
    assert_eq!(queued["message"]["text"], "later");
    let qid = queued["message"]["id"].as_str().unwrap();
    let queue = format!("/sessions/{id}/queue/{qid}");
    let (status, list) = Api::finish(
        api.client
            .patch(format!("{}{queue}", api.base))
            .json(&json!({"text": "  "})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{list}");
    let (status, list) = api.delete(&queue).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !list["queued"].to_string().contains(qid),
        "the message left the queue: {list}"
    );

    let (status, _) = api
        .post(&format!("/sessions/{id}/interrupt"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    let done = events.until("turn_done").await;
    assert_eq!(done["interrupted"], true, "{done}");
    // The cancelled call is gone: a late answer has nothing to reach.
    let (status, _) = api
        .post(
            &format!("/sessions/{id}/respond"),
            json!({"id": call["id"], "result": {"content": "x", "is_error": false}}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Interrupting an idle session is harmless.
    let (status, _) = api
        .post(&format!("/sessions/{id}/interrupt"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn permission_requests_are_answered_over_the_api() {
    if isolated("server::api_tests::permission_requests_are_answered_over_the_api") {
        return;
    }
    let (model, seen) = mock_server(vec![
        json!({"tool_calls": tc("Write", json!({"file_path": "a.txt", "content": "A"}))}),
        json!({"tool_calls": tc("Write", json!({"file_path": "b.txt", "content": "B"}))}),
        json!({"content": "ok"}),
    ])
    .await;
    let api = Api::start(&model).await;
    let (id, mut events) = api.session(json!({"mode": "default"})).await;
    api.post(&format!("/sessions/{id}/messages"), json!({"text": "go"}))
        .await;
    let respond = format!("/sessions/{id}/respond");
    let first = events.until("permission_request").await;
    assert_eq!(first["tool"], "Write");
    api.post(
        &respond,
        json!({"request_id": first["request_id"], "decision": "allow"}),
    )
    .await;
    let second = events.until("permission_request").await;
    let (status, _) = api
        .post(
            &respond,
            json!({"request_id": second["request_id"], "decision": "deny", "feedback": "NOT-THAT-ONE"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(events.until("turn_done").await["result"], "ok");
    assert_eq!(std::fs::read_to_string(api.dir.join("a.txt")).unwrap(), "A");
    assert!(!api.dir.join("b.txt").exists(), "a denied write ran");
    assert!(
        seen.lock().unwrap()[2]["messages"]
            .to_string()
            .contains("NOT-THAT-ONE")
    );
}

#[tokio::test]
async fn forks_copy_the_conversation_and_validate_their_request() {
    if isolated("server::api_tests::forks_copy_the_conversation_and_validate_their_request") {
        return;
    }
    let (model, seen) = mock_server(vec![
        json!({"content": "first answer"}),
        json!({"content": "fork answer"}),
    ])
    .await;
    let api = Api::start(&model).await;
    let (id, mut events) = api.session(json!({"mode": "plan"})).await;
    api.post(
        &format!("/sessions/{id}/messages"),
        json!({"text": "original prompt"}),
    )
    .await;
    events.until("turn_done").await;

    let fork = format!("/sessions/{id}/fork");
    for bad in [
        json!({"point": "one"}),
        json!({"point": -1}),
        json!({"name": 1}),
    ] {
        let (status, reply) = api.post(&fork, bad).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            reply["error"]
                .as_str()
                .unwrap()
                .contains("checkpoint index")
        );
    }
    let (status, _) = api.post(&fork, json!({"point": 99})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, child) = api.post(&fork, json!({"name": "branch"})).await;
    assert_eq!(status, StatusCode::OK, "{child}");
    let child_id = child["id"].as_str().unwrap();
    assert_ne!(child_id, id);
    let (_, state) = api.get(&format!("/sessions/{child_id}")).await;
    assert!(
        state["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("original prompt")
    );
    assert_eq!(state["mode"], "plan", "the fork keeps the source's mode");

    let mut child_events = api.events(child_id).await;
    api.post(
        &format!("/sessions/{child_id}/messages"),
        json!({"text": "fork prompt"}),
    )
    .await;
    assert_eq!(
        child_events.until("turn_done").await["result"],
        "fork answer"
    );
    let sent = seen.lock().unwrap()[1]["messages"].to_string();
    assert!(sent.contains("original prompt") && sent.contains("fork prompt"));
    // The source didn't see the child's turn.
    let (_, state) = api.get(&format!("/sessions/{id}")).await;
    assert_eq!(state["messages"].as_array().unwrap().len(), 2);

    // The list shows the tree: the child points at its parent.
    let (_, list) = api.get("/sessions").await;
    let row = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == child_id)
        .unwrap()
        .clone();
    assert_eq!(row["parent_id"], id.as_str());
    assert_eq!(row["depth"], 1);
}

#[tokio::test]
async fn deleting_a_session_ends_it_and_purge_removes_the_transcript() {
    if isolated("server::api_tests::deleting_a_session_ends_it_and_purge_removes_the_transcript") {
        return;
    }
    let (model, _) = mock_server(vec![json!({"content": "a"}), json!({"content": "b"})]).await;
    let api = Api::start(&model).await;
    let (id, mut events) = api.session(json!({})).await;
    api.post(&format!("/sessions/{id}/messages"), json!({"text": "keep"}))
        .await;
    events.until("turn_done").await;

    // Ended but saved: the live session is gone, its transcript is listed.
    let (status, _) = api.delete(&format!("/sessions/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = api.get(&format!("/sessions/{id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, list) = api.get("/sessions").await;
    let row = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == id.as_str())
        .unwrap()
        .clone();
    assert_eq!(row["saved"], true);

    // A saved session forks from disk, then resumes as a live one.
    let (status, child) = api.post(&format!("/sessions/{id}/fork"), json!({})).await;
    assert_eq!(status, StatusCode::OK, "{child}");
    let (_, state) = api
        .get(&format!("/sessions/{}", child["id"].as_str().unwrap()))
        .await;
    assert!(
        state["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("keep")
    );

    // Purge deletes it, and purging again finds nothing.
    let purge = format!("/sessions/{id}?purge=true");
    let (status, _) = api.delete(&purge).await;
    assert_eq!(status, StatusCode::OK);
    let (_, list) = api.get("/sessions?all=true").await;
    assert!(
        !list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == id.as_str()),
        "{list}"
    );
    let (status, _) = api.delete(&purge).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sessions_list_reads_the_project_named_in_the_query() {
    if isolated("server::api_tests::sessions_list_reads_the_project_named_in_the_query") {
        return;
    }
    let (model, _) = mock_server(vec![]).await;
    let api = Api::start(&model).await;
    let other = crate::util::canonical(tmpdir("server-list")).unwrap();
    let mut saved = crate::session::Session::new(&other, "m");
    saved.title = "elsewhere".into();
    saved.save(vec![crate::llm::Message::user("x")]).unwrap();
    let (_, here) = api.get("/sessions").await;
    assert_eq!(here["sessions"], json!([]));
    let (_, there) = api
        .get(&format!(
            "/sessions?cwd={}",
            urlencoding::encode(other.to_str().unwrap())
        ))
        .await;
    assert_eq!(there["sessions"][0]["id"], saved.id.as_str());
    // A folder that doesn't exist falls back to the server's own.
    let (_, gone) = api.get("/sessions?cwd=/no/such/folder").await;
    assert_eq!(gone["sessions"], json!([]));
}

#[tokio::test]
async fn artifacts_need_html_within_the_limit_and_are_sandboxed() {
    if isolated("server::api_tests::artifacts_need_html_within_the_limit_and_are_sandboxed") {
        return;
    }
    let (model, _) = mock_server(vec![]).await;
    let api = Api::start(&model).await;
    for body in [json!({}), json!({"html": "   "}), json!({"html": 1})] {
        let (status, _) = api.post("/artifacts", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let (status, _) = api
        .post(
            "/artifacts",
            json!({"html": "x".repeat(MAX_ARTIFACT_BYTES + 1)}),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let (status, made) = api.post("/artifacts", json!({"html": "<b>hi</b>"})).await;
    assert_eq!(status, StatusCode::OK);
    let page = api
        .client
        .get(format!("{}{}", api.base, made["url"].as_str().unwrap()))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(page.headers()["content-security-policy"], ARTIFACT_CSP);
    assert_eq!(page.text().await.unwrap(), "<b>hi</b>");
    let missing = api
        .client
        .get(format!("{}/artifacts/nope", api.base))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn voice_needs_a_recording_and_a_configured_backend() {
    if crate::test_support::isolated_with(
        "server::api_tests::voice_needs_a_recording_and_a_configured_backend",
        "[voice]\nurl = \"http://127.0.0.1:9/v1/audio/transcriptions\"\n",
    ) {
        return;
    }
    let (model, _) = mock_server(vec![]).await;
    let api = Api::start(&model).await;
    let empty = Api::finish(api.client.post(format!("{}/voice/transcribe", api.base))).await;
    assert_eq!(empty.0, StatusCode::BAD_REQUEST);
    assert!(
        empty.1["error"]
            .as_str()
            .unwrap()
            .contains("recorded audio")
    );
}

#[test]
fn only_loopback_servers_host_runs_unless_asked() {
    let plain = <Cli as clap::Parser>::parse_from(["chanto", "serve"]);
    assert!(hosts_runs(&plain, "127.0.0.1"));
    assert!(hosts_runs(&plain, "::1"));
    assert!(hosts_runs(&plain, "localhost"));
    assert!(!hosts_runs(&plain, "0.0.0.0"));
    assert!(!hosts_runs(&plain, "100.64.0.1"));
    assert!(!hosts_runs(&plain, "localhost.example.com"));
    let mut asked = plain;
    asked.runs = true;
    assert!(hosts_runs(&asked, "0.0.0.0"));
}
