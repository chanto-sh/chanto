//! LLM provider layer: a small common message model plus streaming clients for
//! Ollama's native API, any OpenAI-compatible server (llama.cpp, LM Studio,
//! vLLM, MLX, ...), Anthropic's Messages API and OpenAI's Responses API, plus
//! the cloud platforms that host them (Bedrock, Vertex AI, Azure).

mod anthropic;
mod bedrock;
mod cloud;
pub use cloud::CloudCheck;
#[cfg(test)]
mod cloud_tests;
#[cfg(test)]
mod hosted_tests;
pub(crate) mod limits;
#[cfg(test)]
pub(crate) mod mock;
#[cfg(test)]
mod overhead_tests;
mod responses;
mod safety;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub content: String,
    /// Base64-encoded images (png/jpeg) attached to the message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Opaque provider output (Anthropic content blocks with thinking
    /// signatures, OpenAI encrypted reasoning items) replayed verbatim on later
    /// requests to the same provider and model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_items: Option<ProviderItems>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderItems {
    pub kind: ProviderKind,
    pub model: String,
    pub items: Vec<Value>,
}

// By hand for `clone_from`, which the derive leaves at its default (a fresh
// clone): refreshing a copy of the conversation (`publish_conversation`)
// then reuses each message's buffers instead of reallocating them all.
// Destructured without `..`, so a new field fails to compile until it's here.
impl Clone for Message {
    fn clone(&self) -> Self {
        let Message {
            role,
            content,
            images,
            tool_calls,
            tool_call_id,
            tool_name,
            provider_items,
        } = self;
        Message {
            role: *role,
            content: content.clone(),
            images: images.clone(),
            tool_calls: tool_calls.clone(),
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            provider_items: provider_items.clone(),
        }
    }
    fn clone_from(&mut self, source: &Self) {
        let Message {
            role,
            content,
            images,
            tool_calls,
            tool_call_id,
            tool_name,
            provider_items,
        } = self;
        *role = source.role;
        content.clone_from(&source.content);
        images.clone_from(&source.images);
        tool_calls.clone_from(&source.tool_calls);
        tool_call_id.clone_from(&source.tool_call_id);
        tool_name.clone_from(&source.tool_name);
        provider_items.clone_from(&source.provider_items);
    }
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Message {
            role,
            content: content.into(),
            images: vec![],
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,
            provider_items: None,
        }
    }
    pub fn system(c: impl Into<String>) -> Self {
        Self::new(Role::System, c)
    }
    pub fn user(c: impl Into<String>) -> Self {
        Self::new(Role::User, c)
    }
    pub fn assistant(c: impl Into<String>) -> Self {
        Self::new(Role::Assistant, c)
    }
    pub fn tool(call: &ToolCall, content: impl Into<String>) -> Self {
        let mut m = Self::new(Role::Tool, content);
        m.tool_call_id = Some(call.id.clone());
        m.tool_name = Some(call.name.clone());
        m
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// What to do about the common ways a model call fails (`msg` is the whole
/// error chain): the raw error says what went wrong, this says how to fix it.
pub fn failure_hint(
    msg: &str,
    kind: ProviderKind,
    base_url: &str,
    model: &str,
    provider_id: Option<&str>,
) -> Option<String> {
    let lower = msg.to_lowercase();
    if [
        "connection refused",
        "tcp connect error",
        "dns error",
        "failed to lookup address",
    ]
    .iter()
    .any(|s| lower.contains(s))
    {
        return Some(format!(
            "Can't reach the model server at {base_url}. Is it running? `chanto doctor` checks the setup"
        ));
    }
    // The request reached the server but the reply was cut off: a proxy or
    // the server dropped the connection, or it closed it early.
    if [
        "connection reset",
        "connection aborted",
        "connection closed before message completed",
        "error decoding response body",
        "error reading a body from connection",
        "incomplete message",
        "unexpected eof",
        "stream error",
    ]
    .iter()
    .any(|s| lower.contains(s))
    {
        return Some(format!(
            "Lost the connection to the model server at {base_url} mid-response: retry the turn, or `chanto doctor` checks the setup"
        ));
    }
    if kind == ProviderKind::Ollama && lower.contains("not found") && lower.contains("model") {
        // `foo/bar` with an unknown `foo` falls through to Ollama: say so in case a provider was meant.
        let provider_note = model
            .split_once('/')
            .filter(|(p, _)| !p.contains('.'))
            .map(|(p, _)| {
                format!(" (`{p}` isn't a provider chanto knows; `chanto providers` lists them)")
            })
            .unwrap_or_default();
        return Some(format!(
            "Ollama doesn't have `{model}`: `ollama pull {model}`, or `chanto models` to see what's installed{provider_note}"
        ));
    }
    // Chanto cloud's errors already say what to do (see `chanto_cloud::gateway_error`).
    if provider_id == Some(crate::chanto_cloud::PROVIDER_ID) {
        return None;
    }
    if lower.contains(" 401")
        || lower.contains("unauthorized")
        || lower.contains("invalid api key")
        || lower.contains("authentication fail")
    {
        // Only built-in providers can be logged in to; others are configured by hand.
        return Some(
            match provider_id.filter(|id| crate::providers::find(id).is_some()) {
                Some(id) => {
                    format!("The {id} API key was rejected: `chanto login {id}` replaces it")
                }
                None => {
                    "The API key was rejected: `chanto doctor` shows where it comes from".into()
                }
            },
        );
    }
    // 404 for most servers; LiteLLM and some gateways answer 400 "Invalid model name".
    let unknown_model = lower.contains(" 404")
        || (lower.contains(" 400")
            && [
                "invalid model",
                "unknown model",
                "model not found",
                "does not exist",
                "not a valid model",
            ]
            .iter()
            .any(|s| lower.contains(s)));
    if unknown_model && lower.contains("model") {
        let list = provider_id
            .map(|id| format!("chanto models {id}"))
            .unwrap_or_else(|| "chanto models".into());
        return Some(format!(
            "The server doesn't recognize model `{model}`: `{list}` lists what it offers"
        ));
    }
    None
}

/// Reasoning effort levels, lowest first. Each provider maps them onto what it accepts.
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The requested effort, if thinking is on, capped at `top` (the highest level the API takes).
pub fn effort_for(req: &ChatRequest, top: &str) -> Option<&'static str> {
    if req.think == Some(false) {
        return None;
    }
    let want = EFFORTS
        .iter()
        .position(|e| Some(*e) == req.effort.as_deref())?;
    let cap = EFFORTS
        .iter()
        .position(|e| *e == top)
        .unwrap_or(EFFORTS.len() - 1);
    Some(EFFORTS[want.min(cap)])
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub think: Option<bool>,
    /// Reasoning effort (one of `EFFORTS`) when thinking is on; `None` keeps the provider default.
    pub effort: Option<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Ask the server to constrain output to JSON (used by the classifier).
    pub json: bool,
}

#[derive(Debug, Clone)]
pub enum Delta {
    Text(String),
    Reasoning(String),
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct ChatResponse {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub finish_reason: Option<String>,
    /// See [`Message::provider_items`]; the agent stores it on the assistant message.
    pub provider_items: Option<ProviderItems>,
    /// Account rate limits reported by the provider on this call (ChatGPT plans).
    pub(crate) rate_limits: Option<crate::llm::limits::Snapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ProviderKind {
    /// Ollama's native `/api/chat`.
    #[default]
    #[serde(rename = "ollama")]
    Ollama,
    /// Any OpenAI-compatible `/chat/completions` server.
    #[serde(rename = "openai")]
    Openai,
    /// Anthropic Messages API (`/v1/messages`).
    #[serde(rename = "anthropic")]
    Anthropic,
    /// OpenAI Responses API (`/v1/responses`).
    #[serde(rename = "openai-responses", alias = "responses")]
    OpenaiResponses,
    /// Amazon Bedrock: Claude through InvokeModel, other models through Converse.
    #[serde(rename = "bedrock")]
    Bedrock,
    /// Google Vertex AI: Claude through `streamRawPredict`, Gemini and Model
    /// Garden models through the OpenAI-compatible endpoint.
    #[serde(rename = "vertex")]
    Vertex,
    /// Azure OpenAI v1 API (chat completions or Responses).
    #[serde(rename = "azure")]
    Azure,
    /// Azure AI Foundry: Claude through the Anthropic Messages API, other
    /// models through the OpenAI-compatible API.
    #[serde(rename = "foundry")]
    Foundry,
}

/// Splits a byte stream into lines without rescanning: a 10 MB line arriving
/// in 16 KB reads is scanned once, not once per read (which took a minute).
#[derive(Default)]
pub(crate) struct LineBuf {
    buf: Vec<u8>,
    /// The start of the next unread line.
    start: usize,
    /// Bytes already known to hold no newline.
    scanned: usize,
}

impl LineBuf {
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        // Compact once per read, rather than shifting the unread chunk for
        // every line. Retain capacity for the next network read.
        if self.start > 0 {
            self.buf.copy_within(self.start.., 0);
            self.buf.truncate(self.buf.len() - self.start);
            self.scanned -= self.start;
            self.start = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete line, with its `\n`.
    pub(crate) fn next_line(&mut self) -> Option<&[u8]> {
        match self.buf[self.scanned..].iter().position(|&b| b == b'\n') {
            Some(off) => {
                let start = self.start;
                self.scanned += off + 1;
                self.start = self.scanned;
                Some(&self.buf[start..self.start])
            }
            None => {
                self.scanned = self.buf.len();
                None
            }
        }
    }

    /// What's left after the stream ended: a last line without its `\n`.
    pub(crate) fn rest(&mut self) -> Vec<u8> {
        self.buf.drain(..self.start);
        self.start = 0;
        self.scanned = 0;
        std::mem::take(&mut self.buf)
    }
}

/// Default for `stream_idle_timeout`: how long a model stream may go silent
/// once it has started.
pub const STREAM_IDLE_DEFAULT: Duration = Duration::from_secs(120);

/// The model's response stream went silent after it had started.
#[derive(Debug)]
pub struct StreamStalled(pub Duration);

impl std::fmt::Display for StreamStalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the model server stopped sending mid-response: nothing for {}s (stream timed out; `stream_idle_timeout` in config sets the limit, 0 turns it off)",
            self.0.as_secs_f64()
        )
    }
}

impl std::error::Error for StreamStalled {}

/// A streamed response body. The wait for the first chunk is unbounded (a
/// local server can prefill for minutes before it sends anything); after
/// that, a gap longer than `idle` fails the read with [`StreamStalled`].
pub(crate) struct Body {
    inner: std::pin::Pin<Box<dyn futures::Stream<Item = reqwest::Result<Vec<u8>>> + Send>>,
    idle: Option<Duration>,
    started: bool,
}

impl Body {
    pub(crate) fn new(resp: reqwest::Response, idle: Option<Duration>) -> Body {
        Body {
            inner: Box::pin(resp.bytes_stream().map(|c| c.map(Vec::from))),
            idle,
            started: false,
        }
    }

    pub(crate) async fn next(&mut self) -> Option<Result<Vec<u8>>> {
        let next = self.inner.next();
        let r = match self.idle.filter(|_| self.started) {
            Some(idle) => match tokio::time::timeout(idle, next).await {
                Ok(r) => r,
                Err(_) => return Some(Err(StreamStalled(idle).into())),
            },
            None => next.await,
        };
        self.started = true;
        r.map(|c| c.map_err(Into::into))
    }
}

