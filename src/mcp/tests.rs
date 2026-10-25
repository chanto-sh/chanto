use super::*;
use std::collections::HashMap as Map;
use tokio::io::AsyncReadExt;

#[derive(Default)]
struct MockState {
    base: String,
    challenge: Option<String>,
    redirect: Option<String>,
    valid_tokens: Vec<String>,
    registered: bool,
    refreshes: u32,
    resource_param: Option<String>,
}

struct Req {
    method: String,
    path: String,
    query: Map<String, String>,
    headers: Map<String, String>,
    body: String,
}

async fn read_req(sock: &mut tokio::net::TcpStream) -> Option<Req> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        let s = String::from_utf8_lossy(&buf).to_string();
        let Some(h) = s.find("\r\n\r\n") else {
            continue;
        };
        let head = &s[..h];
        let mut lines = head.lines();
        let first: Vec<&str> = lines.next()?.split_whitespace().collect();
        let headers: Map<String, String> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        let cl: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if buf.len() < h + 4 + cl {
            continue;
        }
        let body = String::from_utf8_lossy(&buf[h + 4..h + 4 + cl]).to_string();
        let url = reqwest::Url::parse(&format!("http://x{}", first[1])).ok()?;
        return Some(Req {
            method: first[0].to_string(),
            path: url.path().to_string(),
            query: url.query_pairs().into_owned().collect(),
            headers,
            body,
        });
    }
}

fn resp(status: &str, extra: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn rpc(req: &Value) -> Option<Value> {
    let id = req.get("id")?.clone();
    let result = match req["method"].as_str()? {
        "initialize" => {
            json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}, "resources": {}, "prompts": {}}, "serverInfo": {"name": "mock"}})
        }
        "tools/list" => {
            json!({"tools": [{"name": "echo", "description": "Echo", "inputSchema": {"type": "object"}}]})
        }
        "prompts/list" => json!({"prompts": [{"name": "review", "description": "Review a file",
            "arguments": [{"name": "file", "required": true}, {"name": "focus"}]}]}),
        "prompts/get" => {
            let a = &req["params"]["arguments"];
            json!({"messages": [{"role": "user", "content": {"type": "text",
                "text": format!("Review {} focusing on {}", a["file"].as_str().unwrap_or("?"), a["focus"].as_str().unwrap_or("everything"))}}]})
        }
        "resources/list" => {
            json!({"resources": [{"uri": "docs://readme", "name": "README", "mimeType": "text/markdown"}]})
        }
        "resources/read" => {
            json!({"contents": [{"uri": req["params"]["uri"], "mimeType": "text/plain", "text": "hello resource"}]})
        }
        _ => {
            return Some(
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "not found"}}),
            );
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

