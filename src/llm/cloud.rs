//! Endpoints, auth headers, model lists and health checks for the cloud
//! platforms (Bedrock's request path lives in `bedrock.rs`).

use super::*;
use crate::cloud::{Vars, aws, azure, gcp};

/// Context window for a cloud model. Claude on the clouds gets 200k: the 1M
/// window isn't available everywhere, and overshooting fails requests.
pub(super) fn context(kind: ProviderKind, m: &str) -> u32 {
    if m.contains("claude") {
        return 200_000;
    }
    if m.contains("gemini") {
        return 1_048_576;
    }
    match kind {
        ProviderKind::Azure => hosted_context(ProviderKind::OpenaiResponses, m),
        _ if m.contains("nova-pro") || m.contains("nova-lite") || m.contains("nova-premier") => {
            300_000
        }
        _ if m.contains("qwen3") => 262_144,
        _ => 128_000,
    }
}

/// Health of a cloud provider for `chanto doctor`; never contains secrets.
pub struct CloudCheck {
    pub credential: Option<String>,
    /// Region, project, resource, endpoint.
    pub details: Vec<(String, String)>,
    pub reachable: bool,
    pub note: String,
}

fn http_note(status: u16, body: &Value) -> String {
    let msg = body["message"]
        .as_str()
        .or_else(|| body["error"]["message"].as_str())
        .or_else(|| body["Message"].as_str())
        .unwrap_or("");
    format!(
        "HTTP {status}{}",
        if msg.is_empty() {
            String::new()
        } else {
            format!(": {}", crate::util::one_line(msg, 160))
        }
    )
}

impl Provider {
    /// Whether a Vertex or Foundry model goes to the Anthropic Messages API.
    pub(super) fn cloud_claude(&self, model: &str) -> bool {
        match self.cloud.api.as_deref() {
            Some("anthropic") => true,
            Some("openai" | "chat") => false,
            _ => model.to_ascii_lowercase().contains("claude"),
        }
    }

    /// Whether an Azure deployment uses the Responses API (codex and `-pro`
    /// models only take Responses; `api = "responses"` forces it).
    pub(super) fn azure_responses(&self, model: &str) -> bool {
        match self.cloud.api.as_deref() {
            Some("responses") => true,
            Some("chat") => false,
            _ => {
                let m = model.to_ascii_lowercase();
                m.contains("codex") || m.ends_with("-pro")
            }
        }
    }

    /// Model name on the wire: Vertex's OpenAI endpoint wants `publisher/model`.
    pub(super) fn wire_model(&self, model: &str) -> String {
        if self.kind == ProviderKind::Vertex && !model.contains('/') {
            format!("google/{model}")
        } else {
            model.to_string()
        }
    }

    /// `…/v1/projects/{project}/locations/{region}`.
    async fn vertex_location(&self, vars: &Vars) -> Result<String> {
        let (project, _) = gcp::project(&self.cloud, vars).context("no Google Cloud project for Vertex AI: set GOOGLE_CLOUD_PROJECT or [providers.vertex] project")?;
        let (region, _) = gcp::region(&self.cloud, vars);
        let host = if self.base_url.is_empty() {
            gcp::host(&region)
        } else {
            self.base_url.clone()
        };
        Ok(format!("{host}/v1/projects/{project}/locations/{region}"))
    }

    pub(super) async fn vertex_claude_url(&self, model: &str) -> Result<String> {
        Ok(format!(
            "{}/publishers/anthropic/models/{model}:streamRawPredict",
            self.vertex_location(&self.vars()).await?
        ))
    }

    pub(super) async fn foundry_anthropic_url(&self) -> Result<String> {
        Ok(format!(
            "{}/anthropic/v1/messages",
            azure::foundry_root(&self.cloud, &self.base_url, &self.vars())?
        ))
    }

    pub(super) async fn cloud_openai_url(&self, path: &str) -> Result<String> {
        let vars = self.vars();
        Ok(match self.kind {
            ProviderKind::Vertex => format!(
                "{}/endpoints/openapi/{path}",
                self.vertex_location(&vars).await?
            ),
            ProviderKind::Foundry => format!(
                "{}/openai/v1/{path}",
                azure::foundry_root(&self.cloud, &self.base_url, &vars)?
            ),
            _ => {
                let base = azure::openai_base(&self.cloud, &self.base_url, &vars)?;
                match self
                    .cloud
                    .api_version
                    .as_deref()
                    .or_else(|| vars.get("AZURE_OPENAI_API_VERSION"))
                    .filter(|v| *v != "v1")
                {
                    Some(v) => format!("{base}/{path}?api-version={}", urlencoding::encode(v)),
                    None => format!("{base}/{path}"),
                }
            }
        })
    }

    /// Auth headers for Vertex (OAuth bearer) and Azure (API key or Entra ID
    /// bearer). `anthropic` picks Foundry's `x-api-key` over Azure's `api-key`.
    pub(super) async fn cloud_auth_headers(
        &self,
        anthropic: bool,
    ) -> Result<Vec<(&'static str, String)>> {
        let vars = self.vars();
        if self.kind == ProviderKind::Vertex {
            return Ok(vec![(
                "authorization",
                format!("Bearer {}", gcp::access_token(&vars).await?),
            )]);
        }
        let key = self
            .api_key
            .clone()
            .or_else(|| azure::env_key(self.kind == ProviderKind::Foundry, &vars).map(|(_, k)| k));
        Ok(match key {
            Some(k) => vec![(if anthropic { "x-api-key" } else { "api-key" }, k)],
            None => vec![(
                "authorization",
                format!("Bearer {}", azure::entra_token(&vars).await?),
            )],
        })
    }

