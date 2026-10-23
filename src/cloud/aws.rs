//! AWS credentials and region for Amazon Bedrock.
//!
//! Order (as in the AWS SDKs): a profile named in Chanto config, then
//! `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`, then the `AWS_PROFILE` (or
//! `default`) profile from `~/.aws/credentials` and `~/.aws/config` (static
//! keys, `credential_process`, or an IAM Identity Center session from `aws sso
//! login`), then container and EC2 instance credentials. Anything else a
//! profile can express (assume-role chains, web identity, `aws login`) goes
//! through `aws configure export-credentials`. Expiring credentials are cached
//! and refreshed five minutes before they expire.
//!
//! Bedrock API keys (`AWS_BEARER_TOKEN_BEDROCK`) skip all of this and are sent
//! as a bearer token.

use super::sigv4::Credentials;
use super::{Cached, Settings, Vars};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

/// Resolved credentials and where they came from (never the values).
#[derive(Clone, Debug)]
pub struct Resolved {
    pub creds: Credentials,
    pub source: String,
}

type Ini = BTreeMap<String, BTreeMap<String, String>>;

/// Parse an AWS shared config/credentials file. Section names are returned as
/// written (`default`, `profile dev`, `sso-session corp`); nested sub-sections
/// (indented lines) are skipped.
pub fn parse_ini(text: &str) -> Ini {
    let mut out = Ini::new();
    let mut section: Option<String> = None;
    for raw in text.lines() {
        if raw.starts_with([' ', '\t']) {
            continue;
        }
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
            out.entry(name.clone()).or_default();
            section = Some(name);
        } else if let (Some(s), Some((k, v))) = (&section, line.split_once('=')) {
            let v = v.trim();
            out.get_mut(s)
                .unwrap()
                .insert(k.trim().to_ascii_lowercase(), v.to_string());
        }
    }
    out
}

fn config_path(vars: &Vars) -> PathBuf {
    vars.path("AWS_CONFIG_FILE")
        .unwrap_or_else(|| vars.home.join(".aws").join("config"))
}

fn credentials_path(vars: &Vars) -> PathBuf {
    vars.path("AWS_SHARED_CREDENTIALS_FILE")
        .unwrap_or_else(|| vars.home.join(".aws").join("credentials"))
}

fn read_ini(p: &PathBuf) -> Ini {
    std::fs::read_to_string(p)
        .map(|s| parse_ini(&s))
        .unwrap_or_default()
}

/// A profile's settings: `~/.aws/credentials` keys override `~/.aws/config`.
#[derive(Debug, Default)]
pub struct Profile {
    pub name: String,
    pub keys: BTreeMap<String, String>,
    /// The `[sso-session <name>]` section the profile refers to.
    pub sso_session: Option<BTreeMap<String, String>>,
    pub exists: bool,
}

impl Profile {
    fn get(&self, k: &str) -> Option<&str> {
        self.keys
            .get(k)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }
}

pub fn profile_name(settings: &Settings, vars: &Vars) -> String {
    settings
        .profile
        .clone()
        .or_else(|| {
            vars.first(&["AWS_PROFILE", "AWS_DEFAULT_PROFILE"])
                .map(|(_, v)| v.to_string())
        })
        .unwrap_or_else(|| "default".into())
}

