//! Driving an external agent over the Agent Client Protocol (ACP) v1.
//!
//! `chanto acp-client <command>` starts the interactive UI with the given
//! program as the agent instead of Chanto's own model loop. The program must
//! speak ACP on stdio, which is what the adapters for Claude Code
//! (`claude-agent-acp`, the Claude Agent SDK adapter), OpenCode and others do.
//! Everything above this module is unchanged: the TUI renders [`Event`]s,
//! permission prompts and questions the same way, so `/remote` (including
//! `/remote cloud`) and the HTTP server drive the external agent without
//! knowing it is one.
//!
//! We speak the client half of ACP: `initialize`, the `session/*` lifecycle
//! (new, load, resume, close), `session/prompt`, `session/cancel`,
//! `session/request_permission`, `elicitation/create`, the `fs/*` client
//! methods and the `terminal/*` methods. MCP servers from Chanto's own config
//! are handed to the agent at session start, so it gets the same tools.

use crate::agent::{Env, Event, Frontend, PermissionReply, PermissionRequest, TurnEnd};
use crate::permissions::Mode;
use crate::tools::misc::Todo;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio_util::sync::CancellationToken;

const PROTOCOL_VERSION: u64 = 1;
/// How long a cancelled agent gets to stop before it is killed.
const CANCEL_GRACE: Duration = Duration::from_secs(10);
/// Stderr lines kept for the "agent died" message.
const TAIL: usize = 20;
/// Terminal output kept when the agent sets no `outputByteLimit`.
const TERMINAL_LIMIT: usize = 1 << 20;
/// ACP's "authentication required" JSON-RPC error code.
const AUTH_REQUIRED: i64 = -32000;

/// An external ACP agent attached to this Chanto session.
pub struct AcpAgent {
    command: Vec<String>,
    /// A lock, not a plain box, so `Agent` stays `Sync` across turns (the
    /// transport itself need only be `Send`).
    transport: Option<tokio::sync::Mutex<Box<dyn Transport>>>,
    session: Option<Session>,
    /// The last session id, kept across an agent restart so `session/load` or
    /// `session/resume` can restore it. Cleared by `reset`.
    last: Option<String>,
    /// What the agent said it supports in `initialize`.
    caps: AgentCaps,
    /// Sign-in methods the agent advertised in `initialize`.
    auth_methods: Vec<Value>,
    /// The agent's modes and the one it is in, from the session response.
    modes: Option<Value>,
    /// The agent's session configuration options (model, thought level, ...).
    config_options: Vec<Value>,
    /// Slash commands the agent advertised; they are passed through as prompts.
    commands: Vec<Value>,
    /// Set while `session/load` replays the old conversation, which the
    /// Chanto already shows: the replay is not rendered again.
    replaying: bool,
    terminals: HashMap<String, Arc<Terminal>>,
    next_terminal: u64,
    next_id: u64,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    announced: bool,
    commands_announced: bool,
}

#[derive(Default, Clone, Copy)]
struct AgentCaps {
    load: bool,
    resume: bool,
    close: bool,
    mcp_http: bool,
    mcp_sse: bool,
    logout: bool,
}

impl AgentCaps {
    fn from_init(init: &Value) -> Self {
        let agent = &init["agentCapabilities"];
        Self {
            load: agent["loadSession"].as_bool().unwrap_or(false),
            resume: agent["sessionCapabilities"]["resume"].is_object(),
            close: agent["sessionCapabilities"]["close"].is_object(),
            mcp_http: agent["mcpCapabilities"]["http"].as_bool().unwrap_or(false),
            mcp_sse: agent["mcpCapabilities"]["sse"].as_bool().unwrap_or(false),
            logout: agent["auth"]["logout"].is_object(),
        }
    }

    /// The request that restores a previous session, preferring one that
    /// doesn't replay the conversation.
    fn restore_method(&self) -> Option<&'static str> {
        if self.resume {
            Some("session/resume")
        } else if self.load {
            Some("session/load")
        } else {
            None
        }
    }
}

struct Session {
    id: String,
    /// Tool title by ACP tool call id, for `ToolEnd`.
    tools: HashMap<String, String>,
    /// Assistant text of the current turn.
    text: String,
}

#[async_trait]
trait Transport: Send {
    async fn send(&mut self, message: &Value) -> Result<()>;
    /// The next protocol message, or `None` when the agent exited.
    async fn recv(&mut self) -> Result<Option<Value>>;
    async fn stop(&mut self);
}