/// How long to wait before retry number `attempt` (from 0): the server's
/// `retry-after` seconds (at most a minute), else exponential backoff.
fn retry_wait(retry_after: Option<&str>, attempt: u32) -> Duration {
    retry_after
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|s| s.is_finite())
        .map(|s| Duration::from_secs_f64(s.clamp(0.0, 60.0)))
        .unwrap_or_else(|| Duration::from_millis(1000 << attempt.min(5)))
}

/// The server isn't listening (as opposed to slow or overloaded).
pub fn connection_refused(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("connection refused")
        || m.contains("actively refused")
        || m.contains("os error 61")
        || m.contains("os error 111")
        || m.contains("os error 10061")
}

/// The HTTP status in a model request error ("API error 503: …",
/// "Ollama error 404 Not Found: …", "… (HTTP 502)").
pub fn error_status(msg: &str) -> Option<u16> {
    ["API error ", "Ollama error ", "(HTTP "]
        .iter()
        .find_map(|pat| {
            let rest = &msg[msg.find(pat)? + pat.len()..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            (digits.len() == 3).then(|| digits.parse().ok()).flatten()
        })
}

/// Whether a failed model request should move to a fallback model: the model
/// is overloaded, unavailable or the server failed; never auth, billing,
/// rate-limit or request errors. A local server that isn't
/// running also counts as unavailable.
pub fn warrants_fallback(msg: &str) -> bool {
    if connection_refused(msg) {
        return true;
    }
    let lower = msg.to_lowercase();
    let model_missing = lower.contains("model")
        && (lower.contains("not found")
            || lower.contains("does not exist")
            || lower.contains("not available")
            || lower.contains("unavailable"));
    match error_status(msg) {
        Some(500 | 502 | 503 | 504 | 529) => true,
        Some(404) => lower.contains("model"),
        Some(_) => false,
        None => lower.contains("overloaded") || model_missing,
    }
}

/// The innermost cause of an HTTP client error chain ("connecting to …: error
/// sending request …: tcp connect error: Connection refused (os error 61)").
pub fn root_cause(msg: &str) -> &str {
    let last = msg.rsplit(": ").next().unwrap_or(msg).trim();
    if last.is_empty() { msg } else { last }
}

impl ProviderKind {
    /// The wire protocol's name for people.
    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::Ollama => "Ollama",
            ProviderKind::Openai => "OpenAI-compatible",
            ProviderKind::Anthropic => "Anthropic",
            ProviderKind::OpenaiResponses => "OpenAI Responses",
            ProviderKind::Bedrock => "Amazon Bedrock",
            ProviderKind::Vertex => "Google Vertex AI",
            ProviderKind::Azure => "Azure OpenAI",
            ProviderKind::Foundry => "Azure AI Foundry",
        }
    }

    pub fn parse(s: &str) -> Option<ProviderKind> {
        Some(match s {
            "ollama" => ProviderKind::Ollama,
            "openai" | "openai-compatible" => ProviderKind::Openai,
            "anthropic" => ProviderKind::Anthropic,
            "openai-responses" | "responses" => ProviderKind::OpenaiResponses,
            "bedrock" => ProviderKind::Bedrock,
            "vertex" => ProviderKind::Vertex,
            "azure" => ProviderKind::Azure,
            "foundry" => ProviderKind::Foundry,
            _ => return None,
        })
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            ProviderKind::Ollama => "http://localhost:11434",
            ProviderKind::Openai => "http://localhost:8080/v1",
            ProviderKind::Anthropic => "https://api.anthropic.com",
            ProviderKind::OpenaiResponses => "https://api.openai.com/v1",
            // Derived per request from region, project or resource.
            ProviderKind::Bedrock
            | ProviderKind::Vertex
            | ProviderKind::Azure
            | ProviderKind::Foundry => "",
        }
    }

    /// Hosted APIs (billed, large context, API key from the environment).
    pub fn is_hosted(self) -> bool {
        matches!(
            self,
            ProviderKind::Anthropic | ProviderKind::OpenaiResponses
        ) || self.is_cloud()
    }

    /// Cloud platforms that authenticate with the cloud's own credentials.
    pub fn is_cloud(self) -> bool {
        matches!(
            self,
            ProviderKind::Bedrock
                | ProviderKind::Vertex
                | ProviderKind::Azure
                | ProviderKind::Foundry
        )
    }

    /// The catalog id of a cloud platform (`bedrock`, `vertex`, …).
    pub fn cloud_id(self) -> Option<&'static str> {
        Some(match self {
            ProviderKind::Bedrock => "bedrock",
            ProviderKind::Vertex => "vertex",
            ProviderKind::Azure => "azure",
            ProviderKind::Foundry => "foundry",
            _ => return None,
        })
    }

    /// Whether to assume the model takes images when the server doesn't
    /// advertise capabilities. OpenAI-compatible servers (DeepSeek, OpenRouter,
    /// llama.cpp, vLLM, …) expose no `/api/show`, so we send images and let the
    /// endpoint reject them, rather than silently dropping a pasted screenshot.
    pub fn assumes_vision(self) -> bool {
        matches!(self, ProviderKind::Openai | ProviderKind::OpenaiResponses) || self.is_cloud()
    }

    pub(crate) fn key_env(self) -> Option<&'static str> {
        match self {
            ProviderKind::Anthropic => Some("ANTHROPIC_API_KEY"),
            ProviderKind::OpenaiResponses => Some("OPENAI_API_KEY"),
            _ => None,
        }
    }
}

/// Context window to budget against for a hosted model when none is configured.
pub fn hosted_context(kind: ProviderKind, model: &str) -> u32 {
    let m = model.to_ascii_lowercase();
    match kind {
        ProviderKind::Anthropic => {
            // Current Claude models have a 1M window; older ones 200k.
            let old = [
                "claude-3",
                "haiku-4-5",
                "sonnet-4-5",
                "opus-4-5",
                "opus-4-1",
                "sonnet-4-0",
                "opus-4-0",
                "-4-2025",
            ];
            if old.iter().any(|o| m.contains(o)) {
                200_000
            } else {
                1_000_000
            }
        }
        ProviderKind::OpenaiResponses => {
            if m.contains("gpt-4.1") || m.starts_with("gpt-6") {
                1_000_000
            } else if m.starts_with("gpt-5") || m.contains("codex") {
                400_000
            } else {
                200_000
            }
        }
        ProviderKind::Bedrock
        | ProviderKind::Vertex
        | ProviderKind::Azure
        | ProviderKind::Foundry => cloud::context(kind, &m),
        _ => 32_768,
    }
}

/// Model-metadata cache lifetime: a model's context window and vision support
/// are effectively fixed, so a day keeps startup instant (the fetch is paid at
/// most once per day) while still picking up a reconfigured or re-pulled model
/// by the next day.
const MODEL_INFO_TTL_SECS: u64 = 24 * 60 * 60;

fn model_info_cache_path() -> std::path::PathBuf {
    crate::config::home_dir().join("model-info-cache.json")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A fresh cached `(context length, vision)` for this endpoint+model, or `None`
/// when absent or older than the TTL. A `\0` separates the key's two parts so no
/// base URL / model name can collide with another pairing.
fn model_info_cached(base_url: &str, model: &str) -> Option<(u64, bool)> {
    let key = format!("{base_url}\u{0}{model}");
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(model_info_cache_path()).ok()?).ok()?;
    let e = v.get(&key)?;
    if unix_now().saturating_sub(e.get("t")?.as_u64()?) > MODEL_INFO_TTL_SECS {
        return None;
    }
    Some((e.get("ctx")?.as_u64()?, e.get("vision")?.as_bool()?))
}

