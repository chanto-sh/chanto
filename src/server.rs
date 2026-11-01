//! `chanto serve`: an HTTP + server-sent-events API so other programs (web or
//! desktop UIs, editors, scripts, the SDKs in `sdk/`) can drive sessions.
//!
//! API routes (except health) need `Authorization: Bearer <token>`. Events are the same JSON
//! objects as `--output-format stream-json`, plus `permission_request`,
//! `question` and `turn_done`. See `sdk/README.md` for the protocol.

use crate::agent::{
    Agent, Env, Event, Frontend, PermissionReply, PermissionRequest, Queued, TurnEnd, event_json,
    image_data_urls,
};
use crate::wire::{Health, StreamEvent, TurnDone};
use crate::{Cli, commands};
use anyhow::Result;
use async_trait::async_trait;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, broadcast, oneshot};
use tokio_util::sync::CancellationToken;

/// Open requests: the answer channel plus the event, replayed to clients
/// that subscribe after it was sent.
type Pending = Arc<Mutex<HashMap<String, (oneshot::Sender<Value>, Value)>>>;

struct ServerFrontend {
    tx: broadcast::Sender<Value>,
    pending: Pending,
}

impl ServerFrontend {
    /// Send a request event and wait for the client's answer.
    /// Send the event `build` makes with a new request id, and wait for its answer.
    async fn ask(&self, build: impl FnOnce(String) -> Value) -> Option<Value> {
        let id = format!("req-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let (tx, rx) = oneshot::channel();
        let mut ev = build(id.clone());
        crate::secrets::redact_json(&mut ev);
        self.pending
            .lock()
            .unwrap()
            .insert(id.clone(), (tx, ev.clone()));
        let _ = self.tx.send(ev);
        rx.await.ok()
    }
}

#[async_trait]
impl Frontend for ServerFrontend {
    fn emit(&self, ev: Event) {
        if let Some(v) = event_json(&ev) {
            let _ = self.tx.send(v);
        }
    }
    async fn permission(&self, req: PermissionRequest) -> PermissionReply {
        let ev = |request_id| {
            StreamEvent::PermissionRequest {
                request_id,
                tool: &req.tool,
                label: &req.label,
                detail: &req.detail,
                reason: req.reason.as_deref(),
                always_label: &req.always_label,
            }
            .to_value()
        };
        let Some(a) = self.ask(ev).await else {
            return PermissionReply::Deny(None);
        };
        match a["decision"].as_str() {
            Some("allow") => PermissionReply::Allow,
            Some("allow_always") => PermissionReply::AllowAlways,
            _ => PermissionReply::Deny(
                a["feedback"]
                    .as_str()
                    .filter(|f| !f.is_empty())
                    .map(str::to_string),
            ),
        }
    }
    async fn question(&self, question: &str, options: &[String]) -> Option<String> {
        let a = self
            .ask(|request_id| {
                StreamEvent::Question {
                    request_id,
                    question,
                    options,
                }
                .to_value()
            })
            .await?;
        a["answer"].as_str().map(str::to_string)
    }
}

/// Remove unanswered requests even when the agent drops the tool future on interrupt.
struct PendingGuard {
    pending: Pending,
    id: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.id);
    }
}

struct RemoteTool {
    name: String,
    description: String,
    schema: Value,
    tx: broadcast::Sender<Value>,
    pending: Pending,
    timeout: std::time::Duration,
}

#[async_trait]
impl crate::tools::Tool for RemoteTool {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn description(&self) -> String {
        self.description.clone()
    }
    fn schema(&self) -> Value {
        self.schema.clone()
    }
    fn kind(&self) -> crate::tools::ToolKind {
        crate::tools::ToolKind::External
    }
    async fn run(&self, input: Value, ctx: &crate::tools::Ctx) -> Result<crate::tools::ToolOutput> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let ev = StreamEvent::ToolCall {
            id: &id,
            name: &self.name,
            input: &input,
        }
        .to_value();
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(id.clone(), (tx, ev.clone()));
        let _guard = PendingGuard {
            pending: self.pending.clone(),
            id: id.clone(),
        };
        let _ = self.tx.send(ev);
        let answer = tokio::select! {
            _ = ctx.cancel.cancelled() => None,
            a = tokio::time::timeout(self.timeout, rx) => a.ok().and_then(|r| r.ok()),
        };
        Ok(match answer {
            Some(a) => crate::tools::ToolOutput {
                content: a["result"]["content"].as_str().unwrap_or_default().into(),
                is_error: a["result"]["is_error"].as_bool().unwrap_or(false),
                ..Default::default()
            },
            None => {
                crate::tools::ToolOutput::err("Remote tool cancelled, disconnected, or timed out")
            }
        })
    }
}

fn validate_options(body: &Value) -> Result<(), String> {
    if !body.is_object() {
        return Err("session options must be an object".into());
    }
    for key in ["model", "mode", "resume", "cwd", "system_prompt_append"] {
        if let Some(v) = body.get(key) {
            if !v.is_string() {
                return Err(format!("`{key}` must be a string"));
            }
            if key != "system_prompt_append" && v.as_str().unwrap().trim().is_empty() {
                return Err(format!("`{key}` must not be empty"));
            }
        }
    }
    if let Some(v) = body.get("mode")
        && crate::permissions::Mode::parse(v.as_str().unwrap()).is_none()
    {
        return Err("unknown `mode`".into());
    }
    if let Some(v) = body.get("verify")
        && !v.is_boolean()
    {
        return Err("`verify` must be a boolean".into());
    }
    if let Some(v) = body.get("max_turns")
        && !v.as_u64().is_some_and(|n| n > 0 && n <= u32::MAX as u64)
    {
        return Err("`max_turns` must be a positive 32-bit integer".into());
    }
    for key in ["allowed_tools", "disallowed_tools"] {
        if let Some(v) = body.get(key)
            && !v.as_array().is_some_and(|a| {
                a.iter().all(|n| {
                    n.as_str()
                        .is_some_and(|s| !s.is_empty() && !s.contains(','))
                })
            })
        {
            return Err(format!("`{key}` must be a list of nonempty tool names"));
        }
    }
    let mut names = crate::tools::Registry::builtin().names();
    names.extend(["Lsp", "ListMcpResources", "ReadMcpResource", "Computer"].map(str::to_string));
    if let Some(v) = body.get("tools") {
        let Some(tools) = v.as_array() else {
            return Err("`tools` must be a list".into());
        };
        for t in tools {
            let Some(name) = t["name"].as_str() else {
                return Err("tool `name` must be a string".into());
            };
            if name.is_empty()
                || name.len() > 64
                || !name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
            {
                return Err(format!("invalid tool name `{name}`"));
            }
            if names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                return Err(format!("tool name `{name}` is already registered"));
            }
            names.push(name.into());
            if !t["description"].is_string()
                || !t["input_schema"].is_object()
                || t["input_schema"]["type"] != "object"
            {
                return Err(format!(
                    "tool `{name}` requires a description and an object input_schema"
                ));
            }
        }
    }
    Ok(())
}

struct Session {
    options: Value,
    agent: Arc<AsyncMutex<Agent>>,
    env: Arc<Env>,
    tx: broadcast::Sender<Value>,
    pending: Pending,
    cancel: Mutex<Option<CancellationToken>>,
}

/// Visual pieces the web UI previews: the HTML of a code block, kept in
/// memory behind an unguessable id. Nothing is written to disk, the body is
/// capped, and the oldest previews go when the store is full.
#[derive(Default)]
struct Artifacts {
    order: std::collections::VecDeque<String>,
    pages: HashMap<String, String>,
}

impl Artifacts {
    /// Enough for the previews of a long conversation; a code block is small.
    const CAPACITY: usize = 64;

    fn insert(&mut self, html: String) -> String {
        let id = uuid::Uuid::new_v4().simple().to_string();
        self.pages.insert(id.clone(), html);
        self.order.push_back(id.clone());
        while self.order.len() > Self::CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.pages.remove(&old);
            }
        }
        id
    }
}

#[derive(Clone)]
struct AppState {
    cli: Arc<Cli>,
    token: Arc<String>,
    sessions: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    artifacts: Arc<Mutex<Artifacts>>,
}

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({"error": msg.into()}))).into_response()
}

async fn auth(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(bearer_token)
        .is_some_and(|t| constant_time_eq(t.as_bytes(), st.token.as_bytes()));
    if !ok {
        return err(StatusCode::UNAUTHORIZED, "missing or wrong bearer token");
    }
    next.run(req).await
}

/// The token in an `Authorization: Bearer <token>` header. The scheme name is
/// case-insensitive (RFC 7235), so `bearer` and `BEARER` work too.
pub(crate) fn bearer_token(header: &str) -> Option<&str> {
    let (scheme, token) = header.trim_start().split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Handler helpers return the error as an axum `Response`; boxing it would only
/// add an allocation.
#[allow(clippy::result_large_err)]
fn session(st: &AppState, id: &str) -> Result<Arc<Session>, Response> {
    st.sessions
        .lock()
        .unwrap()
        .get(id)
        .cloned()
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("no session {id}")))
}

