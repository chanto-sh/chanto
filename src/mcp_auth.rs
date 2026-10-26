//! OAuth for remote (streamable HTTP) MCP servers, following the MCP
//! authorization spec: protected-resource and authorization-server
//! discovery (RFC 9728 / RFC 8414 / OIDC), dynamic client registration
//! (RFC 7591), authorization code + PKCE with a loopback redirect, the
//! `resource` parameter (RFC 8707), token storage and refresh.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Per-server OAuth settings (`[mcp_servers.<name>.oauth]`), needed only for
/// servers without dynamic client registration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct OAuthConfig {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// Space-separated scopes to request (default: what the server advertises).
    pub scopes: Option<String>,
    /// Fixed loopback port, for clients registered with an exact redirect URI.
    pub callback_port: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix seconds.
    pub expires_at: Option<i64>,
    pub token_endpoint: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub resource: Option<String>,
    pub scope: Option<String>,
}

impl Tokens {
    pub fn expired(&self) -> bool {
        self.expires_at
            .is_some_and(|t| chrono::Utc::now().timestamp() >= t - 30)
    }
}

/// Returned by the HTTP transport when the server answers 401.
#[derive(Debug)]
pub struct AuthRequired {
    pub www_authenticate: Option<String>,
}

impl std::fmt::Display for AuthRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "authentication required")
    }
}

impl std::error::Error for AuthRequired {}

// ------------------------------------------------------------------ storage

/// ~/.chanto/mcp-auth/<name>-<hash of name+url>.json
pub fn store_path(name: &str, url: &str) -> PathBuf {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.bytes().chain([0u8]).chain(url.bytes()) {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    crate::config::home_dir()
        .join("mcp-auth")
        .join(format!("{safe}-{h:016x}.json"))
}

pub fn load(name: &str, url: &str) -> Option<Tokens> {
    let t: Tokens = serde_json::from_slice(&std::fs::read(store_path(name, url)).ok()?).ok()?;
    mask_tokens(name, &t);
    Some(t)
}

/// Mask the server's tokens like any other secret.
fn mask_tokens(name: &str, t: &Tokens) {
    let label = format!("{}_MCP_TOKEN", name.to_uppercase());
    crate::secrets::register(&label, &t.access_token);
    if let Some(r) = &t.refresh_token {
        crate::secrets::register(&label, r);
    }
}

pub fn save(name: &str, url: &str, t: &Tokens) -> Result<()> {
    mask_tokens(name, t);
    let p = store_path(name, url);
    std::fs::create_dir_all(p.parent().unwrap())?;
    let data = serde_json::to_vec_pretty(t)?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&p)?;
        // The file may predate this code with wider permissions.
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(&data)?;
    }
    #[cfg(not(unix))]
    std::fs::write(&p, data)?;
    Ok(())
}

pub fn delete(name: &str, url: &str) -> bool {
    std::fs::remove_file(store_path(name, url)).is_ok()
}

// ---------------------------------------------------------------- discovery

