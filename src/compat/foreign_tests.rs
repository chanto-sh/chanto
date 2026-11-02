use super::*;

fn tmp() -> PathBuf {
    let d = std::env::temp_dir().join(format!("chanto-foreign-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn w(d: &Path, p: &str, t: &str) {
    let p = d.join(p);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, t).unwrap();
}

#[test]
fn cursor_rule_kinds() {
    assert_eq!(
        cursor_apply("---\nalwaysApply: true\nglobs: *.ts\n---\nx"),
        Some(Apply::Always)
    );
    assert_eq!(
        cursor_apply(
            "---\ndescription:\nglobs: *.ts, src/**/*.{tsx,jsx}\nalwaysApply: false\n---\nx"
        ),
        Some(Apply::Globs(vec![
            "*.ts".into(),
            "src/**/*.{tsx,jsx}".into()
        ]))
    );
    assert_eq!(
        cursor_apply("---\ndescription: API style guide\nalwaysApply: false\n---\nx"),
        Some(Apply::OnRequest("API style guide".into()))
    );
    assert_eq!(cursor_apply("---\nalwaysApply: false\n---\nmanual"), None);
    assert_eq!(cursor_apply("no frontmatter"), None);
}

#[test]
fn copilot_apply_to() {
    assert_eq!(
        copilot_apply("---\napplyTo: \"**\"\n---\nx"),
        Some(Apply::Always)
    );
    assert_eq!(
        copilot_apply("---\napplyTo: \"**/*.ts,**/*.tsx\"\n---\nx"),
        Some(Apply::Globs(vec!["**/*.ts".into(), "**/*.tsx".into()]))
    );
    assert_eq!(copilot_apply("---\ndescription: x\n---\nx"), None);
}

#[test]
fn rules_from_every_tool() {
    let d = tmp();
    w(&d, ".cursorrules", "legacy");
    w(
        &d,
        ".cursor/rules/ts.mdc",
        "---\nglobs: web/**/*.ts\n---\nts",
    );
    w(
        &d,
        ".cursor/rules/sub/always.mdc",
        "---\nalwaysApply: true\n---\nalways",
    );
    w(&d, ".github/copilot-instructions.md", "copilot");
    w(
        &d,
        ".github/instructions/py.instructions.md",
        "---\napplyTo: '**/*.py'\n---\npy",
    );
    w(
        &d,
        ".aider.conf.yml",
        "model: x\nread:\n  - CONVENTIONS.md\n  - ../outside.md\n",
    );
    w(&d, "CONVENTIONS.md", "conventions");
    w(&d, "NOT_READ.md", "x");
    w(
        &d,
        "opencode.json",
        r#"{"instructions": ["CONTRIBUTING.md", "docs/*.md", "https://example.com/x.md"]}"#,
    );
    w(&d, "CONTRIBUTING.md", "contrib");
    w(&d, "docs/a.md", "a");
    let rules = foreign_rules(&d, &CompatConfig::default());
    let names: Vec<String> = rules
        .iter()
        .map(|r| {
            r.path
                .strip_prefix(&d)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    assert_eq!(
        names,
        [
            ".cursorrules",
            ".cursor/rules/sub/always.mdc",
            ".cursor/rules/ts.mdc",
            ".github/copilot-instructions.md",
            ".github/instructions/py.instructions.md",
            "CONVENTIONS.md",
            "CONTRIBUTING.md",
            "docs/a.md"
        ]
    );
    assert_eq!(rules[2].apply, Apply::Globs(vec!["web/**/*.ts".into()]));
    assert_eq!(rules[4].apply, Apply::Globs(vec!["**/*.py".into()]));
    // Each source has its own switch.
    let off = CompatConfig {
        cursor: false,
        copilot: false,
        aider: false,
        opencode: false,
        ..Default::default()
    };
    assert!(foreign_rules(&d, &off).is_empty());
    w(&d, "GEMINI.md", "gemini");
    assert_eq!(foreign_dir_files(&d, &CompatConfig::default()).len(), 1);
    assert!(
        foreign_dir_files(
            &d,
            &CompatConfig {
                gemini: false,
                ..Default::default()
            }
        )
        .is_empty()
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn opencode_agent_files() {
    let text = "---\ndescription: Reviews code\nmodel: anthropic/claude-sonnet-4\ntools:\n  write: false\n  bash: false\n  read: true\n---\nYou review.";
    let a = opencode_agent(Path::new("/x/.opencode/agent/review.md"), text).unwrap();
    assert_eq!(
        (a.name.as_str(), a.description.as_str(), a.model.as_deref()),
        ("review", "Reviews code", Some("anthropic/claude-sonnet-4"))
    );
    assert_eq!(a.disallowed_tools, vec!["Write", "Bash"]);
    assert!(a.tools.is_none() && a.prompt.trim() == "You review.");
    assert!(opencode_agent(Path::new("b.md"), "---\nmode: primary\n---\nx").is_none());
}
