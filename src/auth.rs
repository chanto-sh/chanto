//! Provider logins: `chanto login <provider>` / `/login`.
//!
//! - **ChatGPT**: OpenAI's "Sign in with ChatGPT" for third-party apps. Each
//!   installation registers dynamically and gets a token scoped to
//!   `chatgpt.tokens.use.direct`, used as the API key for the OpenAI API, so
//!   requests draw on the user's ChatGPT plan allowance.
//! - **OpenRouter**: OpenRouter's OAuth (PKCE) flow, which issues an API key.
//! - **Any catalog provider**: paste an API key once instead of exporting it.
//! - **Chanto cloud** (`chanto login cloud`, the same as `chanto cloud
//!   login`): see `runs::cloud`; its models are the `chanto` provider.
//!
//! Credentials live in `~/.chanto/auth.json` (mode 0600).
//!
//! **Accounts.** `chanto login <provider> --as <name>` saves a second
//! credential for the same provider under `<provider>@<name>`, next to the
//! unnamed (default) one, so older Chanto versions still read the file.
//! `account = "<name>"` in the config picks that login for every provider that
//! has one; `/login <provider> <name>` switches for the rest of the session.
//!
//! **Keychain.** With `CHANTO_KEYCHAIN=1` (macOS) the secrets live in the login
//! keychain and `auth.json` keeps only a placeholder; see [`keychain`].

mod keychain;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    ApiKey {
        key: String,
    },
    Oauth {
        access: String,
        refresh: String,
        expires: u64,
        client_id: String,
    },
    /// A Chanto server that hosts background runs (`chanto remote add`),
    /// stored as `remote:<name>`; also the Chanto cloud sign-in
    /// (`chanto cloud login`), stored as `cloud`.
    Remote {
        url: String,
        token: String,
    },
    /// The real credential is in the OS keychain ([`keychain`]); only written
    /// when [`keychain::enabled`], and always resolved back on load.
    Keychain,
}

fn path() -> PathBuf {
    crate::config::home_dir().join("auth.json")
}

fn load_all() -> BTreeMap<String, Credential> {
    let mut all: BTreeMap<String, Credential> = std::fs::read(path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    // A placeholder is resolved from the keychain; one that can't be resolved
    // (keychain wiped, or a copy on another machine) is treated as absent.
    for (key, cred) in all.iter_mut() {
        if matches!(cred, Credential::Keychain)
            && let Some(real) = keychain::load(key).and_then(|b| serde_json::from_slice(&b).ok())
        {
            *cred = real;
        }
    }
    all.retain(|_, c| !matches!(c, Credential::Keychain));
    all
}

fn save_all(all: &BTreeMap<String, Credential>) -> Result<()> {
    let mut file = all.clone();
    if keychain::enabled() && keychain::available() {
        // Keep the secrets in the keychain and only a placeholder in the file.
        let previous = load_all();
        for (key, cred) in file.iter_mut() {
            if matches!(cred, Credential::Keychain) {
                continue;
            }
            keychain::save(key, &serde_json::to_vec(cred)?)
                .map_err(|e| anyhow::anyhow!("keychain: {e}"))?;
            *cred = Credential::Keychain;
        }
        for key in previous.keys().filter(|k| !all.contains_key(*k)) {
            let _ = keychain::delete(key);
        }
    }
    let p = path();
    std::fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4().simple()));
    // Create with 0600 so the tokens are never briefly world-readable, and use a
    // random name so another process can't pre-create the path (TOCTOU).
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(&file)?)?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, serde_json::to_vec_pretty(&file)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

/// Saved run servers: name → (url, token).
pub fn remotes() -> BTreeMap<String, (String, String)> {
    load_all()
        .into_iter()
        .filter_map(|(k, c)| match (k.strip_prefix("remote:"), c) {
            (Some(name), Credential::Remote { url, token }) => {
                Some((name.to_string(), (url, token)))
            }
            _ => None,
        })
        .collect()
}

/// The name for the unnamed login in listings and `/login <provider> default`.
pub const DEFAULT_ACCOUNT: &str = "default";