/// Record `(context length, vision)` for this endpoint+model. Best-effort: a
/// write failure just means the next start fetches again. The temp-then-rename
/// keeps a concurrent reader from seeing a half-written file.
fn model_info_cache_store(base_url: &str, model: &str, val: (u64, bool)) {
    let path = model_info_cache_path();
    let mut v: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    let Some(obj) = v.as_object_mut() else { return };
    obj.insert(
        format!("{base_url}\u{0}{model}"),
        json!({ "ctx": val.0, "vision": val.1, "t": unix_now() }),
    );
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, v.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// MiniMax's API (international or China), which takes `reasoning_split`.
pub(crate) fn minimax_host(base_url: &str) -> bool {
    let host = reqwest::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    ["minimax.io", "minimaxi.com", "minimax.cn"]
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// Providers usable as `anthropic/<model>` and `openai/<model>` without any
/// configuration, when `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` is set.
/// A catalog provider (`groq`, `deepseek`, `chatgpt`, …) with whatever
/// credential is available: a saved key or login, or its usual env var.
pub fn builtin_provider(name: &str, model: &str) -> Option<Provider> {
    if name == crate::chanto_cloud::PROVIDER_ID {
        return crate::chanto_cloud::provider(model);
    }
    let h = crate::providers::find(name)?;
    let ctx = if h.kind.is_hosted() {
        hosted_context(h.kind, model)
    } else {
        crate::providers::context(h.id, model).unwrap_or(131_072)
    };
    // Cloud platforms resolve their credentials on the first request, so an
    // explicit `bedrock/<model>` always gets a provider and a precise error.
    if h.kind.is_cloud() {
        let key = match crate::auth::get(h.id) {
            Some(crate::auth::Credential::ApiKey { key }) => Some(key.expose().to_string()),
            _ => h
                .env
                .iter()
                .find_map(|v| std::env::var(v).ok().filter(|k| !k.is_empty())),
        };
        return Some(Provider::new(h.kind, "", key, ctx));
    }
    let mut p = match crate::providers::credential(h)? {
        crate::providers::Cred::Key(k) => Provider::new(h.kind, h.base_url, Some(k), ctx),
        crate::providers::Cred::Login | crate::providers::Cred::Cloud(_) => {
            Provider::new(h.kind, h.base_url, None, ctx)
        }
    };
    if matches!(
        crate::providers::credential(h),
        Some(crate::providers::Cred::Login)
    ) {
        p.login = Some(h.id.to_string());
        p.api_key = None;
    }
    // Billed as this provider, not another catalog entry at the same address
    // (a MiniMax plan key isn't charged MiniMax's per-token rates).
    p.billing_provider = Some(h.id.to_string());
    Some(p)
}

#[derive(Clone)]
pub struct Provider {
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: Option<String>,
    /// Provider id whose browser login supplies the token (see auth.rs).
    pub login: Option<String>,
    pub num_ctx: u32,
    pub keep_alive: Option<String>,
    pub accounting: Option<std::sync::Arc<crate::cost::Accounting>>,
    pub billing_provider: Option<String>,
    /// Region, project, resource, … for cloud platforms.
    pub cloud: crate::cloud::Settings,
    /// Environment for cloud credential discovery (tests supply their own).
    cloud_vars: Option<std::sync::Arc<crate::cloud::Vars>>,
    /// Sent with every request (Chanto cloud's `x-chanto-org`).
    pub headers: Vec<(&'static str, String)>,
    /// Where to list models instead of the API's own route: Chanto cloud's
    /// catalog, which spans both of its APIs.
    pub models_url: Option<String>,
    /// How long a started response stream may go silent (`stream_idle_timeout`).
    pub stream_idle: Option<Duration>,
    http: reqwest::Client,
}

pub fn new_call_id() -> String {
    format!("call_{}", &uuid::Uuid::new_v4().simple().to_string()[..12])
}

impl Provider {
    fn is_local(&self) -> bool {
        self.kind == ProviderKind::Ollama
            || reqwest::Url::parse(&self.base_url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .is_some_and(|host| {
                    host == "localhost"
                        || host
                            .trim_matches(['[', ']'])
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())
                })
    }

    /// The bearer token for this request: a login's current (refreshed) token, or the API key.
    pub async fn bearer(&self) -> Result<Option<String>> {
        match &self.login {
            Some(id) => Ok(Some(crate::auth::token(id).await?)),
            None => Ok(self.api_key.clone()),
        }
    }

    /// Remaining account credit, when the provider publishes a balance endpoint.
    /// Most API-compatible services don't, so this returns `None` rather than a
    /// misleading zero — the `/cost` command only shows a balance it can trust.
    pub async fn balance(&self) -> Option<String> {
        let token = self.bearer().await.ok().flatten()?;
        let base = self.base_url.trim_end_matches('/');
        // DeepSeek's balance lives at the API root, not under the `/v1` used for chat.
        let (url, kind) = if base.contains("deepseek.com") {
            (
                format!("{}/user/balance", base.trim_end_matches("/v1")),
                "deepseek",
            )
        } else if base.contains("openrouter.ai") {
            (format!("{base}/credits"), "openrouter")
        } else {
            return None;
        };
        let resp = self.http.get(url).bearer_auth(&token).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: serde_json::Value = resp.json().await.ok()?;
        match kind {
            "deepseek" => {
                let parts: Vec<String> = v["balance_infos"]
                    .as_array()?
                    .iter()
                    .filter_map(|b| {
                        Some(format!(
                            "{} {}",
                            b["total_balance"].as_str()?,
                            b["currency"].as_str()?
                        ))
                    })
                    .collect();
                (!parts.is_empty()).then(|| parts.join(" · "))
            }
            _ => {
                let total = v["data"]["total_credits"].as_f64()?;
                let used = v["data"]["total_usage"].as_f64()?;
                Some(format!(
                    "${:.2} of ${:.2} credit remaining",
                    total - used,
                    total
                ))
            }
        }
    }

    pub fn new(kind: ProviderKind, base_url: &str, api_key: Option<String>, num_ctx: u32) -> Self {
        let http = crate::http::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(300))
            // Plans used from third-party tools (Kimi Code) ask clients to identify themselves truthfully.
            .user_agent(concat!("chanto/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("http client");
        // Hosted APIs fall back to their usual environment variable for the key.
        let api_key = api_key.or_else(|| {
            kind.key_env()
                .and_then(|v| std::env::var(v).ok())
                .filter(|k| !k.is_empty())
        });
        Provider {
            kind,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            login: None,
            num_ctx,
            keep_alive: Some("30m".into()),
            accounting: None,
            billing_provider: None,
            cloud: Default::default(),
            cloud_vars: None,
            headers: Vec::new(),
            models_url: None,
            stream_idle: Some(STREAM_IDLE_DEFAULT),
            http,
        }
    }

    /// A response's body, read with this provider's stream idle limit.
    pub(crate) fn body(&self, resp: reqwest::Response) -> Body {
        Body::new(resp, self.stream_idle)
    }

    pub fn with_cloud(mut self, settings: crate::cloud::Settings) -> Self {
        self.cloud = settings;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_vars(mut self, vars: crate::cloud::Vars) -> Self {
        self.cloud_vars = Some(std::sync::Arc::new(vars));
        self
    }

    /// The environment for cloud credential discovery.
    pub(crate) fn vars(&self) -> crate::cloud::Vars {
        self.cloud_vars
            .as_deref()
            .cloned()
            .unwrap_or_else(crate::cloud::Vars::process)
    }

    /// The provider name prices are looked up under.
    pub fn billing_id(&self) -> String {
        self.billing_provider
            .clone()
            .or_else(|| self.kind.cloud_id().map(str::to_string))
            .unwrap_or_else(|| {
                crate::providers::CATALOG
                    .iter()
                    .find(|p| {
                        self.base_url.trim_end_matches('/') == p.base_url.trim_end_matches('/')
                    })
                    .map(|p| p.id.to_string())
                    .unwrap_or_else(|| {
                        if self.is_local() {
                            "local".into()
                        } else {
                            "unknown".into()
                        }
                    })
            })
    }

    /// Stream a chat completion, invoking `on_delta` for each text/reasoning
    /// fragment, and return the aggregated response. Secrets in the messages
    /// are masked first, whoever built the request: compaction, hooks, the
    /// classifier and every other side call go through here.
    pub async fn chat(
        &self,
        req: &ChatRequest,
        on_delta: &mut (dyn FnMut(Delta) + Send),
    ) -> Result<ChatResponse> {
        match crate::secrets::redact_request_messages(&req.messages) {
            Some(messages) => {
                let req = ChatRequest {
                    messages,
                    ..req.clone()
                };
                self.chat_masked(&req, on_delta).await
            }
            None => self.chat_masked(req, on_delta).await,
        }
    }

    /// [`Provider::chat`] for a request whose messages are already masked:
    /// the agent loop masks its conversation with a cache that skips the
    /// messages it has seen, rather than scanning all of them every turn.
    pub(crate) async fn chat_masked(
        &self,
        req: &ChatRequest,
        on_delta: &mut (dyn FnMut(Delta) + Send),
    ) -> Result<ChatResponse> {
        let start = std::time::Instant::now();
        if let Some(a) = &self.accounting {
            a.check()?;
        }
        let mut result = match self.kind {
            ProviderKind::Ollama => self.chat_ollama(req, on_delta).await,
            ProviderKind::Openai => self.chat_openai(req, on_delta).await,
            ProviderKind::Anthropic => self.chat_anthropic(req, on_delta).await,
            ProviderKind::OpenaiResponses => self.chat_responses(req, on_delta).await,
            ProviderKind::Bedrock => self.chat_bedrock(req, on_delta).await,
            ProviderKind::Vertex | ProviderKind::Foundry if self.cloud_claude(&req.model) => {
                self.chat_anthropic(req, on_delta).await
            }
            ProviderKind::Vertex | ProviderKind::Foundry => self.chat_openai(req, on_delta).await,
            ProviderKind::Azure if self.azure_responses(&req.model) => {
                self.chat_responses(req, on_delta).await
            }
            ProviderKind::Azure => self.chat_openai(req, on_delta).await,
        };
        if let Ok(response) = &mut result {
            response.show_safety_notice(on_delta);
        }
        if let Some(a) = &self.accounting {
            let provider = self.billing_id();
            // Local models and plan logins (ChatGPT) cost nothing per call.
            if (self.is_local()
                || self.login.as_deref() == Some("chatgpt")
                || provider == "chatgpt")
                && !a.has_override(&provider, &req.model)
                && let Ok(r) = &mut result
                && r.usage.cost.is_none()
            {
                r.usage.cost = Some(0.);
            }
            a.model_call(
                &provider,
                &req.model,
                result.as_ref().ok().map(|r| &r.usage),
                start.elapsed().as_secs_f64(),
            )
            .await;
        }
        result
    }

    /// How likely each of `labels` is as the whole reply, from the
    /// probabilities of the first output token (one token generated, no
    /// text to parse). Labels must start with different words. `None` when
    /// this server or model doesn't report token probabilities (Ollama's and
    /// OpenAI-compatible chat only); a model found not to is remembered, so
    /// it costs one failed request at most.
    pub async fn label_probs(&self, req: &ChatRequest, labels: &[&str]) -> Option<LabelProbs> {
        let masked =
            crate::secrets::redact_request_messages(&req.messages).map(|messages| ChatRequest {
                messages,
                ..req.clone()
            });
        let req = masked.as_ref().unwrap_or(req);

        static UNSUPPORTED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let key = format!("{}|{}", self.base_url, req.model);
        if !matches!(self.kind, ProviderKind::Ollama | ProviderKind::Openai)
            || UNSUPPORTED.lock().unwrap().contains(&key)
        {
            return None;
        }
        if let Some(a) = &self.accounting {
            a.check().ok()?;
        }
        let start = std::time::Instant::now();
        let (top, usage) = match self.kind {
            ProviderKind::Ollama => self.first_token_ollama(req).await,
            _ => self.first_token_openai(req).await,
        }
        .unwrap_or_default();
        if let Some(a) = &self.accounting {
            let mut usage = usage;
            if self.is_local() {
                usage.cost = Some(0.);
            }
            a.model_call(
                &self.billing_id(),
                &req.model,
                Some(&usage),
                start.elapsed().as_secs_f64(),
            )
            .await;
        }
        let probs = label_probs_from(&top, labels);
        if probs.is_none() {
            UNSUPPORTED.lock().unwrap().push(key);
        }
        probs
    }

    /// `(token, logprob)` alternatives for the first output token, from `/api/chat`.
    async fn first_token_ollama(&self, req: &ChatRequest) -> Option<(Vec<(String, f64)>, Usage)> {
        let mut options = json!({ "num_ctx": self.num_ctx, "num_predict": 1 });
        if let Some(t) = req.temperature {
            options["temperature"] = json!(t);
        }
        let mut body = json!({
            "model": req.model,
            "messages": Self::ollama_messages(&req.messages),
            "stream": false,
            "options": options,
            "logprobs": true,
            "top_logprobs": 10,
        });
        if req.think == Some(false) {
            body["think"] = json!(false);
        }
        if let Some(k) = &self.keep_alive {
            body["keep_alive"] = json!(k);
        }
        let resp = self
            .http
            .post(format!("{}/api/chat", self.base_url))
            .json(&body)
            .send()
            .await
            .ok()?;
        let v: Value = resp.json().await.ok()?;
        let usage = Usage {
            prompt_tokens: v["prompt_eval_count"].as_u64().unwrap_or(0),
            completion_tokens: v["eval_count"].as_u64().unwrap_or(0),
            ..Default::default()
        };
        Some((top_logprobs(&v["logprobs"][0]), usage))
    }

    /// `(token, logprob)` alternatives for the first output token, from `/chat/completions`.
    async fn first_token_openai(&self, req: &ChatRequest) -> Option<(Vec<(String, f64)>, Usage)> {
        let mut body = json!({
            "model": self.wire_model(&req.model),
            "stream": false,
            "max_tokens": 1,
            "logprobs": true,
            "top_logprobs": 10,
        });
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if req.think == Some(false) {
            // Every spelling of "don't think" these servers read: a reasoning
            // model would otherwise spend the one token thinking.
            body["chat_template_kwargs"] = json!({"enable_thinking": false});
            body["reasoning_effort"] = json!("none");
            body["thinking"] = json!({"type": "disabled"});
        }
        let resp = self
            .post_openai(&Self::openai_body(&body, &req.messages, &req.model))
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: Value = resp.json().await.ok()?;
        let u = &v["usage"];
        let usage = Usage {
            prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0),
            completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
            cached_tokens: u["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .or_else(|| u["prompt_cache_hit_tokens"].as_u64())
                .unwrap_or(0),
            ..Default::default()
        };
        Some((
            top_logprobs(&v["choices"][0]["logprobs"]["content"][0]),
            usage,
        ))
    }

    pub async fn list_models(&self) -> Result<Vec<String>> {
        if let Some(url) = &self.models_url {
            let mut rb = self.http.get(url);
            if let Some(k) = self.bearer().await? {
                rb = rb.bearer_auth(k);
            }
            let resp = rb.send().await?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                bail!(
                    "{}",
                    crate::chanto_cloud::gateway_error(status.as_u16(), &text)
                );
            }
            let v: Value = serde_json::from_str(&text)?;
            return Ok(v["models"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m["id"].as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default());
        }
        match self.kind {
            ProviderKind::Ollama => {
                let v: Value = self
                    .http
                    .get(format!("{}/api/tags", self.base_url))
                    .send()
                    .await?
                    .json()
                    .await?;
                Ok(v["models"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m["name"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default())
            }
            ProviderKind::Anthropic => self.anthropic_models().await,
            ProviderKind::Bedrock
            | ProviderKind::Vertex
            | ProviderKind::Azure
            | ProviderKind::Foundry => self.cloud_models().await,
            ProviderKind::Openai | ProviderKind::OpenaiResponses => {
                let mut rb = self.http.get(format!("{}/models", self.openai_base()));
                if let Some(k) = self.bearer().await? {
                    rb = rb.bearer_auth(k);
                }
                let resp = rb.send().await?;
                // A coding plan without a model list: the models its docs name.
                let plan =
                    crate::providers::plan_models(self.billing_provider.as_deref().unwrap_or(""));
                if !plan.is_empty() && matches!(resp.status().as_u16(), 404 | 405) {
                    return Ok(plan.iter().map(|m| m.to_string()).collect());
                }
                let v: Value = resp.error_for_status()?.json().await?;
                // Account catalogs expose slugs; the direct API exposes data/id.
                if self.login.as_deref() == Some("chatgpt") && v.get("models").is_some() {
                    return Ok(v["models"]
                        .as_array()
                        .context("ChatGPT model catalog is missing its models array")?
                        .iter()
                        .filter(|m| m["visibility"] == "list")
                        .filter_map(|m| m["slug"].as_str().map(String::from))
                        .collect());
                }
                Ok(v["data"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m["id"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default())
            }
        }
    }

    /// Model metadata: (max context length, supports vision). The network-backed
    /// kinds (an OpenAI-compatible server's `/models`, Ollama's `/api/show`) are
    /// cached on disk per (endpoint, model), so only the first start pays the
    /// round-trip — an OpenAI-compatible `/models` lookup otherwise blocks every
    /// startup ~600ms. The table-backed kinds need no network and aren't cached.
    pub async fn model_info(&self, model: &str) -> Option<(u64, bool)> {
        let cacheable = matches!(self.kind, ProviderKind::Openai | ProviderKind::Ollama);
        if cacheable && let Some(hit) = model_info_cached(&self.base_url, model) {
            return Some(hit);
        }
        let info = self.model_info_uncached(model).await;
        if cacheable && let Some(v) = info {
            model_info_cache_store(&self.base_url, model, v);
        }
        info
    }

    async fn model_info_uncached(&self, model: &str) -> Option<(u64, bool)> {
        match self.kind {
            ProviderKind::Ollama => {}
            ProviderKind::Anthropic => return self.anthropic_model_info(model).await,
            // No network: the context table is enough, and startup stays fast.
            ProviderKind::OpenaiResponses
            | ProviderKind::Bedrock
            | ProviderKind::Vertex
            | ProviderKind::Azure
            | ProviderKind::Foundry => {
                return Some((hosted_context(self.kind, model) as u64, true));
            }
            ProviderKind::Openai => {
                return self.openai_model_info(model).await.map(|ctx| (ctx, true));
            }
        }
        let v: Value = self
            .http
            .post(format!("{}/api/show", self.base_url))
            .json(&json!({ "model": model }))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        let ctx = v["model_info"]
            .as_object()
            .and_then(|o| {
                o.iter()
                    .find(|(k, _)| k.ends_with(".context_length"))
                    .and_then(|(_, v)| v.as_u64())
            })
            .unwrap_or(0);
        let vision = v["capabilities"]
            .as_array()
            .map(|a| a.iter().any(|c| c == "vision"))
            .unwrap_or(false);
        Some((ctx, vision))
    }

    /// Context window an OpenAI-compatible server reports for `model`: the
    /// model list's `max_model_len` (vLLM), `context_length` (OpenRouter-style
    /// catalogs), `max_context_length` (LM Studio) or `meta.n_ctx_train`
    /// (llama.cpp), else LiteLLM's `/model/info`. Quick, since startup waits.
    async fn openai_model_info(&self, model: &str) -> Option<u64> {
        let key = self.bearer().await.ok().flatten();
        let get = |url: String| {
            let mut rb = self
                .http
                .get(url)
                .timeout(std::time::Duration::from_secs(2));
            if let Some(k) = &key {
                rb = rb.bearer_auth(k);
            }
            async move {
                rb.send()
                    .await
                    .ok()?
                    .error_for_status()
                    .ok()?
                    .json::<Value>()
                    .await
                    .ok()
            }
        };
        let base = self.openai_base();
        let listed = get(format!("{base}/models")).await;
        let entry = listed
            .as_ref()
            .and_then(|v| v["data"].as_array()?.iter().find(|m| m["id"] == model));
        if let Some(m) = entry {
            let ctx = ["max_model_len", "context_length", "max_context_length"]
                .iter()
                .find_map(|k| m[*k].as_u64())
                .or_else(|| m["meta"]["n_ctx_train"].as_u64());
            if ctx.is_some() {
                return ctx;
            }
        }
        let v = get(format!("{}/model/info", base.trim_end_matches("/v1"))).await?;
        v["data"]
            .as_array()?
            .iter()
            .filter(|m| m["model_name"] == model)
            .find_map(|m| m["model_info"]["max_input_tokens"].as_u64())
    }

    /// The base for OpenAI-style paths: a bare server address gets `/v1`; a
    /// path that already names an API version (`/v1`, Z.ai's `/paas/v4`,
    /// Gemini's `/v1beta/openai`) is used as given.
    pub(crate) fn openai_base(&self) -> String {
        let versioned = reqwest::Url::parse(&self.base_url).ok().is_some_and(|u| {
            u.path_segments().into_iter().flatten().any(|s| {
                s.strip_prefix('v').is_some_and(|r| {
                    r.starts_with(|c: char| c.is_ascii_digit())
                        && r.chars().all(|c| c.is_ascii_alphanumeric())
                })
            })
        });
        if versioned {
            self.base_url.clone()
        } else {
            format!("{}/v1", self.base_url)
        }
    }

    /// POST with retries on rate limits and overload (429, 529, 5xx), honoring
    /// `retry-after`. Only the request is retried, so nothing has streamed yet.
    pub(crate) async fn post_retrying(
        &self,
        url: &str,
        body: &Value,
        headers: &[(&str, String)],
    ) -> Result<reqwest::Response> {
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        self.post_bytes_retrying(url, serde_json::to_vec(body)?, &headers)
            .await
    }

    /// [`Provider::post_retrying`] with a pre-serialized JSON body (signed requests).
    pub(crate) async fn post_bytes_retrying(
        &self,
        url: &str,
        body: Vec<u8>,
        headers: &[(String, String)],
    ) -> Result<reqwest::Response> {
        // Building transfers the serialized Vec into reqwest's shared body.
        // Cloned requests then share those bytes, including the first send.
        let mut rb = self
            .http
            .post(url)
            .header("content-type", "application/json")
            .body(body);
        for (k, v) in headers {
            rb = rb.header(k.as_str(), v);
        }
        let request = rb.build()?;
        let mut attempt = 0u32;
        loop {
            let resp = self
                .http
                .execute(
                    request
                        .try_clone()
                        .context("cloning a buffered model request")?,
                )
                .await
                .with_context(|| format!("connecting to {}", crate::util::truncate(url, 200)))?;
            let status = resp.status().as_u16();
            if resp.status().is_success() {
                return Ok(resp);
            }
            let retryable = matches!(status, 408 | 429 | 500 | 502 | 503 | 504 | 529);
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let wait = retry_wait(retry_after.as_deref(), attempt);
            let text = resp.text().await.unwrap_or_default();
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                let error = value.get("error").unwrap_or(&value);
                if let Some(error) = limits::quota_error(
                    error,
                    self.login.as_deref() == Some("chatgpt"),
                    retry_after.as_deref(),
                ) {
                    return Err(error.into());
                }
            }
            attempt += 1;
            if !retryable || attempt > 4 {
                if self.login.as_deref() == Some("chatgpt")
                    && status == 400
                    && text.contains("not supported")
                {
                    bail!(
                        "ChatGPT model access rejected (HTTP {status}): {}\nThe sign-in connected, but this model is unavailable for this ChatGPT account. Select a model available to your plan with `/model chatgpt/<model>`. OpenAI API access is configured separately as `openai/<model>`.",
                        crate::util::truncate(&text, 2000)
                    );
                }
                if self.billing_provider.as_deref() == Some(crate::chanto_cloud::PROVIDER_ID) {
                    bail!("{}", crate::chanto_cloud::gateway_error(status, &text));
                }
                if self.login.as_deref() == Some("chatgpt") && status == 401 {
                    bail!(
                        "ChatGPT sign-in rejected (HTTP 401). Run `chanto login chatgpt` again. chanto stores its login separately from Codex."
                    );
                }
                bail!("API error {status}: {}", crate::util::truncate(&text, 2000));
            }
            tokio::time::sleep(wait).await;
        }
    }

    // ---------------------------------------------------------------- Ollama

    fn ollama_messages(msgs: &[Message]) -> Vec<Value> {
        msgs.iter()
            .map(|m| {
                let mut o = json!({ "role": m.role, "content": m.content });
                if !m.images.is_empty() {
                    o["images"] = json!(m.images);
                }
                if !m.tool_calls.is_empty() {
                    o["tool_calls"] = Value::Array(
                        m.tool_calls
                            .iter()
                            .map(
                                |c| json!({"function": {"name": c.name, "arguments": c.arguments}}),
                            )
                            .collect(),
                    );
                }
                if let Some(n) = &m.tool_name {
                    o["tool_name"] = json!(n);
                }
                o
            })
            .collect()
    }

    async fn chat_ollama(
        &self,
        req: &ChatRequest,
        on_delta: &mut (dyn FnMut(Delta) + Send),
    ) -> Result<ChatResponse> {
        let mut options = json!({ "num_ctx": self.num_ctx });
        if let Some(t) = req.temperature {
            options["temperature"] = json!(t);
        }
        if let Some(m) = req.max_tokens {
            options["num_predict"] = json!(m);
        }
        let mut body = json!({
            "model": req.model,
            "messages": Self::ollama_messages(&req.messages),
            "stream": true,
            "options": options,
        });
        if !req.tools.is_empty() {
            body["tools"] = Value::Array(
                req.tools
                    .iter()
                    .map(|t| json!({"type":"function","function": t}))
                    .collect(),
            );
        }
        if let Some(t) = req.think {
            // gpt-oss takes a level; other models reject one, and the retry below drops `think`.
            body["think"] = match effort_for(req, "high") {
                Some(e) => json!(e),
                None => json!(t),
            };
        }
        if let Some(k) = &self.keep_alive {
            body["keep_alive"] = json!(k);
        }
        if req.json {
            body["format"] = json!("json");
        }
        let resp = self
            .http
            .post(format!("{}/api/chat", self.base_url))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("connecting to Ollama at {}", self.base_url))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            // Some models reject `think`; retry once without it.
            if req.think.is_some() && text.contains("think") {
                let mut r2 = req.clone();
                r2.think = None;
                return Box::pin(self.chat_ollama(&r2, on_delta)).await;
            }
            bail!("Ollama error {status}: {text}");
        }
        let mut out = ChatResponse::default();
        let mut watch = ReasoningWatch::default();
        let mut stream = self.body(resp);
        let mut lines = LineBuf::default();
        while let Some(chunk) = stream.next().await {
            lines.push(&chunk?);
            while let Some(line) = lines.next_line() {
                let line = String::from_utf8_lossy(line);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if line.starts_with("data:") || line.starts_with("event:") {
                    bail!(
                        "{OPENAI_SHAPED}: the server streams Server-Sent Events (`{}`), not Ollama's NDJSON",
                        crate::util::one_line(line, 80)
                    );
                }
                let v: Value = serde_json::from_str(line)
                    .with_context(|| format!("bad stream line: {line}"))?;
                if let Some(e) = v["error"].as_str() {
                    bail!("Ollama error: {e}");
                }
                if v.get("choices").is_some()
                    || v["object"]
                        .as_str()
                        .is_some_and(|o| o.starts_with("chat.completion"))
                {
                    bail!(
                        "{OPENAI_SHAPED}: the reply has OpenAI's `choices`, not Ollama's `message`"
                    );
                }
                let msg = &v["message"];
                if let Some(t) = msg["thinking"].as_str()
                    && !t.is_empty()
                {
                    out.reasoning.push_str(t);
                    on_delta(Delta::Reasoning(t.to_string()));
                    if req.think != Some(false) && watch.looping(&out.reasoning) {
                        return Err(ReasoningLoop {
                            chars: out.reasoning.len(),
                        }
                        .into());
                    }
                }
                if let Some(t) = msg["content"].as_str()
                    && !t.is_empty()
                {
                    out.text.push_str(t);
                    on_delta(Delta::Text(t.to_string()));
                }
                if let Some(calls) = msg["tool_calls"].as_array() {
                    for c in calls {
                        let f = &c["function"];
                        let args = match &f["arguments"] {
                            Value::String(s) => serde_json::from_str(s).unwrap_or(json!({})),
                            Value::Null => json!({}),
                            other => other.clone(),
                        };
                        out.tool_calls.push(ToolCall {
                            id: c["id"]
                                .as_str()
                                .map(String::from)
                                .unwrap_or_else(new_call_id),
                            name: f["name"].as_str().unwrap_or_default().to_string(),
                            arguments: args,
                        });
                    }
                }
                if v["done"].as_bool() == Some(true) {
                    out.usage.prompt_tokens = v["prompt_eval_count"].as_u64().unwrap_or(0);
                    out.usage.completion_tokens = v["eval_count"].as_u64().unwrap_or(0);
                    out.finish_reason = v["done_reason"].as_str().map(String::from);
                }
            }
        }
        // A whole (unstreamed) reply with no trailing newline.
        let rest = lines.rest();
        if let Ok(v) = serde_json::from_slice::<Value>(&rest)
            && (v.get("choices").is_some()
                || v["object"]
                    .as_str()
                    .is_some_and(|o| o.starts_with("chat.completion")))
        {
            bail!("{OPENAI_SHAPED}: the reply has OpenAI's `choices`, not Ollama's `message`");
        }
        Ok(out)
    }

    // ---------------------------------------------------------------- OpenAI

    /// Chat-completions messages. Tool calls that came back with
    /// `extra_content` (Gemini's thought signatures) carry it again on later
    /// requests to the same model.
    /// The `/chat/completions` request: `fields` (everything but the
    /// messages) and the messages, written straight from the conversation.
    /// Building them as a `Value` first copied the whole history into a tree
    /// on every call only to serialize and drop it.
    fn openai_body<'a>(fields: &'a Value, msgs: &'a [Message], model: &'a str) -> OpenaiBody<'a> {
        OpenaiBody {
            fields,
            messages: OpenaiMessages { msgs, model },
        }
    }

    /// POST a chat completion. Rate limits and overloads (429, 529, 5xx) are
    /// retried like [`Provider::post_retrying`]; other errors come back as the
    /// response, for the caller's optional-parameter handling.
    async fn post_openai(&self, body: &OpenaiBody<'_>) -> Result<reqwest::Response> {
        let mut attempt = 0u32;
        loop {
            let mut rb = self
                .http
                .post(self.openai_url("chat/completions").await?)
                .json(body);
            for (k, v) in self.auth_headers().await? {
                rb = rb.header(k, v);
            }
            let resp = rb
                .send()
                .await
                .with_context(|| format!("connecting to {}", self.base_url))?;
            let status = resp.status().as_u16();
            if !matches!(status, 408 | 429 | 500 | 502 | 503 | 504 | 529) {
                return Ok(resp);
            }
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let text = resp.text().await.unwrap_or_default();
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                let error = value.get("error").unwrap_or(&value);
                if let Some(error) = limits::quota_error(
                    error,
                    self.login.as_deref() == Some("chatgpt"),
                    retry_after.as_deref(),
                ) {
                    return Err(error.into());
                }
            }
            attempt += 1;
            if attempt > 4 {
                if self.billing_provider.as_deref() == Some(crate::chanto_cloud::PROVIDER_ID) {
                    bail!("{}", crate::chanto_cloud::gateway_error(status, &text));
                }
                bail!("API error {status}: {}", crate::util::truncate(&text, 2000));
            }
            tokio::time::sleep(retry_wait(retry_after.as_deref(), attempt - 1)).await;
        }
    }

    /// URL of an OpenAI-style endpoint (`chat/completions`, `responses`, `models`).
    pub(crate) async fn openai_url(&self, path: &str) -> Result<String> {
        match self.kind {
            ProviderKind::Vertex | ProviderKind::Azure | ProviderKind::Foundry => {
                self.cloud_openai_url(path).await
            }
            _ => Ok(format!("{}/{path}", self.openai_base())),
        }
    }

    /// Authentication headers for OpenAI-style requests.
    pub(crate) async fn auth_headers(&self) -> Result<Vec<(&'static str, String)>> {
        match self.kind {
            ProviderKind::Vertex | ProviderKind::Azure | ProviderKind::Foundry => {
                self.cloud_auth_headers(false).await
            }
            _ => Ok(self
                .bearer()
                .await?
                .map(|k| ("authorization", format!("Bearer {k}")))
                .into_iter()
                .chain(self.headers.iter().cloned())
                .collect()),
        }
    }

    async fn chat_openai(
        &self,
        req: &ChatRequest,
        on_delta: &mut (dyn FnMut(Delta) + Send),
    ) -> Result<ChatResponse> {
        let mut body = json!({
            "model": self.wire_model(&req.model),
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if !req.tools.is_empty() {
            body["tools"] = Value::Array(
                req.tools
                    .iter()
                    .map(|t| json!({"type":"function","function": t}))
                    .collect(),
            );
        }
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(m) = req.max_tokens {
            // Azure's reasoning deployments accept only the newer name.
            body[if self.kind == ProviderKind::Azure {
                "max_completion_tokens"
            } else {
                "max_tokens"
            }] = json!(m);
        }
        if req.json {
            body["response_format"] = json!({"type":"json_object"});
        }
        // Cloud APIs reject unknown fields, and their models have no chat template switch.
        if req.think == Some(false) && !self.kind.is_cloud() {
            // Chat templates (llama.cpp / vLLM) read the first; OpenRouter,
            // LiteLLM and Ollama read the second.
            body["chat_template_kwargs"] = json!({"enable_thinking": false});
            body["reasoning_effort"] = json!("none");
        } else if let Some(e) = effort_for(req, "xhigh") {
            // Only sent when the user picked a level; servers that reject it get the retry below.
            body["reasoning_effort"] = json!(e);
        }
        // MiniMax otherwise puts the thinking in the reply text inside <think>
        // tags; split out, it arrives as `reasoning_content` and is sent back.
        if minimax_host(&self.base_url) {
            body["reasoning_split"] = json!(true);
        }
        let mut resp = self
            .post_openai(&Self::openai_body(&body, &req.messages, &req.model))
            .await?;
        // A server that rejects an optional parameter (a thinking switch, a
        // sampling setting a reasoning model doesn't take) gets the request
        // again without it.
        for _ in 0..3 {
            if resp.status().is_success() {
                break;
            }
            let status = resp.status();
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let text = resp.text().await.unwrap_or_default();
            // A plan's usage limit: say so, with when it resets.
            if let Some(error) = serde_json::from_str::<Value>(&text).ok().and_then(|v| {
                limits::quota_error(
                    v.get("error").unwrap_or(&v),
                    self.login.as_deref() == Some("chatgpt"),
                    retry_after.as_deref(),
                )
            }) {
                return Err(error.into());
            }
            // DeepSeek's thinking mode wants every earlier reasoning_content back. A
            // session saved before Chanto kept it can't supply that, so this one
            // reply goes without thinking rather than failing.
            if status.as_u16() == 400
                && text.contains("reasoning_content")
                && text.contains("passed back")
                && body.get("thinking").is_none()
            {
                body["thinking"] = json!({"type": "disabled"});
                body.as_object_mut().unwrap().remove("reasoning_effort");
                resp = self
                    .post_openai(&Self::openai_body(&body, &req.messages, &req.model))
                    .await?;
                continue;
            }
            let optional = [
                "reasoning_effort",
                "chat_template_kwargs",
                "temperature",
                "stream_options",
            ];
            let Some(param) = optional.iter().find(|p| {
                status.is_client_error() && body.get(**p).is_some() && text.contains(**p)
            }) else {
                if self.billing_provider.as_deref() == Some(crate::chanto_cloud::PROVIDER_ID) {
                    bail!(
                        "{}",
                        crate::chanto_cloud::gateway_error(status.as_u16(), &text)
                    );
                }
                bail!("API error {status}: {text}");
            };
            body.as_object_mut().unwrap().remove(*param);
            resp = self
                .post_openai(&Self::openai_body(&body, &req.messages, &req.model))
                .await?;
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("API error {status}: {text}");
        }
        let mut out = ChatResponse::default();
        let mut watch = ReasoningWatch::default();
        // The reasoning came as `reasoning_content`, which these servers want back.
        let mut replay_reasoning = false;
        // (wire index, id, name, args, extra_content), in order of first appearance.
        // The wire index is a key, not a position: a server sending index 4e9
        // mustn't make us allocate four billion slots.
        let mut partial: Vec<(u64, String, String, String, Value)> = Vec::new();
        // `[DONE]` arrived: the server finished, rather than the connection just closing.
        let mut done = false;
        let mut stream = self.body(resp);
        let mut lines = LineBuf::default();
        'outer: while let Some(chunk) = stream.next().await {
            lines.push(&chunk?);
            while let Some(line) = lines.next_line() {
                let line = String::from_utf8_lossy(line);
                let line = line.trim();
                let Some(data) = line.strip_prefix("data:") else {
                    if line.starts_with('{') && ollama_shaped(line) {
                        bail!(
                            "This looks like an Ollama server (it streams NDJSON with `message` and `done`, not OpenAI's Server-Sent Events); set provider = \"ollama\" (or kind = \"ollama\" in its [providers.<name>] section), or point base_url at its OpenAI endpoint (…/v1)"
                        );
                    }
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    done = true;
                    break 'outer;
                }
                let v: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(e) = v.get("error") {
                    bail!("API error: {e}");
                }
                if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                    out.usage.prompt_tokens = u["prompt_tokens"].as_u64().unwrap_or(0);
                    out.usage.completion_tokens = u["completion_tokens"].as_u64().unwrap_or(0);
                    out.usage.cached_tokens = u["prompt_tokens_details"]["cached_tokens"]
                        .as_u64()
                        .or_else(|| u["prompt_cache_hit_tokens"].as_u64())
                        .unwrap_or(0);
                    out.usage.cost = u["cost"].as_f64();
                }
                let Some(choice) = v["choices"].get(0) else {
                    continue;
                };
                let d = &choice["delta"];
                if let Some(t) = d["refusal"].as_str().filter(|t| !t.is_empty()) {
                    out.text.push_str(t);
                    on_delta(Delta::Text(t.to_string()));
                    out.finish_reason = Some("refusal".into());
                }
                // Some servers send the same text under both names.
                let reasoning = ["reasoning_content", "reasoning"]
                    .iter()
                    .find_map(|k| d[*k].as_str().filter(|t| !t.is_empty()).map(|t| (*k, t)));
                if let Some((key, t)) = reasoning {
                    replay_reasoning |= key == "reasoning_content";
                    out.reasoning.push_str(t);
                    on_delta(Delta::Reasoning(t.to_string()));
                    if req.think != Some(false) && watch.looping(&out.reasoning) {
                        return Err(ReasoningLoop {
                            chars: out.reasoning.len(),
                        }
                        .into());
                    }
                }
                if let Some(t) = d["content"].as_str()
                    && !t.is_empty()
                {
                    out.text.push_str(t);
                    on_delta(Delta::Text(t.to_string()));
                }
                if let Some(tcs) = d["tool_calls"].as_array() {
                    for tc in tcs {
                        let slot = match tc["index"].as_u64() {
                            Some(i) => partial.iter().position(|p| p.0 == i),
                            // No index: a new id starts a new call, otherwise continue the last one.
                            None => match tc["id"].as_str() {
                                Some(id) if partial.last().map(|p| p.1 != id).unwrap_or(true) => {
                                    None
                                }
                                _ => partial.len().checked_sub(1),
                            },
                        };
                        let slot = slot.unwrap_or_else(|| {
                            let key = tc["index"].as_u64().unwrap_or_else(|| {
                                partial.iter().map(|p| p.0 + 1).max().unwrap_or(0)
                            });
                            partial.push((
                                key,
                                String::new(),
                                String::new(),
                                String::new(),
                                Value::Null,
                            ));
                            partial.len() - 1
                        });
                        let p = &mut partial[slot];
                        if let Some(id) = tc["id"].as_str() {
                            p.1 = id.to_string();
                        }
                        if let Some(n) = tc["function"]["name"].as_str() {
                            p.2.push_str(n);
                        }
                        match &tc["function"]["arguments"] {
                            Value::String(a) => p.3.push_str(a),
                            Value::Object(_) => p.3 = tc["function"]["arguments"].to_string(),
                            _ => {}
                        }
                        if let Some(e) = tc.get("extra_content").filter(|e| !e.is_null()) {
                            p.4 = e.clone();
                        }
                    }
                }
                if let Some(f) = choice["finish_reason"].as_str()
                    && (out.finish_reason.as_deref() != Some("refusal") || f == "content_filter")
                {
                    out.finish_reason = Some(f.to_string());
                }
            }
        }
        // The connection closed mid-call (no finish_reason, no [DONE]): the
        // arguments may be cut short, and repairing them would run, say, a
        // truncated Write. Fail the request instead.
        if !done && out.finish_reason.is_none() && partial.iter().any(|p| !p.2.is_empty()) {
            bail!(
                "the connection closed before the model finished its tool call (no finish_reason or [DONE])"
            );
        }
        partial.sort_by_key(|p| p.0);
        let mut extras = Vec::new();
        for (_, id, name, args, extra) in partial {
            if name.is_empty() {
                continue;
            }
            let id = if id.is_empty() { new_call_id() } else { id };
            if !extra.is_null() {
                extras.push(json!({"id": id, "extra_content": extra}));
            }
            let arguments = if args.trim().is_empty() {
                json!({})
            } else {
                parse_tool_args(&args)
            };
            out.tool_calls.push(ToolCall {
                id,
                name,
                arguments,
            });
        }
        // DeepSeek's thinking mode (and servers like it) reject a follow-up
        // request unless the assistant turn carries its `reasoning_content` again.
        if replay_reasoning && !out.reasoning.is_empty() {
            extras.push(json!({"reasoning_content": out.reasoning}));
        }
        if !extras.is_empty() {
            out.provider_items = Some(ProviderItems {
                kind: ProviderKind::Openai,
                model: req.model.clone(),
                items: extras,
            });
        }
        Ok(out)
    }
}

/// The model's hidden reasoning started repeating itself; retrying without
/// thinking beats waiting for it to hit the output limit.
#[derive(Debug)]
pub struct ReasoningLoop {
    pub chars: usize,
}

impl std::fmt::Display for ReasoningLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the model's reasoning started repeating itself after {} characters",
            self.chars
        )
    }
}

