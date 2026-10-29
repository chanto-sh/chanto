//! Anthropic Messages API (`POST /v1/messages`, streamed).
//!
//! - The leading system message becomes the top-level `system` block.
//! - Prompt caching: breakpoints on the system block, the last tool, and the
//!   last user message (rolling), so each request reuses the previous prefix.
//! - Tool results are `tool_result` blocks in user turns, and consecutive
//!   same-role messages are merged.
//! - Thinking: adaptive on current models, `budget_tokens` on older ones.
//!   Assistant turns that carried thinking are replayed verbatim (blocks and
//!   signatures) to the same model.

use super::*;

/// Models that take `thinking: {type: "enabled", budget_tokens}` instead of adaptive.
fn budget_thinking_model(m: &str) -> bool {
    [
        "claude-3-7",
        "sonnet-4-5",
        "haiku-4-5",
        "opus-4-5",
        "opus-4-1",
        "sonnet-4-0",
        "opus-4-0",
        "sonnet-4-2025",
        "opus-4-2025",
    ]
    .iter()
    .any(|p| m.contains(p))
}

/// Models without extended thinking.
fn no_thinking_model(m: &str) -> bool {
    [
        "claude-3-5",
        "claude-3-haiku",
        "claude-3-opus",
        "claude-3-sonnet",
    ]
    .iter()
    .any(|p| m.contains(p))
}

const THINKING_BUDGET: u32 = 8000;

/// The highest `output_config.effort` a model takes; `None` if it rejects the parameter.
pub(super) fn effort_cap(m: &str) -> Option<&'static str> {
    if [
        "sonnet-4-5",
        "haiku-4-5",
        "claude-3",
        "opus-4-1",
        "opus-4-0",
        "sonnet-4-0",
        "sonnet-4-2025",
        "opus-4-2025",
    ]
    .iter()
    .any(|p| m.contains(p))
    {
        None
    } else if m.contains("opus-4-5") {
        Some("high")
    } else {
        Some("max")
    }
}

/// `budget_tokens` for older models, scaled by effort, leaving room for the
/// answer within the model's output limit (32k on Opus 4.0/4.1, 64k on the rest).
fn thinking_budget(model: &str, effort: Option<&str>) -> u32 {
    let want = match effort {
        Some("low") => 2048,
        Some("high") => 16000,
        Some("xhigh" | "max") => 32000,
        _ => THINKING_BUDGET,
    };
    let out_cap: u32 = if ["opus-4-1", "opus-4-0", "opus-4-2025"]
        .iter()
        .any(|p| model.contains(p))
    {
        32000
    } else {
        64000
    };
    want.min(out_cap - 4096)
}

/// Tool ids must match `^[a-zA-Z0-9_-]+$`.
pub(super) fn tool_id(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() { "call_0".into() } else { s }
}

fn image_block(b64: &str) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": sniff_mime(b64), "data": b64}})
}

/// Append blocks to the conversation, merging with the previous turn of the same role.
fn push(msgs: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = msgs.last_mut()
        && last["role"] == role
    {
        let content = last["content"].as_array_mut().unwrap();
        if role == "user" && blocks.iter().any(|b| b["type"] == "tool_result") {
            // tool_result blocks must come before any other user content.
            let at = content
                .iter()
                .take_while(|b| b["type"] == "tool_result")
                .count();
            for (k, b) in blocks.into_iter().enumerate() {
                content.insert(at + k, b);
            }
        } else {
            content.extend(blocks);
        }
        return;
    }
    msgs.push(json!({"role": role, "content": blocks}));
}

