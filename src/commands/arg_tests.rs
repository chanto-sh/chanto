use super::{
    BUILTIN, LoopCmd, McpRow, McpState, SHORTCUTS, expand_args, expand_skill_args, failed_detail,
    is_immediate, parse_cmd, parse_loop, shortcuts_markdown,
};

#[test]
fn suggests_the_closest_command() {
    let names = || BUILTIN.iter().map(|c| c.name.to_string());
    use crate::util::closest;
    assert_eq!(closest("comapct", names()).as_deref(), Some("compact"));
    assert_eq!(closest("reivew", names()).as_deref(), Some("review"));
    assert_eq!(closest("Doctr", names()).as_deref(), Some("doctor"));
    assert_eq!(closest("xyzzy", names()), None);
}

#[test]
fn skill_args_count_from_zero() {
    assert_eq!(
        expand_skill_args("Migrate $0 from $1 to $2.", "Button react vue"),
        "Migrate Button from react to vue."
    );
    assert_eq!(
        expand_skill_args("All: $ARGUMENTS; first: $ARGUMENTS[0]", "a \"b c\""),
        "All: a \"b c\"; first: a"
    );
    assert_eq!(
        expand_skill_args("Second: $ARGUMENTS[1]", "a \"b c\""),
        "Second: b c"
    );
    assert_eq!(
        expand_skill_args("Review", "src/"),
        "Review\n\nARGUMENTS: src/"
    );
    assert_eq!(expand_skill_args("Review", ""), "Review");
}

#[test]
fn template_args() {
    assert_eq!(expand_args("Fix $ARGUMENTS", "bug 1"), "Fix bug 1");
    assert_eq!(expand_args("a=$1 b=$2", "x \"y z\""), "a=x b=y z");
    assert_eq!(
        expand_args("ticket $TICKET", "TICKET=ABC-1"),
        "ticket ABC-1"
    );
    assert_eq!(expand_args("Review", "src/"), "Review\n\nsrc/");
    assert_eq!(expand_args("Review", ""), "Review");
}

#[test]
fn parse_cmd_splits_name_and_args() {
    assert_eq!(parse_cmd("/model gpt-5"), ("model", "gpt-5"));
    assert_eq!(parse_cmd("/help"), ("help", ""));
    assert_eq!(parse_cmd("/model   gpt-5  "), ("model", "gpt-5"));
}

#[test]
fn mcp_rows_describe_each_state() {
    let row = |state, detail: &str| {
        McpRow {
            name: "srv".into(),
            state,
            detail: detail.into(),
            http: false,
            can_enable: false,
        }
        .markdown_line()
    };
    assert_eq!(
        row(McpState::Connected, "3 tools"),
        "- **srv** ✓ connected — 3 tools\n"
    );
    assert_eq!(
        row(McpState::NeedsLogin, "not signed in"),
        "- **srv** ! needs login — run `/mcp login srv`\n"
    );
    assert_eq!(
        row(McpState::Failed, "401"),
        "- **srv** ✗ failed: 401 — `/mcp reconnect srv`\n"
    );
    assert_eq!(
        row(McpState::Off, "turned off"),
        "- **srv** ⊘ disabled: turned off\n"
    );
    assert_eq!(
        row(McpState::Pending, ".mcp.json"),
        "- **srv** ⏸ pending approval · .mcp.json — `/mcp enable srv` approves it for this project\n"
    );
    assert_eq!(
        row(McpState::Rejected, ".mcp.json"),
        "- **srv** ✘ rejected · .mcp.json\n"
    );
}

#[test]
fn failure_reasons_drop_the_servers_own_name() {
    assert_eq!(
        failed_detail("srv", "MCP server `srv` exited"),
        "the process exited"
    );
    assert_eq!(
        failed_detail("srv", "MCP server `srv`: bad handshake"),
        "bad handshake"
    );
    // Another server's name is part of the reason, so it stays.
    assert_eq!(
        failed_detail("srv", "MCP server `proxy` is not running"),
        "MCP server `proxy` is not running"
    );
    assert_eq!(
        failed_detail("srv", "timed out after 30s during startup"),
        "timed out after 30s during startup"
    );
}

