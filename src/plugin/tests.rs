use super::*;

#[test]
fn local_marketplace_lifecycle() {
    let _lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("chanto-market-{}", uuid::Uuid::new_v4()));
    let previous = std::env::var_os("CHANTO_COMPAT_HOME");
    let previous_chanto = std::env::var_os("CHANTO_HOME");
    unsafe {
        std::env::set_var("CHANTO_COMPAT_HOME", &home);
        std::env::set_var("CHANTO_HOME", home.join(".chanto"));
    }
    let source = home.join("source");
    let write = |path: &str, text: &str| {
        let p = source.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    };
    write(
        ".claude-plugin/marketplace.json",
        r#"{"name":"local","owner":{"name":"test"},"plugins":[{"name":"hello","source":"./hello","description":"Greeting","version":"1"},{"name":"risky","source":"./risky","mcpServers":{"test":{"command":"false"}}},{"name":"escape","source":"../../"},{"name":"inline","source":"./inline"},{"name":"grants","source":"./grants"}]}"#,
    );
    write("hello/.claude-plugin/plugin.json", r#"{"name":"hello"}"#);
    write(
        "hello/commands/greet.md",
        "---\ndescription: Greet\n---\nHello $ARGUMENTS",
    );
    write(
        "hello/skills/greeting/SKILL.md",
        "---\nname: greeting\ndescription: Greeting skill\n---\nSay hello.",
    );
    write("risky/.claude-plugin/plugin.json", r#"{"name":"risky"}"#);
    // Skills and commands that run something when invoked ask, too.
    write(
        "inline/skills/s/SKILL.md",
        "---\nname: s\n---\nContext: !`curl example.com`",
    );
    write(
        "grants/commands/c.md",
        "---\nallowed-tools: Bash(*)\n---\nDo it.",
    );
    let base = root();
    let run = |args: &[&str]| {
        run(
            &base,
            &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &mut |_| false,
        )
    };
    run(&["marketplace", "add", source.to_str().unwrap()]).unwrap();
    assert!(run(&["marketplace", "list"]).unwrap().contains("local"));
    assert!(run(&["list"]).unwrap().contains("hello@local"));
    assert!(run(&["install", "escape@local"]).is_err());
    assert!(
        run(&["install", "risky@local"])
            .unwrap_err()
            .to_string()
            .contains("require confirmation")
    );
    assert!(!base.join("installed/risky@local").exists());
    for (id, shows) in [
        ("inline@local", "!`curl example.com`"),
        ("grants@local", "allowed-tools"),
    ] {
        let e = run(&["install", id]).unwrap_err().to_string();
        assert!(
            e.contains("require confirmation") && e.contains(shows),
            "{e}"
        );
    }
    run(&["install", "risky@local", "--yes"]).unwrap();
    run(&["uninstall", "risky"]).unwrap();
    let summary = run(&["install", "hello@local"]).unwrap();
    assert!(summary.contains("greet.md"));
    let cwd = home.join("project");
    fs::create_dir_all(cwd.join(".claude")).unwrap();
    fs::write(
        cwd.join(".claude/settings.json"),
        r#"{"enabledPlugins":{"hello@local":false}}"#,
    )
    .unwrap();
    let ps = crate::compat::plugins(&cwd, false);
    assert_eq!(ps.len(), 1);
    assert!(crate::compat::plugins(&cwd, true).is_empty());
    assert_eq!(
        crate::compat::plugins_with_compat(&cwd, false, false).len(),
        1
    );
    crate::compat::set_active_plugins(ps);
    assert!(
        crate::context::load_commands(&cwd)
            .iter()
            .any(|c| c.name == "hello:greet")
    );
    assert!(
        crate::context::load_skills(&cwd)
            .iter()
            .any(|s| s.name.contains("greeting"))
    );
    run(&["disable", "hello"]).unwrap();
    assert!(crate::compat::plugins(&cwd, false).is_empty());
    run(&["enable", "hello"]).unwrap();
    run(&["marketplace", "remove", "local"]).unwrap();
    assert_eq!(crate::compat::plugins(&cwd, false).len(), 1);
    run(&["uninstall", "hello"]).unwrap();
    assert!(loaded(&base).unwrap().is_empty());
    crate::compat::set_active_plugins(vec![]);
    unsafe {
        match previous_chanto {
            Some(v) => std::env::set_var("CHANTO_HOME", v),
            None => std::env::remove_var("CHANTO_HOME"),
        }
        match previous {
            Some(v) => std::env::set_var("CHANTO_COMPAT_HOME", v),
            None => std::env::remove_var("CHANTO_COMPAT_HOME"),
        }
    }
    fs::remove_dir_all(home).unwrap();
}
