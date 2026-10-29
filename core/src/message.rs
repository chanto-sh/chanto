//! The conversation's data: messages, tool calls and the requests built from
//! them, shared by the CLI and the browser build. Plain data with no I/O, so it
//! compiles for every target, `wasm32-unknown-unknown` included.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

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
    /// Shared, so the copies of the conversation a request needs (the
    /// published snapshot, the request itself) don't copy every message's
    /// text; `Arc::make_mut` copies one only when it changes.
    #[serde(default)]
    pub content: Arc<String>,
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
            content: Arc::new(content.into()),
            images: vec![],
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,
            provider_items: None,
        }
    }
    /// Replace the text (the old text stays with whoever still shares it).
    pub fn set_content(&mut self, c: impl Into<String>) {
        self.content = Arc::new(c.into());
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

    pub fn key_env(self) -> Option<&'static str> {
        match self {
            ProviderKind::Anthropic => Some("ANTHROPIC_API_KEY"),
            ProviderKind::OpenaiResponses => Some("OPENAI_API_KEY"),
            _ => None,
        }
    }
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
    pub rate_limits: Option<crate::limits::Snapshot>,
}

/// Provider safety stops must remain visible even when no answer was sent.
impl ChatResponse {
    pub fn safety_stopped(&self) -> bool {
        matches!(
            self.finish_reason.as_deref(),
            Some("content_filter" | "refusal")
        )
    }

    pub fn show_safety_notice(&mut self, on_delta: &mut (dyn FnMut(Delta) + Send)) {
        if !self.safety_stopped() {
            return;
        }
        // A filtered response can contain an unfinished tool call. Neither
        // native calls nor tool-shaped text from this response should run.
        self.tool_calls.clear();
        let notice = match self.finish_reason.as_deref() {
            Some("content_filter") => {
                "ⓘ This response was stopped by the provider's safety filter."
            }
            _ if self.text.trim().is_empty() => {
                "ⓘ The provider declined to respond to this request."
            }
            _ => return,
        };
        let text = if self.text.trim().is_empty() {
            notice.to_string()
        } else {
            format!("\n\n{notice}")
        };
        self.text.push_str(&text);
        on_delta(Delta::Text(text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_reads_back_as_it_was_saved() {
        let call = ToolCall {
            id: "c1".into(),
            name: "Read".into(),
            arguments: serde_json::json!({"file_path": "a.rs"}),
        };
        let mut m = Message::assistant("reading");
        m.tool_calls = vec![call.clone()];
        let tool = Message::tool(&call, "contents");
        let json = serde_json::to_string(&[m, tool]).unwrap();
        // The shape sessions on disk use: lowercase roles, plain-string content,
        // empty fields left out.
        assert!(
            json.contains(r#""role":"assistant","content":"reading""#),
            "{json}"
        );
        assert!(!json.contains("images"), "{json}");
        let back: Vec<Message> = serde_json::from_str(&json).unwrap();
        assert_eq!(back[1].role, Role::Tool);
        assert_eq!(back[1].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(*back[1].content, "contents");
        assert_eq!(ProviderKind::parse("openai"), Some(ProviderKind::Openai));
    }
}
