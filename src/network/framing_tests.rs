use super::*;
use crate::config::NetworkPolicy;

#[tokio::test]
async fn plain_http_does_not_forward_a_pipelined_denied_host() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut upstream, _) = listener.accept().await.unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            header.push(upstream.read_u8().await.unwrap());
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), upstream.read_u8())
                .await
                .is_err(),
            "a second request must never reach the upstream"
        );
        upstream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .unwrap();
    });
    let config = SandboxConfig {
        network: Some(NetworkPolicy::Allowlist),
        allowed_domains: Some(vec!["127.0.0.1".into()]),
        ..Default::default()
    };
    let lease = Lease::new(&config, Path::new("/proxy-framing")).unwrap();
    let mut client = TcpStream::connect(("127.0.0.1", lease.port)).await.unwrap();
    client.write_all(format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nProxy-Authorization: Basic {}\r\n\r\nGET / HTTP/1.1\r\nHost: denied.invalid\r\n\r\n", lease.key).as_bytes()).await.unwrap();
    let mut out = String::new();
    let read = client.read_to_string(&mut out).await;
    match read {
        Ok(_) => assert_eq!(
            super::tests::status_and_body(&out),
            ("HTTP/1.1 200 OK", "ok")
        ),
        // Closing with unread pipelined bytes can reset the client socket.
        Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::ConnectionReset),
    }
    server.await.unwrap();
}

#[tokio::test]
async fn plain_http_streams_content_length_and_chunked_bodies() {
    for (framing, encoded, expected) in [
        ("Content-Length: 4", "data", "data"),
        (
            "Transfer-Encoding: chunked",
            "4\r\ndata\r\n0\r\n\r\n",
            "4\r\ndata\r\n0\r\n\r\n",
        ),
    ] {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut upstream, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(upstream.read_u8().await.unwrap());
            }
            let mut body = vec![0; expected.len()];
            upstream.read_exact(&mut body).await.unwrap();
            assert_eq!(body, expected.as_bytes());
            upstream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
        });
        let config = SandboxConfig {
            network: Some(NetworkPolicy::Allowlist),
            allowed_domains: Some(vec!["127.0.0.1".into()]),
            ..Default::default()
        };
        let lease = Lease::new(&config, Path::new("/proxy-bodies")).unwrap();
        let mut client = TcpStream::connect(("127.0.0.1", lease.port)).await.unwrap();
        client.write_all(format!("POST / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{framing}\r\nProxy-Authorization: Basic {}\r\n\r\n{encoded}", lease.key).as_bytes()).await.unwrap();
        let mut out = String::new();
        client.read_to_string(&mut out).await.unwrap();
        assert!(out.ends_with("ok"));
        server.await.unwrap();
    }
}
