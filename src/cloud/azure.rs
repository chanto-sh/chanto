//! Azure OpenAI and Azure AI Foundry endpoints and credentials.
//!
//! - Azure OpenAI (`azure/<deployment>`): the v1 API at
//!   `https://<resource>.openai.azure.com/openai/v1`.
//! - Foundry (`foundry/<model>`): Claude through the Anthropic Messages API at
//!   `https://<resource>.services.ai.azure.com/anthropic/v1/messages`, other
//!   models through `…/openai/v1/chat/completions`.
//!
//! Auth: an API key (`api-key` / `x-api-key`), else a Microsoft Entra ID token
//! for `https://cognitiveservices.azure.com`: a service principal from
//! `AZURE_TENANT_ID`/`AZURE_CLIENT_ID`/`AZURE_CLIENT_SECRET`, or the Azure CLI
//! (`az account get-access-token`). Tokens are cached until shortly before expiry.

use super::{Cached, Settings, Vars};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

pub const RESOURCE: &str = "https://cognitiveservices.azure.com";

pub const OPENAI_KEY_ENV: &[&str] = &["AZURE_OPENAI_API_KEY"];
pub const FOUNDRY_KEY_ENV: &[&str] = &["AZURE_FOUNDRY_API_KEY", "ANTHROPIC_FOUNDRY_API_KEY"];

/// `https://<resource>.openai.azure.com/openai/v1` from config or environment.
/// `base_url` (from `[providers.azure]`) wins and gets `/openai/v1` appended
/// when it's a bare resource URL.
pub fn openai_base(settings: &Settings, base_url: &str, vars: &Vars) -> Result<String> {
    let root = if !base_url.is_empty() {
        base_url.to_string()
    } else if let Some(r) = settings.resource.as_deref().filter(|r| !r.is_empty()) {
        format!("https://{r}.openai.azure.com")
    } else if let Some(e) = vars.get("AZURE_OPENAI_ENDPOINT") {
        e.to_string()
    } else if let Some((_, r)) = vars.first(&["AZURE_OPENAI_RESOURCE", "AZURE_RESOURCE_NAME"]) {
        format!("https://{r}.openai.azure.com")
    } else {
        bail!(
            "no Azure OpenAI resource: set AZURE_OPENAI_ENDPOINT (https://<resource>.openai.azure.com) or [providers.azure] resource"
        );
    };
    let root = root.trim_end_matches('/');
    let root = root
        .strip_suffix("/openai/v1")
        .or_else(|| root.strip_suffix("/openai"))
        .unwrap_or(root);
    Ok(format!("{root}/openai/v1"))
}

/// `https://<resource>.services.ai.azure.com` from config or environment.
pub fn foundry_root(settings: &Settings, base_url: &str, vars: &Vars) -> Result<String> {
    let root = if !base_url.is_empty() {
        base_url.to_string()
    } else if let Some(r) = settings.resource.as_deref().filter(|r| !r.is_empty()) {
        format!("https://{r}.services.ai.azure.com")
    } else if let Some((_, u)) =
        vars.first(&["AZURE_FOUNDRY_BASE_URL", "ANTHROPIC_FOUNDRY_BASE_URL"])
    {
        u.to_string()
    } else if let Some((_, r)) =
        vars.first(&["AZURE_FOUNDRY_RESOURCE", "ANTHROPIC_FOUNDRY_RESOURCE"])
    {
        format!("https://{r}.services.ai.azure.com")
    } else {
        bail!(
            "no Azure AI Foundry resource: set AZURE_FOUNDRY_RESOURCE (the <resource> in <resource>.services.ai.azure.com) or [providers.foundry] resource"
        );
    };
    let mut root = root.trim_end_matches('/');
    for suffix in ["/v1", "/anthropic", "/openai/v1", "/openai", "/models"] {
        root = root.strip_suffix(suffix).unwrap_or(root);
    }
    Ok(root.to_string())
}

/// An API key from the environment, with the variable's name.
pub fn env_key(foundry: bool, vars: &Vars) -> Option<(&'static str, String)> {
    vars.first(if foundry {
        FOUNDRY_KEY_ENV
    } else {
        OPENAI_KEY_ENV
    })
    .map(|(k, v)| (k, v.to_string()))
}

