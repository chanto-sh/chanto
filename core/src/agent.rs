//! A minimal agent loop for builds without the CLI's runtime (the browser):
//! send the conversation to an OpenAI-compatible model, run the file tools it
//! calls on a [`Workspace`], and repeat until it answers. The host supplies
//! the network ([`Transport`]); the files come with the workspace.
//!
//! The CLI keeps its own, fuller loop (permissions, hooks, checkpoints,
//! compaction, shells); this one shares its wire format, decoder and tools.

use crate::message::{ChatRequest, Delta, Message};
use crate::model::{OpenaiBody, OpenaiDecoder, OpenaiMessages, extract_text_tool_calls};
use crate::protocol::{normalize_args, to_text_protocol};
use crate::tools::{Workspace, specs};
use futures::stream::{Stream, StreamExt};
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;

/// A future the host runs on its own executor; the browser's are not `Send`.
pub type LocalFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;
/// A response body as it arrives.
pub type ByteStream<'a> = Pin<Box<dyn Stream<Item = anyhow::Result<Vec<u8>>> + 'a>>;

/// The host's network: `fetch` in the browser, an HTTP client elsewhere.
pub trait Transport {
    /// POST `body` to `url` with `headers`, and stream the response body. A
    /// status other than 2xx is an error that says what the server said.
    fn post<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        body: Vec<u8>,
    ) -> LocalFuture<'a, anyhow::Result<ByteStream<'a>>>;
}

/// Where the model is.
#[derive(Debug, Clone)]
pub struct ModelConfig {
    /// The OpenAI-compatible base URL, e.g. `https://api.example.com/v1`.
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
}

/// What a turn reports while it runs. A host that timestamps them can split
/// each step into waiting for the model and the agent's own work.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Step `step` (from 1) is about to ask the model.
    Request { step: usize },
    /// The model's reply to step `step` has fully arrived.
    Reply {
        step: usize,
        prompt_tokens: u64,
        completion_tokens: u64,
    },
    /// Reply text as it streams in.
    Text { text: String },
    /// Reasoning as it streams in.
    Reasoning { text: String },
    /// The model called a tool.
    ToolCall { name: String, arguments: Value },
    /// A tool finished.
    ToolResult {
        name: String,
        content: String,
        is_error: bool,
    },
}

/// One conversation with the model.
pub struct Agent {
    pub config: ModelConfig,
    pub messages: Vec<Message>,
    /// Model requests one turn may make before it stops.
    pub max_steps: usize,
    /// Describe the tools in the system prompt and read `<tool_call>` blocks
    /// from the reply, for models without native tool calls (small local
    /// ones), as the CLI's `tool_protocol = "text"` does.
    pub text_tools: bool,
}

impl Agent {
    pub fn new(config: ModelConfig, system_prompt: impl Into<String>) -> Self {
        Self {
            config,
            messages: vec![Message::system(system_prompt)],
            max_steps: 50,
            text_tools: false,
        }
    }

    /// Answer `input`: ask the model, run the tools it calls, and repeat until
    /// it replies without calling one. Returns the final reply's text.
    pub async fn run_turn(
        &mut self,
        input: &str,
        transport: &dyn Transport,
        workspace: &mut Workspace<'_>,
        on_event: &mut dyn FnMut(Event),
    ) -> anyhow::Result<String> {
        self.messages.push(Message::user(input));
        for step in 1..=self.max_steps {
            on_event(Event::Request { step });
            let reply = self.ask(step, transport, on_event).await?;
            let calls = reply.tool_calls.clone();
            let text = reply.content.to_string();
            self.messages.push(reply);
            if calls.is_empty() {
                return Ok(text);
            }
            for call in &calls {
                on_event(Event::ToolCall {
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });
                let out = workspace.run(&call.name, &call.arguments);
                on_event(Event::ToolResult {
                    name: call.name.clone(),
                    content: out.content.clone(),
                    is_error: out.is_error,
                });
                self.messages.push(Message::tool(call, out.content));
            }
        }
        anyhow::bail!(
            "stopped after {} model requests without an answer",
            self.max_steps
        )
    }

    /// One streamed model request; its reply as an assistant message.
    async fn ask(
        &self,
        step: usize,
        transport: &dyn Transport,
        on_event: &mut dyn FnMut(Event),
    ) -> anyhow::Result<Message> {
        let req = ChatRequest {
            model: self.config.model.clone(),
            messages: vec![],
            tools: specs(),
            think: None,
            effort: None,
            temperature: None,
            max_tokens: None,
            json: false,
        };
        let mut fields = json!({
            "model": req.model,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        let text_msgs;
        let msgs = if self.text_tools {
            text_msgs = to_text_protocol(&self.messages, &req.tools);
            // A small model that writes a call and then makes up its result
            // stops where the made-up result would start: the call runs.
            fields["stop"] = json!(["<tool_response"]);
            &text_msgs
        } else {
            fields["tools"] = req
                .tools
                .iter()
                .map(|t| json!({"type": "function", "function": t}))
                .collect();
            &self.messages
        };
        let body = serde_json::to_vec(&OpenaiBody {
            fields: &fields,
            messages: OpenaiMessages {
                msgs,
                model: &req.model,
            },
        })?;
        let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
        if let Some(key) = &self.config.api_key {
            headers.push(("authorization".into(), format!("Bearer {key}")));
        }
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let mut stream = transport.post(&url, &headers, body).await?;
        let mut decoder = OpenaiDecoder::new(&req);
        let mut on_delta = |d: Delta| match d {
            Delta::Text(text) => on_event(Event::Text { text }),
            Delta::Reasoning(text) => on_event(Event::Reasoning { text }),
            // Reported once the reply is complete, with its arguments.
            Delta::ToolCall(_) => {}
        };
        while let Some(chunk) = stream.next().await {
            if decoder.feed(&chunk?, &mut on_delta)? {
                break;
            }
        }
        let resp = decoder.finish()?;
        on_event(Event::Reply {
            step,
            prompt_tokens: resp.usage.prompt_tokens,
            completion_tokens: resp.usage.completion_tokens,
        });
        let (text, mut calls) = if resp.tool_calls.is_empty() && self.text_tools {
            let names: Vec<String> = req.tools.iter().map(|t| t.name.clone()).collect();
            extract_text_tool_calls(without_stop_fragment(&resp.text), &names)
        } else {
            (resp.text, resp.tool_calls)
        };
        // Small models misname arguments (`path` for `file_path`) and quote
        // numbers: repaired against each tool's schema, as the CLI does.
        for c in &mut calls {
            if let Some(t) = req.tools.iter().find(|t| t.name == c.name) {
                c.arguments = normalize_args(&t.parameters, &c.arguments);
            }
        }
        let mut msg = Message::assistant(text.trim());
        msg.tool_calls = calls;
        msg.provider_items = resp.provider_items;
        Ok(msg)
    }
}

/// `text` without the start of the stop sequence some servers stream before
/// they notice the match (`…}\n<tool`).
fn without_stop_fragment(text: &str) -> &str {
    const STOP: &str = "<tool_response";
    match text.rfind('<') {
        Some(i) if STOP.starts_with(text[i..].trim_end()) => text[..i].trim_end(),
        _ => text,
    }
}

#[cfg(test)]
mod agent_tests;
