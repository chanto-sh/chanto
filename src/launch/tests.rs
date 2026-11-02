use super::*;

#[test]
fn restricted_drops_code_running_tools_unless_named() {
    let known: Vec<String> = [
        "Bash",
        "BashOutput",
        "Read",
        "Edit",
        "WebFetch",
        "WebSearch",
        "Browser",
        "Monitor",
        "Grep",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(kept_tools(None, false, false, &known), None);
    assert_eq!(
        kept_tools(None, false, true, &known).unwrap(),
        vec!["Read", "Edit", "WebSearch", "Grep"]
    );
    // --tools still names what it wants, Bash included.
    assert_eq!(
        kept_tools(Some("Bash,Read"), false, true, &known).unwrap(),
        vec!["Bash", "Read"]
    );
    assert_eq!(
        kept_tools(None, true, false, &known).unwrap(),
        vec!["Bash", "Read", "Edit"]
    );
    assert_eq!(
        kept_tools(None, true, true, &known).unwrap(),
        vec!["Read", "Edit"]
    );
    let safe = Launch {
        safe_mode: true,
        ..Default::default()
    };
    assert!(safe.no_customizations() && safe.skip_discovered_mcp() && safe.wants("user"));
    assert!(
        !Launch {
            restricted: true,
            ..Default::default()
        }
        .no_customizations()
    );
}

#[test]
fn agents_flag_is_a_team_size_or_subagent_json() {
    assert!(matches!(parse_agents("3").unwrap(), AgentsFlag::Team(3)));
    let AgentsFlag::Defs(d) = parse_agents(
        r#"{"reviewer":{"description":"Reviews code","prompt":"You review.","tools":["Read","Grep"],"model":"inherit"},
                "fixer":{"description":"Fixes","prompt":"You fix.","tools":"Read, Edit","disallowedTools":"Bash"}}"#,
    )
    .unwrap() else {
        panic!("expected definitions")
    };
    let r = d.iter().find(|a| a.name == "reviewer").unwrap();
    assert_eq!(
        r.tools.as_deref(),
        Some(&["Read".to_string(), "Grep".to_string()][..])
    );
    assert_eq!(r.model.as_deref(), Some("inherit"));
    let f = d.iter().find(|a| a.name == "fixer").unwrap();
    assert_eq!(
        f.tools.as_deref(),
        Some(&["Read".to_string(), "Edit".to_string()][..])
    );
    assert_eq!(f.disallowed_tools, vec!["Bash".to_string()]);
    for bad in [
        r#"{"x":{"prompt":"p"}}"#,
        r#"["x"]"#,
        r#"{"x":{"description":"d","prompt":"p","tools":3}}"#,
        "{nope",
        "no-such-file.json",
    ] {
        assert!(parse_agents(bad).is_err(), "{bad}");
    }
}

#[test]
fn agents_json_takes_the_frontmatter_fields() {
    let AgentsFlag::Defs(d) = parse_agents(
        r#"{"tester":{"description":"Tests","prompt":"You test.","permissionMode":"acceptEdits","maxTurns":7,"skills":["conventions"],
                "mcpServers":["github",{"playwright":{"type":"stdio","command":"npx","args":["-y","@playwright/mcp"]}}],
                "hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"./check.sh"}]}]},
                "memory":"project","background":true,"effort":"high","isolation":"worktree","initialPrompt":"/review","color":"Cyan","omitClaudeMd":true}}"#,
    )
    .unwrap() else {
        panic!("expected definitions")
    };
    let t = &d[0];
    assert_eq!(
        (
            t.permission_mode.as_deref(),
            t.max_turns,
            t.memory.as_deref(),
            t.effort.as_deref()
        ),
        (Some("acceptEdits"), Some(7), Some("project"), Some("high"))
    );
    assert_eq!(t.skills, vec!["conventions".to_string()]);
    assert!(matches!(&t.mcp_servers[0], crate::context::AgentMcp::Named(n) if n == "github"));
    assert!(
        matches!(&t.mcp_servers[1], crate::context::AgentMcp::Inline(n, c) if n == "playwright" && c.command == "npx")
    );
    assert_eq!(
        t.hooks.as_ref().unwrap()["PreToolUse"][0].command,
        "./check.sh"
    );
    assert!(t.background && t.omit_claude_md);
    assert_eq!(
        (
            t.isolation.as_deref(),
            t.initial_prompt.as_deref(),
            t.color.as_deref()
        ),
        (Some("worktree"), Some("/review"), Some("cyan"))
    );
    let dir =
        std::env::temp_dir().join(format!("chanto-initial-{}", uuid::Uuid::new_v4().simple()));
    let json = r#"{"tester":{"description":"Tests","prompt":"p","initialPrompt":"/review"}}"#;
    assert_eq!(
        initial_prompt(&dir, Some(json), "tester").as_deref(),
        Some("/review")
    );
    assert_eq!(initial_prompt(&dir, Some(json), "other"), None);
}