/// Where an Entra ID token would come from, without running anything.
pub fn entra_source(vars: &Vars) -> &'static str {
    if service_principal(vars).is_some() {
        "Entra ID service principal (env:AZURE_CLIENT_ID)"
    } else {
        "Entra ID via `az account get-access-token`"
    }
}

fn service_principal(vars: &Vars) -> Option<(&str, &str, &str)> {
    Some((
        vars.get("AZURE_TENANT_ID")?,
        vars.get("AZURE_CLIENT_ID")?,
        vars.get("AZURE_CLIENT_SECRET")?,
    ))
}

/// The `az account get-access-token` JSON: `expires_on` (epoch seconds) on
/// current CLIs, else `expiresOn` local time; assume 30 minutes without either.
pub fn parse_az_token(text: &str) -> Result<Cached<String>> {
    let v: Value = serde_json::from_str(text).context("az printed no JSON")?;
    let token = v["accessToken"]
        .as_str()
        .context("az printed no accessToken")?
        .to_string();
    let expires = v["expires_on"]
        .as_i64()
        .or_else(|| v["expires_on"].as_str().and_then(|s| s.parse().ok()))
        .or_else(|| {
            let s = v["expiresOn"].as_str()?;
            let t = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f").ok()?;
            t.and_local_timezone(chrono::Local)
                .single()
                .map(|t| t.timestamp())
        })
        .unwrap_or_else(|| super::now() + 30 * 60);
    Ok(Cached {
        value: token,
        expires: Some(expires),
    })
}

static CACHE: LazyLock<tokio::sync::Mutex<HashMap<String, Cached<String>>>> =
    LazyLock::new(Default::default);

/// A Microsoft Entra ID token for Azure AI services.
pub async fn entra_token(vars: &Vars) -> Result<String> {
    entra_token_with(vars, "https://login.microsoftonline.com").await
}

pub async fn entra_token_with(vars: &Vars, authority: &str) -> Result<String> {
    let key = service_principal(vars)
        .map(|(t, c, _)| format!("{t}/{c}"))
        .unwrap_or_else(|| "az".into());
    let mut cache = CACHE.lock().await;
    if let Some(c) = cache.get(&key).filter(|c| c.fresh()) {
        return Ok(c.value.clone());
    }
    let t = match service_principal(vars) {
        Some((tenant, client, secret)) => {
            let r = super::http()
                .post(format!(
                    "{}/{tenant}/oauth2/v2.0/token",
                    authority.trim_end_matches('/')
                ))
                .form(&[
                    ("grant_type", "client_credentials"),
                    ("client_id", client),
                    ("client_secret", secret),
                    ("scope", &format!("{RESOURCE}/.default")),
                ])
                .send()
                .await
                .context("contacting Microsoft Entra ID")?;
            let status = r.status();
            let v: Value = r.json().await.unwrap_or_default();
            if !status.is_success() {
                bail!(
                    "Entra ID token request failed ({status}): {}",
                    crate::util::one_line(v["error_description"].as_str().unwrap_or(""), 300)
                );
            }
            Cached {
                value: v["access_token"]
                    .as_str()
                    .context("Entra ID returned no access_token")?
                    .to_string(),
                expires: Some(super::now() + v["expires_in"].as_i64().unwrap_or(3600)),
            }
        }
        None => {
            let argv: Vec<String> = [
                vars.cli("az").as_str(),
                "account",
                "get-access-token",
                "--resource",
                RESOURCE,
                "--output",
                "json",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            let out = super::run(&argv, Duration::from_secs(60))
                .await
                .context("no Azure API key (AZURE_OPENAI_API_KEY / AZURE_FOUNDRY_API_KEY) and no Entra ID token; install the Azure CLI and run `az login`, or set AZURE_TENANT_ID/AZURE_CLIENT_ID/AZURE_CLIENT_SECRET")?;
            parse_az_token(&out)?
        }
    };
    cache.insert(key, t.clone());
    Ok(t.value)
}
