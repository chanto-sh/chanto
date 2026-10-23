//! Chanto cloud from the terminal: sign in (`chanto cloud login`), queue a
//! run of the current repository (`chanto run --cloud "<task>"`), follow it
//! until its branch comes back, and list, show or cancel runs
//! (`chanto runs --cloud`, `chanto attach <id> --cloud`).
//!
//! A runner clones the repository from its git remote, so a cloud run starts
//! from what is pushed: the current branch as it is on `origin`. Uncommitted
//! or unpushed work is refused rather than silently left out (`--push` pushes
//! a clean branch first).
//!
//! The account token comes from the server's browser sign-in (or GitHub's
//! device flow when that's all it offers) and is kept in `~/.chanto/auth.json`
//! (0600) as the `cloud` entry; `CHANTO_CLOUD_TOKEN` (with `CHANTO_CLOUD_URL`
//! unless it's the saved server or [`DEFAULT_URL`]) takes its place (CI). The
//! default org is in `~/.chanto/cloud.json`. Agents and the model gateway
//! use the same sign-in (`crate::chanto_cloud`).
//!
//! Server routes used (`chanto_proto` and `chanto_proto::runs`):
//! `GET /v1/auth/providers`, `POST /v1/auth/browser/{start,poll}`,
//! `POST /v1/auth/device/{start,poll}`, `DELETE /v1/auth/token`, `GET /v1/me`,
//! `GET /v1/orgs`, `GET|POST /v1/orgs/{org}/runs`, `GET /v1/runs/{id}`,
//! `GET /v1/runs/{id}/events?after=<seq>`, `POST /v1/runs/{id}/cancel`.

use super::fmt_duration;
use crate::ui;
use anyhow::{Context, Result, bail};
use chanto_proto::runs::{CreateRun, Placement, Run, RunEventList, RunList, RunStatus, RunnerKind};
use chanto_proto::{
    ApiError, AuthProviders, BrowserLoginPoll, BrowserStart, BrowserStarted, DevicePoll,
    DevicePollResult, DeviceStart, ErrorCode, User,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The `auth.json` entry holding the sign-in.
pub const CREDENTIAL: &str = "cloud";

/// The server when neither `--url`, `CHANTO_CLOUD_URL` nor a saved sign-in names one.
pub const DEFAULT_URL: &str = api_url!();

/// How to get started, for people who aren't signed in.
pub const SIGN_UP: &str = concat!(
    "Sign in with GitHub or Google: `chanto cloud login` (in a session, `/login chanto`). \
New accounts get a free trial credit; ",
    site_url!(),
    "/pricing has the plans."
);

pub const USAGE: &str = concat!("usage: chanto cloud login [--url <URL>] [--provider github|google|gitlab|bitbucket] [--no-open]
       chanto cloud status                  who you are, your orgs and the default one
       chanto cloud org <slug>              make <slug> the default org
       chanto cloud logout                  revoke the token and forget it
       chanto cloud models                  models on the model gateway (use as chanto/<model>), with prices
       chanto cloud run --agent <name> [--input <text>] [--ref <branch>] [--wait] [--json]
                                             start an agent's run (`chanto agents --help` for more)
       chanto run --cloud \"<task>\" [--org <slug>] [--label <l>]... [--placement auto|hosted|self-hosted] [--push] [--detach]
       chanto runs --cloud [--org <slug>] [--status <s>]
       chanto runs show|cancel <id> --cloud
       chanto attach <id> --cloud
Sign in with `chanto cloud login`; plans at ", site_url!(), "/pricing.");

/// Requests to the account side of Chanto cloud.
#[derive(Clone)]
pub struct Cloud {
    pub url: String,
    token: String,
    http: reqwest::Client,
}

/// An org, as `GET /v1/orgs` lists it. Only what the CLI shows, all lenient,
/// so a newer server's additions don't break it.
#[derive(Debug, Clone, Deserialize)]
pub struct Org {
    pub slug: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub personal: bool,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub hosted_runners: bool,
    #[serde(default)]
    pub suspended: Option<String>,
}

#[derive(Deserialize)]
struct OrgList {
    orgs: Vec<Org>,
}

impl Cloud {
    pub fn new(url: &str, token: &str) -> Cloud {
        let http = crate::http::builder()
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("chanto/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        Cloud {
            url: url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http,
        }
    }

    /// `CHANTO_CLOUD_TOKEN` (for `CHANTO_CLOUD_URL`, else the saved
    /// server, else [`DEFAULT_URL`]), or the saved sign-in.
    pub fn signed_in() -> Result<Cloud> {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let saved = match crate::auth::get(CREDENTIAL) {
            Some(crate::auth::Credential::Remote { url, token }) => Some((url, token)),
            _ => None,
        };
        if let Some(token) = env("CHANTO_CLOUD_TOKEN") {
            let url = env("CHANTO_CLOUD_URL")
                .or(saved.map(|(u, _)| u))
                .unwrap_or_else(|| DEFAULT_URL.into());
            return Ok(Cloud::new(&url, &token));
        }
        match saved {
            Some((url, token)) => Ok(Cloud::new(&url, &token)),
            None => bail!(
                "You're not signed in to Chanto cloud; run `chanto cloud login` (or set CHANTO_CLOUD_TOKEN).\n{SIGN_UP}"
            ),
        }
    }

    /// The account token, for the model gateway.
    pub fn token(&self) -> &str {
        &self.token
    }

    async fn send<T: DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<T> {
        let req = if self.token.is_empty() {
            req
        } else {
            req.bearer_auth(&self.token)
        };
        let resp = req
            .send()
            .await
            .with_context(|| format!("contacting Chanto cloud at {}", self.url))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.bytes().await.unwrap_or_default();
            return Err(api_error(status, &body, &self.url, what));
        }
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok(serde_json::from_value(Value::Null)?);
        }
        let body = resp
            .bytes()
            .await
            .with_context(|| format!("reading Chanto cloud's answer to {what}"))?;
        serde_json::from_slice(&body)
            .with_context(|| format!("unexpected answer from Chanto cloud to {what}"))
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.send(
            self.http.get(format!("{}{path}", self.url)),
            &format!("GET {path}"),
        )
        .await
    }

    pub async fn post<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        self.send(
            self.http.post(format!("{}{path}", self.url)).json(body),
            &format!("POST {path}"),
        )
        .await
    }

    pub async fn put<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        self.send(
            self.http.put(format!("{}{path}", self.url)).json(body),
            &format!("PUT {path}"),
        )
        .await
    }

    pub async fn delete(&self, path: &str) -> Result<Value> {
        self.send(
            self.http.delete(format!("{}{path}", self.url)),
            &format!("DELETE {path}"),
        )
        .await
    }

    /// The caller's orgs, their personal one first.
    pub async fn orgs(&self) -> Result<Vec<Org>> {
        Ok(self.get::<OrgList>("/v1/orgs").await?.orgs)
    }

    /// Queues a run in `org`.
    pub async fn submit(&self, org: &str, req: &CreateRun) -> Result<Run> {
        self.post(&format!("/v1/orgs/{}/runs", urlencoding::encode(org)), req)
            .await
    }

    pub async fn run(&self, id: &str) -> Result<Run> {
        self.get(&format!("/v1/runs/{}", urlencoding::encode(id)))
            .await
    }
}