#[test]
fn only_env_commands_are_immediate() {
    // These are safe to run while a turn holds the agent lock.
    for c in [
        "/model gpt-5",
        "/models",
        "/think off",
        "/mode plan",
        "/theme light",
        "/help",
        "/exit",
        "/quiet on",
        "/mcp",
    ] {
        assert!(is_immediate(c), "{c} should apply immediately");
    }
    // These rewrite the conversation and must wait for the agent. Queuing
    // them is what keeps history consistent; they must never be treated as
    // env-only, or they would silently run against a busy agent.
    for c in [
        "/compact",
        "/clear",
        "/rewind",
        "/resume",
        "/undo",
        "/context",
        "/cost",
        "/export",
        "/transcript",
    ] {
        assert!(!is_immediate(c), "{c} must not bypass the agent lock");
    }
}

#[test]
fn immediate_commands_are_all_handled_by_handle_env() {
    // Guards against is_immediate() listing something handle_env doesn't
    // serve, which would run it immediately and then do nothing at all.
    for c in [
        "/model", "/models", "/think", "/mode", "/theme", "/vision", "/help", "/exit", "/quit",
        "/?",
    ] {
        assert!(is_immediate(c), "{c}");
    }
    assert!(!is_immediate("/compact"));
}

#[test]
fn shortcuts_are_unique_and_documented() {
    // The TUI's `?` overlay and `/help` both render this list, so a blank or
    // duplicated row would show up in both places.
    let md = shortcuts_markdown();
    let mut seen = std::collections::HashSet::new();
    for (k, d) in SHORTCUTS {
        assert!(
            !k.trim().is_empty() && !d.trim().is_empty(),
            "empty shortcut row"
        );
        assert!(seen.insert(*k), "duplicate shortcut key: {k}");
        assert!(md.contains(*k), "{k} missing from the markdown form");
    }
}

#[test]
fn loop_intervals_lead_or_trail_the_prompt() {
    let start = |secs: u64, prompt: &str| {
        Ok(LoopCmd::Start {
            every: Some(std::time::Duration::from_secs(secs)),
            prompt: Some(prompt.into()),
        })
    };
    assert_eq!(
        parse_loop("5m check the CI and fix failures"),
        start(300, "check the CI and fix failures")
    );
    assert_eq!(parse_loop("2h /review-pr 12"), start(7200, "/review-pr 12"));
    assert_eq!(parse_loop("1d  summarize"), start(86_400, "summarize"));
    assert_eq!(
        parse_loop("check the deploy every 2 hours"),
        start(7200, "check the deploy")
    );
    assert_eq!(
        parse_loop("check the deploy every 15m"),
        start(900, "check the deploy")
    );
    assert_eq!(
        parse_loop("check the deploy every hour"),
        start(3600, "check the deploy")
    );
    // Seconds round up to whole minutes.
    assert_eq!(parse_loop("30s ping"), start(60, "ping"));
    assert_eq!(parse_loop("90s ping"), start(120, "ping"));
    // No interval: self-paced, and "every" mid-sentence is just words.
    assert_eq!(
        parse_loop("look at every file"),
        Ok(LoopCmd::Start {
            every: None,
            prompt: Some("look at every file".into())
        })
    );
    assert!(
        parse_loop("3 bugs to fix")
            .unwrap_err()
            .contains("isn't an interval")
    );
}

#[test]
fn loop_stop_status_and_bad_input() {
    assert_eq!(
        parse_loop(""),
        Ok(LoopCmd::Start {
            every: None,
            prompt: None
        })
    );
    assert_eq!(parse_loop("status"), Ok(LoopCmd::Status));
    assert_eq!(parse_loop("stop"), Ok(LoopCmd::Stop));
    assert_eq!(parse_loop(" cancel "), Ok(LoopCmd::Stop));
    assert!(
        parse_loop("0m ping")
            .unwrap_err()
            .contains("isn't an interval")
    );
    assert!(parse_loop("5x ping").is_err());
    assert!(parse_loop("8d ping").is_err());
    assert_eq!(
        parse_loop("7d ping"),
        Ok(LoopCmd::Start {
            every: Some(std::time::Duration::from_secs(7 * 86_400)),
            prompt: Some("ping".into())
        })
    );
    // An interval alone runs the default prompt on that schedule.
    assert_eq!(
        parse_loop("5m"),
        Ok(LoopCmd::Start {
            every: Some(std::time::Duration::from_secs(300)),
            prompt: None
        })
    );
    assert_eq!(
        parse_loop("every 5m"),
        Ok(LoopCmd::Start {
            every: Some(std::time::Duration::from_secs(300)),
            prompt: None
        })
    );
}
