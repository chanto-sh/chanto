//! Bedrock, Vertex AI and Azure clients against local mock servers, with
//! credentials from hand-built environments (never the real home or clouds).
//! `live_cloud_smoke` talks to the real clouds and only runs with
//! `CHANTO_LIVE_CLOUD=1`.

use super::mock::{serve, serve_bytes, sse};
use super::*;
use crate::cloud::{Settings, Vars, eventstream, sigv4};
use base64::Engine;

fn tool() -> ToolSpec {
    ToolSpec {
        name: "Read".into(),
        description: "read".into(),
        parameters: json!({"type": "object", "properties": {"file_path": {"type": "string"}}}),
    }
}

fn req(model: &str, messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages,
        tools: vec![tool()],
        think: Some(true),
        effort: None,
        temperature: None,
        max_tokens: Some(4000),
        json: false,
    }
}

fn tmp() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("chanto-cloudllm-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn aws_vars() -> Vars {
    Vars::new(
        "/nonexistent",
        &[
            ("AWS_ACCESS_KEY_ID", "AKIDTEST"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
            ("AWS_SESSION_TOKEN", "session"),
        ],
    )
}

/// A Bedrock InvokeModel stream chunk wrapping an Anthropic event.
fn chunk(event: Value) -> Vec<u8> {
    let payload = json!({"bytes": base64::engine::general_purpose::STANDARD.encode(event.to_string()), "p": "abcd"});
    eventstream::encode(
        &[
            (":event-type", "chunk"),
            (":content-type", "application/json"),
            (":message-type", "event"),
        ],
        payload.to_string().as_bytes(),
    )
}

fn converse_event(kind: &str, payload: Value) -> Vec<u8> {
    eventstream::encode(
        &[
            (":event-type", kind),
            (":content-type", "application/json"),
            (":message-type", "event"),
        ],
        payload.to_string().as_bytes(),
    )
}

fn header<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|l| {
        l.split_once(':')
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim())
    })
}

fn claude_events() -> Vec<Value> {
    vec![
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 12, "cache_read_input_tokens": 100, "cache_creation_input_tokens": 0, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Reading."}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_9", "name": "Read", "input": {}}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"file_path\": "}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "\"a.txt\"}"}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 30}}),
        json!({"type": "message_stop", "amazon-bedrock-invocationMetrics": {"inputTokenCount": 12}}),
    ]
}