/// The server's error message, verbatim, plus what to do about it.
fn api_error(status: reqwest::StatusCode, body: &[u8], url: &str, what: &str) -> anyhow::Error {
    let Ok(e) = serde_json::from_slice::<ApiError>(body) else {
        return match status.as_u16() {
            404 | 405 => anyhow::anyhow!(
                "{url} doesn't look like a Chanto cloud server ({what}: {status}); check the URL"
            ),
            _ => anyhow::anyhow!("Chanto cloud: {what} failed ({status})"),
        };
    };
    let hint = match e.error {
        ErrorCode::Unauthorized => {
            "\nThe saved sign-in was rejected (revoked or expired); run `chanto cloud login` again."
        }
        ErrorCode::QuotaExceeded => {
            "\nSee the org's Usage & billing page in the Chanto cloud dashboard, or keep the run on your own runners with `--placement self-hosted`."
        }
        ErrorCode::NotConfigured => {
            "\nThis Chanto cloud server doesn't offer that; ask its operator."
        }
        ErrorCode::NotFound if what.contains("/v1/orgs/") => {
            "\nCheck the org with `chanto cloud status`."
        }
        _ => "",
    };
    anyhow::anyhow!("Chanto cloud: {}{hint}", e.message)
}

fn check_url(url: &str) -> Result<()> {
    // Judge the host the request will go to, as the URL parser reads it:
    // `http://localhost.evil.com` and `http://localhost@evil.com` start like
    // a local address but aren't one.
    let local = reqwest::Url::parse(url).is_ok_and(|u| {
        u.scheme() == "http" && matches!(u.host_str(), Some("localhost" | "127.0.0.1"))
    });
    if !url.starts_with("https://") && !local {
        bail!("{url} must use https:// (plain http only for localhost)");
    }
    Ok(())
}

// ------------------------------------------------------------------ sign-in

/// Signs in to the server at `url` through its browser sign-in (or GitHub's
/// device flow when that's all it offers) and returns the new token and its
/// user. `provider` picks the sign-in provider; `open` opens the browser.
pub async fn sign_in(url: &str, provider: Option<&str>, open: bool) -> Result<(String, User)> {
    sign_in_with(url, provider, open, false).await
}

