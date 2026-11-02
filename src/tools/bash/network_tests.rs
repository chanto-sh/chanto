use super::*;
use crate::config::{NetworkPolicy, SandboxConfig};

#[test]
fn seatbelt_allows_only_proxy_endpoint() {
    let cfg = SandboxConfig {
        network: Some(NetworkPolicy::Allowlist),
        ..Default::default()
    };
    let root = std::env::temp_dir();
    let profile = sandbox_profile_with_port(&root, &cfg, Some(12345));
    assert!(profile.contains("(deny network*)"));
    assert!(profile.contains("(allow network-outbound (remote ip \"localhost:12345\"))"));
    assert!(profile.contains("(remote unix-socket)"));
    assert!(!sandbox_profile(&root, &SandboxConfig::default()).contains("(deny network*)"));
    assert!(
        sandbox_profile(
            &root,
            &SandboxConfig {
                deny_network: true,
                ..Default::default()
            }
        )
        .contains("(deny network*)")
    );
}

#[test]
#[cfg(target_os = "macos")]
fn profile_denies_writes_to_execution_paths_in_writable_trees() {
    let cwd = std::env::temp_dir().join(format!("chanto-wd-{}", uuid::Uuid::new_v4()));
    let profile = sandbox_profile(&cwd, &SandboxConfig::default());
    // The project tree is writable, but its git internals are not. The
    // deny names the resolved path (temp_dir is under a symlink here),
    // since that is what Seatbelt matches.
    let allow_at = profile.find("(allow file-write*").unwrap();
    let git = crate::permissions::normalize(&cwd.join(".git/hooks"))
        .display()
        .to_string();
    let deny = format!("(deny file-write* (subpath \"{}\"))", sbpl_quote(&git));
    assert!(
        profile.contains(&deny),
        "missing git-hooks deny:\n{profile}"
    );
    // The deny comes after the allow block, so SBPL's last match wins.
    assert!(profile.find(&deny).unwrap() > allow_at);
    // Cargo's bin directory is denied even though ~/.cargo is writable.
    assert!(
        profile
            .lines()
            .any(|l| l.starts_with("(deny file-write*") && l.contains(".cargo/bin")),
        "missing cargo-bin deny:\n{profile}"
    );
}

#[test]
fn path_based_profile_preserves_proxy_rules_and_secret_denials() {
    let root = std::env::temp_dir().join(format!("chanto-profile-{}", uuid::Uuid::new_v4()));
    let home = root.join("home");
    let writable = vec![root.join("project").display().to_string()];
    let secret = home.join("private-key").display().to_string();
    let cfg = SandboxConfig {
        network: Some(NetworkPolicy::Allowlist),
        deny_read: vec![secret.clone()],
        ..Default::default()
    };
    let profile = sandbox_profile_with_paths(&writable, &home, &cfg, Some(12345));
    assert!(profile.contains(&format!("(subpath \"{}\")", sbpl_quote(&writable[0]))));
    let resolved = crate::permissions::normalize(std::path::Path::new(&secret));
    assert!(profile.contains(&format!(
        "(deny file-read* (subpath \"{}\"))",
        sbpl_quote(&resolved.display().to_string())
    )));
    assert!(profile.contains("(deny network*)"));
    assert!(profile.contains("(remote ip \"localhost:12345\")"));
}