async fn handle(req: Req, st: &Arc<Mutex<MockState>>) -> String {
    let base = st.lock().unwrap().base.clone();
    match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/mcp") => {
            let auth = req.headers.get("authorization").cloned().unwrap_or_default();
            let ok = st.lock().unwrap().valid_tokens.iter().any(|t| auth == format!("Bearer {t}"));
            if !ok {
                let www = format!("WWW-Authenticate: Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\"\r\n");
                return resp("401 Unauthorized", &www, r#"{"error":"unauthorized"}"#);
            }
            let v: Value = serde_json::from_str(&req.body).unwrap_or_default();
            match rpc(&v) {
                Some(r) => resp("200 OK", "", &r.to_string()),
                None => resp("202 Accepted", "", ""),
            }
        }
        ("GET", "/.well-known/oauth-protected-resource/mcp") => {
            resp("200 OK", "", &json!({"resource": format!("{base}/mcp"), "authorization_servers": [format!("{base}/as")]}).to_string())
        }
        ("GET", "/.well-known/oauth-authorization-server/as") => resp(
            "200 OK",
            "",
            &json!({"issuer": format!("{base}/as"), "authorization_endpoint": format!("{base}/as/authorize"),
                "token_endpoint": format!("{base}/as/token"), "registration_endpoint": format!("{base}/as/register"),
                "code_challenge_methods_supported": ["S256"]})
            .to_string(),
        ),
        ("POST", "/as/register") => {
            let v: Value = serde_json::from_str(&req.body).unwrap_or_default();
            assert!(v["redirect_uris"][0].as_str().unwrap().starts_with("http://127.0.0.1:"));
            st.lock().unwrap().registered = true;
            resp("201 Created", "", r#"{"client_id":"cid-123"}"#)
        }
        ("GET", "/as/authorize") => {
            let q = &req.query;
            assert_eq!(q["client_id"], "cid-123");
            assert_eq!(q["code_challenge_method"], "S256");
            let mut s = st.lock().unwrap();
            s.challenge = Some(q["code_challenge"].clone());
            s.redirect = Some(q["redirect_uri"].clone());
            s.resource_param = q.get("resource").cloned();
            let loc = format!("{}?code=the-code&state={}", q["redirect_uri"], q["state"]);
            format!("HTTP/1.1 302 Found\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        }
        ("POST", "/as/token") => {
            let form: Map<String, String> = reqwest::Url::parse(&format!("http://x/?{}", req.body)).unwrap().query_pairs().into_owned().collect();
            let mut s = st.lock().unwrap();
            match form["grant_type"].as_str() {
                "authorization_code" => {
                    use base64::Engine;
                    use sha2::Digest;
                    assert_eq!(form["code"], "the-code");
                    assert_eq!(Some(&form["redirect_uri"]), s.redirect.as_ref());
                    let c = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(form["code_verifier"].as_bytes()));
                    assert_eq!(Some(c), s.challenge.clone(), "PKCE verifier must match the challenge");
                    assert_eq!(form["resource"], format!("{base}/mcp"));
                    s.valid_tokens.push("tok-1".into());
                    resp("200 OK", "", r#"{"access_token":"tok-1","token_type":"Bearer","expires_in":3600,"refresh_token":"rt-1"}"#)
                }
                "refresh_token" => {
                    assert_eq!(form["refresh_token"], "rt-1");
                    s.refreshes += 1;
                    s.valid_tokens.push("tok-2".into());
                    resp("200 OK", "", r#"{"access_token":"tok-2","token_type":"Bearer","expires_in":3600}"#)
                }
                g => panic!("unexpected grant {g}"),
            }
        }
        _ => resp("404 Not Found", "", "{}"),
    }
}

async fn mock() -> (String, Arc<Mutex<MockState>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let st = Arc::new(Mutex::new(MockState {
        base: base.clone(),
        ..Default::default()
    }));
    let st2 = st.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let st = st2.clone();
            tokio::spawn(async move {
                if let Some(req) = read_req(&mut sock).await {
                    let r = handle(req, &st).await;
                    let _ = sock.write_all(r.as_bytes()).await;
                }
            });
        }
    });
    (base, st)
}

/// A "browser": follow the authorization redirect to the loopback callback.
fn fake_browser(url: &str) {
    let url = url.to_string();
    tokio::spawn(async move {
        let http = crate::http::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let r = http.get(&url).send().await.unwrap();
        let loc = r.headers()["location"].to_str().unwrap().to_string();
        let page = http.get(&loc).send().await.unwrap();
        assert!(page.status().is_success());
    });
}

#[tokio::test]
async fn oauth_login_resources_prompts_and_refresh() {
    let _lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("CHANTO_HOME");
    let home =
        std::env::temp_dir().join(format!("chanto-mcp-auth-{}", uuid::Uuid::new_v4().simple()));
    // SAFETY: test-only; token storage goes to a temp CHANTO_HOME.
    unsafe { std::env::set_var("CHANTO_HOME", &home) };
    let (base, st) = mock().await;
    let cfg = McpServerConfig {
        url: Some(format!("{base}/mcp")),
        ..Default::default()
    };

    // No token yet: connects in the "needs login" state instead of failing.
    let c = McpClient::connect("mock", &cfg).await.unwrap();
    assert_eq!(c.status(), Status::NeedsLogin);
    assert!(c.tools().is_empty());

    // The next start knows it needs a login without asking the server, and
    // logging in from that state still works.
    let c = McpClient::connect("mock", &cfg).await.unwrap();
    assert_eq!(c.status(), Status::NeedsLogin);
    assert!(
        c.challenge.lock().unwrap().is_none(),
        "answered from the cache"
    );

    c.login(&|u: &str| fake_browser(u)).await.unwrap();
    assert_eq!(c.status(), Status::Connected);
    assert!(st.lock().unwrap().registered);
    assert_eq!(
        st.lock().unwrap().resource_param.as_deref(),
        Some(format!("{base}/mcp").as_str())
    );
    assert_eq!(c.tool_names(), vec!["mcp__mock__echo"]);
    assert!(c.has_resources());

    // Token stored with owner-only permissions.
    let path = crate::mcp_auth::store_path("mock", &format!("{base}/mcp"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // Resources and prompts.
    let all = vec![c.clone()];
    let (server, uri, text) = read_mention(&all, "mock:docs://readme")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (server.as_str(), uri.as_str(), text.as_str()),
        ("mock", "docs://readme", "hello resource")
    );
    assert!(read_mention(&all, "other:docs://readme").await.is_none());
    assert_eq!(prompt_commands(&all)[0].0, "mcp__mock__review");
    let p = run_prompt_command(&all, "mcp__mock__review", "src/a.rs error handling")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p, "Review src/a.rs focusing on error handling");
    let missing = run_prompt_command(&all, "mcp__mock__review", "")
        .await
        .unwrap();
    assert!(missing.unwrap_err().to_string().contains("file"));

    // Server revokes the token: the client refreshes and retries transparently.
    st.lock().unwrap().valid_tokens.retain(|t| t != "tok-1");
    assert_eq!(
        c.read_resource("docs://readme").await.unwrap().0,
        "hello resource"
    );
    assert_eq!(st.lock().unwrap().refreshes, 1);
    assert_eq!(
        crate::mcp_auth::load("mock", &format!("{base}/mcp"))
            .unwrap()
            .access_token,
        "tok-2"
    );

    // A new session reuses the stored token.
    let c2 = McpClient::connect("mock", &cfg).await.unwrap();
    assert_eq!(c2.status(), Status::Connected);

    assert!(c2.logout());
    assert!(!path.exists());
    assert_eq!(c2.status(), Status::NeedsLogin);
    unsafe {
        if let Some(previous) = previous {
            std::env::set_var("CHANTO_HOME", previous);
        } else {
            std::env::remove_var("CHANTO_HOME");
        }
    }
    let _ = std::fs::remove_dir_all(home);
}

