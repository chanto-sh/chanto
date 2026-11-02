use super::*;
use clap::Parser;

#[tokio::test]
async fn handshake_and_errors() {
    let (out, mut rx) = mpsc::unbounded_channel();
    let s = Server {
        cli: Arc::new(Cli::parse_from(["chanto"])),
        out,
        sessions: Default::default(),
        running: Default::default(),
    };
    s.handle(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05"}})).await;
    assert_eq!(
        rx.recv().await.unwrap()["result"]["protocolVersion"],
        "2024-11-05"
    );
    s.handle(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
        .await;
    let names: Vec<_> = rx.recv().await.unwrap()["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].clone())
        .collect();
    assert_eq!(names, vec![json!("chanto"), json!("chanto_reply")]);
    s.handle(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "chanto_reply", "arguments": {"sessionId": "nope", "prompt": "hi"}}})).await;
    let r = rx.recv().await.unwrap();
    assert_eq!(r["result"]["isError"], true);
    s.handle(json!({"jsonrpc": "2.0", "id": 4, "method": "bogus"}))
        .await;
    assert_eq!(rx.recv().await.unwrap()["error"]["code"], -32601);
    s.handle(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    assert!(rx.try_recv().is_err(), "notifications get no reply");
}
