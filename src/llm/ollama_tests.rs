//! The Ollama wire format against a loopback server: streamed NDJSON, the
//! errors a server can answer with, model lookups and account balances.

use super::*;
use axum::routing::{get, post};
use axum::{Json, Router};
use std::sync::{Arc, Mutex};

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    base
}

fn request(think: Option<bool>) -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: vec![Message::system("s"), Message::user("hi")],
        tools: vec![],
        think,
        effort: None,
        temperature: Some(0.5),
        max_tokens: Some(7),
        json: false,
    }
}

fn provider(base: &str) -> Provider {
    Provider::new(ProviderKind::Ollama, base, None, 4096)
}

async fn chat_body(body: &'static str) -> Result<(ChatResponse, Vec<String>)> {
    let app = Router::new().route("/api/chat", post(move || async move { body }));
    let base = serve(app).await;
    let mut deltas = Vec::new();
    let r = provider(&base)
        .chat(&request(None), &mut |d| deltas.push(format!("{d:?}")))
        .await?;
    Ok((r, deltas))
}

#[tokio::test]
async fn a_stream_builds_text_thinking_tools_and_usage() {
    let (r, deltas) = chat_body(concat!(
        r#"{"message":{"thinking":"hmm"}}"#, "\n",
        r#"{"message":{"content":"he"}}"#, "\n",
        "\n",
        r#"{"message":{"content":"llo"}}"#, "\n",
        r#"{"message":{"tool_calls":[{"function":{"name":"Read","arguments":{"file_path":"a"}}},{"id":"c2","function":{"name":"Ls","arguments":"{\"path\":\".\"}"}},{"function":{"name":"Bad","arguments":"not json"}},{"function":{"name":"None"}}]}}"#, "\n",
        r#"{"done":true,"done_reason":"stop","prompt_eval_count":3,"eval_count":4}"#, "\n",
    ))
    .await
    .unwrap();
    assert_eq!(r.text, "hello");
    assert_eq!(r.reasoning, "hmm");
    assert_eq!((r.usage.prompt_tokens, r.usage.completion_tokens), (3, 4));
    assert_eq!(r.finish_reason.as_deref(), Some("stop"));
    let names: Vec<_> = r.tool_calls.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["Read", "Ls", "Bad", "None"]);
    assert_eq!(r.tool_calls[1].id, "c2");
    assert!(
        !r.tool_calls[0].id.is_empty(),
        "a call without an id gets one"
    );
    assert_eq!(
        r.tool_calls[1].arguments["path"], ".",
        "string arguments are parsed"
    );
    assert_eq!(
        r.tool_calls[2].arguments,
        serde_json::json!({}),
        "bad arguments are empty"
    );
    assert_eq!(r.tool_calls[3].arguments, serde_json::json!({}));
    assert!(deltas.iter().any(|d| d.contains("Reasoning")));
    assert!(deltas.iter().any(|d| d.contains("ToolCall")));
}

#[tokio::test]
async fn a_server_speaking_openai_is_named_as_that() {
    let e = chat_body(r#"{"object":"chat.completion","choices":[]}"#)
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("`choices`"), "{e:#}");
    let e = chat_body("data: {\"choices\":[]}\n\n").await.unwrap_err();
    assert!(format!("{e:#}").contains("Server-Sent Events"), "{e:#}");
}

#[tokio::test]
async fn a_rejected_think_flag_is_retried_once_without_it() {
    let bodies: Arc<Mutex<Vec<serde_json::Value>>> = Default::default();
    let b2 = bodies.clone();
    let app = Router::new().route(
        "/api/chat",
        post(move |Json(v): Json<serde_json::Value>| {
            let bodies = b2.clone();
            async move {
                let thinks = v.get("think").is_some();
                bodies.lock().unwrap().push(v);
                if thinks {
                    (
                        axum::http::StatusCode::BAD_REQUEST,
                        "model does not support think".to_string(),
                    )
                } else {
                    (
                        axum::http::StatusCode::OK,
                        "{\"message\":{\"content\":\"ok\"},\"done\":true}\n".to_string(),
                    )
                }
            }
        }),
    );
    let base = serve(app).await;
    let r = provider(&base)
        .chat(&request(Some(true)), &mut |_| {})
        .await
        .unwrap();
    assert_eq!(r.text, "ok");
    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0]["options"]["num_ctx"], 4096);
    assert_eq!(bodies[0]["options"]["num_predict"], 7);
    assert!(bodies[1].get("think").is_none());
}

