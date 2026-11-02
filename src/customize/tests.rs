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