async fn create_session(State(st): State<AppState>, body: Option<Json<Value>>) -> Response {
    let body = body.map(|b| b.0).unwrap_or_else(|| json!({}));
    if let Err(e) = validate_options(&body) {
        return err(StatusCode::BAD_REQUEST, e);
    }
    let cwd = match body["cwd"]
        .as_str()
        .map(std::path::PathBuf::from)
        .or_else(|| st.cli.cwd.clone())
    {
        Some(p) => match crate::util::canonical(&p) {
            Ok(p) if p.is_dir() => p,
            _ => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "`cwd` must be an existing directory",
                );
            }
        },
        None => std::env::current_dir().unwrap_or_default(),
    };
    // The project root (permission checks, sandbox, secret masking) is
    // process-wide, so one server serves one project at a time.
    if let Some(other) = st
        .sessions
        .lock()
        .unwrap()
        .values()
        .map(|s| s.env.root.clone())
        .find(|r| *r != cwd)
    {
        return err(
            StatusCode::CONFLICT,
            format!(
                "this server is serving {}; start another `chanto serve` (or use query()) for {}",
                other.display(),
                cwd.display()
            ),
        );
    }
    let (tx, _) = broadcast::channel(1024);
    let pending: Pending = Default::default();
    let fe = Arc::new(ServerFrontend {
        tx: tx.clone(),
        pending: pending.clone(),
    });
    let mut cli = (*st.cli).clone();
    if let Some(m) = body["model"].as_str() {
        cli.model = Some(m.into());
    }
    if let Some(m) = body["mode"].as_str() {
        cli.mode = Some(m.into());
    }
    if let Some(r) = body["resume"].as_str() {
        // Two live sessions on one transcript would overwrite each other.
        if st.sessions.lock().unwrap().keys().any(|k| k.starts_with(r)) {
            return err(StatusCode::CONFLICT, format!("session {r} is already open"));
        }
        cli.resume = Some(r.into());
    }
    cli.append_system_prompt = body["system_prompt_append"]
        .as_str()
        .map(str::to_string)
        .or(cli.append_system_prompt);
    cli.max_turns = body["max_turns"]
        .as_u64()
        .map(|n| n as u32)
        .or(cli.max_turns);
    for (key, dest) in [
        ("allowed_tools", &mut cli.allowed_tools),
        ("disallowed_tools", &mut cli.disallowed_tools),
    ] {
        if let Some(a) = body[key].as_array() {
            *dest = Some(
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
    }
    let remote: Vec<Arc<dyn crate::tools::Tool>> = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        // Skip malformed entries rather than panicking on client input (a
        // missing `name` would otherwise take down the request task).
        .filter_map(|t| {
            let name = t["name"].as_str()?;
            let tool: Arc<dyn crate::tools::Tool> = Arc::new(RemoteTool {
                name: name.into(),
                description: t["description"].as_str().unwrap_or("").into(),
                schema: t["input_schema"].clone(),
                tx: tx.clone(),
                pending: pending.clone(),
                timeout: std::time::Duration::from_secs(600),
            });
            Some(tool)
        })
        .collect();
    let mut setup = match crate::setup_session(
        &cli,
        fe,
        true,
        cwd.clone(),
        remote,
        body["verify"].as_bool(),
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    setup.agent.cwd_override = Some(cwd);
    setup.agent.refresh_system_prompt();
    let id = setup
        .agent
        .session
        .as_ref()
        .map(|s| s.id.clone())
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let model = setup.env.model();
    st.sessions.lock().unwrap().insert(
        id.clone(),
        Arc::new(Session {
            options: body.clone(),
            agent: Arc::new(AsyncMutex::new(setup.agent)),
            env: setup.env,
            tx,
            pending,
            cancel: Mutex::new(None),
        }),
    );
    Json(json!({"id": id, "model": model, "warnings": setup.warnings})).into_response()
}

async fn fork_session(
    State(st): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    let request = body.map(|b| b.0).unwrap_or_else(|| json!({}));
    if request.get("point").is_some_and(|p| !p.is_u64())
        || request.get("name").is_some_and(|n| !n.is_string())
    {
        return err(
            StatusCode::BAD_REQUEST,
            "point must be a checkpoint index and name a string",
        );
    }
    let cwd = st
        .cli
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let (meta, messages, mut options) = match session(&st, &id) {
        Ok(source) => {
            if source.cancel.lock().unwrap().is_some() {
                return err(StatusCode::CONFLICT, "session is busy");
            }
            let Ok(mut agent) = source.agent.try_lock() else {
                return err(StatusCode::CONFLICT, "session is busy");
            };
            agent.save_session();
            let Some(meta) = agent.session.clone() else {
                return err(StatusCode::NOT_FOUND, "session has no transcript");
            };
            let mut options = source.options.clone();
            options["mode"] = json!(source.env.mode());
            (meta, agent.messages.clone(), options)
        }
        Err(_) => match crate::session::Session::load(&cwd, &id) {
            Ok((meta, messages)) => (meta, messages, json!({})),
            Err(_) => return err(StatusCode::NOT_FOUND, "no session"),
        },
    };
    let (child, _) = match meta.fork(
        &messages,
        request["name"].as_str(),
        request["point"].as_u64().map(|n| n as usize),
    ) {
        Ok(child) => child,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };
    options["resume"] = json!(child.id);
    options["cwd"] = json!(child.cwd);
    options["model"] = json!(child.model);
    create_session(State(st), Some(Json(options))).await
}

/// The project whose saved sessions a request means: `?cwd=` (the web UI's
/// open project), else the server's own folder.
fn project_dir(st: &AppState, q: &HashMap<String, String>) -> std::path::PathBuf {
    q.get("cwd")
        .filter(|c| !c.is_empty())
        .and_then(|c| std::fs::canonicalize(c).ok())
        .or_else(|| st.cli.cwd.clone())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

async fn list_sessions(
    State(st): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let list: Vec<Value> = st
        .sessions
        .lock()
        .unwrap()
        .iter()
        .map(|(id, s)| {
            // A busy session's agent is locked; its saved title below covers it.
            let title = s.agent.try_lock().ok().and_then(|a| a.session.as_ref().map(|x| x.title.clone()));
            json!({"id": id, "busy": s.cancel.lock().unwrap().is_some(), "model": s.env.model(), "title": title})
        })
        .collect();
    let mut list = list;
    let cwd = project_dir(&st, &q);
    // Archived sessions are listed with `?all=true` (as `chanto sessions --all`).
    let all = q.get("all").is_some_and(|v| v == "true" || v == "1");
    for (saved, depth) in crate::session::Session::tree(&cwd) {
        if let Some(live) = list.iter_mut().find(|s| s["id"] == saved.id) {
            live["parent_id"] = json!(saved.parent_id);
            live["fork_point"] = json!(saved.fork_point);
            live["depth"] = json!(depth);
            if live["title"].as_str().is_none_or(str::is_empty) {
                live["title"] = json!(saved.title);
            }
        } else if all || !saved.archived {
            list.push(json!({"id": saved.id, "model": saved.model, "title": saved.title, "saved": true, "archived": saved.archived, "parent_id": saved.parent_id, "fork_point": saved.fork_point, "depth": depth}));
        }
    }
    Json(json!({"sessions": list})).into_response()
}

async fn get_session(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let s = match session(&st, &id) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let Ok(a) = s.agent.try_lock() else {
        return err(StatusCode::CONFLICT, "session is busy");
    };
    // A prompt that starts a checkpoint carries its index, for rewind and fork.
    let starts: HashMap<usize, usize> = a
        .env
        .checkpoints
        .lock()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, c)| (c.msg_index, i))
        .collect();
    let msgs: Vec<Value> = a
        .messages
        .iter()
        .enumerate()
        .skip(1)
        .map(|(i, m)| json!({"role": m.role, "content": m.content, "checkpoint": starts.get(&i), "images": image_data_urls(&m.images), "tool_calls": m.tool_calls.iter().map(|c| json!({"id": c.id, "name": c.name, "arguments": c.arguments})).collect::<Vec<_>>()}))
        .collect();
    let mut out = json!({"id": id, "model": s.env.model(), "mode": s.env.mode(), "messages": msgs, "todos": *s.env.todos.lock().unwrap()});
    crate::secrets::redact_json(&mut out);
    Json(out).into_response()
}

/// A session's checkpoints (one per prompt, oldest first), whether each can
/// restore code and the conversation, and whether a code restore can be undone.
fn checkpoint_list(agent: &Agent) -> Value {
    let len = agent.messages.len();
    let list: Vec<Value> = agent
        .env
        .checkpoints
        .lock()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, c)| json!({"index": i, "prompt": c.prompt, "timestamp": c.timestamp,
            "code": c.tree.is_some() || !c.files.is_empty(), "conversation": c.msg_index > 0 && c.msg_index < len}))
        .collect();
    let mut out =
        json!({"checkpoints": list, "undo": agent.env.rewind_undo.lock().unwrap().is_some()});
    crate::secrets::redact_json(&mut out);
    out
}

/// The live session's agent, unless a turn is running.
#[allow(clippy::result_large_err)]
fn idle_agent(s: &Session) -> Result<tokio::sync::MutexGuard<'_, Agent>, Response> {
    if s.cancel.lock().unwrap().is_some() {
        return Err(err(StatusCode::CONFLICT, "session is busy"));
    }
    s.agent
        .try_lock()
        .map_err(|_| err(StatusCode::CONFLICT, "session is busy"))
}

async fn checkpoints(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let s = match session(&st, &id) {
        Ok(s) => s,
        Err(e) => return e,
    };
    match idle_agent(&s) {
        Ok(agent) => Json(checkpoint_list(&agent)).into_response(),
        Err(e) => e,
    }
}

