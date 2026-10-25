//! Linux: Landlock and seccomp for sandboxed commands.

use super::sandbox::{landlock_abi, sandbox_home};
use super::*;

/// The trees a sandboxed command may write, as Landlock rules.
///
/// Landlock only grants access, never denies it. Keeping a path read-only
/// inside a writable tree means granting the tree's other children instead
/// of the tree, and a directory that isn't granted as a whole loses the
/// right to create entries in it. Doing that to the project (to protect
/// `.git` or `.chanto`) breaks `git commit` (`.git/index.lock`), new files at
/// the root and `target/`; doing it to `/dev` breaks ptys. So:
///
/// - the project, temp dirs and `/dev` are granted whole: the in-project
///   guards are not enforced on Linux (the permission layer still asks
///   before file tools touch them);
/// - any other writable tree (download caches, `allow_write`, `--add-dir`)
///   is carved around the home guards beneath it: Chanto's home, shell
///   startup files, toolchain bins and settings. Those only lie in such a
///   tree when the user widened it, e.g. `allow_write = ["~"]`.
///
/// Symlinked children are never granted while carving: Landlock follows
/// them, so a link in a writable tree would grant its target.
#[cfg(target_os = "linux")]
pub(super) fn landlock_write_allow(
    writable: &[String],
    home: &std::path::Path,
) -> Vec<std::path::PathBuf> {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    let roots: Vec<PathBuf> = writable
        .iter()
        .map(|w| crate::permissions::normalize(Path::new(w)))
        .collect();
    let mut whole: HashSet<PathBuf> = ["/dev", "/tmp", "/var/tmp", "/dev/shm"]
        .iter()
        .map(PathBuf::from)
        .collect();
    whole.extend(roots.first().cloned());
    if let Some(t) = std::env::var_os("TMPDIR") {
        whole.insert(crate::permissions::normalize(Path::new(&t)));
    }
    let denied: HashSet<PathBuf> = super::guards::home_guards(home)
        .iter()
        .map(|p| crate::permissions::normalize(p))
        .filter(|p| p.is_absolute())
        .collect();
    let ancestors: HashSet<PathBuf> = denied
        .iter()
        .flat_map(|p| p.ancestors().skip(1).map(Path::to_path_buf))
        .collect();
    let mut out = Vec::new();
    for root in roots {
        if whole.contains(&root) {
            out.push(root);
            continue;
        }
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            if denied.contains(&dir) {
                continue;
            }
            if !ancestors.contains(&dir) {
                out.push(dir); // No guard beneath: grant the whole subtree.
                continue;
            }
            // A guarded path lies below: grant each other child, walk into
            // the ones on the way to a guard.
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                if e.file_type().is_ok_and(|t| t.is_symlink()) {
                    continue;
                }
                stack.push(e.path());
            }
        }
    }
    out
}

/// Linux: restrict a child's writes with Landlock. The ruleset is built here,
/// in the parent; the child only applies it, right before exec.
#[cfg(target_os = "linux")]
pub(super) fn landlock_writes(
    cmd: &mut tokio::process::Command,
    writable: &[String],
    readable: &[std::path::PathBuf],
    restricted_network: bool,
    proxy_port: Option<u16>,
) {
    let home = sandbox_home();
    let home = home.canonical().unwrap_or(home);
    let writable: Vec<String> = landlock_write_allow(writable, &home)
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    let writable = &writable[..];
    use landlock::{
        ABI, AccessFs, AccessNet, NetPort, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
        path_beneath_rules,
    };
    // Ask for exactly the rights the running kernel understands: requesting V3
    // rights on an older kernel makes ruleset creation fail, which used to leave
    // the command running unsandboxed.
    let abi = match landlock_abi() {
        v if v >= 4 => ABI::V4,
        3 => ABI::V3,
        2 => ABI::V2,
        _ => ABI::V1,
    };
    let built = Ruleset::default()
        .handle_access(AccessFs::from_write(abi) | AccessFs::ReadFile)
        .and_then(|r| {
            if restricted_network {
                r.handle_access(AccessNet::ConnectTcp)
            } else {
                Ok(r)
            }
        })
        .and_then(|r| r.create())
        .and_then(|r| {
            if let Some(port) = proxy_port {
                r.add_rule(NetPort::new(port, AccessNet::ConnectTcp))
            } else {
                Ok(r)
            }
        })
        .and_then(|r| r.add_rules(path_beneath_rules(writable, AccessFs::from_write(abi))))
        .and_then(|r| r.add_rules(path_beneath_rules(readable, AccessFs::ReadFile)));
    // Fail closed when the kernel advertises Landlock but the ruleset won't
    // apply: a sandbox that silently didn't take effect is worse than a command
    // that refuses to run. (The model can retry with `dangerouslyDisableSandbox`,
    // which asks a human.) Without Landlock at all there is nothing to enforce.
    let state = std::sync::Mutex::new(Some(match built {
        Ok(r) => Ok(r),
        Err(e) if sandbox_available() => Err(e.to_string()),
        Err(_) => return,
    }));
    // SAFETY: the closure takes the prebuilt ruleset and makes the
    // restrict_self syscalls; it doesn't allocate or take other locks.
    unsafe {
        cmd.pre_exec(move || match state.lock().ok().and_then(|mut g| g.take()) {
            Some(Ok(r)) => {
                let st = r
                    .restrict_self()
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                if st.ruleset != RulesetStatus::FullyEnforced {
                    return Err(std::io::Error::other(format!(
                        "landlock not fully enforced ({:?})",
                        st.ruleset
                    )));
                }
                if restricted_network {
                    restrict_network_sockets()?;
                }
                Ok(())
            }
            Some(Err(e)) => Err(std::io::Error::other(format!(
                "landlock ruleset unavailable: {e}"
            ))),
            None => Err(std::io::Error::other("landlock state missing")),
        });
    }
}

