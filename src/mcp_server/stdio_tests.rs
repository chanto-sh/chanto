//! `chanto mcp-serve` over real pipes: a child process runs the server, this
//! test is the MCP client, and a scripted model answers its turns.

use super::*;
use crate::agent::tests::{mock_server, tmpdir};
use std::process::Stdio;
use tokio::io::AsyncBufReadExt;

const CHILD: &str = "CHANTO_MCP_SERVE_CHILD";

#[tokio::test]
async fn serve_child() {
    let Ok(model) = std::env::var(CHILD) else {
        return;
    };
    let cli = <Cli as clap::Parser>::parse_from([
        "chanto",
        "--provider",
        "openai",
        "--base-url",
        &model,
        "--model",
        "mock",
        "mcp-serve",
    ]);
    run(cli).await.unwrap();
}

struct Client {
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    child: tokio::process::Child,
}

impl Client {
    async fn send(&mut self, raw: &str) {
        self.stdin.write_all(raw.as_bytes()).await.unwrap();
        self.stdin.write_all(b"\n").await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    /// The next message with this id; the test harness's own lines and any
    /// notifications are skipped.
    async fn answer(&mut self, id: Value) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                let line = self
                    .lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("server exited");
                // The harness prints the test's name in front of the first line.
                let json = line.find('{').map_or("", |at| &line[at..]);
                if let Ok(v) = serde_json::from_str::<Value>(json)
                    && v["id"] == id
                    && v["method"].is_null()
                {
                    return v;
                }
            }
        })
        .await
        .expect("no answer")
    }

    async fn call(&mut self, id: i64, method: &str, params: Value) -> Value {
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send(&msg.to_string()).await;
        self.answer(json!(id)).await
    }

    async fn tool(&mut self, id: i64, name: &str, args: Value) -> Value {
        self.call(id, "tools/call", json!({"name": name, "arguments": args}))
            .await["result"]
            .clone()
    }
}

async fn serve(model: &str) -> Client {
    let (home, cwd) = (tmpdir("mcp-home"), tmpdir("mcp-cwd"));
    std::fs::write(
        home.join("config.toml"),
        "[compat]\nclaude = false\ncodex = false\n",
    )
    .unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "mcp_server::stdio_tests::serve_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .current_dir(cwd)
        .env(CHILD, model)
        .env("CHANTO_HOME", &home)
        .env("CHANTO_COMPAT_HOME", &home)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        // No update check or price download: nothing leaves the machine.
        .env("CHANTO_NO_UPDATE_CHECK", "1")
        .env("CI", "1")
        .env("CODEX_HOME", home.join(".codex"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    Client {
        stdin: child.stdin.take().unwrap(),
        lines: BufReader::new(child.stdout.take().unwrap()).lines(),
        child,
    }
}

#[tokio::test]
async fn a_client_runs_and_continues_a_session() {
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    let (model, seen) = mock_server(vec![
        json!({"content": "first answer"}),
        json!({"content": "second answer"}),
    ])
    .await;
    let mut c = serve(&model).await;

    let init = c
        .call(1, "initialize", json!({"protocolVersion": "2025-03-26"}))
        .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26", "{init}");
    assert_eq!(init["result"]["serverInfo"]["name"], "chanto");
    let init = c
        .call(2, "initialize", json!({"protocolVersion": "1999-01-01"}))
        .await;
    assert_eq!(
        init["result"]["protocolVersion"], PROTOCOLS[0],
        "an unknown version gets ours"
    );

    let tools = c.call(3, "tools/list", json!({})).await;
    let names: Vec<_> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["chanto", "chanto_reply"]);

    // Bad input gets an answer and leaves the server running.
    c.send("this is not json").await;
    let unknown = c.call(4, "no/such/method", json!({})).await;
    assert!(unknown["error"]["code"].is_i64(), "{unknown}");
    // A notification gets no reply, so the next answer is for the next request.
    c.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
        .await;
    c.send(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":99}}"#)
        .await;

    for (id, name, args, needle) in [
        (5, "chanto", json!({}), "`prompt` is required"),
        (
            6,
            "chanto",
            json!({"prompt": "   "}),
            "`prompt` is required",
        ),
        (7, "nope", json!({"prompt": "x"}), "unknown tool nope"),
        (
            8,
            "chanto_reply",
            json!({"prompt": "x", "sessionId": "gone"}),
            "no session gone",
        ),
    ] {
        let r = c.tool(id, name, args).await;
        assert_eq!(r["isError"], true, "{r}");
        assert_eq!(r["content"][0]["text"], needle);
    }
    assert!(
        seen.lock().unwrap().is_empty(),
        "none of those reached the model"
    );

    let r = c
        .tool(
            9,
            "chanto",
            json!({"prompt": "do the thing", "verify": false, "mode": "plan"}),
        )
        .await;
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(r["structuredContent"]["result"], "first answer");
    let session = r["structuredContent"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let r = c
        .tool(
            10,
            "chanto_reply",
            json!({"prompt": "and more", "sessionId": session}),
        )
        .await;
    assert_eq!(r["content"][0]["text"], "second answer", "{r}");
    let second = seen.lock().unwrap()[1]["messages"].to_string();
    assert!(
        second.contains("do the thing") && second.contains("first answer"),
        "{second}"
    );

    // Closing stdin ends the server cleanly.
    drop(c.stdin);
    let status = tokio::time::timeout(std::time::Duration::from_secs(30), c.child.wait())
        .await
        .expect("the server stops when its client goes")
        .unwrap();
    assert!(status.success(), "{status:?}");
}
