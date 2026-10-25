//! Model Context Protocol client (stdio, streamable HTTP and the legacy
//! HTTP+SSE transport, with OAuth for HTTP servers): lists tools and exposes
//! them as `mcp__<server>__<tool>`, reads resources (`ListMcpResources` /
//! `ReadMcpResource`, `@server:uri` mentions), turns prompts into
//! `/mcp__<server>__<prompt>` commands and passes servers' questions
//! (elicitation) on to the user.

use crate::config::McpServerConfig;
use crate::llm::ToolSpec;
use crate::tools::{Ctx, Tool, ToolKind, ToolOutput};
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub read_only: bool,
    /// The server's `_meta["anthropic/alwaysLoad"]` for this tool: load it
    /// upfront (true) or let tool search defer it (false / unset).
    pub always_load: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct McpPromptDef {
    pub name: String,
    pub description: String,
    /// (name, required)
    pub arguments: Vec<(String, bool)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Connected,
    /// HTTP server that wants OAuth and has no (valid) token: `/mcp login <name>`.
    NeedsLogin,
    /// Couldn't connect (or start); `/mcp reconnect <name>` tries again.
    Failed(String),
    /// Not connected on purpose: turned off in `/mcp`, `disabled = true`, or
    /// blocked by policy. The text says which.
    Off(String),
}

const PROTOCOL_VERSION: &str = "2025-06-18";

/// Requests waiting for a response that arrives on another channel (stdio, SSE stream).
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

/// Who answers a server's `elicitation/create`: the session's frontend, set
/// once an interactive session starts. Unset (print mode, startup): decline.
/// Who answers a server's own requests: the user (elicitation, approvals) and
/// the user's model (sampling). Both unset when nobody is there to ask.
#[derive(Default)]
pub struct Host {
    frontend: Option<Arc<dyn crate::agent::Frontend>>,
    sampler: Option<Arc<dyn Sampler>>,
    hooks: Option<Arc<dyn ElicitHooks>>,
}

/// Elicitation / ElicitationResult hooks around asking the user.
#[async_trait::async_trait]
pub trait ElicitHooks: Send + Sync {
    /// An answer for the server without asking the user, if a hook gives one.
    async fn before(&self, server: &str, params: &Value) -> Option<Value>;
    /// The user's answer, possibly changed, as it goes to the server.
    async fn after(&self, server: &str, params: &Value, result: Value) -> Value;
}
type Asker = Arc<RwLock<Host>>;

/// A server's `sampling/createMessage`, already approved, to run on the user's model.
pub struct SampleRequest {
    pub system: Option<String>,
    /// (role, text), oldest first; roles are "user" or "assistant".
    pub messages: Vec<(String, String)>,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
}

#[async_trait::async_trait]
pub trait Sampler: Send + Sync {
    /// The model's reply and the model's name.
    async fn sample(&self, request: SampleRequest) -> Result<(String, String)>;
}

/// Server questions (elicitation) waiting on the user right now. A request
/// whose answer depends on one isn't stuck: it gets more time.
static ASKING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How long a request may wait in all while the user is answering a server's question.
const ASKING_LIMIT: Duration = Duration::from_secs(3600);

/// How long a request other than a tool call may take.
const REQUEST_LIMIT: Duration = Duration::from_secs(300);

/// How long a tool call may take when neither the server's `tool_timeout_sec`,
/// `mcp_tool_timeout` nor `MCP_TOOL_TIMEOUT` says.
pub const TOOL_TIMEOUT_DEFAULT: Duration = Duration::from_secs(600);

/// The tool call limit without a per-server one: Claude Code's
/// `MCP_TOOL_TIMEOUT` (milliseconds) when set, else ten minutes.
fn default_tool_timeout(env: Option<&str>) -> Duration {
    env.and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|ms| ms.is_finite() && *ms > 0.0)
        .map(|ms| Duration::from_secs_f64((ms / 1000.0).min(7.0 * 86_400.0)))
        .unwrap_or(TOOL_TIMEOUT_DEFAULT)
}

/// Tells the server a request was cancelled if dropped while armed.
struct CancelOnDrop {
    client: Option<std::sync::Weak<McpClient>>,
    id: u64,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (Some(client), Ok(rt)) = (
            self.client.as_ref().and_then(|c| c.upgrade()),
            tokio::runtime::Handle::try_current(),
        ) else {
            return;
        };
        let id = self.id;
        rt.spawn(async move { client.abandon(id, "cancelled by the user").await });
    }
}

/// Waits up to `limit` for `fut`, and longer (up to `ASKING_LIMIT` in all)
/// while the user is answering a server's question. `None` on timeout.
async fn within<T>(limit: Duration, fut: impl std::future::Future<Output = T>) -> Option<T> {
    tokio::pin!(fut);
    let start = std::time::Instant::now();
    loop {
        match tokio::time::timeout(limit, &mut fut).await {
            Ok(v) => return Some(v),
            Err(_) if ASKING.load(Ordering::Relaxed) > 0 && start.elapsed() < ASKING_LIMIT => {
                continue;
            }
            Err(_) => return None,
        }
    }
}

/// A request got no answer in time.
#[derive(Debug)]
struct TimedOut(Duration);

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no answer after {}s", self.0.as_secs_f64())
    }
}

impl std::error::Error for TimedOut {}

/// The server answered with a JSON-RPC error: it is alive, the request failed.
/// Anything else coming out of a request is the connection going away.
#[derive(Debug)]
struct ServerError(String);

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MCP error: {}", self.0)
    }
}

impl std::error::Error for ServerError {}

// The `Http` variant carries the connection's state inline: it is matched by
// field across the file, and boxing it would add noise for one allocation per
// server, so the size difference is deliberate.
#[allow(clippy::large_enum_variant)]
enum Transport {
    Stdio {
        tx: tokio::sync::mpsc::UnboundedSender<String>,
        pending: Pending,
        alive: Arc<std::sync::atomic::AtomicBool>,
        _child: tokio::process::Child,
    },
    Http {
        url: String,
        headers: Vec<(String, String)>,
        session: Mutex<Option<String>>,
        http: reqwest::Client,
        tokens: Mutex<Option<crate::mcp_auth::Tokens>>,
        oauth: crate::mcp_auth::OAuthConfig,
        /// Legacy HTTP+SSE transport: configured (`transport = "sse"`), or
        /// detected when the server refuses streamable HTTP.
        legacy: AtomicBool,
        sse: Sse,
    },
}

/// The legacy transport's event stream: responses and server requests arrive
/// on it; messages go by POST to the URL its `endpoint` event names.
#[derive(Default)]
struct Sse {
    endpoint: Mutex<Option<String>>,
    pending: Pending,
    alive: Arc<AtomicBool>,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
    opening: tokio::sync::Mutex<()>,
}

impl Sse {
    fn close(&self) {
        if let Some(t) = self.reader.lock().unwrap().take() {
            t.abort();
        }
        self.alive.store(false, Ordering::Relaxed);
        self.endpoint.lock().unwrap().take();
        self.pending.lock().unwrap().clear();
    }
}

impl Drop for Sse {
    fn drop(&mut self) {
        if let Some(t) = self.reader.get_mut().ok().and_then(|r| r.take()) {
            t.abort();
        }
    }
}

/// A non-2xx HTTP reply (a 4xx to the first POST means "try legacy SSE").
#[derive(Debug)]
struct HttpStatus(reqwest::StatusCode, String);

impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MCP HTTP {}: {}", self.0, self.1)
    }
}

impl std::error::Error for HttpStatus {}

async fn http_error(resp: reqwest::Response) -> anyhow::Error {
    let status = resp.status();
    HttpStatus(
        status,
        crate::util::truncate(&resp.text().await.unwrap_or_default(), 300),
    )
    .into()
}

pub struct McpClient {
    pub name: String,
    tools: RwLock<Vec<McpToolDef>>,
    prompts: RwLock<Vec<McpPromptDef>>,
    instructions: RwLock<Option<String>>,
    capabilities: RwLock<Value>,
    status: RwLock<Status>,
    /// Set when the server sends a `*/list_changed` notification.
    stale: Arc<AtomicBool>,
    /// The server's last `WWW-Authenticate` challenge (points at its auth metadata).
    challenge: Mutex<Option<String>>,
    asker: Asker,
    /// `None` while not connected (failed, disabled, or never started).
    transport: RwLock<Option<Arc<Transport>>>,
    /// The server's configuration as written, for reconnecting.
    cfg: McpServerConfig,
    next: AtomicU64,
    self_ref: Mutex<Option<std::sync::Weak<McpClient>>>,
    /// Tool filters from the server's config (`enabled_tools` / `disabled_tools`).
    enabled_tools: Option<Vec<String>>,
    disabled_tools: Vec<String>,
    /// `alwaysLoad` from the server's config: true keeps every tool out of
    /// tool search, false defers even tools the server marks always-load.
    always_load: Option<bool>,
    /// Per-call limit from `tool_timeout_sec` (else `MCP_TOOL_TIMEOUT` or the default).
    tool_timeout: Option<Duration>,
}

impl McpClient {
    /// Connect and list tools. An HTTP server that requires OAuth and has no
    /// usable token comes back with `Status::NeedsLogin` instead of an error.
    pub async fn connect(name: &str, cfg: &McpServerConfig) -> Result<Arc<McpClient>> {
        let c = Self::offline(name, cfg, Status::Connected);
        c.start(true).await?;
        Ok(c)
    }