pub fn load_profile(name: &str, vars: &Vars) -> Profile {
    let config = read_ini(&config_path(vars));
    let creds = read_ini(&credentials_path(vars));
    // `[default]` may also be written `[profile default]`.
    let sections = if name == "default" {
        vec!["default".to_string(), "profile default".to_string()]
    } else {
        vec![format!("profile {name}")]
    };
    let mut p = Profile {
        name: name.to_string(),
        ..Default::default()
    };
    for s in &sections {
        if let Some(c) = config.get(s) {
            p.exists = true;
            for (k, v) in c {
                p.keys.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }
    if let Some(c) = creds.get(name) {
        p.exists = true;
        p.keys.extend(c.clone());
    }
    if let Some(s) = p.get("sso_session") {
        p.sso_session = config.get(&format!("sso-session {s}")).cloned();
    }
    p
}

/// Region: Chanto config, `AWS_REGION`, `AWS_DEFAULT_REGION`, then the profile's `region`.
pub fn region(settings: &Settings, vars: &Vars) -> Option<(String, String)> {
    if let Some(r) = settings.region.as_deref().filter(|r| !r.is_empty()) {
        return Some((r.to_string(), "config".into()));
    }
    if let Some((k, v)) = vars.first(&["AWS_REGION", "AWS_DEFAULT_REGION"]) {
        return Some((v.to_string(), format!("env:{k}")));
    }
    let name = profile_name(settings, vars);
    load_profile(&name, vars)
        .get("region")
        .map(|r| (r.to_string(), format!("profile {name}")))
}

/// Bedrock API key from the environment (`AWS_BEARER_TOKEN_BEDROCK`).
pub fn bearer_token(vars: &Vars) -> Option<String> {
    vars.get("AWS_BEARER_TOKEN_BEDROCK").map(str::to_string)
}

fn env_keys(vars: &Vars) -> Option<Credentials> {
    Some(Credentials {
        access_key_id: vars.get("AWS_ACCESS_KEY_ID")?.to_string(),
        secret_access_key: vars.get("AWS_SECRET_ACCESS_KEY")?.to_string(),
        session_token: vars.get("AWS_SESSION_TOKEN").map(str::to_string),
    })
}

fn static_keys(p: &Profile) -> Option<Credentials> {
    Some(Credentials {
        access_key_id: p.get("aws_access_key_id")?.to_string(),
        secret_access_key: p.get("aws_secret_access_key")?.to_string(),
        session_token: p.get("aws_session_token").map(str::to_string),
    })
}

/// How a profile gets credentials, without running anything.
fn profile_kind(p: &Profile) -> Option<&'static str> {
    if static_keys(p).is_some() {
        Some("static keys")
    } else if p.get("credential_process").is_some() {
        Some("credential_process")
    } else if p.sso_session.is_some() || p.get("sso_start_url").is_some() {
        Some("IAM Identity Center (SSO)")
    } else if p.get("role_arn").is_some()
        || p.get("web_identity_token_file").is_some()
        || p.get("login_session").is_some()
    {
        Some("aws CLI")
    } else {
        None
    }
}

/// Where credentials would come from, without running anything or reading
/// values. `None` when nothing is configured.
pub fn source(settings: &Settings, vars: &Vars) -> Option<String> {
    if bearer_token(vars).is_some() {
        return Some("env:AWS_BEARER_TOKEN_BEDROCK (Bedrock API key)".into());
    }
    let name = profile_name(settings, vars);
    let describe = |p: &Profile| profile_kind(p).map(|k| format!("AWS profile {} ({k})", p.name));
    if settings.profile.is_some() {
        return describe(&load_profile(&name, vars))
            .or_else(|| Some(format!("AWS profile {name} (not found)")));
    }
    if env_keys(vars).is_some() {
        return Some("env:AWS_ACCESS_KEY_ID".into());
    }
    if let Some(d) = describe(&load_profile(&name, vars)) {
        return Some(d);
    }
    if vars.get("AWS_WEB_IDENTITY_TOKEN_FILE").is_some() {
        return Some("web identity (aws CLI)".into());
    }
    if vars
        .first(&[
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
            "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
        ])
        .is_some()
    {
        return Some("container credentials".into());
    }
    None
}

/// The `credential_process` / `aws configure export-credentials` output format.
pub fn parse_process_output(text: &str) -> Result<Cached<Credentials>> {
    let v: Value = serde_json::from_str(text).context("credential process output is not JSON")?;
    if v["Version"].as_i64().is_some_and(|n| n != 1) {
        bail!("unsupported credential process version {}", v["Version"]);
    }
    let creds = Credentials {
        access_key_id: v["AccessKeyId"]
            .as_str()
            .context("credential process output has no AccessKeyId")?
            .to_string(),
        secret_access_key: v["SecretAccessKey"]
            .as_str()
            .context("credential process output has no SecretAccessKey")?
            .to_string(),
        session_token: v["SessionToken"]
            .as_str()
            .or_else(|| v["Token"].as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    };
    Ok(Cached {
        value: creds,
        expires: expiry(&v["Expiration"]),
    })
}

/// An `Expiration`: RFC 3339 text or epoch milliseconds.
fn expiry(v: &Value) -> Option<i64> {
    match v {
        Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.timestamp()),
        Value::Number(n) => n.as_i64().map(|ms| ms / 1000),
        _ => None,
    }
}

async fn credential_process(cmd: &str) -> Result<Cached<Credentials>> {
    let argv = shell_words::split(cmd).context("credential_process is not a valid command line")?;
    parse_process_output(&super::run(&argv, Duration::from_secs(60)).await?)
}

async fn export_credentials(profile: Option<&str>, vars: &Vars) -> Result<Cached<Credentials>> {
    let mut argv = vec![
        vars.cli("aws"),
        "configure".into(),
        "export-credentials".into(),
        "--format".into(),
        "process".into(),
    ];
    if let Some(p) = profile {
        argv.extend(["--profile".into(), p.to_string()]);
    }
    let out = super::run(&argv, Duration::from_secs(60)).await.context(
        "`aws configure export-credentials` (AWS CLI 2.9 or later) is needed for this profile",
    )?;
    parse_process_output(&out)
}

/// The cache file `aws sso login` writes: `~/.aws/sso/cache/<sha1(session or start URL)>.json`.
pub fn sso_cache_file(p: &Profile, vars: &Vars) -> Option<PathBuf> {
    let key = match &p.sso_session {
        Some(_) => p.get("sso_session")?.to_string(),
        None => p.get("sso_start_url")?.to_string(),
    };
    let digest =
        aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY, key.as_bytes());
    Some(
        vars.home
            .join(".aws/sso/cache")
            .join(format!("{}.json", super::sigv4::hex(digest.as_ref()))),
    )
}

