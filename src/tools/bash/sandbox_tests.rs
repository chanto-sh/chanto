use super::*;

async fn run(
    cmd: &str,
    cwd: &std::path::Path,
    cfg: &crate::config::SandboxConfig,
    unsandboxed: bool,
) -> (bool, String) {
    // Use a narrow grant for backend tests so the outside fixture can live
    // in temp too. Never create fixtures or scan secrets in the real home.
    let writable = vec![cwd.display().to_string()];
    let home = cwd.join("test-home");
    let (mut command, _network) =
        build_command_for_policy(cmd, cwd, None, None, true, false).unwrap();
    if !unsandboxed {
        #[cfg(target_os = "macos")]
        {
            let (prog, flags) = crate::shell::shell();
            command = tokio::process::Command::new("/usr/bin/sandbox-exec");
            command
                .arg("-p")
                .arg(sandbox_profile_with_paths(&writable, &home, cfg, None))
                .arg(prog)
                .args(flags)
                .arg(cmd)
                .current_dir(cwd);
        }
        #[cfg(target_os = "linux")]
        landlock_writes(
            &mut command,
            &writable,
            &read_allow_paths(&read_denied_paths(&home, &cfg.deny_read)),
            false,
            None,
        );
    }
    let o = command.output().await.unwrap();
    (
        o.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
    )
}

#[test]
fn spots_whole_disk_scans() {
    assert!(scans_everything(
        "find / -name cli.py -path '*flask*' 2>/dev/null | head"
    ));
    assert!(scans_everything("ls; find ~ -maxdepth 6 -name x"));
    assert!(scans_everything("find \"$HOME\" -name '*.whl'"));
    assert!(!scans_everything("find . -name '*.py'"));
    assert!(!scans_everything("find src/ -name x"));
}