    /// A client that isn't connected (yet): listed in `/mcp`, with no tools,
    /// until `reconnect` starts it.
    pub fn offline(name: &str, cfg: &McpServerConfig, status: Status) -> Arc<McpClient> {
        let c = Arc::new(McpClient {
            name: name.to_string(),
            tools: Default::default(),
            prompts: Default::default(),
            instructions: Default::default(),
            capabilities: RwLock::new(json!({})),
            status: RwLock::new(status),
            stale: Arc::new(AtomicBool::new(false)),
            challenge: Mutex::new(None),
            asker: Default::default(),
            transport: RwLock::new(None),
            cfg: cfg.clone(),
            next: AtomicU64::new(1),
            self_ref: Mutex::new(None),
            enabled_tools: cfg.enabled_tools.clone(),
            always_load: cfg.always_load,
            disabled_tools: cfg.disabled_tools.clone(),
            tool_timeout: cfg
                .tool_timeout_sec
                .filter(|s| s.is_finite() && *s > 0.0)
                .map(|s| Duration::from_secs_f64(s.min(86_400.0))),
        });
        *c.self_ref.lock().unwrap() = Some(Arc::downgrade(&c));
        c
    }

    /// The live transport, or an error when the server isn't connected.
    fn tp(&self) -> Result<Arc<Transport>> {
        self.transport
            .read()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("MCP server `{}` is not connected (see /mcp)", self.name))
    }

    /// Build the transport (spawning a stdio server) without talking to it yet.
    fn open_transport(&self) -> Result<Arc<Transport>> {
        let (cfg, referenced) = expand_config(&self.cfg, &env_lookup)?;
        let transport = if let Some(url) = &cfg.url {
            let legacy = cfg.transport.as_deref() == Some("sse");
            Transport::Http {
                url: url.clone(),
                headers: cfg
                    .headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                session: Mutex::new(None),
                http: crate::http::builder()
                    .timeout(Duration::from_secs(300))
                    .build()?,
                tokens: Mutex::new(crate::mcp_auth::load(&self.name, url)),
                oauth: cfg.oauth.clone().unwrap_or_default(),
                legacy: AtomicBool::new(legacy),
                sse: Sse::default(),
            }
        } else {
            Self::spawn_stdio(
                &self.name,
                &cfg,
                &referenced,
                self.stale.clone(),
                self.asker.clone(),
            )?
        };
        let t = Arc::new(transport);
        *self.transport.write().unwrap() = Some(t.clone());
        Ok(t)
    }

    /// Start the transport and handshake. `use_cache`: a server that needed a
    /// login last time and still has no token is marked as needing one
    /// without asking it (often the slowest part of startup).
    async fn start(&self, use_cache: bool) -> Result<()> {
        let t = self.open_transport()?;
        *self.status.write().unwrap() = Status::Connected;
        let no_token =
            matches!(&*t, Transport::Http { tokens, .. } if tokens.lock().unwrap().is_none());
        drop(t);
        let url = self.cfg.url.clone();
        if use_cache
            && no_token
            && url
                .as_deref()
                .is_some_and(|u| needs_login_cached(&self.name, u))
        {
            *self.status.write().unwrap() = Status::NeedsLogin;
            return Ok(());
        }
        match self.handshake_any().await {
            Ok(()) => {}
            Err(e) if e.downcast_ref::<crate::mcp_auth::AuthRequired>().is_some() => {
                let www = e
                    .downcast_ref::<crate::mcp_auth::AuthRequired>()
                    .and_then(|a| a.www_authenticate.clone());
                *self.challenge.lock().unwrap() = www;
                *self.status.write().unwrap() = Status::NeedsLogin;
                if let Some(u) = &url {
                    remember_needs_login(&self.name, u);
                }
            }
            Err(e) => {
                self.shutdown(Status::Failed(crate::util::one_line(
                    &format!("{e:#}"),
                    200,
                )));
                return Err(e);
            }
        }
        Ok(())
    }

    /// Disconnect (stopping a stdio server) and forget the tools; `status` says why.
    pub fn shutdown(&self, status: Status) {
        if let Some(t) = self.transport.write().unwrap().take()
            && let Transport::Http { sse, .. } = &*t
        {
            sse.close();
        }
        self.tools.write().unwrap().clear();
        self.prompts.write().unwrap().clear();
        *self.instructions.write().unwrap() = None;
        *self.capabilities.write().unwrap() = json!({});
        *self.status.write().unwrap() = status;
    }

    /// Drop the connection and connect again (`/mcp reconnect`, `/mcp enable`),
    /// within the server's startup time limit.
    pub async fn reconnect(&self) -> Result<()> {
        self.shutdown(Status::Failed("reconnecting".into()));
        *self.challenge.lock().unwrap() = None;
        let secs = startup_secs(&self.cfg);
        match tokio::time::timeout(Duration::from_secs_f64(secs), self.start(false)).await {
            Ok(r) => r,
            Err(_) => {
                self.shutdown(Status::Failed(format!("timed out after {secs}s")));
                bail!("MCP server `{}` timed out after {secs}s", self.name)
            }
        }
    }

    /// The configuration this client was started from.
    pub fn config(&self) -> &McpServerConfig {
        &self.cfg
    }

    pub fn status(&self) -> Status {
        self.status.read().unwrap().clone()
    }
    pub fn is_connected(&self) -> bool {
        self.status() == Status::Connected
    }
    pub fn is_http(&self) -> bool {
        self.cfg.url.is_some()
    }
    pub fn tools(&self) -> Vec<McpToolDef> {
        self.tools.read().unwrap().clone()
    }
    pub fn prompts(&self) -> Vec<McpPromptDef> {
        self.prompts.read().unwrap().clone()
    }
    pub fn instructions(&self) -> Option<String> {
        self.instructions.read().unwrap().clone()
    }
    pub fn has_resources(&self) -> bool {
        self.is_connected() && !self.capabilities.read().unwrap()["resources"].is_null()
    }
    /// Let the server ask the user for input (`elicitation/create`) through `fe`.
    /// Without one, requests are declined.
    /// Let this server's sampling requests run on the user's model (after approval).
    pub fn set_sampler(&self, sampler: Arc<dyn Sampler>) {
        self.asker.write().unwrap().sampler = Some(sampler);
    }

    pub fn set_frontend(&self, fe: Arc<dyn crate::agent::Frontend>) {
        self.asker.write().unwrap().frontend = Some(fe);
    }

    pub fn set_elicit_hooks(&self, hooks: Arc<dyn ElicitHooks>) {
        self.asker.write().unwrap().hooks = Some(hooks);
    }

    /// Browser OAuth login, then (re)connect. `open` receives the login URL.
    pub async fn login(&self, open: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
        if !self.is_http() {
            bail!(
                "`{}` is a local (stdio) server; it doesn't use OAuth",
                self.name
            );
        }
        // A server that failed to start gets a fresh transport to log in over.
        let t = match self.tp() {
            Ok(t) => t,
            Err(_) => self.open_transport()?,
        };
        let Transport::Http {
            url,
            http,
            tokens,
            oauth,
            session,
            sse,
            ..
        } = &*t
        else {
            unreachable!()
        };
        let mut www = self.challenge.lock().unwrap().clone();
        if www.is_none() {
            // Restored from the needs-login cache without asking the server: ask
            // now, for its OAuth details (or it may no longer need a login).
            match self.handshake_any().await {
                Ok(()) => {
                    *self.status.write().unwrap() = Status::Connected;
                    return Ok(());
                }
                Err(e) => match e.downcast_ref::<crate::mcp_auth::AuthRequired>() {
                    Some(a) => www = a.www_authenticate.clone(),
                    None => return Err(e),
                },
            }
        }
        let t = crate::mcp_auth::login(url, oauth, http, www, open).await?;
        crate::mcp_auth::save(&self.name, url, &t)?;
        *tokens.lock().unwrap() = Some(t);
        *session.lock().unwrap() = None;
        sse.close();
        self.handshake_any().await?;
        *self.status.write().unwrap() = Status::Connected;
        Ok(())
    }

    /// Forget stored tokens. Returns whether any were stored.
    pub fn logout(&self) -> bool {
        let Some(t) = self.transport.read().unwrap().clone() else {
            return false;
        };
        let Transport::Http {
            url,
            tokens,
            session,
            sse,
            ..
        } = &*t
        else {
            return false;
        };
        tokens.lock().unwrap().take();
        session.lock().unwrap().take();
        sse.close();
        let had = crate::mcp_auth::delete(&self.name, url);
        *self.status.write().unwrap() = Status::NeedsLogin;
        self.tools.write().unwrap().clear();
        self.prompts.write().unwrap().clear();
        had
    }

    fn spawn_stdio(
        name: &str,
        cfg: &McpServerConfig,
        referenced: &[String],
        stale: Arc<AtomicBool>,
        asker: Asker,
    ) -> Result<Transport> {
        let mut cmd = tokio::process::Command::new(&cfg.command);
        cmd.args(&cfg.args);
        if let Some(dir) = cfg.cwd.as_deref().filter(|d| !d.is_empty()) {
            cmd.current_dir(dir);
        }
        // Don't hand a third-party MCP server the user's whole environment: strip
        // secret-named variables by default. A server that genuinely needs a token
        // declares it in its own `env` map (applied after this and winning), names
        // it in `env_vars`, or references it as `${VAR}` in its config.
        let sc = crate::secrets::SecretsConfig::default();
        for (k, _) in std::env::vars_os() {
            if let Some(k) = k
                .to_str()
                .filter(|k| withhold_from_server(k, cfg, referenced, &sc))
            {
                cmd.env_remove(k);
            }
        }
        cmd.envs(&cfg.env);
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow!("spawning MCP server `{name}` ({}): {e}", cfg.command))?;
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let pending: Pending = Default::default();
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        // Writer task: all stdin writes go through here.
        tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if stdin.write_all(m.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
        });
        let p2 = pending.clone();
        let a2 = alive.clone();
        let reply_tx = tx.clone();
        let server = name.to_string();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = String::from_utf8_lossy(&buf);
                let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
                    continue;
                };
                match (v.get("id"), v.get("method")) {
                    // Response to one of our requests.
                    (Some(id), None) => {
                        if let Some(id) = id.as_u64()
                            && let Some(s) = p2.lock().unwrap().remove(&id)
                        {
                            let _ = s.send(v);
                        }
                    }
                    // Request from the server. Answered off the read loop: an
                    // elicitation waits for the user while responses keep coming.
                    (Some(_), Some(_)) => {
                        let (req, tx, asker, server) =
                            (v.clone(), reply_tx.clone(), asker.clone(), server.clone());
                        tokio::spawn(async move {
                            let _ = tx.send(answer(&server, &asker, &req).await.to_string());
                        });
                    }
                    // Notification: tools/prompts/resources changed → refresh lazily.
                    (None, Some(m)) if m.as_str().is_some_and(|m| m.ends_with("/list_changed")) => {
                        stale.store(true, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
            a2.store(false, std::sync::atomic::Ordering::Relaxed);
            p2.lock().unwrap().clear();
        });
        Ok(Transport::Stdio {
            tx,
            pending,
            alive,
            _child: child,
        })
    }

    /// `handshake`, retrying over the legacy HTTP+SSE transport when an HTTP
    /// server refuses the streamable POST with a 4xx (the spec's
    /// backwards-compatibility rule).
    async fn handshake_any(&self) -> Result<()> {
        let r = self.handshake().await;
        let Ok(t) = self.tp() else { return r };
        let Transport::Http {
            legacy,
            session,
            sse,
            ..
        } = &*t
        else {
            return r;
        };
        // The spec's sign of an older server: the POST itself isn't accepted. Not a
        // rate limit, a timeout or an auth problem, which retrying as SSE won't fix.
        let refused = r
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<HttpStatus>())
            .is_some_and(|s| matches!(s.0.as_u16(), 400 | 404 | 405 | 406));
        if !refused || legacy.load(Ordering::Relaxed) {
            return r;
        }
        let first = r.unwrap_err();
        legacy.store(true, Ordering::Relaxed);
        *session.lock().unwrap() = None;
        match self.handshake().await {
            Ok(()) => Ok(()),
            Err(e) if e.downcast_ref::<crate::mcp_auth::AuthRequired>().is_some() => Err(e),
            Err(e) => {
                legacy.store(false, Ordering::Relaxed);
                sse.close();
                Err(anyhow!("{first} (and as a legacy SSE server: {e})"))
            }
        }
    }

    async fn handshake(&self) -> Result<()> {
        let init = self.request("initialize", init_params()).await?;
        *self.instructions.write().unwrap() = init["instructions"].as_str().map(String::from);
        *self.capabilities.write().unwrap() = init["capabilities"].clone();
        self.notify("notifications/initialized", json!({})).await?;
        self.list_tools().await?;
        if !init["capabilities"]["prompts"].is_null() {
            // Prompts are optional extras: don't fail the connection over them.
            let _ = self.list_prompts().await;
        }
        Ok(())
    }

    /// Fetch every page of a `*/list` method.
    async fn list_all(&self, method: &str, key: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(cur) => json!({"cursor": cur}),
                None => json!({}),
            };
            let r = self.request(method, params).await?;
            out.extend(r[key].as_array().cloned().unwrap_or_default());
            cursor = r["nextCursor"].as_str().map(String::from);
            if cursor.is_none() {
                return Ok(out);
            }
        }
    }

    async fn list_tools(&self) -> Result<()> {
        let tools = self
            .list_all("tools/list", "tools")
            .await?
            .iter()
            .map(|t| McpToolDef {
                name: t["name"].as_str().unwrap_or("").to_string(),
                description: t["description"].as_str().unwrap_or("").to_string(),
                schema: object_schema(t.get("inputSchema")),
                read_only: t["annotations"]["readOnlyHint"].as_bool().unwrap_or(false),
                always_load: t["_meta"]["anthropic/alwaysLoad"].as_bool(),
            })
            .filter(|t| !t.name.is_empty())
            .filter(|t| tool_allowed(self.enabled_tools.as_deref(), &self.disabled_tools, &t.name))
            .collect();
        *self.tools.write().unwrap() = tools;
        Ok(())
    }

    async fn list_prompts(&self) -> Result<()> {
        let prompts = self
            .list_all("prompts/list", "prompts")
            .await?
            .iter()
            .map(|p| McpPromptDef {
                name: p["name"].as_str().unwrap_or("").to_string(),
                description: p["description"].as_str().unwrap_or("").to_string(),
                arguments: p["arguments"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|a| {
                        (
                            a["name"].as_str().unwrap_or("").to_string(),
                            a["required"].as_bool().unwrap_or(false),
                        )
                    })
                    .collect(),
            })
            .collect();
        *self.prompts.write().unwrap() = prompts;
        Ok(())
    }

    /// Re-list tools and prompts after a `list_changed` notification.
    pub async fn refresh_if_stale(&self) {
        if self.stale.swap(false, Ordering::Relaxed) && self.is_connected() {
            let _ = self.list_tools().await;
            if !self.capabilities.read().unwrap()["prompts"].is_null() {
                let _ = self.list_prompts().await;
            }
        }
    }

    /// Resources and resource templates.
    pub async fn list_resources(&self) -> Result<Vec<Value>> {
        let mut v = self.list_all("resources/list", "resources").await?;
        if let Ok(t) = self
            .list_all("resources/templates/list", "resourceTemplates")
            .await
        {
            v.extend(t);
        }
        Ok(v)
    }

    /// Read a resource: text contents joined, images (base64) separately.
    pub async fn read_resource(&self, uri: &str) -> Result<(String, Vec<String>)> {
        let r = self.request("resources/read", json!({"uri": uri})).await?;
        let mut text = Vec::new();
        let mut images = Vec::new();
        for c in r["contents"].as_array().into_iter().flatten() {
            let mime = c["mimeType"].as_str().unwrap_or("");
            if let Some(t) = c["text"].as_str() {
                text.push(t.to_string());
            } else if let Some(b) = c["blob"].as_str() {
                if mime.starts_with("image/") {
                    images.push(b.to_string());
                } else {
                    text.push(format!(
                        "[binary resource {} ({mime}, {} bytes base64)]",
                        c["uri"].as_str().unwrap_or(uri),
                        b.len()
                    ));
                }
            }
        }
        Ok((text.join("\n"), images))
    }

    /// `prompts/get`, flattened to the text to send as the user's prompt.
    pub async fn get_prompt(
        &self,
        name: &str,
        args: serde_json::Map<String, Value>,
    ) -> Result<String> {
        let r = self
            .request("prompts/get", json!({"name": name, "arguments": args}))
            .await?;
        let mut parts = Vec::new();
        for m in r["messages"].as_array().into_iter().flatten() {
            let c = &m["content"];
            let t = match c["type"].as_str() {
                Some("text") => c["text"].as_str().unwrap_or("").to_string(),
                Some("resource") => c["resource"]["text"].as_str().unwrap_or("").to_string(),
                _ => continue,
            };
            parts.push(if m["role"] == "assistant" {
                format!("(assistant) {t}")
            } else {
                t
            });
        }
        if parts.is_empty() {
            bail!("prompt `{name}` returned no text");
        }
        Ok(parts.join("\n\n"))
    }

    /// Send an HTTP request built by `build` with the configured headers, the
    /// stored OAuth token (refreshed when expired or rejected) and the session id.
    async fn http_send(
        &self,
        build: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let t = self.tp()?;
        let Transport::Http {
            headers,
            session,
            http,
            tokens,
            ..
        } = &*t
        else {
            unreachable!()
        };
        let has_auth_header = headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
        let mut refreshed = false;
        let resp = loop {
            let tok = tokens.lock().unwrap().clone();
            if let Some(t) = tok
                .as_ref()
                .filter(|t| t.expired() && t.refresh_token.is_some() && !refreshed)
            {
                refreshed = true;
                self.refresh_token(t).await;
                continue;
            }
            let mut rb = build(http).header("MCP-Protocol-Version", PROTOCOL_VERSION);
            for (k, v) in headers {
                rb = rb.header(k, v);
            }
            if let (Some(t), false) = (&tok, has_auth_header) {
                rb = rb.bearer_auth(&t.access_token);
            }
            if let Some(sid) = session.lock().unwrap().clone() {
                rb = rb.header("Mcp-Session-Id", sid);
            }
            let resp = rb.send().await?;
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED && !has_auth_header {
                if let Some(t) = tok
                    .as_ref()
                    .filter(|t| t.refresh_token.is_some() && !refreshed)
                {
                    refreshed = true;
                    if self.refresh_token(t).await {
                        continue;
                    }
                }
                let www = resp
                    .headers()
                    .get("www-authenticate")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                *self.status.write().unwrap() = Status::NeedsLogin;
                return Err(crate::mcp_auth::AuthRequired {
                    www_authenticate: www,
                }
                .into());
            }
            break resp;
        };
        if let Some(sid) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *session.lock().unwrap() = Some(sid.to_string());
        }
        Ok(resp)
    }

    /// Streamable HTTP: POST a request and return its response, which comes
    /// back as JSON or in an event stream. Server requests in that stream (an
    /// elicitation, say) are answered as they arrive.
    async fn http_post(&self, msg: Value, id: u64) -> Result<Value> {
        let t = self.tp()?;
        let Transport::Http { url, .. } = &*t else {
            unreachable!()
        };
        // A tool call's answer can wait on the user answering the server's question.
        let limit = if msg["method"] == "tools/call" {
            ASKING_LIMIT
        } else {
            Duration::from_secs(300)
        };
        let mut resp = self
            .http_send(|h| {
                h.post(url.as_str())
                    .header("Accept", "application/json, text/event-stream")
                    .timeout(limit)
                    .json(&msg)
            })
            .await?;
        if !resp.status().is_success() {
            return Err(http_error(resp).await);
        }
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !ctype.contains("text/event-stream") {
            return Ok(serde_json::from_str(&resp.text().await?)?);
        }
        let mut events = SseParser::default();
        loop {
            let chunk = resp.chunk().await?;
            // At the end, a blank line flushes an unterminated last event.
            for (_, data) in events.push(chunk.as_deref().unwrap_or(&b"\n\n"[..])) {
                let Ok(v) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                if v.get("method").is_none() && v["id"].as_u64() == Some(id) {
                    return Ok(v);
                }
                self.on_message(v).await;
            }
            if chunk.is_none() {
                bail!("MCP: no response for request {id}");
            }
        }
    }

    /// A server message that isn't the response being waited for.
    async fn on_message(&self, v: Value) {
        match (v.get("id").is_some(), v["method"].as_str()) {
            (true, Some(_)) => {
                let reply = answer(&self.name, &self.asker, &v).await;
                let _ = self.http_reply(&reply).await;
            }
            (false, Some(m)) if m.ends_with("/list_changed") => {
                self.stale.store(true, Ordering::Relaxed)
            }
            _ => {}
        }
    }

    /// HTTP: POST a message that gets no response (a notification, or our
    /// answer to a server request): to the server URL, or for legacy SSE to
    /// the stream's endpoint.
    async fn http_reply(&self, msg: &Value) -> Result<()> {
        let t = self.tp()?;
        let Transport::Http {
            url, legacy, sse, ..
        } = &*t
        else {
            unreachable!()
        };
        let target = if legacy.load(Ordering::Relaxed) {
            let endpoint = sse.endpoint.lock().unwrap().clone();
            endpoint.ok_or_else(|| {
                anyhow!("MCP server `{}`: the SSE stream isn't connected", self.name)
            })?
        } else {
            url.clone()
        };
        let resp = self
            .http_send(|h| {
                h.post(target.as_str())
                    .header("Accept", "application/json, text/event-stream")
                    .json(msg)
            })
            .await?;
        if !resp.status().is_success() {
            return Err(http_error(resp).await);
        }
        Ok(())
    }

    /// Legacy SSE: open the event stream unless it's open, and wait for the
    /// `endpoint` event. A new stream is a new session on the server, so after
    /// a dropped connection (`reinit`) it is initialized again.
    async fn sse_open(&self, reinit: bool) -> Result<()> {
        let t = self.tp()?;
        let Transport::Http { url, sse, .. } = &*t else {
            unreachable!()
        };
        let _opening = sse.opening.lock().await;
        if sse.alive.load(Ordering::Relaxed) && sse.endpoint.lock().unwrap().is_some() {
            return Ok(());
        }
        sse.close();
        // The stream stays open for the whole session: no overall timeout.
        let resp = self
            .http_send(|h| {
                h.get(url.as_str())
                    .header("Accept", "text/event-stream")
                    .timeout(Duration::from_secs(10 * 365 * 86_400))
            })
            .await?;
        if !resp.status().is_success() {
            return Err(http_error(resp).await);
        }
        let base = resp.url().clone();
        let (etx, erx) = oneshot::channel::<std::result::Result<String, String>>();
        let (pending, alive, stale) = (sse.pending.clone(), sse.alive.clone(), self.stale.clone());
        let me = self.self_ref.lock().unwrap().clone();
        alive.store(true, Ordering::Relaxed);
        let reader = tokio::spawn(async move {
            let mut resp = resp;
            let mut etx = Some(etx);
            let mut events = SseParser::default();
            while let Ok(Some(chunk)) = resp.chunk().await {
                for (event, data) in events.push(&chunk) {
                    if event == "endpoint" {
                        // Usually a path relative to the stream's URL. Messages to it
                        // carry this server's headers and token, so it must be on
                        // the same origin: never let the server send them elsewhere.
                        if let (Some(tx), Ok(u)) = (etx.take(), base.join(data.trim())) {
                            let same = u.scheme() == base.scheme()
                                && u.host_str() == base.host_str()
                                && u.port_or_known_default() == base.port_or_known_default();
                            let _ = tx.send(if same { Ok(u.to_string()) } else { Err(format!("its SSE `endpoint` points to another origin ({}), which would receive this server's credentials", u.origin().ascii_serialization())) });
                        }
                        continue;
                    }
                    let Ok(v) = serde_json::from_str::<Value>(&data) else {
                        continue;
                    };
                    let method = v["method"].as_str().map(str::to_string);
                    match (v.get("id").cloned(), method) {
                        (Some(id), None) => {
                            let sender = id
                                .as_u64()
                                .and_then(|id| pending.lock().unwrap().remove(&id));
                            if let Some(s) = sender {
                                let _ = s.send(v);
                            }
                        }
                        (Some(_), Some(_)) => {
                            let Some(c) = me.as_ref().and_then(std::sync::Weak::upgrade) else {
                                continue;
                            };
                            tokio::spawn(async move {
                                let reply = answer(&c.name, &c.asker, &v).await;
                                let _ = c.http_reply(&reply).await;
                            });
                        }
                        (None, Some(m)) if m.ends_with("/list_changed") => {
                            stale.store(true, Ordering::Relaxed)
                        }
                        _ => {}
                    }
                }
            }
            alive.store(false, Ordering::Relaxed);
            pending.lock().unwrap().clear();
        });
        *sse.reader.lock().unwrap() = Some(reader);
        let endpoint = match tokio::time::timeout(Duration::from_secs(30), erx).await {
            Ok(Ok(Ok(e))) => e,
            Ok(Ok(Err(why))) => {
                sse.close();
                bail!("MCP server `{}`: {why}", self.name);
            }
            _ => {
                sse.close();
                bail!(
                    "MCP server `{}`: its SSE stream sent no `endpoint` event",
                    self.name
                );
            }
        };
        *sse.endpoint.lock().unwrap() = Some(endpoint);
        if reinit {
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = oneshot::channel();
            sse.pending.lock().unwrap().insert(id, tx);
            self.http_reply(&json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": init_params()})).await?;
            self.wait(&sse.pending, id, rx, REQUEST_LIMIT)
                .await
                .map_err(|e| {
                    if e.is::<TimedOut>() {
                        anyhow!("MCP initialize timed out")
                    } else {
                        e
                    }
                })?;
            self.http_reply(
                &json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
            )
            .await?;
        }
        Ok(())
    }

    /// Refresh and persist the OAuth token; false if the refresh failed.
    async fn refresh_token(&self, t: &crate::mcp_auth::Tokens) -> bool {
        let Some(tp) = self.transport.read().unwrap().clone() else {
            return false;
        };
        let Transport::Http {
            url, http, tokens, ..
        } = &*tp
        else {
            return false;
        };
        match crate::mcp_auth::refresh(http, t).await {
            Ok(n) => {
                let _ = crate::mcp_auth::save(&self.name, url, &n);
                *tokens.lock().unwrap() = Some(n);
                true
            }
            Err(_) => false,
        }
    }

    async fn send(&self, v: Value) -> Result<()> {
        let t = self.tp()?;
        match &*t {
            Transport::Stdio { tx, alive, .. } => {
                if !alive.load(std::sync::atomic::Ordering::Relaxed) {
                    bail!("MCP server `{}` is not running", self.name);
                }
                tx.send(v.to_string())
                    .map_err(|_| anyhow!("MCP server `{}` is not running", self.name))
            }
            Transport::Http { .. } => self.http_reply(&v).await,
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    /// Wait for the response to `id` where it arrives on another channel (stdio, SSE stream).
    async fn wait(
        &self,
        pending: &Pending,
        id: u64,
        rx: oneshot::Receiver<Value>,
        limit: Duration,
    ) -> Result<Value> {
        match within(limit, rx).await {
            Some(Ok(v)) => Ok(v),
            Some(Err(_)) => bail!("MCP server `{}` exited", self.name),
            None => {
                pending.lock().unwrap().remove(&id);
                let _ = self
                    .notify(
                        "notifications/cancelled",
                        json!({"requestId": id, "reason": "timeout"}),
                    )
                    .await;
                Err(TimedOut(limit).into())
            }
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_within(method, params, REQUEST_LIMIT)
            .await
            .map_err(|e| match e.downcast_ref::<TimedOut>() {
                Some(_) => anyhow!("MCP {method} timed out"),
                None => e,
            })
    }

    /// Stop waiting for request `id`: forget its pending reply and tell the
    /// server (MCP's `notifications/cancelled`).
    async fn abandon(&self, id: u64, reason: &str) {
        if let Some(t) = self.transport.read().unwrap().clone() {
            match &*t {
                Transport::Stdio { pending, .. } => {
                    pending.lock().unwrap().remove(&id);
                }
                Transport::Http { sse, .. } => {
                    sse.pending.lock().unwrap().remove(&id);
                }
            }
        }
        let _ = self
            .notify(
                "notifications/cancelled",
                json!({"requestId": id, "reason": reason}),
            )
            .await;
    }

    /// A request that fails with [`TimedOut`] when the server takes longer than
    /// `limit` (more while the user answers the server's question).
    async fn request_within(&self, method: &str, params: Value, limit: Duration) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let t = self.tp()?;
        // Dropped before the answer (the user pressed esc, so the tool call's
        // future was dropped): the server is told the request was cancelled.
        // `initialize` is never cancelled, per the spec.
        let mut guard = CancelOnDrop {
            client: self.self_ref.lock().unwrap().clone(),
            id,
            armed: method != "initialize",
        };
        let v = async {
            Ok::<Value, anyhow::Error>(match &*t {
                Transport::Http { legacy, sse, .. } if legacy.load(Ordering::Relaxed) => {
                    self.sse_open(method != "initialize").await?;
                    let (tx, rx) = oneshot::channel();
                    sse.pending.lock().unwrap().insert(id, tx);
                    if let Err(e) = self.http_reply(&msg).await {
                        sse.pending.lock().unwrap().remove(&id);
                        return Err(e);
                    }
                    self.wait(&sse.pending, id, rx, limit).await?
                }
                Transport::Http { .. } => match within(limit, self.http_post(msg, id)).await {
                    Some(r) => r?,
                    None => {
                        let _ = self
                            .notify(
                                "notifications/cancelled",
                                json!({"requestId": id, "reason": "timeout"}),
                            )
                            .await;
                        return Err(TimedOut(limit).into());
                    }
                },
                Transport::Stdio { pending, .. } => {
                    let (tx, rx) = oneshot::channel();
                    pending.lock().unwrap().insert(id, tx);
                    if let Err(e) = self.send(msg).await {
                        pending.lock().unwrap().remove(&id);
                        return Err(e);
                    }
                    self.wait(pending, id, rx, limit).await?
                }
            })
        }
        .await;
        // Answered, failed or timed out (which notifies by itself): nothing to cancel.
        guard.armed = false;
        let v = v?;
        if let Some(e) = v.get("error") {
            return Err(anyhow::Error::new(ServerError(
                e["message"].as_str().unwrap_or(&e.to_string()).to_string(),
            )));
        }
        Ok(v["result"].clone())
    }

    pub fn full_name(&self, tool: &str) -> String {
        let name = format!("mcp__{}__{}", sanitize(&self.name), sanitize(tool));
        // Model APIs reject the whole request over one tool name longer than
        // 64 characters; shorten it, with a hash so names stay distinct.
        if name.chars().count() <= MAX_TOOL_NAME || tool.is_empty() {
            return name;
        }
        let hash = name.bytes().fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ b as u64).wrapping_mul(0x100000001b3)
        });
        format!("{}_{:08x}", &name[..MAX_TOOL_NAME - 9], hash as u32)
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.tools
            .read()
            .unwrap()
            .iter()
            .map(|t| self.full_name(&t.name))
            .collect()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .read()
            .unwrap()
            .iter()
            .map(|t| ToolSpec {
                name: self.full_name(&t.name),
                description: t.description.clone(),
                parameters: t.schema.clone(),
            })
            .collect()
    }

    /// Tool specs with whether each loads upfront (never deferred by tool search).
    pub fn spec_entries(&self) -> Vec<(ToolSpec, bool)> {
        self.tools
            .read()
            .unwrap()
            .iter()
            .map(|t| {
                let upfront = self.always_load.unwrap_or(t.always_load == Some(true));
                (
                    ToolSpec {
                        name: self.full_name(&t.name),
                        description: t.description.clone(),
                        parameters: t.schema.clone(),
                    },
                    upfront,
                )
            })
            .collect()
    }

    /// A connected client with these tools that can't reach any server (tests).
    #[cfg(test)]
    pub(crate) fn with_tools_for_test(
        name: &str,
        tools: Vec<McpToolDef>,
        always_load: Option<bool>,
    ) -> Arc<McpClient> {
        Self::with_tools_for_test_at(name, tools, always_load, Some("http://127.0.0.1:9/mcp"))
    }

    /// As [`Self::with_tools_for_test`], configured as a stdio server when
    /// `url` is `None`.
    #[cfg(test)]
    pub(crate) fn with_tools_for_test_at(
        name: &str,
        tools: Vec<McpToolDef>,
        always_load: Option<bool>,
        url: Option<&str>,
    ) -> Arc<McpClient> {
        let c = Arc::new(McpClient {
            name: name.to_string(),
            tools: RwLock::new(tools),
            prompts: Default::default(),
            instructions: Default::default(),
            capabilities: RwLock::new(json!({})),
            status: RwLock::new(Status::Connected),
            stale: Default::default(),
            challenge: Mutex::new(None),
            asker: Default::default(),
            transport: RwLock::new(Some(Arc::new(Transport::Http {
                url: "http://127.0.0.1:9/mcp".into(),
                headers: vec![],
                session: Mutex::new(None),
                http: crate::http::client(),
                tokens: Mutex::new(None),
                oauth: Default::default(),
                legacy: AtomicBool::new(false),
                sse: Sse::default(),
            }))),
            cfg: McpServerConfig {
                url: url.map(str::to_string),
                command: if url.is_none() {
                    "server".into()
                } else {
                    String::new()
                },
                always_load,
                ..Default::default()
            },
            next: AtomicU64::new(1),
            self_ref: Mutex::new(None),
            enabled_tools: None,
            disabled_tools: vec![],
            always_load,
            tool_timeout: None,
        });
        *c.self_ref.lock().unwrap() = Some(Arc::downgrade(&c));
        c
    }

    pub fn tool(&self, full: &str) -> Option<Arc<dyn Tool>> {
        let def = self
            .tools
            .read()
            .unwrap()
            .iter()
            .find(|t| self.full_name(&t.name) == full)?
            .clone();
        let me = self.self_ref.lock().unwrap().as_ref()?.upgrade()?;
        Some(Arc::new(McpTool { client: me, def }))
    }

    pub async fn call(&self, tool: &str, args: Value) -> Result<ToolOutput> {
        let limit = self.tool_timeout.unwrap_or_else(|| {
            default_tool_timeout(std::env::var("MCP_TOOL_TIMEOUT").ok().as_deref())
        });
        let req = self.request_within(
            "tools/call",
            json!({"name": tool, "arguments": args}),
            limit,
        );
        let r = match req.await {
            Ok(r) => r,
            Err(e) if e.is::<TimedOut>() => {
                let secs = limit.as_secs_f64();
                bail!(
                    "MCP tool `{tool}` on server `{}` didn't answer within {secs}s and was abandoned. The server may be stuck; try again or another way. (The user can raise the limit with `tool_timeout_sec` on the server, `mcp_tool_timeout` in config or MCP_TOOL_TIMEOUT in milliseconds.)",
                    self.name
                )
            }
            Err(e) if e.is::<ServerError>() => return Err(e),
            Err(e) => {
                // The connection went away. Remember it, so `/mcp` and the
                // footer stop reporting the server as connected, but leave a
                // server the user turned off with its reason.
                let why = crate::util::one_line(&format!("{e:#}"), 200);
                {
                    let mut st = self.status.write().unwrap();
                    if matches!(*st, Status::Connected) {
                        *st = Status::Failed(why);
                    }
                }
                bail!(
                    "MCP server `{}` disconnected: {}. `/mcp reconnect {}` brings it back.",
                    self.name,
                    crate::util::one_line(&format!("{e:#}"), 300),
                    self.name
                )
            }
        };
        let mut text = Vec::new();
        let mut images = Vec::new();
        for c in r["content"].as_array().into_iter().flatten() {
            match c["type"].as_str() {
                Some("text") => text.push(c["text"].as_str().unwrap_or("").to_string()),
                Some("image") => images.push(c["data"].as_str().unwrap_or("").to_string()),
                Some("resource") => {
                    text.push(c["resource"]["text"].as_str().unwrap_or("").to_string())
                }
                _ => text.push(c.to_string()),
            }
        }
        if text.is_empty()
            && let Some(sc) = r.get("structuredContent")
        {
            text.push(sc.to_string());
        }
        Ok(ToolOutput {
            content: text.join("\n"),
            images,
            is_error: r["isError"].as_bool().unwrap_or(false),
            display: None,
        })
    }
}

