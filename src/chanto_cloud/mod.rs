//! Chanto cloud beyond runs of the current repository (`crate::runs::cloud`,
//! which also owns the sign-in): its model gateway as the built-in `chanto`
//! provider (`chanto/<model>`), and agents (`chanto agents`, `chanto cloud
//! run --agent`).
//!
//! The gateway takes the account token as `x-api-key` (Anthropic API) or a
//! bearer token (OpenAI API) and bills the org in `x-chanto-org`, else the
//! caller's personal org.

pub mod agents;
#[cfg(test)]
mod tests;

use crate::runs::cloud::{Cloud, default_org};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The provider id: `chanto/<model>`.
pub const PROVIDER_ID: &str = "chanto";

/// `chanto login cloud` and `chanto login chanto` mean `chanto cloud login`.
pub fn is_login_name(id: &str) -> bool {
    matches!(id, "cloud" | PROVIDER_ID)
}

/// The message in a gateway error body: Chanto's `ApiError`, or the
/// Anthropic and OpenAI shapes.
fn error_message(text: &str) -> String {
    let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    [&v["message"], &v["error"]["message"], &v["error"]]
        .into_iter()
        .find_map(|m| m.as_str().filter(|s| !s.is_empty()).map(str::to_string))
        .unwrap_or_else(|| crate::util::one_line(text, 300))
}

/// A model gateway error for the `chanto` provider, and what to do about it.
pub fn gateway_error(status: u16, text: &str) -> String {
    let message = error_message(text);
    match status {
        401 if message.contains("chanto login") || message.contains("chanto cloud login") => {
            format!("chanto cloud didn't accept the token: {message}")
        }
        401 => format!(
            "chanto cloud didn't accept the token: {message}. Run `chanto cloud login` again, or check CHANTO_CLOUD_TOKEN"
        ),
        402 => format!(
            "chanto cloud: {message}. `chanto cloud status` shows the org; an admin can raise its spending limit or turn on billing"
        ),
        _ => format!("chanto cloud: {message} (HTTP {status})"),
    }
}

/// The `chanto` provider for `model`, if signed in.
pub fn provider(model: &str) -> Option<crate::llm::Provider> {
    let c = Cloud::signed_in().ok()?;
    // So the cost of its calls is known: the gateway publishes what it charges.
    crate::cost::refresh_gateway_prices(&c.url, c.token());
    Some(provider_for(&c.url, c.token(), default_org(), model))
}

/// Claude models go to the gateway's Anthropic API, others to its
/// OpenAI-compatible one; models are listed from its catalog, which has both.
fn provider_for(url: &str, token: &str, org: Option<String>, model: &str) -> crate::llm::Provider {
    use crate::llm::{Provider, ProviderKind, hosted_context};
    let mut p = if model.is_empty() || model.starts_with("claude") {
        Provider::new(
            ProviderKind::Anthropic,
            &format!("{url}/v1/model/anthropic"),
            Some(token.into()),
            hosted_context(ProviderKind::Anthropic, model),
        )
    } else {
        Provider::new(
            ProviderKind::Openai,
            &format!("{url}/v1/model/openai/v1"),
            Some(token.into()),
            131_072,
        )
    };
    p.billing_provider = Some(PROVIDER_ID.into());
    p.models_url = Some(format!("{url}/v1/gateway/models"));
    if let Some(org) = org {
        p.headers.push(("x-chanto-org", org));
    }
    p
}

/// Where the token comes from (never its value), if signed in.
pub fn credential_source() -> Option<String> {
    if std::env::var("CHANTO_CLOUD_TOKEN").is_ok_and(|t| !t.is_empty()) {
        return Some("env:CHANTO_CLOUD_TOKEN".into());
    }
    crate::auth::get(crate::runs::cloud::CREDENTIAL).map(|_| {
        crate::config::home_dir()
            .join("auth.json")
            .display()
            .to_string()
    })
}

/// `chanto providers` status for the `chanto` provider.
pub fn provider_status() -> String {
    match (credential_source(), Cloud::signed_in()) {
        (Some(source), Ok(c)) if source.starts_with("env:") => {
            format!("token from CHANTO_CLOUD_TOKEN · {}", c.url)
        }
        (Some(_), Ok(c)) => format!("signed in · {}", c.url),
        _ => "chanto cloud login".into(),
    }
}

/// For `chanto doctor`: the server, where the token comes from and the org
/// (no requests, no secrets).
pub fn doctor_info() -> Value {
    let c = Cloud::signed_in().ok();
    json!({
        "url": c.as_ref().map(|c| c.url.clone()),
        "credential_source": credential_source(),
        "org": default_org().unwrap_or_else(|| "personal".into()),
    })
}

#[derive(Debug, Serialize, Deserialize)]
struct GatewayModel {
    id: String,
    #[serde(default)]
    default: bool,
    #[serde(default)]
    input_usd_per_mtok: f64,
    #[serde(default)]
    output_usd_per_mtok: f64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct GatewayModelList {
    models: Vec<GatewayModel>,
}

/// `chanto cloud models [--json]`: the gateway's catalog with prices.
pub async fn models_cmd(args: &[String]) -> Result<String> {
    let list: GatewayModelList = Cloud::signed_in()?.get("/v1/gateway/models").await?;
    if args.iter().any(|a| a == "--json") {
        return Ok(serde_json::to_string_pretty(&list)?);
    }
    let mut out = format!(
        "{:<36} {:>12} {:>12}\n",
        "MODEL (use as chanto/<model>)", "IN $/MTOK", "OUT $/MTOK"
    );
    for m in &list.models {
        out.push_str(&format!(
            "{:<36} {:>12.2} {:>12.2}{}\n",
            m.id,
            m.input_usd_per_mtok,
            m.output_usd_per_mtok,
            if m.default {
                "  (default for runs)"
            } else {
                ""
            }
        ));
    }
    Ok(out.trim_end().to_string())
}
