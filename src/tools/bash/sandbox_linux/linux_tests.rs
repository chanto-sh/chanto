use super::*;

/// Writable trees are granted whole on Linux: carving the project around
/// `.git` used to take away the right to create anything at its root or
/// in `.git` (no `target/`, no `index.lock`).
#[test]
fn project_and_temp_are_granted_whole() {
    let root = std::env::temp_dir().join(format!("chanto-llwa-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
    std::fs::create_dir_all(root.join(".chanto")).unwrap();
    let home = std::env::temp_dir().join(format!("chanto-llwa-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    let root_n = crate::permissions::normalize(&root);
    let allow = landlock_write_allow(&[root.display().to_string(), "/dev".into()], &home);
    assert!(allow.contains(&root_n), "{allow:?}");
    assert!(
        allow.contains(&std::path::PathBuf::from("/dev")),
        "{allow:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&home);
}

/// A widened tree (here `allow_write = ["~"]`) is carved around the home
/// guards, and a symlink in it is never granted (Landlock would follow it).
#[test]
fn widened_home_is_carved_and_symlinks_skipped() {
    let base = std::env::temp_dir().join(format!("chanto-llwa-{}", uuid::Uuid::new_v4()));
    let home = base.join("home");
    std::fs::create_dir_all(home.join(".cargo/bin")).unwrap();
    std::fs::create_dir_all(home.join(".cargo/registry")).unwrap();
    std::fs::create_dir_all(home.join("notes")).unwrap();
    std::fs::write(home.join(".zshrc"), "").unwrap();
    std::os::unix::fs::symlink("/", home.join("link")).unwrap();
    let project = base.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let allow = landlock_write_allow(
        &[project.display().to_string(), home.display().to_string()],
        &home,
    );
    let n = |p: std::path::PathBuf| crate::permissions::normalize(&p);
    let granted = |p: &std::path::Path| allow.iter().any(|g| p.starts_with(g));
    assert!(granted(&n(home.join("notes"))), "{allow:?}");
    assert!(granted(&n(home.join(".cargo/registry"))), "{allow:?}");
    assert!(!granted(&n(home.join(".cargo/bin"))), "{allow:?}");
    assert!(!granted(&n(home.join(".zshrc"))), "{allow:?}");
    assert!(!allow.contains(&n(home.join("link"))), "{allow:?}");
    assert!(!allow.contains(&n(home.clone())), "{allow:?}");
    let _ = std::fs::remove_dir_all(&base);
}

/// Both seccomp filters installed in a forked child: ordinary syscalls
/// still work (the ioctl filter once denied every one), TIOCSTI does not.
#[test]
fn seccomp_filters_allow_ordinary_syscalls() {
    // SAFETY: the child only makes raw syscalls (no allocation, no locks)
    // and leaves with _exit.
    unsafe {
        let pid = libc::fork();
        assert!(pid >= 0);
        if pid == 0 {
            libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
            if restrict_network_sockets().is_err() || block_tty_injection().is_err() {
                libc::_exit(10);
            }
            if libc::getppid() <= 0 || libc::close(libc::dup(2)) != 0 {
                libc::_exit(11);
            }
            let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
            if fd < 0 {
                libc::_exit(12);
            }
            libc::close(fd);
            if libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) >= 0 {
                libc::_exit(13);
            }
            let c: libc::c_char = b'x' as libc::c_char;
            if libc::ioctl(0, libc::TIOCSTI, &c) != -1 || *libc::__errno_location() != libc::EPERM {
                libc::_exit(14);
            }
            libc::_exit(0);
        }
        let mut status = 0;
        libc::waitpid(pid, &mut status, 0);
        assert!(libc::WIFEXITED(status), "child died: {status}");
        assert_eq!(libc::WEXITSTATUS(status), 0, "filter check failed");
    }
}

/// The real command builder on Linux, run end to end: a sandboxed shell
/// starts at all (both seccomp filters and Landlock applied), creates
/// files and directories at the project root and a git lock file in
/// `.git`, and still can't write outside its trees.
#[tokio::test]
async fn sandboxed_command_runs_and_creates_at_the_root() {
    if !super::super::test_sandbox_available() {
        return;
    }
    let _g = super::super::SANDBOX_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let root = std::env::temp_dir().join(format!("chanto-llrun-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join(".git")).unwrap();
    // Also in temp, but outside the one tree granted here.
    let outside = std::env::temp_dir().join(format!("chanto-outside-{}", uuid::Uuid::new_v4()));
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(
            "true && mkdir target && touch new.rs .git/index.lock && echo CREATED; \
                  touch \"$0\" 2>/dev/null && echo ESCAPED; true",
        )
        .arg(&outside)
        .current_dir(&root);
    let readable = super::super::read_allow_paths(&[]);
    landlock_writes(
        &mut command,
        &[root.display().to_string(), "/dev".into()],
        &readable,
        landlock_abi() >= 4,
        None,
    );
    // SAFETY: as in build_command_for_policy.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            block_tty_injection()
        });
    }
    let o = command.output().await.unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(o.status.success(), "sandboxed sh failed: {text}");
    assert!(text.contains("CREATED"), "{text}");
    assert!(!text.contains("ESCAPED"), "{text}");
    assert!(root.join("target").is_dir() && root.join(".git/index.lock").exists());
    assert!(!outside.exists());
    let _ = std::fs::remove_dir_all(&root);
}
