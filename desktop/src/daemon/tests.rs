use super::*;

#[test]
fn server_file_is_parsed() {
    let s =
        Server::parse(br#"{"url": "http://127.0.0.1:4141/", "token": "abc", "pid": 7}"#).unwrap();
    assert_eq!(
        s,
        Server {
            url: "http://127.0.0.1:4141".into(),
            token: "abc".into()
        }
    );
    assert!(Server::parse(br#"{"url": "http://127.0.0.1:1", "token": ""}"#).is_none());
    assert!(Server::parse(br#"{"url": "https://x", "token": "t"}"#).is_none());
    assert!(Server::parse(b"not json").is_none());
}

#[test]
fn server_file_records_its_folder() {
    let json = br#"{"url": "http://127.0.0.1:4141", "token": "abc", "cwd": "/Users/me/api"}"#;
    assert_eq!(parse_cwd(json), Some("/Users/me/api".into()));
    // An older server.json has no cwd; an empty one means the same.
    assert_eq!(
        parse_cwd(br#"{"url": "http://127.0.0.1:4141", "token": "abc"}"#),
        None
    );
    assert_eq!(
        parse_cwd(br#"{"url": "http://127.0.0.1:4141", "cwd": ""}"#),
        None
    );
    assert_eq!(parse_cwd(b"not json"), None);
}

#[test]
fn a_server_in_the_chanto_folder_is_detected() {
    let dir = std::env::temp_dir().join(format!("chanto-daemon-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    assert!(!runs_in_home(&dir));
    let write = |cwd: &Path| {
        let json = format!(
            r#"{{"url": "http://127.0.0.1:1", "token": "t", "cwd": {:?}}}"#,
            cwd.to_str().unwrap()
        );
        std::fs::write(dir.join("server.json"), json).unwrap();
    };
    write(&dir);
    assert!(runs_in_home(&dir));
    // A daemon a person started in their project is not in Chanto's folder.
    write(&std::env::temp_dir());
    assert!(!runs_in_home(&dir));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn responses_are_parsed() {
    assert_eq!(
        parse_response(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}"),
        Some((200, b"{}".to_vec()))
    );
    let chunked = b"HTTP/1.1 404 Not Found\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
    assert_eq!(parse_response(chunked), Some((404, b"abcde".to_vec())));
    assert_eq!(parse_response(b"garbage"), None);
}

#[test]
fn health_against_a_local_server() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for mut c in listener.incoming().flatten().take(2) {
            // Read the whole request head: it may arrive in several pieces.
            let mut req = Vec::new();
            let mut buf = [0u8; 1024];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match c.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            let req = String::from_utf8_lossy(&req).to_string();
            let body = if req.starts_with("GET /health ") {
                r#"{"ok":true,"runs":true}"#
            } else {
                ""
            };
            let status = if body.is_empty() {
                "404 Not Found"
            } else {
                "200 OK"
            };
            let _ = write!(
                c,
                "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    let s = Server {
        url,
        token: "t".into(),
    };
    assert_eq!(s.health().unwrap()["runs"], true);
    assert!(!s.serves_web());
    // Nobody listening.
    assert!(
        Server {
            url: "http://127.0.0.1:1".into(),
            token: "t".into()
        }
        .health()
        .is_none()
    );
}

#[test]
fn chanto_is_found_next_to_the_app_first() {
    let dir = std::env::temp_dir().join(format!("chanto-desktop-bin-{}", std::process::id()));
    let (app, other) = (dir.join("app"), dir.join("bin"));
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    for d in [&app, &other] {
        std::fs::write(d.join(exe_name("chanto")), "").unwrap();
    }
    let path = std::env::join_paths([&other])
        .unwrap()
        .into_string()
        .unwrap();
    let exe = Some(app.join("chanto-desktop"));
    assert_eq!(
        chanto_bin(None, exe.clone(), &path),
        Some(app.join(exe_name("chanto")))
    );
    assert_eq!(
        chanto_bin(None, Some(dir.join("x/chanto-desktop")), &path),
        Some(other.join(exe_name("chanto")))
    );
    let explicit = other.join(exe_name("chanto"));
    assert_eq!(
        chanto_bin(Some(explicit.clone()), exe, &path),
        Some(explicit)
    );
    assert_eq!(chanto_bin(None, None, ""), None);
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn daemon_path_keeps_order_and_adds_install_prefixes() {
    let p = daemon_path("/usr/bin:/bin", Some("/Users/me/bin:/usr/bin".into()));
    let dirs: Vec<&str> = p.split(':').collect();
    assert_eq!(&dirs[..3], ["/usr/bin", "/bin", "/Users/me/bin"]);
    assert!(dirs.contains(&"/opt/homebrew/bin"));
    assert_eq!(dirs.iter().filter(|d| **d == "/usr/bin").count(), 1);
}