/// Landlock v4 handles TCP only. Prevent UDP/raw/packet sockets (and io_uring
/// socket operations) from bypassing it. Unix sockets remain available for DNS.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(super) fn restrict_network_sockets() -> std::io::Result<()> {
    let stmt = |code, k| libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    };
    let jump = |k, jt, jf| libc::sock_filter {
        code: 0x15,
        jt,
        jf,
        k,
    };
    #[cfg(target_arch = "x86_64")]
    let arch = 0xc000003e;
    #[cfg(target_arch = "aarch64")]
    let arch = 0xc00000b7;
    let deny = 0x00050000 | libc::EPERM as u32;
    let mut filters = [
        stmt(0x20, 4), // seccomp_data.arch
        jump(arch, 1, 0),
        stmt(0x06, 0x80000000), // kill on ABI mismatch
        stmt(0x20, 0),          // syscall number
        jump(libc::SYS_io_uring_setup as u32, 0, 1),
        stmt(0x06, deny),
        // Reject x32 syscall numbers too (harmless on aarch64).
        stmt(0x45, 0x40000000),
        stmt(0x06, deny),
        jump(libc::SYS_socket as u32, 0, 11),
        stmt(0x20, 16), // args[0]: socket family
        jump(libc::AF_UNIX as u32, 9, 0),
        jump(libc::AF_INET as u32, 1, 0),
        jump(libc::AF_INET6 as u32, 0, 6),
        stmt(0x20, 24),  // args[1]: socket type
        stmt(0x54, 0xf), // strip NONBLOCK / CLOEXEC
        jump(libc::SOCK_STREAM as u32, 0, 3),
        stmt(0x20, 32), // args[2]: protocol (block SCTP and other non-TCP streams)
        jump(0, 2, 0),
        jump(libc::IPPROTO_TCP as u32, 1, 0),
        stmt(0x06, deny),
        stmt(0x06, 0x7fff0000), // allow
    ];
    // JSET needs explicit branches: deny if the x32 bit is present.
    filters[6].jt = 0;
    filters[6].jf = 1;
    let prog = libc::sock_fprog {
        len: filters.len() as u16,
        filter: filters.as_mut_ptr(),
    };
    // SAFETY: the kernel copies this bounded BPF program before returning.
    if unsafe { libc::prctl(libc::PR_SET_SECCOMP, 2, &prog) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(all(
    target_os = "linux",
    not(any(target_arch = "x86_64", target_arch = "aarch64"))
))]
pub(super) fn restrict_network_sockets() -> std::io::Result<()> {
    Err(std::io::Error::other(
        "network sandbox requires seccomp support for this architecture",
    ))
}

/// Refuse the terminal-injection ioctls. Even with its own session a command
/// that somehow reached a tty fd could push input back with TIOCSTI or take
/// the console with TIOCLINUX; deny those two requests and leave every other
/// ioctl alone. Runs after Landlock's `restrict_self`, which has already set
/// `no_new_privs`, so this extra seccomp layer is allowed to install.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(super) fn block_tty_injection() -> std::io::Result<()> {
    const TIOCSTI: u32 = 0x5412;
    const TIOCLINUX: u32 = 0x541c;
    let stmt = |code, k| libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    };
    let jeq = |k, jt, jf| libc::sock_filter {
        code: 0x15, // BPF_JMP | BPF_JEQ | BPF_K
        jt,
        jf,
        k,
    };
    let jset = |k, jt, jf| libc::sock_filter {
        code: 0x45, // BPF_JMP | BPF_JSET | BPF_K
        jt,
        jf,
        k,
    };
    // x32 syscalls carry this bit; their ioctl has another number, so the
    // filter would wave them through. No sandboxed tool needs x32.
    const X32: u32 = 0x40000000;
    #[cfg(target_arch = "x86_64")]
    let arch = 0xc000003e;
    #[cfg(target_arch = "aarch64")]
    let arch = 0xc00000b7;
    let deny = 0x00050000 | libc::EPERM as u32;
    let allow = 0x7fff0000;
    let mut filters = [
        stmt(0x20, 4),                     // 0: seccomp_data.arch
        jeq(arch, 1, 0),                   // 1: right arch?
        stmt(0x06, 0x80000000),            // 2: kill on ABI mismatch
        stmt(0x20, 0),                     // 3: syscall number
        jset(X32, 5, 0),                   // 4: x32 numbering -> deny
        jeq(libc::SYS_ioctl as u32, 0, 3), // 5: not ioctl -> allow
        stmt(0x20, 24),                    // 6: args[1]: the ioctl request (low word)
        jeq(TIOCSTI, 2, 0),                // 7: -> deny
        jeq(TIOCLINUX, 1, 0),              // 8: -> deny
        stmt(0x06, allow),                 // 9
        stmt(0x06, deny),                  // 10
    ];
    // SAFETY: no_new_privs is idempotent; a second call before SECCOMP is fine.
    unsafe {
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    }
    let prog = libc::sock_fprog {
        len: filters.len() as u16,
        filter: filters.as_mut_ptr(),
    };
    // SAFETY: the kernel copies this bounded BPF program before returning.
    if unsafe { libc::prctl(libc::PR_SET_SECCOMP, 2, &prog) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(all(
    target_os = "linux",
    not(any(target_arch = "x86_64", target_arch = "aarch64"))
))]
pub(super) fn block_tty_injection() -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
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
                if libc::ioctl(0, libc::TIOCSTI, &c) != -1
                    || *libc::__errno_location() != libc::EPERM
                {
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
}