struct StdioTransport {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

#[async_trait]
impl Transport for StdioTransport {
    async fn send(&mut self, message: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(message)?;
        line.push(b'\n');
        self.stdin.write_all(&line).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<Value>> {
        let mut line = String::new();
        loop {
            line.clear();
            if self.stdout.read_line(&mut line).await? == 0 {
                return Ok(None);
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str(trimmed) {
                Ok(value) => return Ok(Some(value)),
                // Some agents log to stdout despite the protocol; skip it
                // rather than ending the session.
                Err(_) => continue,
            }
        }
    }

    async fn stop(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

impl AcpAgent {
    pub fn new(command: Vec<String>) -> Self {
        Self {
            command,
            transport: None,
            session: None,
            last: None,
            caps: AgentCaps::default(),
            auth_methods: Vec::new(),
            modes: None,
            config_options: Vec::new(),
            commands: Vec::new(),
            replaying: false,
            terminals: HashMap::new(),
            next_terminal: 0,
            next_id: 0,
            stderr_tail: Arc::new(Mutex::new(VecDeque::new())),
            announced: false,
            commands_announced: false,
        }
    }

    /// Start a new conversation. The current agent process is stopped; the
    /// next turn starts a fresh ACP session. The agent gets a `session/close`
    /// first when it supports one, and running terminals are killed.
    pub fn reset(&mut self) {
        let transport = self.transport.take();
        let id = self
            .session
            .take()
            .map(|s| s.id)
            .or_else(|| self.last.take());
        let close = if self.caps.close { id } else { None };
        self.last = None;
        self.caps = AgentCaps::default();
        self.auth_methods.clear();
        self.modes = None;
        self.config_options.clear();
        self.commands.clear();
        self.replaying = false;
        self.announced = false;
        self.commands_announced = false;
        self.stderr_tail.lock().unwrap().clear();
        for terminal in self.terminals.values() {
            terminal.kill();
        }
        self.terminals.clear();
        let Some(transport) = transport else {
            return;
        };
        // `reset` is called from the UI's synchronous path; say goodbye in the
        // background rather than blocking it. Dropping the transport still
        // kills a process that doesn't go away.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        self.next_id += 1;
        let request_id = self.next_id;
        handle.spawn(async move {
            if let Some(id) = close {
                let message = json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "method": "session/close",
                    "params": {"sessionId": id},
                });
                let _ = transport.lock().await.send(&message).await;
            }
            transport.lock().await.stop().await;
        });
    }

    /// Run one user turn through the external agent.
    pub async fn run_turn(
        &mut self,
        env: &Env,
        input: &str,
        images: Vec<String>,
        cancel: CancellationToken,
    ) -> Result<TurnEnd> {
        let frontend = env.frontend.clone();
        let (servers, errors) = mcp_servers(&env.config.mcp_servers);
        for error in errors {
            frontend.emit(Event::Warn(format!("MCP: {error}")));
        }
        self.run_with(
            frontend.as_ref(),
            &env.cwd(),
            env.mode(),
            input,
            images,
            &servers,
            cancel,
        )
        .await
    }

    /// The ACP session id, saved beside the Chanto session so a resumed
    /// Chanto conversation restores the agent's side of it.
    pub fn session_id(&self) -> Option<&str> {
        self.last.as_deref()
    }

    /// Adopt the ACP session id stored with a resumed Chanto session. Ignored
    /// while a live session exists, which is already the conversation.
    pub fn adopt(&mut self, id: Option<&str>) {
        if self.transport.is_none() {
            self.last = id.map(str::to_string);
        }
    }

    /// Whether the agent advertised this slash command, which is passed
    /// through to it instead of being treated as an unknown command.
    pub fn has_command(&self, name: &str) -> bool {
        self.commands
            .iter()
            .any(|command| command["name"].as_str() == Some(name))
    }

    /// The agent's advertised commands, for completion and `/help`.
    pub fn available_commands(&self) -> Vec<(String, String)> {
        self.commands
            .iter()
            .filter_map(|command| {
                let name = command["name"].as_str()?.to_string();
                let description = command["description"].as_str().unwrap_or("").to_string();
                Some((name, description))
            })
            .collect()
    }

    /// The agent's modes, or a note that it has none.
    pub fn modes_summary(&self) -> String {
        let Some(modes) = &self.modes else {
            return "The agent has not advertised any modes.".into();
        };
        let current = modes["currentModeId"].as_str().unwrap_or("");
        let mut lines = String::from("## Agent modes\nChange one with `/agent-mode <id>`.\n");
        for mode in modes["availableModes"].as_array().into_iter().flatten() {
            let id = mode["id"].as_str().unwrap_or("");
            let name = mode["name"].as_str().unwrap_or(id);
            let mark = if id == current { " **(current)**" } else { "" };
            lines.push_str(&format!("- `{id}` — {name}{mark}\n"));
        }
        lines
    }

    /// The agent's configuration options and their current values.
    pub fn settings_summary(&self) -> String {
        if self.config_options.is_empty() {
            return "The agent has not advertised any settings.".into();
        }
        let mut lines =
            String::from("## Agent settings\nChange one with `/agent-config <id> <value>`.\n");
        for option in &self.config_options {
            let id = option["id"].as_str().unwrap_or("");
            let name = option["name"].as_str().unwrap_or(id);
            let current = config_value_label(option);
            let choices = config_choices(option);
            if choices.is_empty() {
                lines.push_str(&format!("- `{id}` — {name}: **{current}**\n"));
            } else {
                let values: Vec<&str> = choices.iter().map(|(_, value)| value.as_str()).collect();
                lines.push_str(&format!(
                    "- `{id}` — {name}: **{current}** ({})\n",
                    values.join(", ")
                ));
            }
        }
        lines
    }

    /// Set the agent's session mode (`session/set_mode`).
    pub async fn set_mode(
        &mut self,
        env: &Env,
        mode_id: &str,
        cancel: CancellationToken,
    ) -> Result<String> {
        self.ensure_env(env, &cancel).await?;
        if let Some(modes) = &self.modes
            && let Some(available) = modes["availableModes"].as_array()
            && !available.is_empty()
            && !available
                .iter()
                .any(|mode| mode["id"].as_str() == Some(mode_id))
        {
            let ids: Vec<&str> = available
                .iter()
                .filter_map(|mode| mode["id"].as_str())
                .collect();
            bail!(
                "The agent has no mode `{mode_id}`. Available: {}.",
                ids.join(", ")
            );
        }
        let session_id = self.current_session()?;
        self.call(
            env.frontend.as_ref(),
            &env.cwd(),
            env.mode(),
            &cancel,
            "session/set_mode",
            json!({"sessionId": session_id, "modeId": mode_id}),
        )
        .await?;
        if let Some(modes) = &mut self.modes {
            modes["currentModeId"] = json!(mode_id);
        }
        let name = self.mode_name(mode_id);
        Ok(format!("Agent mode: {name} (`{mode_id}`)."))
    }

    /// Set one of the agent's configuration options (`session/set_config_option`).
    pub async fn set_config_option(
        &mut self,
        env: &Env,
        config_id: &str,
        value: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<String> {
        self.ensure_env(env, &cancel).await?;
        let Some(index) = self
            .config_options
            .iter()
            .position(|option| option["id"].as_str() == Some(config_id))
        else {
            let ids: Vec<&str> = self
                .config_options
                .iter()
                .filter_map(|option| option["id"].as_str())
                .collect();
            if ids.is_empty() {
                bail!("The agent has not advertised any settings.");
            }
            bail!(
                "The agent has no setting `{config_id}`. Available: {}.",
                ids.join(", ")
            );
        };
        let option = self.config_options[index].clone();
        let name = option["name"].as_str().unwrap_or(config_id).to_string();
        let boolean = option["type"].as_str() == Some("boolean");
        let (value, label) = if boolean {
            let on = match value.map(str::to_ascii_lowercase).as_deref() {
                Some("true" | "on" | "yes" | "1") => true,
                Some("false" | "off" | "no" | "0") => false,
                Some(other) => bail!("`{other}` is not on or off for `{config_id}`."),
                None => {
                    let options = ["On".to_string(), "Off".to_string()];
                    let prompt = format!("{name}:");
                    let picked = if env.interactive {
                        tokio::select! {
                            picked = env.frontend.choose(&prompt, &options) => picked,
                            _ = cancel.cancelled() => None,
                        }
                    } else {
                        None
                    };
                    let Some(picked) = picked else {
                        bail!("Usage: `/agent-config {config_id} on|off`.");
                    };
                    picked == "On"
                }
            };
            (
                json!(on),
                if on {
                    "on".to_string()
                } else {
                    "off".to_string()
                },
            )
        } else {
            let choices = config_choices(&option);
            let raw = match value {
                Some(value) => value.to_string(),
                None => {
                    let labels: Vec<String> =
                        choices.iter().map(|(label, _)| label.clone()).collect();
                    let prompt = format!("{name}:");
                    let picked = if env.interactive && !labels.is_empty() {
                        tokio::select! {
                            picked = env.frontend.choose(&prompt, &labels) => picked,
                            _ = cancel.cancelled() => None,
                        }
                    } else {
                        None
                    };
                    let Some(picked) = picked else {
                        if choices.is_empty() {
                            bail!("Usage: `/agent-config {config_id} <value>`.");
                        }
                        let values: Vec<&str> =
                            choices.iter().map(|(_, value)| value.as_str()).collect();
                        bail!("Usage: `/agent-config {config_id} <{}>`.", values.join("|"));
                    };
                    picked
                }
            };
            // Accept the option's name as well as its value id.
            let value = choices
                .iter()
                .find(|(label, _)| *label == raw)
                .map(|(_, value)| value.clone())
                .or_else(|| {
                    choices
                        .iter()
                        .any(|(_, value)| *value == raw)
                        .then(|| raw.clone())
                })
                .unwrap_or(raw);
            if !choices.is_empty() && !choices.iter().any(|(_, id)| *id == value) {
                let values: Vec<&str> = choices.iter().map(|(_, value)| value.as_str()).collect();
                bail!(
                    "`{value}` is not a value for `{config_id}`. Available: {}.",
                    values.join(", ")
                );
            }
            let label = choices
                .iter()
                .find(|(_, id)| *id == value)
                .map(|(label, _)| label.clone())
                .unwrap_or_else(|| value.clone());
            (json!(value), label)
        };
        let session_id = self.current_session()?;
        let mut params = json!({
            "sessionId": session_id,
            "configId": config_id,
            "value": value,
        });
        if boolean {
            params["type"] = json!("boolean");
        }
        let result = self
            .call(
                env.frontend.as_ref(),
                &env.cwd(),
                env.mode(),
                &cancel,
                "session/set_config_option",
                params,
            )
            .await?;
        if let Some(options) = result["configOptions"].as_array() {
            self.config_options = options.clone();
        } else if let Some(option) = self.config_options.get_mut(index) {
            option["currentValue"] = value;
        }
        Ok(format!("Agent setting `{config_id}` ({name}): {label}."))
    }

    /// Sign in to the agent with one of its advertised methods.
    pub async fn login(
        &mut self,
        env: &Env,
        method: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<String> {
        let frontend = env.frontend.as_ref();
        if self.transport.is_none() {
            self.handshake(frontend, &env.cwd(), env.mode(), &cancel)
                .await?;
        }
        let methods = self.sign_in_methods();
        if methods.is_empty() {
            bail!(
                "{} does not offer a sign-in method that chanto can run.",
                self.name()
            );
        }
        let method_id = match method.filter(|method| !method.is_empty()) {
            Some(method) => {
                if !methods.iter().any(|(id, _)| id == method) {
                    let ids: Vec<&str> = methods.iter().map(|(id, _)| id.as_str()).collect();
                    bail!(
                        "The agent has no sign-in method `{method}`. Available: {}.",
                        ids.join(", ")
                    );
                }
                method.to_string()
            }
            None => {
                let labels: Vec<String> = methods
                    .iter()
                    .map(|(id, name)| format!("{name} (`{id}`)"))
                    .collect();
                let picked = if env.interactive {
                    tokio::select! {
                        picked = frontend.choose("Sign in to the agent:", &labels) => picked,
                        _ = cancel.cancelled() => None,
                    }
                } else {
                    None
                };
                let Some(picked) = picked else {
                    let list: Vec<String> = methods
                        .iter()
                        .map(|(id, name)| format!("- `{id}` — {name}"))
                        .collect();
                    return Ok(format!(
                        "Sign-in methods:\n{}\nRun `/agent-login <id>`.",
                        list.join("\n")
                    ));
                };
                methods
                    .iter()
                    .find(|(_, label)| *label == picked)
                    .map(|(id, _)| id.clone())
                    .context("the chosen sign-in method disappeared")?
            }
        };
        self.call(
            frontend,
            &env.cwd(),
            env.mode(),
            &cancel,
            "authenticate",
            json!({"methodId": method_id}),
        )
        .await?;
        Ok(format!("The agent accepted the `{method_id}` sign-in."))
    }

    /// Sign out (`logout`), when the agent advertises it.
    pub async fn logout(&mut self, env: &Env, cancel: CancellationToken) -> Result<String> {
        if self.transport.is_none() {
            self.handshake(env.frontend.as_ref(), &env.cwd(), env.mode(), &cancel)
                .await?;
        }
        if !self.caps.logout {
            bail!("{} does not support logout.", self.name());
        }
        self.call(
            env.frontend.as_ref(),
            &env.cwd(),
            env.mode(),
            &cancel,
            "logout",
            json!({}),
        )
        .await?;
        Ok("Signed out of the agent.".into())
    }

    async fn ensure_env(&mut self, env: &Env, cancel: &CancellationToken) -> Result<()> {
        let (servers, errors) = mcp_servers(&env.config.mcp_servers);
        for error in errors {
            env.frontend.emit(Event::Warn(format!("MCP: {error}")));
        }
        self.ensure(
            env.frontend.as_ref(),
            &env.cwd(),
            env.mode(),
            &servers,
            cancel,
        )
        .await
    }

    fn current_session(&self) -> Result<String> {
        self.session
            .as_ref()
            .map(|session| session.id.clone())
            .context("ACP session disappeared")
    }

    fn mode_name(&self, id: &str) -> String {
        self.modes
            .as_ref()
            .and_then(|modes| modes["availableModes"].as_array())
            .and_then(|modes| {
                modes
                    .iter()
                    .find(|mode| mode["id"].as_str() == Some(id))
                    .and_then(|mode| mode["name"].as_str())
            })
            .unwrap_or(id)
            .to_string()
    }

    /// Keep what a new, loaded or resumed session says about its modes,
    /// settings and commands, and show the initial values.
    fn learn_session(&mut self, frontend: &dyn Frontend, result: &Value) {
        if result["modes"].is_object() {
            self.modes = Some(result["modes"].clone());
            if let Some(modes) = &self.modes {
                let current = modes["currentModeId"].as_str().unwrap_or("");
                let available = modes["availableModes"].as_array();
                let names: Vec<String> = available
                    .into_iter()
                    .flatten()
                    .filter_map(|mode| {
                        let id = mode["id"].as_str()?;
                        let name = mode["name"].as_str().unwrap_or(id);
                        Some(if id == current {
                            format!("{name} (current)")
                        } else {
                            name.to_string()
                        })
                    })
                    .collect();
                if !names.is_empty() {
                    frontend.emit(Event::Info(format!(
                        "Agent modes: {}. Change with `/agent-mode <id>`.",
                        names.join(", ")
                    )));
                }
            }
        }
        if let Some(options) = result["configOptions"].as_array() {
            self.config_options = options.clone();
            if !options.is_empty() {
                let settings: Vec<String> = options
                    .iter()
                    .map(|option| {
                        let name = option["name"]
                            .as_str()
                            .or(option["id"].as_str())
                            .unwrap_or("setting");
                        format!("{name}: {}", config_value_label(option))
                    })
                    .collect();
                frontend.emit(Event::Info(format!(
                    "Agent settings — {}. Change with `/agent-config <id> <value>`.",
                    settings.join(", ")
                )));
            }
        }
        if let Some(commands) = result["availableCommands"].as_array() {
            self.commands = commands.clone();
        }
    }

    /// Sign-in methods Chanto can run; terminal methods need an interactive
    /// login that Chanto doesn't offer.
    fn sign_in_methods(&self) -> Vec<(String, String)> {
        self.auth_methods
            .iter()
            .filter(|method| method["type"].as_str() != Some("terminal"))
            .filter_map(|method| {
                let id = method["id"].as_str()?;
                let name = method["name"].as_str().unwrap_or(id);
                Some((id.to_string(), name.to_string()))
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)] // one turn's inputs as the caller has them
    async fn run_with(
        &mut self,
        frontend: &dyn Frontend,
        cwd: &Path,
        mode: Mode,
        input: &str,
        images: Vec<String>,
        mcp: &[Value],
        cancel: CancellationToken,
    ) -> Result<TurnEnd> {
        self.ensure(frontend, cwd, mode, mcp, &cancel).await?;
        let id = self.next_id();
        let session_id = self
            .session
            .as_ref()
            .context("ACP session disappeared")?
            .id
            .clone();
        self.session.as_mut().unwrap().text.clear();
        frontend.emit(Event::AssistantStart);
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": {"sessionId": session_id, "prompt": prompt_blocks(input, &images)},
        }))
        .await?;

        let mut cancelled = false;
        let mut deadline = tokio::time::Instant::now() + Duration::from_secs(3600);
        let result = loop {
            let next = {
                let transport = self
                    .transport
                    .as_mut()
                    .context("ACP agent is not running")?;
                tokio::select! {
                    _ = cancel.cancelled(), if !cancelled => Next::Cancel,
                    _ = tokio::time::sleep_until(deadline), if cancelled => Next::Expired,
                    message = async { transport.lock().await.recv().await } => Next::Message(message?),
                }
            };
            match next {
                Next::Cancel => {
                    cancelled = true;
                    deadline = tokio::time::Instant::now() + CANCEL_GRACE;
                    self.send(json!({
                        "jsonrpc": "2.0",
                        "method": "session/cancel",
                        "params": {"sessionId": session_id},
                    }))
                    .await?;
                    frontend.emit(Event::Info("Cancelling the agent…".into()));
                }
                Next::Expired => {
                    if let Some(t) = self.transport.take() {
                        t.lock().await.stop().await;
                    }
                    self.session = None;
                    frontend.emit(Event::Warn(
                        "The agent did not stop within 10s and was terminated.".into(),
                    ));
                    frontend.emit(Event::AssistantEnd);
                    return Ok(TurnEnd::Interrupted);
                }
                Next::Message(None) => {
                    let tail = self.tail();
                    self.transport = None;
                    self.session = None;
                    bail!("{} exited before finishing the turn{}", self.name(), tail);
                }
                Next::Message(Some(message)) => {
                    if is_response(&message, id) {
                        break message;
                    }
                    self.handle(frontend, cwd, mode, &cancel, &message).await?;
                }
            }
        };

        frontend.emit(Event::AssistantEnd);
        if !result["error"].is_null() {
            bail!("{}: {}", self.name(), error_text(&result));
        }
        let stop = result["result"]["stopReason"]
            .as_str()
            .unwrap_or("end_turn");
        let text = self
            .session
            .as_ref()
            .map(|s| s.text.trim().to_string())
            .unwrap_or_default();
        match stop {
            "cancelled" => Ok(TurnEnd::Interrupted),
            "max_tokens" => {
                frontend.emit(Event::Warn(
                    "The agent stopped at its token limit before finishing.".into(),
                ));
                Ok(TurnEnd::Done(done_text(text)))
            }
            "refusal" => {
                frontend.emit(Event::Warn("The agent refused the request.".into()));
                Ok(TurnEnd::Done(done_text(text)))
            }
            "max_turn_requests" => {
                frontend.emit(Event::Warn(
                    "The agent stopped at its request limit before finishing.".into(),
                ));
                Ok(TurnEnd::Done(done_text(text)))
            }
            _ => Ok(TurnEnd::Done(done_text(text))),
        }
    }

    async fn ensure(
        &mut self,
        frontend: &dyn Frontend,
        cwd: &Path,
        mode: Mode,
        mcp: &[Value],
        cancel: &CancellationToken,
    ) -> Result<()> {
        if self.transport.is_some() && self.session.is_some() {
            return Ok(());
        }
        if self.transport.is_none() {
            self.handshake(frontend, cwd, mode, cancel).await?;
        }
        // MCP servers from Chanto's config, minus ones the agent can't take.
        let mut passing = Vec::new();
        let mut skipped = Vec::new();
        for server in mcp {
            let name = server["name"].as_str().unwrap_or("server");
            match server["type"].as_str() {
                Some("http") if !self.caps.mcp_http => skipped.push(name.to_string()),
                Some("sse") if !self.caps.mcp_sse => skipped.push(name.to_string()),
                _ => passing.push(server.clone()),
            }
        }
        if !passing.is_empty() {
            let names: Vec<&str> = passing.iter().filter_map(|s| s["name"].as_str()).collect();
            frontend.emit(Event::Info(format!(
                "Giving the agent {} MCP server(s): {}.",
                names.len(),
                names.join(", ")
            )));
        }
        if !skipped.is_empty() {
            frontend.emit(Event::Warn(format!(
                "The agent does not support these MCP transports, so they were not passed on: {}.",
                skipped.join(", ")
            )));
        }

        let mut restored = false;
        if let Some(last) = self.last.clone() {
            let method = self.caps.restore_method();
            if let Some(method) = method {
                let params = json!({
                    "sessionId": last,
                    "cwd": cwd,
                    "mcpServers": passing.clone(),
                });
                // `session/load` replays the whole conversation first; the
                // Chanto already shows it, so the replay is not rendered.
                self.replaying = method == "session/load";
                let result = self.call(frontend, cwd, mode, cancel, method, params).await;
                self.replaying = false;
                match result {
                    Ok(result) => {
                        self.session = Some(Session {
                            id: last.clone(),
                            tools: HashMap::new(),
                            text: String::new(),
                        });
                        frontend.emit(Event::Info(format!(
                            "Restored the agent's previous session ({last})."
                        )));
                        self.learn_session(frontend, &result);
                        restored = true;
                    }
                    Err(e) => frontend.emit(Event::Warn(format!(
                        "Could not restore the agent's previous session: {e:#}. Starting a new one."
                    ))),
                }
            }
            if !restored {
                if self.caps.close {
                    let _ = self
                        .call(
                            frontend,
                            cwd,
                            mode,
                            cancel,
                            "session/close",
                            json!({"sessionId": last}),
                        )
                        .await;
                }
                self.last = None;
            }
        }
        if !restored {
            let session = self
                .call(
                    frontend,
                    cwd,
                    mode,
                    cancel,
                    "session/new",
                    json!({"cwd": cwd, "mcpServers": passing}),
                )
                .await?;
            let id = session["sessionId"]
                .as_str()
                .context("session/new returned no sessionId")?
                .to_string();
            self.last = Some(id.clone());
            self.session = Some(Session {
                id,
                tools: HashMap::new(),
                text: String::new(),
            });
            self.learn_session(frontend, &session);
        }
        Ok(())
    }

    /// Start the agent process and negotiate capabilities. Separate from
    /// [`ensure`] so sign-in can run before a session exists.
    async fn handshake(
        &mut self,
        frontend: &dyn Frontend,
        cwd: &Path,
        mode: Mode,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.spawn().await?;
        let init = self
            .call(
                frontend,
                cwd,
                mode,
                cancel,
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "clientCapabilities": {
                        "fs": {"readTextFile": true, "writeTextFile": true},
                        "terminal": true,
                        "elicitation": {"form": {}, "url": {}}
                    },
                    "clientInfo": {
                        "name": "chanto",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                }),
            )
            .await?;
        let protocol = init["protocolVersion"].as_u64().unwrap_or(0);
        if protocol != PROTOCOL_VERSION {
            bail!(
                "{} speaks ACP protocol {protocol}, but this chanto speaks {PROTOCOL_VERSION}",
                self.name()
            );
        }
        self.caps = AgentCaps::from_init(&init);
        self.auth_methods = init["authMethods"].as_array().cloned().unwrap_or_default();
        if !self.announced {
            self.announced = true;
            let name = init["agentInfo"]["name"].as_str().unwrap_or("agent");
            let version = init["agentInfo"]["version"].as_str().unwrap_or("");
            let version = if version.is_empty() {
                String::new()
            } else {
                format!(" {version}")
            };
            frontend.emit(Event::Info(format!(
                "Driving {name}{version} over ACP. /model and chanto tools do not apply; /clear starts a new agent session."
            )));
        }
        let auth = self.sign_in_methods();
        if !auth.is_empty() {
            let names: Vec<&str> = auth.iter().map(|(_, name)| name.as_str()).collect();
            frontend.emit(Event::Info(format!(
                "The agent offers sign-in methods ({}). If it asks for authentication, run `/agent-login`.",
                names.join(", ")
            )));
        }
        Ok(())
    }

    async fn spawn(&mut self) -> Result<()> {
        let Some(program) = self.command.first() else {
            bail!("no ACP agent command given");
        };
        let mut child = Command::new(program)
            .args(&self.command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "Could not start the ACP agent `{}`. Is it installed and on PATH?",
                    self.command.join(" ")
                )
            })?;
        let stdin = child.stdin.take().context("agent stdin")?;
        let stdout = child.stdout.take().context("agent stdout")?;
        if let Some(stderr) = child.stderr.take() {
            let tail = self.stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut tail = tail.lock().unwrap();
                    if tail.len() == TAIL {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
            });
        }
        self.transport = Some(tokio::sync::Mutex::new(Box::new(StdioTransport {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })));
        self.session = None;
        Ok(())
    }

    async fn call(
        &mut self,
        frontend: &dyn Frontend,
        cwd: &Path,
        mode: Mode,
        cancel: &CancellationToken,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let id = self.next_id();
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;
        loop {
            let message = self.recv().await?.with_context(|| {
                format!("{} exited during {method}{}", self.name(), self.tail())
            })?;
            if is_response(&message, id) {
                if !message["error"].is_null() {
                    let mut text = error_text(&message);
                    if message["error"]["code"].as_i64() == Some(AUTH_REQUIRED) {
                        text.push_str(
                            " — the agent needs a sign-in; run `/agent-login` to choose a method",
                        );
                    }
                    bail!("{} ({method}): {text}", self.name());
                }
                return Ok(message["result"].clone());
            }
            self.handle(frontend, cwd, mode, cancel, &message).await?;
        }
    }

    async fn send(&mut self, message: Value) -> Result<()> {
        let transport = self
            .transport
            .as_ref()
            .context("ACP agent is not running")?;
        transport.lock().await.send(&message).await
    }

    async fn recv(&mut self) -> Result<Option<Value>> {
        let transport = self
            .transport
            .as_ref()
            .context("ACP agent is not running")?;
        transport.lock().await.recv().await
    }

    /// Handle one message that is not the response we are waiting for:
    /// notifications (streamed updates) and requests from the agent.
    async fn handle(
        &mut self,
        frontend: &dyn Frontend,
        cwd: &Path,
        mode: Mode,
        cancel: &CancellationToken,
        message: &Value,
    ) -> Result<()> {
        let Some(method) = message["method"].as_str() else {
            return Ok(());
        };
        let id = message.get("id").cloned();
        match method {
            "session/update" => self.apply_update(frontend, &message["params"]["update"]),
            "session/request_permission" => {
                let result = self
                    .permission(frontend, mode, &message["params"], cancel)
                    .await;
                if let Some(id) = id {
                    self.respond(id, Ok(result)).await?;
                }
            }
            "fs/read_text_file" => {
                let result = read_text_file(cwd, &message["params"]).await;
                if let Some(id) = id {
                    self.respond(id, result.map(|content| json!({"content": content})))
                        .await?;
                }
            }
            "fs/write_text_file" => {
                let result = write_text_file(cwd, &message["params"]).await;
                if let Some(id) = id {
                    self.respond(id, result.map(|_| json!({}))).await?;
                }
            }
            "elicitation/create" => {
                let result = elicitation(frontend, &message["params"], cancel).await;
                if let Some(id) = id {
                    self.respond(id, Ok(result)).await?;
                }
            }
            "terminal/create" => {
                let result = self.create_terminal(&message["params"]);
                if let Some(id) = id {
                    self.respond(id, result).await?;
                }
            }
            "terminal/output" => {
                let result = self
                    .terminal(&message["params"])
                    .map(|terminal| terminal.output());
                if let Some(id) = id {
                    self.respond(id, result).await?;
                }
            }
            "terminal/wait_for_exit" => {
                let terminal = self.terminal(&message["params"]);
                if let Some(id) = id {
                    let result = match terminal {
                        Ok(terminal) => Ok(terminal.wait().await),
                        Err(e) => Err(e),
                    };
                    self.respond(id, result).await?;
                }
            }
            "terminal/kill" => {
                let result = self.terminal(&message["params"]).map(|terminal| {
                    terminal.kill();
                    json!({})
                });
                if let Some(id) = id {
                    self.respond(id, result).await?;
                }
            }
            "terminal/release" => {
                let result = match message["params"]["terminalId"].as_str() {
                    Some(terminal_id) => {
                        if let Some(terminal) = self.terminals.remove(terminal_id) {
                            terminal.kill();
                        }
                        Ok(json!({}))
                    }
                    None => Err(anyhow::anyhow!("terminalId is required")),
                };
                if let Some(id) = id {
                    self.respond(id, result).await?;
                }
            }
            _ => {
                if let Some(id) = id {
                    self.respond(id, Err(anyhow::anyhow!("unknown method {method}")))
                        .await?;
                }
            }
        }
        Ok(())
    }

    fn create_terminal(&mut self, params: &Value) -> Result<Value> {
        // `--restricted` is a hard no-exec guarantee for Chanto's own tools;
        // honour it here too so an external ACP agent's terminal/create can't
        // run commands the mode is meant to forbid.
        if crate::permissions::RESTRICTED.load(std::sync::atomic::Ordering::Relaxed) {
            anyhow::bail!("--restricted mode does not allow running terminal commands");
        }
        let terminal = Terminal::start(params)?;
        self.next_terminal += 1;
        let id = format!("term_{}", self.next_terminal);
        self.terminals.insert(id.clone(), terminal);
        Ok(json!({"terminalId": id}))
    }

    fn terminal(&self, params: &Value) -> Result<Arc<Terminal>> {
        let id = params["terminalId"]
            .as_str()
            .context("terminalId is required")?;
        self.terminals
            .get(id)
            .cloned()
            .with_context(|| format!("unknown terminal {id}"))
    }

    async fn respond(&mut self, id: Value, result: Result<Value>) -> Result<()> {
        let message = match result {
            Ok(value) => json!({"jsonrpc": "2.0", "id": id, "result": value}),
            Err(e) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32603, "message": format!("{e:#}")},
            }),
        };
        self.send(message).await
    }

    fn apply_update(&mut self, frontend: &dyn Frontend, update: &Value) {
        // The agent replays its conversation during `session/load`; the
        // Chanto transcript already has all of it.
        if self.replaying {
            return;
        }
        let Some(kind) = update["sessionUpdate"].as_str() else {
            return;
        };
        match kind {
            "agent_message_chunk" => {
                if let Some(text) = update["content"]["text"].as_str() {
                    if let Some(session) = &mut self.session {
                        session.text.push_str(text);
                    }
                    frontend.emit(Event::Text(text.to_string()));
                }
            }
            "agent_thought_chunk" => {
                if let Some(text) = update["content"]["text"].as_str() {
                    frontend.emit(Event::Reasoning(text.to_string()));
                }
            }
            "tool_call" => {
                let id = update["toolCallId"].as_str().unwrap_or("tool").to_string();
                let label = update["title"]
                    .as_str()
                    .or_else(|| update["kind"].as_str())
                    .unwrap_or("tool")
                    .to_string();
                if let Some(session) = &mut self.session {
                    session.tools.insert(id.clone(), label.clone());
                }
                frontend.emit(Event::ToolStart {
                    id: id.clone(),
                    label,
                    parent: None,
                });
                let progress = render_content(&update["content"]);
                if !progress.is_empty() {
                    frontend.emit(Event::ToolProgress { id, text: progress });
                }
            }
            "tool_call_update" => {
                let id = update["toolCallId"].as_str().unwrap_or("tool").to_string();
                let status = update["status"].as_str().unwrap_or("");
                let content = render_content(&update["content"]);
                if matches!(status, "completed" | "failed") {
                    let label = self
                        .session
                        .as_mut()
                        .and_then(|s| s.tools.remove(&id))
                        .or_else(|| update["title"].as_str().map(str::to_string))
                        .unwrap_or_else(|| "tool".into());
                    frontend.emit(Event::ToolEnd {
                        id,
                        display: label,
                        content,
                        is_error: status == "failed",
                        images: Vec::new(),
                        parent: None,
                    });
                } else if !content.is_empty() {
                    frontend.emit(Event::ToolProgress { id, text: content });
                }
            }
            "plan" => {
                let todos = update["entries"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|entry| Todo {
                        content: entry["content"].as_str().unwrap_or_default().to_string(),
                        status: match entry["status"].as_str().unwrap_or("pending") {
                            "completed" => "completed",
                            "in_progress" => "in_progress",
                            _ => "pending",
                        }
                        .into(),
                        active_form: entry["activeForm"].as_str().unwrap_or_default().to_string(),
                    })
                    .collect();
                frontend.emit(Event::Todos(todos));
            }
            "current_mode_update" => {
                let mode = update["currentModeId"].as_str().unwrap_or("mode");
                if let Some(modes) = &mut self.modes {
                    modes["currentModeId"] = json!(mode);
                }
                frontend.emit(Event::Info(format!("Agent mode: {mode}")));
            }
            "available_commands_update" => {
                if let Some(commands) = update["availableCommands"].as_array() {
                    let commands = commands.clone();
                    let names: Vec<String> = commands
                        .iter()
                        .filter_map(|command| command["name"].as_str())
                        .map(|name| format!("/{name}"))
                        .collect();
                    self.commands = commands;
                    if !self.commands_announced && !names.is_empty() {
                        self.commands_announced = true;
                        frontend.emit(Event::Info(format!(
                            "Agent commands: {}.",
                            names.join(", ")
                        )));
                    }
                }
            }
            "config_option_update" => {
                if let Some(options) = update["configOptions"].as_array() {
                    self.config_options = options.clone();
                }
                let options: Vec<String> = self
                    .config_options
                    .iter()
                    .filter_map(|option| {
                        let name = option["name"].as_str().or_else(|| option["id"].as_str())?;
                        Some(format!("{name}: {}", config_value_label(option)))
                    })
                    .collect();
                if !options.is_empty() {
                    frontend.emit(Event::Info(format!(
                        "Agent settings — {}.",
                        options.join(", ")
                    )));
                }
            }
            "user_message_chunk" => {}
            _ => {}
        }
    }

    async fn permission(
        &mut self,
        frontend: &dyn Frontend,
        mode: Mode,
        params: &Value,
        cancel: &CancellationToken,
    ) -> Value {
        let tool = &params["toolCall"];
        let options = params["options"].as_array().cloned().unwrap_or_default();
        let allow_once = option_id(&options, "allow_once");
        let allow_always = option_id(&options, "allow_always");
        let reject =
            option_id(&options, "reject_once").or_else(|| option_id(&options, "reject_always"));
        let title = tool["title"].as_str().unwrap_or("tool");
        if mode == Mode::Bypass
            && let Some(id) = allow_always.as_ref().or(allow_once.as_ref())
        {
            return selected(id);
        }
        let request = PermissionRequest {
            tool: tool["kind"].as_str().unwrap_or("tool").to_string(),
            label: title.to_string(),
            detail: permission_detail(tool),
            always_label: format!("Always allow {title}"),
            reason: None,
            proposed: None,
            auto_deny: None,
        };
        let reply = tokio::select! {
            reply = frontend.permission(request) => Some(reply),
            _ = cancel.cancelled() => None,
        };
        let Some(reply) = reply else {
            return json!({"outcome": {"outcome": "cancelled"}});
        };
        let pick = match &reply {
            PermissionReply::Allow => allow_once.as_ref().or(allow_always.as_ref()),
            PermissionReply::AllowEdited(_) => {
                frontend.emit(Event::Warn(
                    "Edited content cannot be passed to an external agent; allowing its own edit."
                        .into(),
                ));
                allow_once.as_ref().or(allow_always.as_ref())
            }
            PermissionReply::AllowAlways => allow_always.as_ref().or(allow_once.as_ref()),
            PermissionReply::Deny(_) | PermissionReply::AutoDenied(_) => reject.as_ref(),
        };
        match pick {
            Some(id) => selected(id),
            None => json!({"outcome": {"outcome": "cancelled"}}),
        }
    }

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn name(&self) -> String {
        self.command.join(" ")
    }

    fn tail(&self) -> String {
        let tail = self.stderr_tail.lock().unwrap();
        if tail.is_empty() {
            return String::new();
        }
        format!("\n{}", tail.iter().cloned().collect::<Vec<_>>().join("\n"))
    }
}