impl std::error::Error for ReasoningLoop {}

/// Spots a loop in streamed reasoning: two checks in a row (every 2000
/// characters) where most 5-word sequences of the last 300 words already
/// appeared earlier. Catches loops of any length; normal reasoning stays
/// under a third.
#[derive(Default)]
pub struct ReasoningWatch {
    checked: usize,
    strikes: u8,
}

impl ReasoningWatch {
    pub fn looping(&mut self, text: &str) -> bool {
        if text.len() < self.checked + 2000 {
            return false;
        }
        self.checked = text.len();
        let words: Vec<&str> = text.split_whitespace().collect();
        if words.len() < 500 {
            return false;
        }
        let (head, tail) = words.split_at(words.len() - 300);
        let earlier: std::collections::HashSet<&[&str]> = head.windows(5).collect();
        let total = tail.len() - 4;
        let repeated = tail.windows(5).filter(|w| earlier.contains(w)).count();
        if repeated as f64 >= 0.8 * total as f64 {
            self.strikes += 1;
        } else {
            self.strikes = 0;
        }
        self.strikes >= 2
    }
}

pub fn sniff_mime(b64: &str) -> &'static str {
    if b64.starts_with("iVBOR") {
        "image/png"
    } else if b64.starts_with("R0lG") {
        "image/gif"
    } else if b64.starts_with("UklG") {
        "image/webp"
    } else {
        "image/jpeg"
    }
}