#[tokio::test]
async fn other_errors_carry_the_status_and_text() {
    let app = Router::new().route(
        "/api/chat",
        post(|| async {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "out of memory",
            )
        }),
    );
    let base = serve(app).await;
    let e = provider(&base)
        .chat(&request(None), &mut |_| {})
        .await
        .unwrap_err();
    let text = format!("{e:#}");
    assert!(
        text.contains("500") && text.contains("out of memory"),
        "{text}"
    );
}

#[tokio::test]
async fn model_lookups_read_tags_and_show() {
    let app = Router::new()
        .route(
            "/api/tags",
            get(|| async {
                Json(serde_json::json!({"models": [{"name": "a"}, {"name": "b"}, {}]}))
            }),
        )
        .route(
            "/api/show",
            post(|| async {
                Json(serde_json::json!({
                    "model_info": {"x.context_length": 8192, "other": 1},
                    "capabilities": ["completion", "vision"]
                }))
            }),
        );
    let base = serve(app).await;
    let p = provider(&base);
    assert_eq!(p.list_models().await.unwrap(), ["a", "b"]);
    assert_eq!(p.model_info("m").await, Some((8192, true)));
}

#[tokio::test]
async fn label_probabilities_come_from_the_first_token() {
    let app = Router::new().route(
        "/api/chat",
        post(|Json(v): Json<serde_json::Value>| async move {
            assert_eq!(v["options"]["num_predict"], 1);
            assert_eq!(v["stream"], false);
            Json(serde_json::json!({
                "prompt_eval_count": 5, "eval_count": 1,
                "logprobs": [{"top_logprobs": [
                    {"token": "yes", "logprob": -0.1},
                    {"token": "no", "logprob": -2.5}
                ]}]
            }))
        }),
    );
    let base = serve(app).await;
    let p = provider(&base);
    let (top, usage) = p.first_token_ollama(&request(Some(false))).await.unwrap();
    assert_eq!(top.len(), 2);
    assert_eq!(top[0].0, "yes");
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (5, 1));
}

/// A balance is only shown when the provider's answer can be trusted.
#[tokio::test]
async fn balances_are_only_reported_from_known_shapes() {
    let local = provider("http://127.0.0.1:9");
    assert_eq!(local.balance().await, None, "no key");
    let keyed = Provider::new(
        ProviderKind::Openai,
        "http://127.0.0.1:9",
        Some("k".into()),
        4096,
    );
    assert_eq!(keyed.balance().await, None, "not a provider with a balance");
}

#[tokio::test]
async fn a_request_with_tools_sends_them() {
    let bodies: Arc<Mutex<Vec<serde_json::Value>>> = Default::default();
    let b2 = bodies.clone();
    let app = Router::new().route(
        "/api/chat",
        post(move |Json(v): Json<serde_json::Value>| {
            let bodies = b2.clone();
            async move {
                bodies.lock().unwrap().push(v);
                "{\"message\":{\"content\":\"ok\"},\"done\":true}\n"
            }
        }),
    );
    let base = serve(app).await;
    let mut req = request(None);
    let spec = ToolSpec {
        name: "Read".into(),
        description: "Read a file.".into(),
        parameters: serde_json::json!({"type": "object"}),
    };
    req.tools = vec![spec.clone()];
    provider(&base).chat(&req, &mut |_| {}).await.unwrap();
    let bodies = bodies.lock().unwrap();
    assert_eq!(
        bodies[0]["tools"],
        serde_json::json!([{"type": "function", "function": spec}])
    );
}
