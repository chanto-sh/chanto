//! The Seatbelt profile's handling of the paths it is built from.

use super::*;
use crate::config::SandboxConfig;

/// A fresh, resolved temp directory for the sandbox-exec tests below.
#[cfg(target_os = "macos")]
fn scratch(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("chanto-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    crate::util::canonical(&d).unwrap()
}

#[test]
fn secret_key_pattern_is_an_ordinary_string() {
    // `#"…"` is a raw regex literal in SBPL: escaping its backslashes
    // turned `\.ssh` into `\\.ssh`, which matched no key at all.
    let home = std::path::PathBuf::from("/Users/a.b");
    let profile = sandbox_profile_with_paths(&[], &home, &SandboxConfig::default(), None);
    assert!(!profile.contains("#\""), "{profile}");
    assert!(
        profile.contains(r#"(deny file-read* (regex "^/Users/a\\.b/\\.ssh/id_("#),
        "{profile}"
    );
}

/// Runs `script` under the profile; `None` when Seatbelt can't start here
/// (e.g. inside another sandbox).
#[cfg(target_os = "macos")]
fn seatbelt(
    profile: &str,
    script: &str,
    args: &[&std::path::Path],
) -> Option<std::process::Output> {
    let probe = std::process::Command::new("/usr/bin/sandbox-exec")
        .args([
            "-p",
            "(version 1)(allow default)(deny file-write* (literal \"/chanto-probe\"))",
            "/usr/bin/true",
        ])
        .output()
        .ok()?;
    if !probe.status.success() {
        eprintln!("skipping Seatbelt integration: nested sandbox");
        return None;
    }
    Some(
        std::process::Command::new("/usr/bin/sandbox-exec")
            .args(["-p", profile, "/bin/sh", "-c", script])
            .args(args)
            .output()
            .unwrap(),
    )
}

#[cfg(target_os = "macos")]
#[test]
fn seatbelt_denies_reading_private_keys() {
    let root = scratch("keys");
    // Regex metacharacters in the home path must stay literal.
    for name in ["home", "h.o-m~e#&+(x)[y]"] {
        let home = root.join(name);
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(home.join(".aws")).unwrap();
        std::fs::write(home.join(".ssh/id_ed25519"), "KEY-SECRET").unwrap();
        std::fs::write(home.join(".ssh/id_ed25519.pub"), "PUBLIC").unwrap();
        std::fs::write(home.join(".aws/credentials"), "AWS-SECRET").unwrap();
        let profile = sandbox_profile_with_paths(&[], &home, &SandboxConfig::default(), None);
        for (file, secret) in [
            (".ssh/id_ed25519", "KEY-SECRET"),
            (".aws/credentials", "AWS-SECRET"),
        ] {
            let Some(out) = seatbelt(&profile, "/bin/cat \"$0\"", &[&home.join(file)]) else {
                return;
            };
            assert!(
                !String::from_utf8_lossy(&out.stdout).contains(secret),
                "{file} readable under {name}:\n{profile}"
            );
        }
        let public = home.join(".ssh/id_ed25519.pub");
        let out = seatbelt(&profile, "/bin/cat \"$0\"", &[&public]).unwrap();
        assert_eq!(out.stdout, b"PUBLIC", "public key should stay readable");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(target_os = "macos")]
#[test]
fn seatbelt_keeps_crafted_directory_names_inside_their_literal() {
    let root = scratch("names");
    for name in [
        r#"x")) (allow file-write* (regex #".")) (allow file-write* (literal "y"#,
        "a\"b\\\"c)(",
        "nl\n(allow file-write*)\n",
        "end\\",
        "semi;#|c|#",
    ] {
        let project = root.join(name);
        std::fs::create_dir_all(&project).unwrap();
        let writable = vec![project.display().to_string(), "/dev".to_string()];
        let home = root.join("home");
        let profile = sandbox_profile_with_paths(&writable, &home, &SandboxConfig::default(), None);
        let script = "touch \"$0/inside\" && { touch \"$1/outside\" && echo ESCAPED; true; }";
        let Some(out) = seatbelt(&profile, script, &[&project, &root]) else {
            return;
        };
        assert!(
            out.status.success(),
            "{name:?}: project not writable: {}\n{profile}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("ESCAPED"),
            "{name:?} widened the profile:\n{profile}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(target_os = "macos")]
#[test]
fn seatbelt_denies_a_missing_deny_target_under_a_symlinked_cache() {
    // ~/.cargo links elsewhere and has no bin/ yet: the deny must name the
    // resolved path, which is what Seatbelt checks.
    let root = scratch("symcache");
    let home = root.join("home");
    let real = root.join("real-cargo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(&real, home.join(".cargo")).unwrap();
    let writable = vec![
        home.join(".cargo").display().to_string(),
        "/dev".to_string(),
    ];
    let profile = sandbox_profile_with_paths(&writable, &home, &SandboxConfig::default(), None);
    let script = "touch \"$0/ok\" && mkdir -p \"$0/bin\" && touch \"$0/bin/evil\" && echo ESCAPED";
    let Some(out) = seatbelt(&profile, script, &[&home.join(".cargo")]) else {
        return;
    };
    assert!(real.join("ok").exists(), "cache should stay writable");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("ESCAPED"),
        "wrote into ~/.cargo/bin:\n{profile}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The write guards appear in the profile: the newly protected agent/editor
/// config, Chanto's home, and the toolchain execution subtrees. The `.git`
/// directory is pinned with a `literal` deny so renaming it is refused.
#[test]
fn profile_denies_persistence_and_toolchain_paths() {
    let root = std::env::temp_dir().join(format!("chanto-guards-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
    let home = root.join("home");
    let writable = vec![root.display().to_string(), "/dev".to_string()];
    let profile = sandbox_profile_with_paths(&writable, &home, &SandboxConfig::default(), None);
    let q = |p: &std::path::Path| guards::norm(p);
    // The project's own hook/config persistence paths.
    for rel in [
        ".claude/settings.json",
        ".mcp.json",
        ".husky",
        ".githooks",
        ".vscode/tasks.json",
    ] {
        let d = format!(
            "(deny file-write* (subpath \"{}\"))",
            sbpl_quote(&q(&root.join(rel)))
        );
        assert!(profile.contains(&d), "missing deny for {rel}:\n{profile}");
    }
    // `.git` is pinned (literal), not just its contents.
    assert!(
        profile.contains(&format!(
            "(deny file-write* (literal \"{}\"))",
            sbpl_quote(&q(&root.join(".git")))
        )),
        "missing .git pin:\n{profile}"
    );
    // `.claude` and `.vscode` are pinned too, so a prepared copy can't be
    // renamed into place; nested repositories' hooks are denied by pattern.
    for rel in [".claude", ".vscode"] {
        assert!(
            profile.contains(&format!(
                "(deny file-write* (literal \"{}\"))",
                sbpl_quote(&q(&root.join(rel)))
            )),
            "missing {rel} pin:\n{profile}"
        );
    }
    assert!(
        profile.contains("(.+/)?\\\\.git/(hooks(/.*)?|config)$"),
        "{profile}"
    );
    // The home toolchain and startup trees.
    for rel in [".cargo/bin", ".rustup", ".zshrc", ".local/bin"] {
        let d = format!(
            "(deny file-write* (subpath \"{}\"))",
            sbpl_quote(&q(&home.join(rel)))
        );
        assert!(
            profile.contains(&d),
            "missing toolchain deny for {rel}:\n{profile}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// The IPC rules are in the profile: Unix sockets confined to the project
/// and the private temp dir (not the shared temp trees), and no Apple
/// Events, LaunchServices, clipboard or Shortcuts.
// The socket rules are built from Unix paths; Windows has no Seatbelt profile.
#[cfg(unix)]
#[test]
fn profile_blocks_ipc_escape_hatches() {
    let home = std::path::PathBuf::from("/Users/x");
    let writable = vec![
        "/private/tmp/proj".to_string(),
        "/private/tmp".to_string(),
        "/private/var/folders".to_string(),
    ];
    let profile = sandbox_profile_with_paths(&writable, &home, &SandboxConfig::default(), None);
    assert!(profile.contains("(deny network-outbound (remote unix-socket))"));
    assert!(
        profile.contains(
            "(allow network-outbound (remote unix-socket (subpath \"/private/tmp/proj\")))"
        )
    );
    for shared in ["/private/tmp", "/private/var/folders"] {
        assert!(
            !profile.contains(&format!("(remote unix-socket (subpath \"{shared}\"))")),
            "{profile}"
        );
    }
    assert!(profile.contains(&format!(
        "(allow network-outbound (remote unix-socket (subpath \"{}\")))",
        sbpl_quote(&guards::norm(&sandbox_tmpdir()))
    )));
    assert!(profile.contains("(deny appleevent-send)"));
    assert!(profile.contains("com.apple.coreservices.launchservicesd"));
    assert!(profile.contains("com.apple.pasteboard.1"));
    assert!(profile.contains("siri\\\\.VoiceShortcuts"), "{profile}");
    assert!(
        profile.contains("^com\\\\.apple\\\\.shortcuts"),
        "{profile}"
    );
    assert!(profile.contains(r"docker\\.sock$"), "{profile}");
}

/// The real profile, as `build_command_for_policy` makes it, run through
/// `sandbox-exec`: ordinary git work succeeds, while swapping `.claude` for
/// a prepared copy and connecting to a Unix socket outside the project and
/// the command's temp dir both fail. `script` (a pty the command opens
/// itself) still works with the other terminals locked.
#[cfg(target_os = "macos")]
#[test]
fn seatbelt_real_profile_end_to_end() {
    use std::process::Command;
    // Short: a socket path must fit in sun_path (104 bytes).
    let root = std::env::temp_dir().join(format!("ce2e{}", &uuid::Uuid::new_v4().to_string()[..6]));
    std::fs::create_dir_all(&root).unwrap();
    let root = crate::util::canonical(&root).unwrap();
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .args(args)
            .current_dir(&root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::create_dir_all(root.join(".claude")).unwrap();
    std::fs::write(root.join(".claude/settings.json"), "{}").unwrap();
    let inside = root.join("in.sock");
    let outside = std::path::PathBuf::from(format!(
        "/private/tmp/chanto-{}.sock",
        &uuid::Uuid::new_v4().to_string()[..8]
    ));
    let _l1 = std::os::unix::net::UnixListener::bind(&inside).unwrap();
    let _l2 = std::os::unix::net::UnixListener::bind(&outside).unwrap();
    let profile = sandbox_profile_with_port(&root, &SandboxConfig::default(), None);
    let connect = "perl -MIO::Socket::UNIX -e 'IO::Socket::UNIX->new(Peer => shift) or exit 1'";
    let script = format!(
        "cd \"$0\" || exit 9; export GIT_CONFIG_GLOBAL=/dev/null; \
         echo hi > f && git add f && git commit -qm m && echo COMMITTED; \
         mv .claude .claude.orig 2>/dev/null && echo MOVED; \
         mkdir -p evil && mv evil .vscode 2>/dev/null && echo PLACED; \
         {connect} \"$1\" && echo CONNECTED_INSIDE; \
         {connect} \"$2\" && echo CONNECTED_OUTSIDE; \
         script -q /dev/null /usr/bin/true </dev/null >/dev/null 2>&1 && echo SCRIPT_OK; true"
    );
    let Some(out) = seatbelt(&profile, &script, &[&root, &inside, &outside]) else {
        let _ = std::fs::remove_file(&outside);
        return;
    };
    let _ = std::fs::remove_file(&outside);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("COMMITTED"),
        "git commit failed: {text}\n{profile}"
    );
    assert!(!text.contains("MOVED"), "renamed .claude: {text}");
    assert!(
        !text.contains("PLACED"),
        "created .vscode by rename: {text}"
    );
    assert!(root.join(".claude/settings.json").exists());
    assert!(
        text.contains("CONNECTED_INSIDE"),
        "project socket refused: {text}"
    );
    assert!(
        !text.contains("CONNECTED_OUTSIDE"),
        "reached a shared-temp socket: {text}"
    );
    assert!(
        text.contains("SCRIPT_OK"),
        "script(1) failed: {text}\n{profile}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Renaming `.git` used to sidestep the `.git/config` and `.git/hooks`
/// denies; the literal pin refuses it, and writing a fresh `.git` file is
/// refused too.
#[cfg(target_os = "macos")]
#[test]
fn seatbelt_refuses_renaming_dot_git() {
    let root = scratch("gitpin");
    std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
    std::fs::write(root.join(".git/config"), "[core]\n").unwrap();
    let writable = vec![root.display().to_string(), "/dev".to_string()];
    let profile = sandbox_profile_with_paths(
        &writable,
        &root.join("home"),
        &SandboxConfig::default(),
        None,
    );
    let script = "mv \"$0/.git\" \"$0/.g\" 2>/dev/null && echo RENAMED; \
                  : > \"$0/.git\" 2>/dev/null && echo REPLACED; \
                  rm -rf \"$0/.git\" 2>/dev/null && echo REMOVED; true";
    let Some(out) = seatbelt(&profile, script, &[&root]) else {
        return;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("RENAMED"), "renamed .git:\n{profile}");
    assert!(!text.contains("REPLACED"), "replaced .git:\n{profile}");
    assert!(!text.contains("REMOVED"), "removed .git:\n{profile}");
    assert!(root.join(".git/config").exists());
    let _ = std::fs::remove_dir_all(&root);
}
