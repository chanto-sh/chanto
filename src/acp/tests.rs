use super::*;
use crate::agent::tests::{mock_server, tc};
use clap::Parser;

fn server(base: &str) -> (Arc<Server>, mpsc::UnboundedReceiver<Value>) {
    let (out, rx) = mpsc::unbounded_channel();
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
    (
        Arc::new(Server {
            cli,
            peer,
            caps: Mutex::new(json!({})),
            sessions: Default::default(),
            startup: AsyncMutex::new(()),
        }),
        rx,
    )
}
// Separate process isolates Chanto config and process-global permission state.
fn isolated(name: &str) -> bool {
    if std::env::var_os("CHANTO_ACP_TEST_CHILD").is_some() {
        return false;
    }
    let home = std::env::temp_dir().join(format!("chanto-acp-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        "[compat]\nclaude = false\ncodex = false\n",
    )
    .unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &format!("acp::tests::{name}"), "--nocapture"])
        .env("CHANTO_HOME", &home)
        .env("CHANTO_ACP_TEST_CHILD", "1")
        .status()
        .unwrap();
    let _ = std::fs::remove_dir_all(home);
    assert!(status.success());
    true
}
async fn next(rx: &mut mpsc::UnboundedReceiver<Value>) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn editor_buffers_permissions_diffs_and_load() {
    if isolated("editor_buffers_permissions_diffs_and_load") {
        return;
    }
    let dir = std::env::temp_dir().join(format!("chanto-acp-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("a.txt");
    std::fs::write(&path, "disk\n").unwrap();
    let (base, seen) = mock_server(vec![
        json!({"tool_calls":tc("Read",json!({"file_path":"a.txt"}))}),
        json!({"tool_calls":tc("Edit",json!({"file_path":"a.txt","old_string":"buffer","new_string":"edited"}))}),
        json!({"reasoning":"Checked the edit.","content":"Done."}),
    ]).await;
    let (server, mut rx) = server(&base);
    let init = server.handle("initialize", &json!({"protocolVersion":99,"clientCapabilities":{"fs":{"readTextFile":true,"writeTextFile":true}}})).await.unwrap();
    assert_eq!(init["protocolVersion"], 1);
    assert_eq!(init["agentCapabilities"]["loadSession"], true);
    assert!(server.handle("authenticate", &json!({})).await.is_ok());
    let session = server
        .handle("session/new", &json!({"cwd":dir,"mcpServers":[]}))
        .await
        .unwrap();
    let id = session["sessionId"].as_str().unwrap().to_string();
    let config = server
        .handle(
            "session/set_config_option",
            &json!({"sessionId":id,"configId":"model","value":"mock"}),
        )
        .await
        .unwrap();
    assert_eq!(config["configOptions"][0]["currentValue"], "mock");
    server
        .handle("session/set_mode", &json!({"sessionId":id,"modeId":"plan"}))
        .await
        .unwrap();
    server
        .handle(
            "session/set_mode",
            &json!({"sessionId":id,"modeId":"default"}),
        )
        .await
        .unwrap();
    let worker = server.clone();
    let sid = id.clone();
    let mut turn = tokio::spawn(async move {
        worker.handle("session/prompt", &json!({"sessionId":sid,"prompt":[{"type":"text","text":"Read a.txt and replace buffer with edited"}]})).await
    });
    let mut buffer = "buffer\n".to_string();
    let (mut permission, mut diff, mut read, mut thought) = (false, false, false, false);
    let mut completed_diff = false;
    let mut pending_diff = false;
    let result = loop {
        tokio::select! {
            msg = next(&mut rx) => {
                let result = match msg["method"].as_str().unwrap() {
                    "fs/read_text_file" => {read=true; Some(json!({"content":buffer}))},
                    "fs/write_text_file" => {buffer=msg["params"]["content"].as_str().unwrap().into(); Some(json!({}))},
                    "session/request_permission" => {
                        permission=true;
                        assert_eq!(msg["params"]["toolCall"]["kind"],"edit");
                        let call = &msg["params"]["toolCall"];
                        assert_eq!(call["content"][0]["type"], "diff");
                        assert_eq!(call["content"][0]["path"], json!(path));
                        assert_eq!(call["content"][0]["oldText"], "buffer\n");
                        assert_eq!(call["content"][0]["newText"], "edited\n");
                        assert_eq!(call["locations"][0]["path"], json!(path));
                        assert!(pending_diff);
                        Some(json!({"outcome":{"outcome":"selected","optionId":"allow_once"}}))
                    }
                    "session/update" => {
                        let u=&msg["params"]["update"];
                        pending_diff |= u["sessionUpdate"] == "tool_call" && u["status"] == "pending" && u["content"][0]["type"] == "diff";
                        if u["content"][0]["type"]=="diff" {diff=true; completed_diff |= u["status"] == "completed"; assert_eq!(u["content"][0]["oldText"],"buffer\n");assert_eq!(u["content"][0]["newText"],"edited\n");}
                        thought |= u["sessionUpdate"]=="agent_thought_chunk";
                        None
                    }
                    other => panic!("unexpected {other}"),
                };
                if let Some(result)=result {
                    let tx=server.peer.pending.lock().unwrap().remove(msg["id"].as_str().unwrap()).unwrap();
                    tx.send(json!({"result":result})).unwrap();
                }
            }
            result = &mut turn => break result.unwrap().unwrap(),
        }
    };
    assert_eq!(result["stopReason"], "end_turn");
    // Drain notifications emitted just before the response.
    while let Ok(msg) = rx.try_recv() {
        thought |= msg["params"]["update"]["sessionUpdate"] == "agent_thought_chunk";
        let u = &msg["params"]["update"];
        completed_diff |= u["status"] == "completed" && u["content"][0]["type"] == "diff";
    }
    assert!(read && permission && diff && thought && completed_diff);
    assert_eq!(buffer, "edited\n");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "disk\n");
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|r| r.to_string().contains("buffer"))
    );
    let live = server.sessions.lock().unwrap().get(&id).unwrap().clone();
    {
        let mut agent = live.agent.lock().await;
        assert!(agent.env.checkpoints.lock().unwrap().iter().any(|cp| {
            cp.files
                .values()
                .any(|old| old.as_deref() == Some(b"buffer\n"))
        }));
        assert_eq!(agent.rewind(0).unwrap().0, 1);
    }
    let restored = next(&mut rx).await;
    assert_eq!(restored["method"], "fs/write_text_file");
    assert_eq!(restored["params"]["content"], "buffer\n");
    let tx = server
        .peer
        .pending
        .lock()
        .unwrap()
        .remove(restored["id"].as_str().unwrap())
        .unwrap();
    tx.send(json!({"result":{}})).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "buffer\n");
    let loaded = server
        .handle(
            "session/load",
            &json!({"cwd":dir,"sessionId":id,"mcpServers":[]}),
        )
        .await
        .unwrap();
    assert_eq!(loaded["sessionId"], id);
    assert!(rx.try_recv().is_ok());
    let s = server.sessions.lock().unwrap().get(&id).unwrap().clone();
    if let Ok(a) = s.agent.try_lock() {
        let _ = std::fs::remove_file(a.session.as_ref().unwrap().path());
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn cancel_while_permission_is_pending() {
    if isolated("cancel_while_permission_is_pending") {
        return;
    }
    let dir = std::env::temp_dir().join(format!("chanto-acp-cancel-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let (base, _) = mock_server(vec![
        json!({"tool_calls":tc("Write",json!({"file_path":"new.txt","content":"never"}))}),
    ])
    .await;
    let (server, mut rx) = server(&base);
    server
        .handle("initialize", &json!({"protocolVersion":1}))
        .await
        .unwrap();
    let new = server
        .handle("session/new", &json!({"cwd":dir,"mcpServers":[]}))
        .await
        .unwrap();
    let id = new["sessionId"].as_str().unwrap().to_string();
    let worker = server.clone();
    let sid = id.clone();
    let turn = tokio::spawn(async move {
        worker
            .handle(
                "session/prompt",
                &json!({"sessionId":sid,"prompt":[{"type":"text","text":"write new.txt"}]}),
            )
            .await
    });
    loop {
        if next(&mut rx).await["method"] == "session/request_permission" {
            break;
        }
    }
    server
        .handle("session/cancel", &json!({"sessionId":id}))
        .await
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result["stopReason"], "cancelled");
    assert!(!dir.join("new.txt").exists());
    // The turn's checkpoint snapshot runs git in the project in the
    // background; Windows can't remove a directory a process is using.
    let live = server.sessions.lock().unwrap().get(&id).unwrap().clone();
    live.agent.lock().await.env.wait_snapshot().await;
    std::fs::remove_dir_all(dir).unwrap();
}