/// `account` from the config, for every provider with a login by that name.
static CONFIG_ACCOUNT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
/// Per-provider choices made in this session (`/login <provider> <name>`).
static SESSION_ACCOUNTS: std::sync::Mutex<BTreeMap<String, String>> =
    std::sync::Mutex::new(BTreeMap::new());

pub fn set_config_account(name: Option<String>) {
    *CONFIG_ACCOUNT.lock().unwrap_or_else(|e| e.into_inner()) =
        name.filter(|n| !n.trim().is_empty());
}

/// A valid account name: letters, digits, `.`, `_`, `-`.
pub fn valid_account(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Where a provider's login is stored: `provider`, or `provider@name`.
fn account_key(provider: &str, account: Option<&str>) -> String {
    match account.filter(|a| *a != DEFAULT_ACCOUNT) {
        Some(a) => format!("{provider}@{a}"),
        None => provider.to_string(),
    }
}

/// The account chosen for `provider` (session choice, else the config's), if any.
fn chosen_account(provider: &str) -> Option<String> {
    if let Some(a) = SESSION_ACCOUNTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(provider)
    {
        return Some(a.clone());
    }
    CONFIG_ACCOUNT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// The key whose credential `provider` uses now: the chosen account when it
/// has a saved login, else the default one.
fn active_key(all: &BTreeMap<String, Credential>, provider: &str) -> String {
    match chosen_account(provider) {
        Some(a) if a != DEFAULT_ACCOUNT && all.contains_key(&account_key(provider, Some(&a))) => {
            account_key(provider, Some(&a))
        }
        _ => provider.to_string(),
    }
}

/// Saved logins by provider: `default` for the unnamed one, then named ones.
pub fn accounts() -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let all = load_all();
    for k in all.keys() {
        if k.contains(':') || k == crate::runs::cloud::CREDENTIAL {
            continue;
        }
        let (provider, name) = k.split_once('@').unwrap_or((k, DEFAULT_ACCOUNT));
        out.entry(provider.to_string())
            .or_default()
            .push(name.to_string());
    }
    for names in out.values_mut() {
        names.sort_by_key(|n| (n != DEFAULT_ACCOUNT, n.clone()));
    }
    out
}

/// The account `provider` is using, when it has named ones: `work`,
/// `default`, or `default (no saved login "work")`.
pub fn account_label(provider: &str) -> Option<String> {
    let all = load_all();
    if !all
        .keys()
        .any(|k| k.strip_prefix(provider).is_some_and(|r| r.starts_with('@')))
    {
        return None;
    }
    let key = active_key(&all, provider);
    let used = key
        .split_once('@')
        .map(|(_, a)| a.to_string())
        .unwrap_or_else(|| DEFAULT_ACCOUNT.to_string());
    Some(match chosen_account(provider) {
        Some(c) if c != used => format!("{used} (no saved login \"{c}\")"),
        _ => used,
    })
}

/// Use `name`'s login for `provider` for the rest of this session.
pub fn switch_account(provider: &str, name: &str) -> Result<()> {
    if !load_all().contains_key(&account_key(provider, Some(name))) {
        let known = accounts().remove(provider).unwrap_or_default();
        bail!(
            "no saved login \"{name}\" for {provider} (saved: {}); add one with `chanto login {provider} --as {name}`",
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        );
    }
    SESSION_ACCOUNTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(provider.to_string(), name.to_string());
    Ok(())
}

/// The listing for `/login` and `chanto login --list`: providers with
/// saved logins and which one is in use. Never shows secrets.
pub fn accounts_table() -> String {
    let all = accounts();
    if all.is_empty() {
        return "No saved logins.\n".into();
    }
    let mut out = String::from("Saved logins (`*` = in use):\n");
    for (provider, names) in all {
        let used = account_label(&provider)
            .map(|l| l.split(' ').next().unwrap_or("").to_string())
            .unwrap_or_else(|| DEFAULT_ACCOUNT.into());
        let list: Vec<String> = names
            .iter()
            .map(|n| {
                if *n == used {
                    format!("*{n}")
                } else {
                    n.clone()
                }
            })
            .collect();
        out.push_str(&format!("  {provider:<14} {}\n", list.join(", ")));
    }
    out
}

pub fn get(provider: &str) -> Option<Credential> {
    let mut all = load_all();
    let key = active_key(&all, provider);
    all.remove(&key)
}

/// Save a login as `provider`'s default, or as a named account.
pub fn set_as(provider: &str, account: Option<&str>, c: Credential) -> Result<()> {
    set(&account_key(provider, account), c)
}

pub fn set(provider: &str, c: Credential) -> Result<()> {
    let mut all = load_all();
    all.insert(provider.to_string(), c);
    save_all(&all)
}

pub fn remove(provider: &str) -> Result<bool> {
    let mut all = load_all();
    let had = all.remove(provider).is_some();
    save_all(&all)?;
    Ok(had)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A stable per-installation ID (OpenAI's `ext_agent_host_id`).
fn device_id() -> Result<String> {
    let p = crate::config::home_dir().join("device-id");
    if let Ok(s) = std::fs::read_to_string(&p)
        && uuid::Uuid::parse_str(s.trim()).is_ok()
    {
        return Ok(s.trim().to_string());
    }
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(&p, &id)?;
    Ok(id)
}

// ---------------------------------------------------------------- ChatGPT

const OAI_AUTHORIZE: &str = "https://auth.openai.com/api/accounts/authorize";
const OAI_TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
const OAI_RESOURCE: &str = "https://api.openai.com/v1";
const OAI_DIRECT_SCOPE: &str = "chatgpt.tokens.use.direct";
const OAI_PORT: u16 = 1455;
const OAI_CALLBACK: &str = "/auth/callback";

fn oai_redirect() -> String {
    format!("http://127.0.0.1:{OAI_PORT}{OAI_CALLBACK}")
}

fn oauth_from(v: &Value, client_id: &str) -> Result<Credential> {
    let access = v["access_token"]
        .as_str()
        .context("token response has no access_token")?
        .to_string();
    let refresh = v["refresh_token"]
        .as_str()
        .context("token response has no refresh_token")?
        .to_string();
    let expires_in = v["expires_in"].as_u64().unwrap_or(3600);
    let scope = v["scope"].as_str().unwrap_or("");
    if !scope.split_whitespace().any(|s| s == OAI_DIRECT_SCOPE) {
        bail!("ChatGPT didn't grant {OAI_DIRECT_SCOPE}; your plan may not allow third-party apps");
    }
    // Refresh three minutes early so a request never starts with a token about to expire.
    Ok(Credential::Oauth {
        access,
        refresh,
        expires: now() + expires_in.saturating_sub(180),
        client_id: client_id.to_string(),
    })
}

async fn token_request(http: &reqwest::Client, form: &[(&str, &str)]) -> Result<Value> {
    let r = http
        .post(OAI_TOKEN)
        .header("accept", "application/json")
        .form(form)
        .send()
        .await?;
    let status = r.status();
    let body = r.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "OpenAI token request failed ({status}): {}",
            crate::util::truncate(&body, 300)
        );
    }
    Ok(serde_json::from_str(&body)?)
}

pub async fn login_chatgpt(
    account: Option<&str>,
    open: &(dyn Fn(&str) + Send + Sync),
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<()> {
    let http = crate::http::client();
    let host = format!("urn:uuid:{}", device_id()?);
    let (verifier, challenge) = crate::mcp_auth::pkce();
    let state = uuid::Uuid::new_v4().simple().to_string();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", OAI_PORT))
        .await
        .with_context(|| format!("port {OAI_PORT} is busy (another login, or the Codex CLI logging in); finish or cancel that first"))?;
    let scope = format!("openid profile email offline_access resource.invoke {OAI_DIRECT_SCOPE}");
    let redirect = oai_redirect();
    let mut url = reqwest::Url::parse(OAI_AUTHORIZE)?;
    url.query_pairs_mut()
        .append_pair("client_id", "dynamic_agent_client")
        .append_pair("agent_name_hint", "Chanto")
        .append_pair("ext_agent_host_id", &host)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect)
        .append_pair("resource", OAI_RESOURCE)
        .append_pair("scope", &scope)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("nonce", &nonce);
    say(&format!(
        "Opening your browser to sign in with ChatGPT. If it doesn't open, visit:\n{url}"
    ));
    open(url.as_str());
    let params = crate::mcp_auth::wait_for_params(listener, &state, OAI_CALLBACK).await?;
    let code = params
        .get("code")
        .context("no authorization code in the callback")?;
    // OpenAI issues this installation's client ID in the callback.
    let client_id = params
        .get("client_id")
        .context("the callback didn't include an issued client ID")?;
    let v = token_request(
        &http,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code),
            ("code_verifier", &verifier),
            ("redirect_uri", &redirect),
            ("resource", OAI_RESOURCE),
        ],
    )
    .await?;
    set_as("chatgpt", account, oauth_from(&v, client_id)?)?;
    Ok(())
}