    /// Models for the picker. Bedrock lists what the account can call; Azure
    /// names deployments, which only the account knows (`AZURE_OPENAI_DEPLOYMENT`).
    pub(super) async fn cloud_models(&self) -> Result<Vec<String>> {
        let vars = self.vars();
        Ok(match self.kind {
            ProviderKind::Bedrock => return self.bedrock_models().await,
            ProviderKind::Azure => vars
                .get("AZURE_OPENAI_DEPLOYMENT")
                .map(|d| vec![d.to_string()])
                .unwrap_or_default(),
            _ => crate::providers::cloud_models(self.kind.cloud_id().unwrap_or(""))
                .iter()
                .map(|s| s.to_string())
                .collect(),
        })
    }

    /// Credentials, location and a request that spends no tokens.
    pub async fn cloud_check(&self, model: &str) -> CloudCheck {
        let vars = self.vars();
        let mut details = Vec::new();
        let mut check = CloudCheck {
            credential: None,
            details: vec![],
            reachable: false,
            note: String::new(),
        };
        let result: Result<(u16, Value)> = async {
            match self.kind {
                ProviderKind::Bedrock => {
                    check.credential = if self.api_key.is_some() { Some("configured Bedrock API key".into()) } else { aws::source(&self.cloud, &vars) };
                    let (region, from) = aws::region(&self.cloud, &vars).context("no AWS region: set AWS_REGION or [providers.bedrock] region")?;
                    details.push(("region".into(), format!("{region} ({from})")));
                    details.push(("endpoint".into(), self.bedrock_runtime(&region, &vars)));
                    details.push(("api".into(), if self.cloud.api.as_deref() == Some("converse") || (self.cloud.api.is_none() && !bedrock::anthropic_model(model)) { "Converse".into() } else { "InvokeModel (Anthropic Messages)".into() }));
                    // Resolve credentials first so their errors are reported as such.
                    if let bedrock::Auth::Sig(r) = self.bedrock_auth(&vars).await? {
                        check.credential = Some(r.source);
                    }
                    self.bedrock_get("/foundation-models?byOutputModality=TEXT").await
                }
                ProviderKind::Vertex => {
                    check.credential = gcp::source(&vars).or_else(|| Some("gcloud auth print-access-token".into()));
                    let (project, from) = gcp::project(&self.cloud, &vars).context("no Google Cloud project: set GOOGLE_CLOUD_PROJECT or [providers.vertex] project")?;
                    details.push(("project".into(), format!("{project} ({from})")));
                    let (region, from) = gcp::region(&self.cloud, &vars);
                    details.push(("region".into(), format!("{region} ({from})")));
                    details.push(("endpoint".into(), if self.base_url.is_empty() { gcp::host(&region) } else { self.base_url.clone() }));
                    details.push(("api".into(), if self.cloud_claude(model) { "streamRawPredict (Anthropic Messages)".into() } else { "OpenAI-compatible chat completions".into() }));
                    let token = gcp::access_token(&vars).await?;
                    let r = self.http.get(self.vertex_location(&vars).await?).bearer_auth(token).send().await?;
                    let status = r.status().as_u16();
                    Ok((status, r.json().await.unwrap_or_default()))
                }
                _ => {
                    let foundry = self.kind == ProviderKind::Foundry;
                    check.credential = if self.api_key.is_some() {
                        Some("configured API key".into())
                    } else {
                        Some(azure::env_key(foundry, &vars).map(|(k, _)| format!("env:{k}")).unwrap_or_else(|| azure::entra_source(&vars).into()))
                    };
                    let url = self.cloud_openai_url("models").await?;
                    details.push(("endpoint".into(), url.split("/openai/").next().unwrap_or("").to_string()));
                    if foundry {
                        details.push(("api".into(), if self.cloud_claude(model) { "Anthropic Messages".into() } else { "OpenAI-compatible chat completions".into() }));
                    } else {
                        details.push(("api".into(), if self.azure_responses(model) { "Responses".into() } else { "chat completions".into() }));
                    }
                    let mut rb = self.http.get(url);
                    for (k, v) in self.cloud_auth_headers(false).await? {
                        rb = rb.header(k, v);
                    }
                    let r = rb.send().await?;
                    let status = r.status().as_u16();
                    Ok((status, r.json().await.unwrap_or_default()))
                }
            }
        }
        .await;
        check.details = details;
        match result {
            Ok((status, body)) => {
                check.reachable = true;
                check.note = match status {
                    200..=299 => "credentials accepted".into(),
                    401 => format!("credentials rejected ({})", http_note(status, &body)),
                    403 => format!(
                        "reachable, but this identity may lack a permission ({}); see docs/reference.md#cloud-providers-bedrock-vertex-ai-azure",
                        http_note(status, &body)
                    ),
                    404 => format!(
                        "reachable, but the project, region or resource wasn't found ({})",
                        http_note(status, &body)
                    ),
                    _ => http_note(status, &body),
                };
            }
            Err(e) => check.note = crate::util::one_line(&format!("{e:#}"), 300),
        }
        check
    }
}