async fn sso(p: &Profile, vars: &Vars) -> Result<Cached<Credentials>> {
    let login = || format!("run `aws sso login --profile {}`", p.name);
    let file = sso_cache_file(p, vars).context("incomplete SSO profile")?;
    let token: Value = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .with_context(|| format!("no cached SSO session; {}", login()))?;
    if expiry(&token["expiresAt"]).is_none_or(|e| e <= super::now()) {
        bail!("the SSO session expired; {}", login());
    }
    let region = p
        .sso_session
        .as_ref()
        .and_then(|s| s.get("sso_region").cloned())
        .or_else(|| p.get("sso_region").map(str::to_string))
        .context("SSO profile has no sso_region")?;
    let (account, role) = (
        p.get("sso_account_id")
            .context("SSO profile has no sso_account_id")?,
        p.get("sso_role_name")
            .context("SSO profile has no sso_role_name")?,
    );
    let base = vars
        .get("AWS_ENDPOINT_URL_SSO")
        .map(str::to_string)
        .unwrap_or_else(|| format!("https://portal.sso.{region}.amazonaws.com"));
    let r = super::http()
        .get(format!(
            "{}/federation/credentials",
            base.trim_end_matches('/')
        ))
        .query(&[("account_id", account), ("role_name", role)])
        .header(
            "x-amz-sso_bearer_token",
            token["accessToken"].as_str().unwrap_or(""),
        )
        .send()
        .await
        .context("contacting IAM Identity Center")?;
    let status = r.status();
    let v: Value = r.json().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "IAM Identity Center refused the SSO session ({status}); {}",
            login()
        );
    }
    let c = &v["roleCredentials"];
    Ok(Cached {
        value: Credentials {
            access_key_id: c["accessKeyId"]
                .as_str()
                .context("SSO response has no credentials")?
                .to_string(),
            secret_access_key: c["secretAccessKey"].as_str().unwrap_or("").to_string(),
            session_token: c["sessionToken"].as_str().map(str::to_string),
        },
        expires: expiry(&c["expiration"]),
    })
}

/// ECS/EKS container credentials.
async fn container(vars: &Vars) -> Result<Option<Cached<Credentials>>> {
    let url = match (
        vars.get("AWS_CONTAINER_CREDENTIALS_FULL_URI"),
        vars.get("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"),
    ) {
        (Some(full), _) => full.to_string(),
        (None, Some(rel)) => format!("http://169.254.170.2{rel}"),
        _ => return Ok(None),
    };
    let mut rb = super::http().get(url);
    let token = match vars.path("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE") {
        Some(f) => std::fs::read_to_string(f)
            .ok()
            .map(|s| s.trim().to_string()),
        None => vars
            .get("AWS_CONTAINER_AUTHORIZATION_TOKEN")
            .map(str::to_string),
    };
    if let Some(t) = token {
        rb = rb.header("authorization", t);
    }
    let text = rb
        .send()
        .await
        .context("contacting the container credentials endpoint")?
        .error_for_status()?
        .text()
        .await?;
    parse_process_output(&text).map(Some)
}

/// EC2 instance profile credentials (IMDSv2), with a short timeout off EC2.
async fn imds(vars: &Vars) -> Result<Option<Cached<Credentials>>> {
    if vars
        .get("AWS_EC2_METADATA_DISABLED")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"))
    {
        return Ok(None);
    }
    let base = vars
        .get("AWS_EC2_METADATA_SERVICE_ENDPOINT")
        .unwrap_or("http://169.254.169.254")
        .trim_end_matches('/')
        .to_string();
    let http = crate::http::builder()
        .timeout(Duration::from_secs(1))
        .no_proxy()
        .build()?;
    let Ok(r) = http
        .put(format!("{base}/latest/api/token"))
        .header("x-aws-ec2-metadata-token-ttl-seconds", "21600")
        .send()
        .await
    else {
        return Ok(None);
    };
    let Ok(token) = r.error_for_status().map(|r| r.text()) else {
        return Ok(None);
    };
    let token = token.await?;
    let path = format!("{base}/latest/meta-data/iam/security-credentials/");
    let role = http
        .get(&path)
        .header("x-aws-ec2-metadata-token", &token)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let role = role
        .lines()
        .next()
        .context("this EC2 instance has no IAM role")?
        .trim()
        .to_string();
    let text = http
        .get(format!("{path}{role}"))
        .header("x-aws-ec2-metadata-token", &token)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    parse_process_output(&text).map(Some)
}

