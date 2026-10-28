//! Tests for the Anthropic and OpenAI Responses clients against mock SSE servers.

use super::mock::{serve, sse};
use super::*;

fn read_tool() -> ToolSpec {
    ToolSpec {
        name: "Read".into(),
        description: "read".into(),
        parameters: json!({"type": "object", "properties": {"file_path": {"type": "string"}}}),
    }
}

#[tokio::test]
async fn anthropic_tool_round_trip_with_thinking_and_caching() {
    let first = sse(&[
        (
            "message_start",
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 10, "cache_read_input_tokens": 90, "cache_creation_input_tokens": 5, "output_tokens": 1}}}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "plan"}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig123"}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Reading."}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 1}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {}}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"file_"}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "path\": \"a.txt\"}"}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 2}),
        ),
        (
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 20}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ]);
    let second = sse(&[
        (
            "message_start",
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 3, "output_tokens": 1}}}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Done"}}),
        ),
        (
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}, "usage": {"output_tokens": 2}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ]);
    // An overloaded response first: retried before anything streams.
    let overloaded = "{\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}".to_string();
    let (base, seen) = serve(vec![(529, overloaded), (200, first), (200, second)]).await;
    let p = Provider::new(
        ProviderKind::Anthropic,
        &base,
        Some("k-test".into()),
        1_000_000,
    );
    let mut msgs = vec![
        Message::system("You are terse."),
        Message::user("read a.txt"),
    ];
    let mut req = ChatRequest {
        model: "claude-opus-5-5".into(),
        messages: msgs.clone(),
        tools: vec![read_tool()],
        think: Some(true),
        effort: None,
        temperature: Some(0.2),
        max_tokens: Some(16000),
        json: false,
    };
    let mut thoughts = String::new();
    let r = p
        .chat(&req, &mut |d| {
            if let Delta::Reasoning(t) = d {
                thoughts.push_str(&t)
            }
        })
        .await
        .unwrap();
    assert_eq!(r.text, "Reading.");
    assert_eq!(thoughts, "plan");
    assert_eq!(r.tool_calls[0].id, "toolu_1");
    assert_eq!(r.tool_calls[0].arguments["file_path"], "a.txt");
    assert_eq!(
        (r.usage.prompt_tokens, r.usage.completion_tokens),
        (105, 20)
    );
    assert_eq!(r.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(
        r.provider_items.as_ref().unwrap().items[0]["signature"],
        "sig123"
    );

    let mut a = Message::assistant(r.text.clone());
    a.tool_calls = r.tool_calls.clone();
    a.provider_items = r.provider_items.clone();
    msgs.push(a);
    msgs.push(Message::tool(&r.tool_calls[0], "hello"));
    req.messages = msgs;
    let r2 = p.chat(&req, &mut |_| {}).await.unwrap();
    assert_eq!(r2.text, "Done");
    assert_eq!(r2.finish_reason.as_deref(), Some("length"));
    assert!(r2.provider_items.is_none());

    let seen = seen.lock().unwrap();
    assert_eq!(seen.bodies.len(), 3);
    let h = seen.headers[1].to_ascii_lowercase();
    assert!(h.contains("x-api-key: k-test") && h.contains("anthropic-version: 2023-06-01"));
    let b = &seen.bodies[2];
    assert_eq!(b["system"][0]["text"], "You are terse.");
    assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(b["tools"][0]["input_schema"]["type"], "object");
    assert_eq!(b["tools"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(b["thinking"]["type"], "adaptive");
    assert!(b.get("temperature").is_none());
    let m = b["messages"].as_array().unwrap();
    assert_eq!(m.len(), 3);
    // The assistant turn is replayed verbatim, thinking signature included.
    assert_eq!(m[1]["content"][0]["type"], "thinking");
    assert_eq!(m[1]["content"][0]["signature"], "sig123");
    assert_eq!(m[1]["content"][2]["input"]["file_path"], "a.txt");
    assert_eq!(m[2]["content"][0]["type"], "tool_result");
    assert_eq!(m[2]["content"][0]["tool_use_id"], "toolu_1");
    assert_eq!(m[2]["content"][0]["content"], "hello");
    assert_eq!(m[2]["content"][0]["cache_control"]["type"], "ephemeral");
    // Only the last user turn carries the rolling breakpoint.
    assert!(m[0]["content"][0].get("cache_control").is_none());
}

#[test]
fn anthropic_body_reconstructs_and_merges() {
    let call = ToolCall {
        id: "call.1".into(),
        name: "Bash".into(),
        arguments: json!({"command": "ls"}),
    };
    let mut a = Message::assistant("");
    a.tool_calls = vec![call.clone()];
    // Thinking from another model is not replayed.
    a.provider_items = Some(ProviderItems {
        kind: ProviderKind::Anthropic,
        model: "other".into(),
        items: vec![json!({"type": "thinking"})],
    });
    let mut img = Message::user("look");
    img.images = vec!["iVBORw0KGgo".into()];
    let req = ChatRequest {
        model: "claude-haiku-4-5".into(),
        messages: vec![
            Message::system("s"),
            Message::user("hi"),
            a,
            Message::tool(&call, "out"),
            img,
        ],
        tools: vec![],
        think: Some(true),
        effort: None,
        temperature: None,
        max_tokens: Some(4000),
        json: false,
    };
    let b = anthropic::body(&req);
    let m = b["messages"].as_array().unwrap();
    assert_eq!(m.len(), 3);
    assert_eq!(
        m[1]["content"].as_array().unwrap().len(),
        1,
        "no empty text block"
    );
    assert_eq!(m[1]["content"][0]["id"], "call_1");
    // The tool result and the following user message merge, tool_result first.
    assert_eq!(m[2]["content"][0]["type"], "tool_result");
    assert_eq!(m[2]["content"][0]["tool_use_id"], "call_1");
    assert_eq!(m[2]["content"][1]["type"], "image");
    assert_eq!(m[2]["content"][1]["source"]["media_type"], "image/png");
    // Older models use a thinking budget below max_tokens.
    assert_eq!(b["thinking"]["type"], "enabled");
    assert!(b["max_tokens"].as_u64().unwrap() > b["thinking"]["budget_tokens"].as_u64().unwrap());
}

#[tokio::test]
async fn responses_tool_round_trip_with_reasoning() {
    let reasoning_item = json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "ENC", "summary": [{"type": "summary_text", "text": "think"}]});
    let first = sse(&[
        (
            "response.created",
            json!({"type": "response.created", "response": {"id": "resp_1"}}),
        ),
        (
            "response.output_item.added",
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1"}}),
        ),
        (
            "response.reasoning_summary_text.delta",
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "delta": "think"}),
        ),
        (
            "response.output_item.done",
            json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning_item}),
        ),
        (
            "response.output_item.added",
            json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_9", "name": "Read", "arguments": ""}}),
        ),
        (
            "response.function_call_arguments.delta",
            json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "{\"file_path\":"}),
        ),
        (
            "response.function_call_arguments.delta",
            json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "\"a.txt\"}"}),
        ),
        (
            "response.output_item.done",
            json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_9", "name": "Read", "arguments": "{\"file_path\":\"a.txt\"}"}}),
        ),
        (
            "response.completed",
            json!({"type": "response.completed", "response": {"status": "completed", "usage": {"input_tokens": 50, "output_tokens": 7}}}),
        ),
    ]);
    let second = sse(&[
        (
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "output_index": 0, "delta": "All "}),
        ),
        (
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "output_index": 0, "delta": "done"}),
        ),
        (
            "response.incomplete",
            json!({"type": "response.incomplete", "response": {"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, "usage": {"input_tokens": 60, "output_tokens": 2}}}),
        ),
    ]);
    let (base, seen) = serve(vec![(429, "{}".into()), (200, first), (200, second)]).await;
    let p = Provider::new(
        ProviderKind::OpenaiResponses,
        &format!("{base}/v1"),
        Some("sk-test".into()),
        400_000,
    );
    let mut msgs = vec![Message::system("Be brief."), Message::user("read a.txt")];
    let mut req = ChatRequest {
        model: "gpt-5".into(),
        messages: msgs.clone(),
        tools: vec![read_tool()],
        think: Some(true),
        effort: None,
        temperature: Some(0.3),
        max_tokens: Some(8000),
        json: false,
    };
    let mut thoughts = String::new();
    let r = p
        .chat(&req, &mut |d| {
            if let Delta::Reasoning(t) = d {
                thoughts.push_str(&t)
            }
        })
        .await
        .unwrap();
    assert_eq!(thoughts, "think");
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].id, "call_9");
    assert_eq!(r.tool_calls[0].arguments["file_path"], "a.txt");
    assert_eq!(r.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!((r.usage.prompt_tokens, r.usage.completion_tokens), (50, 7));
    assert_eq!(
        r.provider_items.as_ref().unwrap().items[0]["encrypted_content"],
        "ENC"
    );

    let mut a = Message::assistant(r.text.clone());
    a.tool_calls = r.tool_calls.clone();
    a.provider_items = r.provider_items.clone();
    msgs.push(a);
    msgs.push(Message::tool(&r.tool_calls[0], "contents"));
    req.messages = msgs;
    let r2 = p.chat(&req, &mut |_| {}).await.unwrap();
    assert_eq!(r2.text, "All done");
    assert_eq!(r2.finish_reason.as_deref(), Some("length"));

    let seen = seen.lock().unwrap();
    assert!(
        seen.headers[1]
            .to_ascii_lowercase()
            .contains("authorization: bearer sk-test")
    );
    assert!(seen.headers[1].contains("POST /v1/responses"));
    let b = &seen.bodies[2];
    assert_eq!(b["instructions"], "Be brief.");
    assert_eq!(b["store"], false);
    assert_eq!(b["include"][0], "reasoning.encrypted_content");
    assert_eq!(b["reasoning"]["effort"], "medium");
    assert!(
        b.get("temperature").is_none(),
        "reasoning models take no sampling params"
    );
    assert_eq!(b["tools"][0]["type"], "function");
    assert_eq!(b["tools"][0]["strict"], false);
    let input = b["input"].as_array().unwrap();
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[0]["content"][0]["type"], "input_text");
    assert_eq!(input[1]["type"], "reasoning");
    assert_eq!(input[1]["encrypted_content"], "ENC");
    assert_eq!(input[2]["type"], "function_call");
    assert_eq!(input[2]["call_id"], "call_9");
    assert_eq!(input[2]["arguments"], "{\"file_path\":\"a.txt\"}");
    assert_eq!(input[3]["type"], "function_call_output");
    assert_eq!(input[3]["call_id"], "call_9");
    assert_eq!(input[3]["output"], "contents");
}

