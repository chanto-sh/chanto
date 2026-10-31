//! The `chanto serve` API's wire format: the events a session streams and the
//! bodies its routes accept. The server builds its events from these types,
//! and `sdk/schema.json`, which the SDKs' types are generated from, comes from
//! them too, so neither can drift from what the server sends. With the
//! `schema` feature, `cargo test --features schema --lib wire::` checks the
//! committed schema is current (`UPDATE_SCHEMA=1` rewrites it).

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(feature = "schema")]
use schemars::JsonSchema;

/// One event on `GET /sessions/{id}/events` (also `--output-format
/// stream-json`): a JSON object whose `type` names the variant.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent<'a> {
    /// The model started a reply.
    AssistantStart,
    /// The model's reply ended.
    AssistantEnd,
    /// Reply text as it streams in.
    Text {
        text: &'a str,
    },
    /// Reasoning as it streams in.
    Reasoning {
        text: &'a str,
    },
    /// A tool call started; `parent` is the subagent call it belongs to.
    ToolStart {
        id: &'a str,
        label: &'a str,
        parent: Option<&'a str>,
    },
    /// A tool call finished. `output` is what the model saw (secrets
    /// redacted, at most 2,000 characters); `display` is the human-facing
    /// summary, edit diffs included.
    ToolEnd {
        id: &'a str,
        is_error: bool,
        output: String,
        display: &'a str,
        /// Up to two images the tool returned, as data URLs.
        images: Vec<String>,
        parent: Option<&'a str>,
    },
    /// Output from a tool that's still running.
    ToolProgress {
        id: &'a str,
        text: &'a str,
    },
    /// What the agent is busy with, or `null` when it's idle.
    Busy {
        label: Option<&'a str>,
    },
    /// Token use and cost so far.
    Usage {
        context_used: u64,
        context_max: u64,
        total_in: u64,
        total_out: u64,
        /// Tokens per second of the last reply.
        tps: f64,
        total_cost_usd: f64,
        /// True when some model's price isn't known, so the cost is a floor.
        price_unknown: bool,
    },
    #[serde(rename = "warning")]
    Warning {
        message: &'a str,
    },
    Error {
        message: &'a str,
    },
    Info {
        message: &'a str,
    },
    /// The session's todo list, in full.
    Todos {
        todos: Vec<Todo<'a>>,
    },
    /// The permission mode changed.
    Mode {
        mode: Mode,
    },
    /// The conversation was compacted from `before` to `after` tokens.
    Compacted {
        before: u64,
        after: u64,
    },
    /// A background subagent started or changed state.
    BackgroundAgent {
        id: &'a str,
        description: &'a str,
        status: &'a str,
    },
    /// A ChatGPT plan's usage limits.
    RateLimits {
        summary: String,
        plan: Option<&'a str>,
        primary: Option<Window>,
        secondary: Option<Window>,
    },
    /// Live status of a long-running mode, by key ("goal", "team").
    Status {
        key: &'a str,
        lines: &'a [String],
    },
    /// The agent wants to use a tool and needs a decision: answer with
    /// `POST /sessions/{id}/respond` and `{request_id, decision, feedback?}`.
    PermissionRequest {
        request_id: String,
        tool: &'a str,
        label: &'a str,
        detail: &'a str,
        reason: Option<&'a str>,
        /// What "allow always" would allow.
        always_label: &'a str,
    },
    /// The agent asks a question: answer with `{request_id, answer}`.
    Question {
        request_id: String,
        question: &'a str,
        options: &'a [String],
    },
    /// The agent called a tool the session was created with: answer with
    /// `{id, result: {content, is_error}}` within ten minutes.
    ToolCall {
        id: &'a str,
        name: &'a str,
        input: &'a Value,
    },
    /// A prompt or command finished.
    TurnDone(TurnDone),
}

/// A todo-list item.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct Todo<'a> {
    pub content: &'a str,
    /// `pending`, `in_progress` or `completed`.
    pub status: &'a str,
    #[serde(rename = "activeForm")]
    pub active_form: &'a str,
}

/// A permission mode.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub enum Mode {
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    #[serde(rename = "plan")]
    Plan,
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
}

impl From<crate::permissions::Mode> for Mode {
    fn from(m: crate::permissions::Mode) -> Self {
        use crate::permissions::Mode as M;
        match m {
            M::Default => Mode::Default,
            M::AcceptEdits => Mode::AcceptEdits,
            M::Plan => Mode::Plan,
            M::Auto => Mode::Auto,
            M::Bypass => Mode::BypassPermissions,
        }
    }
}

/// One usage-limit window (the 5-hour rolling one or the weekly one).
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct Window {
    pub used_percent: f64,
    pub window_minutes: Option<u64>,
    /// Unix seconds.
    pub resets_at: Option<i64>,
}

impl From<&chanto_core::limits::Window> for Window {
    fn from(w: &chanto_core::limits::Window) -> Self {
        Window {
            used_percent: w.used_percent,
            window_minutes: w.window_minutes,
            resets_at: w.resets_at,
        }
    }
}

/// The end of a prompt or command: its answer (`result`), a command's printed
/// text (`output`), an `error`, or `interrupted`, and the session's cost so far.
#[derive(Debug, Default, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct TurnDone {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interrupted: Option<bool>,
    pub total_cost_usd: f64,
    pub price_unknown: bool,
}

impl StreamEvent<'_> {
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("stream events serialize")
    }
}

/// `POST /sessions`: start a session (every field optional).
#[derive(Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct CreateSession {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    /// A saved session's id to continue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    /// The working directory (the server's own by default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt_append: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disallowed_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u64>,
    /// Run the project's checks before finishing, as `--verify` does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify: Option<bool>,
    /// Tools the client runs itself: the agent calls them with `tool_call`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSpec>>,
}

/// A client-side tool: the agent sees its name, description and the JSON
/// Schema of its input (an object schema).
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// `POST /sessions` answer.
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct SessionCreated {
    pub id: String,
    pub model: String,
    pub warnings: Vec<String>,
}

/// `POST /sessions/{id}/messages`: a prompt or a `/command`.
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct SendMessage {
    pub text: String,
}

/// `POST /sessions/{id}/respond`: answers a `permission_request` (`decision`,
/// optionally `feedback`), a `question` (`answer`), or a `tool_call` (`id`
/// and `result`).
#[derive(Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct Respond {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<Decision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    /// A `tool_call`'s id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<ToolResult>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    AllowAlways,
    Deny,
}

/// What a client-side tool returned.
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

/// `POST /sessions/{id}/fork`: a live copy of the session, from now or from
/// a checkpoint.
#[derive(Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct Fork {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A checkpoint index; the current conversation when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub point: Option<u64>,
}

/// `POST /sessions/{id}/rewind`: back to a checkpoint, or undo the last
/// rewind's code restore. With `preview`, only reports what it would do.
#[derive(Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct Rewind {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<RewindMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub undo: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<bool>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum RewindMode {
    Both,
    Conversation,
    Code,
}

/// `GET /health`.
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct Health {
    pub ok: bool,
    /// The server's Chanto version.
    pub version: String,
    /// Whether this server hosts background runs (`/runs`).
    pub runs: bool,
}

/// Everything the schema covers, as one document: the SDK generator reads
/// each top-level definition from it.
#[cfg(feature = "schema")]
#[derive(JsonSchema)]
#[allow(dead_code)]
pub struct Api<'a> {
    event: StreamEvent<'a>,
    create_session: CreateSession,
    session_created: SessionCreated,
    send_message: SendMessage,
    respond: Respond,
    fork: Fork,
    rewind: Rewind,
    health: Health,
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
