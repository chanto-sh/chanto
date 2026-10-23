use super::verdict::{Verdict, hook_prompt, parse_verdict};
use super::*;

#[test]
fn prompt_hook_text_and_verdicts() {
    let payload = json!({"hook_event_name": "Stop", "x": 1});
    let p = hook_prompt("Check $ARGUMENTS costs \\$1.00", &payload);
    assert!(
        p.contains("\"hook_event_name\": \"Stop\"") && p.ends_with("costs $1.00"),
        "{p}"
    );
    let p = hook_prompt("Literal \\$ARGUMENTS", &payload);
    assert!(
        p.starts_with("Literal $ARGUMENTS\n\n{"),
        "without a placeholder the input is appended: {p}"
    );
    assert_eq!(
        parse_verdict("sure: {\"ok\": false, \"reason\": \"tests fail\"} done"),
        Some(Verdict {
            ok: false,
            reason: "tests fail".into(),
            impossible: false
        })
    );
    assert_eq!(
        parse_verdict("{\"ok\": true}"),
        Some(Verdict {
            ok: true,
            reason: String::new(),
            impossible: false
        })
    );
    assert_eq!(parse_verdict("no json"), None);
    assert_eq!(parse_verdict("{\"decision\": \"block\"}"), None);
}

#[test]
fn verdicts_act_per_event() {
    let prompt = HookConfig {
        kind: "prompt".into(),
        prompt: "p".into(),
        ..Default::default()
    };
    let agent = HookConfig {
        kind: "agent".into(),
        prompt: "p".into(),
        ..Default::default()
    };
    let cont = HookConfig {
        continue_on_block: true,
        ..prompt.clone()
    };
    let no = |impossible| Verdict {
        ok: false,
        reason: "why".into(),
        impossible,
    };
    let run = |event: &str, h: &HookConfig, v: Verdict| {
        let mut r = HookResult::default();
        apply_verdict(&mut r, event, h, v);
        r
    };
    assert_eq!(
        run("Stop", &prompt, no(false)).block.as_deref(),
        Some("why")
    );
    assert!(
        run("Stop", &prompt, no(true)).block.is_none(),
        "impossible lets the turn end"
    );
    assert_eq!(
        run("SubagentStop", &agent, no(true)).block.as_deref(),
        Some("why"),
        "agent hooks ignore impossible"
    );
    // PreToolUse: ends the turn by default; continueOnBlock (and agent hooks) deny instead.
    let r = run("PreToolUse", &prompt, no(false));
    assert_eq!(
        (r.halt.as_deref(), r.decision.is_none()),
        (Some("why"), true)
    );
    assert_eq!(
        run("PreToolUse", &cont, no(false)).decision,
        Some(PermDecision::Deny("why".into()))
    );
    assert_eq!(
        run("PreToolUse", &agent, no(false)).decision,
        Some(PermDecision::Deny("why".into()))
    );
    assert_eq!(
        run("PostToolUse", &prompt, no(false)).halt.as_deref(),
        Some("why")
    );
    assert_eq!(
        run("PostToolUse", &cont, no(false)).block.as_deref(),
        Some("why")
    );
    assert_eq!(
        run("UserPromptSubmit", &prompt, no(false)).block.as_deref(),
        Some("why")
    );
    assert_eq!(
        run("TaskCreated", &prompt, no(false)).block.as_deref(),
        Some("why")
    );
    let r = run("PermissionRequest", &prompt, no(false));
    assert!(r.block.is_none() && r.halt.is_none() && r.decision.is_none());
    assert!(
        run(
            "Stop",
            &prompt,
            Verdict {
                ok: true,
                reason: String::new(),
                impossible: false
            }
        )
        .block
        .is_none()
    );
}

#[test]
fn handler_types_run_on_claude_codes_events() {
    let start = json!({"source": "startup"});
    let clear = json!({"source": "clear"});
    assert!(
        supported("prompt", "PreToolUse", &json!({}))
            && supported("prompt", "PermissionRequest", &json!({}))
    );
    assert!(
        !supported("agent", "PermissionRequest", &json!({}))
            && supported("agent", "Stop", &json!({}))
    );
    assert!(
        !supported("prompt", "SessionStart", &start)
            && !supported("prompt", "Notification", &json!({}))
    );
    assert!(!supported("http", "SessionStart", &start) && !supported("http", "Setup", &json!({})));
    assert!(
        !supported("mcp_tool", "SessionStart", &start)
            && supported("mcp_tool", "SessionStart", &clear)
    );
    assert!(
        !supported("mcp_tool", "Setup", &json!({}))
            && supported("mcp_tool", "Notification", &json!({}))
    );
    assert!(supported("command", "Setup", &json!({})));
}

#[test]
fn mcp_tool_inputs_fill_from_the_hook_input() {
    let payload = json!({"tool_input": {"file_path": "/p/a.rs", "n": 3}, "cwd": "/p"});
    let input = json!({"file": "${tool_input.file_path}", "n": "${tool_input.n}", "nested": ["${cwd}", "${nope}"], "k": 1});
    assert_eq!(
        fill_input(&input, &payload),
        json!({"file": "/p/a.rs", "n": "3", "nested": ["/p", ""], "k": 1})
    );
}

#[test]
fn updated_tool_output_must_fit_the_tool() {
    let bash =
        json!({"stdout": "[redacted]", "stderr": "warn", "interrupted": false, "isImage": false});
    assert_eq!(
        replaced_output("Bash", &bash, false).as_deref(),
        Some("[redacted]\nwarn")
    );
    assert_eq!(
        replaced_output("Bash", &json!("plain"), false),
        None,
        "wrong shape is ignored"
    );
    assert_eq!(
        replaced_output(
            "Read",
            &json!({"type": "text", "file": {"filePath": "x", "content": "c"}}),
            false
        )
        .as_deref(),
        Some("c")
    );
    assert_eq!(
        replaced_output("Grep", &json!({"output": "o"}), false).as_deref(),
        Some("o")
    );
    assert_eq!(
        replaced_output(
            "mcp__s__t",
            &json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]),
            true
        )
        .as_deref(),
        Some("a\nb")
    );
    assert_eq!(
        replaced_output("mcp__s__t", &json!("t"), false).as_deref(),
        Some("t")
    );
    assert_eq!(
        replaced_output("Bash", &bash, true),
        None,
        "updatedMCPToolOutput is for MCP tools only"
    );
    assert_eq!(
        async_json("progress\n{\"systemMessage\": \"m\"}\n").unwrap()["systemMessage"],
        "m"
    );
    assert_eq!(
        async_json("{\n  \"systemMessage\": \"m\"\n}").unwrap()["systemMessage"],
        "m"
    );
}