/// A server's config names an environment variable that is unset and has no
/// `:-default`. The server is skipped rather than handed the literal `${VAR}`.
#[derive(Debug)]
pub struct MissingEnv(pub String);

impl std::fmt::Display for MissingEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "environment variable `{}` is not set (and the config gives no `${{{}:-default}}`)",
            self.0, self.0
        )
    }
}

impl std::error::Error for MissingEnv {}

/// Expand `${VAR}` and `${VAR:-default}` (Claude Code's `.mcp.json` syntax).
/// `${VAR:-default}` uses the default when VAR is unset or empty. Names of the
/// variables referenced are added to `used`.
fn expand_vars(
    s: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
    used: &mut Vec<String>,
) -> std::result::Result<String, MissingEnv> {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(:-([^}]*))?\}").unwrap()
    });
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for c in RE.captures_iter(s) {
        let whole = c.get(0).unwrap();
        out.push_str(&s[last..whole.start()]);
        last = whole.end();
        let name = &c[1];
        if !used.iter().any(|u| u == name) {
            used.push(name.to_string());
        }
        let value = lookup(name);
        match (value, c.get(3)) {
            (Some(v), Some(_)) if !v.is_empty() => out.push_str(&v),
            (_, Some(default)) => out.push_str(default.as_str()),
            (Some(v), None) => out.push_str(&v),
            (None, None) => return Err(MissingEnv(name.to_string())),
        }
    }
    out.push_str(&s[last..]);
    Ok(out)
}

