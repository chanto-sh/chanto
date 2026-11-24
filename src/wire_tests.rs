use super::*;
use serde_json::json;

#[test]
fn events_keep_the_shapes_the_server_has_always_sent() {
    let lines = vec!["working".to_string()];
    let options = vec!["yes".to_string(), "no".to_string()];
    let input = json!({"path": "a.txt"});
    let cases = [
        (
            StreamEvent::AssistantStart,
            json!({"type": "assistant_start"}),
        ),
        (
            StreamEvent::Text { text: "hi" },
            json!({"type": "text", "text": "hi"}),
        ),
        (
            StreamEvent::ToolStart {
                id: "t1",
                label: "Read(a.txt)",
                parent: None,
            },
            json!({"type": "tool_start", "id": "t1", "label": "Read(a.txt)", "parent": null}),
        ),
        (
            StreamEvent::ToolEnd {
                id: "t1",
                is_error: false,
                output: "1\thi".into(),
                display: "Read 1 line",
                images: vec![],
                parent: Some("t0"),
            },
            json!({"type": "tool_end", "id": "t1", "is_error": false, "output": "1\thi",
                   "display": "Read 1 line", "images": [], "parent": "t0"}),
        ),
        (
            StreamEvent::Busy { label: None },
            json!({"type": "busy", "label": null}),
        ),
        (
            StreamEvent::Warning { message: "w" },
            json!({"type": "warning", "message": "w"}),
        ),
        (
            StreamEvent::Mode {
                mode: crate::permissions::Mode::Bypass.into(),
            },
            json!({"type": "mode", "mode": "bypassPermissions"}),
        ),
        (
            StreamEvent::Todos {
                todos: vec![Todo {
                    content: "a",
                    status: "pending",
                    active_form: "Doing a",
                }],
            },
            json!({"type": "todos", "todos": [{"content": "a", "status": "pending", "activeForm": "Doing a"}]}),
        ),
        (
            StreamEvent::BackgroundAgent {
                id: "b1",
                description: "d",
                status: "running",
            },
            json!({"type": "background_agent", "id": "b1", "description": "d", "status": "running"}),
        ),
        (
            StreamEvent::Status {
                key: "goal",
                lines: &lines,
            },
            json!({"type": "status", "key": "goal", "lines": ["working"]}),
        ),
        (
            StreamEvent::PermissionRequest {
                request_id: "req-1".into(),
                tool: "Bash",
                label: "Bash(ls)",
                detail: "ls",
                reason: None,
                always_label: "ls",
            },
            json!({"type": "permission_request", "request_id": "req-1", "tool": "Bash", "label": "Bash(ls)",
                   "detail": "ls", "reason": null, "always_label": "ls"}),
        ),
        (
            StreamEvent::Question {
                request_id: "req-2".into(),
                question: "Which?",
                options: &options,
            },
            json!({"type": "question", "request_id": "req-2", "question": "Which?", "options": ["yes", "no"]}),
        ),
        (
            StreamEvent::ToolCall {
                id: "c1",
                name: "lookup",
                input: &input,
            },
            json!({"type": "tool_call", "id": "c1", "name": "lookup", "input": {"path": "a.txt"}}),
        ),
        // A turn_done carries only what happened, then the cost.
        (
            StreamEvent::TurnDone(TurnDone {
                result: Some("done".into()),
                total_cost_usd: 0.25,
                ..Default::default()
            }),
            json!({"type": "turn_done", "result": "done", "total_cost_usd": 0.25, "price_unknown": false}),
        ),
    ];
    for (event, want) in cases {
        assert_eq!(event.to_value(), want);
    }
}

#[test]
fn request_bodies_read_the_names_the_routes_document() {
    let body: CreateSession = serde_json::from_value(json!({
        "model": "m", "mode": "acceptEdits", "cwd": "/w", "max_turns": 3, "verify": true,
        "tools": [{"name": "lookup", "description": "d", "input_schema": {"type": "object"}}]
    }))
    .unwrap();
    assert!(matches!(body.mode, Some(Mode::AcceptEdits)));
    assert_eq!(body.tools.unwrap()[0].name, "lookup");
    let r: Respond =
        serde_json::from_value(json!({"request_id": "req-1", "decision": "allow_always"})).unwrap();
    assert!(matches!(r.decision, Some(Decision::AllowAlways)));
    let w: Rewind =
        serde_json::from_value(json!({"checkpoint": 1, "mode": "code", "preview": true})).unwrap();
    assert!(matches!(w.mode, Some(RewindMode::Code)));
}

/// `sdk/schema.json` must be what these types generate: the SDKs' types come
/// from it. `UPDATE_SCHEMA=1 cargo test --features schema --lib wire::` rewrites it.
#[cfg(feature = "schema")]
#[test]
fn the_committed_schema_is_current() {
    let schema = schemars::schema_for!(Api);
    let text = serde_json::to_string_pretty(&schema).unwrap() + "\n";
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sdk/schema.json");
    if std::env::var_os("UPDATE_SCHEMA").is_some() {
        std::fs::write(&path, &text).unwrap();
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == text,
        "sdk/schema.json is out of date: run `UPDATE_SCHEMA=1 cargo test --features schema --lib wire::`, \
         then `python3 sdk/gen_types.py`"
    );
}