/// A command the agent asked the client to run. Output from stdout and stderr
/// is merged into one buffer, kept to the agent's byte limit from the end.
struct Terminal {
    output: Arc<Mutex<Vec<u8>>>,
    truncated: Arc<AtomicBool>,
    status: Arc<Mutex<Option<Value>>>,
    child: Arc<Mutex<Child>>,
    done: Arc<tokio::sync::Notify>,
}

impl Terminal {
    fn start(params: &Value) -> Result<Arc<Terminal>> {
        let command = params["command"].as_str().context("command is required")?;
        let mut process = Command::new(command);
        if let Some(args) = params["args"].as_array() {
            process.args(args.iter().filter_map(|arg| arg.as_str()));
        }
        for variable in params["env"].as_array().into_iter().flatten() {
            if let (Some(name), Some(value)) =
                (variable["name"].as_str(), variable["value"].as_str())
            {
                process.env(name, value);
            }
        }
        if let Some(cwd) = params["cwd"].as_str() {
            process.current_dir(cwd);
        }
        let mut child = process
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("could not start `{command}`"))?;
        let stdout = child.stdout.take().context("terminal stdout")?;
        let stderr = child.stderr.take().context("terminal stderr")?;
        let limit = params["outputByteLimit"]
            .as_u64()
            .map(|limit| limit as usize)
            .filter(|limit| *limit > 0)
            .unwrap_or(TERMINAL_LIMIT);
        let output = Arc::new(Mutex::new(Vec::new()));
        let truncated = Arc::new(AtomicBool::new(false));
        let mut readers = [
            tokio::spawn(pump(stdout, output.clone(), truncated.clone(), limit)),
            tokio::spawn(pump(stderr, output.clone(), truncated.clone(), limit)),
        ];
        let status = Arc::new(Mutex::new(None));
        let done = Arc::new(tokio::sync::Notify::new());
        let child = Arc::new(Mutex::new(child));
        let terminal = Arc::new(Terminal {
            output,
            truncated,
            status: status.clone(),
            child: child.clone(),
            done: done.clone(),
        });
        tokio::spawn(async move {
            loop {
                let exited = {
                    let mut child = child.lock().unwrap();
                    match child.try_wait() {
                        Ok(Some(status)) => Some(exit_status(status)),
                        Ok(None) => None,
                        Err(_) => Some(Value::Null),
                    }
                };
                if let Some(exited) = exited {
                    // Let the pumps drain the pipes before the exit status is
                    // visible, so a `terminal/output` after `wait_for_exit`
                    // sees the whole output. A daemon holding the pipes open
                    // must not hang the agent.
                    for reader in readers.iter_mut() {
                        let _ = tokio::time::timeout(Duration::from_secs(1), &mut *reader).await;
                    }
                    *status.lock().unwrap() = Some(exited);
                    done.notify_waiters();
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        Ok(terminal)
    }

    /// The output so far, with the exit status once the command has finished.
    fn output(&self) -> Value {
        let output = String::from_utf8_lossy(&self.output.lock().unwrap()).to_string();
        let mut result = json!({
            "output": output,
            "truncated": self.truncated.load(Ordering::Relaxed),
        });
        if let Some(status) = self.status.lock().unwrap().clone() {
            result["exitStatus"] = status;
        }
        result
    }

    async fn wait(&self) -> Value {
        loop {
            if let Some(status) = self.status.lock().unwrap().clone() {
                return status;
            }
            // `enable` registers interest before the second status check, so a
            // notification from either side of the check is not lost.
            let notified = self.done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(status) = self.status.lock().unwrap().clone() {
                return status;
            }
            notified.await;
        }
    }

    fn kill(&self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.start_kill();
        }
    }
}

async fn pump<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    output: Arc<Mutex<Vec<u8>>>,
    truncated: Arc<AtomicBool>,
    limit: usize,
) {
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let mut output = output.lock().unwrap();
                output.extend_from_slice(&buffer[..read]);
                if output.len() > limit {
                    let excess = output.len() - limit;
                    output.drain(..excess);
                    // Truncation must not split a UTF-8 character.
                    while output.first().is_some_and(|byte| byte & 0xC0 == 0x80) {
                        output.remove(0);
                    }
                    truncated.store(true, Ordering::Relaxed);
                }
            }
        }
    }
}