#[test]
fn responses_body_non_reasoning_and_images() {
    let call = ToolCall {
        id: "c1".into(),
        name: "Browser".into(),
        arguments: json!({}),
    };
    let mut a = Message::assistant("Taking a screenshot.");
    a.tool_calls = vec![call.clone()];
    let mut shot = Message::tool(&call, "screenshot taken");
    shot.images = vec!["/9j/4AAQ".into()];
    let req = ChatRequest {
        model: "gpt-4.1".into(),
        messages: vec![
            Message::system("s"),
            Message::user("go"),
            a,
            shot,
            Message::user("next"),
        ],
        tools: vec![],
        think: Some(true),
        effort: None,
        temperature: Some(0.3),
        max_tokens: None,
        json: true,
    };
    let b = responses::body(&req);
    assert!(b.get("reasoning").is_none());
    assert_eq!(b["temperature"], json!(0.3_f32));
    assert_eq!(b["text"]["format"]["type"], "json_object");
    let input = b["input"].as_array().unwrap();
    let types: Vec<String> = input
        .iter()
        .map(|i| {
            i["type"]
                .as_str()
                .or(i["role"].as_str())
                .unwrap_or("")
                .to_string()
        })
        .collect();
    assert_eq!(
        types,
        vec![
            "user",
            "message",
            "function_call",
            "function_call_output",
            "user",
            "user"
        ]
    );
    assert_eq!(input[4]["content"][0]["type"], "input_image");
    assert!(
        input[4]["content"][0]["image_url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/jpeg;base64,")
    );
}

