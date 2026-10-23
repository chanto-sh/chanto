//! Credentials and wire formats for the cloud model platforms: Amazon Bedrock
//! (SigV4 signing, AWS event streams), Google Vertex AI (Application Default
//! Credentials) and Azure (API keys, Entra ID tokens).
//!
//! Nothing here runs at startup. Credentials are resolved on the first request
//! to a cloud model and cached until shortly before they expire; detection
//! (`chanto doctor`, setup) only reads environment variables and local files.

pub mod aws;
pub mod azure;
pub mod eventstream;
pub mod gcp;
pub mod sigv4;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// Per-provider settings from `[providers.<name>]` (all optional).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// AWS region (`us-east-1`) or Google Cloud location (`us-east5`, `global`).
    pub region: Option<String>,
    /// AWS shared-config profile (`~/.aws/config`, `~/.aws/credentials`).
    pub profile: Option<String>,
    /// Google Cloud project ID.
    pub project: Option<String>,
    /// Azure resource name: `<resource>.openai.azure.com` / `<resource>.services.ai.azure.com`.
    pub resource: Option<String>,
    /// Azure OpenAI `api-version` query parameter. The v1 API needs none.
    pub api_version: Option<String>,
    /// Wire API: `anthropic` or `converse` (Bedrock), `anthropic` or `openai`
    /// (Vertex, Foundry), `chat` or `responses` (Azure OpenAI). Chosen from the
    /// model name when unset.
    pub api: Option<String>,
}

/// Environment variables the cloud providers read (cleared by hermetic tests).
#[cfg(test)]
pub const ENV_VARS: &[&str] = &[
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_PROFILE",
    "AWS_DEFAULT_PROFILE",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
    "AWS_CONFIG_FILE",
    "AWS_SHARED_CREDENTIALS_FILE",
    "AWS_BEARER_TOKEN_BEDROCK",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_ENDPOINT_URL",
    "AWS_ENDPOINT_URL_BEDROCK_RUNTIME",
    "AWS_ENDPOINT_URL_BEDROCK",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CLOUD_PROJECT",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "GCLOUD_PROJECT",
    "CLOUDSDK_CORE_PROJECT",
    "CLOUDSDK_CONFIG",
    "CLOUD_ML_REGION",
    "GOOGLE_CLOUD_LOCATION",
    "AZURE_OPENAI_API_KEY",
    "AZURE_OPENAI_ENDPOINT",
    "AZURE_OPENAI_RESOURCE",
    "AZURE_OPENAI_DEPLOYMENT",
    "AZURE_OPENAI_API_VERSION",
    "AZURE_FOUNDRY_API_KEY",
    "AZURE_FOUNDRY_RESOURCE",
    "AZURE_FOUNDRY_BASE_URL",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_FOUNDRY_RESOURCE",
    "ANTHROPIC_FOUNDRY_BASE_URL",
    "AZURE_TENANT_ID",
    "AZURE_CLIENT_ID",
    "AZURE_CLIENT_SECRET",
];

/// A snapshot of the environment for credential discovery. Tests build one
/// by hand so they never read the real home directory or process variables.
#[derive(Clone, Debug, Default)]
pub struct Vars {
    vars: BTreeMap<String, String>,
    /// Home directory for `~/.aws` and `~/.config/gcloud`.
    pub home: PathBuf,
}

impl Vars {
    pub fn process() -> Self {
        Vars {
            vars: std::env::vars().collect(),
            home: crate::compat::home(),
        }
    }

    #[cfg(test)]
    pub fn new(home: impl Into<PathBuf>, vars: &[(&str, &str)]) -> Self {
        Vars {
            vars: vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            home: home.into(),
        }
    }

    /// A non-empty variable.
    pub fn get(&self, k: &str) -> Option<&str> {
        self.vars.get(k).map(|s| s.trim()).filter(|s| !s.is_empty())
    }

    /// The first set variable of `keys`, with its name.
    pub fn first(&self, keys: &[&'static str]) -> Option<(&'static str, &str)> {
        keys.iter().find_map(|k| self.get(k).map(|v| (*k, v)))
    }

    /// Program used for a cloud CLI fallback (`aws`, `gcloud`, `az`);
    /// `CHANTO_<NAME>_CLI` points at another binary.
    pub fn cli(&self, name: &str) -> String {
        self.get(&format!("CHANTO_{}_CLI", name.to_ascii_uppercase()))
            .unwrap_or(name)
            .to_string()
    }

    /// A path given in a variable, with `~/` expanded against [`Vars::home`].
    pub fn path(&self, k: &str) -> Option<PathBuf> {
        self.get(k).map(|p| match p.strip_prefix("~/") {
            Some(rest) => self.home.join(rest),
            None => PathBuf::from(p),
        })
    }
}

/// Run a credential helper and return its stdout. No shell: `argv[0]` is the program.
pub async fn run(argv: &[String], timeout: Duration) -> Result<String> {
    let (prog, args) = argv.split_first().context("empty command")?;
    let mut cmd = tokio::process::Command::new(prog);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(timeout, cmd.output()).await {
        Err(_) => bail!("`{prog}` timed out after {}s", timeout.as_secs()),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("`{prog}` is not installed")
        }
        Ok(r) => r.with_context(|| format!("running `{prog}`"))?,
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "`{}` failed: {}",
            argv.join(" "),
            crate::util::one_line(err.trim(), 300)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Seconds since the epoch.
pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// A cached token or key set, valid until `expires` (epoch seconds).
#[derive(Clone, Debug)]
pub struct Cached<T> {
    pub value: T,
    pub expires: Option<i64>,
}

impl<T> Cached<T> {
    /// Usable for at least another five minutes (or forever without an expiry).
    pub fn fresh(&self) -> bool {
        self.expires.is_none_or(|e| e - now() > 300)
    }
}

/// HTTP client for token endpoints (no proxy surprises beyond the system's).
pub fn http() -> reqwest::Client {
    crate::http::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()
        .expect("http client")
}
