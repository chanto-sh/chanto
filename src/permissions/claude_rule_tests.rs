use super::*;

#[test]
fn computer_use_asks_for_every_action() {
    let cwd = std::env::temp_dir();
    let shot = serde_json::json!({"action": "screenshot"});
    let click = serde_json::json!({"action": "left_click", "coordinate": [1, 2]});
    for mode in [Mode::Default, Mode::AcceptEdits] {
        assert_eq!(
            static_decision(mode, ToolKind::Browser, "Computer", &shot, &cwd),
            Some(Decision::Ask)
        );
        assert_eq!(
            static_decision(mode, ToolKind::Browser, "Computer", &click, &cwd),
            Some(Decision::Ask)
        );
    }
    assert_eq!(
        static_decision(Mode::Auto, ToolKind::Browser, "Computer", &shot, &cwd),
        Some(Decision::Allow)
    );
    assert_eq!(
        static_decision(Mode::Auto, ToolKind::Browser, "Computer", &click, &cwd),
        None,
        "the classifier decides"
    );
    assert_eq!(
        static_decision(Mode::Plan, ToolKind::Browser, "Computer", &shot, &cwd),
        Some(Decision::Ask)
    );
    assert!(matches!(
        static_decision(Mode::Plan, ToolKind::Browser, "Computer", &click, &cwd),
        Some(Decision::Deny(_))
    ));
    assert_eq!(
        static_decision(Mode::Bypass, ToolKind::Browser, "Computer", &click, &cwd),
        Some(Decision::Allow)
    );
}

#[test]
fn a_domain_rule_lets_a_dev_server_be_fetched_without_asking() {
    // Loopback fetches ask, but the user's own allow rule is checked first.
    let rules = Rules::new(&["WebFetch(domain:localhost)".into()], &[], &[]);
    let subjects = vec![crate::tools::web::domain_of("http://localhost:3000/")];
    assert_eq!(rules.check("WebFetch", &subjects), Some(Decision::Allow));
    let other = vec![crate::tools::web::domain_of("http://127.0.0.1:3000/")];
    assert_eq!(rules.check("WebFetch", &other), None);
}