#[test]
fn responses_gpt6_uses_reasoning_without_sampling() {
    let req = ChatRequest {
        model: "gpt-6".into(),
        messages: vec![Message::user("hello")],
        tools: vec![],
        think: Some(true),
        effort: None,
        temperature: Some(0.3),
        max_tokens: Some(100),
        json: false,
    };
    let b = responses::body(&req);
    assert_eq!(b["reasoning"]["effort"], "medium");
    assert!(b.get("temperature").is_none());
}

#[test]
fn effort_maps_to_each_api() {
    let req = |model: &str, think: bool, effort: &str| ChatRequest {
        model: model.into(),
        messages: vec![Message::user("hi")],
        tools: vec![],
        think: Some(think),
        effort: Some(effort.into()),
        temperature: None,
        max_tokens: Some(4000),
        json: false,
    };
    // Responses: `max` is capped at `xhigh`; thinking off stays `low`.
    assert_eq!(
        responses::body(&req("gpt-6", true, "max"))["reasoning"]["effort"],
        "xhigh"
    );
    assert_eq!(
        responses::body(&req("gpt-6", true, "high"))["reasoning"]["effort"],
        "high"
    );
    assert_eq!(
        responses::body(&req("gpt-6", false, "high"))["reasoning"]["effort"],
        "low"
    );
    // Anthropic: current models take every level in output_config.
    let b = anthropic::body(&req("claude-opus-5-5", true, "xhigh"));
    assert_eq!(b["output_config"]["effort"], "xhigh");
    assert_eq!(b["thinking"]["type"], "adaptive");
    // 4.6 has no xhigh; Opus 4.5 stops at high and also scales its budget.
    assert_eq!(
        anthropic::body(&req("claude-sonnet-4-6", true, "xhigh"))["output_config"]["effort"],
        "high"
    );
    let b = anthropic::body(&req("claude-opus-4-5", true, "max"));
    assert_eq!(b["output_config"]["effort"], "high");
    assert_eq!(b["thinking"]["budget_tokens"], 32000);
    assert!(b["max_tokens"].as_u64().unwrap() > 32000);
    // Opus 4.1 stops at 32k output: the budget leaves room for the answer.
    let b = anthropic::body(&req("claude-opus-4-1", true, "max"));
    assert!(
        b["max_tokens"].as_u64().unwrap() <= 32000,
        "{}",
        b["max_tokens"]
    );
    // Haiku 4.5 rejects effort: only the budget changes.
    let b = anthropic::body(&req("claude-haiku-4-5", true, "low"));
    assert!(b.get("output_config").is_none());
    assert_eq!(b["thinking"]["budget_tokens"], 2048);
    // No effort without thinking.
    assert!(
        anthropic::body(&req("claude-opus-5-5", false, "high"))
            .get("output_config")
            .is_none()
    );
}

