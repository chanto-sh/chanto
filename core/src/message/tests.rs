use super::*;

#[test]
fn a_message_reads_back_as_it_was_saved() {
    let call = ToolCall {
        id: "c1".into(),
        name: "Read".into(),
        arguments: serde_json::json!({"file_path": "a.rs"}),
    };
    let mut m = Message::assistant("reading");
    m.tool_calls = vec![call.clone()];
    let tool = Message::tool(&call, "contents");
    let json = serde_json::to_string(&[m, tool]).unwrap();
    // The shape sessions on disk use: lowercase roles, plain-string content,
    // empty fields left out.
    assert!(
        json.contains(r#""role":"assistant","content":"reading""#),
        "{json}"
    );
    assert!(!json.contains("images"), "{json}");
    let back: Vec<Message> = serde_json::from_str(&json).unwrap();
    assert_eq!(back[1].role, Role::Tool);
    assert_eq!(back[1].tool_call_id.as_deref(), Some("c1"));
    assert_eq!(*back[1].content, "contents");
    assert_eq!(ProviderKind::parse("openai"), Some(ProviderKind::Openai));
}