#[test]
fn urls_that_reach_local_files_or_instance_metadata_ask() {
    let cwd = std::env::temp_dir().join(format!("chanto-url-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&cwd).unwrap();
    for u in [
        "https://docs.rs/serde",
        "https://localhost.example.com/",
        "example.com",
        "about:blank",
    ] {
        assert!(!url_needs_approval(u, &cwd), "{u}");
    }
    for u in [
        "file:///etc/hosts",
        "FILE:///Users/x/.ssh/id_rsa",
        "file:///Users/x/%2Essh/id_rsa",
        "http://169.254.169.254/latest/meta-data/",
        "http://user@169.254.169.254:80/",
        "http://metadata.google.internal/computeMetadata/v1/",
        "http://[fd00:ec2::254]/latest/",
        // The same metadata endpoint in encodings a URL parser accepts.
        "http://2852039166/latest/meta-data/",
        "http://0xA9FEA9FE/",
        "http://0xa9.0xfe.0xa9.0xfe/",
        "http://0251.0376.0251.0376/",
        "http://169.254.169.254/",
        "http://[::ffff:169.254.169.254]/",
        "http://100.100.100.200/",
        // This machine's own services, in any spelling.
        "http://localhost:3000/",
        "http://LOCALHOST./admin",
        "http://app.localhost:8080/",
        "http://127.0.0.1:11434/api/tags",
        "http://127.1.2.3/",
        "http://2130706433/",
        "http://0x7f000001/",
        "http://[::1]:8080/",
        "http://[::ffff:127.0.0.1]/",
        "http://0.0.0.0:5432/",
        "http://user@localhost/",
        // Spellings the URL parser cleans up before it connects: control
        // characters around the URL, tabs inside it, `%`-escapes and a
        // trailing dot in the host.
        "http://@169.254.16\0",
        "http://169.254.169.254/\t\0",
        "http://169.254.16\t9.254/",
        "http://169.254.169.%32%35%34/",
        "http://169.254.169.254./",
        "http://metadata.google.internal./",
        "javascript:alert(1)",
        "chrome://settings",
        "view-source:https://example.com",
    ] {
        assert!(url_needs_approval(u, &cwd), "{u}");
    }
    // Web access stays automatic where it was; these ask instead.
    let fetch = |u: &str| {
        static_decision(
            Mode::AcceptEdits,
            ToolKind::Network,
            "WebFetch",
            &serde_json::json!({"url": u}),
            &cwd,
        )
    };
    assert_eq!(fetch("https://docs.rs/serde"), Some(Decision::Allow));
    assert_eq!(fetch("http://169.254.169.254/"), Some(Decision::Ask));
    let nav = serde_json::json!({"action": "navigate", "url": "file:///etc/passwd"});
    assert_eq!(
        static_decision(Mode::Plan, ToolKind::Network, "Browser", &nav, &cwd),
        Some(Decision::Ask)
    );
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn protected_paths_ignore_case_and_follow_symlinks() {
    for p in [
        "/r/.GIT/config",
        "/r/.Git/hooks/pre-commit",
        "/r/.CHANTO/config.toml",
        "/r/.Claude/Settings.json",
        "/r/.MCP.json",
        "/r/.VSCode/tasks.json",
    ] {
        assert!(is_protected_path(Path::new(p)), "{p}");
    }
    let d = std::env::temp_dir().join(format!("chanto-protect-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(d.join(".git/hooks")).unwrap();
    std::fs::create_dir_all(d.join(".chanto")).unwrap();
    std::fs::write(d.join(".git/config"), "").unwrap();
    #[cfg(unix)]
    {
        // A link to a protected file, and a linked directory leading into one.
        std::os::unix::fs::symlink(d.join(".git/config"), d.join("notes.txt")).unwrap();
        std::os::unix::fs::symlink(d.join(".chanto"), d.join("docs")).unwrap();
        assert!(is_protected_path(&d.join("notes.txt")));
        assert!(
            is_protected_path(&d.join("docs/config.toml")),
            "a file that doesn't exist yet"
        );
    }
    assert!(!is_protected_path(&d.join("src/new.rs")));
    let _ = std::fs::remove_dir_all(&d);
}

/// `..` after a symlink leaves from the link's target, as the OS
/// resolves it, not from the directory holding the link.
#[cfg(unix)]
#[test]
fn dotdot_after_a_symlink_leaves_from_its_target() {
    let d = normalize(
        &std::env::temp_dir().join(format!("chanto-dotdot-{}", uuid::Uuid::new_v4().simple())),
    );
    let (proj, out) = (d.join("proj"), d.join("out/deep"));
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    std::os::unix::fs::symlink(&out, proj.join("link")).unwrap();
    // The file doesn't exist yet, so the check can't just canonicalize it.
    let p = proj.join("link/../new.txt");
    assert_eq!(normalize(&p), d.join("out/new.txt"));
    // Where a write really lands.
    std::fs::write(&p, "x").unwrap();
    assert!(d.join("out/new.txt").exists() && !proj.join("new.txt").exists());
    // Missing directories created on the way don't change that.
    assert_eq!(normalize(&proj.join("a/../link/../b/c")), d.join("out/b/c"));
    assert_eq!(normalize(&proj.join("a/b/../../c")), proj.join("c"));
    // Through the scratch-dir rule: a link in temp to a system directory,
    // then `..` out of it.
    let sys = d.join("sys");
    std::os::unix::fs::symlink("/usr/bin", &sys).unwrap();
    let cwd = std::env::current_dir().unwrap();
    assert!(!in_workspace(&sys.join("../chanto-escape-test"), &cwd));
    let a = serde_json::json!({"file_path": sys.join("../chanto-escape-test"), "content": "x"});
    assert_ne!(
        static_decision(Mode::AcceptEdits, ToolKind::Edit, "Write", &a, &cwd),
        Some(Decision::Allow)
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// A dangling symlink is judged by where writing through it lands.
#[cfg(unix)]
#[test]
fn dangling_symlinks_are_judged_by_their_target() {
    let d = normalize(
        &std::env::temp_dir().join(format!("chanto-dangle-{}", uuid::Uuid::new_v4().simple())),
    );
    std::fs::create_dir_all(d.join(".git/hooks")).unwrap();
    std::os::unix::fs::symlink("/usr/chanto-escape-test", d.join("dangle")).unwrap();
    std::os::unix::fs::symlink("missing/x", d.join("rel")).unwrap();
    std::os::unix::fs::symlink(".git/hooks/pre-commit", d.join("notes")).unwrap();
    assert_eq!(
        normalize(&d.join("dangle")),
        Path::new("/usr/chanto-escape-test")
    );
    assert_eq!(normalize(&d.join("rel")), d.join("missing/x"));
    let cwd = std::env::current_dir().unwrap();
    assert!(!in_workspace(&d.join("dangle"), &cwd));
    // Writing `notes` would create the hook.
    assert!(is_protected_path(&d.join("notes")));
    // A missing directory, then `..`, then a link into `.git`.
    std::os::unix::fs::symlink(".git", d.join("docs")).unwrap();
    assert!(is_protected_path(&d.join("new/../docs/hooks/pre-commit")));
    // A link loop resolves to nothing the OS would open.
    std::os::unix::fs::symlink(d.join("loop"), d.join("loop")).unwrap();
    let _ = normalize(&d.join("loop/x"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn protected_paths() {
    for p in [
        "/r/.git/config",
        "/r/.git/hooks/pre-commit",
        "/r/.chanto/config.toml",
        "/r/.claude/settings.json",
        "/r/.mcp.json",
        "/r/.codex/config.toml",
    ] {
        assert!(is_protected_path(Path::new(p)), "{p}");
    }
    for p in [
        "/r/src/main.rs",
        "/r/.claude/commands/x.md",
        "/r/.chanto/worktrees/agent-1/src/a.rs",
        "/r/.github/x.yml",
        "/r/.gitignore",
    ] {
        assert!(!is_protected_path(Path::new(p)), "{p}");
    }
    // Background-run worktrees live under the Chanto home; inside one,
    // its own settings are protected and its code isn't.
    let wt = crate::runs::worktrees_dir()
        .join("proj-1234abcd")
        .join("ab12cd34");
    assert!(is_protected_path(&wt.join(".chanto/config.toml")));
    assert!(!is_protected_path(&wt.join("src/a.rs")));
    // A worktree's `.git` is a file naming its git directory: rewriting
    // it would point the host's next git call at another repository.
    assert!(is_protected_path(&wt.join(".git")));
    assert!(is_protected_path(Path::new(
        "/r/.chanto/worktrees/agent-1/.git"
    )));
    assert!(is_protected_path(Path::new("/r/.git")));
    // Windows spellings of `.git` (trailing dots and spaces, streams, the
    // 8.3 short name).
    for p in [
        "/r/.git.",
        "/r/.git ",
        "/r/.GIT. ./config",
        "/r/.git::$INDEX_ALLOCATION/config",
        "/r/.git:stream",
        "/r/GIT~1/hooks/pre-commit",
        "/r/.chanto./config.toml",
    ] {
        assert!(is_protected_path(Path::new(p)), "{p}");
    }
    assert_eq!(component_name("c:"), "c:");
    assert_eq!(component_name(".."), "");
    assert!(!is_protected_path(Path::new("/r/src/git.rs")));
    // So do the worktrees of runs a runner takes from a server.
    let pulled = crate::runs::pull::store_dir()
        .join("worktrees")
        .join("https---github-com-acme-1234abcd")
        .join("ab12cd34");
    assert!(is_protected_path(&pulled.join(".chanto/config.toml")));
    assert!(!is_protected_path(&pulled.join("infra/README.md")));
    let cwd = std::env::current_dir().unwrap();
    let a = serde_json::json!({"file_path": "/tmp/chanto-x/.git/config", "content": "x"});
    assert_eq!(
        static_decision(Mode::AcceptEdits, ToolKind::Edit, "Write", &a, &cwd),
        Some(Decision::Ask)
    );
    assert_eq!(
        static_decision(Mode::Auto, ToolKind::Edit, "Write", &a, &cwd),
        Some(Decision::Ask)
    );
}

#[test]
fn patch_paths_must_stay_in_workspace() {
    let cwd = std::env::current_dir().unwrap();
    // Temp dirs always count as workspace. Use a system path for the
    // outside fixture: tests can redirect HOME into temp without changing
    // the result (or touching real user-home files).
    let inside = serde_json::json!({"input": "*** Begin Patch\n*** Add File: /tmp/chanto-x/a.rs\n+a\n*** End Patch"});
    let outside_path = if cfg!(windows) {
        std::path::PathBuf::from(
            std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()),
        )
        .join("chanto-outside-workspace/a.rs")
    } else {
        std::path::PathBuf::from("/usr/chanto-outside-workspace/a.rs")
    };
    let outside = serde_json::json!({"input": format!("*** Begin Patch\n*** Add File: /tmp/chanto-x/a.rs\n+a\n*** Update File: {}\n+b\n*** End Patch", outside_path.display())});
    let empty = serde_json::json!({"input": "nonsense"});
    let d = |a: &Value| static_decision(Mode::AcceptEdits, ToolKind::Edit, "apply_patch", a, &cwd);
    assert_eq!(d(&inside), Some(Decision::Allow));
    assert_eq!(d(&outside), Some(Decision::Ask));
    assert_eq!(d(&empty), Some(Decision::Ask));
    // Auto mode doesn't hand outside writes to the classifier.
    assert_eq!(
        static_decision(Mode::Auto, ToolKind::Edit, "apply_patch", &outside, &cwd),
        Some(Decision::Ask)
    );
}

#[test]
fn claude_rule_semantics() {
    let cwd = std::env::current_dir().unwrap();
    let r = Rules::new(
        &["WebSearch".into(), "mcp__docs".into()],
        &[
            "Read(./.env)".into(),
            "Edit(**/*.lock)".into(),
            "Read(//etc/**)".into(),
        ],
        &[],
    );
    assert_eq!(r.check("WebSearch", &[]), Some(Decision::Allow));
    assert_eq!(r.check("mcp__docs__search", &[]), Some(Decision::Allow));
    assert_eq!(r.check("mcp__docsx__search", &[]), None);
    assert!(matches!(
        r.check("Read", &[".env".into()]),
        Some(Decision::Deny(_))
    ));
    assert!(matches!(
        r.check("Read", &[cwd.join(".env").display().to_string()]),
        Some(Decision::Deny(_))
    ));
    assert!(matches!(
        r.check("Write", &["sub/Cargo.lock".into()]),
        Some(Decision::Deny(_))
    ));
    assert!(matches!(
        r.check("apply_patch", &["Cargo.lock".into()]),
        Some(Decision::Deny(_))
    ));
    assert!(matches!(
        r.check("Read", &["/etc/hosts".into()]),
        Some(Decision::Deny(_))
    ));
    assert_eq!(r.check("Read", &["src/main.rs".into()]), None);
    let r = Rules::new(&[], &["Read(*.pem)".into()], &[]);
    assert!(matches!(
        r.check("Read", &["certs/server.pem".into()]),
        Some(Decision::Deny(_))
    ));
    assert!(r.check("Grep", &[".env".into()]).is_none());
    let r = Rules::new(&[], &["Read(./.env)".into()], &[]);
    assert!(matches!(
        r.check("Grep", &[".env".into()]),
        Some(Decision::Deny(_))
    ));
}
