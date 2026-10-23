//! Test helper: an HTTP server that replays scripted SSE bodies and records
//! the JSON request bodies and headers it received.

use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct Seen {
    pub bodies: Vec<Value>,
    /// Request line and headers of each request.
    pub headers: Vec<String>,
    /// Raw request bodies.
    pub raw: Vec<Vec<u8>>,
}

/// Each script entry is `(status, body)`; bodies are sent as `text/event-stream`.
pub async fn serve(script: Vec<(u16, String)>) -> (String, Arc<Mutex<Seen>>) {
    serve_bytes(
        script
            .into_iter()
            .map(|(s, b)| (s, "text/event-stream", b.into_bytes()))
            .collect(),
    )
    .await
}

/// Each script entry is `(status, content type, body)`.
pub async fn serve_bytes(script: Vec<(u16, &'static str, Vec<u8>)>) -> (String, Arc<Mutex<Seen>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let queue = Arc::new(Mutex::new(VecDeque::from(script)));
    let seen = Arc::new(Mutex::new(Seen {
        bodies: vec![],
        headers: vec![],
        raw: vec![],
    }));
    let seen2 = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let queue = queue.clone();
            let seen = seen2.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 16384];
                loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let s = String::from_utf8_lossy(&buf).to_string();
                    if let Some(h) = s.find("\r\n\r\n") {
                        let cl = s[..h]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buf.len() >= h + 4 + cl {
                            let mut sn = seen.lock().unwrap();
                            sn.headers.push(s[..h].to_string());
                            sn.bodies.push(
                                serde_json::from_slice(&buf[h + 4..h + 4 + cl]).unwrap_or_default(),
                            );
                            sn.raw.push(buf[h + 4..h + 4 + cl].to_vec());
                            break;
                        }
                    }
                }
                let (status, ct, body) = queue.lock().unwrap().pop_front().unwrap_or((
                    500,
                    "text/plain",
                    b"script exhausted".to_vec(),
                ));
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: {ct}\r\nretry-after: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                // Two writes, so clients see a frame split across reads.
                let mid = body.len() / 2;
                let _ = sock.write_all(&body[..mid]).await;
                let _ = sock.flush().await;
                let _ = sock.write_all(&body[mid..]).await;
            });
        }
    });
    (format!("http://{addr}"), seen)
}

/// Format SSE events from (event, data) pairs.
pub fn sse(events: &[(&str, Value)]) -> String {
    events
        .iter()
        .map(|(e, d)| format!("event: {e}\ndata: {d}\n\n"))
        .collect()
}