/// [`sign_in`]; `quiet` (inside the full-screen app) prints nothing and asks
/// nothing: GitHub unless `provider` says otherwise, and no device flow,
/// whose code would have nowhere to show.
async fn sign_in_with(
    url: &str,
    provider: Option<&str>,
    open: bool,
    quiet: bool,
) -> Result<(String, User)> {
    let c = Cloud::new(url, "");
    let offered: AuthProviders = c.get("/v1/auth/providers").await?;
    let names = || {
        offered
            .providers
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let p = match provider {
        Some(id) => offered
            .providers
            .iter()
            .find(|p| p.id == id)
            .with_context(|| {
                format!(
                    "{url} doesn't offer sign-in with {id}; it offers: {}",
                    names()
                )
            })?,
        None => {
            let browser: Vec<&chanto_proto::AuthProvider> =
                offered.providers.iter().filter(|p| p.browser).collect();
            let pick = if quiet {
                browser.iter().position(|p| p.id == "github")
            } else {
                pick_provider(&browser)?
            };
            match pick {
                Some(i) => browser[i],
                None => browser
                    .first()
                    .copied()
                    .or(offered.providers.first())
                    .with_context(|| format!("{url} has no sign-in providers set up"))?,
            }
        }
    };
    let client_name = Some(format!(
        "chanto CLI on {}",
        sysinfo::System::host_name().unwrap_or_else(|| "this machine".into())
    ));
    if p.browser {
        let started: BrowserStarted = c
            .post(
                "/v1/auth/browser/start",
                &BrowserStart {
                    provider: p.id.clone(),
                    client_name,
                    return_to: None,
                },
            )
            .await?;
        if !quiet {
            eprintln!(
                "Sign in with {} in your browser:\n  {}",
                p.name, started.authorize_url
            );
            // Older servers send no code and have no confirmation step.
            if !started.user_code.is_empty() {
                eprintln!(
                    "\nWhen the browser asks, enter this code to confirm it's you:\n  {}",
                    started.user_code
                );
            }
        }
        if open {
            crate::mcp_auth::open_browser(&started.authorize_url);
        }
        let poll = BrowserLoginPoll {
            poll_code: started.poll_code,
        };
        wait_for_token(started.expires_in, 2, async || {
            c.post::<DevicePollResult>("/v1/auth/browser/poll", &poll)
                .await
        })
        .await
    } else if p.device && quiet {
        bail!(
            "signing in with {} shows a code; run `chanto cloud login` in a terminal",
            p.name
        )
    } else if p.device {
        let d: DeviceStart = c.post("/v1/auth/device/start", &json!({})).await?;
        eprintln!(
            "Open {} and enter the code {}",
            d.verification_uri, d.user_code
        );
        if open {
            crate::mcp_auth::open_browser(&d.verification_uri);
        }
        let poll = DevicePoll {
            device_code: d.device_code,
            client_name,
        };
        wait_for_token(d.expires_in, d.interval, async || {
            c.post::<DevicePollResult>("/v1/auth/device/poll", &poll)
                .await
        })
        .await
    } else {
        bail!(
            "{url} offers no sign-in this CLI can use (providers: {})",
            names()
        )
    }
}

/// With several browser sign-ins and a terminal, asks which one (as `chanto
/// setup` does); `None`: no need to ask, or nobody to ask (the first is used).
fn pick_provider(browser: &[&chanto_proto::AuthProvider]) -> Result<Option<usize>> {
    use std::io::IsTerminal;
    if browser.len() < 2
        || !std::io::stdin().is_terminal()
        || !std::io::stdout().is_terminal()
        || cfg!(test)
    {
        return Ok(None);
    }
    let labels: Vec<String> = browser.iter().map(|p| p.name.clone()).collect();
    crate::onboarding::choose("Sign in to Chanto cloud with", &labels, false).map(Some)
}

/// Polls a sign-in until it completes, fails or expires.
async fn wait_for_token(
    expires_in: u64,
    interval: u64,
    mut poll: impl AsyncFnMut() -> Result<DevicePollResult>,
) -> Result<(String, User)> {
    let deadline = Instant::now() + Duration::from_secs(expires_in.max(1));
    eprintln!(
        "{}",
        ui::dim("Waiting for the sign-in to complete (Ctrl-C cancels)…")
    );
    loop {
        // The server may ask for longer waits (GitHub's slow_down), never shorter ones.
        let wait = match poll().await? {
            DevicePollResult::Complete { token, user, .. } => return Ok((token, user)),
            DevicePollResult::Pending { interval: next } => next.max(interval),
        };
        if Instant::now() >= deadline {
            bail!("the sign-in expired before it was completed; run `chanto cloud login` again");
        }
        tokio::time::sleep(Duration::from_secs(wait.clamp(1, 30))).await;
    }
}

fn save_login(url: &str, token: &str) -> Result<()> {
    crate::auth::set(
        CREDENTIAL,
        crate::auth::Credential::Remote {
            url: url.to_string(),
            token: token.to_string(),
        },
    )
}

fn settings_path() -> PathBuf {
    crate::config::home_dir().join("cloud.json")
}

/// The default org: `CHANTO_CLOUD_ORG`, else the one saved with `chanto cloud org`.
pub fn default_org() -> Option<String> {
    if let Some(o) = std::env::var("CHANTO_CLOUD_ORG")
        .ok()
        .filter(|o| !o.is_empty())
    {
        return Some(o);
    }
    let v: Value = serde_json::from_slice(&std::fs::read(settings_path()).ok()?).ok()?;
    v["org"]
        .as_str()
        .filter(|o| !o.is_empty())
        .map(str::to_string)
}

fn set_default_org(slug: Option<&str>) -> Result<()> {
    let p = settings_path();
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(&p, serde_json::to_vec_pretty(&json!({"org": slug}))?)?;
    Ok(())
}

/// `--org`, else the default org, else the caller's first (personal) org.
pub async fn resolve_org(c: &Cloud, flag: Option<&str>) -> Result<String> {
    if let Some(o) = flag.map(str::to_string).or_else(default_org) {
        return Ok(o);
    }
    c.orgs()
        .await?
        .into_iter()
        .next()
        .map(|o| o.slug)
        .context("your Chanto cloud account has no org yet; create one in the dashboard")
}

fn org_line(o: &Org, default: bool) -> String {
    let mut s = format!(
        "{} {:<20} {} · {} plan · hosted runners {}",
        if default { "*" } else { " " },
        if o.name.is_empty() || o.name == o.slug {
            o.slug.clone()
        } else {
            format!("{} ({})", o.slug, o.name)
        },
        o.role.as_deref().unwrap_or("member"),
        o.plan.as_deref().unwrap_or("?"),
        if o.hosted_runners { "on" } else { "off" }
    );
    if o.personal {
        s.push_str(" · personal");
    }
    if let Some(why) = &o.suspended {
        s.push_str(&format!(" · SUSPENDED: {why}"));
    }
    s
}

/// `chanto cloud login|logout|status|org`.
pub async fn cloud_cmd(args: &[String]) -> Result<String> {
    match args.first().map(String::as_str).unwrap_or("status") {
        "login" => login_cmd(&args[1..]).await,
        "logout" => logout_cmd().await,
        "status" | "whoami" => status_cmd().await,
        "org" => {
            let c = Cloud::signed_in()?;
            let orgs = c.orgs().await?;
            match args.get(1).map(String::as_str) {
                None => {
                    let current = default_org().or_else(|| orgs.first().map(|o| o.slug.clone()));
                    Ok(orgs
                        .iter()
                        .map(|o| org_line(o, current.as_deref() == Some(o.slug.as_str())))
                        .collect::<Vec<_>>()
                        .join("\n"))
                }
                Some(slug) => {
                    if !orgs.iter().any(|o| o.slug == slug) {
                        bail!(
                            "you're not in an org named {slug}; yours: {}",
                            orgs.iter()
                                .map(|o| o.slug.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                    set_default_org(Some(slug))?;
                    Ok(format!("Cloud runs go to {slug} unless you pass --org."))
                }
            }
        }
        "models" => crate::chanto_cloud::models_cmd(&args[1..]).await,
        "help" | "-h" | "--help" => Ok(USAGE.into()),
        other => bail!("unknown subcommand `{other}`\n{USAGE}"),
    }
}

async fn login_cmd(args: &[String]) -> Result<String> {
    let (mut url, mut provider, mut open) = (None, None, true);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .cloned()
                .with_context(|| format!("{flag} needs a value"))
        };
        match a.as_str() {
            "--url" => url = Some(value(a)?),
            "--provider" => provider = Some(value(a)?),
            "--no-open" => open = false,
            s if s.starts_with("--url=") => url = Some(s["--url=".len()..].to_string()),
            s if s.starts_with("--provider=") => {
                provider = Some(s["--provider=".len()..].to_string())
            }
            "-h" | "--help" => return Ok(USAGE.into()),
            s => bail!("unexpected argument {s}\n{USAGE}"),
        }
    }
    let saved = match crate::auth::get(CREDENTIAL) {
        Some(crate::auth::Credential::Remote { url, .. }) => Some(url),
        _ => None,
    };
    let url = url
        .or_else(|| {
            std::env::var("CHANTO_CLOUD_URL")
                .ok()
                .filter(|u| !u.is_empty())
        })
        .or(saved)
        .unwrap_or_else(|| DEFAULT_URL.into());
    let url = url.trim_end_matches('/').to_string();
    check_url(&url)?;
    let (token, user) = sign_in(&url, provider.as_deref(), open).await?;
    save_login(&url, &token)?;
    let mut s = format!("Signed in to Chanto cloud ({url}) as {}.", user.login);
    if let Ok(orgs) = Cloud::new(&url, &token).orgs().await {
        let current = default_org()
            .filter(|d| orgs.iter().any(|o| &o.slug == d))
            .or_else(|| orgs.first().map(|o| o.slug.clone()));
        if !orgs.is_empty() {
            s.push_str("\nOrgs (* = where runs go; `chanto cloud org <slug>` changes it):");
            for o in &orgs {
                s.push_str(&format!(
                    "\n  {}",
                    org_line(o, current.as_deref() == Some(o.slug.as_str()))
                ));
            }
        }
    }
    s.push_str("\nQueue a run of this repository with `chanto run --cloud \"<task>\"`.");
    Ok(s)
}

/// `/login chanto [provider]` inside a session: signs in to the saved
/// server (else [`DEFAULT_URL`]) through the browser, quietly.
pub async fn login_in_session(provider: Option<String>) -> Result<String> {
    // The sign-in's future isn't `Send` (its polling closure), and a session
    // runs commands on spawned tasks: give it a thread of its own.
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || handle.block_on(login_quietly(provider.as_deref()))).await?
}

async fn login_quietly(provider: Option<&str>) -> Result<String> {
    let url = std::env::var("CHANTO_CLOUD_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .or(match crate::auth::get(CREDENTIAL) {
            Some(crate::auth::Credential::Remote { url, .. }) => Some(url),
            _ => None,
        })
        .unwrap_or_else(|| DEFAULT_URL.into());
    let url = url.trim_end_matches('/').to_string();
    check_url(&url)?;
    let (token, user) = sign_in_with(&url, provider, true, true).await?;
    save_login(&url, &token)?;
    Ok(format!(
        "Signed in to Chanto cloud as {}. Its models are `chanto/<model>`: see /models, or `/model chanto/deepseek-v4.1-flash`.",
        user.login
    ))
}

async fn logout_cmd() -> Result<String> {
    let Some(crate::auth::Credential::Remote { url, token }) = crate::auth::get(CREDENTIAL) else {
        return Ok("You're not signed in to Chanto cloud.".into());
    };
    let revoked = Cloud::new(&url, &token).delete("/v1/auth/token").await;
    crate::auth::remove(CREDENTIAL)?;
    Ok(match revoked {
        Ok(_) => format!("Signed out of Chanto cloud ({url}); the token is revoked."),
        Err(e) => format!(
            "Signed out of Chanto cloud on this machine. The server didn't revoke the token: {e:#}"
        ),
    })
}

async fn status_cmd() -> Result<String> {
    let Ok(c) = Cloud::signed_in() else {
        return Ok(format!("Not signed in to Chanto cloud.\n{SIGN_UP}"));
    };
    let me: User = c.get("/v1/me").await?;
    let orgs = c.orgs().await?;
    let current = default_org().or_else(|| orgs.first().map(|o| o.slug.clone()));
    let mut s = format!(
        "Chanto cloud: {}\nSigned in as {}{}",
        c.url,
        me.login,
        me.name.map(|n| format!(" ({n})")).unwrap_or_default()
    );
    if orgs.is_empty() {
        s.push_str("\nNo orgs yet.");
    } else {
        s.push_str("\nOrgs (* = where runs go; `chanto cloud org <slug>` changes it, `--org` picks one per command):");
        for o in &orgs {
            s.push_str(&format!(
                "\n  {}",
                org_line(o, current.as_deref() == Some(o.slug.as_str()))
            ));
        }
        if let Some(d) = current
            .as_deref()
            .filter(|d| !orgs.iter().any(|o| o.slug == *d))
        {
            s.push_str(&format!(
                "\nThe default org {d} isn't one of yours; runs will fail until you pick another."
            ));
        }
    }
    Ok(s)
}