/// Build the request body.
pub(super) fn body(req: &ChatRequest) -> Value {
    let mut system: Vec<String> = Vec::new();
    let mut msgs: Vec<Value> = Vec::new();
    for m in &req.messages {
        match m.role {
            Role::System if msgs.is_empty() => system.push(String::clone(&m.content)),
            Role::System => push(
                &mut msgs,
                "user",
                vec![json!({"type": "text", "text": m.content})],
            ),
            Role::User => {
                let mut blocks: Vec<Value> = m.images.iter().map(|i| image_block(i)).collect();
                if !m.content.is_empty() || blocks.is_empty() {
                    blocks.push(json!({"type": "text", "text": if m.content.is_empty() { "(empty)" } else { &m.content }}));
                }
                push(&mut msgs, "user", blocks);
            }
            Role::Tool => {
                let id = tool_id(m.tool_call_id.as_deref().unwrap_or(""));
                let content = if m.images.is_empty() {
                    json!(m.content)
                } else {
                    let mut parts = vec![json!({"type": "text", "text": m.content})];
                    parts.extend(m.images.iter().map(|i| image_block(i)));
                    Value::Array(parts)
                };
                push(
                    &mut msgs,
                    "user",
                    vec![json!({"type": "tool_result", "tool_use_id": id, "content": content})],
                );
            }
            Role::Assistant => {
                let ids: Vec<String> = m.tool_calls.iter().map(|c| tool_id(&c.id)).collect();
                let replay = m.provider_items.as_ref().filter(|p| {
                    p.kind == ProviderKind::Anthropic
                        && p.model == req.model
                        && p.items
                            .iter()
                            .filter(|b| b["type"] == "tool_use")
                            .map(|b| b["id"].as_str().unwrap_or("").to_string())
                            .collect::<Vec<_>>()
                            == ids
                });
                let blocks = match replay {
                    Some(p) => p.items.clone(),
                    None => {
                        let mut b = Vec::new();
                        if !m.content.trim().is_empty() {
                            b.push(json!({"type": "text", "text": m.content}));
                        }
                        for c in &m.tool_calls {
                            let input = if c.arguments.is_object() {
                                c.arguments.clone()
                            } else {
                                json!({})
                            };
                            b.push(json!({"type": "tool_use", "id": tool_id(&c.id), "name": c.name, "input": input}));
                        }
                        b
                    }
                };
                push(&mut msgs, "assistant", blocks);
            }
        }
    }
    // Rolling cache breakpoint on the last user turn.
    if let Some(last) = msgs.iter_mut().rev().find(|m| m["role"] == "user")
        && let Some(b) = last["content"].as_array_mut().and_then(|c| c.last_mut())
    {
        b["cache_control"] = json!({"type": "ephemeral"});
    }
    let model = req.model.to_ascii_lowercase();
    let mut max_tokens = req.max_tokens.unwrap_or(32_000).max(1024);
    // Claude 3.x models reject more than 8192 output tokens (also as
    // `anthropic.claude-3-…` on Bedrock).
    if model.contains("claude-3") {
        max_tokens = max_tokens.min(8192);
    }
    let mut body = json!({
        "model": req.model,
        "max_tokens": max_tokens,
        "stream": true,
        "messages": msgs,
    });
    if !system.is_empty() {
        body["system"] = json!([{"type": "text", "text": system.join("\n\n"), "cache_control": {"type": "ephemeral"}}]);
    }
    if !req.tools.is_empty() {
        let n = req.tools.len();
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let mut v = json!({"name": t.name, "description": t.description, "input_schema": t.parameters});
                    if i + 1 == n {
                        v["cache_control"] = json!({"type": "ephemeral"});
                    }
                    v
                })
                .collect(),
        );
    }
    if req.think == Some(true) && !no_thinking_model(&model) {
        if budget_thinking_model(&model) {
            let budget = thinking_budget(&model, effort_for(req, "max"));
            max_tokens = max_tokens.max(budget + 4096);
            body["max_tokens"] = json!(max_tokens);
            body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
        } else {
            body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
        }
        if let Some(e) = effort_cap(&model).and_then(|top| effort_for(req, top)) {
            // Opus/Sonnet 4.6 stop at `max` but have no `xhigh`.
            let e = if e == "xhigh" && ["opus-4-6", "sonnet-4-6"].iter().any(|p| model.contains(p))
            {
                "high"
            } else {
                e
            };
            body["output_config"] = json!({"effort": e});
        }
    }
    // Sampling parameters are rejected by current models; leave them at defaults.
    body
}

/// Calls `f` with each JSON `data:` payload of a server-sent event stream.
pub(super) async fn sse_json(
    mut stream: Body,
    mut f: impl FnMut(Value) -> Result<bool>,
) -> Result<()> {
    let mut lines = super::LineBuf::default();
    while let Some(chunk) = stream.next().await {
        lines.push(&chunk?);
        while let Some(line) = lines.next_line() {
            let line = String::from_utf8_lossy(line);
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if !f(v)? {
                return Ok(());
            }
        }
    }
    Ok(())
}