/// Answers each question with the first scripted answer whose key appears in it.
struct Scripted {
    answers: Mutex<Vec<(&'static str, &'static str)>>,
    asked: Mutex<Vec<String>>,
}

impl Scripted {
    fn new(answers: &[(&'static str, &'static str)]) -> Arc<Scripted> {
        Arc::new(Scripted {
            answers: Mutex::new(answers.to_vec()),
            asked: Default::default(),
        })
    }
}

#[async_trait]
impl crate::agent::Frontend for Scripted {
    fn emit(&self, _: crate::agent::Event) {}
    async fn permission(
        &self,
        _: crate::agent::PermissionRequest,
    ) -> crate::agent::PermissionReply {
        crate::agent::PermissionReply::Deny(None)
    }
    async fn question(&self, q: &str, _: &[String]) -> Option<String> {
        self.asked.lock().unwrap().push(q.to_string());
        let mut answers = self.answers.lock().unwrap();
        let i = answers.iter().position(|(k, _)| q.contains(k))?;
        Some(answers.remove(i).1.to_string())
    }
}

fn ask(fe: &Arc<Scripted>) -> Option<&dyn crate::agent::Frontend> {
    Some(&**fe as &dyn crate::agent::Frontend)
}

#[test]
fn tool_specs_are_valid_for_model_apis() {
    // A server's bad schema or long name used to fail every model request.
    assert_eq!(
        object_schema(Some(&json!("nope"))),
        json!({"type": "object"})
    );
    assert_eq!(
        object_schema(Some(&json!({"type": "array"}))),
        json!({"type": "object"})
    );
    assert_eq!(object_schema(None), json!({"type": "object"}));
    assert_eq!(
        object_schema(Some(&json!({"properties": {"a": {}}}))),
        json!({"type": "object", "properties": {"a": {}}})
    );
    let c = McpClient::with_tools_for_test("github-enterprise-prod", vec![], None);
    assert_eq!(c.full_name("echo"), "mcp__github-enterprise-prod__echo");
    let long = c.full_name("list_repository_collaborators_with_their_permissions");
    let other = c.full_name("list_repository_collaborators_with_their_permission_levels");
    assert_eq!(long.len(), 64, "{long}");
    assert!(long.starts_with("mcp__github-enterprise-prod__list_repository"));
    assert_ne!(long, other);
    assert_eq!(
        long,
        c.full_name("list_repository_collaborators_with_their_permissions"),
        "stable"
    );
}

#[test]
fn sse_parser_handles_split_chunks() {
    let mut p = SseParser::default();
    assert!(p.push(b"event: endpoint\r\ndata: /messages?s").is_empty());
    assert_eq!(
        p.push(b"=1\r\n\r\ndata: {\"a\":\ndata: 1}\n\n"),
        vec![
            ("endpoint".to_string(), "/messages?s=1".to_string()),
            (String::new(), "{\"a\":\n1}".to_string())
        ]
    );
    assert!(p.push(b"data: x\n").is_empty());
    assert_eq!(p.push(b"\n\n"), vec![(String::new(), "x".to_string())]);
}