/// A parameter from a `WWW-Authenticate: Bearer k="v", ...` header.
pub fn www_auth_param(header: &str, key: &str) -> Option<String> {
    let re = regex::Regex::new(&format!(
        r#"(?i)(?:^|[\s,]){}\s*=\s*(?:"([^"]*)"|([^\s,]+))"#,
        regex::escape(key)
    ))
    .ok()?;
    let c = re.captures(header)?;
    c.get(1).or(c.get(2)).map(|m| m.as_str().to_string())
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuthMeta {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    /// RFC 8707 resource indicator for the MCP server.
    pub resource: String,
    pub scopes: Option<String>,
}

async fn get_json(http: &reqwest::Client, url: &str) -> Option<Value> {
    let r = http
        .get(url)
        .header("Accept", "application/json")
        .header("MCP-Protocol-Version", "2025-06-18")
        .send()
        .await
        .ok()?;
    if !r.status().is_success() {
        return None;
    }
    r.json().await.ok()
}

fn split_url(u: &str) -> Result<(String, String)> {
    let p = reqwest::Url::parse(u).with_context(|| format!("invalid URL {u}"))?;
    Ok((
        p.origin().ascii_serialization(),
        p.path().trim_end_matches('/').to_string(),
    ))
}

pub async fn discover(
    http: &reqwest::Client,
    server_url: &str,
    www_auth: Option<&str>,
) -> Result<AuthMeta> {
    let (origin, path) = split_url(server_url)?;
    // 1. Protected resource metadata: the header's pointer, then well-known URLs.
    let mut candidates = Vec::new();
    if let Some(u) = www_auth.and_then(|h| www_auth_param(h, "resource_metadata")) {
        candidates.push(u);
    }
    if !path.is_empty() {
        candidates.push(format!(
            "{origin}/.well-known/oauth-protected-resource{path}"
        ));
    }
    candidates.push(format!("{origin}/.well-known/oauth-protected-resource"));
    let mut prm = None;
    for c in &candidates {
        if let Some(v) = get_json(http, c).await {
            prm = Some(v);
            break;
        }
    }
    // The metadata must describe this server (MCP authorization spec), or a
    // server could point us at another resource's authorization server.
    if let Some(r) = prm.as_ref().and_then(|v| v["resource"].as_str()) {
        let norm = |u: &str| u.trim_end_matches('/').to_ascii_lowercase();
        let (ro, _) = split_url(r)?;
        if !ro.eq_ignore_ascii_case(&origin) || !norm(server_url).starts_with(&norm(r)) {
            bail!("protected resource metadata is for {r}, not {server_url}");
        }
    }
    let (issuer, resource, mut scopes) = match &prm {
        Some(v) => (
            v["authorization_servers"][0]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| origin.clone()),
            v["resource"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| server_url.to_string()),
            v["scopes_supported"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|s| !s.is_empty()),
        ),
        // 2025-03-26 servers: the authorization server lives at the MCP server's origin.
        None => (origin.clone(), server_url.to_string(), None),
    };
    if let Some(s) = www_auth.and_then(|h| www_auth_param(h, "scope")) {
        scopes = Some(s);
    }
    // 2. Authorization server metadata (RFC 8414, then OpenID Connect discovery).
    let (iorigin, ipath) = split_url(&issuer)?;
    let as_candidates = if ipath.is_empty() {
        vec![
            format!("{iorigin}/.well-known/oauth-authorization-server"),
            format!("{iorigin}/.well-known/openid-configuration"),
        ]
    } else {
        vec![
            format!("{iorigin}/.well-known/oauth-authorization-server{ipath}"),
            format!("{iorigin}/.well-known/openid-configuration{ipath}"),
            format!(
                "{}/.well-known/openid-configuration",
                issuer.trim_end_matches('/')
            ),
        ]
    };
    for c in &as_candidates {
        let Some(v) = get_json(http, c).await else {
            continue;
        };
        let (Some(a), Some(t)) = (
            v["authorization_endpoint"].as_str(),
            v["token_endpoint"].as_str(),
        ) else {
            continue;
        };
        if let Some(methods) = v["code_challenge_methods_supported"].as_array()
            && !methods.iter().any(|m| m == "S256")
        {
            bail!("authorization server {issuer} does not support PKCE S256, which MCP requires");
        }
        if scopes.is_none() {
            scopes = v["scopes_supported"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|s| !s.is_empty());
        }
        return Ok(AuthMeta {
            authorization_endpoint: a.to_string(),
            token_endpoint: t.to_string(),
            registration_endpoint: v["registration_endpoint"].as_str().map(str::to_string),
            resource,
            scopes,
        });
    }
    if prm.is_none() {
        // 2025-03-26 fallback: default endpoint paths at the server origin.
        return Ok(AuthMeta {
            authorization_endpoint: format!("{iorigin}/authorize"),
            token_endpoint: format!("{iorigin}/token"),
            registration_endpoint: Some(format!("{iorigin}/register")),
            resource,
            scopes,
        });
    }
    bail!("no OAuth authorization server metadata found for {issuer}")
}