/// The `(token, logprob)` pairs of one entry of a `logprobs` list (the
/// chosen token and its `top_logprobs` alternatives; the same shape in
/// OpenAI's and Ollama's responses).
fn top_logprobs(entry: &Value) -> Vec<(String, f64)> {
    let pair = |t: &Value| Some((t["token"].as_str()?.to_string(), t["logprob"].as_f64()?));
    let mut out: Vec<(String, f64)> = entry["top_logprobs"]
        .as_array()
        .map(|a| a.iter().filter_map(pair).collect())
        .unwrap_or_default();
    if out.is_empty()
        && let Some(p) = pair(entry)
    {
        out.push(p);
    }
    out
}

/// The first output token's probabilities, read as labels.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelProbs {
    /// Each label's absolute probability: the sum over the alternatives that
    /// read as it. Mass on anything else (prose, a label split into short
    /// pieces) counts for no label.
    pub mass: Vec<f64>,
    /// The label the single most likely token reads as, if it reads as one.
    pub top: Option<usize>,
}

/// The labels' probabilities among the top alternatives. A token counts for
/// a label only when it is that whole label, ignoring case and surrounding
/// blanks ("ALLOW", " allow"). A piece of one ("Al", "All", "Allo") could as
/// well begin prose ("All of this…"), so it counts for none: that keeps a
/// label's mass from being overstated, never understated in its rival's
/// favour. `None` when no alternative is a label.
fn label_probs_from(top: &[(String, f64)], labels: &[&str]) -> Option<LabelProbs> {
    let label_of = |token: &str| {
        let t = token.trim();
        labels.iter().position(|l| l.eq_ignore_ascii_case(t))
    };
    let mut mass = vec![0.0; labels.len()];
    for (token, lp) in top {
        if let Some(i) = label_of(token) {
            mass[i] += lp.exp();
        }
    }
    let best = top
        .iter()
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    (mass.iter().sum::<f64>() > 0.0).then(|| LabelProbs {
        top: best.and_then(|(t, _)| label_of(t)),
        mass,
    })
}