#[tokio::test]
async fn elicitation_answers() {
    // Nobody to ask (print mode): declined.
    let confirm =
        json!({"message": "Deploy now?", "requestedSchema": {"type": "object", "properties": {}}});
    assert_eq!(
        elicit("srv", &confirm, None).await,
        json!({"action": "decline"})
    );
    let fe = Scripted::new(&[("Deploy now?", "Accept")]);
    assert_eq!(
        elicit("srv", &confirm, ask(&fe)).await,
        json!({"action": "accept", "content": {}})
    );
    assert!(fe.asked.lock().unwrap()[0].contains("MCP server `srv` asks: Deploy now?"));
    // Dismissed: cancel.
    assert_eq!(
        elicit("srv", &confirm, ask(&Scripted::new(&[]))).await,
        json!({"action": "cancel"})
    );

    let p = json!({"message": "Profile", "requestedSchema": {"type": "object", "required": ["handle"], "properties": {
        "handle": {"type": "string", "title": "Handle", "minLength": 2},
        "score": {"type": "number", "title": "Score", "minimum": 0}}}});
    // Too short, then valid; the optional number is skipped.
    let fe = Scripted::new(&[
        ("Handle", "x"),
        ("Handle", "xy"),
        ("Score", SKIP),
        ("Send this", SEND),
    ]);
    assert_eq!(
        elicit("srv", &p, ask(&fe)).await,
        json!({"action": "accept", "content": {"handle": "xy"}})
    );
    assert!(
        fe.asked
            .lock()
            .unwrap()
            .iter()
            .any(|q| q.contains("\"x\" isn't valid: needs at least 2 characters"))
    );
    // Declining is the last step, after reviewing what would be sent.
    let fe = Scripted::new(&[("Handle", "xy"), ("Score", SKIP), ("Send this", DECLINE)]);
    assert_eq!(
        elicit("srv", &p, ask(&fe)).await,
        json!({"action": "decline"})
    );
    assert!(
        fe.asked
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("handle: \"xy\"")
    );
    // Required fields come first, in the server's order, whatever the key order.
    let order = json!({"message": "Order", "requestedSchema": {"type": "object", "required": ["zed", "alpha"], "properties": {
        "alpha": {"type": "string", "title": "Alpha"}, "beta": {"type": "string", "title": "Beta"}, "zed": {"type": "string", "title": "Zed"}}}});
    let fe = Scripted::new(&[
        ("Zed", "z"),
        ("Alpha", "a"),
        ("Beta", SKIP),
        ("Send this", SEND),
    ]);
    elicit("srv", &order, ask(&fe)).await;
    let asked = fe.asked.lock().unwrap();
    assert!(
        asked[0].contains("[1/3] Zed")
            && asked[1].contains("[2/3] Alpha")
            && asked[2].contains("[3/3] Beta"),
        "{asked:?}"
    );
}

struct EchoSampler(Mutex<Option<SampleRequest>>);

#[async_trait]
impl Sampler for EchoSampler {
    async fn sample(&self, r: SampleRequest) -> Result<(String, String)> {
        let reply = format!("echo: {}", r.messages.last().unwrap().1);
        *self.0.lock().unwrap() = Some(r);
        Ok((reply, "test-model".into()))
    }
}

#[tokio::test]
async fn sampling_asks_then_runs_on_the_users_model() {
    let params = json!({"messages": [{"role": "user", "content": {"type": "text", "text": "Summarize this page"}}],
                        "systemPrompt": "Be brief", "maxTokens": 200});
    let sampler = EchoSampler(Mutex::new(None));
    // Nobody to approve it: refused.
    assert!(sample("srv", &params, None, Some(&sampler)).await.is_err());
    // Denied: refused, nothing run.
    let fe = Scripted::new(&[("wants to run a prompt", "Deny")]);
    assert_eq!(
        sample("srv", &params, ask(&fe), Some(&sampler))
            .await
            .unwrap_err()
            .0,
        -1
    );
    assert!(sampler.0.lock().unwrap().is_none());
    // Allowed: runs with the server's messages, system prompt and limit.
    let fe = Scripted::new(&[("wants to run a prompt", "Allow")]);
    let out = sample("srv", &params, ask(&fe), Some(&sampler))
        .await
        .unwrap();
    assert_eq!(
        out,
        json!({"role": "assistant", "content": {"type": "text", "text": "echo: Summarize this page"}, "model": "test-model", "stopReason": "endTurn"})
    );
    let r = sampler.0.lock().unwrap().take().unwrap();
    assert_eq!((r.system.as_deref(), r.max_tokens), (Some("Be brief"), 200));
    assert!(fe.asked.lock().unwrap()[0].contains("Summarize this page"));
    // Images aren't supported.
    let image = json!({"messages": [{"role": "user", "content": {"type": "image", "data": "x", "mimeType": "image/png"}}]});
    let fe = Scripted::new(&[("wants to run a prompt", "Allow")]);
    assert_eq!(
        sample("srv", &image, ask(&fe), Some(&sampler))
            .await
            .unwrap_err()
            .0,
        -32602
    );
}

