//! `chanto mcp-serve`: Chanto as an MCP server over stdio, so other agents
//! and editors (Claude Code, Codex, Cursor, ...) can hand it coding tasks.
//!
//! Two tools, like `codex mcp-server`: `chanto` starts a session and runs a
//! task; `chanto_reply` continues a session. Verified mode is on by default:
//! the result is only "done" once the project's checks pass.

use crate::Cli;
use crate::agent::{Agent, Event, Frontend, PermissionReply, PermissionRequest, TurnEnd};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio_util::sync::CancellationToken;

const PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// Forwards tool activity as MCP progress notifications; permission prompts
/// can't be answered over MCP, so they're denied with an explanation.
struct McpFrontend {
    out: mpsc::UnboundedSender<Value>,
    /// Progress token of the call currently running in this session.
    progress: Mutex<Option<(Value, u64)>>,
}

#[async_trait]
impl Frontend for McpFrontend {
    fn emit(&self, ev: Event) {
        let msg = match ev {
            Event::ToolStart {
                label,
                parent: None,
                ..
            } => label,
            Event::Warn(w) | Event::Info(w) => w,
            _ => return,
        };
        let mut p = self.progress.lock().unwrap();
        if let Some((token, n)) = p.as_mut() {
            *n += 1;
            let _ = self.out.send(json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": token, "progress": *n, "message": msg}}));
        }
    }
    async fn permission(&self, req: PermissionRequest) -> PermissionReply {
        PermissionReply::Deny(Some(format!(
            "{} needs approval, which isn't available when Chanto runs as an MCP server. \
             Try another approach, or tell the caller it needs mode \"bypassPermissions\" (or an allow rule).",
            req.tool
        )))
    }
    async fn question(&self, _: &str, _: &[String]) -> Option<String> {
        None
    }
}

struct Session {
    agent: AsyncMutex<Agent>,
    frontend: Arc<McpFrontend>,
}

#[derive(Clone)]
struct Server {
    cli: Arc<Cli>,
    out: mpsc::UnboundedSender<Value>,
    sessions: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    running: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

fn tools() -> Value {
    json!([
        {
            "name": "chanto",
            "description": "Run a coding task with the Chanto agent in this project. In verified mode (the default) it can't finish until the project's tests and checks pass, and it can't weaken existing tests. Returns the final answer and a sessionId for chanto_reply.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "The task."},
                    "model": {"type": "string", "description": "Model to use (provider/model), if not the configured default."},
                    "mode": {"type": "string", "enum": ["default", "acceptEdits", "plan", "auto", "bypassPermissions"], "description": "Permission mode. Approvals can't be asked for over MCP, so actions needing one are refused unless the mode allows them. Default: acceptEdits."},
                    "verify": {"type": "boolean", "description": "Verified mode: run the project's checks before finishing (default true)."}
                },
                "required": ["prompt"]
            }
        },
        {
            "name": "chanto_reply",
            "description": "Continue a Chanto session with a follow-up message.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": {"type": "string"},
                    "prompt": {"type": "string"}
                },
                "required": ["sessionId", "prompt"]
            }
        }
    ])
}

fn tool_error(msg: impl Into<String>) -> Value {
    json!({"content": [{"type": "text", "text": msg.into()}], "isError": true})
}

impl Server {
    async fn call(&self, params: &Value, cancel: CancellationToken) -> Value {
        let args = &params["arguments"];
        let Some(prompt) = args["prompt"].as_str().filter(|p| !p.trim().is_empty()) else {
            return tool_error("`prompt` is required");
        };
        let session = match params["name"].as_str() {
            Some("chanto") => match self.start(args).await {
                Ok(s) => s,
                Err(e) => return tool_error(format!("{e:#}")),
            },
            Some("chanto_reply") => {
                let id = args["sessionId"].as_str().unwrap_or("");
                match self.sessions.lock().unwrap().get(id).cloned() {
                    Some(s) => (id.to_string(), s),
                    None => return tool_error(format!("no session {id}")),
                }
            }
            other => return tool_error(format!("unknown tool {}", other.unwrap_or(""))),
        };
        let (id, s) = session;
        let Ok(mut agent) = s.agent.try_lock() else {
            return tool_error(format!("session {id} is busy with another call"));
        };
        *s.frontend.progress.lock().unwrap() =
            params["_meta"].get("progressToken").map(|t| (t.clone(), 0));
        let end = agent.run_turn(prompt, vec![], cancel).await;
        agent.save_session();
        *s.frontend.progress.lock().unwrap() = None;
        match end {
            Ok(TurnEnd::Done(text)) => json!({
                "content": [{"type": "text", "text": text}],
                "structuredContent": {"sessionId": id, "result": text},
                "isError": false,
            }),
            Ok(TurnEnd::Interrupted) => tool_error("Cancelled."),
            Err(e) => tool_error(format!("{e:#}")),
        }
    }

