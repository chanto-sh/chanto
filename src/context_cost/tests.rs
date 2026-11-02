use super::*;

fn spec(name: &str, desc: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: desc.into(),
        parameters: serde_json::json!({"type": "object", "properties": {"q": {"type": "string", "description": "x".repeat(400)}}}),
    }
}

#[test]
fn skills_cost_a_line_until_loaded() {
    let dir = std::env::temp_dir().join(format!("chanto-skillcost-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("SKILL.md");
    std::fs::write(
        &path,
        format!(
            "---\nname: big\ndescription: d\n---\n{}",
            "word ".repeat(800)
        ),
    )
    .unwrap();
    let mut sk = Skill {
        name: "big".into(),
        description: "Does big things".into(),
        path,
        model_invocable: true,
        user_invocable: true,
        fork: None,
        project: false,
        meta: Default::default(),
    };
    let c = skill(&sk);
    assert_eq!(c.listing, tokens("- big: Does big things\n".len()));
    assert!(c.full >= c.listing + 1000, "{c:?}");
    sk.model_invocable = false;
    let c = skill(&sk);
    assert_eq!(c.listing, 0);
    assert!(c.full >= 1000 && c.note.contains("not listed"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mcp_tools_cost_less_when_deferred() {
    let tools = vec![
        (spec("mcp__s__a", "Search things\nmore"), false),
        (spec("mcp__s__b", "Fetch"), true),
    ];
    let upfront = mcp("s", Some("Use me well"), &tools, false);
    let deferred = mcp("s", Some("Use me well"), &tools, true);
    assert_eq!(upfront.listing, upfront.full);
    assert_eq!(deferred.full, upfront.full);
    assert!(
        deferred.listing < upfront.listing && deferred.note.contains("1 deferred"),
        "{deferred:?}"
    );
    let rows = vec![
        deferred,
        Cost {
            kind: "skill",
            name: "x".into(),
            listing: 7,
            full: 70,
            note: String::new(),
        },
    ];
    let r = report(&rows, 1000);
    assert!(
        r.contains("| MCP server | s |")
            && r.contains("| skill | x | 7 | 70 |")
            && r.contains("of 1000"),
        "{r}"
    );
}