fn check_claude(r: &ChatResponse) {
    assert_eq!(r.text, "Reading.");
    assert_eq!(r.tool_calls[0].id, "toolu_9");
    assert_eq!(r.tool_calls[0].arguments["file_path"], "a.txt");
    assert_eq!(r.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(
        (
            r.usage.prompt_tokens,
            r.usage.cached_tokens,
            r.usage.completion_tokens
        ),
        (112, 100, 30)
    );
}

#[tokio::test]
async fn bedrock_claude_streams_event_frames_with_sigv4() {
    let body: Vec<u8> = claude_events().into_iter().flat_map(chunk).collect();
    let (base, seen) = serve_bytes(vec![(200, "application/vnd.amazon.eventstream", body)]).await;
    let model = "us.anthropic.claude-sonnet-4-5-20250929-v1:0";
    let p = Provider::new(ProviderKind::Bedrock, &base, None, 200_000)
        .with_cloud(Settings {
            region: Some("us-west-2".into()),
            ..Default::default()
        })
        .with_vars(aws_vars());
    let r = p
        .chat(
            &req(
                model,
                vec![Message::system("Be terse."), Message::user("read a.txt")],
            ),
            &mut |_| {},
        )
        .await
        .unwrap();
    check_claude(&r);

    let s = seen.lock().unwrap();
    let path = "/model/us.anthropic.claude-sonnet-4-5-20250929-v1%3A0/invoke-with-response-stream";
    assert!(
        s.headers[0].starts_with(&format!("POST {path} HTTP/1.1")),
        "{}",
        s.headers[0]
    );
    let b = &s.bodies[0];
    assert_eq!(b["anthropic_version"], "bedrock-2023-05-31");
    assert!(b.get("model").is_none() && b.get("stream").is_none());
    assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(b["tools"][0]["name"], "Read");
    // Sonnet 4.5 takes a thinking budget.
    assert_eq!(b["thinking"]["type"], "enabled");
    // The signature covers exactly the bytes sent.
    let h = &s.headers[0];
    assert_eq!(header(h, "x-amz-security-token"), Some("session"));
    let time =
        chrono::NaiveDateTime::parse_from_str(header(h, "x-amz-date").unwrap(), "%Y%m%dT%H%M%SZ")
            .unwrap()
            .and_utc();
    let creds = sigv4::Credentials {
        access_key_id: "AKIDTEST".into(),
        secret_access_key: "secret".into(),
        session_token: Some("session".into()),
    };
    let url = reqwest::Url::parse(&format!("{base}{path}")).unwrap();
    let expect = sigv4::sign(
        "POST",
        &url,
        &[("content-type", "application/json")],
        &s.raw[0],
        &creds,
        "us-west-2",
        "bedrock",
        time,
    );
    assert_eq!(header(h, "authorization"), Some(expect[2].1.as_str()));
    assert!(expect[2].1.contains("/us-west-2/bedrock/aws4_request"));
}

#[tokio::test]
async fn bedrock_api_key_and_stream_exceptions() {
    let mut body = chunk(claude_events()[0].clone());
    body.extend(eventstream::encode(
        &[
            (":message-type", "exception"),
            (":exception-type", "throttlingException"),
        ],
        br#"{"message":"Too many tokens, please wait"}"#,
    ));
    let (base, seen) = serve_bytes(vec![
        (200, "application/vnd.amazon.eventstream", body),
        (
            403,
            "application/json",
            br#"{"message":"You don't have access to the model"}"#.to_vec(),
        ),
    ])
    .await;
    // A Bedrock API key: bearer auth, nothing signed.
    let p = Provider::new(
        ProviderKind::Bedrock,
        &base,
        Some("bedrock-api-key".into()),
        200_000,
    )
    .with_vars(Vars::new("/nonexistent", &[("AWS_REGION", "eu-central-1")]));
    let e = p
        .chat(
            &req(
                "eu.anthropic.claude-haiku-4-5-20251001-v1:0",
                vec![Message::user("hi")],
            ),
            &mut |_| {},
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("throttlingException: Too many tokens"), "{e}");
    let e = p
        .chat(
            &req(
                "eu.anthropic.claude-haiku-4-5-20251001-v1:0",
                vec![Message::user("hi")],
            ),
            &mut |_| {},
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("403")
            && e.contains("bedrock:InvokeModelWithResponseStream")
            && e.contains("region eu-central-1"),
        "{e}"
    );
    let s = seen.lock().unwrap();
    assert_eq!(
        header(&s.headers[0], "authorization"),
        Some("Bearer bedrock-api-key")
    );
    assert!(header(&s.headers[0], "x-amz-date").is_none());
    // No region anywhere is a clear error before any request.
    let p = Provider::new(ProviderKind::Bedrock, &base, None, 200_000).with_vars(aws_vars());
    let e = p
        .chat(
            &req("anthropic.claude-x", vec![Message::user("hi")]),
            &mut |_| {},
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("no AWS region"), "{e}");
}

#[tokio::test]
async fn bedrock_converse_maps_tools_and_streams() {
    let frames = [
        converse_event("messageStart", json!({"role": "assistant"})),
        converse_event(
            "contentBlockDelta",
            json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"text": "think"}}}),
        ),
        converse_event(
            "contentBlockDelta",
            json!({"contentBlockIndex": 1, "delta": {"text": "Let me look."}}),
        ),
        converse_event(
            "contentBlockStart",
            json!({"contentBlockIndex": 2, "start": {"toolUse": {"toolUseId": "tooluse_1", "name": "Read"}}}),
        ),
        converse_event(
            "contentBlockDelta",
            json!({"contentBlockIndex": 2, "delta": {"toolUse": {"input": "{\"file_path\":"}}}),
        ),
        converse_event(
            "contentBlockDelta",
            json!({"contentBlockIndex": 2, "delta": {"toolUse": {"input": "\"b.rs\"}"}}}),
        ),
        converse_event("contentBlockStop", json!({"contentBlockIndex": 2})),
        converse_event("messageStop", json!({"stopReason": "tool_use"})),
        converse_event(
            "metadata",
            json!({"usage": {"inputTokens": 50, "outputTokens": 9, "totalTokens": 59}, "metrics": {"latencyMs": 1}}),
        ),
    ];
    let (base, seen) = serve_bytes(vec![(
        200,
        "application/vnd.amazon.eventstream",
        frames.concat(),
    )])
    .await;
    let p = Provider::new(ProviderKind::Bedrock, &base, None, 128_000).with_vars(Vars::new(
        "/nonexistent",
        &[
            ("AWS_REGION", "us-east-1"),
            ("AWS_ACCESS_KEY_ID", "a"),
            ("AWS_SECRET_ACCESS_KEY", "b"),
        ],
    ));
    let call = ToolCall {
        id: "call.1".into(),
        name: "Read".into(),
        arguments: json!({"file_path": "a.rs"}),
    };
    let mut a = Message::assistant("");
    a.tool_calls = vec![call.clone()];
    let mut img = Message::user("and this");
    img.images = vec!["iVBORw0KGgo".into()];
    let messages = vec![
        Message::system("sys"),
        Message::user("read a.rs"),
        a,
        Message::tool(&call, "fn a() {}"),
        img,
    ];
    let mut thoughts = String::new();
    let r = p
        .chat(&req("openai.gpt-oss-120b-1:0", messages), &mut |d| {
            if let Delta::Reasoning(t) = d {
                thoughts.push_str(&t)
            }
        })
        .await
        .unwrap();
    assert_eq!(
        (r.text.as_str(), thoughts.as_str()),
        ("Let me look.", "think")
    );
    assert_eq!(
        (r.tool_calls[0].id.as_str(), r.tool_calls[0].name.as_str()),
        ("tooluse_1", "Read")
    );
    assert_eq!(r.tool_calls[0].arguments["file_path"], "b.rs");
    assert_eq!(r.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!((r.usage.prompt_tokens, r.usage.completion_tokens), (50, 9));

    let s = seen.lock().unwrap();
    assert!(
        s.headers[0].starts_with("POST /model/openai.gpt-oss-120b-1%3A0/converse-stream "),
        "{}",
        s.headers[0]
    );
    let b = &s.bodies[0];
    assert_eq!(b["system"], json!([{"text": "sys"}]));
    assert_eq!(b["inferenceConfig"]["maxTokens"], 4000);
    assert_eq!(
        b["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"]["type"],
        "object"
    );
    let m = b["messages"].as_array().unwrap();
    assert_eq!(
        m.iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["user", "assistant", "user"]
    );
    assert_eq!(
        m[1]["content"][0]["toolUse"],
        json!({"toolUseId": "call_1", "name": "Read", "input": {"file_path": "a.rs"}})
    );
    // The tool result comes first in the merged user turn, then the new text and image.
    assert_eq!(m[2]["content"][0]["toolResult"]["toolUseId"], "call_1");
    assert_eq!(
        m[2]["content"][0]["toolResult"]["content"][0]["text"],
        "fn a() {}"
    );
    assert_eq!(m[2]["content"][1]["text"], "and this");
    assert_eq!(m[2]["content"][2]["image"]["format"], "png");
}

