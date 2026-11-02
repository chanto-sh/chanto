//! The local OAuth callback: any page in the user's browser can reach it, so
//! it must only accept the right path and state.

use super::*;
use tokio::io::AsyncReadExt;

async fn get(port: u16, path: &str) -> String {
    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    sock.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut out = String::new();
    let _ = sock.read_to_string(&mut out).await;
    out
}

async fn waiting(
    state: &'static str,
) -> (
    u16,
    tokio::task::JoinHandle<Result<std::collections::HashMap<String, String>>>,
) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(wait_for_params(listener, state, "/cb"));
    (port, task)
}

#[tokio::test]
async fn only_the_right_path_and_state_complete_the_login() {
    let (port, task) = waiting("good").await;
    assert!(
        get(port, "/other?code=1&state=good")
            .await
            .starts_with("HTTP/1.1 404")
    );
    let r = get(port, "/cb?code=stolen&state=bad").await;
    assert!(r.starts_with("HTTP/1.1 400") && r.contains("State mismatch"));
    let r = get(port, "/cb?code=stolen").await;
    assert!(r.contains("State mismatch"));
    let r = get(port, "/cb?state=good").await;
    assert!(r.contains("Missing code"));
    assert!(!task.is_finished(), "none of those ended the wait");
    let r = get(port, "/cb?code=real&state=good").await;
    assert!(r.starts_with("HTTP/1.1 200"));
    let params = task.await.unwrap().unwrap();
    assert_eq!(params["code"], "real");
}

#[tokio::test]
async fn an_error_needs_the_state_too_and_is_escaped_in_the_page() {
    let (port, task) = waiting("good").await;
    // Any local page could send this: without the state it changes nothing.
    assert!(
        get(port, "/cb?error=denied")
            .await
            .contains("State mismatch")
    );
    let r = get(
        port,
        "/cb?state=good&error=denied&error_description=%3Cscript%3Ex%26y",
    )
    .await;
    assert!(r.contains("&lt;script&gt;x&amp;y"));
    assert!(!r.contains("<script>"));
    let e = task.await.unwrap().unwrap_err();
    assert_eq!(e.to_string(), "authorization failed: denied: <script>x&y");
}

#[test]
fn escaping_covers_markup() {
    assert_eq!(html_escape("<a>&"), "&lt;a&gt;&amp;");
}