/// A server config with `${VAR}` / `${VAR:-default}` expanded in `command`,
/// `args`, `env`, `cwd`, `url` and `headers`, plus the names of the variables
/// it references (they still reach a stdio server, whatever their names).
fn expand_config(
    cfg: &McpServerConfig,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<(McpServerConfig, Vec<String>), MissingEnv> {
    let mut used = Vec::new();
    let mut out = cfg.clone();
    out.command = expand_vars(&cfg.command, lookup, &mut used)?;
    let mut args = Vec::with_capacity(cfg.args.len());
    for a in &cfg.args {
        args.push(expand_vars(a, lookup, &mut used)?);
    }
    out.args = args;
    for (src, dst) in [(&cfg.env, &mut out.env), (&cfg.headers, &mut out.headers)] {
        dst.clear();
        for (k, v) in src {
            dst.insert(k.clone(), expand_vars(v, lookup, &mut used)?);
        }
    }
    if let Some(u) = &cfg.url {
        out.url = Some(expand_vars(u, lookup, &mut used)?);
    }
    if let Some(d) = &cfg.cwd {
        out.cwd = Some(expand_vars(d, lookup, &mut used)?);
    }
    Ok((out, used))
}

fn env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Strip this variable from a stdio server's environment? Secret-looking names
/// are withheld unless the server's config asks for them (`env_vars`, `${VAR}`).
fn withhold_from_server(
    k: &str,
    cfg: &McpServerConfig,
    referenced: &[String],
    sc: &crate::secrets::SecretsConfig,
) -> bool {
    crate::secrets::withhold_env(k, sc) && !cfg.env_vars.iter().chain(referenced).any(|v| v == k)
}

/// Is a tool kept by the server's `enabled_tools` / `disabled_tools`?
fn tool_allowed(cfg_enabled: Option<&[String]>, disabled: &[String], tool: &str) -> bool {
    cfg_enabled.is_none_or(|e| e.iter().any(|t| t == tool)) && !disabled.iter().any(|t| t == tool)
}

/// The longest tool name OpenAI-style APIs accept.
const MAX_TOOL_NAME: usize = 64;

/// A tool's `inputSchema` as a JSON Schema object: model APIs reject the whole
/// request when one tool's parameters aren't `{"type": "object", ...}`.
fn object_schema(s: Option<&Value>) -> Value {
    match s {
        Some(Value::Object(o)) if o.get("type").is_none_or(|t| t == "object") => {
            let mut o = o.clone();
            o.insert("type".into(), json!("object"));
            Value::Object(o)
        }
        _ => json!({"type": "object"}),
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn init_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {"elicitation": {}, "sampling": {}},
        "clientInfo": {"name": "chanto", "version": env!("CARGO_PKG_VERSION")}
    })
}