#[tokio::test]
async fn chatgpt_model_rejection_is_distinct_from_login_failure() {
    for (status, body, expected) in [
        (
            400,
            r#"{"detail":"The model is not supported when using Codex with a ChatGPT account."}"#,
            "model access rejected",
        ),
        (401, r#"{"error":"unauthorized"}"#, "chanto login chatgpt"),
    ] {
        let (base, _) = serve(vec![(status, body.into())]).await;
        let mut p = Provider::new(
            ProviderKind::OpenaiResponses,
            &base,
            Some("test".into()),
            1000,
        );
        p.login = Some("chatgpt".into());
        let e = p
            .post_retrying(&base, &json!({}), &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains(expected), "{e}");
    }
}

#[tokio::test]
async fn chatgpt_catalog_uses_visible_slugs_in_server_order() {
    let (base, _) = serve(vec![(
        200,
        json!({"models": [
            {"slug": "gpt-6.1-sol", "visibility": "list"},
            {"slug": "hidden", "visibility": "hide"},
            {"slug": "gpt-6-astra", "visibility": "list"}
        ]})
        .to_string(),
    )])
    .await;
    let mut p = Provider::new(
        ProviderKind::OpenaiResponses,
        &base,
        Some("test".into()),
        1000,
    );
    // Use a private test auth home so list_models obtains a dummy login token.
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let d = std::env::temp_dir().join(format!("chanto-catalog-{}", uuid::Uuid::new_v4()));
    let saved_home = std::env::var_os("CHANTO_HOME");
    unsafe { std::env::set_var("CHANTO_HOME", &d) };
    crate::auth::set(
        "chatgpt",
        crate::auth::Credential::ApiKey { key: "test".into() },
    )
    .unwrap();
    p.login = Some("chatgpt".into());
    let result = p.list_models().await;
    let (api_base, _) = serve(vec![(
        200,
        json!({"data": [
            {"id": "gpt-6.1-sol"}, {"id": "gpt-6-astra"}
        ]})
        .to_string(),
    )])
    .await;
    p.base_url = api_base;
    let api_result = p.list_models().await;
    let (error_base, _) = serve(vec![(401, r#"{"error":"unauthorized"}"#.into())]).await;
    p.base_url = error_base;
    let error = p.list_models().await;
    unsafe {
        match saved_home {
            Some(home) => std::env::set_var("CHANTO_HOME", home),
            None => std::env::remove_var("CHANTO_HOME"),
        }
    }
    let _ = std::fs::remove_dir_all(d);
    assert_eq!(result.unwrap(), vec!["gpt-6.1-sol", "gpt-6-astra"]);
    assert_eq!(api_result.unwrap(), vec!["gpt-6.1-sol", "gpt-6-astra"]);
    assert!(
        error.is_err(),
        "HTTP failures must not appear as empty catalogs"
    );
}

#[test]
fn provider_kinds_and_contexts() {
    assert_eq!(
        serde_json::from_value::<ProviderKind>(json!("openai-responses")).unwrap(),
        ProviderKind::OpenaiResponses
    );
    assert_eq!(
        serde_json::from_value::<ProviderKind>(json!("anthropic")).unwrap(),
        ProviderKind::Anthropic
    );
    assert_eq!(
        serde_json::from_value::<ProviderKind>(json!("openai")).unwrap(),
        ProviderKind::Openai
    );
    assert_eq!(
        hosted_context(ProviderKind::Anthropic, "claude-opus-5-5"),
        1_000_000
    );
    assert_eq!(
        hosted_context(ProviderKind::Anthropic, "claude-haiku-4-5"),
        200_000
    );
    // Sessions saved before provider_items existed still load.
    let m: Message = serde_json::from_value(json!({"role": "assistant", "content": "x"})).unwrap();
    assert!(m.provider_items.is_none());
}

#[tokio::test]
async fn rate_limit_event_and_headers_reach_the_caller() {
    // Some deployments report limits in the SSE stream instead of headers; the
    // streamed value must win over the (absent) header value.
    let body = sse(&[
        (
            "codex.rate_limits",
            json!({"type": "codex.rate_limits", "rate_limits": {"primary": {"used_percent": 74.0, "window_minutes": 300}}}),
        ),
        (
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "output_index": 0, "delta": "hi"}),
        ),
        (
            "response.completed",
            json!({"type": "response.completed", "response": {"status": "completed", "usage": {"input_tokens": 1, "output_tokens": 1}}}),
        ),
    ]);
    let (base, _) = serve(vec![(200, body)]).await;
    let p = Provider::new(
        ProviderKind::OpenaiResponses,
        &base,
        Some("test".into()),
        1000,
    );
    let req = ChatRequest {
        model: "gpt-5".into(),
        messages: vec![Message::user("hi")],
        tools: vec![],
        think: None,
        effort: None,
        temperature: None,
        max_tokens: None,
        json: false,
    };
    let r = p.chat(&req, &mut |_| {}).await.unwrap();
    assert_eq!(r.text, "hi");
    let lim = r.rate_limits.expect("streamed limits must be captured");
    assert_eq!(lim.primary.as_ref().unwrap().used_percent, 74.0);
    assert_eq!(lim.summary(), "5h 26% left");
}

/// Opt-in real API smoke tests; ordinary tests never read real credentials.
#[tokio::test]
#[ignore = "requires real provider credentials and network access"]
async fn live_smoke() {
    for (name, model) in [
        ("anthropic", "claude-haiku-4-5"),
        ("openai", "gpt-4.1-mini"),
    ] {
        let Some(p) = builtin_provider(name, model) else {
            continue;
        };
        let req = ChatRequest {
            model: model.into(),
            messages: vec![
                Message::system("Answer with one word."),
                Message::user("Say hello."),
            ],
            tools: vec![],
            think: Some(false),
            effort: None,
            temperature: None,
            max_tokens: Some(50),
            json: false,
        };
        let r = p.chat(&req, &mut |_| {}).await.unwrap();
        assert!(!r.text.is_empty(), "{name}: empty reply");
        assert!(r.usage.prompt_tokens > 0, "{name}: no usage");
    }
}

#[tokio::test]
async fn openai_compatible_servers_report_their_context_window() {
    let j = |v: Value| v.to_string().into_bytes();
    // vLLM-style: the model list carries the window.
    let (base, _) = super::mock::serve_bytes(vec![(200, "application/json", j(json!({"data": [{"id": "other", "max_model_len": 8192}, {"id": "m", "max_model_len": 131072}]})))]).await;
    let p = Provider::new(ProviderKind::Openai, &base, None, 32768);
    assert_eq!(p.model_info("m").await, Some((131072, true)));
    // LiteLLM: the list has no window, `/model/info` does.
    let (base, seen) = super::mock::serve_bytes(vec![
        (200, "application/json", j(json!({"data": [{"id": "m"}]}))),
        (200, "application/json", j(json!({"data": [{"model_name": "m", "model_info": {"max_input_tokens": 1_000_000}}]}))),
    ])
    .await;
    let p = Provider::new(ProviderKind::Openai, &base, Some("sk".into()), 32768);
    assert_eq!(p.model_info("m").await, Some((1_000_000, true)));
    let heads = seen.lock().unwrap().headers.clone();
    assert!(
        heads[0].starts_with("GET /v1/models") && heads[1].starts_with("GET /model/info"),
        "{heads:?}"
    );
    assert!(
        heads
            .iter()
            .all(|h| h.to_ascii_lowercase().contains("authorization: bearer sk"))
    );
    // Nothing reported: no window.
    let (base, _) = super::mock::serve_bytes(vec![
        (404, "text/plain", vec![]),
        (404, "text/plain", vec![]),
    ])
    .await;
    assert_eq!(
        Provider::new(ProviderKind::Openai, &base, None, 32768)
            .model_info("m")
            .await,
        None
    );
}

/// OpenAI-style SSE body: `data:` lines and the closing `[DONE]`.
fn chat_sse(chunks: &[Value]) -> String {
    chunks
        .iter()
        .map(|c| format!("data: {c}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n"
}

fn chat_req(model: &str, messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages,
        tools: vec![],
        think: None,
        effort: None,
        temperature: None,
        max_tokens: None,
        json: false,
    }
}

#[tokio::test]
async fn deepseek_reasoning_content_goes_back_with_its_assistant_turn() {
    let first = chat_sse(&[
        json!({"choices": [{"delta": {"reasoning_content": "I should read the file."}, "finish_reason": null}]}),
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "Read", "arguments": "{\"file_path\":\"a\"}"}}]}, "finish_reason": "tool_calls"}]}),
    ]);
    let second =
        chat_sse(&[json!({"choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}]})]);
    let (base, seen) = super::mock::serve(vec![(200, first), (200, second)]).await;
    let p = Provider::new(
        ProviderKind::Openai,
        &format!("{base}/v1"),
        Some("k".into()),
        32768,
    );
    let r = p
        .chat(
            &chat_req("deepseek-flash", vec![Message::user("read a".to_string())]),
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(r.reasoning, "I should read the file.");
    let mut asst = Message::new(Role::Assistant, r.text.clone());
    asst.tool_calls = r.tool_calls.clone();
    asst.provider_items = r.provider_items.clone();
    let mut tool = Message::new(Role::Tool, "contents");
    tool.tool_call_id = Some("c1".into());
    let history = vec![Message::user("read a".to_string()), asst, tool];
    p.chat(&chat_req("deepseek-flash", history.clone()), &mut |_| {})
        .await
        .unwrap();
    let sent = &seen.lock().unwrap().bodies[1]["messages"][1];
    assert_eq!(
        sent["reasoning_content"], "I should read the file.",
        "{sent}"
    );
    // Another model never gets this model's reasoning.
    let fields = json!({});
    let other =
        serde_json::to_value(Provider::openai_body(&fields, &history, "other-model")).unwrap();
    assert!(other["messages"][1].get("reasoning_content").is_none());
}

#[tokio::test]
async fn a_session_missing_its_reasoning_retries_without_thinking() {
    let rejected = r#"{"error":{"message":"The `reasoning_content` in the thinking mode must be passed back to the API.","type":"invalid_request_error"}}"#;
    let ok =
        chat_sse(&[json!({"choices": [{"delta": {"content": "fine"}, "finish_reason": "stop"}]})]);
    let (base, seen) = super::mock::serve_bytes(vec![
        (400, "application/json", rejected.as_bytes().to_vec()),
        (200, "text/event-stream", ok.into_bytes()),
    ])
    .await;
    let p = Provider::new(
        ProviderKind::Openai,
        &format!("{base}/v1"),
        Some("k".into()),
        32768,
    );
    let mut req = chat_req("deepseek-flash", vec![Message::user("hi".to_string())]);
    req.effort = Some("high".into());
    req.think = Some(true);
    let r = p.chat(&req, &mut |_| {}).await.unwrap();
    assert_eq!(r.text, "fine");
    let bodies = &seen.lock().unwrap().bodies;
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[1]["thinking"], json!({"type": "disabled"}));
    assert!(bodies[1].get("reasoning_effort").is_none());
}

#[tokio::test]
async fn chat_completions_retry_rate_limits_and_overloads() {
    // OpenAI-compatible providers (OpenRouter, DeepSeek, Groq, local servers)
    // failed the turn on the first 429 or 500.
    let ok =
        chat_sse(&[json!({"choices": [{"delta": {"content": "fine"}, "finish_reason": "stop"}]})]);
    let (base, seen) = super::mock::serve(vec![
        (429, "{\"error\":{\"message\":\"slow down\"}}".into()),
        (500, "oops".into()),
        (529, "{}".into()),
        (200, ok),
    ])
    .await;
    let p = Provider::new(ProviderKind::Openai, &format!("{base}/v1"), None, 32768);
    let r = p
        .chat(
            &chat_req("m", vec![Message::user("hi".to_string())]),
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(r.text, "fine");
    assert_eq!(seen.lock().unwrap().bodies.len(), 4);
    // A client error is not retried.
    let (base, seen) =
        super::mock::serve(vec![(400, "{\"error\":{\"message\":\"bad\"}}".into())]).await;
    let p = Provider::new(ProviderKind::Openai, &format!("{base}/v1"), None, 32768);
    let e = p
        .chat(
            &chat_req("m", vec![Message::user("hi".to_string())]),
            &mut |_| {},
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("400"), "{e}");
    assert_eq!(seen.lock().unwrap().bodies.len(), 1);
    // A storm gives up after five tries, naming the status.
    let (base, seen) = super::mock::serve(vec![(503, "busy".into()); 6]).await;
    let p = Provider::new(ProviderKind::Openai, &format!("{base}/v1"), None, 32768);
    let e = p
        .chat(
            &chat_req("m", vec![Message::user("hi".to_string())]),
            &mut |_| {},
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("API error 503"), "{e}");
    assert_eq!(seen.lock().unwrap().bodies.len(), 5);
}

#[tokio::test]
async fn tool_call_indexes_are_keys_not_positions() {
    // A server sending a huge index used to make us allocate a slot per index.
    let body = chat_sse(&[
        json!({"choices": [{"delta": {"tool_calls": [{"index": 4294967295u64, "id": "big", "function": {"name": "Read", "arguments": "{\"file_path\":"}}]}, "finish_reason": null}]}),
        json!({"choices": [{"delta": {"tool_calls": [{"index": 7, "id": "seven", "function": {"name": "Ls", "arguments": "{}"}}]}, "finish_reason": null}]}),
        json!({"choices": [{"delta": {"tool_calls": [{"index": 4294967295u64, "function": {"arguments": "\"a\"}"}}]}, "finish_reason": "tool_calls"}]}),
    ]);
    let (base, _) = super::mock::serve(vec![(200, body)]).await;
    let p = Provider::new(ProviderKind::Openai, &format!("{base}/v1"), None, 32768);
    let r = p
        .chat(
            &chat_req("m", vec![Message::user("hi".to_string())]),
            &mut |_| {},
        )
        .await
        .unwrap();
    let calls: Vec<_> = r
        .tool_calls
        .iter()
        .map(|c| (c.id.as_str(), c.name.as_str(), c.arguments.clone()))
        .collect();
    assert_eq!(
        calls,
        vec![
            ("seven", "Ls", json!({})),
            ("big", "Read", json!({"file_path": "a"}))
        ]
    );
}

#[tokio::test]
async fn a_stream_cut_off_mid_tool_call_is_an_error_not_a_repaired_call() {
    // No finish_reason and no [DONE]: the connection just closed. Repairing the
    // JSON would write a truncated file.
    let cut = [
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "Write", "arguments": ""}}]}, "finish_reason": null}]}),
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"file_path\":\"a.txt\",\"content\":\"line1\\nli"}}]}, "finish_reason": null}]}),
    ];
    let body: String = cut.iter().map(|c| format!("data: {c}\n\n")).collect();
    let (base, _) = super::mock::serve(vec![(200, body)]).await;
    let p = Provider::new(ProviderKind::Openai, &format!("{base}/v1"), None, 32768);
    let e = p
        .chat(
            &chat_req("m", vec![Message::user("hi".to_string())]),
            &mut |_| {},
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("connection closed"), "{e}");
    // A server that ends with finish_reason but no [DONE] is still fine.
    let mut fin = cut.to_vec();
    fin[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"] =
        json!("{\"file_path\":\"a.txt\",\"content\":\"x\"}");
    fin.push(json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}));
    let body: String = fin.iter().map(|c| format!("data: {c}\n\n")).collect();
    let (base, _) = super::mock::serve(vec![(200, body)]).await;
    let p = Provider::new(ProviderKind::Openai, &format!("{base}/v1"), None, 32768);
    let r = p
        .chat(
            &chat_req("m", vec![Message::user("hi".to_string())]),
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(
        r.tool_calls[0].arguments,
        json!({"file_path": "a.txt", "content": "x"})
    );
}