/// Current bearer token for a login-backed provider, refreshing it when due.
pub async fn token(provider: &str) -> Result<String> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _g = LOCK.lock().await;
    let key = active_key(&load_all(), provider);
    match load_all().remove(&key) {
        Some(Credential::ApiKey { key }) => Ok(key),
        Some(Credential::Oauth {
            access, expires, ..
        }) if expires > now() => Ok(access),
        Some(Credential::Oauth {
            refresh, client_id, ..
        }) if provider == "chatgpt" => {
            let http = crate::http::client();
            let v = token_request(
                &http,
                &[("grant_type", "refresh_token"), ("client_id", &client_id), ("refresh_token", &refresh), ("resource", OAI_RESOURCE)],
            )
            .await
            .context("refreshing the ChatGPT sign-in (run `chanto login chatgpt` again if this keeps failing)")?;
            let c = oauth_from(&v, &client_id)?;
            let Credential::Oauth { access, .. } = &c else {
                unreachable!()
            };
            let access = access.clone();
            set(&key, c)?;
            Ok(access)
        }
        Some(_) => bail!("the {provider} login expired; run `chanto login {provider}`"),
        None => bail!("not logged in to {provider}; run `chanto login {provider}`"),
    }
}

// ---------------------------------------------------------------- OpenRouter