impl Provider {
    fn anthropic_url(&self, path: &str) -> String {
        format!("{}/v1/{path}", self.base_url.trim_end_matches("/v1"))
    }

    fn anthropic_headers(&self) -> Vec<(&'static str, String)> {
        let mut h = vec![("anthropic-version", "2023-06-01".to_string())];
        if let Some(k) = &self.api_key {
            h.push(("x-api-key", k.clone()));
        }
        h.extend(self.headers.iter().cloned());
        h
    }

    fn anthropic_get(&self, url: String) -> reqwest::RequestBuilder {
        let mut rb = self.http.get(url);
        for (k, v) in self.anthropic_headers() {
            rb = rb.header(k, v);
        }
        rb
    }

    pub(super) async fn anthropic_models(&self) -> Result<Vec<String>> {
        let resp = self
            .anthropic_get(self.anthropic_url("models?limit=1000"))
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!(
                "API error {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            );
        }
        let v: Value = resp.json().await?;
        Ok(v["data"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| m["id"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Context window from the Models API (`max_input_tokens`); all current Claude models take images.
    pub(super) async fn anthropic_model_info(&self, model: &str) -> Option<(u64, bool)> {
        let fallback = hosted_context(ProviderKind::Anthropic, model) as u64;
        let resp = self
            .anthropic_get(self.anthropic_url(&format!("models/{model}")))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return Some((fallback, true));
        }
        let v: Value = resp.json().await.ok()?;
        Some((v["max_input_tokens"].as_u64().unwrap_or(fallback), true))
    }

    pub(super) async fn chat_anthropic(
        &self,
        req: &ChatRequest,
        on_delta: &mut (dyn FnMut(Delta) + Send),
    ) -> Result<ChatResponse> {
        let mut body = body(req);
        let (url, headers) = match self.kind {
            // Vertex takes the model in the URL and the API version in the body.
            ProviderKind::Vertex => {
                let o = body.as_object_mut().unwrap();
                o.remove("model");
                o.insert("anthropic_version".into(), json!("vertex-2023-10-16"));
                (
                    self.vertex_claude_url(&req.model).await?,
                    self.cloud_auth_headers(true).await?,
                )
            }
            ProviderKind::Foundry => {
                let mut h = self.cloud_auth_headers(true).await?;
                h.push(("anthropic-version", "2023-06-01".into()));
                (self.foundry_anthropic_url().await?, h)
            }
            _ => (self.anthropic_url("messages"), self.anthropic_headers()),
        };
        let resp = self.post_retrying(&url, &body, &headers).await?;
        let mut stream = AnthropicStream::default();
        sse_json(self.body(resp), |v| stream.event(v, on_delta)).await?;
        Ok(stream.finish(&req.model))
    }
}

/// Folds Anthropic stream events (from SSE, or Bedrock's event stream) into a response.
#[derive(Default)]
pub(super) struct AnthropicStream {
    blocks: Vec<Value>,
    json_bufs: Vec<String>,
    out: ChatResponse,
    input: u64,
    cache_w: u64,
    cache_r: u64,
}

impl AnthropicStream {
    /// Handle one event; `false` once the message is complete.
    pub(super) fn event(
        &mut self,
        v: Value,
        on_delta: &mut (dyn FnMut(Delta) + Send),
    ) -> Result<bool> {
        let (blocks, json_bufs, out) = (&mut self.blocks, &mut self.json_bufs, &mut self.out);
        match v["type"].as_str().unwrap_or("") {
            "message_start" => {
                let u = &v["message"]["usage"];
                self.input = u["input_tokens"].as_u64().unwrap_or(0);
                self.cache_w = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                self.cache_r = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                out.usage.completion_tokens = u["output_tokens"].as_u64().unwrap_or(0);
            }
            "content_block_start" => {
                let i = v["index"].as_u64().unwrap_or(blocks.len() as u64) as usize;
                while blocks.len() <= i {
                    blocks.push(Value::Null);
                    json_bufs.push(String::new());
                }
                blocks[i] = v["content_block"].clone();
            }
            "content_block_delta" => {
                let i = v["index"].as_u64().unwrap_or(0) as usize;
                let Some(b) = blocks.get_mut(i) else {
                    return Ok(true);
                };
                let d = &v["delta"];
                let append = |b: &mut Value, key: &str, s: &str| {
                    let cur = b[key].as_str().unwrap_or("").to_string();
                    b[key] = json!(cur + s);
                };
                match d["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        let t = d["text"].as_str().unwrap_or("");
                        append(b, "text", t);
                        if !t.is_empty() {
                            on_delta(Delta::Text(t.to_string()));
                        }
                    }
                    "thinking_delta" => {
                        let t = d["thinking"].as_str().unwrap_or("");
                        append(b, "thinking", t);
                        if !t.is_empty() {
                            out.reasoning.push_str(t);
                            on_delta(Delta::Reasoning(t.to_string()));
                        }
                    }
                    "signature_delta" => {
                        append(b, "signature", d["signature"].as_str().unwrap_or(""))
                    }
                    "input_json_delta" => {
                        // `i` is a model-controlled stream index; don't let it
                        // index `json_bufs` raw even though the blocks/json_bufs
                        // lengths move in lockstep today.
                        if let Some(buf) = json_bufs.get_mut(i) {
                            buf.push_str(d["partial_json"].as_str().unwrap_or(""));
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let i = v["index"].as_u64().unwrap_or(0) as usize;
                if let Some(raw) = json_bufs.get(i).filter(|r| !r.trim().is_empty())
                    && let Some(b) = blocks.get_mut(i)
                    && b["type"] == "tool_use"
                {
                    b["input"] = parse_tool_args(raw);
                }
            }
            "message_delta" => {
                if let Some(r) = v["delta"]["stop_reason"].as_str() {
                    out.finish_reason = Some(
                        match r {
                            "max_tokens" | "model_context_window_exceeded" => "length",
                            "tool_use" => "tool_calls",
                            "refusal" => "refusal",
                            _ => "stop",
                        }
                        .to_string(),
                    );
                }
                let u = &v["usage"];
                if let Some(o) = u["output_tokens"].as_u64() {
                    out.usage.completion_tokens = o;
                }
                if let Some(x) = u["input_tokens"].as_u64() {
                    self.input = x;
                }
                if let Some(x) = u["cache_creation_input_tokens"].as_u64() {
                    self.cache_w = x;
                }
                if let Some(x) = u["cache_read_input_tokens"].as_u64() {
                    self.cache_r = x;
                }
            }
            "message_stop" => return Ok(false),
            "error" => {
                let e = &v["error"];
                bail!(
                    "API error {}: {}",
                    e["type"].as_str().unwrap_or("error"),
                    e["message"].as_str().unwrap_or("")
                );
            }
            _ => {}
        }
        Ok(true)
    }

    pub(super) fn finish(self, model: &str) -> ChatResponse {
        let AnthropicStream {
            mut blocks,
            mut out,
            input,
            cache_w,
            cache_r,
            ..
        } = self;
        out.usage.prompt_tokens = input + cache_w + cache_r;
        out.usage.cached_tokens = cache_r;
        out.usage.cache_write_tokens = cache_w;
        blocks.retain(|b| !b.is_null());
        for b in &blocks {
            match b["type"].as_str().unwrap_or("") {
                "text" => out.text.push_str(b["text"].as_str().unwrap_or("")),
                "tool_use" => {
                    let raw = &b["input"];
                    out.tool_calls.push(ToolCall {
                        id: b["id"]
                            .as_str()
                            .map(String::from)
                            .unwrap_or_else(new_call_id),
                        name: b["name"].as_str().unwrap_or("").to_string(),
                        arguments: if raw.is_null() {
                            json!({})
                        } else {
                            raw.clone()
                        },
                    });
                }
                _ => {}
            }
        }
        // Thinking must be sent back unchanged with the turn it belongs to.
        if blocks
            .iter()
            .any(|b| matches!(b["type"].as_str(), Some("thinking" | "redacted_thinking")))
        {
            for b in blocks.iter_mut().filter(|b| b["type"] == "tool_use") {
                if b["input"].get("__raw").is_some() || !b["input"].is_object() {
                    b["input"] = json!({});
                }
            }
            out.provider_items = Some(ProviderItems {
                kind: ProviderKind::Anthropic,
                model: model.to_string(),
                items: blocks,
            });
        }
        out
    }
}
