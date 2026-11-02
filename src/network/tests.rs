use super::*;
use crate::config::NetworkPolicy;

fn cfg(allowed: &[&str], denied: &[&str]) -> SandboxConfig {
    SandboxConfig {
        network: Some(NetworkPolicy::Allowlist),
        allowed_domains: Some(allowed.iter().map(|s| s.to_string()).collect()),
        denied_domains: denied.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

#[test]
fn exact_wildcard_deny_and_ip() {
    let p = Policy {
        allowed: vec!["github.com".into(), "*.npmjs.org".into()],
        denied: vec!["evil.npmjs.org".into()],
        blocked: Mutex::default(),
    };
    assert!(p.allows("github.com"));
    assert!(p.allows("registry.npmjs.org"));
    assert!(p.allows("a.b.npmjs.org"));
    for host in [
        "npmjs.org",
        "evil.npmjs.org",
        "notnpmjs.org",
        "github.com.evil",
        "127.0.0.1",
        "::1",
    ] {
        assert!(!p.allows(host), "{host}");
    }
    assert!(domain_matches("GitHub.COM.", "github.com"));
    assert!(!domain_matches("*.0.0.1", "127.0.0.1"));
    assert_eq!(authority("[::1]:8080", 80).unwrap(), ("::1".into(), 8080));
    assert!(authority("user@github.com", 80).is_err());
}

async fn server() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    header.push(socket.read_u8().await.unwrap());
                }
                assert!(
                    !String::from_utf8_lossy(&header)
                        .to_ascii_lowercase()
                        .contains("proxy-authorization")
                );
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nfixture",
                    )
                    .await
                    .unwrap();
            });
        }
    });
    port
}

async fn request(lease: &Lease, request: &str) -> String {
    let mut client = TcpStream::connect(("127.0.0.1", lease.port)).await.unwrap();
    client
        .write_all(
            format!(
                "{request}\r\nProxy-Authorization: Basic {}\r\n\r\n",
                lease.key
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        client.read_to_string(&mut response),
    )
    .await
    .unwrap()
    .unwrap();
    response
}

/// A raw response's status line and body. Tests compare these with
/// `assert_eq!`, whose failure report escapes them (`{:?}`): a response
/// is network input, and printed raw it could forge lines in the log.
pub(super) fn status_and_body(response: &str) -> (&str, &str) {
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((response, ""));
    (head.lines().next().unwrap_or_default(), body)
}

#[tokio::test]
async fn plain_http_allows_exact_host_and_ip_when_listed() {
    let port = server().await;
    let lease = Lease::new(
        &cfg(&["127.0.0.1", "localhost"], &[]),
        Path::new("/proxy-http-test"),
    )
    .unwrap();
    for host in ["127.0.0.1", "localhost"] {
        let out = request(
            &lease,
            &format!("GET http://{host}:{port}/path?q=1 HTTP/1.1\r\nHost: {host}:{port}"),
        )
        .await;
        assert_eq!(status_and_body(&out), ("HTTP/1.1 200 OK", "fixture"));
    }
    assert!(lease.take_blocked().is_empty());
}

#[tokio::test]
async fn http_and_connect_denials_are_reported_even_without_client_output() {
    let port = server().await;
    for config in [cfg(&[], &[]), cfg(&["127.0.0.1"], &["127.0.0.1"])] {
        let lease = Lease::new(&config, Path::new("/proxy-deny-test")).unwrap();
        for req in [
            format!("GET http://127.0.0.1:{port}/ HTTP/1.1\r\nHost: 127.0.0.1:{port}"),
            format!("CONNECT 127.0.0.1:{port} HTTP/1.1"),
        ] {
            let out = request(&lease, &req).await;
            let (status, body) = status_and_body(&out);
            assert_eq!(status, "HTTP/1.1 403 Forbidden");
            assert!(
                body.contains("network access to 127.0.0.1 was blocked by the sandbox allowlist")
            );
            assert!(body.contains("allowed_domains"));
            assert_eq!(lease.take_blocked(), ["127.0.0.1"]);
        }
    }
}

#[tokio::test]
async fn connect_tunnels_allowed_host() {
    let port = server().await;
    let lease = Lease::new(&cfg(&["127.0.0.1"], &[]), Path::new("/proxy-connect-test")).unwrap();
    let mut client = TcpStream::connect(("127.0.0.1", lease.port)).await.unwrap();
    client
        .write_all(
            format!(
                "CONNECT 127.0.0.1:{port} HTTP/1.1\r\nProxy-Authorization: Basic {}\r\n\r\n",
                lease.key
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        header.push(client.read_u8().await.unwrap());
    }
    assert!(header.starts_with(b"HTTP/1.1 200"));
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: fixture\r\n\r\n")
        .await
        .unwrap();
    let mut out = String::new();
    client.read_to_string(&mut out).await.unwrap();
    assert!(out.ends_with("fixture"));
}

#[tokio::test]
async fn policies_are_isolated_and_host_mismatch_cannot_bypass() {
    let port = server().await;
    let a = Lease::new(&cfg(&["127.0.0.1"], &[]), Path::new("/proxy-isolation")).unwrap();
    let b = Lease::new(&cfg(&[], &[]), Path::new("/proxy-isolation")).unwrap();
    assert!(
        request(&b, &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}"))
            .await
            .contains("403 Forbidden")
    );
    assert!(a.take_blocked().is_empty());
    assert_eq!(b.take_blocked(), ["127.0.0.1"]);
    let out = request(
        &a,
        &format!("GET http://evil.invalid:{port}/ HTTP/1.1\r\nHost: 127.0.0.1:{port}"),
    )
    .await;
    assert!(!out.contains("fixture"));
    let mut client = TcpStream::connect(("127.0.0.1", a.port)).await.unwrap();
    client
        .write_all(b"CONNECT github.com:443 HTTP/1.1\r\n\r\n")
        .await
        .unwrap();
    let mut out = String::new();
    client.read_to_string(&mut out).await.unwrap();
    assert!(out.contains("407 Proxy Authentication Required"));
}

#[test]
fn grants_are_once_or_persisted_preserving_config() {
    let root = std::env::temp_dir().join(format!("chanto-net-grant-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join(".chanto")).unwrap();
    std::fs::write(
        root.join(".chanto/config.toml"),
        "# keep me\nmodel = 'test'\n[sandbox]\nnetwork = 'allowlist'\nallowed_domains = []\n",
    )
    .unwrap();
    let config = cfg(&[], &[]);
    grant(&root, "once.invalid", false, &config).unwrap();
    let first = Lease::new(&config, &root).unwrap();
    assert!(first.policy.allows("once.invalid"));
    assert!(
        !Lease::new(&config, &root)
            .unwrap()
            .policy
            .allows("once.invalid")
    );
    grant(&root, "saved.invalid", true, &config).unwrap();
    grant(&root, "saved.invalid", true, &config).unwrap();
    let text = std::fs::read_to_string(root.join(".chanto/config.toml")).unwrap();
    assert!(text.contains("# keep me"));
    let saved: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(
        saved["sandbox"]["allowed_domains"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        Lease::new(&config, &root)
            .unwrap()
            .policy
            .allows("saved.invalid")
    );
    std::fs::remove_dir_all(root).unwrap();
}
