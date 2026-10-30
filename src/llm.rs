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
use serde_json::{Value, json};
use std::time::Duration;

pub use chanto_core::message::{
    ChatRequest, ChatResponse, Delta, Message, ProviderItems, ProviderKind, Role, ToolCall,
    ToolSpec, Usage,
};
pub use chanto_core::model::*;

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

/// A fresh cached answer for this endpoint+model: `Some(None)` when the server
/// had no metadata for it, `None` when nothing fresh is cached. A `\0`
/// separates the key's two parts so no base URL / model name can collide with
/// another pairing.
fn model_info_cached(base_url: &str, model: &str) -> Option<Option<(u64, bool)>> {
    let key = format!("{base_url}\u{0}{model}");
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(model_info_cache_path()).ok()?).ok()?;
    let e = v.get(&key)?;
    if unix_now().saturating_sub(e.get("t")?.as_u64()?) > MODEL_INFO_TTL_SECS {
        return None;
    }
    if e.get("unknown").and_then(Value::as_bool) == Some(true) {
        return Some(None);
    }
    Some(Some((e.get("ctx")?.as_u64()?, e.get("vision")?.as_bool()?)))
}

/// Record the server's answer for this endpoint+model, `None` when it had no
/// metadata. Best-effort: a write failure just means the next start fetches
/// again. The temp-then-rename keeps a concurrent reader from seeing a
/// half-written file.
fn model_info_cache_store(base_url: &str, model: &str, val: Option<(u64, bool)>) {
    let path = model_info_cache_path();
    let mut v: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    let Some(obj) = v.as_object_mut() else { return };
    obj.insert(
        format!("{base_url}\u{0}{model}"),
        match val {
            Some((ctx, vision)) => json!({ "ctx": ctx, "vision": vision, "t": unix_now() }),
            None => json!({ "unknown": true, "t": unix_now() }),
        },
    );
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
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
        let http = crate::http::model_client(base_url);
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
            return hit;
        }
        let looked = self.model_info_uncached(model).await;
        // An answer is cached even when it has no size (DeepSeek's model list
        // gives none), so the next start skips the round trip; a server that
        // couldn't be reached is asked again next time.
        if cacheable && let Some(info) = looked {
            model_info_cache_store(&self.base_url, model, info);
        }
        looked.flatten()
    }

    /// `None` when the server couldn't be asked, `Some(None)` when it answered
    /// without the metadata.
    async fn model_info_uncached(&self, model: &str) -> Option<Option<(u64, bool)>> {
        match self.kind {
            ProviderKind::Ollama => {}
            ProviderKind::Anthropic => return Some(self.anthropic_model_info(model).await),
            // No network: the context table is enough, and startup stays fast.
            ProviderKind::OpenaiResponses
            | ProviderKind::Bedrock
            | ProviderKind::Vertex
            | ProviderKind::Azure
            | ProviderKind::Foundry => {
                return Some(Some((hosted_context(self.kind, model) as u64, true)));
            }
            ProviderKind::Openai => {
                return self
                    .openai_model_info(model)
                    .await
                    .map(|ctx| ctx.map(|ctx| (ctx, true)));
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
        Some(Some((ctx, vision)))
    }

    /// Context window an OpenAI-compatible server reports for `model`: the
    /// model list's `max_model_len` (vLLM), `context_length` (OpenRouter-style
    /// catalogs), `max_context_length` (LM Studio) or `meta.n_ctx_train`
    /// (llama.cpp), else LiteLLM's `/model/info`. Quick, since startup waits.
    /// `None` when the server couldn't be asked; `Some(None)` when it answered
    /// without a size, as DeepSeek's list does.
    async fn openai_model_info(&self, model: &str) -> Option<Option<u64>> {
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
        let listed = get(format!("{base}/models")).await?;
        let entry = listed["data"]
            .as_array()
            .and_then(|a| a.iter().find(|m| m["id"] == model));
        if let Some(m) = entry {
            let ctx = ["max_model_len", "context_length", "max_context_length"]
                .iter()
                .find_map(|k| m[*k].as_u64())
                .or_else(|| m["meta"]["n_ctx_train"].as_u64());
            if ctx.is_some() {
                return Some(ctx);
            }
        }
        let v = get(format!("{}/model/info", base.trim_end_matches("/v1"))).await;
        Some(v.and_then(|v| {
            v["data"]
                .as_array()?
                .iter()
                .filter(|m| m["model_name"] == model)
                .find_map(|m| m["model_info"]["max_input_tokens"].as_u64())
        }))
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
        let mut decoder = OpenaiDecoder::new(req);
        let mut stream = self.body(resp);
        while let Some(chunk) = stream.next().await {
            if decoder.feed(&chunk?, on_delta)? {
                break;
            }
        }
        decoder.finish()
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
}