/// Credentials from a profile; `None` when the profile doesn't exist.
async fn from_profile(p: &Profile, vars: &Vars) -> Result<Option<(Cached<Credentials>, String)>> {
    if !p.exists {
        return Ok(None);
    }
    let label = |k: &str| format!("AWS profile {} ({k})", p.name);
    if let Some(c) = static_keys(p) {
        return Ok(Some((
            Cached {
                value: c,
                expires: None,
            },
            label("static keys"),
        )));
    }
    if let Some(cmd) = p.get("credential_process") {
        let c = credential_process(cmd)
            .await
            .with_context(|| format!("credential_process for AWS profile {}", p.name))?;
        return Ok(Some((c, label("credential_process"))));
    }
    if p.sso_session.is_some() || p.get("sso_start_url").is_some() {
        return Ok(Some((
            sso(p, vars).await?,
            label("IAM Identity Center (SSO)"),
        )));
    }
    Ok(Some((
        export_credentials(Some(&p.name), vars).await?,
        label("aws CLI"),
    )))
}

/// Resolved credentials by profile name, with the label to show.
type CredCache = tokio::sync::Mutex<HashMap<String, (Cached<Credentials>, String)>>;

static CACHE: LazyLock<CredCache> = LazyLock::new(Default::default);

/// Resolve SigV4 credentials (see the module docs for the order).
pub async fn credentials(settings: &Settings, vars: &Vars) -> Result<Resolved> {
    let name = profile_name(settings, vars);
    if settings.profile.is_none()
        && let Some(c) = env_keys(vars)
    {
        return Ok(Resolved {
            creds: c,
            source: "env:AWS_ACCESS_KEY_ID".into(),
        });
    }
    let key = format!(
        "{name}\0{}\0{}",
        config_path(vars).display(),
        credentials_path(vars).display()
    );
    let mut cache = CACHE.lock().await;
    if let Some((c, source)) = cache.get(&key).filter(|(c, _)| c.fresh()) {
        return Ok(Resolved {
            creds: c.value.clone(),
            source: source.clone(),
        });
    }
    let profile = load_profile(&name, vars);
    let found = match from_profile(&profile, vars).await? {
        Some(found) => found,
        None if settings.profile.is_some() => bail!(
            "AWS profile `{name}` not found in {} or {}",
            config_path(vars).display(),
            credentials_path(vars).display()
        ),
        None if vars.get("AWS_WEB_IDENTITY_TOKEN_FILE").is_some() => (
            export_credentials(None, vars).await?,
            "web identity (aws CLI)".into(),
        ),
        None => match container(vars).await? {
            Some(c) => (c, "container credentials".into()),
            None => match imds(vars).await.ok().flatten() {
                Some(c) => (c, "EC2 instance profile".into()),
                None => bail!(
                    "no AWS credentials found. Set AWS_PROFILE (or [providers.bedrock] profile) to a profile in ~/.aws/config, export AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY, or use a Bedrock API key in AWS_BEARER_TOKEN_BEDROCK"
                ),
            },
        },
    };
    // Static keys are re-read each time so edits to the file take effect.
    if found.0.expires.is_some()
        || found.1.ends_with("(credential_process)")
        || found.1.ends_with("(aws CLI)")
    {
        cache.insert(key, found.clone());
    }
    Ok(Resolved {
        creds: found.0.value,
        source: found.1,
    })
}

/// Bedrock runtime endpoint for a region.
pub fn runtime_endpoint(region: &str, vars: &Vars) -> String {
    if let Some((_, url)) = vars.first(&["AWS_ENDPOINT_URL_BEDROCK_RUNTIME", "AWS_ENDPOINT_URL"]) {
        return url.trim_end_matches('/').to_string();
    }
    format!("https://bedrock-runtime.{region}.{}", dns_suffix(region))
}

/// Bedrock control-plane endpoint (model listing).
pub fn control_endpoint(region: &str, vars: &Vars) -> String {
    if let Some((_, url)) = vars.first(&["AWS_ENDPOINT_URL_BEDROCK", "AWS_ENDPOINT_URL"]) {
        return url.trim_end_matches('/').to_string();
    }
    format!("https://bedrock.{region}.{}", dns_suffix(region))
}

fn dns_suffix(region: &str) -> &'static str {
    if region.starts_with("cn-") {
        "amazonaws.com.cn"
    } else {
        "amazonaws.com"
    }
}
