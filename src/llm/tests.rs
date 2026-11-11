use super::*;

/// The messages as the request built them before `OpenaiMessages`: the
/// reference it must match.
fn reference_openai_messages(msgs: &[Message], model: &str) -> Vec<Value> {
    msgs.iter()
        .map(|m| {
            let extra = m.provider_items.as_ref().filter(|p| p.kind == ProviderKind::Openai && p.model == model);
            let content = if m.images.is_empty() {
                json!(m.content)
            } else {
                let mut parts = vec![json!({"type":"text","text": m.content})];
                for img in &m.images {
                    parts.push(json!({"type":"image_url","image_url":{"url": format!("data:{};base64,{img}", sniff_mime(img))}}));
                }
                Value::Array(parts)
            };
            let mut o = json!({ "role": m.role, "content": content });
            if !m.tool_calls.is_empty() {
                o["tool_calls"] = Value::Array(
                    m.tool_calls
                        .iter()
                        .map(|c| {
                            let mut v = json!({"id": c.id, "type":"function","function":{"name": c.name, "arguments": c.arguments.to_string()}});
                            if let Some(e) = extra.and_then(|p| p.items.iter().find(|i| i["id"] == c.id.as_str())) {
                                v["extra_content"] = e["extra_content"].clone();
                            }
                            v
                        })
                        .collect(),
                );
            }
            if let Some(id) = &m.tool_call_id {
                o["tool_call_id"] = json!(id);
            }
            if let Some(r) = extra.and_then(|p| p.items.iter().find_map(|i| i["reasoning_content"].as_str())) {
                o["reasoning_content"] = json!(r);
            }
            o
        })
        .collect()
}

#[test]
fn openai_messages_serialize_as_the_value_tree_did() {
    let call = |id: &str, args: Value| ToolCall {
        id: id.into(),
        name: "Read".into(),
        arguments: args,
    };
    let mut assistant = Message::assistant("reading \"two\" files\n\u{1}");
    assistant.tool_calls = vec![
        call("c1", json!({"file_path": "a.rs"})),
        call(
            "c2",
            json!({"file_path": "b.rs", "limit": 5, "nested": [1, null, {"x": "é"}]}),
        ),
    ];
    assistant.provider_items = Some(ProviderItems {
        kind: ProviderKind::Openai,
        model: "m".into(),
        items: vec![
            json!({"id": "c1", "extra_content": {"google": {"thought_signature": "sig"}}}),
            json!({"id": "c2"}),
            json!({"reasoning_content": "thought about it"}),
        ],
    });
    let mut other_model = assistant.clone();
    other_model.provider_items.as_mut().unwrap().model = "other".into();
    let mut image = Message::user("look");
    image.images = vec!["iVBORw0KGgo=".into(), "/9j/4AAQ".into()];
    let msgs = vec![
        Message::system("sys"),
        Message::user(""),
        image,
        assistant.clone(),
        Message::tool(&assistant.tool_calls[0], "fn a() {}"),
        Message::tool(&assistant.tool_calls[1], ""),
        other_model,
        Message::assistant("done"),
    ];
    for model in ["m", "other"] {
        let fields = json!({"model": model, "stream": true, "max_tokens": 10});
        let mut expected = fields.clone();
        expected["messages"] = Value::Array(reference_openai_messages(&msgs, model));
        let body = Provider::openai_body(&fields, &msgs, model);
        assert_eq!(serde_json::to_value(&body).unwrap(), expected);
        let text = serde_json::to_string(&body).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), expected);
    }
}

#[test]
fn failure_hints_say_what_to_do() {
    let h = |msg: &str, kind| {
        failure_hint(
            msg,
            kind,
            "http://localhost:4555/v1",
            "qwen3",
            Some("deepseek"),
        )
    };
    assert!(
        h(
            "connecting to x: tcp connect error: Connection refused (os error 61)",
            ProviderKind::Openai
        )
        .unwrap()
        .contains("Can't reach the model server at http://localhost:4555/v1")
    );
    assert!(
        h(
            "Ollama error 404 Not Found: {\"error\":\"model 'qwen3' not found\"}",
            ProviderKind::Ollama
        )
        .unwrap()
        .contains("ollama pull qwen3")
    );
    assert!(
        h(
            "API error 401 Unauthorized: Authentication Fails",
            ProviderKind::Openai
        )
        .unwrap()
        .contains("chanto login deepseek")
    );
    assert!(
        h(
            "API error 404 Not Found: model does not exist",
            ProviderKind::Openai
        )
        .unwrap()
        .contains("doesn't recognize model `qwen3`")
    );
    assert!(
        h(
            "API error 400 Bad Request: Invalid model name passed in model=qwen3",
            ProviderKind::Openai
        )
        .unwrap()
        .contains("`chanto models deepseek`")
    );
    assert_eq!(
        h(
            "API error 400 Bad Request: max_tokens is too large for this model",
            ProviderKind::Openai
        ),
        None
    );
    assert_eq!(h("API error 500: boom", ProviderKind::Openai), None);
    // A custom provider section can't be logged in to.
    assert!(
        failure_hint(
            "API error 401 Unauthorized",
            ProviderKind::Openai,
            "u",
            "m",
            Some("work")
        )
        .unwrap()
        .contains("chanto doctor")
    );
}

