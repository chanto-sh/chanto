//! Google Cloud access tokens and project/region for Vertex AI.
//!
//! Application Default Credentials: `GOOGLE_APPLICATION_CREDENTIALS`, else the
//! file `gcloud auth application-default login` writes. `authorized_user`
//! files are exchanged with their refresh token; `service_account` keys sign
//! an RS256 JWT for the JWT-bearer grant. Other credential types (workload
//! identity federation, impersonation) and machines without an ADC file use
//! `gcloud` to print a token. Tokens are cached until five minutes before
//! they expire.

use super::{Cached, Settings, Vars};
use anyhow::{Context, Result, bail};
use base64::Engine;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

#[derive(Clone, Debug, PartialEq)]
pub enum Adc {
    AuthorizedUser {
        client_id: String,
        client_secret: String,
        refresh_token: String,
        quota_project: Option<String>,
        token_uri: String,
    },
    ServiceAccount {
        client_email: String,
        private_key: String,
        private_key_id: Option<String>,
        token_uri: String,
        project_id: Option<String>,
    },
    /// Any other type (`external_account`, `impersonated_service_account`, …): gcloud handles it.
    Other(String),
}

pub fn gcloud_dir(vars: &Vars) -> PathBuf {
    if let Some(p) = vars.path("CLOUDSDK_CONFIG") {
        return p;
    }
    if cfg!(windows)
        && let Some(p) = vars.path("APPDATA")
    {
        return p.join("gcloud");
    }
    vars.home.join(".config").join("gcloud")
}

pub fn adc_path(vars: &Vars) -> PathBuf {
    vars.path("GOOGLE_APPLICATION_CREDENTIALS")
        .unwrap_or_else(|| gcloud_dir(vars).join("application_default_credentials.json"))
}

pub fn parse_adc(text: &str) -> Result<Adc> {
    let v: Value = serde_json::from_str(text).context("credentials file is not JSON")?;
    let s = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(str::to_string);
    Ok(match v["type"].as_str().unwrap_or("") {
        "authorized_user" => Adc::AuthorizedUser {
            client_id: s("client_id").context("ADC has no client_id")?,
            client_secret: s("client_secret").context("ADC has no client_secret")?,
            refresh_token: s("refresh_token").context("ADC has no refresh_token")?,
            quota_project: s("quota_project_id"),
            token_uri: s("token_uri").unwrap_or_else(|| TOKEN_URL.into()),
        },
        "service_account" => Adc::ServiceAccount {
            client_email: s("client_email").context("service account key has no client_email")?,
            private_key: s("private_key").context("service account key has no private_key")?,
            private_key_id: s("private_key_id"),
            token_uri: s("token_uri").unwrap_or_else(|| TOKEN_URL.into()),
            project_id: s("project_id"),
        },
        other => Adc::Other(other.to_string()),
    })
}

/// The ADC file and its parsed contents, if one exists.
pub fn load_adc(vars: &Vars) -> Option<(PathBuf, Result<Adc>)> {
    let p = adc_path(vars);
    let text = std::fs::read_to_string(&p).ok()?;
    Some((p, parse_adc(&text)))
}