/// Incremental `text/event-stream` parser: yields (event type, data) per event.
#[derive(Default)]
struct SseParser {
    buf: Vec<u8>,
    event: String,
    data: Vec<String>,
}

impl SseParser {
    fn push(&mut self, chunk: &[u8]) -> Vec<(String, String)> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.buf.drain(..=nl).collect();
            let text = String::from_utf8_lossy(&raw);
            let line = text.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                // Blank line: dispatch. Data may span several `data:` lines.
                let event = std::mem::take(&mut self.event);
                if !self.data.is_empty() {
                    out.push((event, self.data.join("\n")));
                    self.data.clear();
                }
            } else if let Some(d) = line.strip_prefix("data:") {
                self.data.push(d.strip_prefix(' ').unwrap_or(d).to_string());
            } else if let Some(e) = line.strip_prefix("event:") {
                self.event = e.trim().to_string();
            }
        }
        out
    }
}

/// Answer a request from the server: ping, roots (none) and elicitation; the
/// rest (sampling, …) aren't supported.
async fn answer(server: &str, asker: &Asker, req: &Value) -> Value {
    let id = req["id"].clone();
    let result = match req["method"].as_str().unwrap_or("") {
        "ping" => json!({}),
        "roots/list" => json!({"roots": []}),
        "sampling/createMessage" => {
            let (fe, sampler) = {
                let h = asker.read().unwrap();
                (h.frontend.clone(), h.sampler.clone())
            };
            match sample(server, &req["params"], fe.as_deref(), sampler.as_deref()).await {
                Ok(v) => v,
                Err((code, message)) => {
                    return json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});
                }
            }
        }
        "elicitation/create" => {
            let (fe, hooks) = {
                let h = asker.read().unwrap();
                (h.frontend.clone(), h.hooks.clone())
            };
            ASKING.fetch_add(1, Ordering::Relaxed);
            let result =
                elicit_with_hooks(server, &req["params"], fe.as_deref(), hooks.as_deref()).await;
            ASKING.fetch_sub(1, Ordering::Relaxed);
            result
        }
        _ => {
            return json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not supported by client"}});
        }
    };
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// `sampling/createMessage`: the server wants a completion from the user's
/// model. The user approves each one (it runs on their account) after seeing
/// the prompt; only text content is supported. Errors are JSON-RPC (code, message).
async fn sample(
    server: &str,
    params: &Value,
    fe: Option<&dyn crate::agent::Frontend>,
    sampler: Option<&dyn Sampler>,
) -> std::result::Result<Value, (i64, String)> {
    let (Some(fe), Some(sampler)) = (fe, sampler) else {
        return Err((
            -1,
            "sampling needs a user to approve it, and this session has none".into(),
        ));
    };
    let mut messages = Vec::new();
    for m in params["messages"].as_array().into_iter().flatten() {
        let role = m["role"].as_str().unwrap_or("user");
        let content = &m["content"];
        // A single block, or (newer spec) an array of them.
        let blocks: Vec<&Value> = match content.as_array() {
            Some(a) => a.iter().collect(),
            None => vec![content],
        };
        let mut text = String::new();
        for b in blocks {
            match b["type"].as_str() {
                Some("text") => text.push_str(b["text"].as_str().unwrap_or("")),
                other => {
                    return Err((
                        -32602,
                        format!(
                            "only text content can be sampled, not {}",
                            other.unwrap_or("unknown")
                        ),
                    ));
                }
            }
        }
        messages.push((
            if role == "assistant" {
                "assistant"
            } else {
                "user"
            }
            .to_string(),
            text,
        ));
    }
    if messages.is_empty() {
        return Err((-32602, "no messages to sample".into()));
    }
    let max_tokens = params["maxTokens"]
        .as_u64()
        .unwrap_or(1024)
        .clamp(1, 16_384) as u32;
    let last = messages.last().map(|(_, t)| t.as_str()).unwrap_or("");
    let preview = crate::util::truncate(last, 600);
    let q = format!(
        "MCP server `{server}` wants to run a prompt on your model (up to {max_tokens} tokens, on your account):\n\n{preview}"
    );
    match fe
        .choose(&q, &["Allow".into(), "Deny".into()])
        .await
        .as_deref()
    {
        Some("Allow") => {}
        _ => return Err((-1, "the user declined the sampling request".into())),
    }
    let request = SampleRequest {
        system: params["systemPrompt"].as_str().map(String::from),
        messages,
        max_tokens,
        temperature: params["temperature"].as_f64().map(|t| t as f32),
    };
    let (text, model) = sampler
        .sample(request)
        .await
        .map_err(|e| (-32603, format!("{e:#}")))?;
    Ok(
        json!({"role": "assistant", "content": {"type": "text", "text": text}, "model": model, "stopReason": "endTurn"}),
    )
}