#[derive(Default)]
struct SseMock {
    stream: Option<tokio::sync::mpsc::UnboundedSender<Value>>,
    /// The `tools/call` waiting for the client's elicitation answer.
    call: Option<Value>,
    elicitation_declared: bool,
    streams: u32,
    /// Never answer `tools/call`.
    hang: bool,
    /// Every message the client POSTed.
    posted: Vec<Value>,
}

/// The server's reply to a POSTed message, sent on the event stream.
fn sse_rpc(v: &Value, st: &Arc<Mutex<SseMock>>) -> Option<Value> {
    let mut s = st.lock().unwrap();
    s.posted.push(v.clone());
    if s.hang && v["method"] == "tools/call" {
        return None;
    }
    match v["method"].as_str() {
        // The client's answer to our elicitation: finish the tool call with it.
        None => {
            let id = s.call.take()?;
            Some(
                json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": v["result"].to_string()}]}}),
            )
        }
        Some("tools/call") => {
            s.call = Some(v["id"].clone());
            Some(
                json!({"jsonrpc": "2.0", "id": 99, "method": "elicitation/create", "params": {
                "message": "Who are you?",
                "requestedSchema": {"type": "object", "required": ["name"], "properties": {
                    "name": {"type": "string", "title": "Name"},
                    "age": {"type": "integer", "title": "Age", "minimum": 0},
                    "ok": {"type": "boolean", "title": "Subscribe"},
                    "color": {"type": "string", "title": "Colour", "enum": ["r", "g"], "enumNames": ["Red", "Green"]}}}}}),
            )
        }
        Some(m) => {
            if m == "initialize" {
                s.elicitation_declared = v["params"]["capabilities"]["elicitation"].is_object();
            }
            rpc(v)
        }
    }
}

/// A legacy HTTP+SSE server at `/sse` (which refuses streamable POSTs).
async fn sse_mock() -> (String, Arc<Mutex<SseMock>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let st = Arc::new(Mutex::new(SseMock::default()));
    let st2 = st.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let st = st2.clone();
            tokio::spawn(async move {
                let Some(req) = read_req(&mut sock).await else {
                    return;
                };
                match (req.method.as_str(), req.path.as_str()) {
                    ("GET", "/sse") => {
                        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
                        {
                            let mut s = st.lock().unwrap();
                            s.stream = Some(tx);
                            s.streams += 1;
                        }
                        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\nevent: endpoint\r\ndata: /messages?sessionId=s1\r\n\r\n";
                        if sock.write_all(head.as_bytes()).await.is_err() {
                            return;
                        }
                        while let Some(v) = rx.recv().await {
                            if sock
                                .write_all(format!("event: message\ndata: {v}\n\n").as_bytes())
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                    ("POST", "/messages")
                        if req.query.get("sessionId").map(String::as_str) == Some("s1") =>
                    {
                        let v: Value = serde_json::from_str(&req.body).unwrap_or_default();
                        let _ = sock
                            .write_all(resp("202 Accepted", "", "").as_bytes())
                            .await;
                        let out = sse_rpc(&v, &st);
                        let stream = st.lock().unwrap().stream.clone();
                        if let (Some(out), Some(tx)) = (out, stream) {
                            let _ = tx.send(out);
                        }
                    }
                    _ => {
                        let _ = sock
                            .write_all(resp("405 Method Not Allowed", "", "{}").as_bytes())
                            .await;
                    }
                }
            });
        }
    });
    (base, st)
}