// ------------------------------------------------------------------ the repository

/// Where a cloud run starts: the current branch as it is on `origin`.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    /// The URL the runner clones (HTTPS where the remote was SSH).
    pub repo: String,
    pub branch: String,
}

/// The HTTPS URL for a git remote: `git@github.com:o/r.git` and
/// `ssh://git@host/o/r` become `https://github.com/o/r.git` and
/// `https://host/o/r.git`, and credentials in an HTTPS URL are dropped.
/// Anything else is returned as is.
pub fn https_url(remote: &str) -> String {
    let r = remote.trim();
    if let Some((scheme, rest)) = r.split_once("://") {
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        return match scheme {
            "https" | "http" => format!("{scheme}://{host}/{path}"),
            "ssh" | "git+ssh" => {
                // The SSH port means nothing to HTTPS.
                let host = host.split_once(':').map_or(host, |(h, _)| h);
                with_git_suffix(&format!("https://{host}/{}", path.trim_start_matches('/')))
            }
            _ => r.to_string(),
        };
    }
    // scp-like: [user@]host:path (a path with a slash before the colon is local).
    match r.split_once(':') {
        Some((authority, path))
            if !authority.contains('/') && !authority.is_empty() && authority.len() > 1 =>
        {
            let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            with_git_suffix(&format!("https://{host}/{}", path.trim_start_matches('/')))
        }
        _ => r.to_string(),
    }
}

fn with_git_suffix(url: &str) -> String {
    if url.ends_with(".git") {
        url.to_string()
    } else {
        format!("{url}.git")
    }
}

/// The repository and branch a cloud run of `cwd` starts from. Refuses
/// uncommitted changes and commits that aren't on `origin`, which the run
/// wouldn't see; with `push`, pushes the (clean) branch first.
pub fn local_source(cwd: &Path, push: bool) -> Result<Source> {
    let repo = super::git::toplevel(cwd)?;
    let git = |args: &[&str]| super::git::git(&repo, args);
    let remote = git(&["remote", "get-url", "origin"]).ok().filter(|u| !u.is_empty()).context(
        "this repository has no `origin` remote, and Chanto cloud clones the repository from there. \
         Add one (`git remote add origin <url>`) and push your branch.",
    )?;
    let branch = git(&["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .filter(|b| !b.is_empty())
        .context("HEAD is detached; check out the branch the cloud run should start from")?;
    let dirty = git(&["status", "--porcelain"])?;
    if !dirty.trim().is_empty() {
        bail!(
            "you have uncommitted changes ({} file(s)). A cloud run starts from {branch} as it is on origin, so it wouldn't see them: \
             commit and push them, or stash them, first.",
            dirty.lines().count()
        );
    }
    let tracking = format!("refs/remotes/origin/{branch}");
    let unpushed = if git(&["rev-parse", "--verify", "-q", &tracking]).is_ok_and(|s| !s.is_empty())
    {
        let ahead: usize = git(&["rev-list", "--count", &format!("{tracking}..HEAD")])?
            .trim()
            .parse()
            .unwrap_or(0);
        (ahead > 0).then(|| format!("{ahead} commit(s) on {branch} aren't on origin"))
    } else {
        Some(format!("{branch} isn't on origin yet"))
    };
    if let Some(why) = unpushed {
        if !push {
            bail!(
                "{why}, and a cloud run starts from origin. Push first (`git push -u origin {branch}`), or pass --push to have Chanto push it."
            );
        }
        eprintln!("{}", ui::dim(&format!("{why}; pushing it…")));
        git(&[
            "push",
            "-q",
            "-u",
            "origin",
            &format!("HEAD:refs/heads/{branch}"),
        ])
        .context("pushing the branch to origin")?;
    }
    Ok(Source {
        repo: https_url(&remote),
        branch,
    })
}

// ------------------------------------------------------------------ runs

#[derive(Debug)]
struct RunOpts {
    task: String,
    org: Option<String>,
    labels: Vec<String>,
    placement: Option<Placement>,
    push: bool,
    detach: bool,
    cwd: Option<PathBuf>,
}

fn parse_placement(s: &str) -> Result<Placement> {
    match s {
        "auto" => Ok(Placement::Auto),
        "hosted" => Ok(Placement::Hosted),
        "self-hosted" | "self_hosted" => Ok(Placement::SelfHosted),
        _ => bail!("--placement is auto, hosted or self-hosted, not {s:?}"),
    }
}

fn placement_label(p: Placement) -> &'static str {
    match p {
        Placement::Auto => "auto",
        Placement::Hosted => "hosted",
        Placement::SelfHosted => "self-hosted",
    }
}

fn runner_label(k: Option<RunnerKind>) -> &'static str {
    match k {
        Some(RunnerKind::Hosted) => "hosted",
        Some(RunnerKind::SelfHosted) => "self-hosted",
        None => "-",
    }
}

fn status_label(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Queued => "queued",
        RunStatus::Running => "running",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

/// `chanto run --cloud` arguments (after `run`).
fn parse_run(args: &[String]) -> Result<RunOpts> {
    let mut o = RunOpts {
        task: String::new(),
        org: None,
        labels: vec![],
        placement: None,
        push: false,
        detach: false,
        cwd: None,
    };
    let mut words: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .cloned()
                .with_context(|| format!("{flag} needs a value"))
        };
        match a.as_str() {
            "--cloud" => {}
            "--push" => o.push = true,
            "--detach" | "--no-follow" => o.detach = true,
            "--org" => o.org = Some(value(a)?),
            "--label" => o.labels.push(value(a)?),
            "--placement" => o.placement = Some(parse_placement(&value(a)?)?),
            "-C" | "--cwd" | "--cd" => o.cwd = Some(PathBuf::from(value(a)?)),
            s if s.starts_with("--org=") => o.org = Some(s["--org=".len()..].to_string()),
            s if s.starts_with("--label=") => o.labels.push(s["--label=".len()..].to_string()),
            s if s.starts_with("--placement=") => {
                o.placement = Some(parse_placement(&s["--placement=".len()..])?)
            }
            "--" => break,
            "-h" | "--help" => bail!("{USAGE}"),
            s if s.starts_with('-') => bail!(
                "{s} doesn't apply to cloud runs: the runner's [background] settings and the org's model settings decide how it runs\n{USAGE}"
            ),
            s => words.push(s.to_string()),
        }
    }
    // Everything after `--` is the task.
    words.extend(it.cloned());
    o.task = words.join(" ");
    if o.task.trim().is_empty() {
        bail!("usage: chanto run --cloud \"<task>\"");
    }
    Ok(o)
}