const SKIP: &str = "Skip this field";
const DECLINE: &str = "Decline the request";
const SEND: &str = "Send";

/// An elicitation through the hooks: an Elicitation hook may answer it
/// outright (then no ElicitationResult hooks run); otherwise the user
/// answers and ElicitationResult hooks see, and may change, the answer.
pub(crate) async fn elicit_with_hooks(
    server: &str,
    params: &Value,
    fe: Option<&dyn crate::agent::Frontend>,
    hooks: Option<&dyn ElicitHooks>,
) -> Value {
    let Some(hooks) = hooks else {
        return elicit(server, params, fe).await;
    };
    if let Some(answer) = hooks.before(server, params).await {
        return answer;
    }
    let result = elicit(server, params, fe).await;
    hooks.after(server, params, result).await
}

/// `elicitation/create`: ask the user for each field of the (flat) requested
/// schema in turn. Returns the result: accept with content, decline (the user
/// said no, or nobody to ask) or cancel (dismissed).
async fn elicit(server: &str, params: &Value, fe: Option<&dyn crate::agent::Frontend>) -> Value {
    let Some(fe) = fe else {
        return json!({"action": "decline"});
    };
    let header = format!(
        "MCP server `{server}` asks: {}",
        params["message"].as_str().unwrap_or("")
    );
    let schema = &params["requestedSchema"];
    let required: Vec<&str> = schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let empty = serde_json::Map::new();
    let props = schema["properties"].as_object().unwrap_or(&empty);
    if props.is_empty() {
        return match fe
            .choose(&header, &["Accept".into(), DECLINE.into()])
            .await
            .as_deref()
        {
            Some("Accept") => json!({"action": "accept", "content": {}}),
            Some(_) => json!({"action": "decline"}),
            None => json!({"action": "cancel"}),
        };
    }
    // Object keys come back sorted, so ask the required fields first, in the
    // order the server lists them, then the rest.
    let mut keys: Vec<&String> = required
        .iter()
        .filter_map(|r| props.keys().find(|k| k.as_str() == *r))
        .collect();
    keys.extend(props.keys().filter(|k| !required.contains(&k.as_str())));
    let mut content = serde_json::Map::new();
    for (i, key) in keys.iter().enumerate() {
        let p = &props[key.as_str()];
        let req = required.contains(&key.as_str());
        let mut options = field_options(p);
        if !req {
            options.push(SKIP.into());
        }
        let mut problem = String::new();
        loop {
            let q = format!(
                "{header}\n\n{}{problem}\n\n(esc to cancel)",
                field_prompt(key, p, req, i, keys.len())
            );
            let Some(a) = fe.question(&q, &options).await else {
                return json!({"action": "cancel"});
            };
            if a == SKIP && !req {
                break;
            }
            match field_value(p, &a) {
                Ok(v) => {
                    content.insert(key.to_string(), v);
                    break;
                }
                Err(e) => problem = format!("\n\n{a:?} isn't valid: {e}."),
            }
        }
    }
    // Show exactly what will be sent before it goes: declining is a deliberate
    // last step, never what a stray Enter on a field picks.
    let review = content
        .iter()
        .map(|(k, v)| format!("- {k}: {v}"))
        .collect::<Vec<_>>()
        .join("\n");
    let q = format!("{header}\n\nSend this to `{server}`?\n\n{review}");
    match fe
        .choose(&q, &[SEND.into(), DECLINE.into()])
        .await
        .as_deref()
    {
        Some(SEND) => json!({"action": "accept", "content": content}),
        Some(_) => json!({"action": "decline"}),
        None => json!({"action": "cancel"}),
    }
}

