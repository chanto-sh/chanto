//! Amazon Bedrock runtime.
//!
//! - Claude: `InvokeModelWithResponseStream` with an Anthropic Messages body
//!   (`anthropic_version: bedrock-2023-05-31`, model in the URL). The response
//!   is an AWS event stream whose `chunk` events carry base64 Anthropic stream
//!   events, folded by the same parser as the first-party API.
//! - Everything else (Llama, Mistral, Nova, gpt-oss, DeepSeek, Qwen, …):
//!   `ConverseStream`, with Chanto tools as Converse `toolSpec`s.
//!
//! Requests are SigV4-signed (service `bedrock`), or carry a Bedrock API key
//! as a bearer token.

use super::anthropic::AnthropicStream;
use super::*;
use crate::cloud::{Vars, aws, eventstream, sigv4};
use base64::Engine;

pub(super) enum Auth {
    Bearer(String),
    Sig(aws::Resolved),
}

/// Model IDs (`us.anthropic.claude-…`, `anthropic.claude-…`) that take the Anthropic Messages body.
pub(super) fn anthropic_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("anthropic.") || m.contains("claude")
}

/// Converse image formats.
fn image_format(b64: &str) -> &'static str {
    sniff_mime(b64).trim_start_matches("image/")
}

fn converse_image(b64: &str) -> Value {
    json!({"image": {"format": image_format(b64), "source": {"bytes": b64}}})
}