/// `project` from the active gcloud configuration.
fn gcloud_project(vars: &Vars) -> Option<String> {
    let dir = gcloud_dir(vars);
    let name = vars
        .get("CLOUDSDK_ACTIVE_CONFIG_NAME")
        .map(str::to_string)
        .or_else(|| {
            std::fs::read_to_string(dir.join("active_config"))
                .ok()
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".into());
    let ini = super::aws::parse_ini(
        &std::fs::read_to_string(dir.join("configurations").join(format!("config_{name}"))).ok()?,
    );
    ini.get("core")?
        .get("project")
        .filter(|p| !p.is_empty())
        .cloned()
}

/// Project: Chanto config, environment, the service account's project, the
/// gcloud configuration, then the ADC quota project.
pub fn project(settings: &Settings, vars: &Vars) -> Option<(String, String)> {
    if let Some(p) = settings.project.as_deref().filter(|p| !p.is_empty()) {
        return Some((p.to_string(), "config".into()));
    }
    if let Some((k, v)) = vars.first(&[
        "ANTHROPIC_VERTEX_PROJECT_ID",
        "GOOGLE_CLOUD_PROJECT",
        "GCLOUD_PROJECT",
        "CLOUDSDK_CORE_PROJECT",
    ]) {
        return Some((v.to_string(), format!("env:{k}")));
    }
    let adc = load_adc(vars).and_then(|(_, a)| a.ok());
    if let Some(Adc::ServiceAccount {
        project_id: Some(p),
        ..
    }) = &adc
    {
        return Some((p.clone(), "service account key".into()));
    }
    if let Some(p) = gcloud_project(vars) {
        return Some((p, "gcloud config".into()));
    }
    if let Some(Adc::AuthorizedUser {
        quota_project: Some(p),
        ..
    }) = &adc
    {
        return Some((p.clone(), "ADC quota project".into()));
    }
    None
}

/// Location: Chanto config, `CLOUD_ML_REGION`, `GOOGLE_CLOUD_LOCATION`, else `global`.
pub fn region(settings: &Settings, vars: &Vars) -> (String, String) {
    if let Some(r) = settings.region.as_deref().filter(|r| !r.is_empty()) {
        return (r.to_string(), "config".into());
    }
    match vars.first(&["CLOUD_ML_REGION", "GOOGLE_CLOUD_LOCATION"]) {
        Some((k, v)) => (v.to_string(), format!("env:{k}")),
        None => ("global".into(), "default".into()),
    }
}

/// `https://{region}-aiplatform.googleapis.com` (`aiplatform.googleapis.com` for `global`).
pub fn host(region: &str) -> String {
    if region == "global" {
        "https://aiplatform.googleapis.com".into()
    } else {
        format!("https://{region}-aiplatform.googleapis.com")
    }
}

/// Where a token would come from, without contacting anything.
pub fn source(vars: &Vars) -> Option<String> {
    match load_adc(vars) {
        Some((p, Ok(Adc::AuthorizedUser { .. }))) => {
            Some(format!("ADC user credentials ({})", p.display()))
        }
        Some((p, Ok(Adc::ServiceAccount { .. }))) => {
            Some(format!("ADC service account key ({})", p.display()))
        }
        Some((p, Ok(Adc::Other(t)))) => Some(format!("ADC {t} via gcloud ({})", p.display())),
        Some((p, Err(_))) => Some(format!("unreadable ADC file ({})", p.display())),
        None => None,
    }
}

fn b64url(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// DER bytes from a PEM block (`-----BEGIN PRIVATE KEY-----`).
pub fn pem_der(pem: &str) -> Result<Vec<u8>> {
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .map(str::trim)
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .context("private key is not valid PEM")
}

/// RS256-signed JWT assertion for the service-account JWT-bearer grant.
pub fn service_account_jwt(
    client_email: &str,
    private_key: &str,
    key_id: Option<&str>,
    aud: &str,
    now: i64,
) -> Result<String> {
    use aws_lc_rs::{rand::SystemRandom, signature};
    let mut header = json!({"alg": "RS256", "typ": "JWT"});
    if let Some(k) = key_id {
        header["kid"] = json!(k);
    }
    let claims =
        json!({"iss": client_email, "scope": SCOPE, "aud": aud, "iat": now, "exp": now + 3600});
    let input = format!(
        "{}.{}",
        b64url(header.to_string().as_bytes()),
        b64url(claims.to_string().as_bytes())
    );
    let key = signature::RsaKeyPair::from_pkcs8(&pem_der(private_key)?)
        .map_err(|e| anyhow::anyhow!("service account private key rejected: {e}"))?;
    let mut sig = vec![0u8; key.public_modulus_len()];
    key.sign(
        &signature::RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        input.as_bytes(),
        &mut sig,
    )
    .map_err(|_| anyhow::anyhow!("signing the service account JWT failed"))?;
    Ok(format!("{input}.{}", b64url(&sig)))
}

async fn token_request(url: &str, form: &[(&str, &str)]) -> Result<Cached<String>> {
    let r = super::http()
        .post(url)
        .form(form)
        .send()
        .await
        .with_context(|| format!("contacting {url}"))?;
    let status = r.status();
    let text = r.text().await.unwrap_or_default();
    if !status.is_success() {
        let v: Value = serde_json::from_str(&text).unwrap_or_default();
        let why = v["error_description"]
            .as_str()
            .or_else(|| v["error"].as_str())
            .unwrap_or(&text);
        bail!(
            "Google token request failed ({status}): {}",
            crate::util::one_line(why, 300)
        );
    }
    let v: Value = serde_json::from_str(&text).context("token response is not JSON")?;
    let token = v["access_token"]
        .as_str()
        .context("token response has no access_token")?
        .to_string();
    Ok(Cached {
        value: token,
        expires: Some(super::now() + v["expires_in"].as_i64().unwrap_or(3600)),
    })
}

/// Exchange ADC for an access token.
pub async fn fetch_token(adc: Option<&Adc>, vars: &Vars) -> Result<Cached<String>> {
    match adc {
        Some(Adc::AuthorizedUser { client_id, client_secret, refresh_token, token_uri, .. }) => {
            token_request(token_uri, &[("grant_type", "refresh_token"), ("client_id", client_id), ("client_secret", client_secret), ("refresh_token", refresh_token)])
                .await
                .context("refreshing Application Default Credentials (run `gcloud auth application-default login` if this keeps failing)")
        }
        Some(Adc::ServiceAccount { client_email, private_key, private_key_id, token_uri, .. }) => {
            let jwt = service_account_jwt(client_email, private_key, private_key_id.as_deref(), token_uri, super::now())?;
            token_request(token_uri, &[("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"), ("assertion", &jwt)]).await
        }
        // gcloud tokens last an hour; assume a bit less since it doesn't say.
        Some(Adc::Other(_)) => gcloud(vars, &["auth", "application-default", "print-access-token"]).await,
        None => gcloud(vars, &["auth", "print-access-token"]).await.context("no Application Default Credentials: run `gcloud auth application-default login` or set GOOGLE_APPLICATION_CREDENTIALS"),
    }
}

async fn gcloud(vars: &Vars, args: &[&str]) -> Result<Cached<String>> {
    let argv: Vec<String> = std::iter::once(vars.cli("gcloud"))
        .chain(args.iter().map(|s| s.to_string()))
        .collect();
    let token = super::run(&argv, Duration::from_secs(60)).await?;
    if token.is_empty() {
        bail!("gcloud printed no access token");
    }
    Ok(Cached {
        value: token,
        expires: Some(super::now() + 45 * 60),
    })
}

static CACHE: LazyLock<tokio::sync::Mutex<HashMap<String, Cached<String>>>> =
    LazyLock::new(Default::default);

/// A current access token for Vertex AI.
pub async fn access_token(vars: &Vars) -> Result<String> {
    let adc = load_adc(vars);
    let key = adc
        .as_ref()
        .map(|(p, _)| p.display().to_string())
        .unwrap_or_else(|| "gcloud".into());
    let mut cache = CACHE.lock().await;
    if let Some(c) = cache.get(&key).filter(|c| c.fresh()) {
        return Ok(c.value.clone());
    }
    let adc = match adc {
        Some((p, a)) => Some(a.with_context(|| format!("reading {}", p.display()))?),
        None => None,
    };
    let t = fetch_token(adc.as_ref(), vars).await?;
    cache.insert(key, t.clone());
    Ok(t.value)
}