#[test]
fn read_allow_paths_skip_only_denied() {
    let base = std::env::temp_dir()
        .canonical()
        .unwrap()
        .join(format!("chanto-rap-{}", std::process::id()));
    std::fs::create_dir_all(base.join("sub")).unwrap();
    std::fs::write(base.join("secret"), "x").unwrap();
    std::fs::write(base.join("ok"), "x").unwrap();
    let allowed = read_allow_paths(&[base.join("secret")]);
    assert!(allowed.contains(&base.join("ok")) && allowed.contains(&base.join("sub")));
    assert!(!allowed.contains(&base.join("secret")) && !allowed.contains(&base));
    assert!(
        !allowed.iter().any(|p| base.starts_with(p)),
        "no ancestor of a denied path is granted whole"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn read_allow_paths_walk_into_ancestors_of_a_deep_denial() {
    // Landlock only grants access, so denying a file nested several levels
    // deep means granting every ancestor directory *except* the denied leaf,
    // and its siblings at each level. Getting this wrong either blocks the
    // whole home tree or leaks the denied path.
    let root = std::env::temp_dir()
        .canonical()
        .unwrap()
        .join(format!("chanto-rap-deep-{}", std::process::id()));
    let denied = root.join("a/b/secret");
    std::fs::create_dir_all(denied.parent().unwrap()).unwrap();
    std::fs::create_dir_all(root.join("a/c")).unwrap();
    std::fs::write(root.join("a/keep"), "x").unwrap();
    std::fs::write(&denied, "x").unwrap();
    let allowed = read_allow_paths(std::slice::from_ref(&denied));
    // The denied leaf is never granted...
    assert!(!allowed.contains(&denied));
    // ...its ancestors are traversed rather than granted wholesale...
    for anc in [root.clone(), root.join("a"), root.join("a/b")] {
        assert!(
            !allowed.contains(&anc),
            "ancestor {anc:?} must not be granted whole"
        );
    }
    // ...but their siblings are readable.
    assert!(allowed.contains(&root.join("a/keep")));
    assert!(allowed.contains(&root.join("a/c")));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn the_shell_snapshot_works_inside_the_sandbox() {
    if !test_sandbox_available() {
        return;
    }
    let _home = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _g = SANDBOX_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = crate::shell::snapshot::tests::home();
    std::fs::write(home.join(".bashrc"), crate::shell::snapshot::tests::RC).unwrap();
    let snap = crate::shell::snapshot::create(
        std::path::Path::new("/bin/bash"),
        Some(&home),
        Duration::from_secs(20),
        &Default::default(),
    )
    .unwrap();
    let project =
        std::env::temp_dir().join(format!("chanto-sbx-snap-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&project).unwrap();
    let script = format!(
        "{}hx-only-tool && hxgreet && hxfn s && echo hi > inside.txt",
        crate::shell::snapshot::source_line(&snap)
    );
    let cfg = crate::config::SandboxConfig::default();
    let (mut cmd, _lease) =
        build_command_for_policy(&script, &project, Some(&cfg), None, false, true).unwrap();
    let o = cmd.output().await.unwrap();
    let out = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(o.status.success(), "{out}");
    assert!(
        out.contains("tool-ran") && out.contains("hello-from-alias") && out.contains("fn-got-s"),
        "{out}"
    );
    assert!(project.join("inside.txt").exists());
    let _ = std::fs::remove_dir_all(snap.parent().unwrap());
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&project);
}

#[tokio::test]
async fn sandbox_limits_writes_and_secret_reads() {
    if !test_sandbox_available() {
        return;
    }
    // Tests that move CHANTO_HOME hold this lock; the sandbox's test home
    // is CHANTO_HOME, so keep it still while this runs.
    let _home = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _g = SANDBOX_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let home = sandbox_home();
    let project = std::env::temp_dir().join(format!("chanto-sbx-{}", std::process::id()));
    std::fs::create_dir_all(&project).unwrap();

    let outside = home.join(format!(".chanto-sbx-outside-{}", std::process::id()));
    let secret_dir = home.join(format!(".chanto-sbx-secret-{}", std::process::id()));
    // Keep the secret fixture in the isolated test home.
    if std::fs::create_dir_all(&secret_dir)
        .and_then(|_| std::fs::write(secret_dir.join("key"), "s3cret"))
        .is_err()
    {
        let _ = std::fs::remove_dir_all(&project);

        return;
    }
    let cfg = crate::config::SandboxConfig {
        deny_read: vec![secret_dir.display().to_string()],
        ..Default::default()
    };

    let (ok, out) = run("echo hi > inside.txt", &project, &cfg, false).await;
    assert!(
        ok && project.join("inside.txt").exists(),
        "writes inside the project work: {out}"
    );
    // The project is writable, but its git internals are not: a hook
    // written here would otherwise run, unsandboxed, on the next git call.
    // The Seatbelt profile denies writes to .git internals inside the
    // writable project. Landlock doesn't carve them out: that would also
    // take away creating files in the project (see landlock_write_allow).
    #[cfg(target_os = "macos")]
    {
        std::fs::create_dir_all(project.join(".git/hooks")).unwrap();
        let (ok, _) = run(
            "echo payload > .git/hooks/pre-commit",
            &project,
            &cfg,
            false,
        )
        .await;
        assert!(
            !ok && !project.join(".git/hooks/pre-commit").exists(),
            "writing a git hook must fail in the sandbox"
        );
    }
    let (ok, out) = run(
        &format!("echo hi > '{}'", outside.display()),
        &project,
        &cfg,
        false,
    )
    .await;
    assert!(
        !ok && !outside.exists(),
        "write outside the project must fail: {out}"
    );
    let (ok, out) = run(
        &format!("echo hi > '{}'", outside.display()),
        &project,
        &cfg,
        true,
    )
    .await;
    assert!(
        ok && outside.exists(),
        "approved outside write works: {out}"
    );
    std::fs::remove_file(&outside).unwrap();
    let (ok, out) = run(
        &format!("cat '{}'", secret_dir.join("key").display()),
        &project,
        &cfg,
        false,
    )
    .await;
    assert!(
        !ok && !out.contains("s3cret"),
        "denied read must fail: {out}"
    );
    assert!(looks_sandbox_blocked(&out), "{out}");
    assert!(
        looks_network_failure(
            "curl: (7) Failed to connect to 127.0.0.1 port 11434 after 0 ms: Couldn't connect to server"
        ) && !looks_network_failure("error: no such file")
    );
    // Approved commands can read the otherwise denied fixture.
    let (ok, out) = run(
        &format!("cat '{}'", secret_dir.join("key").display()),
        &project,
        &cfg,
        true,
    )
    .await;
    assert!(ok && out.contains("s3cret"), "approved read works");
    let _ = std::fs::remove_dir_all(&secret_dir);
    let _ = std::fs::remove_dir_all(&project);
}

/// Landlock grants access rather than denying it, so the read-deny path is
/// only exercised on Linux. Assert the denied file is unreadable *and* that
/// its neighbours stay readable — the failure mode of the ancestor-walk is
/// locking down too much, which this catches.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn landlock_denies_a_secret_but_not_its_neighbours() {
    if !test_sandbox_available() {
        return;
    }
    let _home = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _g = SANDBOX_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let home = sandbox_home();
    let project = std::env::temp_dir().join(format!("chanto-ll-sbx-{}", std::process::id()));
    std::fs::create_dir_all(&project).unwrap();
    let dir = home.join(format!(".chanto-ll-{}", std::process::id()));
    // Keep fixtures in the isolated test home.
    if std::fs::create_dir_all(&dir)
        .and_then(|_| std::fs::write(dir.join("secret"), "s3cret"))
        .and_then(|_| std::fs::write(dir.join("public"), "hello"))
        .is_err()
    {
        let _ = std::fs::remove_dir_all(&project);

        return;
    }
    // Deny the one file; the file next to it must stay readable.
    let cfg = crate::config::SandboxConfig {
        deny_read: vec![dir.join("secret").display().to_string()],
        ..Default::default()
    };

    let (ok, out) = run(
        &format!("cat '{}'", dir.join("secret").display()),
        &project,
        &cfg,
        false,
    )
    .await;
    assert!(
        !ok && !out.contains("s3cret"),
        "denied read must fail: {out}"
    );
    let (ok, out) = run(
        &format!("cat '{}'", dir.join("public").display()),
        &project,
        &cfg,
        false,
    )
    .await;
    assert!(
        ok && out.contains("hello"),
        "sibling of a denied path stays readable: {out}"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&project);
}