pub async fn login_openrouter(
    account: Option<&str>,
    open: &(dyn Fn(&str) + Send + Sync),
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let callback = format!("http://localhost:{port}/callback");
    let (verifier, challenge) = crate::mcp_auth::pkce();
    let mut url = reqwest::Url::parse("https://openrouter.ai/auth")?;
    url.query_pairs_mut()
        .append_pair("callback_url", &callback)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    say(&format!(
        "Opening your browser to connect OpenRouter. If it doesn't open, visit:\n{url}"
    ));
    open(url.as_str());
    let params = crate::mcp_auth::wait_for_params(listener, "", "/callback").await?;
    let code = params
        .get("code")
        .context("no code in OpenRouter's callback")?;
    let r = crate::http::client()
        .post("https://openrouter.ai/api/v1/auth/keys")
        .json(&serde_json::json!({"code": code, "code_verifier": verifier, "code_challenge_method": "S256"}))
        .send()
        .await?;
    let v: Value = r.json().await?;
    let key = v["key"]
        .as_str()
        .context("OpenRouter didn't return a key")?;
    set_as(
        "openrouter",
        account,
        Credential::ApiKey {
            key: key.to_string(),
        },
    )
}

// ---------------------------------------------------------------- CLI

pub fn read_secret(prompt: &str) -> Result<String> {
    use std::io::{IsTerminal, Write};
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    if !std::io::stdin().is_terminal() {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        return Ok(line.trim().to_string());
    }
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    crossterm::terminal::enable_raw_mode()?;
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
            eprintln!();
        }
    }
    let _restore = Restore;
    let mut key = String::new();
    loop {
        if let Event::Key(k) = event::read()? {
            if k.kind == KeyEventKind::Release {
                continue;
            }
            match k.code {
                KeyCode::Enter => return Ok(key.trim().to_string()),
                KeyCode::Esc => bail!("Key entry cancelled"),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    bail!("Key entry cancelled")
                }
                KeyCode::Backspace => {
                    if key.pop().is_some() {
                        eprint!("\x08 \x08");
                    }
                }
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                    key.push(c);
                    eprint!("*");
                }
                _ => {}
            }
            std::io::stderr().flush()?;
        }
    }
}