#[tokio::test]
async fn a_server_of_the_other_kind_gets_a_hint_not_a_bad_line() {
    // Ollama provider, OpenAI server: SSE lines, or a whole chat.completion body.
    let sse_body =
        chat_sse(&[json!({"choices": [{"delta": {"content": "hi"}, "finish_reason": "stop"}]})]);
    let whole = json!({"object": "chat.completion", "choices": [{"message": {"role": "assistant", "content": "hi"}}]}).to_string();
    for body in [sse_body, whole] {
        let (base, _) = serve(vec![(200, body)]).await;
        let p = Provider::new(ProviderKind::Ollama, &base, None, 32768);
        let e = p
            .chat(
                &chat_req("m", vec![Message::user("hi".to_string())]),
                &mut |_| {},
            )
            .await
            .unwrap_err();
        assert!(
            format!("{e:#}")
                .contains("looks like an OpenAI-compatible server; set provider = \"openai\""),
            "{e:#}"
        );
    }
    // OpenAI provider, Ollama server: NDJSON.
    let ndjson = format!(
        "{}\n{}\n",
        json!({"message": {"role": "assistant", "content": "hi"}, "done": false}),
        json!({"message": {"role": "assistant", "content": ""}, "done": true})
    );
    let (base, _) = serve(vec![(200, ndjson)]).await;
    let p = Provider::new(ProviderKind::Openai, &format!("{base}/v1"), None, 32768);
    let e = p
        .chat(
            &chat_req("m", vec![Message::user("hi".to_string())]),
            &mut |_| {},
        )
        .await
        .unwrap_err();
    assert!(
        format!("{e:#}").contains("looks like an Ollama server")
            && format!("{e:#}").contains("provider = \"ollama\""),
        "{e:#}"
    );
}