fn exit_status(status: std::process::ExitStatus) -> Value {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        json!({
            "exitCode": status.code(),
            "signal": status.signal().map(signal_name),
        })
    }
    #[cfg(not(unix))]
    {
        json!({"exitCode": status.code(), "signal": Value::Null})
    }
}

#[cfg(unix)]
fn signal_name(signal: i32) -> String {
    match signal {
        1 => "SIGHUP".into(),
        2 => "SIGINT".into(),
        3 => "SIGQUIT".into(),
        6 => "SIGABRT".into(),
        9 => "SIGKILL".into(),
        11 => "SIGSEGV".into(),
        13 => "SIGPIPE".into(),
        14 => "SIGALRM".into(),
        15 => "SIGTERM".into(),
        other => format!("SIG{other}"),
    }
}

/// Chanto's configured MCP servers as ACP `mcpServers` entries. Servers that
/// are off, or whose `${VAR}` has no value, are left out.
fn mcp_servers(
    config: &BTreeMap<String, crate::config::McpServerConfig>,
) -> (Vec<Value>, Vec<String>) {
    let (servers, errors) = crate::mcp::resolved(config);
    let mut entries = Vec::with_capacity(servers.len());
    for (name, server) in servers {
        match server.url.as_deref().filter(|url| !url.is_empty()) {
            Some(url) => {
                let kind = if server.transport.as_deref() == Some("sse") {
                    "sse"
                } else {
                    "http"
                };
                entries.push(json!({
                    "type": kind,
                    "name": name,
                    "url": url,
                    "headers": name_value_pairs(&server.headers),
                }));
            }
            None => entries.push(json!({
                "name": name,
                "command": command_path(&server.command),
                "args": server.args,
                "env": name_value_pairs(&server.env),
            })),
        }
    }
    (entries, errors)
}