const VERTEX_LOGIN: &str = "Vertex AI uses Google Application Default Credentials: run `gcloud auth application-default login` (or set GOOGLE_APPLICATION_CREDENTIALS to a service account key) and set GOOGLE_CLOUD_PROJECT.";

/// A one-line status per catalog provider.
pub fn providers_table() -> String {
    let mut out = String::from("Providers (use as <provider>/<model>):\n");
    for h in crate::providers::CATALOG {
        let status = match crate::providers::credential(h) {
            _ if h.kind.is_cloud() => crate::providers::cloud_status(h),
            _ if h.id == crate::chanto_cloud::PROVIDER_ID => crate::chanto_cloud::provider_status(),
            Some(crate::providers::Cred::Login) => "signed in".to_string(),
            Some(crate::providers::Cred::Cloud(source)) => source,
            Some(crate::providers::Cred::Key(_)) if get(h.id).is_some() => "key saved".to_string(),
            Some(crate::providers::Cred::Key(_)) => "key from environment".to_string(),
            None if h.browser_login => format!("chanto login {}", h.id),
            None => format!(
                "set {} or `chanto login {}`",
                h.env.first().copied().unwrap_or("a key"),
                h.id
            ),
        };
        out.push_str(&format!("  {:<14} {:<32} {status}\n", h.id, h.name));
    }
    out
}