/// Dynamic client registration; returns (client_id, client_secret).
pub async fn register(
    http: &reqwest::Client,
    endpoint: &str,
    redirect_uri: &str,
    scopes: Option<&str>,
) -> Result<(String, Option<String>)> {
    let mut body = json!({
        "client_name": "chanto",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    if let Some(s) = scopes {
        body["scope"] = json!(s);
    }
    let r = http
        .post(endpoint)
        .json(&body)
        .send()
        .await
        .context("client registration")?;
    let status = r.status();
    let v: Value = r.json().await.unwrap_or_default();
    if !status.is_success() {
        bail!("client registration failed ({status}): {v}");
    }
    let id = v["client_id"]
        .as_str()
        .ok_or_else(|| anyhow!("registration response has no client_id"))?;
    Ok((
        id.to_string(),
        v["client_secret"].as_str().map(str::to_string),
    ))
}

// -------------------------------------------------------------------- login

/// PKCE verifier and S256 challenge.
pub fn pkce() -> (String, String) {
    use base64::Engine;
    use sha2::Digest;
    let verifier = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn authorize_url(
    meta: &AuthMeta,
    client_id: &str,
    redirect: &str,
    challenge: &str,
    state: &str,
    scope: Option<&str>,
) -> Result<String> {
    let mut params = vec![
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", redirect),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("resource", meta.resource.as_str()),
    ];
    if let Some(s) = scope {
        params.push(("scope", s));
    }
    Ok(reqwest::Url::parse_with_params(&meta.authorization_endpoint, &params)?.to_string())
}

const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>chanto</title><body style=\"font-family:system-ui;padding:3em\"><h2>Logged in</h2><p>You can close this tab and return to the terminal.</p>";

/// Serve the loopback redirect until the browser delivers the code.
async fn wait_for_code(listener: tokio::net::TcpListener, state: &str) -> Result<String> {
    let q = wait_for_params(listener, state, "/callback").await?;
    q.get("code").cloned().context("missing code")
}

/// Wait for the OAuth redirect on `path` and return its query parameters.
/// With a non-empty `state`, requests that don't carry it are rejected (any
/// local page could hit this URL). Flows without state rely on PKCE.
pub async fn wait_for_params(
    listener: tokio::net::TcpListener,
    state: &str,
    path: &str,
) -> Result<std::collections::HashMap<String, String>> {
    loop {
        let (mut sock, _) = listener.accept().await?;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 16384 {
            match sock.read(&mut tmp).await {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        let req = String::from_utf8_lossy(&buf);
        let target = req
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("/")
            .to_string();
        let Ok(url) = reqwest::Url::parse(&format!("http://127.0.0.1{target}")) else {
            continue;
        };
        let respond = |status: &str, body: &str| {
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        };
        if url.path() != path {
            let _ = sock
                .write_all(respond("404 Not Found", "").as_bytes())
                .await;
            continue;
        }
        let q: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
        // Check state first: any local page could hit this URL with `?error=`.
        if !state.is_empty() && q.get("state").map(String::as_str) != Some(state) {
            let _ = sock
                .write_all(respond("400 Bad Request", "<h2>State mismatch</h2>").as_bytes())
                .await;
            continue;
        }
        if let Some(e) = q.get("error") {
            let msg = format!(
                "{e}: {}",
                q.get("error_description").map(String::as_str).unwrap_or("")
            );
            let _ = sock
                .write_all(
                    respond(
                        "400 Bad Request",
                        &format!("<h2>Login failed</h2><p>{}</p>", html_escape(&msg)),
                    )
                    .as_bytes(),
                )
                .await;
            bail!("authorization failed: {msg}");
        }
        if !q.contains_key("code") {
            let _ = sock
                .write_all(respond("400 Bad Request", "<h2>Missing code</h2>").as_bytes())
                .await;
            continue;
        }
        let _ = sock
            .write_all(respond("200 OK", DONE_PAGE).as_bytes())
            .await;
        return Ok(q);
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

async fn token_request(
    http: &reqwest::Client,
    endpoint: &str,
    form: &[(&str, String)],
) -> Result<Value> {
    let r = http
        .post(endpoint)
        .header("Accept", "application/json")
        .form(form)
        .send()
        .await
        .context("token request")?;
    let status = r.status();
    let body = r.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "token endpoint returned {status}: {}",
            crate::util::truncate(&body, 300)
        );
    }
    serde_json::from_str(&body).with_context(|| {
        format!(
            "token response is not JSON: {}",
            crate::util::truncate(&body, 200)
        )
    })
}

fn tokens_from(v: &Value, base: &Tokens) -> Result<Tokens> {
    let access = v["access_token"]
        .as_str()
        .ok_or_else(|| anyhow!("token response has no access_token"))?;
    Ok(Tokens {
        access_token: access.to_string(),
        refresh_token: v["refresh_token"]
            .as_str()
            .map(str::to_string)
            .or_else(|| base.refresh_token.clone()),
        expires_at: v["expires_in"]
            .as_i64()
            .map(|s| chrono::Utc::now().timestamp() + s),
        scope: v["scope"]
            .as_str()
            .map(str::to_string)
            .or_else(|| base.scope.clone()),
        ..base.clone()
    })
}

/// Probe the server for its `WWW-Authenticate` challenge.
async fn probe(http: &reqwest::Client, server_url: &str) -> Option<String> {
    let body = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "chanto", "version": env!("CARGO_PKG_VERSION")}}});
    let r = http
        .post(server_url)
        .header("Accept", "application/json, text/event-stream")
        .json(&body)
        .send()
        .await
        .ok()?;
    r.headers()
        .get("www-authenticate")
        .and_then(|h| h.to_str().ok())
        .map(str::to_string)
}

/// Run the full browser login. `open` is called with the authorization URL.
/// `www` is the server's `WWW-Authenticate` challenge, if already known.
pub async fn login(
    server_url: &str,
    cfg: &OAuthConfig,
    http: &reqwest::Client,
    www: Option<String>,
    open: &(dyn Fn(&str) + Send + Sync),
) -> Result<Tokens> {
    let www = match www {
        Some(w) => Some(w),
        None => probe(http, server_url).await,
    };
    let meta = discover(http, server_url, www.as_deref()).await?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", cfg.callback_port.unwrap_or(0)))
        .await
        .context("binding the login callback port")?;
    let redirect = format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr()?.port()
    );
    let scope = cfg.scopes.clone().or_else(|| meta.scopes.clone());
    let (client_id, client_secret) = match (&cfg.client_id, &meta.registration_endpoint) {
        (Some(id), _) => (id.clone(), cfg.client_secret.clone()),
        (None, Some(reg)) => register(http, reg, &redirect, scope.as_deref()).await?,
        (None, None) => bail!(
            "the server doesn't support dynamic client registration; set `oauth.client_id` for this server in config"
        ),
    };
    let (verifier, challenge) = pkce();
    let state = uuid::Uuid::new_v4().simple().to_string();
    let url = authorize_url(
        &meta,
        &client_id,
        &redirect,
        &challenge,
        &state,
        scope.as_deref(),
    )?;
    open(&url);
    let code = tokio::time::timeout(Duration::from_secs(300), wait_for_code(listener, &state))
        .await
        .map_err(|_| anyhow!("timed out after 5 minutes waiting for the browser login"))??;
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code),
        ("redirect_uri", redirect),
        ("client_id", client_id.clone()),
        ("code_verifier", verifier),
        ("resource", meta.resource.clone()),
    ];
    if let Some(s) = &client_secret {
        form.push(("client_secret", s.clone()));
    }
    let v = token_request(http, &meta.token_endpoint, &form).await?;
    let base = Tokens {
        token_endpoint: meta.token_endpoint.clone(),
        client_id,
        client_secret,
        resource: Some(meta.resource.clone()),
        scope,
        ..Default::default()
    };
    tokens_from(&v, &base)
}