fn name_value_pairs(pairs: &BTreeMap<String, String>) -> Vec<Value> {
    pairs
        .iter()
        .map(|(name, value)| json!({"name": name, "value": value}))
        .collect()
}

/// ACP asks for an absolute command path. Resolve a bare name on PATH; leave
/// one we can't find alone for the agent to interpret.
fn command_path(command: &str) -> String {
    if Path::new(command).is_absolute() {
        return command.to_string();
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return command.to_string();
    };
    for directory in std::env::split_paths(&paths) {
        let candidate = directory.join(command);
        if candidate.is_file() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    command.to_string()
}

async fn elicitation(frontend: &dyn Frontend, params: &Value, cancel: &CancellationToken) -> Value {
    let message = params["message"]
        .as_str()
        .unwrap_or("The agent needs some input.");
    match params["mode"].as_str() {
        Some("form") => form(frontend, message, &params["requestedSchema"], cancel).await,
        Some("url") => {
            let url = params["url"].as_str().unwrap_or("");
            if url.is_empty() {
                return json!({"action": "decline"});
            }
            frontend.emit(Event::Info(format!("{message}\n{url}")));
            let options = ["I opened it".to_string(), "Decline".to_string()];
            let answer = tokio::select! {
                answer = frontend.choose("Open the URL above and confirm.", &options) => answer,
                _ = cancel.cancelled() => None,
            };
            match answer.as_deref() {
                Some("I opened it") => json!({"action": "accept"}),
                Some("Decline") => json!({"action": "decline"}),
                _ => json!({"action": "cancel"}),
            }
        }
        _ => json!({"action": "decline"}),
    }
}

/// A flat form: each property is a string (possibly an enum), number, integer
/// or boolean, answered through the normal question UI.
async fn form(
    frontend: &dyn Frontend,
    message: &str,
    schema: &Value,
    cancel: &CancellationToken,
) -> Value {
    let required: Vec<&str> = schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|name| name.as_str())
        .collect();
    let Some(properties) = schema["properties"].as_object() else {
        return json!({"action": "accept", "content": {}});
    };
    let mut content = Map::new();
    for (name, property) in properties {
        let needed = required.contains(&name.as_str());
        let label = property["title"]
            .as_str()
            .or_else(|| property["description"].as_str())
            .unwrap_or(name);
        let prompt = format!("{message}\n\n{label}");
        let value = if let Some(choices) = choices(property) {
            let labels: Vec<String> = choices.iter().map(|(label, _)| label.clone()).collect();
            let picked = tokio::select! {
                picked = frontend.choose(&prompt, &labels) => picked,
                _ = cancel.cancelled() => None,
            };
            let Some(picked) = picked else {
                return json!({"action": "cancel"});
            };
            choices
                .into_iter()
                .find(|(label, _)| *label == picked)
                .map(|(_, value)| json!(value))
        } else if property["type"].as_str() == Some("boolean") {
            let options = ["Yes".to_string(), "No".to_string()];
            let picked = tokio::select! {
                picked = frontend.choose(&prompt, &options) => picked,
                _ = cancel.cancelled() => None,
            };
            let Some(picked) = picked else {
                return json!({"action": "cancel"});
            };
            Some(json!(picked == "Yes"))
        } else {
            let answer = tokio::select! {
                answer = frontend.question(&prompt, &[]) => answer,
                _ = cancel.cancelled() => None,
            };
            let Some(answer) = answer else {
                return json!({"action": "cancel"});
            };
            match property["type"].as_str() {
                Some("integer") => match answer.trim().parse::<i64>() {
                    Ok(number) => Some(json!(number)),
                    Err(_) => {
                        frontend.emit(Event::Warn(format!(
                            "`{answer}` is not a whole number; leaving {name} unset."
                        )));
                        None
                    }
                },
                Some("number") => match answer.trim().parse::<f64>() {
                    Ok(number) => Some(json!(number)),
                    Err(_) => {
                        frontend.emit(Event::Warn(format!(
                            "`{answer}` is not a number; leaving {name} unset."
                        )));
                        None
                    }
                },
                _ if answer.is_empty() && !needed => None,
                _ => Some(json!(answer)),
            }
        };
        match value {
            Some(value) => {
                content.insert(name.clone(), value);
            }
            None if needed => return json!({"action": "cancel"}),
            None => {}
        }
    }
    json!({"action": "accept", "content": content})
}