#[test]
fn a_dropped_stream_says_what_to_do() {
    let hint = failure_hint(
        "error decoding response body: error reading a body from connection: connection reset",
        ProviderKind::Openai,
        "http://localhost:4555/v1",
        "qwen3",
        Some("deepseek"),
    )
    .unwrap();
    assert!(
        hint.starts_with("Lost the connection to the model server at http://localhost:4555/v1"),
        "{hint}"
    );
    assert!(hint.contains("retry the turn"), "{hint}");
    // A body that ended before the provider's end-of-message event.
    let hint = failure_hint(
        &format!("{CUT_OFF} its reply (no message_stop)"),
        ProviderKind::Anthropic,
        "https://api.anthropic.com",
        "m",
        None,
    )
    .unwrap();
    assert!(hint.starts_with("Lost the connection"), "{hint}");
}

/// A server that waits `prefill`, sends `head` (status line, headers and
/// the first chunk), then goes silent with the connection open.
async fn stalling_server(prefill: Duration, head: String) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let head = head.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 16384];
                loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let s = String::from_utf8_lossy(&buf);
                    if let Some(h) = s.find("\r\n\r\n") {
                        let cl = s[..h]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buf.len() >= h + 4 + cl {
                            break;
                        }
                    }
                }
                tokio::time::sleep(prefill).await;
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.flush().await;
                tokio::time::sleep(Duration::from_secs(3600)).await;
                drop(sock);
            });
        }
    });
    format!("http://{addr}")
}

fn stall_req() -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: vec![Message::user("hi")],
        tools: vec![],
        think: None,
        effort: None,
        temperature: None,
        max_tokens: None,
        json: false,
    }
}

#[tokio::test]
async fn a_stream_that_stalls_after_starting_times_out() {
    let sse_head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n".to_string();
    let ndjson_head = "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\n\r\n{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":false}\n".to_string();
    for (kind, head, path) in [
        (ProviderKind::Openai, sse_head, "/v1"),
        (ProviderKind::Ollama, ndjson_head, ""),
    ] {
        let base = stalling_server(Duration::ZERO, head).await;
        let mut p = Provider::new(kind, &format!("{base}{path}"), None, 8192);
        p.stream_idle = Some(Duration::from_millis(300));
        let t = std::time::Instant::now();
        let r = tokio::time::timeout(Duration::from_secs(10), p.chat(&stall_req(), &mut |_| {}))
            .await
            .expect("the stall must not hang");
        let e = r.expect_err("a stalled stream fails");
        assert!(
            e.downcast_ref::<StreamStalled>().is_some(),
            "{kind:?}: {e:#}"
        );
        assert!(format!("{e:#}").contains("stream_idle_timeout"), "{e:#}");
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    }
}

#[tokio::test]
async fn the_idle_limit_waits_out_a_slow_first_byte() {
    // Prefill (no bytes at all yet) longer than the idle limit is fine.
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let base = stalling_server(Duration::from_millis(900), head).await;
    let mut p = Provider::new(ProviderKind::Openai, &format!("{base}/v1"), None, 8192);
    p.stream_idle = Some(Duration::from_millis(300));
    let r = tokio::time::timeout(Duration::from_secs(10), p.chat(&stall_req(), &mut |_| {}))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.text, "ok");
}

#[test]
fn stream_idle_timeout_config() {
    let mut c = crate::config::Config::default();
    assert_eq!(c.stream_idle(), Some(STREAM_IDLE_DEFAULT));
    c.stream_idle_timeout = Some(0.0);
    assert_eq!(c.stream_idle(), None);
    c.stream_idle_timeout = Some(30.0);
    assert_eq!(c.stream_idle(), Some(Duration::from_secs(30)));
}
