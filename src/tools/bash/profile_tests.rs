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