/// `chanto run --cloud "<task>"`: queue a run of this repository's current
/// branch, then follow it. Returns the exit code.
pub async fn run_cmd(args: &[String]) -> Result<i32> {
    let o = parse_run(args)?;
    let c = Cloud::signed_in()?;
    let cwd = match &o.cwd {
        Some(p) => p.clone(),
        None => std::env::current_dir()?,
    };
    let push = o.push;
    let src = tokio::task::spawn_blocking(move || local_source(&cwd, push)).await??;
    let org = resolve_org(&c, o.org.as_deref()).await?;
    let req = CreateRun {
        repo: src.repo.clone(),
        git_ref: Some(src.branch.clone()),
        task: o.task.clone(),
        placement: o.placement,
        labels: o.labels.clone(),
        ..Default::default()
    };
    let run = c.submit(&org, &req).await?;
    println!("{}", queued_message(&run));
    if o.detach {
        return Ok(0);
    }
    Ok(match follow(&c, &run.id, Duration::from_secs(2)).await? {
        Some(r) if r.status == RunStatus::Succeeded => 0,
        Some(_) => 1,
        None => 0,
    })
}

pub(crate) fn queued_message(run: &Run) -> String {
    let labels = if run.labels.is_empty() {
        String::new()
    } else {
        format!(" · labels {}", run.labels.join(", "))
    };
    format!(
        "Queued cloud run {id} in {org}: {task}\n  from {git_ref} of {repo} · placement {placement}{labels}\n{hint}",
        id = run.id,
        org = run.org,
        task = crate::util::one_line(&run.task, 80),
        git_ref = run.git_ref.as_deref().unwrap_or("the default branch"),
        repo = run.repo,
        placement = placement_label(run.placement),
        hint = ui::dim(&format!(
            "  chanto attach {short} --cloud     follow it (Ctrl-C detaches; the run keeps going)\n  chanto runs cancel {short} --cloud  stop it",
            short = short_id(&run.id)
        )),
    )
}

fn short_id(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// How long ago, or how long, in words.
fn duration_label(run: &Run) -> String {
    match run.started_at {
        Some(start) => {
            let end = run.finished_at.unwrap_or_else(chrono::Utc::now);
            fmt_duration((end - start).num_seconds().max(0) as u64)
        }
        None => "-".into(),
    }
}

/// Events per page from `GET /v1/runs/{id}/events`; a full page means more follow.
const EVENT_PAGE: usize = 1000;

/// Prints a cloud run's events and state until it finishes. `None`: the
/// user detached with Ctrl-C. `every`: how often to ask the server.
pub async fn follow(c: &Cloud, id: &str, every: Duration) -> Result<Option<Run>> {
    follow_or_cancel(c, id, every, None).await
}

/// [`follow`], where Ctrl-C first asks `cancel` whether to cancel the run:
/// yes cancels it and keeps following until it stops (Ctrl-C again detaches),
/// no detaches.
pub async fn follow_or_cancel(
    c: &Cloud,
    id: &str,
    every: Duration,
    cancel: Option<&(dyn Fn(&str) -> bool + Sync)>,
) -> Result<Option<Run>> {
    let mut cancelling = false;
    let mut pause = false;
    let mut after = 0u64;
    let mut in_text = false;
    let mut last: Option<RunStatus> = None;
    let mut queued_since = Instant::now();
    let mut hinted = false;
    let mut failures = 0u32;
    loop {
        // The pause is inside the select, so Ctrl-C works while waiting too.
        let polled = tokio::select! {
            r = async { if pause { tokio::time::sleep(every).await; } poll_once(c, id, after).await } => r,
            _ = tokio::signal::ctrl_c() => {
                if in_text { println!(); in_text = false; }
                if let Some(ask) = cancel.filter(|_| !cancelling)
                    && tokio::task::block_in_place(|| ask(&format!("Cancel cloud run {}? (no: stop following; it keeps going)", short_id(id))))
                {
                    cancelling = true;
                    let _: Run = c.post(&format!("/v1/runs/{}/cancel", urlencoding::encode(id)), &json!({})).await?;
                    println!("{}", ui::dim("Cancelling; Ctrl-C again stops following."));
                    continue;
                }
                println!("{}", ui::dim(&format!("Detached; cloud run {id} keeps going. `chanto attach {} --cloud` comes back.", short_id(id))));
                return Ok(None);
            }
        };
        let (run, events) = match polled {
            Ok(p) => {
                failures = 0;
                p
            }
            // A blip in a long follow shouldn't end it; a server that says no should.
            Err(e) if failures < 5 && e.chain().any(|x| x.is::<reqwest::Error>()) => {
                failures += 1;
                eprintln!("{}", ui::dim(&format!("! {e:#}; retrying")));
                tokio::time::sleep(every * 2).await;
                continue;
            }
            Err(e) => return Err(e),
        };
        if last != Some(run.status) {
            if in_text {
                println!();
                in_text = false;
            }
            match run.status {
                RunStatus::Queued => {
                    queued_since = Instant::now();
                    println!(
                        "{}",
                        ui::dim(&format!(
                            "● queued: waiting for a runner{}",
                            if run.attempts > 0 {
                                " again (the last one stopped responding)"
                            } else {
                                ""
                            }
                        ))
                    );
                }
                RunStatus::Running => println!(
                    "{}",
                    ui::dim(&format!(
                        "● running on a {} runner",
                        runner_label(run.runner_kind)
                    ))
                ),
                _ => {}
            }
            last = Some(run.status);
        }
        if run.status == RunStatus::Queued
            && !hinted
            && queued_since.elapsed() > Duration::from_secs(60)
        {
            hinted = true;
            println!(
                "{}",
                ui::dim(&format!(
                    "  Still waiting. A run needs a runner: one of {}'s own (`chanto runner connect`, with every label the run asks for) or, for placement hosted or auto, hosted runners (billing on in the dashboard).",
                    run.org
                ))
            );
        }
        let full = events.events.len() >= EVENT_PAGE;
        for ev in &events.events {
            after = after.max(ev.seq);
            print_cloud_event(&ev.data, &mut in_text);
        }
        if run.status.is_terminal() && !full {
            if in_text {
                println!();
            }
            println!("\n{}", finished_text(&run));
            return Ok(Some(run));
        }
        pause = !full;
    }
}

/// The run's state, then its new events: once the state is final, the events
/// fetched after it are all there are.
async fn poll_once(c: &Cloud, id: &str, after: u64) -> Result<(Run, RunEventList)> {
    let run = c.run(id).await?;
    let events: RunEventList = c
        .get(&format!(
            "/v1/runs/{}/events?after={after}",
            urlencoding::encode(id)
        ))
        .await?;
    Ok((run, events))
}

fn print_cloud_event(ev: &Value, in_text: &mut bool) {
    if super::cli::print_event(ev, in_text) {
        return;
    }
    // Nobody watches a cloud run: its runner answers prompts itself.
    match ev["type"].as_str() {
        Some("permission_request") => {
            println!(
                "{}",
                ui::dim(&format!(
                    "▸ needed permission: {} (denied: cloud runs are unattended)",
                    ev["label"].as_str().unwrap_or("")
                ))
            )
        }
        Some("question") => println!(
            "{}",
            ui::dim(&format!(
                "? {} (the run uses its best judgment)",
                ev["question"].as_str().unwrap_or("")
            ))
        ),
        _ => {}
    }
}

/// A GitHub page to open a pull request from the run's branch.
fn compare_url(repo: &str, base: Option<&str>, branch: &str) -> Option<String> {
    let path = repo
        .strip_prefix("https://github.com/")?
        .trim_end_matches('/')
        .trim_end_matches(".git");
    Some(match base {
        Some(b) => format!("https://github.com/{path}/compare/{b}...{branch}?expand=1"),
        None => format!("https://github.com/{path}/compare/{branch}?expand=1"),
    })
}

/// How a run ended, and how to get its work.
pub fn finished_text(run: &Run) -> String {
    let mut s = format!(
        "● Cloud run {} {} · {}",
        short_id(&run.id),
        status_label(run.status),
        duration_label(run)
    );
    let result = run.result.as_ref();
    if let Some(summary) = result.map(|r| r.summary.trim()).filter(|s| !s.is_empty()) {
        s.push_str(&format!("\n\n{summary}\n"));
    }
    if let Some(pr) = result.and_then(|r| r.pr_url.as_deref()) {
        s.push_str(&format!("\nPull request: {pr}"));
    }
    match result.and_then(|r| r.branch.as_deref()) {
        Some(b) => {
            s.push_str(&format!("\nIts work is on the branch {b} on origin:\n  git fetch origin {b} && git switch {b}"));
            if let Some(url) = result
                .and_then(|r| r.pr_url.as_ref())
                .is_none()
                .then(|| compare_url(&run.repo, run.git_ref.as_deref(), b))
                .flatten()
            {
                s.push_str(&format!("\nOpen a pull request: {url}"));
            }
        }
        None if run.status == RunStatus::Succeeded => {
            s.push_str("\nNo branch was pushed (the run made no changes).")
        }
        None => {}
    }
    if run.status == RunStatus::Failed
        && result.is_some_and(|r| r.summary.contains("no runner took the run"))
    {
        s.push_str(&format!(
            "\nNo runner was available: connect one of {}'s own (`chanto runner connect`), or use hosted runners (`--placement hosted`, billing on).",
            run.org
        ));
    }
    s
}

pub fn table(runs: &[Run]) -> String {
    if runs.is_empty() {
        return "No cloud runs. Queue one with `chanto run --cloud \"<task>\"`.".into();
    }
    let mut s = format!(
        "{:<8}  {:<9}  {:<11}  {:<12}  {:>8}  {:<24}  {}\n",
        "ID", "STATUS", "RUNNER", "CREATED", "DURATION", "BRANCH", "TASK"
    );
    for r in runs {
        s.push_str(&format!(
            "{:<8}  {:<9}  {:<11}  {:<12}  {:>8}  {:<24}  {}\n",
            short_id(&r.id),
            status_label(r.status),
            runner_label(r.runner_kind),
            r.created_at
                .with_timezone(&chrono::Local)
                .format("%b %d %H:%M")
                .to_string(),
            duration_label(r),
            r.result
                .as_ref()
                .and_then(|x| x.branch.as_deref())
                .unwrap_or("-"),
            crate::util::one_line(&r.task, 60),
        ));
    }
    s.trim_end().to_string()
}

pub(crate) fn detail_text(r: &Run) -> String {
    let labels = if r.labels.is_empty() {
        String::new()
    } else {
        format!(" · labels {}", r.labels.join(", "))
    };
    let mut s = format!(
        "Cloud run {}  {}\nOrg:       {}\nTask:      {}\nRepo:      {} @ {}\nPlacement: {}{labels} · runner {} · attempts {}\nCreated:   {}",
        r.id,
        status_label(r.status),
        r.org,
        r.task,
        r.repo,
        r.git_ref.as_deref().unwrap_or("default branch"),
        placement_label(r.placement),
        runner_label(r.runner_kind),
        r.attempts,
        r.created_at
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M"),
    );
    if r.cancel_requested && !r.status.is_terminal() {
        s.push_str("\nCancelling (the runner stops at its next heartbeat).");
    }
    if r.status.is_terminal() {
        s.push_str(&format!("\n\n{}", finished_text(r)));
    }
    s
}

/// A run by its full id, or by a prefix of one among the org's recent runs.
pub(crate) async fn find_run(c: &Cloud, org: Option<&str>, id: &str) -> Result<Run> {
    if id.len() == 36 {
        return c.run(id).await;
    }
    let org = resolve_org(c, org).await?;
    let list: RunList = c
        .get(&format!(
            "/v1/orgs/{}/runs?limit=100",
            urlencoding::encode(&org)
        ))
        .await?;
    let mut found: Vec<Run> = list
        .runs
        .into_iter()
        .filter(|r| r.id.starts_with(id))
        .collect();
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => bail!(
            "no cloud run {id} among {org}'s 100 most recent runs; pass the full id (or --org)"
        ),
        n => bail!("{id} matches {n} cloud runs; give more of the id"),
    }
}