#[test]
fn converse_without_tools_replays_tool_turns_as_text() {
    let call = ToolCall {
        id: "c1".into(),
        name: "Bash".into(),
        arguments: json!({"command": "ls"}),
    };
    let mut a = Message::assistant("");
    a.tool_calls = vec![call.clone()];
    let mut r = req(
        "meta.llama3-3-70b-instruct-v1:0",
        vec![a, Message::tool(&call, "x.rs"), Message::user("summarize")],
    );
    r.tools.clear();
    r.temperature = Some(0.2);
    let b = bedrock::converse_body(&r);
    assert!(b.get("toolConfig").is_none());
    assert_eq!(
        b["inferenceConfig"]["temperature"]
            .as_f64()
            .map(|t| (t * 10.).round()),
        Some(2.)
    );
    let m = b["messages"].as_array().unwrap();
    // Opens with a user turn, alternates, and has no tool blocks.
    assert_eq!(
        m.iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["user", "assistant", "user"]
    );
    assert!(
        m[1]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("called Bash")
    );
    assert!(
        m[2]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("[Bash result]\nx.rs")
    );
    assert!(!b.to_string().contains("toolUse"));
}

/// An authorized-user ADC file whose token endpoint is `base`.
fn adc(home: &std::path::Path, base: &str) -> Vars {
    let f = home.join("adc.json");
    std::fs::write(&f, format!(r#"{{"type":"authorized_user","client_id":"c","client_secret":"s","refresh_token":"r","token_uri":"{base}/token"}}"#)).unwrap();
    Vars::new(
        home,
        &[("GOOGLE_APPLICATION_CREDENTIALS", f.to_str().unwrap())],
    )
}

#[tokio::test]
async fn vertex_claude_uses_stream_raw_predict() {
    let home = tmp();
    let events: Vec<(&str, Value)> = claude_events().into_iter().map(|e| ("x", e)).collect();
    let (base, seen) = serve(vec![
        (
            200,
            r#"{"access_token":"ya29.vertex","expires_in":3600}"#.into(),
        ),
        (200, sse(&events)),
        (200, sse(&events)),
    ])
    .await;
    let p = Provider::new(ProviderKind::Vertex, &base, None, 200_000)
        .with_cloud(Settings {
            project: Some("proj-1".into()),
            region: Some("us-east5".into()),
            ..Default::default()
        })
        .with_vars(adc(&home, &base));
    let model = "claude-sonnet-4-5@20250929";
    check_claude(
        &p.chat(&req(model, vec![Message::user("hi")]), &mut |_| {})
            .await
            .unwrap(),
    );
    check_claude(
        &p.chat(&req(model, vec![Message::user("again")]), &mut |_| {})
            .await
            .unwrap(),
    );
    let s = seen.lock().unwrap();
    // One token request, reused for both calls.
    assert_eq!(s.headers.len(), 3);
    assert!(s.headers[0].starts_with("POST /token "));
    for h in &s.headers[1..] {
        assert!(h.starts_with("POST /v1/projects/proj-1/locations/us-east5/publishers/anthropic/models/claude-sonnet-4-5@20250929:streamRawPredict "), "{h}");
        assert_eq!(header(h, "authorization"), Some("Bearer ya29.vertex"));
    }
    let b = &s.bodies[1];
    assert_eq!(b["anthropic_version"], "vertex-2023-10-16");
    assert!(b.get("model").is_none());
    assert_eq!(b["stream"], true);
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
async fn vertex_gemini_uses_the_openai_endpoint_and_replays_thought_signatures() {
    let home = tmp();
    let call = json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "fc_1", "type": "function", "function": {"name": "Read", "arguments": "{\"file_path\":\"c.rs\"}"}, "extra_content": {"google": {"thought_signature": "sig=="}}}]}, "finish_reason": "tool_calls"}]});
    let first = format!(
        "data: {call}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [], "usage": {"prompt_tokens": 7, "completion_tokens": 3}})
    );
    let second = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]})
    );
    let (base, seen) = serve(vec![
        (200, r#"{"access_token":"ya29.g","expires_in":3600}"#.into()),
        (200, first),
        (200, second),
    ])
    .await;
    let p = Provider::new(ProviderKind::Vertex, &base, None, 1_000_000)
        .with_cloud(Settings {
            project: Some("p2".into()),
            ..Default::default()
        })
        .with_vars(adc(&home, &base));
    let mut r1 = req("gemini-3-pro-preview", vec![Message::user("read c.rs")]);
    r1.think = Some(false);
    let r = p.chat(&r1, &mut |_| {}).await.unwrap();
    assert_eq!(r.tool_calls[0].arguments["file_path"], "c.rs");
    let mut a = Message::assistant("");
    a.tool_calls = r.tool_calls.clone();
    a.provider_items = r.provider_items.clone();
    r1.messages
        .extend([a, Message::tool(&r.tool_calls[0], "data")]);
    assert_eq!(p.chat(&r1, &mut |_| {}).await.unwrap().text, "ok");
    let s = seen.lock().unwrap();
    assert!(
        s.headers[1].starts_with(
            "POST /v1/projects/p2/locations/global/endpoints/openapi/chat/completions "
        ),
        "{}",
        s.headers[1]
    );
    let b = &s.bodies[1];
    assert_eq!(b["model"], "google/gemini-3-pro-preview");
    // Cloud APIs reject fields they don't know.
    assert!(b.get("chat_template_kwargs").is_none() && b.get("reasoning_effort").is_none());
    assert_eq!(
        s.bodies[2]["messages"][1]["tool_calls"][0]["extra_content"]["google"]["thought_signature"],
        "sig=="
    );
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
async fn azure_chat_completions_and_responses() {
    let chat = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {"content": "hello"}, "finish_reason": "stop"}]})
    );
    let responses = sse(&[
        (
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "delta": "patched"}),
        ),
        (
            "response.completed",
            json!({"type": "response.completed", "response": {"usage": {"input_tokens": 5, "output_tokens": 2}}}),
        ),
    ]);
    let (base, seen) = serve(vec![(200, chat), (200, responses)]).await;
    let vars = Vars::new("/nonexistent", &[]);
    let p = Provider::new(ProviderKind::Azure, &base, Some("az-key".into()), 400_000)
        .with_vars(vars.clone());
    assert_eq!(
        p.chat(&req("prod-gpt-5", vec![Message::user("hi")]), &mut |_| {})
            .await
            .unwrap()
            .text,
        "hello"
    );
    let p = p.with_cloud(Settings {
        api_version: Some("preview".into()),
        ..Default::default()
    });
    assert_eq!(
        p.chat(&req("gpt-5-codex", vec![Message::user("fix")]), &mut |_| {})
            .await
            .unwrap()
            .text,
        "patched"
    );
    let s = seen.lock().unwrap();
    assert!(
        s.headers[0].starts_with("POST /openai/v1/chat/completions "),
        "{}",
        s.headers[0]
    );
    assert_eq!(header(&s.headers[0], "api-key"), Some("az-key"));
    assert!(header(&s.headers[0], "authorization").is_none());
    assert_eq!(s.bodies[0]["model"], "prod-gpt-5");
    assert_eq!(s.bodies[0]["max_completion_tokens"], 4000);
    assert!(s.bodies[0].get("max_tokens").is_none());
    assert!(
        s.headers[1].starts_with("POST /openai/v1/responses?api-version=preview "),
        "{}",
        s.headers[1]
    );
    assert_eq!(s.bodies[1]["store"], false);
}

