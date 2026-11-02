use super::*;

#[test]
fn review_args() {
    let a = parse_review("high --fix --max-findings 7 main...feature").unwrap();
    assert_eq!(a.level.as_deref(), Some("high"));
    assert!(a.fix && !a.comment);
    assert_eq!(a.max_findings.as_deref(), Some("7"));
    assert_eq!(a.target, "main...feature");
    let a = parse_review("/fix-issue 123").unwrap();
    assert_eq!(
        a.target, "/fix-issue 123",
        "the rest is the target, even a command"
    );
    assert!(a.level.is_none());
    let a = parse_review("--comment 1234").unwrap();
    assert!(a.comment && a.target == "1234");
    assert!(parse_review("ultra").unwrap().ultra);
    assert_eq!(parse_review("low src/high").unwrap().target, "src/high");
    assert_eq!(parse_review("").unwrap(), ReviewArgs::default());
    assert!(parse_review("--max-findings lots").is_err());
    assert_eq!(
        parse_review("--max-findings all")
            .unwrap()
            .max_findings
            .as_deref(),
        Some("all")
    );
}

#[test]
fn review_prompts() {
    let a = parse_review("--fix").unwrap();
    let p = review_prompt(&a, "low", None);
    assert!(
        p.contains("at most 5") && p.contains("apply the fixes") && !p.contains("Do not modify")
    );
    let p = review_prompt(&parse_review("1234").unwrap(), "max", Some("all"));
    assert!(p.contains("`1234`") && p.contains("every finding") && p.contains("Do not modify"));
    assert!(
        review_prompt(&parse_review("--comment").unwrap(), "medium", Some("3")).contains("gh api")
    );
}

#[test]
fn tallies_commands_and_mcp_tools() {
    use crate::llm::{Message, Role, ToolCall};
    let call = |name: &str, args: serde_json::Value| ToolCall {
        id: "1".into(),
        name: name.into(),
        arguments: args,
    };
    let mut m = Message::new(Role::Assistant, String::new());
    m.tool_calls = vec![
        call(
            "Bash",
            serde_json::json!({"command": "git status --short && git diff | head -5"}),
        ),
        call(
            "Bash",
            serde_json::json!({"command": "FOO=1 cargo test -p x; ls -la"}),
        ),
        call(
            "Bash",
            serde_json::json!({"command": "cd src && git status"}),
        ),
        call("mcp__gh__list_issues", serde_json::json!({})),
        call("Read", serde_json::json!({"file_path": "x"})),
    ];
    let (bash, mcp) = tally(&[m]);
    assert_eq!(bash["git status"], 2);
    assert_eq!(bash["git diff"], 1);
    assert_eq!(bash["head"], 1);
    assert_eq!(bash["cargo test"], 1);
    assert_eq!(bash["ls"], 1);
    assert!(!bash.contains_key("cd"));
    assert_eq!(mcp["mcp__gh__list_issues"], 1);
}

#[test]
fn status_line_is_saved_keeping_the_rest() {
    let dir = std::env::temp_dir().join(format!("chanto-sl-{}", uuid::Uuid::new_v4().simple()));
    let p = dir.join("config.toml");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        &p,
        "# mine\nmodel = \"m\"\nstatus_line = \"old\"\n\n[permissions]\nallow = [\"Read\"]\n",
    )
    .unwrap();
    set_status_line(&p, "/home/me/.chanto/statusline.sh").unwrap();
    let text = std::fs::read_to_string(&p).unwrap();
    let t: toml::Table = toml::from_str(&text).unwrap();
    assert_eq!(
        t["status_line"].as_str(),
        Some("/home/me/.chanto/statusline.sh")
    );
    assert_eq!(t["permissions"]["allow"][0].as_str(), Some("Read"));
    assert!(text.starts_with("# mine"));
    set_status_line(&dir.join("new/config.toml"), "x").unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bundled_names_are_distinct_from_builtins() {
    for b in BUNDLED {
        assert!(
            !crate::commands::BUILTIN.iter().any(|c| c.name == b.name),
            "/{} is both built-in and bundled",
            b.name
        );
    }
    assert!(batch_prompt("do it", 4).contains("isolation: \"worktree\""));
    assert!(simplify_prompt("").contains("four reviewers"));
}