#[cfg(windows)]
#[test]
fn windows_allowlist_fails_closed() {
    let cfg = SandboxConfig {
        network: Some(NetworkPolicy::Allowlist),
        ..Default::default()
    };
    let error = build_command_for_policy(
        "echo test",
        &std::env::temp_dir(),
        Some(&cfg),
        None,
        false,
        true,
    )
    .err()
    .unwrap();
    assert!(
        error.to_string().contains("unsupported on Windows"),
        "{error} (sandbox available: {})",
        sandbox_available()
    );
    assert!(
        build_command_for_policy(
            "echo test",
            &std::env::temp_dir(),
            Some(&cfg),
            None,
            true,
            true
        )
        .is_ok()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_network_policy_fails_closed_before_abi_v4() {
    if landlock_abi() >= 4 {
        return;
    }
    let cfg = SandboxConfig {
        network: Some(NetworkPolicy::Allowlist),
        ..Default::default()
    };
    let error = build_command_for_policy(":", &std::env::temp_dir(), Some(&cfg), None, false, true)
        .err()
        .unwrap();
    assert!(error.to_string().contains("network sandbox"));
    let allowed = SandboxConfig::default();
    assert!(
        build_command_for_policy(
            ":",
            &std::env::temp_dir(),
            Some(&allowed),
            None,
            false,
            true
        )
        .is_ok()
    );
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
fn linux_network_socket_filter() {
    // Apply seccomp only in an isolated test process, never in the runner.
    if std::env::var("CHANTO_TEST_SECCOMP").as_deref() != Ok("1") {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::bash::network_tests::linux_network_socket_filter",
                "--nocapture",
            ])
            .env("CHANTO_TEST_SECCOMP", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
        0
    );
    restrict_network_sockets().unwrap();
    assert!(std::net::TcpListener::bind(("127.0.0.1", 0)).is_ok());
    assert_eq!(
        std::net::UdpSocket::bind(("127.0.0.1", 0))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EPERM)
    );
    let unix = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0) };
    assert!(unix >= 0);
    unsafe {
        libc::close(unix);
    }
    for (family, kind, protocol) in [
        (libc::AF_INET, libc::SOCK_RAW, libc::IPPROTO_ICMP),
        (libc::AF_INET, libc::SOCK_STREAM, libc::IPPROTO_SCTP),
        (libc::AF_PACKET, libc::SOCK_RAW, 0),
    ] {
        assert_eq!(unsafe { libc::socket(family, kind, protocol) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
    }
    assert_eq!(
        unsafe { libc::syscall(libc::SYS_io_uring_setup, 0, std::ptr::null::<u8>()) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EPERM)
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_allowlist_proxy_works_and_direct_connection_fails() {
    use tokio::io::AsyncWriteExt;
    if !sandbox_available() {
        return;
    }
    let check = std::process::Command::new("/usr/bin/sandbox-exec")
        .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
        .output()
        .unwrap();
    if !check.status.success() {
        eprintln!(
            "skipping Seatbelt integration: {}",
            String::from_utf8_lossy(&check.stderr)
        );
        return;
    }
    let _lock = SANDBOX_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let root = std::env::temp_dir().join(format!("chanto-net-sandbox-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        loop {
            let (mut client, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(client.read_u8().await.unwrap());
                }
                client
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nfixture",
                    )
                    .await
                    .unwrap();
            });
        }
    });
    let cfg = SandboxConfig {
        network: Some(NetworkPolicy::Allowlist),
        allowed_domains: Some(vec!["127.0.0.1".into()]),
        ..Default::default()
    };
    let script = format!("/usr/bin/curl -fsS --max-time 3 http://127.0.0.1:{port}/");
    let (mut cmd, _lease) =
        build_command_for_policy(&script, &root, Some(&cfg), None, false, true).unwrap();
    let out = cmd.output().await.unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"fixture");
    let script = format!("/usr/bin/curl -fsS --noproxy '*' --max-time 3 http://127.0.0.1:{port}/");
    let (mut cmd, _lease) =
        build_command_for_policy(&script, &root, Some(&cfg), None, false, true).unwrap();
    assert!(
        !cmd.output().await.unwrap().status.success(),
        "direct connection must fail"
    );
    let deny = SandboxConfig {
        network: Some(NetworkPolicy::Deny),
        ..Default::default()
    };
    let (mut cmd, _) =
        build_command_for_policy(&script, &root, Some(&deny), None, false, true).unwrap();
    assert!(
        !cmd.output().await.unwrap().status.success(),
        "deny must block direct connections"
    );

    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}