#[tokio::test]
async fn repaired_tool_arguments_are_marked() {
    // finish_reason says done, but the arguments are cut short.
    let body = chat_sse(&[
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "Write", "arguments": "{\"file_path\":\"a.txt\",\"content\":\"li"}}]}, "finish_reason": "tool_calls"}]}),
    ]);
    let (base, _) = serve(vec![(200, body)]).await;
    let p = Provider::new(ProviderKind::Openai, &format!("{base}/v1"), None, 32768);
    let r = p
        .chat(
            &chat_req("m", vec![Message::user("hi".to_string())]),
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(r.tool_calls[0].arguments[REPAIRED], json!(true));
    assert_eq!(
        parse_tool_args("{\"a\": 1}"),
        json!({"a": 1}),
        "valid JSON is left alone"
    );
}

#[test]
fn coding_plan_presets() {
    let kimi = crate::providers::find("kimi-coding").unwrap();
    assert_eq!(
        (kimi.kind, kimi.base_url, kimi.env),
        (
            ProviderKind::Openai,
            "https://api.kimi.ai/coding/v1",
            &["KIMI_CODE_API_KEY"][..]
        )
    );
    let minimax = crate::providers::find("minimax-coding").unwrap();
    assert_eq!(
        (minimax.kind, minimax.base_url, minimax.env),
        (
            ProviderKind::Openai,
            "https://api.minimax.io/v1",
            &["MINIMAX_CODING_API_KEY"][..]
        )
    );
    assert_eq!(
        crate::detect::default_model("kimi-coding").as_deref(),
        Some("kimi-coding/kimi-for-coding")
    );
    assert_eq!(
        crate::detect::default_model("minimax-coding").as_deref(),
        Some("minimax-coding/MiniMax-M3.1-Flash-Preview")
    );
    assert_eq!(
        crate::providers::context("kimi-coding", "kimi-for-coding"),
        Some(1_048_576)
    );
    assert_eq!(
        crate::providers::context("kimi-coding", "k3"),
        Some(262_144)
    );
    assert_eq!(
        crate::providers::context("minimax-coding", "MiniMax-M3.1-Flash-Preview"),
        Some(1_000_000)
    );
    assert_eq!(
        crate::providers::context("minimax-coding", "MiniMax-M2.7"),
        Some(204_800)
    );
    assert_eq!(
        crate::providers::context("deepseek", "deepseek-flash"),
        Some(1_000_000)
    );
    assert_eq!(
        crate::providers::context("deepseek", "deepseek-v4-pro"),
        Some(1_000_000)
    );
    assert_eq!(crate::providers::context("deepseek", "deepseek-chat"), None);
    assert!(
        crate::providers::key_page("kimi-coding")
            .unwrap()
            .contains("kimi.com/code/console")
    );
    assert!(
        crate::providers::key_page("minimax-coding")
            .unwrap()
            .contains("console/plan")
    );
    // Flat-rate plans have no bundled per-token price.
    let u = Usage {
        prompt_tokens: 1000,
        completion_tokens: 1000,
        ..Default::default()
    };
    assert_eq!(
        crate::cost::price("minimax-coding", "MiniMax-M2.7", &u, &Default::default()),
        None
    );
    assert_eq!(
        crate::cost::price("kimi-coding", "kimi-for-coding", &u, &Default::default()),
        None
    );
}

