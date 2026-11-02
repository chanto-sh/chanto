#[test]
fn upfront_builtins_follow_the_request_and_the_project() {
    let dir =
        std::env::temp_dir().join(format!("chanto-upfront-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.py"), "").unwrap();
    assert!(upfront_builtins(&dir, "fix the parser", &[]).is_empty());
    assert_eq!(
        upfront_builtins(&dir, "Take a screenshot of http://localhost:3000", &[]),
        ["Browser"]
    );
    assert_eq!(
        upfront_builtins(&dir, "start the dev server and watch it", &[]),
        ["Monitor"]
    );
    assert_eq!(
        upfront_builtins(&dir, "fix the parser", &["RepoMap".into()]),
        ["RepoMap"]
    );
    std::fs::write(dir.join("eda.ipynb"), "{}").unwrap();
    assert_eq!(
        upfront_builtins(&dir, "fix the parser", &[]),
        ["NotebookEdit"]
    );
    for i in 0..=LARGE_REPO {
        std::fs::write(dir.join(format!("f{i}.txt")), "").unwrap();
    }
    assert_eq!(
        upfront_builtins(&dir, "fix the parser", &[]),
        ["NotebookEdit", "RepoMap", "SemanticSearch"]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

use super::*;

fn spec(name: &str, desc: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: desc.into(),
        parameters: json!({"type": "object"}),
    }
}

#[test]
fn settings_parse() {
    assert_eq!(Setting::parse("auto"), Some(Setting::Auto(10.0)));
    assert_eq!(Setting::parse("auto:5"), Some(Setting::Auto(5.0)));
    assert_eq!(Setting::parse("TRUE"), Some(Setting::On));
    assert_eq!(Setting::parse("false"), Some(Setting::Off));
    assert_eq!(Setting::parse("auto:200"), None);
    let big = vec![spec("a", &"x".repeat(4000))];
    assert!(Setting::Auto(10.0).defers(&big, 8192));
    assert!(!Setting::Auto(10.0).defers(&big, 128_000));
    assert!(!Setting::On.defers(&[], 1));
}

#[test]
fn select_and_keyword_search() {
    let tools = vec![
        spec("mcp__github__create_issue", "Create a GitHub issue"),
        spec("mcp__github__list_prs", "List pull requests"),
        spec("mcp__slack__send_message", "Send a Slack message"),
        spec(
            "mcp__mail__send",
            "Send an email; also posts to slack channels",
        ),
    ];
    let (f, m) = search("select:mcp__github__list_prs, nope", 5, &tools);
    assert_eq!(
        f.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        ["mcp__github__list_prs"]
    );
    assert_eq!(m, ["nope"]);
    let (f, _) = search("slack send", 5, &tools);
    assert_eq!(f[0].name, "mcp__slack__send_message");
    let (f, _) = search("+slack send", 5, &tools);
    assert_eq!(f.len(), 1);
    let (f, _) = search("github", 1, &tools);
    assert_eq!(f.len(), 1);
    let (f, _) = search("pull requests", 5, &tools);
    assert_eq!(f[0].name, "mcp__github__list_prs");
}

#[test]
fn announcements_and_loads_are_read_back() {
    let tools = vec![spec("mcp__a__x", "Does x\nmore"), spec("mcp__a__y", "")];
    let note = announcement(&tools.iter().collect::<Vec<_>>());
    assert!(note.contains("mcp__a__x — Does x\n"));
    let mut msgs = vec![Message::user(format!("hi\n\n{note}"))];
    assert_eq!(
        announced(&msgs),
        HashSet::from(["mcp__a__x".to_string(), "mcp__a__y".to_string()])
    );
    let call = crate::llm::ToolCall {
        id: "1".into(),
        name: NAME.into(),
        arguments: json!({}),
    };
    msgs.push(Message::tool(
        &call,
        "Loaded 1 tool(s)\n{\"name\":\"mcp__a__y\",\"description\":\"\"}",
    ));
    let mut a = Message::assistant("");
    a.tool_calls = vec![crate::llm::ToolCall {
        id: "2".into(),
        name: "mcp__a__x".into(),
        arguments: json!({}),
    }];
    msgs.push(a);
    assert_eq!(loaded(&msgs, &tools), ["mcp__a__y", "mcp__a__x"]);
}