/// `chanto login [provider]` / `chanto logout <provider>` / `chanto providers`.
pub async fn cli(args: &[String]) -> Result<String> {
    let say = |m: &str| eprintln!("{m}");
    if args
        .get(1)
        .is_some_and(|a| matches!(a.as_str(), "--help" | "-h" | "help"))
    {
        return Ok(match args.first().map(String::as_str) {
            Some("logout") => "Usage: chanto logout <provider> [--as <name>]\n\nRemove credentials Chanto saved for a provider (`--as`: a named account). `chanto providers` lists them.".into(),
            _ => "Usage: chanto login <provider> [--as <name>] [--with-api-key]\n\nLog in to a provider: in the browser where it supports that, otherwise by pasting an API key (`--with-api-key` reads it from stdin). `--as <name>` saves it as a separate account (e.g. work and personal); pick one with `account = \"<name>\"` in the config or `/login <provider> <name>` in a session. `chanto providers` lists the providers and saved logins.".into(),
        });
    }
    // `--as <name>` / `--as=<name>`: a named account.
    let account = args
        .iter()
        .enumerate()
        .find_map(|(i, a)| match a.strip_prefix("--as=") {
            Some(v) => Some(v.to_string()),
            None if a == "--as" => Some(args.get(i + 1).cloned().unwrap_or_default()),
            None => None,
        });
    if let Some(a) = &account
        && (!valid_account(a) || a == DEFAULT_ACCOUNT)
    {
        bail!(
            "account names are letters, digits, `.`, `_` and `-` (and not \"{DEFAULT_ACCOUNT}\"): got {a:?}"
        );
    }
    let account = account.as_deref();
    let args: Vec<String> = {
        let mut skip = false;
        args.iter()
            .filter(|a| {
                if skip {
                    skip = false;
                    return false;
                }
                if *a == "--as" {
                    skip = true;
                    return false;
                }
                !a.starts_with("--as=")
            })
            .cloned()
            .collect()
    };
    let as_note = account
        .map(|a| format!(" as account \"{a}\""))
        .unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("providers") => {
            if let Some(extra) = args.get(1) {
                anyhow::bail!("unknown argument `{extra}`; usage: chanto providers");
            }
            Ok(format!("{}\n{}", providers_table(), accounts_table()))
        }
        Some("logout") => {
            let id = args
                .get(1)
                .context("usage: chanto logout <provider> [--as <name>]")?;
            if remove(&account_key(id, account))? {
                return Ok(format!("Logged out of {id}{as_note}."));
            }
            if crate::providers::find(id).is_none() {
                anyhow::bail!("unknown provider {id}; see `chanto providers`");
            }
            Ok(format!("Wasn't logged in to {id}{as_note}."))
        }
        Some("login") => {
            let Some(id) = args.get(1) else {
                return Ok(format!(
                    "{}\n{}\nUsage: chanto login <provider> [--as <name>]",
                    providers_table(),
                    accounts_table()
                ));
            };
            let h = crate::providers::find(id)
                .with_context(|| format!("unknown provider {id}; see `chanto providers`"))?;
            if args.iter().any(|a| a == "--with-api-key") {
                use std::io::Read;
                let mut key = String::new();
                std::io::stdin().read_to_string(&mut key)?;
                let key = key.trim().to_string();
                if key.is_empty() {
                    bail!("no API key on stdin");
                }
                if h.id == "chatgpt" {
                    bail!("ChatGPT requires browser login; use chanto login chatgpt");
                }
                if h.id == "vertex" {
                    bail!("{}", VERTEX_LOGIN);
                }
                set_as(h.id, account, Credential::ApiKey { key })?;
                return Ok(format!("Saved key for {}{as_note}.", h.id));
            }
            match h.id {
                "vertex" => bail!("{}", VERTEX_LOGIN),
                "chatgpt" => {
                    login_chatgpt(account, &|u| crate::mcp_auth::open_browser(u), &say).await?
                }
                "openrouter" if !args.iter().any(|a| a == "--key") => {
                    login_openrouter(account, &|u| crate::mcp_auth::open_browser(u), &say).await?
                }
                _ => {
                    if let Some(page) = crate::providers::key_page(h.id) {
                        say(page);
                    }
                    let key = read_secret(&format!("{} API key: ", h.name))?;
                    if key.is_empty() {
                        bail!("no key entered");
                    }
                    set_as(h.id, account, Credential::ApiKey { key })?;
                }
            }
            if let Some(a) = account {
                return Ok(format!(
                    "Logged in to {}{as_note}. Use it with `account = \"{a}\"` in the config, or `/login {} {a}` in a session.",
                    h.name, h.id
                ));
            }
            Ok(format!(
                "Logged in to {}. Use it as `{}/<model>` (for example `chanto --model {}/<model>`), or list models with /models.",
                h.name, h.id, h.id
            ))
        }
        _ => Ok(providers_table()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_round_trip_with_private_permissions() {
        let _g = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let d = std::env::temp_dir().join(format!("chanto-auth-{}", std::process::id()));
        // SAFETY: test-only, serialized by TEST_HOME_LOCK.
        let previous = std::env::var_os("CHANTO_HOME");
        unsafe { std::env::set_var("CHANTO_HOME", &d) };
        set(
            "groq",
            Credential::ApiKey {
                key: "gsk_test".into(),
            },
        )
        .unwrap();
        assert!(matches!(get("groq"), Some(Credential::ApiKey { key }) if key == "gsk_test"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path()).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(remove("groq").unwrap() && get("groq").is_none());
        let id = device_id().unwrap();
        assert_eq!(id, device_id().unwrap(), "device id is stable");
        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("CHANTO_HOME", previous);
            } else {
                std::env::remove_var("CHANTO_HOME");
            }
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn keychain_keeps_secrets_out_of_the_file() {
        let _g = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let d = std::env::temp_dir().join(format!("chanto-keychain-{}", std::process::id()));
        let prev_home = std::env::var_os("CHANTO_HOME");
        let prev_kc = std::env::var_os("CHANTO_KEYCHAIN");
        // SAFETY: test-only, serialized by TEST_HOME_LOCK.
        unsafe {
            std::env::set_var("CHANTO_HOME", &d);
            std::env::set_var("CHANTO_KEYCHAIN", "1");
        }
        keychain::reset();
        set(
            "groq",
            Credential::ApiKey {
                key: "gsk_secret".into(),
            },
        )
        .unwrap();
        let raw = std::fs::read_to_string(path()).unwrap();
        assert!(
            !raw.contains("gsk_secret"),
            "the secret must not be in the file: {raw}"
        );
        assert!(raw.contains("keychain"), "a placeholder is written: {raw}");
        assert!(matches!(get("groq"), Some(Credential::ApiKey { key }) if key == "gsk_secret"));
        assert!(remove("groq").unwrap() && get("groq").is_none());
        // SAFETY: test-only, serialized by TEST_HOME_LOCK.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("CHANTO_HOME", v),
                None => std::env::remove_var("CHANTO_HOME"),
            }
            match prev_kc {
                Some(v) => std::env::set_var("CHANTO_KEYCHAIN", v),
                None => std::env::remove_var("CHANTO_KEYCHAIN"),
            }
        }
        keychain::reset();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn named_accounts_sit_beside_the_default_login() {
        let _g = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let d =
            std::env::temp_dir().join(format!("chanto-accounts-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        // An auth.json from before accounts existed.
        std::fs::write(d.join("auth.json"), r#"{"groqtest": {"type": "api_key", "key": "gsk_personal"}, "remote:box": {"type": "remote", "url": "https://x", "token": "t"}}"#).unwrap();
        let previous = std::env::var_os("CHANTO_HOME");
        // SAFETY: test-only, serialized by TEST_HOME_LOCK.
        unsafe { std::env::set_var("CHANTO_HOME", &d) };
        assert!(
            matches!(get("groqtest"), Some(Credential::ApiKey { key }) if key == "gsk_personal")
        );
        assert_eq!(
            account_label("groqtest"),
            None,
            "no named accounts: nothing to show"
        );
        assert!(
            cli(&[
                "login".into(),
                "groqtest".into(),
                "--as".into(),
                "bad name".into()
            ])
            .await
            .is_err()
        );
        set_as(
            "groqtest",
            Some("work"),
            Credential::ApiKey {
                key: "gsk_work".into(),
            },
        )
        .unwrap();
        // The file keeps the old shape: the default login is still a plain entry.
        let raw: Value =
            serde_json::from_slice(&std::fs::read(d.join("auth.json")).unwrap()).unwrap();
        assert_eq!(raw["groqtest"]["key"], "gsk_personal");
        assert_eq!(raw["groqtest@work"]["key"], "gsk_work");
        assert_eq!(
            accounts().get("groqtest").unwrap(),
            &vec!["default".to_string(), "work".to_string()]
        );
        assert!(!accounts().contains_key("remote:box"));
        assert_eq!(account_label("groqtest").as_deref(), Some("default"));
        assert!(
            switch_account("groqtest", "nope")
                .unwrap_err()
                .to_string()
                .contains("default, work")
        );
        switch_account("groqtest", "work").unwrap();
        assert!(matches!(get("groqtest"), Some(Credential::ApiKey { key }) if key == "gsk_work"));
        assert_eq!(token("groqtest").await.unwrap(), "gsk_work");
        assert_eq!(account_label("groqtest").as_deref(), Some("work"));
        let table = accounts_table();
        assert!(
            table.contains("*work") && !table.contains("gsk_"),
            "{table}"
        );
        switch_account("groqtest", DEFAULT_ACCOUNT).unwrap();
        assert!(
            matches!(get("groqtest"), Some(Credential::ApiKey { key }) if key == "gsk_personal")
        );
        let msg = cli(&["logout".into(), "groqtest".into(), "--as=work".into()])
            .await
            .unwrap();
        assert!(msg.contains("as account \"work\""), "{msg}");
        assert!(get("groqtest").is_some() && !accounts()["groqtest"].contains(&"work".to_string()));
        SESSION_ACCOUNTS.lock().unwrap().remove("groqtest");
        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("CHANTO_HOME", previous);
            } else {
                std::env::remove_var("CHANTO_HOME");
            }
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