/// `{checkpoint, mode?}` rewinds to before that prompt like `/rewind` (mode
/// `both`, `conversation` or `code`); `{undo: true}` undoes the last code
/// restore. With `preview: true` nothing changes: the reply's `summary` says
/// what a code restore would write and delete (null when it restores no code).
async fn rewind_session(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let s = match session(&st, &id) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let undo = body["undo"].as_bool().unwrap_or(false);
    let mode = body
        .get("mode")
        .map_or(Some("both"), |m| m.as_str())
        .unwrap_or("");
    let idx = body["checkpoint"].as_u64().map(|n| n as usize);
    if !matches!(mode, "both" | "conversation" | "code") || (!undo && idx.is_none()) {
        return err(
            StatusCode::BAD_REQUEST,
            "`checkpoint` must be a checkpoint index and `mode` one of both, conversation or code",
        );
    }
    let mut agent = match idle_agent(&s) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let env = agent.env.clone();
    let idx = idx.unwrap_or(0);
    if !undo && idx >= env.checkpoints.lock().unwrap().len() {
        return err(StatusCode::BAD_REQUEST, "no such checkpoint");
    }
    // Same test as the list's `conversation` flag: a prompt compacted away can't
    // be rewound to, and trying would still drop the later checkpoints.
    if !undo && mode != "code" {
        let at = env.checkpoints.lock().unwrap()[idx].msg_index;
        if at == 0 || at >= agent.messages.len() {
            return err(
                StatusCode::BAD_REQUEST,
                "that prompt is no longer in the conversation (it was compacted); only its code can be restored",
            );
        }
    }
    if body["preview"].as_bool().unwrap_or(false) {
        let tree = if undo {
            env.rewind_undo.lock().unwrap().clone()
        } else if mode != "conversation" {
            env.checkpoints.lock().unwrap()[idx].tree.clone()
        } else {
            None
        };
        return match tree
            .map(|t| env.shadow().and_then(|sh| sh.restore_summary(&t)))
            .transpose()
        {
            Ok(summary) => Json(json!({"summary": summary})).into_response(),
            Err(e) => err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Cannot preview restore: {e:#}"),
            ),
        };
    }
    let result = if undo {
        agent.rewind_undo().map(|n| (n, String::new()))
    } else {
        agent.rewind_mode(idx, mode)
    };
    match result {
        Ok((files, prompt)) => {
            agent.save_session();
            let mut out = checkpoint_list(&agent);
            out["files_restored"] = json!(files);
            out["prompt"] = json!(prompt);
            Json(out).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn send_message(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let s = match session(&st, &id) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let Some(text) = body["text"].as_str().map(str::to_string) else {
        return err(StatusCode::BAD_REQUEST, "`text` is required");
    };
    // Claim the turn in a scope of its own, so the lock is never held across
    // the awaits below.
    let cancel = {
        let mut c = s.cancel.lock().unwrap();
        match c.as_ref() {
            Some(_) => None,
            None => {
                let t = CancellationToken::new();
                *c = Some(t.clone());
                Some(t)
            }
        }
    };
    let Some(cancel) = cancel else {
        // Commands that only read or change tooling state run now, as they do
        // in the terminal; prompts still wait for the model's next step.
        let line = text.trim();
        if line.starts_with('/') && commands::is_immediate(line) {
            match commands::handle_env(&s.env, line).await {
                Some(commands::Outcome::Show(md)) => {
                    let _ = s.tx.send(StreamEvent::Info { message: &md }.to_value());
                    return (StatusCode::ACCEPTED, Json(json!({"output": md}))).into_response();
                }
                Some(commands::Outcome::Exit | commands::Outcome::Nothing) => {
                    return (StatusCode::ACCEPTED, Json(json!({}))).into_response();
                }
                _ => {}
            }
        }
        // Like typing while the model works: delivered at its next step.
        let q = Queued::new(text);
        s.env.inbox.lock().unwrap().push(q.clone());
        return (
            StatusCode::ACCEPTED,
            Json(json!({"queued": true, "message": {"id": q.id, "text": q.text}})),
        )
            .into_response();
    };
    let s2 = s.clone();
    tokio::spawn(async move {
        let mut a = s2.agent.lock().await;
        let mut prompt = text.clone();
        let mut result = TurnDone::default();
        // Typed in the web UI; a command's expansion and the jobs it runs are
        // Chanto's prompts, so only the command itself is the user's.
        let command = text.trim_start().starts_with('/');
        if command {
            a.env.record_user_request(text.trim());
        }
        let mut typed = !command;
        if command {
            match commands::handle(&mut a, text.trim()).await {
                commands::Outcome::Prompt(p) => prompt = p,
                commands::Outcome::Run(job) => {
                    match job.run(&mut a, cancel.clone()).await {
                        Ok(r) => result.output = Some(r),
                        Err(e) => result.error = Some(format!("{e:#}")),
                    }
                    prompt.clear();
                }
                commands::Outcome::Show(md) => {
                    result.output = Some(md);
                    prompt.clear();
                }
                commands::Outcome::Reset(m) => {
                    result.output = Some(m.unwrap_or_else(|| "Session reset.".into()));
                    prompt.clear();
                }
                commands::Outcome::Exit | commands::Outcome::Nothing => prompt.clear(),
            }
        }
        if !prompt.is_empty() {
            let (t, images) = commands::expand_mentions(&a, &prompt).await;
            let mut next = Some((t, images));
            while let Some((t, images)) = next.take() {
                a.typed_input = std::mem::take(&mut typed);
                match a.run_turn(&t, images, cancel.clone()).await {
                    Ok(TurnEnd::Done(r)) => result.result = Some(r),
                    Ok(TurnEnd::Interrupted) => result.interrupted = Some(true),
                    Err(e) => result.error = Some(format!("{e:#}")),
                }
                a.save_session();
                // Messages queued during the final answer, and background reports.
                let mut left: Vec<String> = s2
                    .env
                    .inbox
                    .lock()
                    .unwrap()
                    .drain(..)
                    .map(|q| q.text)
                    .collect();
                // The user typed these; the reports joined to them aren't.
                for t in &left {
                    s2.env.record_user_request(t);
                }
                left.extend(s2.env.notifications.lock().unwrap().drain(..));
                if !left.is_empty() && !cancel.is_cancelled() {
                    next = Some((left.join("\n\n"), vec![]));
                }
            }
        }
        result.total_cost_usd = s2.env.accounting.total();
        result.price_unknown = s2.env.accounting.unknown();
        drop(a);
        *s2.cancel.lock().unwrap() = None;
        let _ = s2.tx.send(StreamEvent::TurnDone(result).to_value());
    });
    (StatusCode::ACCEPTED, Json(json!({"started": true}))).into_response()
}

/// Prompts waiting for the model's next step, with ids so a client can take
/// one back to edit it. Reading them doesn't take the agent lock, so the queue
/// stays editable while the turn runs.
fn queued_json(env: &Env) -> Value {
    let queued: Vec<Value> = env
        .inbox
        .lock()
        .unwrap()
        .iter()
        .map(|q| json!({"id": q.id, "text": q.text}))
        .collect();
    let mut out = json!({"queued": queued});
    crate::secrets::redact_json(&mut out);
    out
}

async fn get_queue(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let s = match session(&st, &id) {
        Ok(s) => s,
        Err(e) => return e,
    };
    Json(queued_json(&s.env)).into_response()
}

async fn edit_queued(
    State(st): State<AppState>,
    Path((id, qid)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Response {
    let s = match session(&st, &id) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let Some(text) = body["text"].as_str() else {
        return err(StatusCode::BAD_REQUEST, "`text` is required");
    };
    if text.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "`text` cannot be empty");
    }
    let mut inbox = s.env.inbox.lock().unwrap();
    match inbox.iter_mut().find(|q| q.id == qid) {
        Some(q) => q.text = text.to_string(),
        // The agent took it at a step boundary in the meantime.
        None => return err(StatusCode::NOT_FOUND, "that message was already sent"),
    }
    drop(inbox);
    Json(queued_json(&s.env)).into_response()
}

/// Withdraw a queued prompt. Idempotent: a message the agent already took is
/// simply no longer in the returned list.
async fn remove_queued(
    State(st): State<AppState>,
    Path((id, qid)): Path<(String, String)>,
) -> Response {
    let s = match session(&st, &id) {
        Ok(s) => s,
        Err(e) => return e,
    };
    s.env.inbox.lock().unwrap().retain(|q| q.id != qid);
    Json(queued_json(&s.env)).into_response()
}

async fn events(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let s = match session(&st, &id) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let rx = s.tx.subscribe();
    // Replay requests still waiting for an answer (sent before this client connected).
    let open: Vec<Value> = s
        .pending
        .lock()
        .unwrap()
        .values()
        .map(|(_, ev)| ev.clone())
        .collect();
    let replay = futures::stream::iter(
        open.into_iter()
            .map(|v| Ok::<_, std::convert::Infallible>(SseEvent::default().data(v.to_string()))),
    );
    let live = futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(v) => {
                    return Some((
                        Ok::<_, std::convert::Infallible>(SseEvent::default().data(v.to_string())),
                        rx,
                    ));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(futures::StreamExt::chain(replay, live))
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn respond(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let s = match session(&st, &id) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let Some(rid) = body["request_id"].as_str().or_else(|| body["id"].as_str()) else {
        return err(
            StatusCode::BAD_REQUEST,
            "`request_id` or tool-call `id` is required",
        );
    };
    let mut pending = s.pending.lock().unwrap();
    if pending
        .get(rid)
        .is_some_and(|(_, ev)| ev["type"] == "tool_call")
        && (!body["result"]["content"].is_string() || !body["result"]["is_error"].is_boolean())
    {
        return err(
            StatusCode::BAD_REQUEST,
            "tool result requires string `content` and boolean `is_error`",
        );
    }
    match pending.remove(rid) {
        Some((tx, _)) => {
            let _ = tx.send(body.clone());
            Json(json!({"ok": true})).into_response()
        }
        None => err(StatusCode::NOT_FOUND, format!("no pending request {rid}")),
    }
}

async fn interrupt(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let s = match session(&st, &id) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if let Some(c) = s.cancel.lock().unwrap().as_ref() {
        c.cancel();
    }
    // Prompts from the cancelled turn are dropped by its tools; background
    // agents' prompts stay answerable. Prune ones nobody waits on.
    s.pending
        .lock()
        .unwrap()
        .retain(|_, (tx, _)| !tx.is_closed());
    Json(json!({"ok": true})).into_response()
}

/// Ends a live session. `?purge=true` also deletes its saved transcript.
async fn delete_session(
    State(st): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let purge = q.get("purge").is_some_and(|v| v == "true" || v == "1");
    let s = st.sessions.lock().unwrap().remove(&id);
    if let Some(s) = &s {
        close_session(s).await;
        // Wait for cancellation and the last save before removing the transcript.
        let _agent = s.agent.lock().await;
    }
    let cwd = project_dir(&st, &q);
    let saved = purge
        .then(|| {
            crate::session::Session::list(&cwd)
                .into_iter()
                .find(|s| s.id == id)
        })
        .flatten();
    match (&s, saved) {
        (None, None) => return err(StatusCode::NOT_FOUND, format!("no session {id}")),
        (_, Some(saved)) => {
            // The transcript and its rewind snapshots, as `chanto sessions delete`.
            if let Err(e) = crate::session::Session::delete(&saved.cwd, &saved.id) {
                return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
            }
        }
        _ => {}
    }
    Json(json!({"ok": true})).into_response()
}

/// Stop the session's turn, background agents, shells and browser.
async fn close_session(s: &Session) {
    if let Some(c) = s.cancel.lock().unwrap().as_ref() {
        c.cancel();
    }
    for b in s.env.bg_agents.lock().unwrap().iter() {
        b.cancel.cancel();
    }
    s.env.shutdown().await;
}

/// The file extension for an uploaded recording's content type.
fn audio_ext(content_type: &str) -> &'static str {
    let t = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match t.as_str() {
        "audio/webm" | "video/webm" => "webm",
        "audio/ogg" | "application/ogg" => "ogg",
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" | "audio/aac" | "video/mp4" => "m4a",
        "audio/mpeg" | "audio/mp3" => "mp3",
        _ => "wav",
    }
}

/// Write an upload to a temp file (deleted afterwards) and transcribe it.
async fn transcribe_upload(
    backend: &crate::voice::Backend,
    bytes: &[u8],
    content_type: &str,
) -> Result<String> {
    let audio = crate::voice::TempAudio::new(audio_ext(content_type))?;
    std::fs::write(audio.path(), bytes)?;
    crate::voice::transcribe(backend, audio.path()).await
}

/// Voice settings come from the user's own config: a project can't choose
/// where audio is sent.
fn voice_backend() -> std::result::Result<crate::voice::Backend, String> {
    let cfg = crate::config::Config::load(&crate::config::home_dir(), false)
        .map(|(c, _)| c)
        .unwrap_or_default();
    crate::voice::backend(&cfg)
}

/// `GET /voice`: whether dictation works here, and how.
pub(crate) async fn voice_status() -> Response {
    match tokio::task::spawn_blocking(voice_backend)
        .await
        .unwrap_or_else(|e| Err(e.to_string()))
    {
        Ok(b) => Json(json!({"available": true, "backend": b.describe()})).into_response(),
        Err(e) => Json(json!({"available": false, "reason": e})).into_response(),
    }
}

/// `POST /voice/transcribe`: the request body is the recording (any format
/// ffmpeg reads; the content type names it). Replies `{"text": ...}`.
pub(crate) async fn voice_transcribe(
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if body.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "the request body must be the recorded audio",
        );
    }
    let backend = match tokio::task::spawn_blocking(voice_backend)
        .await
        .unwrap_or_else(|e| Err(e.to_string()))
    {
        Ok(b) => b,
        Err(e) => return err(StatusCode::SERVICE_UNAVAILABLE, e),
    };
    let ct = headers
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    match transcribe_upload(&backend, &body, ct).await {
        Ok(text) => Json(json!({"text": text, "backend": backend.describe()})).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

/// The largest visual piece the web UI may preview: a code block in a message.
const MAX_ARTIFACT_BYTES: usize = 2 << 20;

/// A preview is model-written HTML in a sandboxed, opaque-origin frame with no
/// network: inline scripts and styles only, so the data a chart needs travels
/// with its markup.
const ARTIFACT_CSP: &str = "sandbox allow-scripts; default-src 'none'; \
script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: blob:; \
font-src data:; media-src data: blob:; connect-src 'none'; base-uri 'none'; \
form-action 'none'";

/// Store the HTML of an HTML or SVG code block for the preview pane.
async fn create_artifact(State(st): State<AppState>, Json(body): Json<Value>) -> Response {
    let html = body["html"].as_str().unwrap_or_default();
    if html.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "html is required");
    }
    if html.len() > MAX_ARTIFACT_BYTES {
        return err(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "preview is larger than {} MB",
                MAX_ARTIFACT_BYTES / (1 << 20)
            ),
        );
    }
    let id = st.artifacts.lock().unwrap().insert(html.to_string());
    Json(json!({"id": id, "url": format!("/artifacts/{id}")})).into_response()
}

/// Serve a stored preview. The id is a capability (it never appears in a URL
/// the page did not create), so the frame doesn't need the bearer token.
async fn get_artifact(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let html = st.artifacts.lock().unwrap().pages.get(&id).cloned();
    match html {
        Some(html) => (
            [
                ("content-type", "text/html; charset=utf-8"),
                ("content-security-policy", ARTIFACT_CSP),
                ("referrer-policy", "no-referrer"),
                ("x-content-type-options", "nosniff"),
                ("cache-control", "no-store"),
            ],
            html,
        )
            .into_response(),
        None => err(StatusCode::NOT_FOUND, "no preview with that id"),
    }
}

fn router(
    cli: Cli,
    token: String,
    sessions: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    runs: Option<Arc<crate::runner::Scheduler>>,
) -> Router {
    let st = AppState {
        cli: Arc::new(cli),
        token: Arc::new(token),
        sessions,
        artifacts: Arc::new(Mutex::new(Artifacts::default())),
    };
    let hosts_runs = runs.is_some();
    let runs_api = runs
        .as_ref()
        .map(|s| {
            crate::runs::api::router::<AppState>(s.manager())
                .merge(crate::runner::router(s.clone()))
        })
        .unwrap_or_default();
    // Webhooks authenticate with their own per-job tokens and a preview is
    // fetched by a frame that can't send a header, so these routes sit outside
    // the bearer-token layer.
    let triggers = runs
        .map(|s| crate::runner::public_router::<AppState>(s, st.token.clone()))
        .unwrap_or_default();
    let artifacts = Router::new().route("/artifacts/{id}", get(get_artifact));
    Router::new()
        .route("/sessions", post(create_session).get(list_sessions))
        .route("/sessions/{id}", get(get_session).delete(delete_session))
        .route("/sessions/{id}/fork", post(fork_session))
        .route("/sessions/{id}/checkpoints", get(checkpoints))
        .route("/sessions/{id}/rewind", post(rewind_session))
        .route("/sessions/{id}/messages", post(send_message))
        .route("/sessions/{id}/events", get(events))
        .route("/sessions/{id}/respond", post(respond))
        .route("/sessions/{id}/interrupt", post(interrupt))
        .route("/sessions/{id}/queue", get(get_queue))
        .route(
            "/sessions/{id}/queue/{qid}",
            patch(edit_queued).delete(remove_queued),
        )
        .route(
            "/artifacts",
            post(create_artifact).layer(axum::extract::DefaultBodyLimit::max(
                MAX_ARTIFACT_BYTES + (64 << 10),
            )),
        )
        .route("/voice", get(voice_status))
        // Hosted endpoints take up to 25 MB; a long dictation is far less.
        .route(
            "/voice/transcribe",
            post(voice_transcribe).layer(axum::extract::DefaultBodyLimit::max(25 << 20)),
        )
        .merge(runs_api)
        .layer(axum::middleware::from_fn_with_state(st.clone(), auth))
        .merge(artifacts)
        .merge(triggers)
        .route(
            "/health",
            get(move || async move {
                Json(Health {
                    ok: true,
                    version: env!("CARGO_PKG_VERSION").into(),
                    runs: hosts_runs,
                })
            }),
        )
        .with_state(st)
}

const WEB_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; frame-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

pub(crate) fn web_router(app: Router) -> Router {
    app.route(
        "/",
        get(|| async {
            (
                [
                    ("content-type", "text/html; charset=utf-8"),
                    ("content-security-policy", WEB_CSP),
                    ("referrer-policy", "no-referrer"),
                    ("x-content-type-options", "nosniff"),
                    ("cache-control", "no-store"),
                ],
                include_str!("web/index.html"),
            )
        }),
    )
    .route(
        "/app.js",
        get(|| async {
            (
                [
                    ("content-type", "text/javascript; charset=utf-8"),
                    ("x-content-type-options", "nosniff"),
                ],
                include_str!("web/app.js"),
            )
        }),
    )
    .route(
        "/style.css",
        get(|| async {
            (
                [
                    ("content-type", "text/css; charset=utf-8"),
                    ("x-content-type-options", "nosniff"),
                ],
                include_str!("web/style.css"),
            )
        }),
    )
    .route(
        "/logo.svg",
        get(|| async {
            (
                [
                    ("content-type", "image/svg+xml"),
                    ("x-content-type-options", "nosniff"),
                    ("cache-control", "public, max-age=86400"),
                ],
                include_str!("../assets/logo.svg"),
            )
        }),
    )
}

/// Where clients find the running server.
pub fn server_file() -> std::path::PathBuf {
    crate::config::home_dir().join("server.json")
}

/// Record where this server is. A program's private server (the SDKs'
/// `start`) writes nothing, so it can't take over the file that says where
/// the user's own server is.
fn write_server_file(
    path: &std::path::Path,
    info: &serde_json::Value,
    private: bool,
) -> anyhow::Result<()> {
    if private {
        return Ok(());
    }
    let _ = std::fs::remove_file(path);
    // Owner-only from the start: the file holds the bearer token.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    std::io::Write::write_all(&mut opts.open(path)?, &serde_json::to_vec_pretty(info)?)?;
    Ok(())
}

/// Remove the file on shutdown, unless a private server never wrote it.
fn remove_server_file(path: &std::path::Path, private: bool) {
    if !private {
        let _ = std::fs::remove_file(path);
    }
}

/// The first IPv4 literal in `s` (skips the CLI's version warning noise).
fn parse_first_ipv4(s: &str) -> Option<Ipv4Addr> {
    s.split_whitespace()
        .find_map(|t| t.parse::<Ipv4Addr>().ok())
}

/// A human explanation for a Tailscale backend state.
fn tailscale_reason(state: &str) -> String {
    match state {
        "NeedsLogin" => {
            "Tailscale is not logged in — run `tailscale up` and approve in the browser".into()
        }
        "Stopped" => "Tailscale is stopped — run `tailscale up`".into(),
        "Starting" => "Tailscale is still starting — retry in a moment".into(),
        "" | "NoState" => "Tailscale doesn't appear to be running — run `tailscale up`".into(),
        other => format!("Tailscale is not up (state: {other}) — run `tailscale up`"),
    }
}

fn tailscale_status() -> Option<Value> {
    let out = std::process::Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .ok()?;
    serde_json::from_slice(&out.stdout).ok()
}

/// The `BackendState` from `tailscale status --json`, if the CLI is present.
fn tailscale_state() -> Option<String> {
    tailscale_status()?
        .get("BackendState")?
        .as_str()
        .map(str::to_string)
}

/// This machine's MagicDNS name (no trailing dot), when known.
pub(crate) fn tailscale_dns_name() -> Option<String> {
    let dns = tailscale_status()?
        .get("Self")?
        .get("DNSName")?
        .as_str()?
        .trim_end_matches('.')
        .to_string();
    (!dns.is_empty()).then_some(dns)
}

/// The local Tailscale IPv4, e.g. `100.101.102.103`.
///
/// Binding to this rather than `0.0.0.0` keeps the API off the LAN: only devices
/// already on your tailnet can reach it, over WireGuard, with Tailscale's own
/// identity check in front of our bearer token.
pub fn tailscale_ipv4() -> std::result::Result<Ipv4Addr, String> {
    let out = match std::process::Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
    {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(
                "the `tailscale` CLI was not found on PATH (install it, or use --host)".into(),
            );
        }
        Err(e) => return Err(format!("could not run `tailscale ip -4`: {e}")),
        Ok(o) => o,
    };
    // The IP is on stdout when up; the reason lands on stderr (or, when logged
    // out, an explanatory line on stdout), so look at both.
    let ip = parse_first_ipv4(&String::from_utf8_lossy(&out.stdout))
        .or_else(|| parse_first_ipv4(&String::from_utf8_lossy(&out.stderr)));
    if let Some(ip) = ip {
        return Ok(ip);
    }
    Err(tailscale_reason(&tailscale_state().unwrap_or_default()))
}