#[tokio::test]
async fn foundry_claude_uses_anthropic_messages() {
    let events: Vec<(&str, Value)> = claude_events().into_iter().map(|e| ("x", e)).collect();
    let (base, seen) = serve(vec![(200, sse(&events))]).await;
    let p = Provider::new(ProviderKind::Foundry, &base, None, 200_000).with_vars(Vars::new(
        "/nonexistent",
        &[("ANTHROPIC_FOUNDRY_API_KEY", "f-key")],
    ));
    check_claude(
        &p.chat(
            &req("claude-opus-4-5", vec![Message::user("hi")]),
            &mut |_| {},
        )
        .await
        .unwrap(),
    );
    let s = seen.lock().unwrap();
    assert!(
        s.headers[0].starts_with("POST /anthropic/v1/messages "),
        "{}",
        s.headers[0]
    );
    assert_eq!(header(&s.headers[0], "x-api-key"), Some("f-key"));
    assert_eq!(
        header(&s.headers[0], "anthropic-version"),
        Some("2023-06-01")
    );
    assert_eq!(s.bodies[0]["model"], "claude-opus-4-5");
}

#[tokio::test]
async fn cloud_check_reports_sources_without_secrets() {
    let (base, _) = serve_bytes(vec![
        (
            200,
            "application/json",
            br#"{"modelSummaries":[]}"#.to_vec(),
        ),
        (
            403,
            "application/json",
            br#"{"message":"User is not authorized to perform: bedrock:ListFoundationModels"}"#
                .to_vec(),
        ),
    ])
    .await;
    let p = Provider::new(ProviderKind::Bedrock, &base, None, 200_000).with_vars(Vars::new(
        "/nonexistent",
        &[
            ("AWS_REGION", "us-east-2"),
            ("AWS_ACCESS_KEY_ID", "AKSECRETVALUE"),
            ("AWS_SECRET_ACCESS_KEY", "verysecret"),
        ],
    ));
    let c = p
        .cloud_check("us.anthropic.claude-sonnet-4-5-20250929-v1:0")
        .await;
    assert!(
        c.reachable && c.note == "credentials accepted",
        "{}",
        c.note
    );
    assert_eq!(c.credential.as_deref(), Some("env:AWS_ACCESS_KEY_ID"));
    assert!(
        c.details
            .contains(&("region".into(), "us-east-2 (env:AWS_REGION)".into()))
    );
    let c = p.cloud_check("openai.gpt-oss-120b-1:0").await;
    assert!(
        c.reachable && c.note.contains("permission") && c.note.contains("ListFoundationModels"),
        "{}",
        c.note
    );
    assert!(c.details.contains(&("api".into(), "Converse".into())));
    let all = format!("{:?} {:?} {}", c.credential, c.details, c.note);
    assert!(!all.contains("AKSECRETVALUE") && !all.contains("verysecret"));
    // Missing configuration is reported, not panicked on.
    let p = Provider::new(ProviderKind::Vertex, "", None, 200_000)
        .with_vars(Vars::new("/nonexistent", &[]));
    let c = p.cloud_check("gemini-2.5-pro").await;
    assert!(
        !c.reachable && c.note.contains("GOOGLE_CLOUD_PROJECT"),
        "{}",
        c.note
    );
}

