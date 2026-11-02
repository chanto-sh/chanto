//! The prompt blocks and session requests an editor can send, in a child
//! process since sessions set process-wide state.

use super::*;
use crate::agent::tests::{mock_server, tmpdir};
use crate::test_support::isolated;
use base64::Engine;
use clap::Parser;

fn server(base: &str) -> Arc<Server> {
    let (out, rx) = mpsc::unbounded_channel();
    std::mem::forget(rx);
    let peer = Peer {
        out,
        pending: Default::default(),
        serial: Arc::new(AtomicU64::new(1)),
    };
    let cli = crate::Cli::parse_from([
        "chanto",
        "--provider",
        "openai",
        "--base-url",
        base,
        "--model",
        "mock",
        "acp",
    ]);
    Arc::new(Server {
        cli,
        peer,
        caps: Mutex::new(json!({})),
        sessions: Default::default(),
        startup: AsyncMutex::new(()),
    })
}

async fn new_session(s: &Arc<Server>, cwd: &std::path::Path) -> String {
    let r = s
        .handle("session/new", &json!({"cwd": cwd, "mcpServers": []}))
        .await
        .unwrap();
    r["sessionId"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn prompt_blocks_reach_the_model_and_bad_ones_are_refused() {
    if isolated("acp::prompt_tests::prompt_blocks_reach_the_model_and_bad_ones_are_refused") {
        return;
    }
    let (model, seen) = mock_server(vec![json!({"content": "ok"})]).await;
    let s = server(&model);
    let dir = crate::util::canonical(tmpdir("acp-prompt")).unwrap();
    let id = new_session(&s, &dir).await;
    let b64 = |t: &str| base64::engine::general_purpose::STANDARD.encode(t);
    let prompt = |blocks: Value| json!({"sessionId": id, "prompt": blocks});

    for (blocks, needle) in [
        (json!("nope"), "prompt must be an array"),
        (json!([{"type": "text"}]), "text required"),
        (json!([{"type": "image"}]), "image data required"),
        (json!([{"type": "resource_link"}]), "uri required"),
        (
            json!([{"type": "resource", "resource": {"uri": "u"}}]),
            "text or blob required",
        ),
        (
            json!([{"type": "resource", "resource": {"uri": "u", "blob": "!!"}}]),
            "",
        ),
        (
            json!([{"type": "resource", "resource": {"uri": "u", "blob": base64::engine::general_purpose::STANDARD.encode([0xFFu8, 0xFE])}}]),
            "must contain text or an image",
        ),
        (json!([{"type": "audio"}]), "unsupported prompt content"),
    ] {
        let e = s
            .handle("session/prompt", &prompt(blocks.clone()))
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains(needle), "{blocks}: {e:#}");
    }
    assert!(
        seen.lock().unwrap().is_empty(),
        "a bad prompt never reaches the model"
    );

    let r = s
        .handle(
            "session/prompt",
            &prompt(json!([
                {"type": "text", "text": "TEXT-BLOCK"},
                {"type": "resource_link", "uri": "https://x.test/doc"},
                {"type": "resource", "resource": {"uri": "inline.txt", "text": "INLINE-TEXT"}},
                {"type": "resource", "resource": {"uri": "blob.txt", "blob": b64("BLOB-TEXT")}},
            ])),
        )
        .await
        .unwrap();
    assert_eq!(r["stopReason"], "end_turn", "{r}");
    let sent = seen.lock().unwrap()[0]["messages"].to_string();
    for part in [
        "TEXT-BLOCK",
        "Resource: https://x.test/doc",
        "INLINE-TEXT",
        "BLOB-TEXT",
    ] {
        assert!(sent.contains(part), "{part}: {sent}");
    }

    for (method, params, needle) in [
        (
            "session/prompt",
            json!({"sessionId": "ghost", "prompt": []}),
            "unknown session",
        ),
        (
            "session/set_model",
            json!({"sessionId": id}),
            "modelId required",
        ),
        (
            "session/new",
            json!({"cwd": dir, "mcpServers": [{}]}),
            "MCP name required",
        ),
        (
            "session/new",
            json!({"cwd": dir, "mcpServers": [{"name": "a", "env": [{}]}]}),
            "env name required",
        ),
        (
            "session/new",
            json!({"cwd": dir, "mcpServers": [{"name": "a", "env": [{"name": "X"}]}]}),
            "env value required",
        ),
        (
            "session/new",
            json!({"cwd": dir, "mcpServers": [{"name": "a", "headers": [{}]}]}),
            "header name required",
        ),
        (
            "session/new",
            json!({"cwd": dir, "mcpServers": [{"name": "a", "headers": [{"name": "X"}]}]}),
            "header value required",
        ),
        (
            "session/load",
            json!({"cwd": dir, "mcpServers": []}),
            "sessionId required",
        ),
        (
            "session/load",
            json!({"cwd": dir, "sessionId": "ghost", "mcpServers": []}),
            "",
        ),
    ] {
        let e = s.handle(method, &params).await.unwrap_err();
        assert!(
            format!("{e:#}").contains(needle),
            "{method} {params}: {e:#}"
        );
    }
}
