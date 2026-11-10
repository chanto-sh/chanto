use super::*;
fn temp() -> PathBuf {
    let d = std::env::temp_dir().join(format!("chanto-customize-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}
#[test]
fn guide_covers_extension_points() {
    // The guide and the expected home path both read CHANTO_HOME, which
    // other tests change under this lock; hold it so they agree.
    let _home_lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let guide = extension_guide();
    for word in [
        "config.toml",
        "commands/",
        "SKILL.md",
        "agents/",
        "output-styles/",
        "profiles",
        "MCP",
        "Plugins",
        "AGENTS.md",
        &crate::config::home_dir().display().to_string(),
        ".chanto/",
        "check-extensions",
        "/reload",
        "max_turns",
        "guardrails",
    ] {
        assert!(guide.contains(word), "missing {word}");
    }
    for event in HOOK_EVENTS {
        assert!(guide.contains(event));
    }
}
#[test]
fn check_detects_broken_extensions() {
    let _home_lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let d = temp();
    let root = d.join(".chanto");
    std::fs::create_dir_all(root.join("commands")).unwrap();
    std::fs::write(root.join("config.toml"), "model = [").unwrap();
    assert!(
        check_extensions(&d)
            .unwrap_err()
            .to_string()
            .contains("config.toml")
    );
    std::fs::remove_file(root.join("config.toml")).unwrap();
    std::fs::write(root.join("commands/bad.md"), "---\ndescription: broken").unwrap();
    assert!(
        check_extensions(&d)
            .unwrap_err()
            .to_string()
            .contains("bad.md")
    );
    std::fs::remove_file(root.join("commands/bad.md")).unwrap();
    std::fs::create_dir_all(root.join("skills/bad")).unwrap();
    std::fs::write(root.join("skills/bad/SKILL.md"), "").unwrap();
    assert!(
        check_extensions(&d)
            .unwrap_err()
            .to_string()
            .contains("SKILL.md")
    );
    std::fs::remove_dir_all(d).unwrap();
}
#[test]
fn detects_source_checkout() {
    let d = temp();
    std::fs::write(d.join("Cargo.toml"), "[package]\nname = 'chanto'\n").unwrap();
    assert!(!source_checkout(&d));
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&d)
            .status()
            .unwrap()
            .success()
    );
    assert!(source_checkout(&d));
    std::fs::write(d.join("Cargo.toml"), "[package]\nname = 'other'\n").unwrap();
    assert!(!source_checkout(&d));
    std::fs::remove_dir_all(d).unwrap();
}

fn hooks(toml_text: &str) -> crate::config::HooksConfig {
    #[derive(serde::Deserialize)]
    struct Wrap {
        hooks: crate::config::HooksConfig,
    }
    toml::from_str::<Wrap>(toml_text).unwrap().hooks
}

#[test]
fn check_hooks_rejects_each_kind_of_mistake() {
    let ok = hooks(
        "[[hooks.PreToolUse]]\nmatcher = \"Bash|Read\"\ncommand = \"true\"\nif = \"Bash(git *)\"\n",
    );
    check_hooks(&ok).unwrap();
    let nested =
        hooks("[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype = \"http\"\nurl = \"http://x\"\n");
    check_hooks(&nested).unwrap();
    for (text, want) in [
        (
            "[[hooks.NoSuchEvent]]\ncommand = \"x\"\n",
            "unknown hook event",
        ),
        ("[[hooks.Stop]]\ncommand = \"  \"\n", "empty hook command"),
        (
            "[[hooks.Stop]]\ntype = \"weird\"\n",
            "unsupported hook type `weird`",
        ),
        (
            "[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ncommand = \"x\"\n",
            "invalid nested hook",
        ),
        (
            "[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype = \"http\"\n",
            "invalid nested hook",
        ),
        (
            "[[hooks.PreToolUse]]\ncommand = \"x\"\nif = \"not a rule((\"\n",
            "invalid hook `if`",
        ),
        (
            "[[hooks.PreToolUse]]\ncommand = \"x\"\nmatcher = \"(\"\n",
            "invalid hook matcher",
        ),
    ] {
        let err = format!("{:#}", check_hooks(&hooks(text)).unwrap_err());
        assert!(err.contains(want), "{text}: {err}");
    }
}

#[test]
fn check_markdown_front_matter() {
    let d = temp();
    let p = d.join("x.md");
    let check = |text: &str| {
        std::fs::write(&p, text).unwrap();
        check_markdown(&p).map_err(|e| e.to_string())
    };
    check("plain body").unwrap();
    check("---\ndescription: d\ntools:\n  - Read\n- Bash\n# note\n\n---\nbody").unwrap();
    assert!(
        check("---\nnot a pair\n---\nbody")
            .unwrap_err()
            .contains("key: value")
    );
    assert!(check("---\na: b\n").unwrap_err().contains("unclosed"));
    assert!(
        check("---\na: b\n---\n  \n")
            .unwrap_err()
            .contains("empty instruction body")
    );
    assert!(
        check_markdown(&d.join("missing.md"))
            .unwrap_err()
            .to_string()
            .contains("reading")
    );
    std::fs::remove_dir_all(d).unwrap();
}

#[test]
fn check_extensions_reports_what_loads() {
    let _home_lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let d = temp();
    let root = d.join(".chanto");
    for sub in [
        "commands",
        "skills/tidy",
        "skills/group",
        "agents",
        "output-styles",
    ] {
        std::fs::create_dir_all(root.join(sub)).unwrap();
    }
    std::fs::write(
        root.join("commands/ship.md"),
        "---\ndescription: ship it\n---\nShip $ARGUMENTS",
    )
    .unwrap();
    std::fs::write(root.join("commands/notes.txt"), "ignored").unwrap();
    std::fs::write(
        root.join("skills/tidy/SKILL.md"),
        "---\nname: tidy\ndescription: t\n---\nTidy up",
    )
    .unwrap();
    std::fs::write(
        root.join("agents/helper.md"),
        "---\nname: helper\ndescription: h\n---\nHelp",
    )
    .unwrap();
    std::fs::write(
        root.join("output-styles/terse.md"),
        "---\nname: terse\n---\nBe terse",
    )
    .unwrap();
    std::fs::write(
        root.join("config.toml"),
        "[[hooks.Stop]]\ncommand = \"true\"\n\n[profiles.fast]\nmodel = \"m\"\n",
    )
    .unwrap();
    // A native skill folder without SKILL.md is an error, not a grouping folder.
    let err = check_extensions(&d).unwrap_err().to_string();
    assert!(err.contains("group"), "{err}");
    std::fs::remove_dir_all(root.join("skills/group")).unwrap();
    let (_, report) = check_extensions(&d).unwrap();
    assert!(report.starts_with("Extensions valid"), "{report}");
    for want in [
        "ship",
        "tidy",
        "helper",
        "Ignored untrusted settings: hooks",
        "Validation does not execute hooks",
    ] {
        assert!(report.contains(want), "missing {want}: {report}");
    }
    // A bad profile hook is caught even though profiles apply later.
    std::fs::write(
        root.join("config.toml"),
        "[profiles.fast]\n[[profiles.fast.hooks.Bogus]]\ncommand = \"x\"\n",
    )
    .unwrap();
    assert!(
        check_extensions(&d)
            .unwrap_err()
            .to_string()
            .contains("Bogus")
    );
    std::fs::write(root.join("config.toml"), "profiles = 3\n").unwrap();
    assert!(
        check_extensions(&d)
            .unwrap_err()
            .to_string()
            .contains("profiles must be a table")
    );
    std::fs::write(root.join("config.toml"), "[profiles.fast]\nmodel = 3\n").unwrap();
    assert!(
        check_extensions(&d)
            .unwrap_err()
            .to_string()
            .contains("profile fast")
    );
    std::fs::write(
        root.join("config.toml"),
        "[[hooks.Bogus]]\ncommand = \"x\"\n",
    )
    .unwrap();
    assert!(format!("{:#}", check_extensions(&d).unwrap_err()).contains("unknown hook event"));
    std::fs::remove_dir_all(d).unwrap();
}

struct Quiet;

#[async_trait::async_trait]
impl crate::agent::Frontend for Quiet {
    fn emit(&self, _: crate::agent::Event) {}
    async fn permission(
        &self,
        _: crate::agent::PermissionRequest,
    ) -> crate::agent::PermissionReply {
        crate::agent::PermissionReply::Deny(None)
    }
    async fn question(&self, _: &str, _: &[String]) -> Option<String> {
        None
    }
}

#[tokio::test]
async fn reload_refreshes_extensions_and_keeps_state_on_error() {
    use crate::llm::{Provider, ProviderKind};
    let _home_lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let d = temp();
    let env = crate::agent::Env::new(crate::agent::EnvInit {
        config: Default::default(),
        provider: Provider::new(ProviderKind::Openai, "http://127.0.0.1:9", None, 32768),
        model: "mock".into(),
        mode: crate::permissions::Mode::Default,
        vision: false,
        cwd: d.clone(),
        tools: crate::tools::Registry::builtin(),
        mcp: vec![],
        frontend: std::sync::Arc::new(Quiet),
        interactive: false,
    });
    let root = d.join(".chanto");
    std::fs::create_dir_all(root.join("commands")).unwrap();
    std::fs::write(root.join("commands/fresh.md"), "Do the fresh thing").unwrap();
    std::fs::write(root.join("config.toml"), "output_style = \"Default\"\n").unwrap();
    let report = reload_checked(&env).await.unwrap();
    assert!(
        report.contains("fresh") && report.contains("Reloaded commands"),
        "{report}"
    );
    assert!(
        env.commands
            .read()
            .unwrap()
            .iter()
            .any(|c| c.name == "fresh")
    );
    assert!(env.output_style.read().unwrap().is_none());
    std::fs::write(root.join("commands/broken.md"), "---\nx: y").unwrap();
    assert!(reload(&env).unwrap_err().to_string().contains("broken.md"));
    assert!(
        env.commands
            .read()
            .unwrap()
            .iter()
            .any(|c| c.name == "fresh")
    );
    std::fs::remove_dir_all(d).unwrap();
}

#[test]
fn slugs_name_the_branch() {
    assert_eq!(
        slug("Add a /foo command, please now!"),
        "add-a-foo-command-please"
    );
    assert_eq!(slug("!!!"), "change");
    assert_eq!(slug(&"x".repeat(80)).len(), 48);
}

#[tokio::test]
async fn command_reports_failure() {
    let d = temp();
    command(&d, "git", &["--version"]).await.unwrap();
    let err = command(&d, "git", &["no-such-subcommand-xyz"])
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("git no-such-subcommand-xyz failed")
    );
    assert!(
        command(&d, "chanto-no-such-program-xyz", &[])
            .await
            .is_err()
    );
    std::fs::remove_dir_all(d).unwrap();
}

#[tokio::test]
async fn run_source_needs_a_request() {
    use clap::Parser;
    let cli = crate::Cli::parse_from(["chanto"]);
    let err = run_source(cli).await.unwrap_err();
    assert!(err.to_string().contains("Usage: chanto customize --source"));
}