/// Append blocks, merging consecutive turns of the same role (Converse requires alternation).
fn push(msgs: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = msgs.last_mut().filter(|m| m["role"] == role) {
        let content = last["content"].as_array_mut().unwrap();
        if role == "user" && blocks.iter().any(|b| b.get("toolResult").is_some()) {
            let at = content
                .iter()
                .take_while(|b| b.get("toolResult").is_some())
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

fn text_block(t: &str) -> Vec<Value> {
    if t.trim().is_empty() {
        vec![]
    } else {
        vec![json!({"text": t})]
    }
}

/// The Converse request body. Without tools in the request, earlier tool
/// calls and results are replayed as text (Converse rejects tool blocks
/// without a `toolConfig`).
pub(super) fn converse_body(req: &ChatRequest) -> Value {
    let tools = !req.tools.is_empty();
    let mut system: Vec<Value> = Vec::new();
    let mut msgs: Vec<Value> = Vec::new();
    for m in &req.messages {
        match m.role {
            Role::System if msgs.is_empty() => system.extend(text_block(&m.content)),
            Role::System => push(&mut msgs, "user", text_block(&m.content)),
            Role::User => {
                let mut blocks = text_block(&m.content);
                blocks.extend(m.images.iter().map(|i| converse_image(i)));
                if blocks.is_empty() {
                    blocks.push(json!({"text": "(empty)"}));
                }
                push(&mut msgs, "user", blocks);
            }
            Role::Tool if tools => {
                let mut content = vec![
                    json!({"text": if m.content.is_empty() { "(no output)" } else { &m.content }}),
                ];
                content.extend(m.images.iter().map(|i| converse_image(i)));
                let id = super::anthropic::tool_id(m.tool_call_id.as_deref().unwrap_or(""));
                push(
                    &mut msgs,
                    "user",
                    vec![json!({"toolResult": {"toolUseId": id, "content": content}})],
                );
            }
            Role::Tool => push(
                &mut msgs,
                "user",
                text_block(&format!(
                    "[{} result]\n{}",
                    m.tool_name.as_deref().unwrap_or("tool"),
                    m.content
                )),
            ),
            Role::Assistant => {
                let mut blocks = text_block(&m.content);
                for c in &m.tool_calls {
                    if tools {
                        let input = if c.arguments.is_object() {
                            c.arguments.clone()
                        } else {
                            json!({})
                        };
                        blocks.push(json!({"toolUse": {"toolUseId": super::anthropic::tool_id(&c.id), "name": c.name, "input": input}}));
                    } else {
                        blocks.extend(text_block(&format!(
                            "[called {} with {}]",
                            c.name, c.arguments
                        )));
                    }
                }
                if blocks.is_empty() {
                    blocks.push(json!({"text": "(no output)"}));
                }
                push(&mut msgs, "assistant", blocks);
            }
        }
    }
    // A conversation must open with a user turn.
    if msgs.first().is_some_and(|m| m["role"] == "assistant") {
        msgs.insert(
            0,
            json!({"role": "user", "content": [{"text": "(continue)"}]}),
        );
    }
    let mut inference = json!({"maxTokens": req.max_tokens.unwrap_or(8192)});
    if let Some(t) = req.temperature {
        inference["temperature"] = json!(t);
    }
    let mut body = json!({"messages": msgs, "inferenceConfig": inference});
    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }
    if tools {
        body["toolConfig"] = json!({"tools": req.tools.iter().map(|t| json!({"toolSpec": {"name": t.name, "description": t.description, "inputSchema": {"json": t.parameters}}})).collect::<Vec<_>>()});
    }
    body
}

/// The Anthropic Messages body as Bedrock's InvokeModel takes it.
pub(super) fn anthropic_body(req: &ChatRequest) -> Value {
    let mut body = super::anthropic::body(req);
    let o = body.as_object_mut().unwrap();
    o.remove("model");
    o.remove("stream");
    o.insert("anthropic_version".into(), json!("bedrock-2023-05-31"));
    body
}

/// A Bedrock error frame or response: `ThrottlingException: Too many requests`.
fn exception(kind: &str, payload: &[u8]) -> anyhow::Error {
    let v: Value = serde_json::from_slice(payload).unwrap_or_default();
    let msg = v["message"]
        .as_str()
        .or_else(|| v["Message"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| String::from_utf8_lossy(payload).into_owned());
    anyhow::anyhow!("Bedrock {kind}: {msg}")
}

/// Decode an event-stream response, calling `f(event_type, payload)` per event until it returns `false`.
async fn events(mut stream: Body, mut f: impl FnMut(&str, Value) -> Result<bool>) -> Result<()> {
    let mut dec = eventstream::Decoder::default();
    while let Some(chunk) = stream.next().await {
        dec.push(&chunk?);
        while let Some(frame) = dec.next_frame()? {
            match frame.header(":message-type").unwrap_or("event") {
                "event" => {
                    let v: Value = serde_json::from_slice(&frame.payload).unwrap_or_default();
                    if !f(frame.header(":event-type").unwrap_or(""), v)? {
                        return Ok(());
                    }
                }
                _ => {
                    return Err(exception(
                        frame
                            .header(":exception-type")
                            .or_else(|| frame.header(":error-code"))
                            .unwrap_or("error"),
                        &frame.payload,
                    ));
                }
            }
        }
    }
    if dec.pending() > 0 {
        bail!("Bedrock stream ended mid-frame");
    }
    Ok(())
}

impl Provider {
    pub(super) fn bedrock_region(&self, vars: &Vars) -> Result<String> {
        aws::region(&self.cloud, vars)
            .map(|(r, _)| r)
            .context("no AWS region for Bedrock: set AWS_REGION, [providers.bedrock] region, or `region` in your AWS profile")
    }

    /// Runtime endpoint: `base_url` from config, `AWS_ENDPOINT_URL_BEDROCK_RUNTIME`, or the regional endpoint.
    pub(super) fn bedrock_runtime(&self, region: &str, vars: &Vars) -> String {
        if self.base_url.is_empty() {
            aws::runtime_endpoint(region, vars)
        } else {
            self.base_url.clone()
        }
    }

    pub(super) async fn bedrock_auth(&self, vars: &Vars) -> Result<Auth> {
        if let Some(k) = self.api_key.clone().or_else(|| aws::bearer_token(vars)) {
            return Ok(Auth::Bearer(k));
        }
        Ok(Auth::Sig(aws::credentials(&self.cloud, vars).await?))
    }

    /// Headers for a Bedrock request (`service` is `bedrock` for both planes).
    pub(super) fn bedrock_headers(
        auth: &Auth,
        method: &str,
        url: &str,
        body: &[u8],
        region: &str,
    ) -> Result<Vec<(String, String)>> {
        Ok(match auth {
            Auth::Bearer(k) => vec![("authorization".into(), format!("Bearer {k}"))],
            Auth::Sig(r) => {
                let url = reqwest::Url::parse(url)?;
                let ct: &[(&str, &str)] = if method == "POST" {
                    &[("content-type", "application/json")]
                } else {
                    &[]
                };
                sigv4::sign(
                    method,
                    &url,
                    ct,
                    body,
                    &r.creds,
                    region,
                    "bedrock",
                    chrono::Utc::now(),
                )
            }
        })
    }

    async fn bedrock_post(&self, path: &str, body: &Value) -> Result<reqwest::Response> {
        let vars = self.vars();
        let region = self.bedrock_region(&vars)?;
        let auth = self.bedrock_auth(&vars).await?;
        let url = format!("{}{path}", self.bedrock_runtime(&region, &vars));
        let bytes = serde_json::to_vec(body)?;
        let mut headers = Self::bedrock_headers(&auth, "POST", &url, &bytes, &region)?;
        headers.push(("accept".into(), "application/vnd.amazon.eventstream".into()));
        self.post_bytes_retrying(&url, bytes, &headers).await.map_err(|e| {
            let hint = match &auth {
                Auth::Sig(r) => format!(" (AWS credentials from {}, region {region})", r.source),
                Auth::Bearer(_) => format!(" (Bedrock API key, region {region})"),
            };
            let s = e.to_string();
            let access = if s.contains("API error 403") || s.contains("AccessDenied") {
                "\nThe identity needs bedrock:InvokeModelWithResponseStream on this model (or inference profile), and the model must be enabled in the Bedrock console for this region."
            } else if s.contains("on-demand throughput") {
                "\nUse an inference profile ID such as `us.<model-id>` or `global.<model-id>` instead of the base model ID."
            } else {
                ""
            };
            anyhow::anyhow!("{s}{hint}{access}")
        })
    }

    pub(super) async fn chat_bedrock(
        &self,
        req: &ChatRequest,
        on_delta: &mut (dyn FnMut(Delta) + Send),
    ) -> Result<ChatResponse> {
        let anthropic = match self.cloud.api.as_deref() {
            Some("anthropic") => true,
            Some("converse") => false,
            _ => anthropic_model(&req.model),
        };
        let model = urlencoding::encode(&req.model);
        if !anthropic {
            return self.chat_converse(req, &model, on_delta).await;
        }
        let resp = self
            .bedrock_post(
                &format!("/model/{model}/invoke-with-response-stream"),
                &anthropic_body(req),
            )
            .await?;
        let mut stream = AnthropicStream::default();
        events(self.body(resp), |kind, v| {
            if kind != "chunk" {
                return Ok(true);
            }
            let raw = base64::engine::general_purpose::STANDARD
                .decode(v["bytes"].as_str().unwrap_or(""))
                .context("Bedrock chunk is not base64")?;
            let event: Value = serde_json::from_slice(&raw).context("Bedrock chunk is not JSON")?;
            stream.event(event, on_delta)
        })
        .await?;
        Ok(stream.finish(&req.model))
    }

    async fn chat_converse(
        &self,
        req: &ChatRequest,
        model: &str,
        on_delta: &mut (dyn FnMut(Delta) + Send),
    ) -> Result<ChatResponse> {
        let resp = self
            .bedrock_post(
                &format!("/model/{model}/converse-stream"),
                &converse_body(req),
            )
            .await?;
        let mut out = ChatResponse::default();
        let mut watch = ReasoningWatch::default();
        // contentBlockIndex -> (id, name, input json)
        let mut calls: std::collections::BTreeMap<u64, (String, String, String)> =
            Default::default();
        let (mut cache_r, mut cache_w) = (0, 0);
        events(self.body(resp), |kind, v| {
            let i = v["contentBlockIndex"].as_u64().unwrap_or(0);
            match kind {
                "contentBlockStart" => {
                    let t = &v["start"]["toolUse"];
                    if !t.is_null() {
                        if let Some(name) = t["name"].as_str().filter(|n| !n.is_empty()) {
                            on_delta(Delta::ToolCall(name.to_string()));
                        }
                        calls.insert(
                            i,
                            (
                                t["toolUseId"].as_str().unwrap_or("").into(),
                                t["name"].as_str().unwrap_or("").into(),
                                String::new(),
                            ),
                        );
                    }
                }
                "contentBlockDelta" => {
                    let d = &v["delta"];
                    if let Some(t) = d["text"].as_str().filter(|t| !t.is_empty()) {
                        out.text.push_str(t);
                        on_delta(Delta::Text(t.to_string()));
                    }
                    let r = &d["reasoningContent"];
                    if let Some(t) = r["text"]
                        .as_str()
                        .or_else(|| r["reasoningText"]["text"].as_str())
                        .filter(|t| !t.is_empty())
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
                    if let Some(input) = d["toolUse"]["input"].as_str() {
                        calls.entry(i).or_default().2.push_str(input);
                    }
                }
                "messageStop" => {
                    out.finish_reason = Some(
                        match v["stopReason"].as_str().unwrap_or("end_turn") {
                            "max_tokens" | "model_context_window_exceeded" => "length",
                            "tool_use" => "tool_calls",
                            "guardrail_intervened" | "content_filtered" => "content_filter",
                            _ => "stop",
                        }
                        .into(),
                    );
                }
                "metadata" => {
                    let u = &v["usage"];
                    cache_r = u["cacheReadInputTokens"].as_u64().unwrap_or(0);
                    cache_w = u["cacheWriteInputTokens"].as_u64().unwrap_or(0);
                    out.usage.prompt_tokens =
                        u["inputTokens"].as_u64().unwrap_or(0) + cache_r + cache_w;
                    out.usage.completion_tokens = u["outputTokens"].as_u64().unwrap_or(0);
                    return Ok(false);
                }
                _ => {}
            }
            Ok(true)
        })
        .await?;
        out.usage.cached_tokens = cache_r;
        out.usage.cache_write_tokens = cache_w;
        for (_, (id, name, args)) in calls {
            if name.is_empty() {
                continue;
            }
            let arguments = if args.trim().is_empty() {
                json!({})
            } else {
                parse_tool_args(&args)
            };
            out.tool_calls.push(ToolCall {
                id: if id.is_empty() { new_call_id() } else { id },
                name,
                arguments,
            });
        }
        Ok(out)
    }

    /// Signed GET against the Bedrock control plane.
    pub(super) async fn bedrock_get(&self, path: &str) -> Result<(u16, Value)> {
        let vars = self.vars();
        let region = self.bedrock_region(&vars)?;
        let auth = self.bedrock_auth(&vars).await?;
        let base = if self.base_url.is_empty() {
            aws::control_endpoint(&region, &vars)
        } else {
            self.base_url.clone()
        };
        let url = format!("{base}{path}");
        let mut rb = self.http.get(&url);
        for (k, v) in Self::bedrock_headers(&auth, "GET", &url, b"", &region)? {
            rb = rb.header(k, v);
        }
        let r = rb
            .send()
            .await
            .with_context(|| format!("connecting to {base}"))?;
        let status = r.status().as_u16();
        let text = r.text().await.unwrap_or_default();
        Ok((
            status,
            serde_json::from_str(&text).unwrap_or_else(|_| json!({"message": text})),
        ))
    }

    /// Text models this account can call on demand: system inference
    /// profiles, then on-demand foundation models.
    pub(super) async fn bedrock_models(&self) -> Result<Vec<String>> {
        let (status, v) = self
            .bedrock_get("/inference-profiles?maxResults=1000&type=SYSTEM_DEFINED")
            .await?;
        if status != 200 {
            bail!(
                "Bedrock ListInferenceProfiles: HTTP {status}: {}",
                v["message"].as_str().unwrap_or("")
            );
        }
        let mut out: Vec<String> = v["inferenceProfileSummaries"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["status"].as_str().is_none_or(|s| s == "ACTIVE"))
            .filter_map(|p| p["inferenceProfileId"].as_str().map(str::to_string))
            .collect();
        let (status, v) = self
            .bedrock_get("/foundation-models?byOutputModality=TEXT&byInferenceType=ON_DEMAND")
            .await?;
        if status == 200 {
            out.extend(
                v["modelSummaries"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|m| m["responseStreamingSupported"].as_bool() != Some(false))
                    .filter_map(|m| m["modelId"].as_str().map(str::to_string)),
            );
        }
        Ok(out)
    }
}