/// The `notifications/cancelled` messages in `msgs`, waiting up to 5s for one.
async fn wait_cancelled(msgs: impl Fn() -> Vec<Value>) -> Vec<Value> {
    for _ in 0..50 {
        let c: Vec<Value> = msgs()
            .into_iter()
            .filter(|m| m["method"] == "notifications/cancelled")
            .collect();
        if !c.is_empty() {
            return c;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    vec![]
}

#[tokio::test]
async fn a_dropped_call_is_cancelled_on_legacy_sse() {
    let (base, st) = sse_mock().await;
    let cfg = McpServerConfig {
        url: Some(format!("{base}/sse")),
        transport: Some("sse".into()),
        ..Default::default()
    };
    let c = McpClient::connect("old", &cfg).await.unwrap();
    st.lock().unwrap().hang = true;
    // The user pressed esc: the call's future is dropped.
    assert!(
        tokio::time::timeout(Duration::from_millis(300), c.call("echo", json!({})))
            .await
            .is_err()
    );
    let cancelled = wait_cancelled(|| st.lock().unwrap().posted.clone()).await;
    let call_id = st
        .lock()
        .unwrap()
        .posted
        .iter()
        .find(|m| m["method"] == "tools/call")
        .unwrap()["id"]
        .clone();
    assert_eq!(cancelled.len(), 1, "{:?}", st.lock().unwrap().posted);
    assert_eq!(
        cancelled[0]["params"],
        json!({"requestId": call_id, "reason": "cancelled by the user"})
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_dropped_call_is_cancelled_on_stdio() {
    let dir = std::env::temp_dir().join(format!(
        "chanto-mcp-cancel-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("got.jsonl");
    // A shell server: answers initialize and tools/list, never tools/call, logs every line.
    let script = format!(
        r#"while IFS= read -r line; do
  printf '%s\n' "$line" >> '{log}'
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
*'"method":"initialize"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"2025-06-18","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"sh"}}}}}}\n' "$id" ;;
*'"method":"tools/list"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":[{{"name":"echo","inputSchema":{{"type":"object"}}}}]}}}}\n' "$id" ;;
*'"method":"tools/call"'*) ;;
*'"id":'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id" ;;
  esac
done"#,
        log = log.display()
    );
    let cfg = McpServerConfig {
        command: "sh".into(),
        args: vec!["-c".into(), script],
        ..Default::default()
    };
    let c = McpClient::connect("sh", &cfg).await.unwrap();
    assert_eq!(c.tool_names(), vec!["mcp__sh__echo"]);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), c.call("echo", json!({})))
            .await
            .is_err()
    );
    let read = || {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect::<Vec<_>>()
    };
    let cancelled = wait_cancelled(read).await;
    let call_id = read().iter().find(|m| m["method"] == "tools/call").unwrap()["id"].clone();
    assert_eq!(cancelled.len(), 1, "{:?}", read());
    assert_eq!(
        cancelled[0]["params"],
        json!({"requestId": call_id, "reason": "cancelled by the user"})
    );
    c.shutdown(Status::Off("test over".into()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn legacy_sse_fallback_and_elicitation() {
    let (base, st) = sse_mock().await;
    // No `transport = "sse"`: the refused POST makes the client fall back.
    let cfg = McpServerConfig {
        url: Some(format!("{base}/sse")),
        ..Default::default()
    };
    let c = McpClient::connect("old", &cfg).await.unwrap();
    assert_eq!(c.status(), Status::Connected);
    assert_eq!(c.tool_names(), vec!["mcp__old__echo"]);
    assert!(st.lock().unwrap().elicitation_declared);
    let all = vec![c.clone()];
    assert_eq!(
        read_mention(&all, "old:docs://readme")
            .await
            .unwrap()
            .unwrap()
            .2,
        "hello resource"
    );

    // No frontend (print mode): the server's question is declined.
    let out = c.call("echo", json!({})).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&out.content).unwrap(),
        json!({"action": "decline"})
    );

    // Interactive: asked one field at a time; a bad number is asked again.
    let fe = Scripted::new(&[
        ("Name", "Ada"),
        ("Age", "abc"),
        ("Age", "36"),
        ("Subscribe", "Yes"),
        ("Colour", "Green"),
        ("Send this", SEND),
    ]);
    c.set_frontend(fe.clone());
    let out = c.call("echo", json!({})).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&out.content).unwrap(),
        json!({"action": "accept", "content": {"name": "Ada", "age": 36, "ok": true, "color": "g"}})
    );
    let asked = fe.asked.lock().unwrap().clone();
    // Four fields (one asked twice), then the review.
    assert_eq!(asked.len(), 6);
    assert!(
        asked
            .iter()
            .all(|q| q.contains("MCP server `old` asks: Who are you?"))
    );
    assert!(
        asked
            .iter()
            .any(|q| q.contains("isn't valid: enter a whole number"))
    );

    // Configured explicitly: straight to the event stream.
    let sse_cfg = McpServerConfig {
        transport: Some("sse".into()),
        ..cfg
    };
    let c2 = McpClient::connect("old2", &sse_cfg).await.unwrap();
    assert_eq!(c2.tool_names(), vec!["mcp__old2__echo"]);
    assert_eq!(st.lock().unwrap().streams, 2);
}

#[test]
fn mcp_json_type_sse() {
    let s: McpServerConfig =
        serde_json::from_value(json!({"type": "sse", "url": "https://x/sse"})).unwrap();
    assert_eq!(s.transport.as_deref(), Some("sse"));
}

#[test]
fn prompt_arg_mapping() {
    let def = McpPromptDef {
        name: "p".into(),
        description: String::new(),
        arguments: vec![("a".into(), true), ("b".into(), false)],
    };
    let m = prompt_args(&def, "x y z").unwrap();
    assert_eq!((m["a"].as_str(), m["b"].as_str()), (Some("x"), Some("y z")));
    let m = prompt_args(&def, "\"one two\"").unwrap();
    assert_eq!(m["a"], "one two");
    assert!(!m.contains_key("b"));
    assert!(prompt_args(&def, "").is_err());
}

fn vars(name: &str) -> Option<String> {
    match name {
        "HOME_DIR" => Some("/home/me".into()),
        "TOKEN" => Some("s3cret".into()),
        "EMPTY" => Some(String::new()),
        _ => None,
    }
}

#[test]
fn env_expansion_forms() {
    let mut used = vec![];
    assert_eq!(
        expand_vars("${HOME_DIR}/bin", &vars, &mut used).unwrap(),
        "/home/me/bin"
    );
    assert_eq!(
        expand_vars("${NOPE:-fallback}", &vars, &mut used).unwrap(),
        "fallback"
    );
    assert_eq!(
        expand_vars("${TOKEN:-x}", &vars, &mut used).unwrap(),
        "s3cret"
    );
    assert_eq!(
        expand_vars("${EMPTY:-d}|${EMPTY}", &vars, &mut used).unwrap(),
        "d|"
    );
    assert_eq!(expand_vars("${NOPE:-}", &vars, &mut used).unwrap(), "");
    assert_eq!(
        expand_vars("$HOME_DIR plain {x}", &vars, &mut used).unwrap(),
        "$HOME_DIR plain {x}"
    );
    assert_eq!(used, vec!["HOME_DIR", "NOPE", "TOKEN", "EMPTY"]);
    assert_eq!(
        expand_vars("a ${MISSING} b", &vars, &mut used)
            .unwrap_err()
            .0,
        "MISSING"
    );
}

#[test]
fn env_expansion_in_every_field() {
    let cfg: McpServerConfig = serde_json::from_value(json!({
        "command": "${HOME_DIR}/bin/srv",
        "args": ["--root", "${ROOT:-/tmp}"],
        "env": {"API_TOKEN": "${TOKEN}"},
        "url": "https://${HOST:-example.com}/mcp",
        "headers": {"Authorization": "Bearer ${TOKEN}"},
        "cwd": "${HOME_DIR}",
    }))
    .unwrap();
    let (out, used) = expand_config(&cfg, &vars).unwrap();
    assert_eq!(out.command, "/home/me/bin/srv");
    assert_eq!(out.args, vec!["--root", "/tmp"]);
    assert_eq!(out.env["API_TOKEN"], "s3cret");
    assert_eq!(out.url.as_deref(), Some("https://example.com/mcp"));
    assert_eq!(out.headers["Authorization"], "Bearer s3cret");
    assert_eq!(out.cwd.as_deref(), Some("/home/me"));
    assert!(used.contains(&"TOKEN".to_string()));
    // A missing variable without a default names itself; nothing is sent.
    let bad = McpServerConfig {
        headers: [("X-Key".to_string(), "${NO_SUCH_KEY}".to_string())].into(),
        ..cfg
    };
    assert_eq!(expand_config(&bad, &vars).unwrap_err().0, "NO_SUCH_KEY");
}

#[tokio::test]
async fn missing_env_skips_server_with_warning() {
    let cfg = McpServerConfig {
        command: "definitely-not-run".into(),
        args: vec!["${CHANTO_TEST_UNSET_VAR_7F3A}".into()],
        ..Default::default()
    };
    let servers: std::collections::BTreeMap<String, McpServerConfig> =
        [("srv".to_string(), cfg)].into();
    let (ok, errs) = connect_all(&servers).await;
    assert!(
        ok.iter().all(|c| matches!(c.status(), Status::Failed(_))),
        "listed, but not connected"
    );
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(
        errs[0].contains("`srv`") && errs[0].contains("CHANTO_TEST_UNSET_VAR_7F3A"),
        "{errs:?}"
    );
}

#[tokio::test]
async fn offline_clients_reconnect_and_shut_down() {
    let cfg = McpServerConfig {
        command: "definitely-not-a-real-mcp-server-7f3a".into(),
        ..Default::default()
    };
    let c = McpClient::offline("srv", &cfg, Status::Off("disabled".into()));
    assert!(c.tools().is_empty() && !c.is_connected());
    assert!(
        c.call("x", json!({})).await.is_err(),
        "no transport: calls fail cleanly"
    );
    assert!(c.reconnect().await.is_err());
    assert!(matches!(c.status(), Status::Failed(_)), "{:?}", c.status());
    c.shutdown(Status::Off("disabled for this project".into()));
    assert_eq!(c.status(), Status::Off("disabled for this project".into()));
    let servers: std::collections::BTreeMap<String, McpServerConfig> = [(
        "off".to_string(),
        McpServerConfig {
            gate: Some(crate::mcp_policy::Gate::Disabled),
            ..cfg.clone()
        },
    )]
    .into();
    let (ok, errs) = connect_all(&servers).await;
    assert!(errs.is_empty(), "{errs:?}");
    assert!(
        matches!(ok[0].status(), Status::Off(_)),
        "gated servers are listed but never started"
    );
}

#[tokio::test]
async fn a_dropped_connection_shows_as_failed() {
    let cfg = McpServerConfig {
        command: "definitely-not-a-real-mcp-server-7f3a".into(),
        ..Default::default()
    };
    // No transport behind a connected status: the next call finds the
    // connection gone, which `/mcp` and the footer must stop calling connected.
    let c = McpClient::offline("srv", &cfg, Status::Connected);
    let e = c.call("x", json!({})).await.unwrap_err();
    assert!(matches!(c.status(), Status::Failed(_)), "{:?}", c.status());
    assert!(format!("{e:#}").contains("`/mcp reconnect srv`"), "{e:#}");
    // A server the user turned off keeps the reason it was turned off.
    let off = McpClient::offline("srv", &cfg, Status::Off("disabled".into()));
    assert!(off.call("x", json!({})).await.is_err());
    assert_eq!(off.status(), Status::Off("disabled".into()));
}

#[test]
fn referenced_secrets_reach_the_server() {
    let sc = crate::secrets::SecretsConfig::default();
    let cfg = McpServerConfig {
        env_vars: vec!["GITHUB_TOKEN".into()],
        ..Default::default()
    };
    assert!(withhold_from_server("OPENAI_API_KEY", &cfg, &[], &sc));
    assert!(!withhold_from_server(
        "OPENAI_API_KEY",
        &cfg,
        &["OPENAI_API_KEY".to_string()],
        &sc
    ));
    assert!(!withhold_from_server("GITHUB_TOKEN", &cfg, &[], &sc));
    assert!(!withhold_from_server("PATH", &cfg, &[], &sc));
}

#[test]
fn mcp_tool_timeout_env_is_milliseconds() {
    assert_eq!(default_tool_timeout(None), TOOL_TIMEOUT_DEFAULT);
    assert_eq!(default_tool_timeout(Some("90000")), Duration::from_secs(90));
    assert_eq!(default_tool_timeout(Some("nope")), TOOL_TIMEOUT_DEFAULT);
    assert_eq!(default_tool_timeout(Some("0")), TOOL_TIMEOUT_DEFAULT);
}

#[test]
fn tool_filters() {
    let enabled = vec!["read".to_string(), "write".to_string()];
    let disabled = vec!["write".to_string()];
    assert!(tool_allowed(Some(enabled.as_slice()), &disabled, "read"));
    assert!(!tool_allowed(Some(enabled.as_slice()), &disabled, "write"));
    assert!(!tool_allowed(Some(enabled.as_slice()), &[], "delete"));
    assert!(tool_allowed(None, &[], "delete"));
    assert!(!tool_allowed(None, &disabled, "write"));
}

#[test]
fn a_remote_servers_read_only_hint_doesnt_skip_approval() {
    let def = McpToolDef {
        name: "send_email".into(),
        description: "Send an email".into(),
        schema: json!({"type": "object"}),
        read_only: true,
        always_load: None,
    };
    let client = McpClient::with_tools_for_test("remote", vec![def], None);
    assert!(client.is_http());
    let tool = client.tool("mcp__remote__send_email").expect("tool");
    assert_eq!(tool.kind(), ToolKind::External);
}

#[test]
fn a_local_servers_read_only_hint_is_only_advisory() {
    // A stdio server's "read-only" fetch or search could still carry data
    // out: the hint doesn't make the tool ReadOnly (which runs unasked).
    let def = McpToolDef {
        name: "fetch".into(),
        description: "Fetch a URL".into(),
        schema: json!({"type": "object"}),
        read_only: true,
        always_load: None,
    };
    let client = McpClient::with_tools_for_test_at("local", vec![def], None, None);
    assert!(!client.is_http());
    let tool = client.tool("mcp__local__fetch").expect("tool");
    assert_eq!(tool.kind(), ToolKind::External);
    assert!(tool.read_only_hint());
    for mode in [
        crate::permissions::Mode::Default,
        crate::permissions::Mode::AcceptEdits,
    ] {
        assert_eq!(
            crate::permissions::static_decision(
                mode,
                tool.kind(),
                &tool.name(),
                &json!({"url": "https://example.com"}),
                std::path::Path::new("/tmp"),
            ),
            Some(crate::permissions::Decision::Ask),
            "{mode:?}"
        );
    }
}