/// `chanto runs ... --cloud` and `chanto attach <id> --cloud`. Returns the exit code.
pub async fn runs_cmd(args: &[String]) -> Result<i32> {
    let (mut org, mut status, mut limit) = (None, None, 30u32);
    let mut pos: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .cloned()
                .with_context(|| format!("{flag} needs a value"))
        };
        match a.as_str() {
            "--cloud" => {}
            "--org" => org = Some(value(a)?),
            "--status" => status = Some(value(a)?),
            "--limit" => limit = value(a)?.parse().context("--limit needs a number")?,
            s if s.starts_with("--org=") => org = Some(s["--org=".len()..].to_string()),
            "-h" | "--help" | "help" => {
                println!("{USAGE}");
                return Ok(0);
            }
            s if s.starts_with('-') => bail!("unknown flag {s}\n{USAGE}"),
            s => pos.push(s.to_string()),
        }
    }
    let c = Cloud::signed_in()?;
    let sub = pos.first().map(String::as_str).unwrap_or("list");
    let id = || {
        pos.get(1)
            .cloned()
            .with_context(|| format!("usage: chanto runs {sub} <id> --cloud"))
    };
    match sub {
        "list" | "ls" => {
            let org = resolve_org(&c, org.as_deref()).await?;
            let mut path = format!(
                "/v1/orgs/{}/runs?limit={}",
                urlencoding::encode(&org),
                limit.clamp(1, 100)
            );
            if let Some(s) = &status {
                path.push_str(&format!("&status={}", urlencoding::encode(s)));
            }
            let list: RunList = c.get(&path).await?;
            println!("{}", table(&list.runs));
        }
        "show" => println!(
            "{}",
            detail_text(&find_run(&c, org.as_deref(), &id()?).await?)
        ),
        "cancel" | "stop" => {
            let run = find_run(&c, org.as_deref(), &id()?).await?;
            let r: Run = c
                .post(
                    &format!("/v1/runs/{}/cancel", urlencoding::encode(&run.id)),
                    &json!({}),
                )
                .await?;
            println!(
                "{}",
                match r.status {
                    RunStatus::Cancelled => format!("Cancelled cloud run {}.", short_id(&r.id)),
                    RunStatus::Running => format!(
                        "Cancelling cloud run {}: its runner stops at the next heartbeat.",
                        short_id(&r.id)
                    ),
                    s => format!(
                        "Cloud run {} had already {}.",
                        short_id(&r.id),
                        status_label(s)
                    ),
                }
            );
        }
        "attach" | "follow" => {
            let run = find_run(&c, org.as_deref(), &id()?).await?;
            println!(
                "{}",
                ui::dim(&format!(
                    "Following cloud run {} ({}) · {} · Ctrl-C detaches",
                    short_id(&run.id),
                    status_label(run.status),
                    crate::util::one_line(&run.task, 80)
                ))
            );
            return Ok(match follow(&c, &run.id, Duration::from_secs(2)).await? {
                Some(r) if r.status != RunStatus::Succeeded => 1,
                _ => 0,
            });
        }
        other => bail!("`chanto runs {other}` isn't available for cloud runs\n{USAGE}"),
    }
    Ok(0)
}

