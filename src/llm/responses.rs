//! OpenAI Responses API (`POST /v1/responses`, streamed, stateless).
//!
//! Requests use `store: false`. Reasoning models return encrypted reasoning
//! items (`include: ["reasoning.encrypted_content"]`), which are replayed
//! before the assistant turn they belong to on later requests to the same model.

use super::anthropic::sse_json;
use super::*;

/// Models that accept `reasoning` (and reject sampling parameters).
pub(super) fn reasoning_model(m: &str) -> bool {
    let m = m.to_ascii_lowercase();
    m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
        || m.starts_with("gpt-5")
        || m.starts_with("gpt-6")
        || m.contains("codex")
        || m.contains("gpt-oss")
}

fn data_url(b64: &str) -> String {
    format!("data:{};base64,{b64}", sniff_mime(b64))
}

fn user_images(images: &[String]) -> Value {
    let parts: Vec<Value> = images
        .iter()
        .map(|i| json!({"type": "input_image", "image_url": data_url(i)}))
        .collect();
    json!({"role": "user", "content": parts})
}

pub(super) fn body(req: &ChatRequest) -> Value {
    let mut instructions: Vec<String> = Vec::new();
    let mut input: Vec<Value> = Vec::new();
    // Images from tool results go in a user message after the outputs of a batch.
    let mut pending_images: Vec<String> = Vec::new();
    for m in &req.messages {
        if m.role != Role::Tool && !pending_images.is_empty() {
            input.push(user_images(&std::mem::take(&mut pending_images)));
        }
        match m.role {
            Role::System if input.is_empty() => instructions.push(String::clone(&m.content)),
            Role::System => input.push(json!({"role": "developer", "content": m.content})),
            Role::User => {
                let mut parts = vec![json!({"type": "input_text", "text": m.content})];
                parts.extend(
                    m.images
                        .iter()
                        .map(|i| json!({"type": "input_image", "image_url": data_url(i)})),
                );
                input.push(json!({"role": "user", "content": parts}));
            }
            Role::Tool => {
                input.push(json!({"type": "function_call_output", "call_id": m.tool_call_id.clone().unwrap_or_default(), "output": m.content}));
                pending_images.extend(m.images.iter().cloned());
            }
            Role::Assistant => {
                if let Some(p) = m
                    .provider_items
                    .as_ref()
                    .filter(|p| p.kind == ProviderKind::OpenaiResponses && p.model == req.model)
                {
                    input.extend(p.items.iter().cloned());
                }
                if !m.content.trim().is_empty() {
                    input.push(json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": m.content}]}));
                }
                for c in &m.tool_calls {
                    input.push(json!({"type": "function_call", "call_id": c.id, "name": c.name, "arguments": c.arguments.to_string()}));
                }
            }
        }
    }
    if !pending_images.is_empty() {
        input.push(user_images(&pending_images));
    }
    let mut body = json!({
        "model": req.model,
        "input": input,
        "stream": true,
        "store": false,
    });
    if !instructions.is_empty() {
        body["instructions"] = json!(instructions.join("\n\n"));
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| json!({"type": "function", "name": t.name, "description": t.description, "parameters": t.parameters, "strict": false}))
                .collect(),
        );
        body["parallel_tool_calls"] = json!(true);
    }
    if let Some(m) = req.max_tokens {
        body["max_output_tokens"] = json!(m);
    }
    if reasoning_model(&req.model) {
        let effort = if req.think == Some(false) {
            "low"
        } else {
            effort_for(req, "xhigh").unwrap_or("medium")
        };
        body["reasoning"] = json!({"effort": effort, "summary": "auto"});
        body["include"] = json!(["reasoning.encrypted_content"]);
    } else if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if req.json {
        body["text"] = json!({"format": {"type": "json_object"}});
    }
    body
}