#[test]
fn versioned_base_urls_get_no_extra_v1() {
    let base = |u: &str| Provider::new(ProviderKind::Openai, u, None, 1000).openai_base();
    assert_eq!(
        base("https://api.z.ai/api/coding/paas/v4"),
        "https://api.z.ai/api/coding/paas/v4"
    );
    assert_eq!(
        base("https://generativelanguage.googleapis.com/v1beta/openai"),
        "https://generativelanguage.googleapis.com/v1beta/openai"
    );
    assert_eq!(
        base("https://api.kimi.ai/coding/v1/"),
        "https://api.kimi.ai/coding/v1"
    );
    assert_eq!(base("http://localhost:8080"), "http://localhost:8080/v1");
    assert_eq!(
        base("https://api.groq.com/openai"),
        "https://api.groq.com/openai/v1"
    );
    assert_eq!(base("https://host/vision"), "https://host/vision/v1");
}

#[test]
fn reasoning_split_goes_only_to_minimax() {
    assert!(
        minimax_host("https://api.minimax.io/v1")
            && minimax_host("https://api.minimax.cn/v1")
            && minimax_host("https://api.minimaxi.com/v1")
    );
    assert!(
        !minimax_host("https://api.kimi.ai/coding/v1")
            && !minimax_host("https://notminimax.io/v1")
            && !minimax_host("http://127.0.0.1:1")
    );
}