/// The choices a string property offers, as `(label, value)`.
fn choices(property: &Value) -> Option<Vec<(String, String)>> {
    if let Some(values) = property["enum"].as_array() {
        let choices: Vec<(String, String)> = values
            .iter()
            .filter_map(|value| value.as_str())
            .map(|value| (value.to_string(), value.to_string()))
            .collect();
        if !choices.is_empty() {
            return Some(choices);
        }
    }
    if let Some(options) = property["oneOf"].as_array() {
        let choices: Vec<(String, String)> = options
            .iter()
            .filter_map(|option| {
                let value = option["const"].as_str()?;
                Some((
                    option["title"].as_str().unwrap_or(value).to_string(),
                    value.to_string(),
                ))
            })
            .collect();
        if !choices.is_empty() {
            return Some(choices);
        }
    }
    None
}

enum Next {
    Cancel,
    Expired,
    Message(Option<Value>),
}

fn is_response(message: &Value, id: u64) -> bool {
    message.get("method").is_none()
        && message.get("id").and_then(Value::as_u64) == Some(id)
        && (message.get("result").is_some() || message.get("error").is_some())
}

fn error_text(message: &Value) -> String {
    let error = &message["error"];
    error["message"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string())
}

fn option_id(options: &[Value], kind: &str) -> Option<String> {
    options
        .iter()
        .find(|o| o["kind"].as_str() == Some(kind))
        .and_then(|o| o["optionId"].as_str().map(str::to_string))
}

/// A select option's `(label, value)` pairs, flattening groups. Grouped
/// choices are labelled `Group / Choice`.
fn config_choices(option: &Value) -> Vec<(String, String)> {
    let mut choices = Vec::new();
    for entry in option["options"].as_array().into_iter().flatten() {
        if entry.get("group").is_some() {
            let group = entry["name"].as_str().unwrap_or("");
            for choice in entry["options"].as_array().into_iter().flatten() {
                if let (Some(value), Some(name)) =
                    (choice["value"].as_str(), choice["name"].as_str())
                {
                    choices.push((format!("{group} / {name}"), value.to_string()));
                }
            }
        } else if let (Some(value), Some(name)) = (entry["value"].as_str(), entry["name"].as_str())
        {
            choices.push((name.to_string(), value.to_string()));
        }
    }
    choices
}

/// A configuration option's current value as a person reads it.
fn config_value_label(option: &Value) -> String {
    if option["type"].as_str() == Some("boolean") {
        return if option["currentValue"].as_bool().unwrap_or(false) {
            "on".into()
        } else {
            "off".into()
        };
    }
    let value = option["currentValue"].as_str().unwrap_or("");
    config_choices(option)
        .into_iter()
        .find(|(_, id)| id == value)
        .map(|(label, _)| label)
        .unwrap_or_else(|| value.to_string())
}

fn selected(id: &str) -> Value {
    json!({"outcome": {"outcome": "selected", "optionId": id}})
}

fn done_text(text: String) -> String {
    if text.is_empty() {
        "Done.".into()
    } else {
        text
    }
}

fn prompt_blocks(input: &str, images: &[String]) -> Vec<Value> {
    let mut blocks = Vec::with_capacity(images.len() + 1);
    if !input.is_empty() {
        blocks.push(json!({"type": "text", "text": input}));
    }
    for image in images {
        blocks.push(json!({
            "type": "image",
            "data": image,
            "mimeType": image_mime(image),
        }));
    }
    blocks
}

fn image_mime(data: &str) -> &'static str {
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
        return "image/png";
    };
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "image/png"
    }
}

/// The visible text of an ACP content array (text blocks, tool output, diffs).
fn render_content(content: &Value) -> String {
    let mut parts = Vec::new();
    for block in content.as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("content") => {
                if let Some(text) = block["content"]["text"].as_str() {
                    parts.push(text.to_string());
                }
            }
            Some("diff") => {
                let path = block["path"].as_str().unwrap_or("file");
                let new = block["newText"].as_str().unwrap_or("");
                parts.push(format!("{path}:\n{new}"));
            }
            Some("text") => {
                if let Some(text) = block["text"].as_str() {
                    parts.push(text.to_string());
                }
            }
            _ => {}
        }
    }
    truncate(parts.join("\n"), 8000)
}

fn permission_detail(tool: &Value) -> String {
    let mut parts = Vec::new();
    if !tool["rawInput"].is_null() {
        parts.push(serde_json::to_string_pretty(&tool["rawInput"]).unwrap_or_else(|_| "{}".into()));
    }
    let content = render_content(&tool["content"]);
    if !content.is_empty() {
        parts.push(content);
    }
    if parts.is_empty() {
        return tool["title"].as_str().unwrap_or_default().to_string();
    }
    truncate(parts.join("\n\n"), 4000)
}

fn truncate(mut text: String, max: usize) -> String {
    if text.chars().count() > max {
        text = text.chars().take(max).collect::<String>();
        text.push_str("\n…");
    }
    text
}

fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// The external ACP agent's `fs/read_text_file` and `fs/write_text_file` are
/// serviced by Chanto itself, bypassing its tool-permission layer. Confine
/// them to the working directory (plus the usual extra and scratch dirs), the
/// same boundary Chanto's own file tools honour, so a prompt-injected or
/// malicious agent can't silently read or overwrite `~/.ssh/id_rsa`, `~/.zshrc`
/// or anything else outside the project via an absolute or `..` path.
fn confine(path: &Path, cwd: &Path) -> Result<()> {
    if crate::permissions::in_workspace(path, cwd) {
        Ok(())
    } else {
        anyhow::bail!(
            "refused: {} is outside the working directory",
            path.display()
        )
    }
}

async fn read_text_file(cwd: &Path, params: &Value) -> Result<String> {
    let path = resolve(cwd, params["path"].as_str().context("path is required")?);
    confine(&path, cwd)?;
    let text = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    let (line, limit) = (params["line"].as_u64(), params["limit"].as_u64());
    if line.is_none() && limit.is_none() {
        return Ok(text);
    }
    let start = line.unwrap_or(1).saturating_sub(1) as usize;
    let take = limit.map(|n| n as usize).unwrap_or(usize::MAX);
    Ok(text
        .lines()
        .skip(start)
        .take(take)
        .collect::<Vec<_>>()
        .join("\n"))
}