/// Best-effort fix for truncated JSON emitted by small models (unclosed
/// strings/brackets).
/// Marks tool-call arguments that weren't valid JSON and were repaired
/// (closed brackets, strings): they were probably cut off, so the agent
/// won't run an edit with them. Removed before the call is recorded.
pub const REPAIRED: &str = "__repaired";

/// The start of the error when the Ollama provider gets an OpenAI-style reply.
const OPENAI_SHAPED: &str = "This looks like an OpenAI-compatible server; set provider = \"openai\" (or kind = \"openai\" in its [providers.<name>] section) and point base_url at its /v1 endpoint";

/// A line of Ollama's `/api/chat` NDJSON stream (sent to the OpenAI provider).
fn ollama_shaped(line: &str) -> bool {
    serde_json::from_str::<Value>(line).is_ok_and(|v| {
        v["done"].is_boolean()
            && (v["message"].is_object() || v.get("response").is_some())
            && v.get("choices").is_none()
    })
}

/// Tool-call arguments as sent: parsed, or repaired and marked with
/// [`REPAIRED`], or kept as `__raw` when even repair fails.
pub fn parse_tool_args(raw: &str) -> Value {
    if let Ok(v) = serde_json::from_str(raw) {
        return v;
    }
    match serde_json::from_str::<Value>(&repair_json(raw)) {
        Ok(Value::Object(mut o)) => {
            o.insert(REPAIRED.into(), json!(true));
            Value::Object(o)
        }
        Ok(v) => v,
        Err(_) => json!({ "__raw": raw }),
    }
}