/// "[2/3] Age (required): Your age (whole number, at least 18)"
fn field_prompt(key: &str, p: &Value, req: bool, i: usize, n: usize) -> String {
    let mut s = format!("[{}/{n}] {}", i + 1, p["title"].as_str().unwrap_or(key));
    if req {
        s.push_str(" (required)");
    }
    if let Some(d) = p["description"].as_str() {
        s.push_str(&format!(": {d}"));
    }
    let mut hints = Vec::new();
    match p["type"].as_str() {
        Some("integer") => hints.push("whole number".to_string()),
        Some("number") => hints.push("number".to_string()),
        _ => {}
    }
    if let Some(f) = p["format"].as_str() {
        hints.push(f.to_string());
    }
    for (k, label, unit) in [
        ("minimum", "at least", ""),
        ("maximum", "at most", ""),
        ("minLength", "at least", " characters"),
        ("maxLength", "at most", " characters"),
    ] {
        if let Some(v) = p.get(k).filter(|v| v.is_number()) {
            hints.push(format!("{label} {v}{unit}"));
        }
    }
    if let Some(d) = p.get("default").filter(|d| !d.is_null()) {
        hints.push(format!("default {d}"));
    }
    if !hints.is_empty() {
        s.push_str(&format!(" ({})", hints.join(", ")));
    }
    s
}

/// Choices offered for a field: enum labels, yes/no, or the default value.
fn field_options(p: &Value) -> Vec<String> {
    if let Some(values) = p["enum"].as_array() {
        let names = p["enumNames"].as_array();
        return values
            .iter()
            .enumerate()
            .map(|(i, v)| {
                names
                    .and_then(|n| n.get(i))
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(|| {
                        v.as_str()
                            .map(String::from)
                            .unwrap_or_else(|| v.to_string())
                    })
            })
            .collect();
    }
    match (
        p["type"].as_str(),
        p.get("default").filter(|d| !d.is_null()),
    ) {
        (Some("boolean"), _) => vec!["Yes".into(), "No".into()],
        (_, Some(d)) => vec![
            d.as_str()
                .map(String::from)
                .unwrap_or_else(|| d.to_string()),
        ],
        _ => vec![],
    }
}

/// The user's answer as the field's JSON value, or what's wrong with it.
fn field_value(p: &Value, a: &str) -> std::result::Result<Value, String> {
    let a = a.trim();
    if let Some(values) = p["enum"].as_array() {
        let labels = field_options(p);
        let i = labels
            .iter()
            .position(|l| l == a)
            .or_else(|| values.iter().position(|v| v.as_str() == Some(a)));
        return i
            .map(|i| values[i].clone())
            .ok_or_else(|| format!("pick one of: {}", labels.join(", ")));
    }
    let range = |n: f64| -> std::result::Result<(), String> {
        if let Some(min) = p["minimum"].as_f64().filter(|m| n < *m) {
            return Err(format!("must be at least {min}"));
        }
        if let Some(max) = p["maximum"].as_f64().filter(|m| n > *m) {
            return Err(format!("must be at most {max}"));
        }
        Ok(())
    };
    match p["type"].as_str() {
        Some("boolean") => match a.to_ascii_lowercase().as_str() {
            "yes" | "y" | "true" => Ok(json!(true)),
            "no" | "n" | "false" => Ok(json!(false)),
            _ => Err("answer yes or no".into()),
        },
        Some("integer") => {
            let n: i64 = a.parse().map_err(|_| "enter a whole number".to_string())?;
            range(n as f64)?;
            Ok(json!(n))
        }
        Some("number") => {
            let n: f64 = a
                .parse()
                .ok()
                .filter(|n: &f64| n.is_finite())
                .ok_or_else(|| "enter a number".to_string())?;
            range(n)?;
            Ok(json!(n))
        }
        _ => {
            let len = a.chars().count() as u64;
            if let Some(min) = p["minLength"].as_u64().filter(|m| len < *m) {
                return Err(format!("needs at least {min} characters"));
            }
            if let Some(max) = p["maxLength"].as_u64().filter(|m| len > *m) {
                return Err(format!("allows at most {max} characters"));
            }
            Ok(json!(a))
        }
    }
}

pub struct McpTool {
    client: Arc<McpClient>,
    def: McpToolDef,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> String {
        self.client.full_name(&self.def.name)
    }
    fn description(&self) -> String {
        self.def.description.clone()
    }
    fn schema(&self) -> Value {
        self.def.schema.clone()
    }
    fn kind(&self) -> ToolKind {
        // Not ReadOnly even with a `readOnlyHint`: that's the server's own
        // claim, and ReadOnly tools run without asking in every mode, so a
        // server's "read-only" fetch or search would carry data out with no
        // one asked. An allow rule for the tool is how a user vouches for it.
        ToolKind::External
    }
    fn read_only_hint(&self) -> bool {
        // Believed (as a hint) only from a server running here, one the user
        // installed, not from a remote one.
        self.def.read_only && !self.client.is_http()
    }
    fn label(&self, args: &Value) -> String {
        format!(
            "{}:{}({})",
            self.client.name,
            self.def.name,
            crate::util::one_line(&args.to_string(), 60)
        )
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let mut out = self.client.call(&self.def.name, args).await?;
        if !ctx.env.vision() {
            out.images.clear();
        }
        Ok(out)
    }
}

// ------------------------------------------------------------ resources

fn resource_server<'a>(mcp: &'a [Arc<McpClient>], name: &str) -> Result<&'a Arc<McpClient>> {
    mcp.iter()
        .filter(|c| c.has_resources())
        .find(|c| c.name == name || sanitize(&c.name) == name)
        .ok_or_else(|| {
            let names: Vec<&str> = mcp
                .iter()
                .filter(|c| c.has_resources())
                .map(|c| c.name.as_str())
                .collect();
            anyhow!(
                "no connected MCP server `{name}` with resources (available: {})",
                names.join(", ")
            )
        })
}

/// Lists resources from MCP servers (Claude Code's `ListMcpResourcesTool`).
pub struct ListMcpResources;

#[async_trait]
impl Tool for ListMcpResources {
    fn name(&self) -> String {
        "ListMcpResources".into()
    }
    fn description(&self) -> String {
        "List resources (files, docs, records) exposed by connected MCP servers. Read one with ReadMcpResource.".into()
    }
    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {"server": {"type": "string", "description": "Only this server (default: all)"}}})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }
    fn label(&self, args: &Value) -> String {
        format!(
            "ListMcpResources({})",
            args["server"].as_str().unwrap_or("all")
        )
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let servers: Vec<&Arc<McpClient>> = match args["server"].as_str().filter(|s| !s.is_empty())
        {
            Some(s) => vec![resource_server(&ctx.env.mcp, s)?],
            None => ctx.env.mcp.iter().filter(|c| c.has_resources()).collect(),
        };
        let mut out = Vec::new();
        for c in servers {
            match c.list_resources().await {
                Ok(list) => {
                    for r in list {
                        let uri = r["uri"]
                            .as_str()
                            .or(r["uriTemplate"].as_str())
                            .unwrap_or("");
                        let mut line = format!("{}: {uri}", c.name);
                        if let Some(n) = r["name"].as_str() {
                            line.push_str(&format!(" — {n}"));
                        }
                        if let Some(d) = r["description"].as_str() {
                            line.push_str(&format!(": {}", crate::util::one_line(d, 120)));
                        }
                        if let Some(m) = r["mimeType"].as_str() {
                            line.push_str(&format!(" [{m}]"));
                        }
                        out.push(line);
                    }
                }
                Err(e) => out.push(format!("{}: error listing resources: {e}", c.name)),
            }
        }
        Ok(ToolOutput::ok(if out.is_empty() {
            "No resources.".to_string()
        } else {
            out.join("\n")
        }))
    }
}

/// Reads one MCP resource by URI (Claude Code's `ReadMcpResourceTool`).
pub struct ReadMcpResource;

#[async_trait]
impl Tool for ReadMcpResource {
    fn name(&self) -> String {
        "ReadMcpResource".into()
    }
    fn description(&self) -> String {
        "Read a resource from an MCP server by its URI (see ListMcpResources).".into()
    }
    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {"server": {"type": "string"}, "uri": {"type": "string"}}, "required": ["server", "uri"]})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }
    fn label(&self, args: &Value) -> String {
        format!(
            "ReadMcpResource({}:{})",
            args["server"].as_str().unwrap_or(""),
            args["uri"].as_str().unwrap_or("")
        )
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let c = resource_server(&ctx.env.mcp, crate::tools::arg_str(&args, "server")?)?;
        let uri = crate::tools::arg_str(&args, "uri")?;
        let (text, images) = c.read_resource(uri).await?;
        let mut out = ToolOutput::ok(crate::util::truncate(&text, 100_000));
        if ctx.env.vision() {
            out.images = images;
        }
        Ok(out)
    }
}

/// Read `server:uri` for an `@server:uri` mention; None if `server` isn't a
/// connected server with resources.
pub async fn read_mention(
    mcp: &[Arc<McpClient>],
    mention: &str,
) -> Option<Result<(String, String, String)>> {
    let (server, uri) = mention.split_once(':')?;
    let c = mcp
        .iter()
        .find(|c| c.has_resources() && (c.name == server || sanitize(&c.name) == server))?;
    Some(
        c.read_resource(uri)
            .await
            .map(|(t, _)| (c.name.clone(), uri.to_string(), t)),
    )
}

// -------------------------------------------------------------- prompts

pub fn prompt_command(c: &McpClient, p: &McpPromptDef) -> String {
    format!("mcp__{}__{}", sanitize(&c.name), sanitize(&p.name))
}