#[tokio::test]
async fn coding_plans_send_their_key_to_their_endpoint() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let d = std::env::temp_dir().join(format!("chanto-plans-{}", uuid::Uuid::new_v4()));
    let saved_home = std::env::var_os("CHANTO_HOME");
    unsafe { std::env::set_var("CHANTO_HOME", &d) };
    let result = async {
        for (id, model) in [
            ("kimi-coding", "kimi-for-coding"),
            ("minimax-coding", "MiniMax-M3.1-Flash-Preview"),
            ("zai", "glm-4.6"),
        ] {
            // What `chanto login <id>` stores.
            crate::auth::set_as(
                id,
                None,
                crate::auth::Credential::ApiKey {
                    key: format!("key-{id}").into(),
                },
            )
            .unwrap();
            let mut p = builtin_provider(id, model)
                .unwrap_or_else(|| panic!("{id} resolves with a saved key"));
            let h = crate::providers::find(id).unwrap();
            assert_eq!(p.base_url, h.base_url);
            assert_eq!(
                p.billing_id(),
                id,
                "billed as the plan, not another provider at the same address"
            );
            assert_eq!(
                p.num_ctx,
                crate::providers::context(id, model).unwrap_or(131_072)
            );
            // The same path on a local server.
            let path = reqwest::Url::parse(h.base_url).unwrap().path().to_string();
            let reply = chat_sse(&[
                json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]}),
            ]);
            let (base, seen) = serve(vec![(200, reply)]).await;
            p.base_url = format!("{base}{path}");
            let r = p
                .chat(&chat_req(model, vec![Message::user("hi")]), &mut |_| {})
                .await
                .unwrap();
            assert_eq!(r.text, "ok");
            let seen = seen.lock().unwrap();
            let head = seen.headers[0].to_ascii_lowercase();
            assert!(
                head.starts_with(&format!("post {path}/chat/completions ")),
                "{id}: {head}"
            );
            assert!(
                head.contains(&format!("authorization: bearer key-{id}")),
                "{id}: {head}"
            );
            assert!(head.contains("user-agent: chanto/"), "{id}: {head}");
            assert_eq!(seen.bodies[0]["model"], model);
        }
        assert!(
            crate::auth::providers_table()
                .lines()
                .any(|l| l.contains("minimax-coding") && l.contains("key saved"))
        );
        // A second account (`--as work`), switched to for the session.
        crate::auth::set_as(
            "kimi-coding",
            Some("work"),
            crate::auth::Credential::ApiKey {
                key: "work-key".into(),
            },
        )
        .unwrap();
        crate::auth::switch_account("kimi-coding", "work").unwrap();
        let work = builtin_provider("kimi-coding", "k3").unwrap().api_key;
        crate::auth::switch_account("kimi-coding", crate::auth::DEFAULT_ACCOUNT).unwrap();
        assert_eq!(work.as_deref(), Some("work-key"));
        assert_eq!(
            builtin_provider("kimi-coding", "k3")
                .unwrap()
                .api_key
                .as_deref(),
            Some("key-kimi-coding")
        );
        // Without a saved key, the plan's variable.
        crate::auth::remove("minimax-coding").unwrap();
        assert!(builtin_provider("minimax-coding", "MiniMax-M3").is_none());
        unsafe { std::env::set_var("MINIMAX_CODING_API_KEY", "sk-cp-env") };
        let p = builtin_provider("minimax-coding", "MiniMax-M3");
        unsafe { std::env::remove_var("MINIMAX_CODING_API_KEY") };
        assert_eq!(p.unwrap().api_key.as_deref(), Some("sk-cp-env"));
    };
    let result = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(result)).await;
    unsafe {
        match saved_home {
            Some(home) => std::env::set_var("CHANTO_HOME", home),
            None => std::env::remove_var("CHANTO_HOME"),
        }
    }
    let _ = std::fs::remove_dir_all(d);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

#[tokio::test]
async fn a_coding_plan_without_a_model_list_offers_its_documented_models() {
    let mut p = Provider::new(ProviderKind::Openai, "", Some("k".into()), 1000);
    p.billing_provider = Some("kimi-coding".into());
    let (base, seen) = serve(vec![(404, r#"{"error":"not found"}"#.into())]).await;
    p.base_url = format!("{base}/coding/v1");
    assert_eq!(
        p.list_models().await.unwrap(),
        crate::providers::plan_models("kimi-coding")
    );
    assert!(seen.lock().unwrap().headers[0].starts_with("GET /coding/v1/models "));
    // A listed catalog wins, and a rejected key is still an error.
    let (base, _) = serve(vec![
        (
            200,
            json!({"object": "list", "data": [{"id": "kimi-for-coding"}]}).to_string(),
        ),
        (401, r#"{"error":"bad key"}"#.into()),
    ])
    .await;
    p.base_url = format!("{base}/coding/v1");
    assert_eq!(p.list_models().await.unwrap(), vec!["kimi-for-coding"]);
    assert!(p.list_models().await.is_err());
}