/// Real requests to the clouds, with the real environment and credentials.
/// Skipped unless `CHANTO_LIVE_CLOUD=1`; each cloud runs when its model is set:
///
/// ```sh
/// CHANTO_LIVE_CLOUD=1 \
///   CHANTO_LIVE_BEDROCK_MODEL=us.anthropic.claude-haiku-4-5-20251001-v1:0 \
///   CHANTO_LIVE_VERTEX_MODEL=gemini-2.5-flash \
///   CHANTO_LIVE_AZURE_MODEL=<deployment> \
///   cargo test live_cloud_smoke -- --nocapture
/// ```
#[tokio::test]
async fn live_cloud_smoke() {
    if std::env::var("CHANTO_LIVE_CLOUD").as_deref() != Ok("1") {
        return;
    }
    let mut ran = 0;
    for (var, id) in [
        ("CHANTO_LIVE_BEDROCK_MODEL", "bedrock"),
        ("CHANTO_LIVE_VERTEX_MODEL", "vertex"),
        ("CHANTO_LIVE_AZURE_MODEL", "azure"),
        ("CHANTO_LIVE_FOUNDRY_MODEL", "foundry"),
    ] {
        let Ok(model) = std::env::var(var) else {
            continue;
        };
        let p = builtin_provider(id, &model).unwrap();
        let check = p.cloud_check(&model).await;
        eprintln!(
            "{id}: credential {:?}, {:?}, {}",
            check.credential, check.details, check.note
        );
        let mut r = req(
            &model,
            vec![Message::user("Reply with exactly the word OK.")],
        );
        r.max_tokens = Some(2048);
        r.think = Some(false);
        let out = p
            .chat(&r, &mut |_| {})
            .await
            .unwrap_or_else(|e| panic!("{id}/{model}: {e:#}"));
        eprintln!(
            "{id}/{model}: {:?} ({} in, {} out)",
            out.text, out.usage.prompt_tokens, out.usage.completion_tokens
        );
        assert!(
            out.text.to_ascii_uppercase().contains("OK"),
            "{id}: {:?}",
            out.text
        );
        ran += 1;
    }
    assert!(
        ran > 0,
        "set CHANTO_LIVE_BEDROCK_MODEL, CHANTO_LIVE_VERTEX_MODEL, CHANTO_LIVE_AZURE_MODEL or CHANTO_LIVE_FOUNDRY_MODEL"
    );
}