pub async fn refresh(http: &reqwest::Client, t: &Tokens) -> Result<Tokens> {
    let rt = t
        .refresh_token
        .clone()
        .ok_or_else(|| anyhow!("no refresh token"))?;
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", rt),
        ("client_id", t.client_id.clone()),
    ];
    if let Some(r) = &t.resource {
        form.push(("resource", r.clone()));
    }
    if let Some(s) = &t.client_secret {
        form.push(("client_secret", s.clone()));
    }
    let v = token_request(http, &t.token_endpoint, &form).await?;
    tokens_from(&v, t)
}

/// Open a URL in the user's browser (best effort).
pub fn open_browser(url: &str) {
    let mut cmd = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        // Not `cmd /C start`: cmd would interpret `&` in the query string.
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    let _ = cmd
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn www_authenticate_params() {
        let h = r#"Bearer realm="mcp", resource_metadata="https://x.dev/.well-known/oauth-protected-resource", scope=read"#;
        assert_eq!(
            www_auth_param(h, "resource_metadata").as_deref(),
            Some("https://x.dev/.well-known/oauth-protected-resource")
        );
        assert_eq!(www_auth_param(h, "scope").as_deref(), Some("read"));
        assert_eq!(www_auth_param(h, "error"), None);
    }

    #[test]
    fn pkce_is_s256() {
        use base64::Engine;
        use sha2::Digest;
        let (v, c) = pkce();
        assert!(v.len() >= 43);
        assert_eq!(
            c,
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(sha2::Sha256::digest(v.as_bytes()))
        );
    }

    /// Live check against a real server (discovery + registration, no login):
    /// `MCP_DISCOVER_URL=https://… cargo test live_discovery -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_discovery() {
        let url = std::env::var("MCP_DISCOVER_URL").expect("set MCP_DISCOVER_URL");
        let http = crate::http::client();
        let www = probe(&http, &url).await;
        println!("WWW-Authenticate: {www:?}");
        let meta = discover(&http, &url, www.as_deref()).await.unwrap();
        println!("{meta:#?}");
        if let Some(reg) = &meta.registration_endpoint {
            let r = register(
                &http,
                reg,
                "http://127.0.0.1:53682/callback",
                meta.scopes.as_deref(),
            )
            .await;
            println!(
                "registration: {:?}",
                r.map(|(id, secret)| (id, secret.is_some()))
            );
        }
    }
}