pub fn repair_json(s: &str) -> String {
    let mut out = String::from(s.trim());
    let mut stack = Vec::new();
    let mut in_str = false;
    let mut esc = false;
    for c in out.chars() {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    if in_str {
        out.push('"');
    }
    while let Some(c) = stack.pop() {
        out.push(c);
    }
    out
}

/// Some local models emit tool calls as text (Hermes/Qwen `<tool_call>` tags or
/// bare JSON) instead of using the native tool-call channel. Extract them.
pub fn extract_text_tool_calls(text: &str, known: &[String]) -> (String, Vec<ToolCall>) {
    let mut calls = Vec::new();
    let mut remaining = String::new();
    let mut rest = text;
    let tags = [
        ("<tool_call>", "</tool_call>"),
        ("<function_call>", "</function_call>"),
    ];
    loop {
        let found = tags
            .iter()
            .filter_map(|(o, c)| rest.find(o).map(|i| (i, *o, *c)))
            .min_by_key(|x| x.0);
        let Some((start, open, close)) = found else {
            remaining.push_str(rest);
            break;
        };
        remaining.push_str(&rest[..start]);
        let after = &rest[start + open.len()..];
        let (body, next) = match after.find(close) {
            Some(e) => (&after[..e], &after[e + close.len()..]),
            None => (after, ""),
        };
        if let Some(c) = parse_call_json(body.trim(), known) {
            calls.push(c);
        } else {
            remaining.push_str(&rest[start..start + open.len()]);
            remaining.push_str(body);
        }
        rest = next;
        if rest.is_empty() {
            break;
        }
    }
    if calls.is_empty() {
        // Bare JSON object as the entire message.
        let t = text.trim();
        let t = t
            .strip_prefix("```json")
            .or_else(|| t.strip_prefix("```"))
            .map(|s| s.trim_end_matches("```").trim())
            .unwrap_or(t);
        if t.starts_with('{')
            && t.ends_with('}')
            && let Some(c) = parse_call_json(t, known)
        {
            return (String::new(), vec![c]);
        }
    }
    (remaining.trim().to_string(), calls)
}

/// The byte offset where a text-protocol tool call begins in a reply, or
/// `text.len()` when the reply is all prose. Only a tag followed by JSON counts
/// as a call, so a `<tool_call>` mentioned inline — in backticks, or in a
/// sentence about the protocol — does not hide the rest of the answer.
pub fn tool_call_start(text: &str) -> usize {
    let mut best = text.len();
    for tag in ["<tool_call", "<function_call"] {
        let mut from = 0;
        while let Some(rel) = text[from..].find(tag) {
            let i = from + rel;
            let rest = &text[i + tag.len()..];
            let after = rest.strip_prefix('>').unwrap_or(rest).trim_start();
            if after.starts_with('{') || after.starts_with('[') {
                best = best.min(i);
                break;
            }
            from = i + tag.len();
        }
    }
    best
}

fn parse_call_json(body: &str, known: &[String]) -> Option<ToolCall> {
    let (v, repaired): (Value, bool) = match serde_json::from_str(body) {
        Ok(v) => (v, false),
        Err(_) => (serde_json::from_str(&repair_json(body)).ok()?, true),
    };
    let name = v["name"]
        .as_str()
        .or_else(|| v["function"]["name"].as_str())?
        .to_string();
    if !known.iter().any(|k| k == &name) {
        return None;
    }
    let args = v
        .get("arguments")
        .or_else(|| v.get("parameters"))
        .or_else(|| v["function"].get("arguments"))
        .cloned()
        .unwrap_or(json!({}));
    let mut args = match args {
        Value::String(s) => match parse_tool_args(&s) {
            v if v.get("__raw").is_some() => json!({}),
            v => v,
        },
        a => a,
    };
    if let (true, Some(o)) = (repaired, args.as_object_mut()) {
        o.insert(REPAIRED.into(), json!(true));
    }
    Some(ToolCall {
        id: new_call_id(),
        name,
        arguments: args,
    })
}

/// Rough token estimate (≈ 3.5 chars/token) for budgeting when the server
/// doesn't report usage.
pub fn estimate_tokens(msgs: &[Message]) -> u64 {
    let chars: usize = msgs
        .iter()
        .map(|m| {
            m.content.len()
                + m.tool_calls
                    .iter()
                    .map(|c| c.arguments.to_string().len() + c.name.len())
                    .sum::<usize>()
                + m.images.len() * 2000
        })
        .sum();
    (chars as f64 / 3.5) as u64
}

/// A `/chat/completions` body: the request's own fields, then its messages.
struct OpenaiBody<'a> {
    fields: &'a Value,
    messages: OpenaiMessages<'a>,
}

impl Serialize for OpenaiBody<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let fields = self.fields.as_object();
        let mut map = s.serialize_map(Some(fields.map_or(0, |f| f.len()) + 1))?;
        for (k, v) in fields.into_iter().flatten() {
            map.serialize_entry(k, v)?;
        }
        map.serialize_entry("messages", &self.messages)?;
        map.end()
    }
}

/// The conversation as `/chat/completions` messages. Replayed provider items
/// (a call's `extra_content`, a reply's `reasoning_content`) go back only to
/// the provider and model that produced them.
struct OpenaiMessages<'a> {
    msgs: &'a [Message],
    model: &'a str,
}