async fn write_text_file(cwd: &Path, params: &Value) -> Result<()> {
    let path = resolve(cwd, params["path"].as_str().context("path is required")?);
    confine(&path, cwd)?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create {}", parent.display()))?;
    }
    tokio::fs::write(&path, params["content"].as_str().unwrap_or(""))
        .await
        .with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

    #[tokio::test]
    async fn acp_fs_is_confined_to_the_working_directory() {
        let dir = std::env::temp_dir().join(format!("acp-fs-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        // A write inside the working directory is allowed.
        write_text_file(
            &dir,
            &serde_json::json!({"path": "note.txt", "content": "hi"}),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("note.txt")).unwrap(), "hi");
        // Absolute and `..` paths that escape the working directory are refused,
        // before any read or write touches disk.
        for p in [
            "/etc/chanto-acp-nope",
            "../../etc/passwd",
            "/Users/x/.ssh/id_rsa",
        ] {
            let w = write_text_file(&dir, &serde_json::json!({"path": p, "content": "x"})).await;
            assert!(w.is_err(), "write {p} should be refused");
            assert!(
                w.unwrap_err()
                    .to_string()
                    .contains("outside the working directory")
            );
            assert!(
                read_text_file(&dir, &serde_json::json!({"path": p}))
                    .await
                    .is_err(),
                "read {p} should be refused"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    struct Fake {
        rx: UnboundedReceiver<Value>,
        sent: Arc<Mutex<Vec<Value>>>,
    }

    #[async_trait]
    impl Transport for Fake {
        async fn send(&mut self, message: &Value) -> Result<()> {
            self.sent.lock().unwrap().push(message.clone());
            Ok(())
        }

        async fn recv(&mut self) -> Result<Option<Value>> {
            Ok(self.rx.recv().await)
        }

        async fn stop(&mut self) {}
    }

    struct Recording {
        events: Mutex<Vec<Event>>,
        asked: Mutex<Vec<PermissionRequest>>,
        replies: Mutex<VecDeque<PermissionReply>>,
        answers: Mutex<VecDeque<Option<String>>>,
    }

    impl Recording {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                asked: Mutex::new(Vec::new()),
                replies: Mutex::new(VecDeque::new()),
                answers: Mutex::new(VecDeque::new()),
            }
        }

        fn reply(&self, reply: PermissionReply) {
            self.replies.lock().unwrap().push_back(reply);
        }

        fn answer(&self, answer: &str) {
            self.answers
                .lock()
                .unwrap()
                .push_back(Some(answer.to_string()));
        }

        fn cancel_answer(&self) {
            self.answers.lock().unwrap().push_back(None);
        }
    }

    #[async_trait]
    impl Frontend for Recording {
        fn emit(&self, event: Event) {
            self.events.lock().unwrap().push(event);
        }

        async fn permission(&self, request: PermissionRequest) -> PermissionReply {
            self.asked.lock().unwrap().push(request);
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(PermissionReply::Deny(None))
        }

        async fn question(&self, _: &str, _: &[String]) -> Option<String> {
            self.answers.lock().unwrap().pop_front().unwrap_or_default()
        }

        async fn choose(&self, _: &str, _: &[String]) -> Option<String> {
            self.answers.lock().unwrap().pop_front().unwrap_or_default()
        }
    }

    fn agent() -> (AcpAgent, UnboundedSender<Value>, Arc<Mutex<Vec<Value>>>) {
        let (tx, rx) = unbounded_channel();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut agent = AcpAgent::new(vec!["fake-agent".into()]);
        agent.transport = Some(tokio::sync::Mutex::new(Box::new(Fake {
            rx,
            sent: sent.clone(),
        })));
        agent.session = Some(Session {
            id: "s1".into(),
            tools: HashMap::new(),
            text: String::new(),
        });
        (agent, tx, sent)
    }

    #[tokio::test]
    async fn streams_text_tools_and_todos() {
        let (mut agent, tx, sent) = agent();
        let frontend = Recording::new();
        let cwd = std::env::temp_dir();
        for update in [
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hello "}}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "world"}}),
            json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Bash", "kind": "execute", "status": "pending", "rawInput": {"command": "ls"}, "content": [{"type": "content", "content": {"type": "text", "text": "ls"}}]}),
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed", "content": [{"type": "content", "content": {"type": "text", "text": "a.txt"}}]}),
            json!({"sessionUpdate": "plan", "entries": [{"content": "Read", "status": "completed", "activeForm": "Reading"}]}),
        ] {
            tx.send(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": update}})).unwrap();
        }
        tx.send(json!({"jsonrpc": "2.0", "id": 1, "result": {"stopReason": "end_turn"}}))
            .unwrap();
        let end = agent
            .run_with(
                &frontend,
                &cwd,
                Mode::Default,
                "do it",
                vec![],
                &[],
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(done_text("hello world".into()), "hello world");
        assert!(matches!(end, TurnEnd::Done(ref t) if t == "hello world"));
        let sent = sent.lock().unwrap();
        let prompt = &sent[0];
        assert_eq!(prompt["method"], "session/prompt");
        assert_eq!(prompt["params"]["prompt"][0]["text"], "do it");
        let events = frontend.events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Text(t) if t == "hello "))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Text(t) if t == "world"))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ToolStart { id, .. } if id == "t1"))
        );
        assert!(events.iter().any(|e| matches!(e, Event::ToolEnd { id, content, is_error: false, .. } if id == "t1" && content == "a.txt")));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Todos(t) if t.len() == 1 && t[0].content == "Read"))
        );
    }

    #[tokio::test]
    async fn answers_permission_from_the_frontend() {
        let (mut agent, tx, sent) = agent();
        let frontend = Recording::new();
        frontend.reply(PermissionReply::Allow);
        let cwd = std::env::temp_dir();
        tx.send(json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "session/request_permission",
            "params": {
                "sessionId": "s1",
                "toolCall": {"toolCallId": "t1", "title": "Run tests", "kind": "execute", "rawInput": {"command": "cargo test"}},
                "options": [
                    {"optionId": "allow", "name": "Allow once", "kind": "allow_once"},
                    {"optionId": "always", "name": "Always", "kind": "allow_always"},
                    {"optionId": "no", "name": "No", "kind": "reject_once"}
                ]
            }
        }))
        .unwrap();
        tx.send(json!({"jsonrpc": "2.0", "id": 1, "result": {"stopReason": "end_turn"}}))
            .unwrap();
        agent
            .run_with(
                &frontend,
                &cwd,
                Mode::Default,
                "run",
                vec![],
                &[],
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let asked = frontend.asked.lock().unwrap();
        assert_eq!(asked[0].label, "Run tests");
        assert!(asked[0].detail.contains("cargo test"));
        let sent = sent.lock().unwrap();
        let response = sent
            .iter()
            .find(|m| m["id"] == 7 && m.get("method").is_none())
            .unwrap();
        assert_eq!(response["result"]["outcome"]["optionId"], "allow");
    }

    #[tokio::test]
    async fn reads_and_writes_files_relative_to_the_session() {
        let dir =
            std::env::temp_dir().join(format!("chanto-acp-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("in.txt"), "one\ntwo\nthree\n").unwrap();
        let (mut agent, tx, sent) = agent();
        let frontend = Recording::new();
        tx.send(json!({"jsonrpc": "2.0", "id": 10, "method": "fs/read_text_file", "params": {"path": "in.txt", "line": 2, "limit": 1}})).unwrap();
        tx.send(json!({"jsonrpc": "2.0", "id": 11, "method": "fs/write_text_file", "params": {"path": "out/out.txt", "content": "written"}})).unwrap();
        tx.send(json!({"jsonrpc": "2.0", "id": 1, "result": {"stopReason": "end_turn"}}))
            .unwrap();
        agent
            .run_with(
                &frontend,
                &dir,
                Mode::Default,
                "files",
                vec![],
                &[],
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let sent = sent.lock().unwrap();
        let read = sent.iter().find(|m| m["id"] == 10).unwrap();
        assert_eq!(read["result"]["content"], "two");
        assert_eq!(
            std::fs::read_to_string(dir.join("out/out.txt")).unwrap(),
            "written"
        );
        drop(sent);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cancel_sends_session_cancel_and_stops_the_turn() {
        let (mut agent, tx, sent) = agent();
        let frontend = Recording::new();
        let cwd = std::env::temp_dir();
        let cancel = CancellationToken::new();
        let controller = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cancel.cancel();
            tokio::time::sleep(Duration::from_millis(10)).await;
            tx.send(json!({"jsonrpc": "2.0", "id": 1, "result": {"stopReason": "cancelled"}}))
                .unwrap();
        };
        let (end, ()) = tokio::join!(
            agent.run_with(
                &frontend,
                &cwd,
                Mode::Default,
                "stop",
                vec![],
                &[],
                cancel.clone()
            ),
            controller
        );
        assert!(matches!(end.unwrap(), TurnEnd::Interrupted));
        let sent = sent.lock().unwrap();
        assert!(
            sent.iter()
                .any(|m| m["method"] == "session/cancel" && m["params"]["sessionId"] == "s1")
        );
    }

    #[test]
    fn prompts_carry_image_mime_types() {
        // A one-pixel PNG header is enough to recognise the type.
        let png = base64::engine::general_purpose::STANDARD
            .encode([0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        let blocks = prompt_blocks("look", &[png]);
        assert_eq!(blocks[1]["mimeType"], "image/png");
        assert_eq!(blocks[1]["type"], "image");
    }

    #[test]
    fn content_and_diffs_are_rendered() {
        let content = json!([
            {"type": "content", "content": {"type": "text", "text": "ran"}},
            {"type": "diff", "path": "a.rs", "oldText": "old", "newText": "new"}
        ]);
        let text = render_content(&content);
        assert!(text.contains("ran"));
        assert!(text.contains("a.rs"));
        assert!(text.contains("new"));
    }

    /// The transport end to end: spawn a process, initialize it, open a
    /// session and stream one turn back through the frontend.
    #[cfg(unix)]
    #[tokio::test]
    async fn drives_a_process_over_stdio() {
        use std::os::unix::fs::PermissionsExt;

        let dir =
            std::env::temp_dir().join(format!("chanto-acp-e2e-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("agent.sh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"Fake agent","version":"1.0"},"authMethods":[]}}' ;;
    *'"method":"session/new"'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s9"}}' ;;
    *'"method":"session/prompt"'*)
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s9","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"from the agent"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}' ;;
  esac
done
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut agent = AcpAgent::new(vec![script.display().to_string()]);
        let frontend = Recording::new();
        let end = agent
            .run_with(
                &frontend,
                &dir,
                Mode::Default,
                "hello",
                vec![],
                &[],
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(end, TurnEnd::Done(ref t) if t == "from the agent"));
        assert!(
            frontend
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, Event::Text(t) if t == "from the agent"))
        );
        assert!(
            frontend
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, Event::Info(t) if t.contains("Fake agent")))
        );
        agent.reset();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capabilities_decide_how_a_session_is_restored() {
        let caps = AgentCaps::from_init(&json!({
            "agentCapabilities": {
                "loadSession": true,
                "mcpCapabilities": {"http": true}
            }
        }));
        assert_eq!(caps.restore_method(), Some("session/load"));
        assert!(caps.mcp_http);
        assert!(!caps.mcp_sse);
        assert!(!caps.close);

        let caps = AgentCaps::from_init(&json!({
            "agentCapabilities": {
                "loadSession": true,
                "sessionCapabilities": {"resume": {}, "close": {}}
            }
        }));
        assert_eq!(caps.restore_method(), Some("session/resume"));
        assert!(caps.close);

        let caps = AgentCaps::from_init(&json!({}));
        assert_eq!(caps.restore_method(), None);
    }

    #[test]
    fn configured_mcp_servers_become_acp_entries() {
        let mut config = BTreeMap::new();
        config.insert(
            "stdio".to_string(),
            crate::config::McpServerConfig {
                command: "echo".into(),
                args: vec!["hi".into()],
                env: [("TOKEN".to_string(), "secret".to_string())].into(),
                ..Default::default()
            },
        );
        config.insert(
            "remote".to_string(),
            crate::config::McpServerConfig {
                url: Some("https://example.com/mcp".into()),
                headers: [("Authorization".to_string(), "Bearer x".to_string())].into(),
                ..Default::default()
            },
        );
        config.insert(
            "legacy".to_string(),
            crate::config::McpServerConfig {
                url: Some("https://example.com/sse".into()),
                transport: Some("sse".into()),
                ..Default::default()
            },
        );
        config.insert(
            "off".to_string(),
            crate::config::McpServerConfig {
                command: "never".into(),
                disabled: true,
                ..Default::default()
            },
        );
        let (servers, errors) = mcp_servers(&config);
        assert!(errors.is_empty());
        assert_eq!(servers.len(), 3);
        let stdio = servers.iter().find(|s| s["name"] == "stdio").unwrap();
        assert_eq!(stdio["args"][0], "hi");
        assert_eq!(stdio["env"][0]["name"], "TOKEN");
        assert!(stdio.get("type").is_none());
        let remote = servers.iter().find(|s| s["name"] == "remote").unwrap();
        assert_eq!(remote["type"], "http");
        assert_eq!(remote["headers"][0]["value"], "Bearer x");
        let legacy = servers.iter().find(|s| s["name"] == "legacy").unwrap();
        assert_eq!(legacy["type"], "sse");
    }

    #[test]
    fn replayed_session_updates_are_not_shown_twice() {
        let (mut agent, _, _) = agent();
        let frontend = Recording::new();
        agent.replaying = true;
        agent.apply_update(
            &frontend,
            &json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "old"}}),
        );
        assert!(frontend.events.lock().unwrap().is_empty());
        agent.replaying = false;
        agent.apply_update(
            &frontend,
            &json!({"sessionUpdate": "available_commands_update", "availableCommands": [{"name": "review"}, {"name": "test"}]}),
        );
        assert!(frontend.events.lock().unwrap().iter().any(
            |e| matches!(e, Event::Info(text) if text.contains("/review") && text.contains("/test"))
        ));
    }

    #[tokio::test]
    async fn asks_the_user_for_form_input() {
        let frontend = Recording::new();
        // Properties are offered in name order: dry_run, environment, project.
        frontend.answer("Yes");
        frontend.answer("Staging");
        frontend.answer("acme");
        let request = json!({
            "mode": "form",
            "sessionId": "s1",
            "message": "Deployment details",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "project": {"type": "string", "title": "Project"},
                    "dry_run": {"type": "boolean", "description": "Dry run?"},
                    "environment": {"type": "string", "oneOf": [
                        {"const": "staging", "title": "Staging"},
                        {"const": "production", "title": "Production"}
                    ]}
                },
                "required": ["project", "environment"]
            }
        });
        let result = elicitation(&frontend, &request, &CancellationToken::new()).await;
        assert_eq!(result["action"], "accept");
        assert_eq!(result["content"]["project"], "acme");
        assert_eq!(result["content"]["dry_run"], true);
        assert_eq!(result["content"]["environment"], "staging");
    }

    #[tokio::test]
    async fn cancelling_a_form_declines_it() {
        let frontend = Recording::new();
        frontend.cancel_answer();
        let request = json!({
            "mode": "form",
            "sessionId": "s1",
            "message": "Need a name",
            "requestedSchema": {
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }
        });
        let result = elicitation(&frontend, &request, &CancellationToken::new()).await;
        assert_eq!(result["action"], "cancel");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_terminal_commands_for_the_agent() {
        let (mut agent, tx, sent) = agent();
        let frontend = Recording::new();
        tx.send(json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "terminal/create",
            "params": {
                "sessionId": "s1",
                "command": "/bin/sh",
                "args": ["-c", "printf 'hello\\n'"],
                "outputByteLimit": 1024
            }
        }))
        .unwrap();
        tx.send(json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "terminal/wait_for_exit",
            "params": {"sessionId": "s1", "terminalId": "term_1"}
        }))
        .unwrap();
        tx.send(json!({
            "jsonrpc": "2.0",
            "id": 22,
            "method": "terminal/output",
            "params": {"sessionId": "s1", "terminalId": "term_1"}
        }))
        .unwrap();
        tx.send(json!({"jsonrpc": "2.0", "id": 1, "result": {"stopReason": "end_turn"}}))
            .unwrap();
        agent
            .run_with(
                &frontend,
                &std::env::temp_dir(),
                Mode::Default,
                "run",
                vec![],
                &[],
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let sent = sent.lock().unwrap();
        let created = sent.iter().find(|m| m["id"] == 20).unwrap();
        assert_eq!(created["result"]["terminalId"], "term_1");
        let waited = sent.iter().find(|m| m["id"] == 21).unwrap();
        assert_eq!(waited["result"]["exitCode"], 0);
        let output = sent.iter().find(|m| m["id"] == 22).unwrap();
        assert_eq!(output["result"]["output"], "hello\n");
        assert_eq!(output["result"]["truncated"], false);
        drop(sent);
        agent.reset();
    }

    #[test]
    fn learns_modes_and_initial_settings() {
        let (mut agent, _tx, _sent) = agent();
        let frontend = Recording::new();
        agent.learn_session(
            &frontend,
            &json!({
                "modes": {
                    "currentModeId": "ask",
                    "availableModes": [
                        {"id": "ask", "name": "Ask"},
                        {"id": "plan", "name": "Plan"}
                    ]
                },
                "configOptions": [
                    {
                        "id": "model", "name": "Model", "type": "select",
                        "currentValue": "sonnet",
                        "options": [
                            {"value": "sonnet", "name": "Sonnet"},
                            {"value": "opus", "name": "Opus"}
                        ]
                    },
                    {
                        "id": "thinking", "name": "Thinking", "type": "boolean",
                        "currentValue": true
                    }
                ]
            }),
        );
        let modes = agent.modes_summary();
        assert!(modes.contains("Ask **(current)**"), "{modes}");
        assert!(modes.contains("Plan"), "{modes}");
        let settings = agent.settings_summary();
        assert!(settings.contains("Model: **Sonnet**"), "{settings}");
        assert!(settings.contains("Thinking: **on**"), "{settings}");
        assert!(settings.contains("`model` — Model"), "{settings}");
        let events = frontend.events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Info(text) if text.contains("Agent modes")))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Info(text) if text.contains("Model: Sonnet")))
        );
    }

    #[tokio::test]
    async fn changes_modes_and_settings_over_the_protocol() {
        use crate::agent::tests::{make_env, tmpdir};
        let dir = tmpdir("acp-settings");
        let env = make_env("http://127.0.0.1:1", &dir, Mode::Default, 32768);
        let (mut agent, tx, sent) = agent();
        agent.modes = Some(json!({
            "currentModeId": "ask",
            "availableModes": [
                {"id": "ask", "name": "Ask"},
                {"id": "plan", "name": "Plan"}
            ]
        }));
        agent.config_options = vec![
            json!({
                "id": "model", "name": "Model", "type": "select",
                "currentValue": "sonnet",
                "options": [
                    {"value": "sonnet", "name": "Sonnet"},
                    {"value": "opus", "name": "Opus"}
                ]
            }),
            json!({
                "id": "thinking", "name": "Thinking", "type": "boolean",
                "currentValue": false
            }),
        ];

        tx.send(json!({"jsonrpc": "2.0", "id": 1, "result": {}}))
            .unwrap();
        let message = agent
            .set_mode(&env, "plan", CancellationToken::new())
            .await
            .unwrap();
        assert!(message.contains("Plan"), "{message}");
        assert_eq!(agent.modes.as_ref().unwrap()["currentModeId"], "plan");

        tx.send(json!({
            "jsonrpc": "2.0", "id": 2,
            "result": {"configOptions": [
                {
                    "id": "model", "name": "Model", "type": "select",
                    "currentValue": "opus",
                    "options": [
                        {"value": "sonnet", "name": "Sonnet"},
                        {"value": "opus", "name": "Opus"}
                    ]
                },
                {
                    "id": "thinking", "name": "Thinking", "type": "boolean",
                    "currentValue": false
                }
            ]}
        }))
        .unwrap();
        // Accepts the option's name, not just its value id.
        let message = agent
            .set_config_option(&env, "model", Some("Opus"), CancellationToken::new())
            .await
            .unwrap();
        assert!(message.contains("Opus"), "{message}");
        assert_eq!(agent.config_options[0]["currentValue"], "opus");

        tx.send(json!({
            "jsonrpc": "2.0", "id": 3,
            "result": {"configOptions": [
                {
                    "id": "model", "name": "Model", "type": "select",
                    "currentValue": "opus",
                    "options": [
                        {"value": "sonnet", "name": "Sonnet"},
                        {"value": "opus", "name": "Opus"}
                    ]
                },
                {
                    "id": "thinking", "name": "Thinking", "type": "boolean",
                    "currentValue": true
                }
            ]}
        }))
        .unwrap();
        agent
            .set_config_option(&env, "thinking", Some("on"), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(agent.config_options[1]["currentValue"], true);

        let sent = sent.lock().unwrap();
        let set_mode = sent
            .iter()
            .find(|message| message["method"] == "session/set_mode")
            .unwrap();
        assert_eq!(set_mode["params"]["modeId"], "plan");
        let set_option = sent.iter().find(|message| message["id"] == 3).unwrap();
        assert_eq!(set_option["params"]["configId"], "thinking");
        assert_eq!(set_option["params"]["type"], "boolean");
        assert_eq!(set_option["params"]["value"], true);
    }

    #[tokio::test]
    async fn signs_in_with_an_advertised_method() {
        use crate::agent::tests::{make_env, tmpdir};
        let dir = tmpdir("acp-login");
        let env = make_env("http://127.0.0.1:1", &dir, Mode::Default, 32768);
        let (mut agent, tx, sent) = agent();
        agent.auth_methods = vec![
            json!({"id": "oauth", "name": "Sign in with GitHub"}),
            json!({"id": "shell", "name": "Run a login command"}),
        ];
        tx.send(json!({"jsonrpc": "2.0", "id": 1, "result": {}}))
            .unwrap();
        let message = agent
            .login(&env, Some("oauth"), CancellationToken::new())
            .await
            .unwrap();
        assert!(message.contains("oauth"), "{message}");
        let sent = sent.lock().unwrap();
        let authenticate = sent
            .iter()
            .find(|message| message["method"] == "authenticate")
            .unwrap();
        assert_eq!(authenticate["params"]["methodId"], "oauth");
    }

    #[test]
    fn adopts_the_saved_agent_session_only_before_one_starts() {
        let (mut saved, _tx, _sent) = agent();
        saved.transport = None;
        saved.adopt(Some("s9"));
        assert_eq!(saved.session_id(), Some("s9"));
        // A live session is already the conversation and stays put.
        let (mut live, _tx, _sent) = agent();
        live.adopt(Some("other"));
        assert_eq!(live.session_id(), None);
    }

    #[test]
    fn remembers_agent_commands_for_passthrough() {
        let (mut agent, _tx, _sent) = agent();
        let frontend = Recording::new();
        agent.apply_update(
            &frontend,
            &json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": [
                    {"name": "review", "description": "Review the diff"}
                ]
            }),
        );
        assert!(agent.has_command("review"));
        assert_eq!(
            agent.available_commands(),
            vec![("review".to_string(), "Review the diff".to_string())]
        );
        agent.apply_update(
            &frontend,
            &json!({"sessionUpdate": "available_commands_update", "availableCommands": []}),
        );
        assert!(!agent.has_command("review"));
    }
}
