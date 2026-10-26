use super::*;
use crate::llm::mock::{serve, sse};

async fn response(kind: ProviderKind, events: Vec<Value>) -> (ChatResponse, String) {
    let body = sse(&events
        .into_iter()
        .map(|v| ("message", v))
        .collect::<Vec<_>>());
    let (base, _) = serve(vec![(200, body)]).await;
    let provider = Provider::new(kind, &base, None, 8192);
    let req = ChatRequest {
        model: "mock".into(),
        messages: vec![Message::user("hello")],
        tools: vec![],
        think: None,
        effort: None,
        temperature: None,
        max_tokens: None,
        json: false,
    };
    let mut streamed = String::new();
    let out = provider
        .chat(&req, &mut |d| {
            if let Delta::Text(t) = d {
                streamed.push_str(&t);
            }
        })
        .await
        .unwrap();
    assert_eq!(streamed, out.text);
    assert!(out.tool_calls.is_empty());
    (out, streamed)
}

#[tokio::test]
async fn safety_refusal_text_and_links_survive_both_openai_protocols() {
    let notice = "This content can't be shown. Learn more: https://example.com/access";
    let (out, _) = response(
        ProviderKind::Openai,
        vec![
            json!({"choices":[{"delta":{"refusal":notice}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        ],
    )
    .await;
    assert_eq!(out.text, notice);
    assert!(out.safety_stopped());
    for events in [
        vec![
            json!({"type":"response.refusal.delta","delta":"This content can't be shown. "}),
            json!({"type":"response.refusal.delta","delta":"Learn more: https://example.com/access"}),
            json!({"type":"response.refusal.done","refusal":notice}),
        ],
        vec![json!({"type":"response.refusal.done","refusal":notice})],
        vec![
            json!({"type":"response.refusal.delta","delta":""}),
            json!({"type":"response.refusal.done","refusal":notice}),
        ],
        vec![
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"refusal","refusal":notice}]}}),
        ],
        vec![
            json!({"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"refusal","refusal":notice}]}]}}),
        ],
    ] {
        let (out, _) = response(ProviderKind::OpenaiResponses, events).await;
        assert_eq!(out.text, notice);
        assert_eq!(out.finish_reason.as_deref(), Some("refusal"));
    }
}

#[tokio::test]
async fn safety_filters_show_a_notice_after_partial_or_empty_answers() {
    for text in ["", "Partial answer"] {
        for kind in [ProviderKind::Openai, ProviderKind::OpenaiResponses] {
            let events = if kind == ProviderKind::Openai {
                vec![
                    json!({"choices":[{"delta":{"content":text,"tool_calls":[{"index":0,"id":"c1","function":{"name":"Write","arguments":"{}"}}]},"finish_reason":"content_filter"}]}),
                ]
            } else {
                vec![
                    json!({"type":"response.output_text.delta","delta":text}),
                    json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"Write","arguments":"{}"}}),
                    json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}),
                ]
            };
            let (out, _) = response(kind, events).await;
            assert!(out.text.starts_with(text));
            assert!(out.text.contains("provider's safety filter"));
            assert!(out.safety_stopped());
        }
    }
}

#[tokio::test]
async fn safety_empty_refusals_are_visible() {
    let (out, _) = response(
        ProviderKind::OpenaiResponses,
        vec![json!({"type":"response.refusal.done","refusal":""})],
    )
    .await;
    assert!(out.text.contains("declined to respond"));
    let (out, _) = response(
        ProviderKind::Anthropic,
        vec![
            json!({"type":"message_delta","delta":{"stop_reason":"refusal"}}),
            json!({"type":"message_stop"}),
        ],
    )
    .await;
    assert!(out.text.contains("declined to respond"));
}

#[tokio::test]
async fn safety_notices_do_not_change_normal_answers() {
    let (out, _) = response(
        ProviderKind::Openai,
        vec![json!({"choices":[{"delta":{"content":"hello"},"finish_reason":"stop"}]})],
    )
    .await;
    assert_eq!(out.text, "hello");
    assert!(!out.safety_stopped());
}