impl Serialize for OpenaiMessages<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        struct Content<'a>(&'a Message);
        impl Serialize for Content<'_> {
            fn serialize<S: serde::Serializer>(
                &self,
                s: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                let m = self.0;
                if m.images.is_empty() {
                    return s.serialize_str(&m.content);
                }
                let mut parts = s.serialize_seq(Some(m.images.len() + 1))?;
                parts.serialize_element(&json!({"type": "text", "text": m.content}))?;
                for img in &m.images {
                    parts.serialize_element(&json!({"type": "image_url", "image_url": {"url": format!("data:{};base64,{img}", sniff_mime(img))}}))?;
                }
                parts.end()
            }
        }
        struct Calls<'a>(&'a [ToolCall], Option<&'a ProviderItems>);
        impl Serialize for Calls<'_> {
            fn serialize<S: serde::Serializer>(
                &self,
                s: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                struct Function<'a>(&'a ToolCall);
                impl Serialize for Function<'_> {
                    fn serialize<S: serde::Serializer>(
                        &self,
                        s: S,
                    ) -> std::result::Result<S::Ok, S::Error> {
                        let mut f = s.serialize_map(Some(2))?;
                        f.serialize_entry("name", &self.0.name)?;
                        f.serialize_entry("arguments", &self.0.arguments.to_string())?;
                        f.end()
                    }
                }
                let mut seq = s.serialize_seq(Some(self.0.len()))?;
                for c in self.0 {
                    struct Call<'a>(&'a ToolCall, Option<&'a Value>);
                    impl Serialize for Call<'_> {
                        fn serialize<S: serde::Serializer>(
                            &self,
                            s: S,
                        ) -> std::result::Result<S::Ok, S::Error> {
                            let mut m = s.serialize_map(None)?;
                            m.serialize_entry("id", &self.0.id)?;
                            m.serialize_entry("type", "function")?;
                            m.serialize_entry("function", &Function(self.0))?;
                            if let Some(e) = self.1 {
                                m.serialize_entry("extra_content", &e["extra_content"])?;
                            }
                            m.end()
                        }
                    }
                    let extra = self
                        .1
                        .and_then(|p| p.items.iter().find(|i| i["id"] == c.id.as_str()));
                    seq.serialize_element(&Call(c, extra))?;
                }
                seq.end()
            }
        }
        let mut seq = s.serialize_seq(Some(self.msgs.len()))?;
        for m in self.msgs {
            struct Msg<'a>(&'a Message, Option<&'a ProviderItems>);
            impl Serialize for Msg<'_> {
                fn serialize<S: serde::Serializer>(
                    &self,
                    s: S,
                ) -> std::result::Result<S::Ok, S::Error> {
                    let (m, extra) = (self.0, self.1);
                    let mut o = s.serialize_map(None)?;
                    o.serialize_entry("role", &m.role)?;
                    o.serialize_entry("content", &Content(m))?;
                    if !m.tool_calls.is_empty() {
                        o.serialize_entry("tool_calls", &Calls(&m.tool_calls, extra))?;
                    }
                    if let Some(id) = &m.tool_call_id {
                        o.serialize_entry("tool_call_id", id)?;
                    }
                    if let Some(r) = extra
                        .and_then(|p| p.items.iter().find_map(|i| i["reasoning_content"].as_str()))
                    {
                        o.serialize_entry("reasoning_content", r)?;
                    }
                    o.end()
                }
            }
            let extra = m
                .provider_items
                .as_ref()
                .filter(|p| p.kind == ProviderKind::Openai && p.model == self.model);
            seq.serialize_element(&Msg(m, extra))?;
        }
        seq.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The messages as the request built them before `OpenaiMessages`: the
    /// reference it must match.
    fn reference_openai_messages(msgs: &[Message], model: &str) -> Vec<Value> {
        msgs.iter()
            .map(|m| {
                let extra = m.provider_items.as_ref().filter(|p| p.kind == ProviderKind::Openai && p.model == model);
                let content = if m.images.is_empty() {
                    json!(m.content)
                } else {
                    let mut parts = vec![json!({"type":"text","text": m.content})];
                    for img in &m.images {
                        parts.push(json!({"type":"image_url","image_url":{"url": format!("data:{};base64,{img}", sniff_mime(img))}}));
                    }
                    Value::Array(parts)
                };
                let mut o = json!({ "role": m.role, "content": content });
                if !m.tool_calls.is_empty() {
                    o["tool_calls"] = Value::Array(
                        m.tool_calls
                            .iter()
                            .map(|c| {
                                let mut v = json!({"id": c.id, "type":"function","function":{"name": c.name, "arguments": c.arguments.to_string()}});
                                if let Some(e) = extra.and_then(|p| p.items.iter().find(|i| i["id"] == c.id.as_str())) {
                                    v["extra_content"] = e["extra_content"].clone();
                                }
                                v
                            })
                            .collect(),
                    );
                }
                if let Some(id) = &m.tool_call_id {
                    o["tool_call_id"] = json!(id);
                }
                if let Some(r) = extra.and_then(|p| p.items.iter().find_map(|i| i["reasoning_content"].as_str())) {
                    o["reasoning_content"] = json!(r);
                }
                o
            })
            .collect()
    }

    #[test]
    fn openai_messages_serialize_as_the_value_tree_did() {
        let call = |id: &str, args: Value| ToolCall {
            id: id.into(),
            name: "Read".into(),
            arguments: args,
        };
        let mut assistant = Message::assistant("reading \"two\" files\n\u{1}");
        assistant.tool_calls = vec![
            call("c1", json!({"file_path": "a.rs"})),
            call(
                "c2",
                json!({"file_path": "b.rs", "limit": 5, "nested": [1, null, {"x": "é"}]}),
            ),
        ];
        assistant.provider_items = Some(ProviderItems {
            kind: ProviderKind::Openai,
            model: "m".into(),
            items: vec![
                json!({"id": "c1", "extra_content": {"google": {"thought_signature": "sig"}}}),
                json!({"id": "c2"}),
                json!({"reasoning_content": "thought about it"}),
            ],
        });
        let mut other_model = assistant.clone();
        other_model.provider_items.as_mut().unwrap().model = "other".into();
        let mut image = Message::user("look");
        image.images = vec!["iVBORw0KGgo=".into(), "/9j/4AAQ".into()];
        let msgs = vec![
            Message::system("sys"),
            Message::user(""),
            image,
            assistant.clone(),
            Message::tool(&assistant.tool_calls[0], "fn a() {}"),
            Message::tool(&assistant.tool_calls[1], ""),
            other_model,
            Message::assistant("done"),
        ];
        for model in ["m", "other"] {
            let fields = json!({"model": model, "stream": true, "max_tokens": 10});
            let mut expected = fields.clone();
            expected["messages"] = Value::Array(reference_openai_messages(&msgs, model));
            let body = Provider::openai_body(&fields, &msgs, model);
            assert_eq!(serde_json::to_value(&body).unwrap(), expected);
            let text = serde_json::to_string(&body).unwrap();
            assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), expected);
        }
    }

    #[test]
    fn fallback_triggers() {
        assert!(warrants_fallback(
            "API error 529: {\"type\":\"overloaded_error\"}"
        ));
        assert!(warrants_fallback("API error 503 Service Unavailable: busy"));
        assert!(warrants_fallback(
            "Ollama error 404 Not Found: {\"error\":\"model 'x' not found\"}"
        ));
        assert!(warrants_fallback(
            "Ollama error: model 'x' not found, try pulling it first"
        ));
        assert!(warrants_fallback(
            "connecting to http://localhost:11434: tcp connect error: Connection refused (os error 61)"
        ));
        assert!(warrants_fallback(
            "chanto cloud: upstream failed (HTTP 502)"
        ));
        assert!(!warrants_fallback("API error 429: rate limited"));
        assert!(!warrants_fallback("API error 401: invalid api key"));
        assert!(!warrants_fallback(
            "API error 400 Bad Request: model context length exceeded"
        ));
        assert!(!warrants_fallback("API error 413: request too large"));
        assert!(!warrants_fallback("error decoding response body"));
        assert_eq!(error_status("API error 404 Not Found: x"), Some(404));
    }

    #[test]
    fn failure_hints_say_what_to_do() {
        let h = |msg: &str, kind| {
            failure_hint(
                msg,
                kind,
                "http://localhost:4555/v1",
                "qwen3",
                Some("deepseek"),
            )
        };
        assert!(
            h(
                "connecting to x: tcp connect error: Connection refused (os error 61)",
                ProviderKind::Openai
            )
            .unwrap()
            .contains("Can't reach the model server at http://localhost:4555/v1")
        );
        assert!(
            h(
                "Ollama error 404 Not Found: {\"error\":\"model 'qwen3' not found\"}",
                ProviderKind::Ollama
            )
            .unwrap()
            .contains("ollama pull qwen3")
        );
        assert!(
            h(
                "API error 401 Unauthorized: Authentication Fails",
                ProviderKind::Openai
            )
            .unwrap()
            .contains("chanto login deepseek")
        );
        assert!(
            h(
                "API error 404 Not Found: model does not exist",
                ProviderKind::Openai
            )
            .unwrap()
            .contains("doesn't recognize model `qwen3`")
        );
        assert!(
            h(
                "API error 400 Bad Request: Invalid model name passed in model=qwen3",
                ProviderKind::Openai
            )
            .unwrap()
            .contains("`chanto models deepseek`")
        );
        assert_eq!(
            h(
                "API error 400 Bad Request: max_tokens is too large for this model",
                ProviderKind::Openai
            ),
            None
        );
        assert_eq!(h("API error 500: boom", ProviderKind::Openai), None);
        // A custom provider section can't be logged in to.
        assert!(
            failure_hint(
                "API error 401 Unauthorized",
                ProviderKind::Openai,
                "u",
                "m",
                Some("work")
            )
            .unwrap()
            .contains("chanto doctor")
        );
    }

    #[test]
    fn a_dropped_stream_says_what_to_do() {
        let hint = failure_hint(
            "error decoding response body: error reading a body from connection: connection reset",
            ProviderKind::Openai,
            "http://localhost:4555/v1",
            "qwen3",
            Some("deepseek"),
        )
        .unwrap();
        assert!(
            hint.starts_with("Lost the connection to the model server at http://localhost:4555/v1"),
            "{hint}"
        );
        assert!(hint.contains("retry the turn"), "{hint}");
    }

    #[test]
    fn extracts_tagged_tool_calls() {
        let known = vec!["Read".to_string()];
        let (rest, calls) = extract_text_tool_calls(
            "Let me look.\n<tool_call>\n{\"name\": \"Read\", \"arguments\": {\"file_path\": \"a.rs\"}}\n</tool_call>",
            &known,
        );
        assert_eq!(rest, "Let me look.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments["file_path"], "a.rs");
    }

    #[test]
    fn ignores_unknown_tools() {
        let known = vec!["Read".to_string()];
        let (_, calls) = extract_text_tool_calls("{\"name\": \"Nope\", \"arguments\": {}}", &known);
        assert!(calls.is_empty());
    }

    #[test]
    fn a_mention_of_the_tag_is_not_a_call() {
        // Writing *about* the protocol must not hide the rest of the answer.
        let prose = "the parser does full.split(\"<tool_call>\") and stops";
        assert_eq!(
            tool_call_start(prose),
            prose.len(),
            "inline mention is prose"
        );
        assert_eq!(
            tool_call_start("see <tool_call>{} in the docs"),
            4,
            "a block still counts"
        );
        let plain = "all prose, nothing to see";
        assert_eq!(tool_call_start(plain), plain.len());
    }

    #[test]
    fn label_probs_count_only_whole_labels() {
        let top = |v: &[(&str, f64)]| -> Vec<(String, f64)> {
            v.iter().map(|(t, p)| (t.to_string(), p.ln())).collect()
        };
        let p = label_probs_from(
            &top(&[("allow", 0.6), (" BLOCK", 0.2), ("I", 0.2)]),
            &["ALLOW", "BLOCK"],
        )
        .unwrap();
        assert!(
            (p.mass[0] - 0.6).abs() < 1e-9 && (p.mass[1] - 0.2).abs() < 1e-9 && p.top == Some(0),
            "{p:?}"
        );
        // A prefix of a label could begin prose ("All of this…"): it counts
        // for no label, however likely.
        for piece in ["Al", "All", "Allo", "ALLOWED"] {
            let p = label_probs_from(&top(&[(piece, 0.95), ("ALLOW", 0.04)]), &["ALLOW", "BLOCK"])
                .unwrap();
            assert!(
                p.top.is_none() && (p.mass[0] - 0.04).abs() < 1e-9,
                "{piece}: {p:?}"
            );
        }
        // Absolute, not a share of the label mass: ALLOW at 0.7% stays 0.7%
        // when most of the probability went to prose.
        let p = label_probs_from(
            &top(&[("Sure", 0.95), ("ALLOW", 0.007)]),
            &["ALLOW", "BLOCK"],
        )
        .unwrap();
        assert!((p.mass[0] - 0.007).abs() < 1e-9 && p.top.is_none(), "{p:?}");
        // A label split into one-letter pieces counts for neither label.
        let p =
            label_probs_from(&top(&[("ALLOW", 0.5), ("B", 0.45)]), &["ALLOW", "BLOCK"]).unwrap();
        assert!((p.mass[0] - 0.5).abs() < 1e-9 && p.mass[1] == 0.0, "{p:?}");
        // Nothing that reads as a label: no answer, not a guess.
        assert!(
            label_probs_from(&top(&[("Sure", 0.9), ("I", 0.1)]), &["ALLOW", "BLOCK"]).is_none()
        );
        let entry = json!({"token": "BLOCK", "logprob": -0.1, "top_logprobs": [{"token": "BLOCK", "logprob": -0.1}, {"token": "ALLOW", "logprob": -2.4}]});
        assert_eq!(top_logprobs(&entry).len(), 2);
        assert_eq!(
            top_logprobs(&json!({"token": "ALLOW", "logprob": -0.01})).len(),
            1
        );
    }

    #[test]
    fn reasoning_watch_spots_paragraph_loops_only() {
        let para = "The Verse function builds each verse by first adding the introduction line, then the chain, and returns the joined string. ";
        let mut w = ReasoningWatch::default();
        let mut text = String::new();
        let mut fired = None;
        while text.len() < 40_000 && fired.is_none() {
            text.push_str(para);
            if w.looping(&text) {
                fired = Some(text.len());
            }
        }
        assert!(fired.is_some_and(|n| n < 12_000), "{fired:?}");
        // A long cycle (about 350 words) is a loop too.
        let cycle: String = (0..350).map(|i| format!("w{i} ")).collect();
        let mut w = ReasoningWatch::default();
        let mut text = String::new();
        let mut fired = None;
        while text.len() < 80_000 && fired.is_none() {
            text.push_str(&cycle[..cycle.len().min(400)]);
            text.push_str(&cycle[cycle.len().min(400)..]);
            if w.looping(&text) {
                fired = Some(text.len());
            }
        }
        assert!(fired.is_some(), "long cycle not caught");
        // Varied reasoning of the same length never trips it.
        let mut w = ReasoningWatch::default();
        let mut text = String::new();
        for i in 0..4000 {
            text.push_str(&format!(
                "step {i} checks value {} against {} ",
                i * 7 % 13,
                i * 3
            ));
            assert!(!w.looping(&text), "false positive at {}", text.len());
        }
    }

    /// A server that waits `prefill`, sends `head` (status line, headers and
    /// the first chunk), then goes silent with the connection open.
    async fn stalling_server(prefill: Duration, head: String) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let head = head.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 16384];
                    loop {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        let s = String::from_utf8_lossy(&buf);
                        if let Some(h) = s.find("\r\n\r\n") {
                            let cl = s[..h]
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if buf.len() >= h + 4 + cl {
                                break;
                            }
                        }
                    }
                    tokio::time::sleep(prefill).await;
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.flush().await;
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    drop(sock);
                });
            }
        });
        format!("http://{addr}")
    }

    fn stall_req() -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            think: None,
            effort: None,
            temperature: None,
            max_tokens: None,
            json: false,
        }
    }

    #[tokio::test]
    async fn a_stream_that_stalls_after_starting_times_out() {
        let sse_head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n".to_string();
        let ndjson_head = "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\n\r\n{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":false}\n".to_string();
        for (kind, head, path) in [
            (ProviderKind::Openai, sse_head, "/v1"),
            (ProviderKind::Ollama, ndjson_head, ""),
        ] {
            let base = stalling_server(Duration::ZERO, head).await;
            let mut p = Provider::new(kind, &format!("{base}{path}"), None, 8192);
            p.stream_idle = Some(Duration::from_millis(300));
            let t = std::time::Instant::now();
            let r =
                tokio::time::timeout(Duration::from_secs(10), p.chat(&stall_req(), &mut |_| {}))
                    .await
                    .expect("the stall must not hang");
            let e = r.expect_err("a stalled stream fails");
            assert!(
                e.downcast_ref::<StreamStalled>().is_some(),
                "{kind:?}: {e:#}"
            );
            assert!(format!("{e:#}").contains("stream_idle_timeout"), "{e:#}");
            assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        }
    }

    #[tokio::test]
    async fn the_idle_limit_waits_out_a_slow_first_byte() {
        // Prefill (no bytes at all yet) longer than the idle limit is fine.
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let base = stalling_server(Duration::from_millis(900), head).await;
        let mut p = Provider::new(ProviderKind::Openai, &format!("{base}/v1"), None, 8192);
        p.stream_idle = Some(Duration::from_millis(300));
        let r = tokio::time::timeout(Duration::from_secs(10), p.chat(&stall_req(), &mut |_| {}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.text, "ok");
    }

    #[test]
    fn stream_idle_timeout_config() {
        let mut c = crate::config::Config::default();
        assert_eq!(c.stream_idle(), Some(STREAM_IDLE_DEFAULT));
        c.stream_idle_timeout = Some(0.0);
        assert_eq!(c.stream_idle(), None);
        c.stream_idle_timeout = Some(30.0);
        assert_eq!(c.stream_idle(), Some(Duration::from_secs(30)));
    }

    #[test]
    fn line_buf_splits_across_reads_and_scans_long_lines_once() {
        let mut l = LineBuf::default();
        l.push(b"data: a\ndata: b");
        assert_eq!(l.next_line(), Some(&b"data: a\n"[..]));
        assert_eq!(l.next_line(), None);
        l.push(b"c\n\nx");
        assert_eq!(l.next_line(), Some(&b"data: bc\n"[..]));
        assert_eq!(l.next_line(), Some(&b"\n"[..]));
        assert_eq!(l.next_line(), None);
        // A 20 MB line in 16 KB reads: linear, so this is instant.
        let mut l = LineBuf::default();
        let t = std::time::Instant::now();
        for _ in 0..1280 {
            l.push(&[b'y'; 16384]);
            assert_eq!(l.next_line(), None);
        }
        l.push(b"\n");
        assert_eq!(l.next_line().map(|v| v.len()), Some(1280 * 16384 + 1));
        assert!(
            t.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            t.elapsed()
        );
    }

    #[test]
    fn repairs_truncated_json() {
        let v: Value = serde_json::from_str(&repair_json("{\"a\": [1, 2, {\"b\": \"x")).unwrap();
        assert_eq!(v["a"][2]["b"], "x");
    }
}