impl Provider {
    pub(super) async fn chat_responses(
        &self,
        req: &ChatRequest,
        on_delta: &mut (dyn FnMut(Delta) + Send),
    ) -> Result<ChatResponse> {
        let mut body = body(req);
        let headers = self.auth_headers().await?;
        let url = self.openai_url("responses").await?;
        let resp = match self.post_retrying(&url, &body, &headers).await {
            // Older reasoning models (gpt-5, o3) stop at "high": ask again at that level.
            Err(e)
                if body["reasoning"]["effort"] == "xhigh"
                    && format!("{e:#}").contains("effort") =>
            {
                body["reasoning"]["effort"] = json!("high");
                self.post_retrying(&url, &body, &headers).await?
            }
            r => r?,
        };
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let rate_limits = limits::snapshot_from_headers(resp.headers());
        let mut out = ChatResponse {
            rate_limits,
            ..Default::default()
        };
        // output_index -> (call_id, name, arguments) while streaming
        let mut partial: std::collections::BTreeMap<u64, (String, String, String)> =
            Default::default();
        let mut done: std::collections::BTreeMap<u64, Value> = Default::default();
        let mut refused = false;
        let mut refusal_text_sent = false;
        sse_json(self.body(resp), |v| {
            let idx = v["output_index"].as_u64().unwrap_or(0);
            match v["type"].as_str().unwrap_or("") {
                "response.output_text.delta" => {
                    let t = v["delta"].as_str().unwrap_or("");
                    if !t.is_empty() {
                        out.text.push_str(t);
                        on_delta(Delta::Text(t.to_string()));
                    }
                }
                "response.refusal.delta" | "response.refusal.done" => {
                    let delta = v["type"] == "response.refusal.delta";
                    if delta || !refusal_text_sent {
                        let t = v[if delta { "delta" } else { "refusal" }]
                            .as_str()
                            .unwrap_or("");
                        if !t.is_empty() {
                            refusal_text_sent = true;
                            out.text.push_str(t);
                            on_delta(Delta::Text(t.to_string()));
                        }
                    }
                    refused = true;
                }
                "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                    let t = v["delta"].as_str().unwrap_or("");
                    if !t.is_empty() {
                        out.reasoning.push_str(t);
                        on_delta(Delta::Reasoning(t.to_string()));
                    }
                }
                "response.reasoning_summary_part.done" => {
                    out.reasoning.push_str("\n\n");
                    on_delta(Delta::Reasoning("\n\n".into()));
                }
                "response.output_item.added" => {
                    let it = &v["item"];
                    if it["type"] == "function_call" {
                        partial.insert(
                            idx,
                            (
                                it["call_id"].as_str().unwrap_or("").into(),
                                it["name"].as_str().unwrap_or("").into(),
                                it["arguments"].as_str().unwrap_or("").into(),
                            ),
                        );
                    }
                }
                "response.function_call_arguments.delta" => {
                    if let Some(p) = partial.get_mut(&idx) {
                        p.2.push_str(v["delta"].as_str().unwrap_or(""));
                    }
                }
                "response.function_call_arguments.done" => {
                    if let (Some(p), Some(a)) = (partial.get_mut(&idx), v["arguments"].as_str()) {
                        p.2 = a.to_string();
                    }
                }
                "response.output_item.done" => {
                    done.insert(idx, v["item"].clone());
                }
                "response.completed" | "response.incomplete" => {
                    let r = &v["response"];
                    for (i, item) in r["output"].as_array().into_iter().flatten().enumerate() {
                        done.entry(i as u64).or_insert_with(|| item.clone());
                    }
                    out.usage.prompt_tokens = r["usage"]["input_tokens"].as_u64().unwrap_or(0);
                    out.usage.completion_tokens = r["usage"]["output_tokens"].as_u64().unwrap_or(0);
                    out.usage.cached_tokens = r["usage"]["input_tokens_details"]["cached_tokens"]
                        .as_u64()
                        .unwrap_or(0);
                    let reason = r["incomplete_details"]["reason"].as_str();
                    out.finish_reason = Some(match reason {
                        Some("max_output_tokens") => "length".into(),
                        Some(other) => other.to_string(),
                        None => "stop".into(),
                    });
                    return Ok(false);
                }
                "response.failed" => {
                    let e = &v["response"]["error"];
                    if let Some(error) = limits::quota_error(
                        e,
                        self.login.as_deref() == Some("chatgpt"),
                        retry_after.as_deref(),
                    ) {
                        return Err(error.into());
                    }
                    bail!("{}", stream_error(&v, e));
                }
                "error" => {
                    let e = v.get("error").unwrap_or(&v);
                    if let Some(error) = limits::quota_error(
                        e,
                        self.login.as_deref() == Some("chatgpt"),
                        retry_after.as_deref(),
                    ) {
                        return Err(error.into());
                    }
                    bail!("{}", stream_error(&v, e));
                }
                // Account limits can also arrive as an event rather than headers.
                t if t.contains("rate_limits") || t.contains("usage") => {
                    if let Some(s) = limits::snapshot_from_json(v.get("rate_limits").unwrap_or(&v))
                    {
                        out.rate_limits = Some(s);
                    }
                }
                _ => {}
            }
            Ok(true)
        })
        .await?;
        // Prefer completed items; fall back to what streamed.
        for (i, (call_id, name, args)) in &partial {
            let item = done.get(i);
            let call_id = item
                .and_then(|it| it["call_id"].as_str())
                .unwrap_or(call_id)
                .to_string();
            let name = item
                .and_then(|it| it["name"].as_str())
                .unwrap_or(name)
                .to_string();
            let args = item
                .and_then(|it| it["arguments"].as_str())
                .unwrap_or(args)
                .to_string();
            out.tool_calls.push(parse_call(call_id, name, &args));
        }
        for (i, it) in &done {
            if it["type"] == "function_call" && !partial.contains_key(i) {
                out.tool_calls.push(parse_call(
                    it["call_id"].as_str().unwrap_or("").into(),
                    it["name"].as_str().unwrap_or("").into(),
                    it["arguments"].as_str().unwrap_or(""),
                ));
            }
        }
        if !refusal_text_sent {
            for it in done.values().filter(|it| it["type"] == "message") {
                for c in it["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|c| c["type"] == "refusal")
                {
                    refused = true;
                    let t = c["refusal"].as_str().unwrap_or("");
                    if !t.is_empty() {
                        let t = if out.text.is_empty() {
                            t.to_string()
                        } else {
                            format!("\n\n{t}")
                        };
                        out.text.push_str(&t);
                        on_delta(Delta::Text(t));
                    }
                }
            }
        }
        if out.text.is_empty() {
            for it in done.values().filter(|it| it["type"] == "message") {
                for c in it["content"].as_array().into_iter().flatten() {
                    let t = if c["type"] == "refusal" {
                        refused = true;
                        c["refusal"].as_str().unwrap_or("")
                    } else {
                        c["text"].as_str().unwrap_or("")
                    };
                    out.text.push_str(t);
                    on_delta(Delta::Text(t.to_string()));
                }
            }
        }
        let reasoning: Vec<Value> = done
            .into_values()
            .filter(|it| {
                it["type"] == "reasoning"
                    && it.get("encrypted_content").is_some_and(|e| !e.is_null())
            })
            .collect();
        if !reasoning.is_empty() {
            out.provider_items = Some(ProviderItems {
                kind: ProviderKind::OpenaiResponses,
                model: req.model.clone(),
                items: reasoning,
            });
        }
        if !out.tool_calls.is_empty() && out.finish_reason.as_deref() == Some("stop") {
            out.finish_reason = Some("tool_calls".into());
        }
        if refused && out.finish_reason.as_deref() != Some("content_filter") {
            out.finish_reason = Some("refusal".into());
        }
        Ok(out)
    }
}

/// Describe a streamed error event. The details sit under `error` for some
/// events and at the top level for others, and the payload can be a bare
/// string; read every shape so the provider's own words reach the user instead
/// of an empty "API error error:".
fn stream_error(event: &Value, nested: &Value) -> String {
    fn field<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
        v[k].as_str().filter(|s| !s.is_empty())
    }
    let code = field(nested, "code")
        .or_else(|| field(nested, "type"))
        .or_else(|| field(event, "code"));
    let message = field(nested, "message")
        .or_else(|| field(event, "message"))
        .or_else(|| nested.as_str().filter(|s| !s.is_empty()))
        .map(str::to_owned)
        .unwrap_or_else(|| crate::util::truncate(&nested.to_string(), 2000));
    match code {
        Some(code) => format!("API error {code}: {message}"),
        None => format!("API error: {message}"),
    }
}

fn parse_call(call_id: String, name: String, args: &str) -> ToolCall {
    let arguments = if args.trim().is_empty() {
        json!({})
    } else {
        parse_tool_args(args)
    };
    ToolCall {
        id: if call_id.is_empty() {
            new_call_id()
        } else {
            call_id
        },
        name,
        arguments,
    }
}
