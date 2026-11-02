use super::{parse_stream_input, stream_input_error};

#[test]
fn string_content_is_the_prompt() {
    let m = parse_stream_input(
        r#"{"type":"user","message":{"role":"user","content":"fix the tests"}}"#,
    )
    .unwrap()
    .unwrap();
    assert_eq!(m, ("fix the tests".to_string(), vec![]));
}

#[test]
fn content_blocks_join_text_and_collect_base64_images() {
    let line = r#"{"type":"user","session_id":"x","message":{"role":"user","content":[{"type":"text","text":"what is this?"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0K"}},{"type":"text","text":"be brief"}]}}"#;
    let (text, images) = parse_stream_input(line).unwrap().unwrap();
    assert_eq!(text, "what is this?\nbe brief");
    assert_eq!(images, vec!["iVBORw0K".to_string()]);
}

#[test]
fn blank_lines_are_skipped() {
    assert_eq!(parse_stream_input("").unwrap(), None);
    assert_eq!(parse_stream_input("   \r").unwrap(), None);
}

#[test]
fn malformed_messages_are_rejected() {
    for line in [
        "not json",
        r#"{"message":{"content":"hi"}}"#,
        r#"{"type":"control_request","request":{}}"#,
        r#"{"type":"user","message":{"role":"assistant","content":"hi"}}"#,
        r#"{"type":"user","message":{"role":"user"}}"#,
        r#"{"type":"user","message":{"role":"user","content":"  "}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"x"}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"image","source":{"type":"url","url":"https://x/y.png"}}]}}"#,
    ] {
        assert!(parse_stream_input(line).is_err(), "{line}");
    }
}

#[test]
fn stream_input_needs_print_and_stream_json_output() {
    assert_eq!(stream_input_error("text", false, "text", false), None);
    assert_eq!(
        stream_input_error("stream-json", true, "stream-json", false),
        None
    );
    assert!(stream_input_error("stream-json", false, "stream-json", false).is_some());
    assert!(stream_input_error("stream-json", true, "json", false).is_some());
    assert!(stream_input_error("stream-json", true, "stream-json", true).is_some());
}