#[test]
fn agents_json_can_come_from_a_file() {
    let d = std::env::temp_dir().join(format!(
        "chanto-agents-{}.json",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&d, r#"{"r":{"description":"d","prompt":"p"}}"#).unwrap();
    assert!(
        matches!(parse_agents(d.to_str().unwrap()).unwrap(), AgentsFlag::Defs(v) if v.len() == 1)
    );
    let _ = std::fs::remove_file(d);
}

#[test]
fn settings_and_mcp_config_take_json_or_files() {
    let (_, t) = parse_settings(r#"{"permissions":{"allow":["Bash(ls:*)"],"defaultMode":"acceptEdits"},"model":"m1","env":{"A":"1"}}"#).unwrap();
    assert_eq!(t["mode"].as_str(), Some("acceptEdits"));
    assert_eq!(t["model"].as_str(), Some("m1"));
    assert_eq!(
        t["permissions"]["allow"].as_array().unwrap()[0].as_str(),
        Some("Bash(ls:*)")
    );
    assert!(parse_settings("[1]").is_err());
    let f = std::env::temp_dir().join(format!(
        "chanto-settings-{}.toml",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&f, "num_ctx = 777\n").unwrap();
    let (_, t) = parse_settings(f.to_str().unwrap()).unwrap();
    assert_eq!(t["num_ctx"].as_integer(), Some(777));
    let _ = std::fs::remove_file(f);

    let (_, s) = parse_mcp_config(r#"{"mcpServers":{"fs":{"command":"npx","args":["-y","x"]},"web":{"type":"http","url":"http://h/mcp"}}}"#).unwrap();
    assert_eq!(s["fs"].command, "npx");
    assert_eq!(s["web"].url.as_deref(), Some("http://h/mcp"));
    let (_, s) = parse_mcp_config(r#"{"fs":{"command":"npx"}}"#).unwrap();
    assert!(s.contains_key("fs"));
    assert!(parse_mcp_config(r#"{"mcpServers":{"x":{}}}"#).is_err());
    assert!(parse_mcp_config("/no/such/mcp.json").is_err());
}

#[test]
fn sources_tools_and_session_ids() {
    assert_eq!(parse_sources("user, local").unwrap(), vec!["user", "local"]);
    assert!(parse_sources("user,global").is_err());
    let l = Launch {
        sources: Some(vec!["user".into()]),
        ..Default::default()
    };
    assert!(l.wants("user") && !l.wants("project") && Launch::default().wants("local"));
    assert_eq!(parse_tools("default"), None);
    assert_eq!(parse_tools(""), Some(vec![]));
    assert_eq!(
        parse_tools("Bash,Edit Read"),
        Some(vec!["Bash".to_string(), "Edit".into(), "Read".into()])
    );
    assert_eq!(
        parse_session_id("550E8400-E29B-41D4-A716-446655440000").unwrap(),
        "550e8400-e29b-41d4-a716-446655440000"
    );
    assert!(parse_session_id("abc").is_err());
}

#[test]
fn plugin_dirs_load_one_plugin_or_a_folder_of_them() {
    let d = std::env::temp_dir().join(format!("chanto-plugdir-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(d.join("one/.claude-plugin")).unwrap();
    std::fs::write(
        d.join("one/.claude-plugin/plugin.json"),
        r#"{"name":"uno"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(d.join("two/commands")).unwrap();
    std::fs::create_dir_all(d.join("not-a-plugin")).unwrap();
    let one = parse_plugin_dir(d.join("one").to_str().unwrap()).unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].name, "uno");
    let all = parse_plugin_dir(d.to_str().unwrap()).unwrap();
    assert_eq!(
        all.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        vec!["uno", "two"]
    );
    assert!(parse_plugin_dir(d.join("not-a-plugin").to_str().unwrap()).is_err());
    assert!(parse_plugin_dir("x.zip").is_err());
    let _ = std::fs::remove_dir_all(d);
}