#[cfg(test)]
mod cmd_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    fn user() -> Value {
        json!({"id": "u1", "login": "octo", "name": "Octo Cat"})
    }

    #[tokio::test]
    async fn browser_sign_in_returns_the_token() {
        let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
        let (s1, s2, s3) = (seen.clone(), seen.clone(), seen.clone());
        let polls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let app = axum::Router::new()
            .route(
                "/v1/auth/providers",
                axum::routing::get(|| async {
                    axum::Json(json!({"providers": [
                        {"id": "github", "name": "GitHub", "browser": true, "device": true},
                        {"id": "google", "name": "Google", "browser": true, "device": false}]}))
                }),
            )
            .route(
                "/v1/auth/browser/start",
                axum::routing::post(move |axum::Json(b): axum::Json<Value>| async move {
                    s1.lock().unwrap().push(b);
                    axum::Json(json!({"authorize_url": "https://github.example/authorize?x=1", "poll_code": "pc_secret", "expires_in": 60}))
                }),
            )
            .route(
                "/v1/auth/browser/poll",
                axum::routing::post(move |axum::Json(b): axum::Json<Value>| async move {
                    s2.lock().unwrap().push(b);
                    if polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        axum::Json(json!({"status": "pending", "interval": 1}))
                    } else {
                        axum::Json(json!({"status": "complete", "token": "crt_token", "user": user()}))
                    }
                }),
            );
        let url = serve(app).await;
        let (token, who) = sign_in(&url, Some("google"), false).await.unwrap();
        assert_eq!((token.as_str(), who.login.as_str()), ("crt_token", "octo"));
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0]["provider"], "google");
        assert!(
            seen[0]["client_name"]
                .as_str()
                .unwrap()
                .starts_with("chanto CLI on ")
        );
        assert_eq!(
            (seen[1]["poll_code"].as_str(), seen[2]["poll_code"].as_str()),
            (Some("pc_secret"), Some("pc_secret"))
        );

        let err = sign_in(&url, Some("bitbucket"), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("github, google"), "{err}");
        drop(seen);

        // Inside a session (`/login chanto`): GitHub, without asking.
        let (token, _) = sign_in_with(&url, None, false, true).await.unwrap();
        assert_eq!(token, "crt_token");
        assert_eq!(seen_now(&s3).last().unwrap()["provider"], "github");
    }

    fn seen_now(seen: &Arc<Mutex<Vec<Value>>>) -> Vec<Value> {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|b| b.get("provider").is_some())
            .cloned()
            .collect()
    }

    #[tokio::test]
    async fn device_sign_in_when_the_browser_is_not_offered_and_errors_are_verbatim() {
        let app = axum::Router::new()
            .route(
                "/v1/auth/providers",
                axum::routing::get(|| async { axum::Json(json!({"providers": [{"id": "github", "name": "GitHub", "browser": false, "device": true}]})) }),
            )
            .route(
                "/v1/auth/device/start",
                axum::routing::post(|| async {
                    axum::Json(json!({"device_code": "dc", "user_code": "ABCD-1234", "verification_uri": "https://github.example/device", "expires_in": 60, "interval": 1}))
                }),
            )
            .route(
                "/v1/auth/device/poll",
                axum::routing::post(|axum::Json(b): axum::Json<Value>| async move {
                    assert_eq!(b["device_code"], "dc");
                    (axum::http::StatusCode::FORBIDDEN, axum::Json(json!({"error": "access_denied", "message": "the sign-in was denied"})))
                }),
            );
        let url = serve(app).await;
        // Inside a session the code would have nowhere to show: the terminal command instead.
        let err = sign_in_with(&url, None, false, true)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("run `chanto cloud login` in a terminal"),
            "{err}"
        );
        let err = sign_in(&url, None, false).await.unwrap_err().to_string();
        assert_eq!(err, "Chanto cloud: the sign-in was denied");
    }

    #[test]
    fn the_login_is_saved_privately() {
        let _home = crate::runs::tests::TestHome::new("cloud-login");
        save_login("https://cloud.example", "crt_saved").unwrap();
        // SAFETY: test-only, under the TestHome lock.
        let prev = (
            std::env::var_os("CHANTO_CLOUD_URL"),
            std::env::var_os("CHANTO_CLOUD_TOKEN"),
        );
        unsafe {
            std::env::remove_var("CHANTO_CLOUD_URL");
            std::env::remove_var("CHANTO_CLOUD_TOKEN");
        }
        let c = Cloud::signed_in().unwrap();
        assert_eq!(
            (c.url.as_str(), c.token.as_str()),
            ("https://cloud.example", "crt_saved")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(crate::config::home_dir().join("auth.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        set_default_org(Some("acme")).unwrap();
        assert_eq!(default_org().as_deref(), Some("acme"));
        // A token from the environment alone goes to the saved server (else DEFAULT_URL),
        // and the `chanto` provider bills the default org.
        unsafe { std::env::set_var("CHANTO_CLOUD_TOKEN", "crt_env") };
        let c = Cloud::signed_in().unwrap();
        assert_eq!(
            (c.url.as_str(), c.token()),
            ("https://cloud.example", "crt_env")
        );
        let p = crate::chanto_cloud::provider("claude-sonnet-4-5").unwrap();
        assert_eq!(p.base_url, "https://cloud.example/v1/model/anthropic");
        assert!(
            p.api_key.as_deref() == Some("crt_env")
                && p.headers.contains(&("x-chanto-org", "acme".to_string()))
        );
        unsafe { std::env::remove_var("CHANTO_CLOUD_TOKEN") };
        unsafe {
            if let Some(v) = prev.0 {
                std::env::set_var("CHANTO_CLOUD_URL", v)
            }
            if let Some(v) = prev.1 {
                std::env::set_var("CHANTO_CLOUD_TOKEN", v)
            }
        }
    }

    #[test]
    fn remotes_become_https_urls() {
        assert_eq!(
            https_url("git@github.com:acme/app.git"),
            "https://github.com/acme/app.git"
        );
        assert_eq!(
            https_url("git@github.com:acme/app"),
            "https://github.com/acme/app.git"
        );
        assert_eq!(
            https_url("ssh://git@gitlab.example:2222/group/app.git"),
            "https://gitlab.example/group/app.git"
        );
        assert_eq!(
            https_url("https://x-token:secret@github.com/acme/app.git"),
            "https://github.com/acme/app.git"
        );
        assert_eq!(
            https_url("https://github.com/acme/app"),
            "https://github.com/acme/app"
        );
        assert_eq!(https_url("/srv/git/app.git"), "/srv/git/app.git");
        assert_eq!(
            compare_url(
                "https://github.com/acme/app.git",
                Some("main"),
                "chanto/run/ab12cd34"
            )
            .as_deref(),
            Some("https://github.com/acme/app/compare/main...chanto/run/ab12cd34?expand=1")
        );
    }

    #[test]
    fn cloud_run_flags() {
        let args: Vec<String> = [
            "--cloud",
            "fix",
            "the",
            "--org",
            "acme",
            "--label",
            "gpu",
            "--placement=self-hosted",
            "--push",
            "bug",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let o = parse_run(&args).unwrap();
        assert_eq!(o.task, "fix the bug");
        assert_eq!(
            (o.org.as_deref(), o.labels.as_slice(), o.placement, o.push),
            (
                Some("acme"),
                &["gpu".to_string()][..],
                Some(Placement::SelfHosted),
                true
            )
        );
        assert!(parse_run(&["--cloud".to_string()]).is_err());
        assert!(
            parse_run(&["-m".to_string(), "x".to_string(), "task".to_string()])
                .unwrap_err()
                .to_string()
                .contains("doesn't apply")
        );
        assert!(
            parse_run(&[
                "--placement".to_string(),
                "moon".to_string(),
                "t".to_string()
            ])
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_branch_must_be_committed_and_pushed() {
        let dir = std::env::temp_dir().join(format!(
            "chanto-cloud-src-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let (origin, work) = (dir.join("origin.git"), dir.join("work"));
        std::fs::create_dir_all(&work).unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "init.defaultBranch=main",
                ])
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&dir, &["init", "-q", "--bare", origin.to_str().unwrap()]);
        git(&work, &["init", "-q"]);
        git(&work, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(work.join("a.txt"), "a").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "a"]);
        git(
            &work,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );

        let err = local_source(&work, false).unwrap_err().to_string();
        assert!(
            err.contains("feature isn't on origin") && err.contains("--push"),
            "{err}"
        );
        let src = local_source(&work, true).unwrap();
        assert_eq!(
            src,
            Source {
                repo: origin.to_str().unwrap().to_string(),
                branch: "feature".into()
            }
        );
        assert!(local_source(&work, false).is_ok(), "pushed");

        std::fs::write(work.join("b.txt"), "b").unwrap();
        let err = local_source(&work, true).unwrap_err().to_string();
        assert!(err.contains("uncommitted changes (1 file(s))"), "{err}");
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "b"]);
        let err = local_source(&work, false).unwrap_err().to_string();
        assert!(
            err.contains("1 commit(s) on feature aren't on origin"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn run_json(status: &str, result: Value) -> Value {
        json!({
            "id": "0b9e6f1c-3f6c-4a7e-9d61-2a1b3c4d5e6f", "org": "acme", "status": status,
            "repo": "https://github.com/acme/app.git", "git_ref": "feature", "task": "fix the bug",
            "placement": "auto", "labels": ["gpu"], "runner_kind": if status == "queued" { Value::Null } else { json!("hosted") },
            "attempts": if status == "queued" { 0 } else { 1 }, "cancel_requested": false,
            "created_at": "2026-10-14T12:00:00Z", "started_at": if status == "queued" { Value::Null } else { json!("2026-10-14T12:00:05Z") },
            "finished_at": if status == "succeeded" { json!("2026-10-14T12:04:05Z") } else { Value::Null },
            "result": result,
        })
    }

    /// Queue a run, then follow it from queued through running to its branch.
    #[tokio::test]
    async fn submit_and_follow_to_completion() {
        type Created = Arc<Mutex<Option<(String, Option<String>, Value)>>>;
        let created: Created = Arc::default();
        let afters: Arc<Mutex<Vec<u64>>> = Arc::default();
        let gets = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let (cr, af) = (created.clone(), afters.clone());
        let app = axum::Router::new()
            .route(
                "/v1/orgs/{org}/runs",
                axum::routing::post(
                    move |axum::extract::Path(org): axum::extract::Path<String>, h: axum::http::HeaderMap, axum::Json(b): axum::Json<Value>| async move {
                        let auth = h.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_string);
                        *cr.lock().unwrap() = Some((org, auth, b));
                        (axum::http::StatusCode::CREATED, axum::Json(run_json("queued", Value::Null)))
                    },
                ),
            )
            .route(
                "/v1/runs/{id}",
                axum::routing::get(move || async move {
                    match gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                        0 => axum::Json(run_json("queued", Value::Null)),
                        1 => axum::Json(run_json("running", Value::Null)),
                        _ => axum::Json(run_json("succeeded", json!({"summary": "Fixed it.", "branch": "chanto/run/ab12cd34"}))),
                    }
                }),
            )
            .route(
                "/v1/runs/{id}/events",
                axum::routing::get(move |axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>| async move {
                    let after: u64 = q.get("after").and_then(|a| a.parse().ok()).unwrap_or(0);
                    af.lock().unwrap().push(after);
                    let all = [
                        json!({"type": "assistant_start"}),
                        json!({"type": "text", "text": "Looking at the bug."}),
                        json!({"type": "tool_start", "label": "Bash cargo test"}),
                    ];
                    let events: Vec<Value> = all
                        .iter()
                        .enumerate()
                        .map(|(i, d)| (i as u64 + 1, d))
                        .filter(|(seq, _)| *seq > after)
                        .map(|(seq, d)| json!({"seq": seq, "at": "2026-10-14T12:00:06Z", "data": d}))
                        .collect();
                    axum::Json(json!({"events": events}))
                }),
            );
        let url = serve(app).await;
        let c = Cloud::new(&url, "crt_token");
        let req = CreateRun {
            repo: "https://github.com/acme/app.git".into(),
            git_ref: Some("feature".into()),
            task: "fix the bug".into(),
            placement: Some(Placement::SelfHosted),
            labels: vec!["gpu".into()],
            ..Default::default()
        };
        let queued = c.submit("acme", &req).await.unwrap();
        assert_eq!(queued.status, RunStatus::Queued);
        let (org, auth, body) = created.lock().unwrap().clone().unwrap();
        assert_eq!(
            (org.as_str(), auth.as_deref()),
            ("acme", Some("Bearer crt_token"))
        );
        assert_eq!(
            serde_json::from_value::<CreateRun>(body.clone()).unwrap(),
            req
        );
        assert_eq!(body["placement"], "self_hosted");

        let done = follow(&c, &queued.id, Duration::from_millis(10))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.status, RunStatus::Succeeded);
        assert_eq!(
            done.result.as_ref().and_then(|r| r.branch.as_deref()),
            Some("chanto/run/ab12cd34")
        );
        let afters = afters.lock().unwrap().clone();
        assert_eq!(afters.first(), Some(&0));
        assert_eq!(
            afters.last(),
            Some(&3),
            "asks only for events it hasn't seen: {afters:?}"
        );
        let text = finished_text(&done);
        assert!(
            text.contains("git fetch origin chanto/run/ab12cd34"),
            "{text}"
        );
        assert!(
            text.contains("https://github.com/acme/app/compare/feature...chanto/run/ab12cd34"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn server_refusals_are_shown_with_what_to_do() {
        let app = axum::Router::new()
            .route(
                "/v1/orgs/{org}/runs",
                axum::routing::post(|| async {
                    (
                        axum::http::StatusCode::PAYMENT_REQUIRED,
                        axum::Json(json!({"error": "quota_exceeded", "message": "hosted runners need a card on file: add one in acme's billing settings"})),
                    )
                }),
            )
            .route(
                "/v1/me",
                axum::routing::get(|| async { (axum::http::StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "unauthorized", "message": "invalid token"}))) }),
            );
        let url = serve(app).await;
        let c = Cloud::new(&url, "crt_old");
        let req = CreateRun {
            repo: "https://github.com/acme/app.git".into(),
            task: "t".into(),
            placement: Some(Placement::Hosted),
            ..Default::default()
        };
        let err = c.submit("acme", &req).await.unwrap_err().to_string();
        assert!(err.starts_with("Chanto cloud: hosted runners need a card on file: add one in acme's billing settings"), "{err}");
        assert!(err.contains("--placement self-hosted"), "{err}");
        let err = c.get::<User>("/v1/me").await.unwrap_err().to_string();
        assert!(
            err.contains("invalid token") && err.contains("chanto cloud login"),
            "{err}"
        );
        let err = c.get::<Value>("/v1/nothing").await.unwrap_err().to_string();
        assert!(
            err.contains("doesn't look like a Chanto cloud server"),
            "{err}"
        );
    }
}