/// Background runs are hosted on loopback servers (the local daemon), and
/// beyond this machine only with `--runs`.
fn hosts_runs(cli: &Cli, bind_host: &str) -> bool {
    cli.runs
        || bind_host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
        || bind_host == "localhost"
}

/// The pinned bearer token: `CHANTO_SERVER_TOKEN`, or the contents of the file
/// named by `CHANTO_SERVER_TOKEN_FILE` (for systemd `LoadCredential=` and
/// Docker/Compose secrets, so the token never sits in the environment).
/// Unset or unreadable falls back to a fresh random token.
fn server_token() -> Option<String> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if let Some(t) = var("CHANTO_SERVER_TOKEN") {
        return Some(t);
    }
    let path = var("CHANTO_SERVER_TOKEN_FILE")?;
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub async fn run(mut cli: Cli, host: &str, port: u16, web: bool, tailscale: bool) -> Result<()> {
    cli.no_trust_prompt = true;
    if let Some(cwd) = &cli.cwd {
        cli.cwd = Some(crate::util::canonical(cwd)?);
    }
    let token = server_token().unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    // `--tailscale` binds to this machine's tailnet address so the API is
    // reachable from your own devices and nothing else.
    let tailnet_ip = if tailscale {
        Some(tailscale_ipv4().map_err(|e| anyhow::anyhow!("--tailscale: {e}"))?)
    } else {
        None
    };
    let bind_host = tailnet_ip
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| host.to_string());
    let listener = tokio::net::TcpListener::bind((bind_host.as_str(), port)).await?;
    let addr = listener.local_addr()?;
    let url = format!("http://{addr}");
    let info = json!({
        "url": url,
        "token": token,
        "pid": std::process::id(),
        "cwd": std::env::current_dir()?,
        "tailscale": tailnet_ip.is_some(),
    });
    std::fs::create_dir_all(crate::config::home_dir())?;
    let path = server_file();
    let private = std::env::var_os("CHANTO_PRIVATE_SERVER").is_some();
    write_server_file(&path, &info, private)?;
    eprintln!(
        "chanto server listening on {url}\ntoken: {token}\n(connection details written to {})",
        path.display()
    );
    if let Some(ip) = tailnet_ip {
        let host = tailscale_dns_name().unwrap_or_else(|| ip.to_string());
        eprintln!(
            "Tailscale: reachable on your tailnet at http://{host}:{}",
            addr.port()
        );
    }
    let sessions: Arc<Mutex<HashMap<String, Arc<Session>>>> = Default::default();
    let no_open = cli.no_open;
    let runs = hosts_runs(&cli, &bind_host).then(|| {
        let exe = std::env::current_exe().unwrap_or_else(|_| "chanto".into());
        let home = crate::config::home_dir();
        let cfg = crate::config::Config::load(&home, false)
            .map(|(c, _)| c)
            .unwrap_or_default();
        let m = crate::runs::manager::Manager::open(
            crate::runs::Store::new(crate::runs::Store::default_dir()),
            crate::runs::manager::Launcher::Process(exe),
            cfg.background.max_parallel.unwrap_or(2),
        );
        m.max_bundle_bytes.store(
            (cfg.background.max_bundle_mb.unwrap_or(1024) as usize) << 20,
            std::sync::atomic::Ordering::Relaxed,
        );
        // Desktop notifications by default only for the local daemon; links
        // point at this server's web UI (the tailnet name with --tailscale).
        let local = addr.ip().is_loopback();
        let link = match tailnet_ip {
            Some(ip) => format!(
                "http://{}:{}",
                tailscale_dns_name().unwrap_or_else(|| ip.to_string()),
                addr.port()
            ),
            None => url.clone(),
        };
        m.set_notifier(crate::runs::notify::Notifier::new(
            cfg.notify.clone(),
            local,
            web.then_some(link),
        ));
        m
    });
    // Scheduled jobs (`chanto runner`) fire wherever runs are hosted.
    let jobs = runs.clone().map(|m| {
        let s = crate::runner::Scheduler::new(m);
        s.start();
        s
    });
    let app = router(cli, token.clone(), sessions.clone(), jobs);
    let app = if web { web_router(app) } else { app };
    if web {
        let launch = format!("{url}/#token={}", urlencoding::encode(&token));
        eprintln!("Web UI: {launch}");
        if !no_open {
            crate::mcp_auth::open_browser(&launch);
        }
    }
    let res = axum::serve(listener, app).with_graceful_shutdown(async move {
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("signal handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        // Runs save their sessions and are recorded as interrupted; their
        // event streams end so open connections don't hold up shutdown.
        if let Some(m) = runs {
            m.close().await;
        }
    });
    let r = res.await;
    let all: Vec<Arc<Session>> = sessions.lock().unwrap().drain().map(|(_, s)| s).collect();
    for s in all {
        close_session(&s).await;
    }
    remove_server_file(&path, private);
    r?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::EnvInit;
    use crate::agent::tests::{mock_server, tc};
    use crate::llm::{Provider, ProviderKind};
    use crate::tools::{Ctx, Registry, Tool};

    #[test]
    fn private_server_leaves_the_users_server_file_alone() {
        let dir = std::env::temp_dir().join(format!("chanto-server-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.json");
        std::fs::write(&path, "the user's server").unwrap();

        write_server_file(&path, &json!({"url": "private"}), true).unwrap();
        remove_server_file(&path, true);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "the user's server");

        // A normal server replaces the file, then removes it on shutdown.
        write_server_file(&path, &json!({"url": "normal"}), false).unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["url"], "normal");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        remove_server_file(&path, false);
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn request_events_and_replay_mask_secrets() {
        let (tx, mut rx) = broadcast::channel(8);
        let frontend = ServerFrontend {
            tx,
            pending: Default::default(),
        };
        // Token-shaped, assembled at runtime so the source holds no such literal.
        let secret = &format!("{}_{}", "ghp", "abcdefghijklmnopqrstuvwxyz0123456789");
        let secret = secret.as_str();
        for kind in ["permission_request", "question"] {
            let ask = frontend.ask(
                |request_id| json!({"type": kind, "request_id": request_id, "detail": secret}),
            );
            tokio::pin!(ask);
            tokio::select! {
                _ = &mut ask => panic!("request should wait for an answer"),
                ev = rx.recv() => {
                    let ev = ev.unwrap();
                    assert!(!ev.to_string().contains(secret));
                    assert!(ev.to_string().contains("REDACTED"));
                    let pending = frontend.pending.lock().unwrap();
                    assert!(!pending.values().next().unwrap().1.to_string().contains(secret));
                }
            }
        }
    }

    #[tokio::test]
    async fn permission_requests_and_questions_reach_the_client_and_come_back() {
        let (tx, mut rx) = broadcast::channel(16);
        let frontend = Arc::new(ServerFrontend {
            tx,
            pending: Default::default(),
        });
        // The client: answer each request by its id, as POST /respond does.
        let answer = |frontend: Arc<ServerFrontend>, reply: Value| async move {
            let ev = rx_next(&frontend).await;
            let id = ev["request_id"].as_str().unwrap().to_string();
            let (sender, _) = frontend.pending.lock().unwrap().remove(&id).unwrap();
            sender.send(reply).unwrap();
            ev
        };
        async fn rx_next(frontend: &ServerFrontend) -> Value {
            loop {
                if let Some((_, (_, ev))) = frontend.pending.lock().unwrap().iter().next() {
                    return ev.clone();
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
        let req = PermissionRequest {
            tool: "Bash".into(),
            label: "Bash(ls)".into(),
            detail: "ls".into(),
            always_label: "ls".into(),
            reason: Some("lists files".into()),
            proposed: None,
            auto_deny: None,
        };
        let client = tokio::spawn(answer(
            frontend.clone(),
            json!({"decision": "allow_always"}),
        ));
        assert_eq!(frontend.permission(req).await, PermissionReply::AllowAlways);
        let ev = client.await.unwrap();
        assert_eq!(ev["type"], "permission_request");
        assert_eq!(ev["tool"], "Bash");
        assert_eq!(ev["reason"], "lists files");
        assert_eq!(ev["always_label"], "ls");
        assert_eq!(rx.recv().await.unwrap()["type"], "permission_request");

        let options = vec!["red".to_string(), "blue".to_string()];
        let client = tokio::spawn(answer(frontend.clone(), json!({"answer": "blue"})));
        assert_eq!(
            frontend.question("Which?", &options).await.as_deref(),
            Some("blue")
        );
        let ev = client.await.unwrap();
        assert_eq!(ev["type"], "question");
        assert_eq!(ev["options"], json!(["red", "blue"]));
    }

    #[test]
    fn the_documented_session_body_passes_validation() {
        // `CreateSession` is what sdk/schema.json and the SDKs' types say the
        // route takes: every field of it, filled in, has to be accepted.
        let body = crate::wire::CreateSession {
            model: Some("m".into()),
            mode: Some(crate::wire::Mode::AcceptEdits),
            resume: Some("abc".into()),
            cwd: Some("/w".into()),
            system_prompt_append: Some("".into()),
            allowed_tools: Some(vec!["Read".into()]),
            disallowed_tools: Some(vec!["Bash".into()]),
            max_turns: Some(3),
            verify: Some(true),
            tools: Some(vec![crate::wire::ToolSpec {
                name: "lookup".into(),
                description: "Look something up.".into(),
                input_schema: json!({"type": "object", "properties": {}}),
            }]),
        };
        let body = serde_json::to_value(body).unwrap();
        assert_eq!(validate_options(&body), Ok(()), "{body}");
    }

    #[test]
    fn bearer_scheme_is_case_insensitive() {
        for h in [
            "Bearer abc",
            "bearer abc",
            "BEARER abc",
            "Bearer  abc ",
            " bEaReR abc",
        ] {
            assert_eq!(bearer_token(h), Some("abc"), "{h:?}");
        }
        for h in ["Basic abc", "Bearer", "Bearer ", "Bearerabc", "abc"] {
            assert_eq!(bearer_token(h), None, "{h:?}");
        }
    }

    #[test]
    fn server_token_prefers_env_then_reads_file() {
        let _g = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let path = std::env::temp_dir().join(format!("chanto-server-token-{}", std::process::id()));
        std::fs::write(&path, "file-token\n").unwrap();
        let prev_t = std::env::var_os("CHANTO_SERVER_TOKEN");
        let prev_f = std::env::var_os("CHANTO_SERVER_TOKEN_FILE");
        unsafe {
            std::env::remove_var("CHANTO_SERVER_TOKEN");
            std::env::set_var("CHANTO_SERVER_TOKEN_FILE", &path);
        }
        // Trailing whitespace from the file is trimmed.
        assert_eq!(server_token().as_deref(), Some("file-token"));
        unsafe { std::env::set_var("CHANTO_SERVER_TOKEN", "env-token") };
        assert_eq!(server_token().as_deref(), Some("env-token"));
        unsafe {
            match prev_t {
                Some(v) => std::env::set_var("CHANTO_SERVER_TOKEN", v),
                None => std::env::remove_var("CHANTO_SERVER_TOKEN"),
            }
            match prev_f {
                Some(v) => std::env::set_var("CHANTO_SERVER_TOKEN_FILE", v),
                None => std::env::remove_var("CHANTO_SERVER_TOKEN_FILE"),
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tailscale_ip_is_read_past_cli_noise() {
        // `tailscale ip -4` prints only the address when up.
        assert_eq!(
            parse_first_ipv4("100.101.102.103\n"),
            Some(Ipv4Addr::new(100, 101, 102, 103))
        );
        // Version mismatches are printed with the answer; the address still wins.
        assert_eq!(
            parse_first_ipv4("Warning: client version \"x\" != \"y\"\n100.64.0.5\n"),
            Some(Ipv4Addr::new(100, 64, 0, 5))
        );
        // No address when logged out.
        assert_eq!(
            parse_first_ipv4("Warning: client version \"1.102\" != \"1.98\"\n"),
            None
        );
        assert_eq!(parse_first_ipv4(""), None);
    }

    #[test]
    fn tailscale_states_get_actionable_reasons() {
        assert!(tailscale_reason("NeedsLogin").contains("tailscale up"));
        assert!(tailscale_reason("Stopped").contains("tailscale up"));
        assert!(tailscale_reason("NoState").contains("tailscale up"));
        assert!(tailscale_reason("SomethingElse").contains("SomethingElse"));
    }

    #[test]
    fn artifacts_keep_the_newest_previews() {
        let mut a = Artifacts::default();
        let first = a.insert("one".into());
        for i in 0..Artifacts::CAPACITY {
            a.insert(format!("{i}"));
        }
        assert_eq!(a.pages.len(), Artifacts::CAPACITY);
        assert!(!a.pages.contains_key(&first));
        let last = a.insert("last".into());
        assert_eq!(a.pages.len(), Artifacts::CAPACITY);
        assert_eq!(a.pages[&last], "last");
    }

    /// The transcript, checkpoint and queue endpoints show what the web UI
    /// shows, so secrets in the history are masked there too.
    #[tokio::test]
    async fn transcript_checkpoint_and_queue_endpoints_mask_secrets() {
        use crate::llm::{Message, ToolCall};
        let _g = crate::secrets::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = crate::util::canonical(crate::agent::tests::tmpdir("server-mask")).unwrap();
        crate::secrets::init(&crate::config::Config::default(), &dir);
        // Token-shaped, assembled at runtime so the source holds no such literal.
        let secret = &format!("{}_{}", "ghp", "abcdefghijklmnopqrstuvwxyz0123456789");
        let secret = secret.as_str();
        let (tx, _) = broadcast::channel(8);
        let pending: Pending = Default::default();
        let env = Env::new(EnvInit {
            config: crate::config::Config::default(),
            provider: Provider::new(ProviderKind::Openai, "http://127.0.0.1:9", None, 32768),
            model: "mock".into(),
            mode: crate::permissions::Mode::Bypass,
            vision: false,
            cwd: dir,
            tools: Registry::builtin(),
            mcp: vec![],
            frontend: Arc::new(ServerFrontend {
                tx: tx.clone(),
                pending: pending.clone(),
            }),
            interactive: false,
        });
        let mut agent = Agent::new(env.clone());
        let mut call = Message::assistant("");
        call.tool_calls.push(ToolCall {
            id: "c1".into(),
            name: "Bash".into(),
            arguments: json!({"command": format!("gh auth login --with-token {secret}")}),
        });
        agent.messages = vec![
            Message::system("sys"),
            Message::user(format!("use {secret}")),
            call,
        ];
        *env.checkpoints.lock().unwrap() = vec![crate::agent::Checkpoint {
            id: "a".into(),
            timestamp: "t1".into(),
            tree: None,
            msg_index: 1,
            prompt: format!("use {secret}"),
            files: Default::default(),
        }];
        env.inbox
            .lock()
            .unwrap()
            .push(Queued::new(format!("later {secret}")));
        let s = Arc::new(Session {
            options: json!({}),
            agent: Arc::new(AsyncMutex::new(agent)),
            env: env.clone(),
            tx,
            pending,
            cancel: Mutex::new(None),
        });
        let st = AppState {
            cli: Arc::new(<Cli as clap::Parser>::parse_from(["chanto"])),
            token: Arc::new("test".into()),
            sessions: Arc::new(Mutex::new(HashMap::from([("test".into(), s)]))),
            artifacts: Arc::new(Mutex::new(Artifacts::default())),
        };
        let id = || Path("test".to_string());
        for (what, body) in [
            (
                "transcript",
                body_json(get_session(State(st.clone()), id()).await).await,
            ),
            (
                "checkpoints",
                body_json(checkpoints(State(st.clone()), id()).await).await,
            ),
            (
                "queue",
                body_json(get_queue(State(st.clone()), id()).await).await,
            ),
        ] {
            let text = body.to_string();
            assert!(
                !text.contains(secret) && text.contains("[REDACTED:GITHUB_TOKEN]"),
                "{what}: {text}"
            );
        }
    }

    #[tokio::test]
    async fn queued_messages_can_be_listed_edited_and_removed() {
        let dir = crate::util::canonical(crate::agent::tests::tmpdir("server-queue")).unwrap();
        let (tx, _) = broadcast::channel(8);
        let pending: Pending = Default::default();
        let env = Env::new(EnvInit {
            config: crate::config::Config::default(),
            provider: Provider::new(ProviderKind::Openai, "http://127.0.0.1:9", None, 32768),
            model: "mock".into(),
            mode: crate::permissions::Mode::Bypass,
            vision: false,
            cwd: dir,
            tools: Registry::builtin(),
            mcp: vec![],
            frontend: Arc::new(ServerFrontend {
                tx: tx.clone(),
                pending: pending.clone(),
            }),
            interactive: false,
        });
        let s = Arc::new(Session {
            options: json!({}),
            agent: Arc::new(AsyncMutex::new(Agent::new(env.clone()))),
            env: env.clone(),
            tx,
            pending,
            cancel: Mutex::new(None),
        });
        let st = AppState {
            cli: Arc::new(<Cli as clap::Parser>::parse_from(["chanto"])),
            token: Arc::new("test".into()),
            sessions: Arc::new(Mutex::new(HashMap::from([("test".into(), s)]))),
            artifacts: Arc::new(Mutex::new(Artifacts::default())),
        };
        let path = |qid: &str| Path(("test".to_string(), qid.to_string()));
        let id = || Path("test".to_string());
        env.inbox.lock().unwrap().push(Queued::new("first"));
        env.inbox.lock().unwrap().push(Queued::new("second"));
        let queue = body_json(get_queue(State(st.clone()), id()).await).await;
        assert_eq!(queue["queued"][0]["text"], "first");
        assert_eq!(queue["queued"][1]["text"], "second");
        let qid = queue["queued"][0]["id"].as_str().unwrap().to_string();
        let edited = body_json(
            edit_queued(
                State(st.clone()),
                path(&qid),
                Json(json!({"text": "first, edited"})),
            )
            .await,
        )
        .await;
        assert_eq!(edited["queued"][0]["text"], "first, edited");
        assert_eq!(
            edit_queued(State(st.clone()), path(&qid), Json(json!({"text": "  "})),)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            edit_queued(State(st.clone()), path("gone"), Json(json!({"text": "x"})),)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        // Withdrawing is idempotent: taking one the model already has changes nothing.
        let left = body_json(remove_queued(State(st.clone()), path(&qid)).await).await;
        assert_eq!(left["queued"].as_array().unwrap().len(), 1);
        assert_eq!(left["queued"][0]["text"], "second");
        let again = body_json(remove_queued(State(st), path(&qid)).await).await;
        assert_eq!(again["queued"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn immediate_commands_run_while_a_turn_is_running() {
        let dir = crate::util::canonical(crate::agent::tests::tmpdir("server-immediate")).unwrap();
        let (tx, _) = broadcast::channel(8);
        let pending: Pending = Default::default();
        let env = Env::new(EnvInit {
            config: crate::config::Config::default(),
            provider: Provider::new(ProviderKind::Openai, "http://127.0.0.1:9", None, 32768),
            model: "mock".into(),
            mode: crate::permissions::Mode::Bypass,
            vision: false,
            cwd: dir,
            tools: Registry::builtin(),
            mcp: vec![],
            frontend: Arc::new(ServerFrontend {
                tx: tx.clone(),
                pending: pending.clone(),
            }),
            interactive: false,
        });
        let s = Arc::new(Session {
            options: json!({}),
            agent: Arc::new(AsyncMutex::new(Agent::new(env.clone()))),
            env: env.clone(),
            tx,
            pending,
            cancel: Mutex::new(Some(CancellationToken::new())),
        });
        let st = AppState {
            cli: Arc::new(<Cli as clap::Parser>::parse_from(["chanto"])),
            token: Arc::new("test".into()),
            sessions: Arc::new(Mutex::new(HashMap::from([("test".into(), s)]))),
            artifacts: Arc::new(Mutex::new(Artifacts::default())),
        };
        let id = || Path("test".to_string());
        // `/mcp` lists now instead of waiting behind the running turn.
        let out =
            body_json(send_message(State(st.clone()), id(), Json(json!({"text": "/mcp"}))).await)
                .await;
        assert!(out["output"].as_str().unwrap().contains("No MCP servers"));
        assert!(env.inbox.lock().unwrap().is_empty());
        // A prompt still queues for the model's next step.
        let out =
            body_json(send_message(State(st), id(), Json(json!({"text": "keep going"}))).await)
                .await;
        assert_eq!(out["queued"], true);
        assert_eq!(env.inbox.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn web_page_and_api_auth() {
        let cli = <Cli as clap::Parser>::parse_from(["chanto", "web", "--no-open"]);
        let store_dir = std::env::temp_dir().join(format!(
            "chanto-runs-auth-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let runs = crate::runs::manager::Manager::open(
            crate::runs::Store::new(store_dir.clone()),
            crate::runs::manager::Launcher::InProcess,
            1,
        );
        let app = web_router(router(
            cli,
            "secret".into(),
            Default::default(),
            Some(crate::runner::Scheduler::new(runs)),
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = crate::http::client();
        let page = client.get(format!("{base}/")).send().await.unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        assert_eq!(page.headers()["content-security-policy"], WEB_CSP);
        let html = page.text().await.unwrap();
        assert!(html.contains("/app.js"));
        // The project picker and the mic button; the desktop shell's hooks.
        assert!(html.contains("id=\"open-project\"") && html.contains("id=\"mic\""));
        let js = client
            .get(format!("{base}/app.js"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(js.contains("window.chanto = {") && js.contains("'/voice/transcribe'"));
        for asset in ["app.js", "style.css"] {
            assert_eq!(
                client
                    .get(format!("{base}/{asset}"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::OK
            );
        }
        for (method, path) in [
            (reqwest::Method::GET, "/sessions"),
            (reqwest::Method::POST, "/sessions"),
            (reqwest::Method::GET, "/sessions/missing"),
            (reqwest::Method::GET, "/sessions/missing/events"),
            (reqwest::Method::POST, "/sessions/missing/messages"),
            (reqwest::Method::POST, "/sessions/missing/fork"),
            (reqwest::Method::GET, "/sessions/missing/checkpoints"),
            (reqwest::Method::POST, "/sessions/missing/rewind"),
            (reqwest::Method::POST, "/sessions/missing/respond"),
            (reqwest::Method::POST, "/sessions/missing/interrupt"),
            (reqwest::Method::GET, "/sessions/missing/queue"),
            (reqwest::Method::PATCH, "/sessions/missing/queue/q1"),
            (reqwest::Method::DELETE, "/sessions/missing/queue/q1"),
            (reqwest::Method::DELETE, "/sessions/missing"),
            (reqwest::Method::GET, "/runs"),
            (reqwest::Method::POST, "/runs"),
            (reqwest::Method::GET, "/runs/missing"),
            (reqwest::Method::GET, "/runs/missing/events"),
            (reqwest::Method::POST, "/runs/missing/respond"),
            (reqwest::Method::POST, "/runs/missing/stop"),
            (reqwest::Method::POST, "/runs/missing/resume"),
            (reqwest::Method::DELETE, "/runs/missing"),
            (reqwest::Method::GET, "/runs/jobs"),
            (reqwest::Method::POST, "/runs/jobs/due"),
            (reqwest::Method::POST, "/runs/jobs/missing/trigger"),
            (reqwest::Method::GET, "/voice"),
            (reqwest::Method::POST, "/voice/transcribe"),
            (reqwest::Method::POST, "/artifacts"),
        ] {
            for token in [None, Some("wrong")] {
                let mut req = client.request(method.clone(), format!("{base}{path}"));
                if let Some(token) = token {
                    req = req.bearer_auth(token);
                }
                assert_eq!(
                    req.send().await.unwrap().status(),
                    StatusCode::UNAUTHORIZED,
                    "{path}"
                );
            }
        }
        assert_eq!(
            client
                .get(format!("{base}/sessions/missing"))
                .bearer_auth("secret")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        // Voice uploads need the token and a body; unauthenticated audio is never read.
        let audio = client
            .post(format!("{base}/voice/transcribe"))
            .header("content-type", "audio/webm")
            .body(vec![0u8; 64])
            .send()
            .await
            .unwrap();
        assert_eq!(audio.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            client
                .post(format!("{base}/voice/transcribe"))
                .bearer_auth("secret")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        // The runs API, with the token.
        let health: Value = client
            .get(format!("{base}/health"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(health["runs"], true);
        let list = client
            .get(format!("{base}/runs"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap();
        assert_eq!(list.status(), StatusCode::OK);
        assert_eq!(list.json::<Value>().await.unwrap()["runs"], json!([]));
        assert_eq!(
            client
                .get(format!("{base}/runs/missing"))
                .bearer_auth("secret")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        let bad = client
            .post(format!("{base}/runs"))
            .bearer_auth("secret")
            .json(&json!({"task": ""}))
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
        // Visual previews: stored with the token, fetched by the frame without
        // one, sandboxed and offline, and gone once the store forgets them.
        let created = client
            .post(format!("{base}/artifacts"))
            .bearer_auth("secret")
            .json(&json!({"html": "<h1>chart</h1><script>document.title='x'</script>"}))
            .send()
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::OK);
        let created: Value = created.json().await.unwrap();
        let url = created["url"].as_str().unwrap().to_string();
        assert_eq!(
            url,
            format!("/artifacts/{}", created["id"].as_str().unwrap())
        );
        let piece = client.get(format!("{base}{url}")).send().await.unwrap();
        assert_eq!(piece.status(), StatusCode::OK);
        assert_eq!(piece.headers()["content-security-policy"], ARTIFACT_CSP);
        assert_eq!(piece.headers()["cache-control"], "no-store");
        assert!(piece.text().await.unwrap().contains("<h1>chart</h1>"));
        let empty = client
            .post(format!("{base}/artifacts"))
            .bearer_auth("secret")
            .json(&json!({"html": "  "}))
            .send()
            .await
            .unwrap();
        assert_eq!(empty.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            client
                .get(format!("{base}/artifacts/missing"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        task.abort();
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// The app mark is served for the tab icon and the welcome badge.
    #[tokio::test]
    async fn web_page_serves_the_app_mark() {
        let app = web_router(Router::new());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let res = reqwest::get(format!("{base}/logo.svg")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["content-type"], "image/svg+xml");
        assert!(res.text().await.unwrap().contains("<svg"));
        task.abort();
    }

    /// The web UI's script parses (when node is installed to check it).
    #[test]
    fn web_app_script_parses() {
        let Some(node) = crate::shell::find_on_path("node") else {
            return;
        };
        let out = std::process::Command::new(node)
            .args([
                "--check",
                concat!(env!("CARGO_MANIFEST_DIR"), "/src/web/app.js"),
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn upload_types_map_to_extensions() {
        assert_eq!(audio_ext("audio/webm;codecs=opus"), "webm");
        assert_eq!(audio_ext("audio/mp4"), "m4a");
        assert_eq!(audio_ext("audio/ogg; codecs=opus"), "ogg");
        assert_eq!(audio_ext(""), "wav");
    }

    #[tokio::test]
    async fn uploads_are_transcribed_and_deleted() {
        // The hosted backend against a local mock that records the file name.
        let names = Arc::new(Mutex::new(Vec::<String>::new()));
        let n2 = names.clone();
        let app = Router::new().route(
            "/v1/audio/transcriptions",
            post(move |body: axum::body::Bytes| {
                let names = n2.clone();
                async move {
                    let text = String::from_utf8_lossy(&body).to_string();
                    names.lock().unwrap().push(
                        text.split("filename=\"")
                            .nth(1)
                            .and_then(|r| r.split('"').next())
                            .unwrap_or("")
                            .to_string(),
                    );
                    Json(json!({"text": "run the tests"}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let backend = crate::voice::Backend::Hosted(crate::voice::HostedBackend {
            url: format!("{base}/v1/audio/transcriptions"),
            key: crate::voice::Key::None,
            model: "m".into(),
            language: None,
        });
        assert_eq!(
            transcribe_upload(&backend, b"OggS", "audio/ogg")
                .await
                .unwrap(),
            "run the tests"
        );
        assert_eq!(names.lock().unwrap().as_slice(), ["audio.ogg"]);
        // A local backend sees the temp file, which is gone afterwards.
        #[cfg(unix)]
        {
            let dir = std::env::temp_dir().join(format!(
                "chanto-voice-bin-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let bin = dir.join("whisper-cli");
            std::fs::write(&bin, "#!/bin/sh\ntest -s \"$4\" && echo \"$4\"\n").unwrap();
            std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
            // A file just written can briefly be "busy" for exec while another
            // test's fork still holds it open (ETXTBSY on Linux).
            for _ in 0..50 {
                match std::process::Command::new(&bin).status() {
                    Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                        std::thread::sleep(std::time::Duration::from_millis(20))
                    }
                    _ => break,
                }
            }
            let local = crate::voice::Backend::Local {
                bin,
                model: "m.bin".into(),
                python: false,
                language: None,
            };
            let path = transcribe_upload(&local, b"RIFF....WAVE", "audio/wav")
                .await
                .unwrap();
            let name = std::path::Path::new(&path).file_name().unwrap();
            assert!(name.to_string_lossy().starts_with("chanto-voice-"));
            assert_eq!(
                std::path::Path::new(&path).extension(),
                Some("wav".as_ref())
            );
            assert!(!std::path::Path::new(&path).exists());
            std::fs::remove_dir_all(dir).unwrap();
        }
        task.abort();
    }

    #[test]
    fn session_option_validation() {
        for body in [
            json!({"verify": "yes"}),
            json!({"max_turns": 0}),
            json!({"mode": "wrong"}),
            json!({"allowed_tools": "Read"}),
            json!({"tools": [{"name": "read"}]}),
            json!({"cwd": false}),
        ] {
            assert!(validate_options(&body).is_err(), "{body}");
        }
        assert!(
            validate_options(&json!({"verify": false, "allowed_tools": [], "max_turns": 1}))
                .is_ok()
        );
    }

    async fn body_json(r: Response) -> Value {
        serde_json::from_slice(
            &axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn checkpoints_list_and_rewind() {
        use crate::agent::Checkpoint;
        use crate::llm::Message;
        let dir = crate::util::canonical(crate::agent::tests::tmpdir("server-rewind")).unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, "v2\n").unwrap();
        let (tx, _) = broadcast::channel(8);
        let pending: Pending = Default::default();
        let env = Env::new(EnvInit {
            config: crate::config::Config::default(),
            provider: Provider::new(ProviderKind::Openai, "http://127.0.0.1:9", None, 32768),
            model: "mock".into(),
            mode: crate::permissions::Mode::Bypass,
            vision: false,
            cwd: dir.clone(),
            tools: Registry::builtin(),
            mcp: vec![],
            frontend: Arc::new(ServerFrontend {
                tx: tx.clone(),
                pending: pending.clone(),
            }),
            interactive: false,
        });
        let mut agent = Agent::new(env.clone());
        agent.messages = vec![
            Message::system("sys"),
            Message::user("one"),
            Message::assistant("first"),
            Message::user("two"),
            Message::assistant("second"),
        ];
        *env.checkpoints.lock().unwrap() = vec![
            Checkpoint {
                id: "a".into(),
                timestamp: "t1".into(),
                tree: None,
                msg_index: 1,
                prompt: "one".into(),
                files: Default::default(),
            },
            Checkpoint {
                id: "b".into(),
                timestamp: "t2".into(),
                tree: None,
                msg_index: 3,
                prompt: "two".into(),
                files: HashMap::from([(file.clone(), Some(b"v1\n".to_vec()))]),
            },
        ];
        let s = Arc::new(Session {
            options: json!({}),
            agent: Arc::new(AsyncMutex::new(agent)),
            env: env.clone(),
            tx,
            pending,
            cancel: Mutex::new(None),
        });
        let st = AppState {
            cli: Arc::new(<Cli as clap::Parser>::parse_from(["chanto"])),
            token: Arc::new("test".into()),
            sessions: Arc::new(Mutex::new(HashMap::from([("test".into(), s.clone())]))),
            artifacts: Arc::new(Mutex::new(Artifacts::default())),
        };
        let id = || Path("test".to_string());
        let list = body_json(checkpoints(State(st.clone()), id()).await).await;
        assert_eq!(list["checkpoints"][1]["prompt"], "two");
        assert_eq!(
            (
                list["checkpoints"][0]["code"].clone(),
                list["checkpoints"][1]["code"].clone()
            ),
            (json!(false), json!(true))
        );
        assert_eq!(list["checkpoints"][1]["conversation"], true);
        assert_eq!(list["undo"], false);
        // The transcript marks the prompts that start a checkpoint.
        let transcript = body_json(get_session(State(st.clone()), id()).await).await;
        assert_eq!(transcript["messages"][0]["checkpoint"], 0);
        assert!(transcript["messages"][1]["checkpoint"].is_null());
        assert_eq!(transcript["messages"][2]["checkpoint"], 1);
        for bad in [
            json!({}),
            json!({"checkpoint": 1, "mode": "files"}),
            json!({"checkpoint": 5}),
        ] {
            assert_eq!(
                rewind_session(State(st.clone()), id(), Json(bad.clone()))
                    .await
                    .status(),
                StatusCode::BAD_REQUEST,
                "{bad}"
            );
        }
        // Nothing changes while a turn runs.
        *s.cancel.lock().unwrap() = Some(CancellationToken::new());
        assert_eq!(
            rewind_session(State(st.clone()), id(), Json(json!({"checkpoint": 1})))
                .await
                .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            checkpoints(State(st.clone()), id()).await.status(),
            StatusCode::CONFLICT
        );
        *s.cancel.lock().unwrap() = None;
        // A preview changes nothing; without a shadow snapshot there's no summary.
        let preview = body_json(
            rewind_session(
                State(st.clone()),
                id(),
                Json(json!({"checkpoint": 1, "preview": true})),
            )
            .await,
        )
        .await;
        assert!(preview["summary"].is_null());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v2\n");
        // Code only: the conversation and its checkpoints stay.
        let code = body_json(
            rewind_session(
                State(st.clone()),
                id(),
                Json(json!({"checkpoint": 1, "mode": "code"})),
            )
            .await,
        )
        .await;
        assert_eq!(
            (code["files_restored"].clone(), code["prompt"].clone()),
            (json!(1), json!("two"))
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1\n");
        assert_eq!(code["checkpoints"].as_array().unwrap().len(), 2);
        assert_eq!(s.agent.lock().await.messages.len(), 5);
        // Conversation only: files stay, the prompt and everything after it go.
        std::fs::write(&file, "v3\n").unwrap();
        let conversation = body_json(
            rewind_session(
                State(st.clone()),
                id(),
                Json(json!({"checkpoint": 1, "mode": "conversation"})),
            )
            .await,
        )
        .await;
        assert_eq!(conversation["files_restored"], 0);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v3\n");
        assert_eq!(conversation["checkpoints"].as_array().unwrap().len(), 1);
        assert_eq!(s.agent.lock().await.messages.len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn remote_tool_round_trip() {
        let (base, seen) = mock_server(vec![
            json!({"tool_calls": tc("lookup_ticket", json!({"id": 42}))}),
            json!({"content": "Ticket found."}),
        ])
        .await;
        let (tx, mut rx) = broadcast::channel(32);
        let pending: Pending = Default::default();
        let fe = Arc::new(ServerFrontend {
            tx: tx.clone(),
            pending: pending.clone(),
        });
        let tool = Arc::new(RemoteTool {
            name: "lookup_ticket".into(),
            description: "Find ticket".into(),
            schema: json!({"type": "object", "properties": {"id": {"type": "integer"}}}),
            tx: tx.clone(),
            pending: pending.clone(),
            timeout: std::time::Duration::from_secs(2),
        });
        let mut registry = Registry::builtin();
        registry.tools.push(tool.clone());
        // Its own empty project: the shared temp directory can hold gigabytes
        // that the turn's secret scan and snapshot would otherwise walk.
        let cwd = std::env::temp_dir().join(format!(
            "chanto-remote-tool-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&cwd).unwrap();
        let env = Env::new(EnvInit {
            config: crate::config::Config::default(),
            provider: Provider::new(ProviderKind::Openai, &base, None, 32768),
            model: "mock".into(),
            mode: crate::permissions::Mode::Bypass,
            vision: false,
            cwd: cwd.clone(),
            tools: registry,
            mcp: vec![],
            frontend: fe,
            interactive: false,
        });
        let mut agent = Agent::new(env.clone());
        let s = Arc::new(Session {
            options: json!({}),
            agent: Arc::new(AsyncMutex::new(Agent::new(env.clone()))),
            env: env.clone(),
            tx,
            pending: pending.clone(),
            cancel: Mutex::new(None),
        });
        let st = AppState {
            cli: Arc::new(<Cli as clap::Parser>::parse_from(["chanto"])),
            token: Arc::new("test".into()),
            sessions: Arc::new(Mutex::new(HashMap::from([("test".into(), s)]))),
            artifacts: Arc::new(Mutex::new(Artifacts::default())),
        };
        let client = tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                if ev["type"] == "tool_call" {
                    assert_eq!(ev["name"], "lookup_ticket");
                    assert_eq!(ev["input"]["id"], 42);
                    let response = respond(
                        State(st),
                        Path("test".into()),
                        Json(json!({"id": ev["id"],
                        "result": {"content": "Ticket 42: fix tests", "is_error": false}})),
                    )
                    .await;
                    assert_eq!(response.status(), StatusCode::OK);
                    break;
                }
            }
        });
        let end = agent
            .run_turn("look up ticket 42", vec![], CancellationToken::new())
            .await
            .unwrap();
        client.await.unwrap();
        assert!(matches!(end, TurnEnd::Done(_)));
        assert!(
            seen.lock().unwrap()[1]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["role"] == "tool"
                    && m["content"]
                        .as_str()
                        .unwrap()
                        .contains("Ticket 42: fix tests"))
        );
        assert!(pending.lock().unwrap().is_empty());

        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = Ctx {
            env,
            cancel,
            depth: 0,
            call_id: None,
            cwd_override: None,
            reads: None,
        };
        assert!(tool.run(json!({}), &ctx).await.unwrap().is_error);
        assert!(pending.lock().unwrap().is_empty());
        let short = RemoteTool {
            name: "timeout".into(),
            description: "".into(),
            schema: json!({}),
            tx: tool.tx.clone(),
            pending: pending.clone(),
            timeout: std::time::Duration::from_millis(1),
        };
        let ctx = Ctx {
            cancel: CancellationToken::new(),
            ..ctx
        };
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(1),
                tool.run(json!({}), &ctx)
            )
            .await
            .is_err()
        );
        assert!(pending.lock().unwrap().is_empty());
        assert!(short.run(json!({}), &ctx).await.unwrap().is_error);
        assert!(pending.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&cwd);
    }
}
