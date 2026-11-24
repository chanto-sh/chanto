//! ACP version 1, newline-delimited JSON-RPC on stdio. Stdout is protocol only.
use crate::agent::{Agent, Event, Frontend, PermissionReply, PermissionRequest, TurnEnd};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex as AsyncMutex, mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct Peer {
    out: mpsc::UnboundedSender<Value>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    serial: Arc<AtomicU64>,
}
impl Peer {
    fn send(&self, v: Value) {
        let _ = self.out.send(v);
    }
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = format!("chanto-{}", self.serial.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        struct Pending {
            id: String,
            map: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
        }
        impl Drop for Pending {
            fn drop(&mut self) {
                self.map.lock().unwrap().remove(&self.id);
            }
        }
        let _pending = Pending {
            id,
            map: self.pending.clone(),
        };
        let result = rx.await.context("ACP client disconnected")?;
        if !result["error"].is_null() {
            bail!("ACP client: {}", result["error"]);
        }
        Ok(result["result"].clone())
    }
}
struct AcpFrontend {
    peer: Peer,
    id: Mutex<String>,
    caps: Value,
    last_tool: Mutex<Value>,
    contents: Mutex<HashMap<String, Vec<Value>>>,
    rejected: Mutex<std::collections::HashSet<String>>,
}
impl AcpFrontend {
    fn update(&self, update: Value) {
        self.peer.send(json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":*self.id.lock().unwrap(),"update":update}}));
    }
    async fn preview_text(&self, path: &Path, editor: bool) -> Option<String> {
        if !editor {
            return tokio::fs::read_to_string(path).await.ok();
        }
        match self.read_text_file(path).await {
            Some(result) => result.ok(),
            None => tokio::fs::read_to_string(path).await.ok(),
        }
    }
    async fn preview_tool_call(
        &self,
        id: &str,
        name: &str,
        args: &Value,
        cwd: &Path,
        editor: bool,
    ) {
        let kind = match name {
            "Read" | "Ls" => "read",
            "Edit" | "MultiEdit" | "Write" | "NotebookEdit" | "apply_patch" => "edit",
            "Bash" => "execute",
            "Glob" | "Grep" | "RepoMap" | "SemanticSearch" | "WebSearch" => "search",
            "WebFetch" => "fetch",
            "TodoWrite" => "think",
            _ => "other",
        };
        let locations: Vec<Value> = ["file_path", "path", "notebook_path"].iter().filter_map(|k| args[*k].as_str())
            .map(|p| json!({"path":crate::util::resolve(cwd,p),"line":args["offset"].as_u64().unwrap_or(1)})).collect();
        let mut tool = json!({"toolCallId":id,"title":name,"kind":kind,"status":"pending","rawInput":args,"locations":locations});
        if name == "Bash" {
            let command = args["command"].as_str().unwrap_or("");
            tool["title"] = json!(command);
            tool["content"] = json!([{"type":"content","content":{"type":"text","text":command}}]);
        } else if kind == "edit" {
            let mut changes = Vec::new();
            if name == "apply_patch" {
                use crate::tools::patch::Op;
                let patch = args["input"]
                    .as_str()
                    .or_else(|| args["patch"].as_str())
                    .unwrap_or("");
                for op in crate::tools::patch::parse(patch).unwrap_or_default() {
                    let path = match &op {
                        Op::Add { path, .. } | Op::Delete { path } | Op::Update { path, .. } => {
                            crate::util::resolve(cwd, path)
                        }
                    };
                    let old = self.preview_text(&path, editor).await;
                    match op {
                        Op::Add { content, .. } => changes.push((path, old, content)),
                        Op::Delete { .. } => changes.push((path, old, String::new())),
                        Op::Update {
                            path: source,
                            move_to,
                            chunks,
                        } => {
                            if let Some(new) = old.as_deref().and_then(|text| {
                                crate::tools::patch::apply_chunks(text, &chunks, &source).ok()
                            }) {
                                if let Some(dest) = move_to {
                                    changes.push((path, old, String::new()));
                                    let dest = crate::util::resolve(cwd, &dest);
                                    changes.push((
                                        dest.clone(),
                                        self.preview_text(&dest, editor).await,
                                        new,
                                    ));
                                } else {
                                    changes.push((path, old, new));
                                }
                            }
                        }
                    }
                }
            } else if let Some(path) = tool["locations"][0]["path"].as_str().map(PathBuf::from) {
                let old = self.preview_text(&path, editor).await;
                let proposed = match name {
                    "Write" => args["content"].as_str().map(str::to_string),
                    "NotebookEdit" => old
                        .as_deref()
                        .and_then(|text| crate::tools::fs::notebook_preview(text, args).ok()),
                    _ => {
                        let mut text = old.clone().unwrap_or_default();
                        let edits = if name == "Edit" {
                            vec![args]
                        } else {
                            args["edits"]
                                .as_array()
                                .map(|a| a.iter().collect())
                                .unwrap_or_default()
                        };
                        for edit in edits {
                            let from = edit["old_string"].as_str().unwrap_or("");
                            let to = edit["new_string"].as_str().unwrap_or("");
                            text = if edit["replace_all"] == true {
                                text.replace(from, to)
                            } else {
                                text.replacen(from, to, 1)
                            };
                        }
                        Some(text)
                    }
                };
                if let Some(new) = proposed {
                    changes.push((path, old, new));
                }
            }
            tool["locations"] = json!(
                changes
                    .iter()
                    .map(|(path, _, _)| json!({"path":path,"line":1}))
                    .collect::<Vec<_>>()
            );
            tool["content"] = json!(changes.into_iter().map(|(path, old, new)| json!({"type":"diff","path":path,"oldText":old,"newText":new})).collect::<Vec<_>>());
        }
        *self.last_tool.lock().unwrap() = tool.clone();
        let mut update = tool;
        update["sessionUpdate"] = json!("tool_call");
        self.update(update);
    }
    fn params(&self, path: &Path) -> Value {
        json!({"sessionId":*self.id.lock().unwrap(),"path":path})
    }
}
#[async_trait]
impl Frontend for AcpFrontend {
    fn emit(&self, event: Event) {
        let update = match event {
            Event::Text(text) => {
                json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}})
            }
            Event::Reasoning(text) => {
                json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":text}})
            }
            Event::ToolStart { id, .. } => {
                json!({"sessionUpdate":"tool_call_update","toolCallId":id,"status":"in_progress"})
            }
            Event::ToolEnd {
                id,
                content,
                is_error,
                ..
            } => {
                let mut contents = self
                    .contents
                    .lock()
                    .unwrap()
                    .remove(&id)
                    .unwrap_or_default();
                contents.push(json!({"type":"content","content":{"type":"text","text":content}}));
                json!({"sessionUpdate":"tool_call_update","toolCallId":id,"status":if is_error {"failed"} else {"completed"},"content":contents})
            }
            Event::ToolProgress { id, text } => {
                json!({"sessionUpdate":"tool_call_update","toolCallId":id,"content":[{"type":"content","content":{"type":"text","text":text}}]})
            }
            Event::Todos(todos) => {
                json!({"sessionUpdate":"plan","entries":todos.into_iter().map(|t| json!({"content":t.content,"priority":"medium","status":t.status})).collect::<Vec<_>>()})
            }
            Event::ModeChanged(mode) => {
                json!({"sessionUpdate":"current_mode_update","currentModeId":mode})
            }
            Event::Warn(text) | Event::Error(text) => {
                eprintln!("{text}");
                return;
            }
            _ => return,
        };
        self.update(update);
    }
    async fn tool_call(&self, id: &str, name: &str, args: &Value, cwd: &Path) {
        self.preview_tool_call(id, name, args, cwd, true).await;
    }
    fn writes_via_client(&self) -> bool {
        self.caps["fs"]["writeTextFile"] == true
    }
    fn file_restored(&self, path: &Path, text: &str) {
        if self.caps["fs"]["writeTextFile"] != true {
            return;
        }
        let mut params = self.params(path);
        params["content"] = json!(text);
        let peer = self.peer.clone();
        tokio::spawn(async move {
            if let Err(e) = peer.request("fs/write_text_file", params).await {
                eprintln!("{e:#}");
            }
        });
    }
    fn file_diff(&self, id: &str, path: &Path, old: Option<&str>, new: &str) {
        let content = json!({"type":"diff","path":path,"oldText":old,"newText":new});
        self.contents
            .lock()
            .unwrap()
            .entry(id.into())
            .or_default()
            .push(content.clone());
        self.update(
            json!({"sessionUpdate":"tool_call_update","toolCallId":id,"content":[content]}),
        );
    }
    async fn permission(&self, req: PermissionRequest) -> PermissionReply {
        let tool = self.last_tool.lock().unwrap().clone();
        let key = format!("{}:{}", req.tool, tool["rawInput"]);
        if self.rejected.lock().unwrap().contains(&key) {
            return PermissionReply::Deny(Some("Previously rejected for this session".into()));
        }
        let options = json!([
            {"optionId":"allow_once","name":"Allow once","kind":"allow_once"},
            {"optionId":"allow_always","name":req.always_label,"kind":"allow_always"},
            {"optionId":"reject_once","name":"Reject once","kind":"reject_once"},
            {"optionId":"reject_always","name":"Reject always","kind":"reject_always"}
        ]);
        let params =
            json!({"sessionId":*self.id.lock().unwrap(),"toolCall":tool,"options":options});
        match self
            .peer
            .request("session/request_permission", params)
            .await
        {
            Ok(r) if r["outcome"]["outcome"] == "selected" => {
                match r["outcome"]["optionId"].as_str() {
                    Some("allow_once") => PermissionReply::Allow,
                    Some("allow_always") => PermissionReply::AllowAlways,
                    Some("reject_always") => {
                        self.rejected.lock().unwrap().insert(key);
                        PermissionReply::Deny(None)
                    }
                    _ => PermissionReply::Deny(None),
                }
            }
            _ => PermissionReply::Deny(None),
        }
    }
    async fn question(&self, question: &str, _: &[String]) -> Option<String> {
        self.update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":question}}));
        None
    }
    async fn read_text_file(&self, path: &Path) -> Option<Result<String>> {
        if self.caps["fs"]["readTextFile"] != true {
            return None;
        }
        Some(
            self.peer
                .request("fs/read_text_file", self.params(path))
                .await
                .and_then(|r| {
                    r["content"]
                        .as_str()
                        .map(str::to_string)
                        .context("missing file content")
                }),
        )
    }
    async fn write_text_file(&self, path: &Path, text: &str) -> Option<Result<()>> {
        if self.caps["fs"]["writeTextFile"] != true {
            return None;
        }
        let mut p = self.params(path);
        p["content"] = json!(text);
        Some(self.peer.request("fs/write_text_file", p).await.map(|_| ()))
    }
}
struct Session {
    agent: AsyncMutex<Agent>,
    cancel: Mutex<Option<CancellationToken>>,
}
struct Server {
    cli: crate::Cli,
    peer: Peer,
    caps: Mutex<Value>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    startup: AsyncMutex<()>,
}
fn modes(current: crate::permissions::Mode) -> Value {
    json!({"currentModeId":current,"availableModes":[
        {"id":"default","name":"Default"},{"id":"acceptEdits","name":"Accept edits"},
        {"id":"plan","name":"Plan"},{"id":"bypassPermissions","name":"Bypass permissions"},{"id":"auto","name":"Auto"}]})
}
fn config_options(env: &crate::agent::Env) -> Value {
    let mut models = vec![env.model()];
    for model in [
        &env.config.model,
        &env.config.subagent_model,
        &env.config.classifier_model,
    ]
    .into_iter()
    .flatten()
    {
        if !models.contains(model) {
            models.push(model.clone());
        }
    }
    json!([{"id":"model","name":"Model","category":"model","type":"select","currentValue":env.model(),
        "options":models.into_iter().map(|model| json!({"value":model,"name":model})).collect::<Vec<_>>() }])
}
impl Server {
    async fn handle(&self, method: &str, p: &Value) -> Result<Value> {
        match method {
            "initialize" => {
                if !p["protocolVersion"].is_u64() {
                    bail!("protocolVersion must be an integer");
                }
                *self.caps.lock().unwrap() = p["clientCapabilities"].clone();
                Ok(
                    json!({"protocolVersion":1,"agentInfo":{"name":"chanto","version":env!("CARGO_PKG_VERSION")},"authMethods":[],"agentCapabilities":{"loadSession":true,"promptCapabilities":{"image":true,"embeddedContext":true},"mcpCapabilities":{"http":true,"sse":true,"stdio":true}}}),
                )
            }
            "authenticate" => {
                let cwd = self.cli.cwd.clone().unwrap_or(std::env::current_dir()?);
                let cfg = crate::config::Config::load(&cwd, crate::config::is_trusted(&cwd))?.0;
                if self.cli.model.is_none() && cfg.model.is_none() {
                    bail!("No model configured. Run `chanto setup` or `chanto login`.");
                }
                Ok(json!({}))
            }
            "session/new" | "session/load" => {
                let _guard = self.startup.lock().await;
                let cwd = PathBuf::from(p["cwd"].as_str().context("cwd is required")?);
                if !cwd.is_absolute() || !cwd.is_dir() {
                    bail!("cwd must be an existing absolute directory");
                }
                let mut cli = self.cli.clone();
                cli.prompt.clear();
                cli.mode = Some("default".into());
                cli.no_trust_prompt = true;
                cli.r#continue = false;
                cli.resume = if method == "session/load" {
                    Some(
                        p["sessionId"]
                            .as_str()
                            .context("sessionId required")?
                            .into(),
                    )
                } else {
                    None
                };
                if let Some(id) = &cli.resume {
                    crate::session::Session::load(&cwd, id)?;
                }
                let mut servers = std::collections::BTreeMap::new();
                for s in p["mcpServers"].as_array().into_iter().flatten() {
                    let name = s["name"].as_str().context("MCP name required")?;
                    let mut cfg = crate::config::McpServerConfig {
                        command: s["command"].as_str().unwrap_or("").into(),
                        url: s["url"].as_str().map(str::to_string),
                        transport: s["type"].as_str().map(str::to_string),
                        args: s["args"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|s| s.as_str().map(str::to_string))
                            .collect(),
                        ..Default::default()
                    };
                    for e in s["env"].as_array().into_iter().flatten() {
                        cfg.env.insert(
                            e["name"].as_str().context("env name required")?.into(),
                            e["value"].as_str().context("env value required")?.into(),
                        );
                    }
                    for h in s["headers"].as_array().into_iter().flatten() {
                        cfg.headers.insert(
                            h["name"].as_str().context("header name required")?.into(),
                            h["value"].as_str().context("header value required")?.into(),
                        );
                    }
                    servers.insert(name.into(), cfg);
                }
                let (mcp, errors) = crate::mcp::connect_all(&servers).await;
                if !errors.is_empty() {
                    bail!("MCP: {}", errors.join("; "));
                }
                let remote = mcp
                    .iter()
                    .flat_map(|c| c.tool_names().into_iter().filter_map(|name| c.tool(&name)))
                    .collect();
                let fe = Arc::new(AcpFrontend {
                    peer: self.peer.clone(),
                    id: Mutex::new(String::new()),
                    caps: self.caps.lock().unwrap().clone(),
                    last_tool: Mutex::new(Value::Null),
                    contents: Default::default(),
                    rejected: Default::default(),
                });
                let mut setup =
                    crate::setup_session(&cli, fe.clone(), true, cwd.clone(), remote, Some(false))
                        .await
                        .context("Configure a model with `chanto setup` or `chanto login`")?;
                setup.agent.cwd_override = Some(cwd);
                setup.agent.refresh_system_prompt();
                let id = setup
                    .agent
                    .session
                    .as_ref()
                    .context("session persistence disabled")?
                    .id
                    .clone();
                *fe.id.lock().unwrap() = id.clone();
                if method == "session/load" {
                    for m in &setup.agent.messages {
                        for call in &m.tool_calls {
                            // Replaying history must not request current editor buffers.
                            fe.preview_tool_call(
                                &call.id,
                                &call.name,
                                &call.arguments,
                                &setup.env.root,
                                false,
                            )
                            .await;
                        }
                        if m.role == crate::llm::Role::Tool {
                            fe.emit(Event::ToolEnd {
                                id: m.tool_call_id.clone().unwrap_or_default(),
                                content: String::clone(&m.content),
                                display: String::new(),
                                is_error: false,
                                images: Vec::new(),
                                parent: None,
                            });
                        }
                        let kind = match m.role {
                            crate::llm::Role::User => "user_message_chunk",
                            crate::llm::Role::Assistant => "agent_message_chunk",
                            _ => continue,
                        };
                        if !m.content.is_empty() {
                            fe.update(json!({"sessionUpdate":kind,"content":{"type":"text","text":m.content}}));
                        }
                    }
                }
                let result = json!({"sessionId":id,"modes":modes(setup.env.mode()),"configOptions":config_options(&setup.env),"models":{"currentModelId":setup.env.model(),"availableModels":[{"modelId":setup.env.model(),"name":setup.env.model()}]}});
                self.sessions.lock().unwrap().insert(
                    id,
                    Arc::new(Session {
                        agent: AsyncMutex::new(setup.agent),
                        cancel: Mutex::new(None),
                    }),
                );
                Ok(result)
            }
            "session/prompt"
            | "session/cancel"
            | "session/set_mode"
            | "session/set_model"
            | "session/set_config_option" => {
                let id = p["sessionId"].as_str().context("sessionId required")?;
                let s = self
                    .sessions
                    .lock()
                    .unwrap()
                    .get(id)
                    .cloned()
                    .context("unknown session")?;
                if method == "session/cancel" {
                    if let Some(c) = &*s.cancel.lock().unwrap() {
                        c.cancel();
                    }
                    return Ok(json!({}));
                }
                let mut a = s
                    .agent
                    .try_lock()
                    .map_err(|_| anyhow::anyhow!("session busy"))?;
                if method == "session/set_mode" {
                    let mode = crate::permissions::Mode::parse(
                        p["modeId"].as_str().context("modeId required")?,
                    )
                    .context("unknown mode")?;
                    a.env.set_mode(mode);
                    a.refresh_system_prompt();
                    return Ok(json!({}));
                }
                if method == "session/set_config_option" {
                    if p["configId"] != "model" {
                        bail!("unknown configId");
                    }
                    let model = p["value"]
                        .as_str()
                        .filter(|s| !s.trim().is_empty())
                        .context("model value required")?;
                    crate::hooks::switch_model(&a.env, model, "sdk")
                        .await
                        .map_err(|e| anyhow::anyhow!(e))?;
                    a.refresh_system_prompt();
                    let options = config_options(&a.env);
                    self.peer.send(json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":id,"update":{"sessionUpdate":"config_option_update","configOptions":options}}}));
                    return Ok(json!({"configOptions":options}));
                }
                if method == "session/set_model" {
                    let model = p["modelId"].as_str().context("modelId required")?;
                    crate::hooks::switch_model(&a.env, model, "sdk")
                        .await
                        .map_err(|e| anyhow::anyhow!(e))?;
                    a.refresh_system_prompt();
                    return Ok(json!({}));
                }
                let mut text = String::new();
                let mut images = Vec::new();
                for block in p["prompt"].as_array().context("prompt must be an array")? {
                    match block["type"].as_str() {
                        Some("text") => {
                            text.push_str(block["text"].as_str().context("text required")?);
                            text.push('\n');
                        }
                        Some("image") => images.push(
                            block["data"]
                                .as_str()
                                .context("image data required")?
                                .into(),
                        ),
                        Some("resource_link") => {
                            let uri = block["uri"].as_str().context("uri required")?;
                            if let Some(path) = uri.strip_prefix("file://") {
                                let path = PathBuf::from(urlencoding::decode(path)?.into_owned());
                                let frontend = &a.env.frontend;
                                if let Some(contents) = frontend.read_text_file(&path).await {
                                    text.push_str(&format!(
                                        "\n<file path={:?}>\n{}\n</file>\n",
                                        path, contents?
                                    ));
                                    a.env.mark_read(&path);
                                } else {
                                    text.push_str(&format!(" @{} ", path.display()));
                                }
                            } else {
                                text.push_str(&format!("\nResource: {uri}\n"));
                            }
                        }
                        Some("resource") => {
                            let resource = &block["resource"];
                            let content = if let Some(text) = resource["text"].as_str() {
                                text.to_string()
                            } else {
                                let blob = resource["blob"]
                                    .as_str()
                                    .context("embedded resource text or blob required")?;
                                if resource["mimeType"]
                                    .as_str()
                                    .is_some_and(|m| m.starts_with("image/"))
                                {
                                    images.push(blob.into());
                                    continue;
                                }
                                String::from_utf8(
                                    base64::engine::general_purpose::STANDARD.decode(blob)?,
                                )
                                .context("embedded resource must contain text or an image")?
                            };
                            text.push_str(&format!(
                                "\n<file path={:?}>\n{}\n</file>\n",
                                resource["uri"], content
                            ));
                        }
                        _ => bail!("unsupported prompt content"),
                    }
                }
                let cancel = CancellationToken::new();
                *s.cancel.lock().unwrap() = Some(cancel.clone());
                // The client's prompt is the user's; the attachments in it
                // are cut before it is recorded.
                a.typed_input = true;
                let end = a.run_turn(&text, images, cancel).await;
                *s.cancel.lock().unwrap() = None;
                a.save_session();
                Ok(
                    json!({"stopReason":match end? { TurnEnd::Done(_) => "end_turn", TurnEnd::Interrupted => "cancelled" }}),
                )
            }
            _ => bail!("unknown method {method}"),
        }
    }
}

pub async fn run(cli: crate::Cli) -> Result<()> {
    let (out, mut rx) = mpsc::unbounded_channel();
    let peer = Peer {
        out,
        pending: Default::default(),
        serial: Arc::new(AtomicU64::new(1)),
    };
    let server = Arc::new(Server {
        cli,
        peer: peer.clone(),
        caps: Mutex::new(json!({})),
        sessions: Default::default(),
        startup: AsyncMutex::new(()),
    });
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(v) = rx.recv().await {
            stdout.write_all(format!("{v}\n").as_bytes()).await?;
            stdout.flush().await?;
        }
        Ok::<_, std::io::Error>(())
    });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut tasks = tokio::task::JoinSet::new();
    while let Some(line) = lines.next_line().await? {
        while tasks.try_join_next().is_some() {}
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                peer.send(json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}));
                continue;
            }
        };
        if !msg.is_object()
            || msg["jsonrpc"] != "2.0"
            || msg.get("method").is_some_and(|m| !m.is_string())
            || msg
                .get("id")
                .is_some_and(|id| !id.is_null() && !id.is_string() && !id.is_number())
        {
            peer.send(json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid Request"}}));
            continue;
        }
        if msg.get("method").is_none() {
            if let Some(tx) = peer
                .pending
                .lock()
                .unwrap()
                .remove(msg["id"].as_str().unwrap_or(""))
            {
                let _ = tx.send(msg);
            }
            continue;
        }
        let server = server.clone();
        tasks.spawn(async move {
            let method = msg["method"].as_str().unwrap_or("");
            let result = server.handle(method, &msg["params"]).await;
            if let Some(id) = msg.get("id") {
                let response = match result { Ok(r) => json!({"jsonrpc":"2.0","id":id,"result":r}), Err(e) => json!({"jsonrpc":"2.0","id":id,"error":{"code":if method.starts_with("session/") || matches!(method,"initialize"|"authenticate") {-32602} else {-32601},"message":format!("{e:#}")}}) };
                server.peer.send(response);
            }
        });
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    peer.pending.lock().unwrap().clear();
    drop(server);
    drop(peer);
    writer.await??;
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod prompt_tests;