/// Every prompt from connected servers: (command name without `/`, description).
pub fn prompt_commands(mcp: &[Arc<McpClient>]) -> Vec<(String, String)> {
    let mut v = Vec::new();
    for c in mcp.iter().filter(|c| c.is_connected()) {
        for p in c.prompts() {
            let args: Vec<String> = p
                .arguments
                .iter()
                .map(|(n, req)| {
                    if *req {
                        format!("<{n}>")
                    } else {
                        format!("[{n}]")
                    }
                })
                .collect();
            let desc = format!(
                "{}{}",
                if args.is_empty() {
                    String::new()
                } else {
                    format!("{} — ", args.join(" "))
                },
                crate::util::one_line(&p.description, 80)
            );
            v.push((prompt_command(c, &p), desc));
        }
    }
    v
}

/// Map positional arguments onto the prompt's declared arguments in order
/// (extra words go to the last one).
pub fn prompt_args(def: &McpPromptDef, arg: &str) -> Result<serde_json::Map<String, Value>> {
    let words = shell_words::split(arg)
        .unwrap_or_else(|_| arg.split_whitespace().map(str::to_string).collect());
    let mut map = serde_json::Map::new();
    let n = def.arguments.len();
    for (i, (name, _)) in def.arguments.iter().enumerate() {
        let v = if i + 1 == n && words.len() > n {
            words[i..].join(" ")
        } else if let Some(w) = words.get(i) {
            w.clone()
        } else {
            continue;
        };
        map.insert(name.clone(), Value::String(v));
    }
    let missing: Vec<&str> = def
        .arguments
        .iter()
        .filter(|(n, req)| *req && !map.contains_key(n))
        .map(|(n, _)| n.as_str())
        .collect();
    if !missing.is_empty() {
        bail!("missing required argument(s): {}", missing.join(", "));
    }
    Ok(map)
}

/// Run an `/mcp__server__prompt` command: Some(prompt text) if `cmd` names one.
pub async fn run_prompt_command(
    mcp: &[Arc<McpClient>],
    cmd: &str,
    arg: &str,
) -> Option<Result<String>> {
    for c in mcp.iter().filter(|c| c.is_connected()) {
        if let Some(p) = c
            .prompts()
            .into_iter()
            .find(|p| prompt_command(c, p) == cmd)
        {
            return Some(match prompt_args(&p, arg) {
                Ok(args) => c.get_prompt(&p.name, args).await,
                Err(e) => Err(e),
            });
        }
    }
    None
}

/// Connect to all configured servers concurrently, skipping failures.
fn needs_login_file() -> std::path::PathBuf {
    crate::config::home_dir().join("mcp-needs-login.json")
}

/// Servers seen asking for a login, re-checked once a day.
fn needs_login_cached(name: &str, url: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(needs_login_file()) else {
        return false;
    };
    let v: Value = serde_json::from_str(&text).unwrap_or_default();
    let key = format!("{name} {url}");
    v[&key]
        .as_i64()
        .is_some_and(|t| chrono::Utc::now().timestamp() - t < 86_400)
}

fn remember_needs_login(name: &str, url: &str) {
    let path = needs_login_file();
    let mut v: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}));
    v[format!("{name} {url}")] = json!(chrono::Utc::now().timestamp());
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, v.to_string());
}

/// Imported servers (from Claude Code or Codex configs) whose problem was
/// already reported once. They stay listed in `/mcp`, but don't warn on every launch.
fn first_notice(keys: &[String]) -> Vec<String> {
    let path = crate::config::home_dir().join("mcp-notified.json");
    let mut seen: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}));
    let new: Vec<String> = keys
        .iter()
        .filter(|k| seen.get(k.as_str()).is_none())
        .cloned()
        .collect();
    if !new.is_empty() {
        for k in &new {
            seen[k.as_str()] = json!(chrono::Utc::now().timestamp());
        }
        let _ = std::fs::create_dir_all(crate::config::home_dir());
        let _ = std::fs::write(&path, seen.to_string());
    }
    new
}

/// How long a server may take to start: its `startup_timeout_sec`, else 30s
/// (10s for servers borrowed from other agents' configs).
fn startup_secs(c: &McpServerConfig) -> f64 {
    c.startup_timeout_sec
        .filter(|s| s.is_finite() && *s > 0.0)
        .map(|s| s.min(86_400.0))
        .unwrap_or(if c.imported_from.is_some() {
            10.0
        } else {
            30.0
        })
}

/// Why a configured server isn't started at all, or `None` if it should be.
fn off_reason(c: &McpServerConfig) -> Option<String> {
    if let Some(g) = &c.gate {
        return Some(g.describe());
    }
    if c.disabled {
        return Some("disabled in its config file (`disabled = true`)".into());
    }
    (c.command.is_empty() && c.url.as_deref().unwrap_or("").is_empty())
        .then(|| "not configured (no command or URL)".into())
}

/// The servers that should run, with `${VAR}` / `${VAR:-default}` expanded, for
/// a client that starts them itself (the ACP client hands them to the agent).
/// Returns the usable `(name, config)` pairs and one message per server that
/// could not be prepared.
pub(crate) fn resolved(
    servers: &std::collections::BTreeMap<String, McpServerConfig>,
) -> (Vec<(String, McpServerConfig)>, Vec<String>) {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for (name, cfg) in servers {
        if off_reason(cfg).is_some() {
            continue;
        }
        match expand_config(cfg, &env_lookup) {
            Ok((cfg, _)) => out.push((name.clone(), cfg)),
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }
    (out, errors)
}

/// Connect every configured server. Every server gets a client, so `/mcp`
/// can list and reconnect it: ones that are off or failed come back offline.
pub async fn connect_all(
    servers: &std::collections::BTreeMap<String, McpServerConfig>,
) -> (Vec<Arc<McpClient>>, Vec<String>) {
    let futs = servers
        .iter()
        .filter(|(_, c)| off_reason(c).is_none())
        .map(|(n, c)| async move {
            let r = tokio::time::timeout(
                Duration::from_secs_f64(startup_secs(c)),
                McpClient::connect(n, c),
            )
            .await;
            if crate::util::timing_on() {
                crate::util::timing(&format!(
                    "mcp {n} {}",
                    match &r {
                        Ok(Ok(_)) => "connected",
                        Ok(Err(_)) => "failed",
                        Err(_) => "timed out",
                    }
                ));
            }
            (n.clone(), c, r)
        });
    let mut ok: Vec<Arc<McpClient>> = servers
        .iter()
        .filter_map(|(n, c)| Some(McpClient::offline(n, c, Status::Off(off_reason(c)?))))
        .collect();
    let mut errs = Vec::new();
    let mut imported_failed = Vec::new();
    let mut needs_login = Vec::new();
    let mut imported_login = Vec::new();
    let mut missing_env = Vec::new();
    for (n, cfg, r) in futures::future::join_all(futs).await {
        let origin = cfg.imported_from.clone();
        if let Some(why) = match &r {
            Ok(Ok(_)) => None,
            Ok(Err(e)) => Some(crate::util::one_line(&format!("{e:#}"), 200)),
            Err(_) => Some(format!(
                "timed out after {}s during startup",
                startup_secs(cfg)
            )),
        } {
            ok.push(McpClient::offline(&n, cfg, Status::Failed(why)));
        }
        match (r, origin) {
            // A `${VAR}` the server needs isn't set: say which, rather than "failed".
            (Ok(Err(e)), o) if e.downcast_ref::<MissingEnv>().is_some() => {
                let var = &e.downcast_ref::<MissingEnv>().unwrap().0;
                let msg = format!(
                    "Skipped MCP server `{n}`: environment variable `{var}` is not set (set it, or write `${{{var}:-default}}` in the config)"
                );
                match o {
                    // Imported configs are someone else's: report once.
                    Some(o) => missing_env.push((format!("{n} ({o}) env {var}"), msg)),
                    None => errs.push(msg),
                }
            }
            (Ok(Ok(c)), o) => {
                if c.status() == Status::NeedsLogin {
                    if o.is_some() {
                        imported_login.push(n)
                    } else {
                        needs_login.push(n)
                    }
                }
                ok.push(c)
            }
            (_, Some(o)) => imported_failed.push(format!("{n} ({o})")),
            (Ok(Err(e)), None) => errs.push(format!("MCP server `{n}` failed: {e}")),
            (Err(_), None) => errs.push(format!("MCP server `{n}` timed out during startup")),
        }
    }
    // Imported servers are someone else's config: mention each problem once.
    let keys: Vec<String> = missing_env.iter().map(|(k, _)| k.clone()).collect();
    let fresh = first_notice(&keys);
    errs.extend(
        missing_env
            .into_iter()
            .filter(|(k, _)| fresh.contains(k))
            .map(|(_, m)| m),
    );
    needs_login.extend(
        first_notice(
            &imported_login
                .iter()
                .map(|n| format!("{n} login"))
                .collect::<Vec<_>>(),
        )
        .into_iter()
        .map(|k| k.trim_end_matches(" login").to_string()),
    );
    let imported_failed: Vec<String> = first_notice(
        &imported_failed
            .iter()
            .map(|n| format!("{n} failed"))
            .collect::<Vec<_>>(),
    )
    .into_iter()
    .map(|k| k.trim_end_matches(" failed").to_string())
    .collect();
    if !needs_login.is_empty() {
        errs.push(format!(
            "MCP servers that need you to log in: {} (run /mcp login <name>)",
            needs_login.join(", ")
        ));
    }
    if !imported_failed.is_empty() {
        // Often OAuth-only servers; keep it to one line. Details in /mcp.
        errs.push(format!(
            "Skipped imported MCP servers that failed to connect: {}. `/mcp` shows their status; this is only reported once. Disable them with `[mcp_servers.<name>] disabled = true` in ~/.chanto/config.toml.",
            imported_failed.join(", ")
        ));
    }
    (ok, errs)
}

#[cfg(test)]
mod tests;