    async fn start(&self, args: &Value) -> Result<(String, Arc<Session>)> {
        let mut cli = (*self.cli).clone();
        if let Some(m) = args["model"].as_str() {
            cli.model = Some(m.into());
        }
        cli.mode = Some(
            args["mode"]
                .as_str()
                .map(str::to_string)
                .or(cli.mode)
                .unwrap_or_else(|| "acceptEdits".into()),
        );
        cli.verify = args["verify"].as_bool().unwrap_or(true);
        let frontend = Arc::new(McpFrontend {
            out: self.out.clone(),
            progress: Mutex::new(None),
        });
        let setup = crate::setup(&cli, frontend.clone(), false).await?;
        let id = setup
            .agent
            .session
            .as_ref()
            .map(|s| s.id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        let s = Arc::new(Session {
            agent: AsyncMutex::new(setup.agent),
            frontend,
        });
        self.sessions.lock().unwrap().insert(id.clone(), s.clone());
        Ok((id, s))
    }

    async fn handle(&self, msg: Value) {
        let id = msg.get("id").cloned();
        let method = msg["method"].as_str().unwrap_or("");
        let params = &msg["params"];
        let result = match method {
            "initialize" => {
                let asked = params["protocolVersion"].as_str().unwrap_or("");
                let version = if PROTOCOLS.contains(&asked) {
                    asked
                } else {
                    PROTOCOLS[0]
                };
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "chanto", "version": env!("CARGO_PKG_VERSION")},
                })
            }
            "ping" => json!({}),
            "tools/list" => json!({"tools": tools()}),
            "tools/call" => {
                let key = id.clone().unwrap_or_default().to_string();
                let cancel = CancellationToken::new();
                self.running
                    .lock()
                    .unwrap()
                    .insert(key.clone(), cancel.clone());
                let r = self.call(params, cancel).await;
                self.running.lock().unwrap().remove(&key);
                r
            }
            "notifications/cancelled" => {
                if let Some(t) = self
                    .running
                    .lock()
                    .unwrap()
                    .get(&params["requestId"].to_string())
                {
                    t.cancel();
                }
                return;
            }
            _ if id.is_none() => return, // other notifications
            _ => {
                let _ = self.out.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method not found: {method}")}}));
                return;
            }
        };
        if id.is_some() {
            let _ = self
                .out
                .send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
        }
    }
}

/// Serve MCP over stdin/stdout until stdin closes.
pub async fn run(cli: Cli) -> Result<()> {
    let (out, mut rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        // Null is the stop signal: agent internals may keep senders alive.
        while let Some(v) = rx.recv().await.filter(|v| !v.is_null()) {
            let line = format!("{v}\n");
            if stdout.write_all(line.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });
    let server = Server {
        cli: Arc::new(cli),
        out: out.clone(),
        sessions: Default::default(),
        running: Default::default(),
    };
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut tasks = tokio::task::JoinSet::new();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = out.send(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}}));
                continue;
            }
        };
        // Calls run concurrently, so a long task doesn't block pings or cancellation.
        let s = server.clone();
        tasks.spawn(async move { s.handle(msg).await });
    }
    // Input closed: stop running calls, then flush what's left.
    for t in server.running.lock().unwrap().values() {
        t.cancel();
    }
    while tasks.join_next().await.is_some() {}
    let envs: Vec<_> = server
        .sessions
        .lock()
        .unwrap()
        .values()
        .filter_map(|s| s.agent.try_lock().ok().map(|a| a.env.clone()))
        .collect();
    for env in envs {
        env.session_end("other").await;
    }
    let _ = out.send(Value::Null);
    let _ = writer.await;
    Ok(())
}

#[cfg(test)]
mod tests {
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
}
