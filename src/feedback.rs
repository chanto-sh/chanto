//! `/feedback` (and `chanto feedback`), plus opt-in automatic error reports.
//!
//! Both go to the Chanto website as small JSON records. Feedback is sent only
//! after the user has seen the exact payload and said Send. Error reports are
//! off unless the user said yes (asked once, recorded as `[feedback]
//! share_errors` in ~/.chanto/config.toml); they are rate-limited, sent in
//! the background, and failures stay silent. Everything is redacted: the home
//! directory becomes `~`, secrets are masked and URLs keep only their host.
//! Prompts, code, conversation and API keys are never part of a record.

use crate::agent::Frontend;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

pub const ENDPOINT: &str = concat!(site_url!(), "/api/feedback");
/// Characters of an error message that go into a report.
const MAX_ERROR_CHARS: usize = 500;
/// Characters of a feedback message.
const MAX_FEEDBACK_CHARS: usize = 4000;
/// The same kind of error is reported at most once an hour…
const PER_KIND_SECS: i64 = 3600;
/// …and at most this many reports go out a day.
const PER_DAY: u32 = 10;
/// Error/warn lines from the diagnostic log attached to feedback.
const LOG_LINES: usize = 20;
/// Diagnostic mode: reports a day, and how often new log lines go out.
const DIAGNOSTICS_PER_DAY: u32 = 300;
const DIAGNOSTICS_EVERY: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FeedbackConfig {
    /// Send short, redacted error reports when something breaks (default off;
    /// asked once on the first interactive run).
    pub share_errors: Option<bool>,
}

/// Where reports go (`CHANTO_FEEDBACK_URL` overrides it, for tests).
pub fn url() -> String {
    std::env::var("CHANTO_FEEDBACK_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| ENDPOINT.to_string())
}

/// `CHANTO_NO_ERROR_REPORTS=1` turns error reports off whatever the config says.
fn forced_off() -> bool {
    std::env::var("CHANTO_NO_ERROR_REPORTS").is_ok_and(|v| !matches!(v.trim(), "" | "0" | "false"))
}

/// Whether error reports go out, given the setting and the override.
fn reports_on(setting: Option<bool>, forced_off: bool) -> bool {
    setting == Some(true) && !forced_off
}

/// For `chanto doctor` and `/privacy-settings`.
pub fn status(cfg: &crate::config::Config) -> String {
    if forced_off() {
        return "off (CHANTO_NO_ERROR_REPORTS)".into();
    }
    if let Some(name) = load(&crate::config::home_dir()).diagnostics(chrono::Utc::now().timestamp())
    {
        return format!(
            "on (diagnostic mode{}; `chanto diagnostics off` stops it)",
            if name.is_empty() {
                String::new()
            } else {
                format!(", tagged \"{name}\"")
            }
        );
    }
    match cfg.feedback.share_errors {
        Some(true) => "on ([feedback] share_errors = true)".into(),
        Some(false) => "off ([feedback] share_errors = false)".into(),
        None => "off (default)".into(),
    }
}

// ------------------------------------------------------------------ redaction

/// Make text safe to send: secrets masked, URLs reduced to scheme and host
/// (no credentials, paths or queries), the home directory written as `~`.
pub fn scrub(s: &str) -> String {
    let home = dirs::home_dir().map(|h| h.display().to_string());
    scrub_with(s, home.as_deref())
}

fn scrub_with(s: &str, home: Option<&str>) -> String {
    static URL: OnceLock<regex::Regex> = OnceLock::new();
    let url = URL.get_or_init(|| {
        regex::Regex::new(r#"\b[a-zA-Z][a-zA-Z0-9+.-]*://[^\s"'<>()\[\]{}`]+"#).unwrap()
    });
    let text = url
        .replace_all(s, |c: &regex::Captures| {
            let raw = &c[0];
            match reqwest::Url::parse(raw) {
                Ok(u) if u.host_str().is_some() => {
                    let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
                    let more = if u.path().len() > 1 || u.query().is_some() {
                        "/…"
                    } else {
                        ""
                    };
                    format!(
                        "{}://{}{port}{more}",
                        u.scheme(),
                        u.host_str().unwrap_or("")
                    )
                }
                _ => format!("{}://…", raw.split("://").next().unwrap_or("url")),
            }
        })
        .into_owned();
    let mut text = crate::secrets::redact_always(&text).into_owned();
    if let Some(home) = home
        .map(|h| h.trim_end_matches(['/', '\\']))
        .filter(|h| h.len() > 1)
    {
        // Inside JSON (log lines) a Windows home is escaped: C:\\Users\\me.
        if home.contains('\\') {
            text = text.replace(&home.replace('\\', "\\\\"), "~");
        }
        text = text.replace(home, "~");
        // Windows paths also show up with forward slashes.
        if home.contains('\\') {
            text = text.replace(&home.replace('\\', "/"), "~");
        }
    }
    text
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

// --------------------------------------------------------------- diagnostics

/// The small block of facts sent with feedback.
#[derive(Debug, Clone, Default)]
pub struct Diagnostics {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub mode: Option<String>,
}

fn provider_id(kind: crate::llm::ProviderKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn terminal() -> Option<String> {
    std::env::var("TERM_PROGRAM")
        .ok()
        .or_else(|| std::env::var("TERM").ok())
        .filter(|t| !t.is_empty())
        .map(|t| truncate(&scrub(&t), 60))
}

/// The last error/warn lines of the diagnostic log (`chanto logs`), redacted.
pub fn recent_problems(root: &Path, max: usize) -> Vec<String> {
    problems_since(root, None, max)
        .into_iter()
        .map(|(_, l)| l)
        .collect()
}

/// Warning and error lines from the diagnostic log newer than `since` (its
/// RFC 3339 time), oldest first, at most the last `max`: (time, redacted line).
fn problems_since(root: &Path, since: Option<&str>, max: usize) -> Vec<(String, String)> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(root.join("logs"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("chanto-") && n.ends_with(".log"))
        })
        .collect();
    files.sort();
    let mut lines = Vec::new();
    for file in files.iter().rev().take(2).rev() {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let level = v["level"].as_str().unwrap_or("");
            if !matches!(level, "error" | "warn" | "warning") {
                continue;
            }
            // Unset fields (`"cost": null`) only make the line longer.
            let mut data = v["data"].clone();
            if let Some(o) = data.as_object_mut() {
                o.retain(|_, x| !x.is_null());
            }
            let data = if data.is_null() || data.as_object().is_some_and(|o| o.is_empty()) {
                String::new()
            } else {
                format!(" {data}")
            };
            let time = v["time"].as_str().unwrap_or("").to_string();
            if since.is_some_and(|s| time.as_str() <= s) {
                continue;
            }
            let entry = format!("{time} {level} {}{data}", v["event"].as_str().unwrap_or(""));
            lines.push((time, truncate(&scrub(&entry), 300)));
        }
    }
    let skip = lines.len().saturating_sub(max);
    lines.split_off(skip)
}

fn base_record(kind: &str) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("type".into(), json!(kind));
    m.insert("version".into(), json!(env!("CARGO_PKG_VERSION")));
    m.insert("os".into(), json!(std::env::consts::OS));
    m.insert("arch".into(), json!(std::env::consts::ARCH));
    m
}

/// The feedback record exactly as it will be sent.
pub fn feedback_payload(message: &str, d: &Diagnostics, log: &[String]) -> Value {
    let mut m = base_record("feedback");
    m.insert(
        "message".into(),
        json!(truncate(&scrub(message.trim()), MAX_FEEDBACK_CHARS)),
    );
    if let Some(t) = terminal() {
        m.insert("terminal".into(), json!(t));
    }
    for (key, value) in [
        ("provider", &d.provider),
        ("model", &d.model),
        ("mode", &d.mode),
    ] {
        if let Some(v) = value.as_deref().filter(|v| !v.is_empty()) {
            m.insert(key.into(), json!(truncate(&scrub(v), 100)));
        }
    }
    if !log.is_empty() {
        m.insert("log".into(), json!(log));
    }
    Value::Object(m)
}

fn error_payload_with(
    kind: &str,
    message: &str,
    provider: Option<&str>,
    install_id: &str,
    timestamp: i64,
    label: Option<&str>,
    log: &[String],
) -> Value {
    let mut m = base_record("error");
    if let Some(l) = label.filter(|l| !l.is_empty()) {
        m.insert("label".into(), json!(truncate(l, 64)));
    }
    if !log.is_empty() {
        m.insert("log".into(), json!(log));
    }
    m.insert("kind".into(), json!(kind));
    m.insert(
        "message".into(),
        json!(truncate(&scrub(message), MAX_ERROR_CHARS)),
    );
    if let Some(p) = provider.filter(|p| !p.is_empty()) {
        m.insert("provider".into(), json!(p));
    }
    m.insert("install_id".into(), json!(install_id));
    m.insert(
        "timestamp".into(),
        json!(
            chrono::DateTime::from_timestamp(timestamp, 0)
                .unwrap_or_default()
                .to_rfc3339()
        ),
    );
    Value::Object(m)
}

// --------------------------------------------------------------------- send

pub async fn send(url: &str, payload: &Value, timeout: Duration) -> Result<()> {
    let client = crate::http::builder()
        .timeout(timeout)
        .user_agent(concat!("chanto/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp = client
        .post(url)
        .json(payload)
        .send()
        .await
        .context("couldn't reach the feedback server")?;
    if !resp.status().is_success() {
        anyhow::bail!("the feedback server answered {}", resp.status());
    }
    Ok(())
}

// ------------------------------------------------------------------ feedback

const SEND: &str = "Send";
const SEND_NO_LOG: &str = "Send without log lines";
const CANCEL: &str = "Cancel";

/// Show the exact payload, ask, and send only on Send.
pub async fn confirm_and_send(
    fe: &dyn Frontend,
    url: &str,
    message: &str,
    d: &Diagnostics,
    log: &[String],
) -> String {
    let full = feedback_payload(message, d, log);
    let mut options = vec![SEND.to_string()];
    if !log.is_empty() {
        options.push(SEND_NO_LOG.to_string());
    }
    options.push(CANCEL.to_string());
    let host = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| url.to_string());
    let q = format!(
        "This is exactly what will be sent to {host} (nothing else: no prompts, code or keys):\n\n{}\n\nSend it?",
        serde_json::to_string_pretty(&full).unwrap_or_default()
    );
    let payload = match fe.choose(&q, &options).await.as_deref() {
        Some(SEND) => full,
        Some(SEND_NO_LOG) => feedback_payload(message, d, &[]),
        _ => return "Feedback not sent.".into(),
    };
    match send(url, &payload, Duration::from_secs(15)).await {
        Ok(()) => "Thanks! Your feedback was sent.".into(),
        Err(e) => {
            format!("Couldn't send your feedback ({e:#}). Nothing was saved; try again later.")
        }
    }
}

/// `/feedback <message>` (and `/bug`).
pub async fn command(
    env: &std::sync::Arc<crate::agent::Env>,
    arg: &str,
) -> crate::commands::Outcome {
    if arg.trim().is_empty() {
        return crate::commands::Outcome::Show(
            "Usage: `/feedback <message>`: what went wrong, what you expected, or what you'd like. You'll see exactly what is sent (your message, the chanto version, OS, terminal, provider type, model, mode and recent error lines from the log, redacted) and confirm first. From a shell: `chanto feedback \"<message>\"`.".into(),
        );
    }
    let d = Diagnostics {
        provider: Some(provider_id(env.provider().kind)),
        model: Some(env.model()),
        mode: serde_json::to_value(env.mode())
            .ok()
            .and_then(|v| v.as_str().map(str::to_string)),
    };
    let log = recent_problems(&crate::config::home_dir(), LOG_LINES);
    crate::commands::Outcome::Show(
        confirm_and_send(env.frontend.as_ref(), &url(), arg, &d, &log).await,
    )
}

pub const USAGE: &str = "Usage: chanto feedback [--yes] [--no-log] <message>\n\nSend feedback (or a bug report) to the Chanto developers. Shows exactly what will be sent and asks first; --yes sends without asking, --no-log leaves out recent error lines from the diagnostic log.";

/// `chanto feedback [--yes] [--no-log] <message>`.
pub async fn cli(args: &[String]) -> Result<String> {
    use std::io::{IsTerminal, Write};
    if args
        .iter()
        .any(|a| a == "-h" || a == "--help" || a == "help")
        && args.len() == 1
    {
        return Ok(USAGE.into());
    }
    let yes = args.iter().any(|a| a == "--yes" || a == "-y");
    let no_log = args.iter().any(|a| a == "--no-log");
    let message = args
        .iter()
        .filter(|a| !matches!(a.as_str(), "--yes" | "-y" | "--no-log"))
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    if message.trim().is_empty() {
        anyhow::bail!("{USAGE}");
    }
    let cwd = std::env::current_dir()?;
    let (cfg, _) = crate::config::Config::load(&cwd, crate::config::is_trusted(&cwd))?;
    let d = Diagnostics {
        provider: Some(provider_id(cfg.provider)),
        model: cfg.model.clone(),
        mode: serde_json::to_value(cfg.mode.unwrap_or_default())
            .ok()
            .and_then(|v| v.as_str().map(str::to_string)),
    };
    let log = if no_log {
        vec![]
    } else {
        recent_problems(&crate::config::home_dir(), LOG_LINES)
    };
    let payload = feedback_payload(&message, &d, &log);
    let url = url();
    println!(
        "This is exactly what will be sent to {url}:\n\n{}\n",
        serde_json::to_string_pretty(&payload)?
    );
    if !yes {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("Not sent: add --yes to send without a prompt");
        }
        print!("Send it? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            return Ok("Feedback not sent.".into());
        }
    }
    send(&url, &payload, Duration::from_secs(15)).await?;
    Ok("Thanks! Your feedback was sent.".into())
}

// ------------------------------------------------------------- error reports

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    /// Random, generated once; ties reports from one install together and nothing else.
    install_id: Option<String>,
    /// The first-run question was answered (also recorded in config.toml).
    asked: bool,
    /// Unix time each error kind was last sent.
    sent: BTreeMap<String, i64>,
    day: String,
    day_count: u32,
    /// Diagnostic mode: on until this Unix time.
    diagnostics_until: Option<i64>,
    /// The name the tester gave, sent with each report.
    diagnostics_name: Option<String>,
    /// The newest log line already sent in diagnostic mode (its time).
    diagnostics_sent_upto: Option<String>,
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join("feedback.json")
}

fn load(dir: &Path) -> State {
    std::fs::read(state_path(dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(dir: &Path, state: &State) {
    let _ = std::fs::create_dir_all(dir);
    if let Ok(bytes) = serde_json::to_vec_pretty(state) {
        let _ = crate::util::write_private(&state_path(dir), &bytes);
    }
}

impl State {
    fn install_id(&mut self) -> String {
        self.install_id
            .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
            .clone()
    }

    /// Diagnostic mode's name, while it's on.
    fn diagnostics(&self, now: i64) -> Option<String> {
        self.diagnostics_until
            .filter(|u| *u > now)
            .map(|_| self.diagnostics_name.clone().unwrap_or_default())
    }

    /// Rate limit: each kind at most once per `PER_KIND_SECS`, `PER_DAY` in all.
    /// Diagnostic mode, which a tester turned on to send everything, allows far more.
    fn allow(&mut self, kind: &str, now: i64) -> bool {
        let diagnostics = self.diagnostics(now).is_some();
        let today = chrono::DateTime::from_timestamp(now, 0)
            .unwrap_or_default()
            .format("%Y-%m-%d")
            .to_string();
        if self.day != today {
            self.day = today;
            self.day_count = 0;
        }
        self.sent.retain(|_, t| now - *t < 86_400);
        let (per_day, per_kind) = if diagnostics {
            (DIAGNOSTICS_PER_DAY, 0)
        } else {
            (PER_DAY, PER_KIND_SECS)
        };
        if self.day_count >= per_day || self.sent.get(kind).is_some_and(|t| now - t < per_kind) {
            return false;
        }
        self.sent.insert(kind.to_string(), now);
        self.day_count += 1;
        true
    }
}

static STATE_LOCK: Mutex<()> = Mutex::new(());
static ENABLED: AtomicBool = AtomicBool::new(false);
static PROVIDER: Mutex<String> = Mutex::new(String::new());

/// Send one error report from `dir`'s install, unless rate-limited.
/// Returns whether it was sent.
async fn report_in(
    dir: &Path,
    url: &str,
    kind: &str,
    message: &str,
    provider: Option<&str>,
    timestamp: i64,
) -> Result<bool> {
    let now = chrono::Utc::now().timestamp();
    let id = {
        let _guard = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = load(dir);
        if !state.allow(kind, now) {
            return Ok(false);
        }
        let id = state.install_id();
        save(dir, &state);
        (id, state.diagnostics(now))
    };
    let (id, label) = id;
    send(
        url,
        &error_payload_with(
            kind,
            message,
            provider,
            &id,
            timestamp,
            label.as_deref(),
            &[],
        ),
        Duration::from_secs(5),
    )
    .await?;
    Ok(true)
}

// --------------------------------------------------------- diagnostic mode

/// Diagnostic mode, for someone helping test Chanto: for a set time, every
/// error report goes out (whatever `share_errors` says, with a looser limit)
/// and new warning and error lines from the diagnostic log are sent in
/// batches, redacted the same way, tagged with a name the tester picks.
pub const DIAGNOSTICS_USAGE: &str = "Usage: chanto diagnostics on [--hours N] [--name <your name>] | off | status\n\n\
Diagnostic mode sends the Chanto developers what goes wrong while you use it, without asking each time: \
error reports, plus new warning and error lines from the diagnostic log (`chanto logs`) every few minutes. \
Paths, secrets and URLs are removed as for `/feedback`; your prompts, code and files are never sent. \
It turns itself off after --hours (default 24, at most 168). --name tags your reports so the developers know whose they are.";

/// `chanto diagnostics …` and `/diagnostics …`.
pub fn diagnostics_command(args: &[String]) -> Result<String> {
    let dir = crate::config::home_dir();
    let now = chrono::Utc::now().timestamp();
    let _guard = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = load(&dir);
    match args.first().map(String::as_str) {
        Some("on") => {
            if forced_off() {
                anyhow::bail!("CHANTO_NO_ERROR_REPORTS is set, so nothing can be sent; unset it first");
            }
            let mut hours = 24i64;
            let mut name = state.diagnostics_name.clone();
            let mut it = args.iter().skip(1);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--hours" => hours = it.next().and_then(|h| h.parse().ok()).context("--hours needs a number")?,
                    "--name" => name = Some(it.next().context("--name needs a value")?.trim().to_string()).filter(|n| !n.is_empty()),
                    other => anyhow::bail!("unknown option {other}\n\n{DIAGNOSTICS_USAGE}"),
                }
            }
            let hours = hours.clamp(1, 168);
            state.diagnostics_until = Some(now + hours * 3600);
            state.diagnostics_name = name.clone();
            // Only what goes wrong from now on, not the whole log's history.
            state.diagnostics_sent_upto = Some(chrono::Utc::now().to_rfc3339());
            state.install_id();
            save(&dir, &state);
            Ok(format!(
                "Diagnostic mode is on for {hours} hour(s){}: Chanto sends error reports and new warnings and errors from its log to the developers (no prompts, code or files). `chanto diagnostics off` stops it. It takes effect the next time Chanto starts.",
                name.map(|n| format!(", tagged \"{n}\"")).unwrap_or_default()
            ))
        }
        Some("off") => {
            state.diagnostics_until = None;
            save(&dir, &state);
            Ok("Diagnostic mode is off.".into())
        }
        Some("status") | None => Ok(match state.diagnostics(now) {
            Some(name) => {
                let left = (state.diagnostics_until.unwrap_or(now) - now) / 60;
                format!("Diagnostic mode is on for {}h{:02}m more{}.", left / 60, left % 60, if name.is_empty() { String::new() } else { format!(", tagged \"{name}\"") })
            }
            None => "Diagnostic mode is off. `chanto diagnostics on --name <you>` turns it on for a day.".into(),
        }),
        Some("--help" | "-h" | "help") => Ok(DIAGNOSTICS_USAGE.into()),
        Some(other) => anyhow::bail!("unknown subcommand {other}\n\n{DIAGNOSTICS_USAGE}"),
    }
}

/// Send the log lines that are new since the last batch. Returns how many went.
async fn send_diagnostics_batch(dir: &Path, url: &str, provider: Option<&str>) -> Result<usize> {
    let now = chrono::Utc::now().timestamp();
    let (since, label, id) = {
        let _guard = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = load(dir);
        let Some(label) = state.diagnostics(now) else {
            return Ok(0);
        };
        let id = state.install_id();
        (state.diagnostics_sent_upto.clone(), label, id)
    };
    let lines = problems_since(dir, since.as_deref(), 50);
    let Some(newest) = lines.last().map(|(t, _)| t.clone()) else {
        return Ok(0);
    };
    let log: Vec<String> = lines.into_iter().map(|(_, l)| l).collect();
    let n = log.len();
    let payload = error_payload_with(
        "diagnostics",
        &format!("{n} new warning/error line(s) from the diagnostic log"),
        provider,
        &id,
        now,
        Some(&label),
        &log,
    );
    send(url, &payload, Duration::from_secs(10)).await?;
    let _guard = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = load(dir);
    state.diagnostics_sent_upto = Some(newest);
    save(dir, &state);
    Ok(n)
}

/// Report in the background when `enabled`; failures go to the local log only.
fn spawn_report(
    enabled: bool,
    dir: PathBuf,
    url: String,
    kind: &str,
    message: &str,
    provider: Option<String>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !enabled {
        return None;
    }
    let handle = tokio::runtime::Handle::try_current().ok()?;
    let (kind, message) = (kind.to_string(), message.to_string());
    Some(handle.spawn(async move {
        let now = chrono::Utc::now().timestamp();
        if let Err(e) = report_in(&dir, &url, &kind, &message, provider.as_deref(), now).await {
            note_failure(&dir, &format!("error report ({kind}) not sent: {e:#}"));
        }
    }))
}

fn note_failure(dir: &Path, text: &str) {
    use std::io::Write;
    let _ = std::fs::create_dir_all(dir.join("logs"));
    if let Ok(mut f) = crate::util::append_private(&dir.join("logs").join("feedback.log")) {
        let _ = writeln!(f, "{} {text}", chrono::Utc::now().to_rfc3339());
    }
}

/// A known-bad event: reported when the user opted in, never blocking.
pub fn report_error(kind: &str, message: &str) {
    let provider = PROVIDER
        .lock()
        .map(|p| p.clone())
        .ok()
        .filter(|p| !p.is_empty());
    spawn_report(
        ENABLED.load(Ordering::Relaxed),
        crate::config::home_dir(),
        url(),
        kind,
        message,
        provider,
    );
}

/// At startup: ask once (interactive), set the switch, send waiting crash files.
pub fn init(cfg: &crate::config::Config, can_ask: bool) {
    let dir = crate::config::home_dir();
    let mut setting = cfg.feedback.share_errors;
    if can_ask && setting.is_none() && !forced_off() && !cfg!(test) && !load(&dir).asked {
        let answer = ask_stdin();
        record_answer(&dir, answer);
        setting = Some(answer);
    }
    let diagnostics = !forced_off()
        && load(&dir)
            .diagnostics(chrono::Utc::now().timestamp())
            .is_some();
    let on = reports_on(setting, forced_off()) || diagnostics;
    ENABLED.store(on, Ordering::Relaxed);
    if diagnostics
        && !cfg!(test)
        && let Ok(handle) = tokio::runtime::Handle::try_current()
    {
        let (dir, provider) = (dir.clone(), provider_id(cfg.provider));
        handle.spawn(async move {
            loop {
                tokio::time::sleep(DIAGNOSTICS_EVERY).await;
                if load(&dir)
                    .diagnostics(chrono::Utc::now().timestamp())
                    .is_none()
                {
                    break;
                }
                if let Err(e) = send_diagnostics_batch(&dir, &url(), Some(&provider)).await {
                    note_failure(&dir, &format!("diagnostics batch not sent: {e:#}"));
                }
            }
        });
    }
    if let Ok(mut p) = PROVIDER.lock() {
        *p = provider_id(cfg.provider);
    }
    if on && let Ok(handle) = tokio::runtime::Handle::try_current() {
        let provider = provider_id(cfg.provider);
        handle.spawn(async move {
            send_crashes(&dir, &url(), true, Some(&provider)).await;
        });
    }
}

pub const QUESTION: &str = "Send anonymous error reports to help improve chanto? Only the error type and message (paths and secrets stripped), the version, OS and provider type, and a random install id. Never your prompts, code, files or keys. Change it any time with `[feedback] share_errors`.";

fn ask_stdin() -> bool {
    use std::io::Write;
    eprint!(
        "{} ",
        crate::ui::word_wrap(
            &format!("\n{QUESTION}\n\nSend error reports? [y/N]"),
            crate::ui::width().saturating_sub(1)
        )
    );
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    let yes = matches!(line.trim(), "y" | "Y" | "yes" | "Yes");
    eprintln!(
        "{}",
        if yes {
            "Error reports on. Thanks!\n"
        } else {
            "Error reports off.\n"
        }
    );
    yes
}

/// `chanto setup`: ask again (the answer may change).
pub fn ask_in_setup() {
    use std::io::IsTerminal;
    if forced_off() || !std::io::stdin().is_terminal() {
        return;
    }
    record_answer(&crate::config::home_dir(), ask_stdin());
}

/// Remember the answer: in config.toml (where people look) and the state file
/// (so an unwritable or broken config never makes us ask again).
fn record_answer(dir: &Path, yes: bool) {
    {
        let _guard = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = load(dir);
        state.asked = true;
        state.install_id();
        save(dir, &state);
    }
    let path = dir.join("config.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else {
        return;
    };
    if !doc.get("feedback").is_some_and(|f| f.is_table()) {
        doc["feedback"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc["feedback"]["share_errors"] = toml_edit::value(yes);
    let tmp = path.with_extension(format!("toml.{}.tmp", uuid::Uuid::new_v4().simple()));
    if std::fs::write(&tmp, doc.to_string()).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

// -------------------------------------------------------------------- panics

fn crash_dir(dir: &Path) -> PathBuf {
    dir.join("crashes")
}

/// Write a crash file for every panic; it is sent at the next start when
/// error reports are on, and stays local otherwise.
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic".into());
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let _ = write_crash(&crash_dir(&crate::config::home_dir()), &message, &location);
        prev(info);
    }));
}

fn write_crash(dir: &Path, message: &str, location: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    // Keep a few at most when nobody sends them.
    let mut existing: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    existing.sort();
    while existing.len() >= 10 {
        let _ = std::fs::remove_file(existing.remove(0));
    }
    let now = chrono::Utc::now();
    let record = json!({
        "message": truncate(&scrub(message), MAX_ERROR_CHARS),
        "location": scrub(location),
        "version": env!("CARGO_PKG_VERSION"),
        "timestamp": now.timestamp(),
    });
    let name = format!(
        "{}-{}.json",
        now.format("%Y%m%dT%H%M%S"),
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    crate::util::write_private(&dir.join(name), record.to_string().as_bytes())
}

/// Send crash files left by earlier runs (when enabled), deleting each once
/// it is sent. Returns how many were sent.
async fn send_crashes(dir: &Path, url: &str, enabled: bool, provider: Option<&str>) -> usize {
    if !enabled {
        return 0;
    }
    let crashes = crash_dir(dir);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&crashes)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    let mut sent = 0;
    for file in files {
        let Some(record) = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        else {
            let _ = std::fs::remove_file(&file);
            continue;
        };
        // A crash from an older version is only noise for this one.
        if record["version"].as_str() != Some(env!("CARGO_PKG_VERSION")) {
            let _ = std::fs::remove_file(&file);
            continue;
        }
        let message = format!(
            "{} at {}",
            record["message"].as_str().unwrap_or("panic"),
            record["location"].as_str().unwrap_or("?")
        );
        let timestamp = record["timestamp"]
            .as_i64()
            .unwrap_or_else(|| chrono::Utc::now().timestamp());
        match report_in(dir, url, "panic", &message, provider, timestamp).await {
            Ok(true) => {
                let _ = std::fs::remove_file(&file);
                sent += 1;
            }
            // Rate-limited: try again at a later start.
            Ok(false) => break,
            Err(e) => {
                note_failure(dir, &format!("crash report not sent: {e:#}"));
                break;
            }
        }
    }
    sent
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A feedback endpoint that records what it receives.
    async fn server() -> (String, Arc<Mutex<Vec<Value>>>) {
        let got: Arc<Mutex<Vec<Value>>> = Arc::default();
        let store = got.clone();
        let app = axum::Router::new().route(
            "/api/feedback",
            axum::routing::post(move |axum::Json(b): axum::Json<Value>| {
                let store = store.clone();
                async move {
                    store.lock().unwrap().push(b);
                    (
                        axum::http::StatusCode::ACCEPTED,
                        axum::Json(json!({"ok": true})),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/api/feedback"), got)
    }

    struct Answer(&'static str, Mutex<Vec<String>>);
    #[async_trait::async_trait]
    impl Frontend for Answer {
        fn emit(&self, _: crate::agent::Event) {}
        async fn permission(
            &self,
            _: crate::agent::PermissionRequest,
        ) -> crate::agent::PermissionReply {
            crate::agent::PermissionReply::Allow
        }
        async fn question(&self, q: &str, _: &[String]) -> Option<String> {
            self.1.lock().unwrap().push(q.to_string());
            Some(self.0.to_string())
        }
    }

    #[test]
    fn redaction_removes_home_secrets_and_url_details() {
        let text = "failed reading /Users/alice/proj/src/main.rs with key sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG at https://user:hunter2@api.example.com/v1/chat?key=abc and OPENAI_API_KEY=Zx81kQpLm29vBnTr4sYw";
        let out = scrub_with(text, Some("/Users/alice"));
        assert!(out.contains("~/proj/src/main.rs"), "{out}");
        assert!(
            !out.contains("/Users/alice") && !out.contains("hunter2") && !out.contains("key=abc"),
            "{out}"
        );
        assert!(
            !out.contains("sk-ant-api03") && !out.contains("Zx81kQpLm29vBnTr4sYw"),
            "{out}"
        );
        assert!(out.contains("https://api.example.com/…"), "{out}");
        let p = feedback_payload(
            "it broke in /Users/alice/x",
            &Diagnostics {
                provider: Some("openai".into()),
                model: Some("gpt-5".into()),
                mode: Some("auto".into()),
            },
            &[],
        );
        let home = dirs::home_dir().unwrap().display().to_string();
        assert!(!p.to_string().contains(&home), "{p}");
        assert_eq!(p["type"], "feedback");
        assert_eq!(p["provider"], "openai");
        assert!(p.get("log").is_none());
    }

    #[test]
    fn windows_home_is_redacted_plain_escaped_and_with_slashes() {
        let home = r"C:\Users\sam";
        let s = r#"C:\Users\sam\a {"path":"C:\\Users\\sam\\b"} C:/Users/sam/c"#;
        assert_eq!(scrub_with(s, Some(home)), r#"~\a {"path":"~\\b"} ~/c"#);
    }

    #[test]
    fn log_lines_are_errors_and_warnings_only_and_redacted() {
        let dir = tempfile_dir();
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        let home = dirs::home_dir().unwrap().display().to_string();
        let mut lines = vec![
            json!({"time": "t0", "level": "info", "event": "model_call", "data": {}}).to_string(),
        ];
        for i in 0..25 {
            lines.push(json!({"time": format!("t{i}"), "level": "error", "event": "tool", "data": {"path": format!("{home}/secret/{i}")}}).to_string());
        }
        std::fs::write(dir.join("logs/chanto-2026-10-15.log"), lines.join("\n")).unwrap();
        let got = recent_problems(&dir, 20);
        assert_eq!(got.len(), 20);
        assert!(
            got.iter()
                .all(|l| l.contains(" error tool") && !l.contains(&home)),
            "{got:?}"
        );
        let last = got.last().unwrap();
        assert!(
            last.contains("~/secret/24") || last.contains("~\\\\secret/24"),
            "{last}"
        );
    }

    fn tempfile_dir() -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("chanto-feedback-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn feedback_needs_confirmation() {
        let (url, got) = server().await;
        let d = Diagnostics::default();
        let cancel = Answer(CANCEL, Mutex::default());
        assert_eq!(
            confirm_and_send(&cancel, &url, "hello", &d, &["line".into()]).await,
            "Feedback not sent."
        );
        assert!(got.lock().unwrap().is_empty());
        // The question shows the exact payload.
        assert!(cancel.1.lock().unwrap()[0].contains("\"message\": \"hello\""));

        let send = Answer(SEND_NO_LOG, Mutex::default());
        assert!(
            confirm_and_send(&send, &url, "hello", &d, &["line".into()])
                .await
                .starts_with("Thanks")
        );
        let send = Answer(SEND, Mutex::default());
        assert!(
            confirm_and_send(&send, &url, "again", &d, &["line".into()])
                .await
                .starts_with("Thanks")
        );
        let got = got.lock().unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0]["message"], "hello");
        assert!(got[0].get("log").is_none());
        assert_eq!(got[1]["log"], json!(["line"]));
    }

    #[tokio::test]
    async fn error_reports_respect_opt_in_and_rate_limit() {
        let (url, got) = server().await;
        let dir = tempfile_dir();
        assert!(
            !reports_on(None, false)
                && !reports_on(Some(false), false)
                && !reports_on(Some(true), true)
                && reports_on(Some(true), false)
        );
        // Off: nothing is even started.
        assert!(
            spawn_report(
                false,
                dir.clone(),
                url.clone(),
                "model_request",
                "boom",
                None
            )
            .is_none()
        );
        // On: sent once, then the same kind is held back for an hour.
        spawn_report(
            true,
            dir.clone(),
            url.clone(),
            "model_request",
            "boom at /x",
            Some("ollama".into()),
        )
        .unwrap()
        .await
        .unwrap();
        spawn_report(
            true,
            dir.clone(),
            url.clone(),
            "model_request",
            "boom again",
            None,
        )
        .unwrap()
        .await
        .unwrap();
        {
            let got = got.lock().unwrap();
            assert_eq!(got.len(), 1);
            assert_eq!(got[0]["type"], "error");
            assert_eq!(got[0]["kind"], "model_request");
            assert_eq!(got[0]["provider"], "ollama");
            assert!(
                got[0]["install_id"]
                    .as_str()
                    .is_some_and(|id| id.len() == 36)
            );
        }
        let mut state = load(&dir);
        let now = chrono::Utc::now().timestamp();
        assert!(!state.allow("model_request", now + 60));
        assert!(state.allow("model_request", now + PER_KIND_SECS + 1));
        // A daily cap across kinds.
        let mut s = State::default();
        let day = 1_760_000_000;
        assert_eq!(
            (0..20).filter(|i| s.allow(&format!("k{i}"), day)).count(),
            PER_DAY as usize
        );
        // The install id is stable.
        let id = load(&dir).install_id;
        spawn_report(
            true,
            dir.clone(),
            url.clone(),
            "classifier_unavailable",
            "x",
            None,
        )
        .unwrap()
        .await
        .unwrap();
        assert_eq!(got.lock().unwrap()[1]["install_id"].as_str(), id.as_deref());
    }

    #[tokio::test]
    async fn diagnostic_mode_tags_reports_relaxes_limits_and_sends_new_log_lines() {
        let (url, got) = server().await;
        let dir = tempfile_dir();
        let now = chrono::Utc::now().timestamp();
        let mut state = State {
            diagnostics_until: Some(now + 3600),
            diagnostics_name: Some("sam".into()),
            ..Default::default()
        };
        // The same kind again within the hour is fine, and far more per day.
        assert!(state.allow("model_request", now) && state.allow("model_request", now + 1));
        assert_eq!(
            (0..50)
                .filter(|i| state.allow(&format!("k{i}"), now + 2))
                .count(),
            50
        );
        // Expired: back to the normal limits, and no label.
        let mut expired = State {
            diagnostics_until: Some(now - 1),
            ..Default::default()
        };
        assert!(expired.diagnostics(now).is_none());
        assert!(expired.allow("x", now) && !expired.allow("x", now + 1));

        state.sent.clear();
        state.day_count = 0;
        state.diagnostics_sent_upto = Some("2026-10-15T10:00:00Z".into());
        save(&dir, &state);
        spawn_report(
            true,
            dir.clone(),
            url.clone(),
            "model_request",
            "boom",
            None,
        )
        .unwrap()
        .await
        .unwrap();
        assert_eq!(got.lock().unwrap()[0]["label"], "sam");

        std::fs::create_dir_all(dir.join("logs")).unwrap();
        let line = |t: &str, level: &str, event: &str| {
            json!({"time": t, "level": level, "event": event, "data": {}}).to_string()
        };
        let log = [
            line("2026-10-15T09:00:00Z", "error", "old"),
            line("2026-10-15T11:00:00Z", "info", "fine"),
            line("2026-10-15T11:00:01Z", "warn", "new"),
        ];
        std::fs::write(dir.join("logs/chanto-2026-10-15.log"), log.join("\n")).unwrap();
        assert_eq!(
            send_diagnostics_batch(&dir, &url, Some("deepseek"))
                .await
                .unwrap(),
            1
        );
        {
            let got = got.lock().unwrap();
            assert_eq!(got[1]["kind"], "diagnostics");
            assert_eq!(got[1]["label"], "sam");
            assert_eq!(got[1]["log"].as_array().unwrap().len(), 1);
            assert!(got[1]["log"][0].as_str().unwrap().contains("warn new"));
        }
        // Nothing new: nothing sent.
        assert_eq!(send_diagnostics_batch(&dir, &url, None).await.unwrap(), 0);
        assert_eq!(got.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn crash_files_are_sent_when_enabled_then_deleted() {
        let (url, got) = server().await;
        let dir = tempfile_dir();
        let home = dirs::home_dir().unwrap().display().to_string();
        write_crash(
            &crash_dir(&dir),
            &format!("index out of range in {home}/proj"),
            "src/agent.rs:10",
        )
        .unwrap();
        // Off: kept locally, nothing sent.
        assert_eq!(send_crashes(&dir, &url, false, None).await, 0);
        assert_eq!(std::fs::read_dir(crash_dir(&dir)).unwrap().count(), 1);
        assert!(got.lock().unwrap().is_empty());
        assert_eq!(send_crashes(&dir, &url, true, Some("openai")).await, 1);
        assert_eq!(std::fs::read_dir(crash_dir(&dir)).unwrap().count(), 0);
        let got = got.lock().unwrap();
        assert_eq!(got[0]["kind"], "panic");
        let msg = got[0]["message"].as_str().unwrap();
        assert!(
            msg.contains("~/proj") && msg.contains("src/agent.rs:10") && !msg.contains(&home),
            "{msg}"
        );
    }

    #[test]
    fn answer_is_recorded_in_config_and_state() {
        let dir = tempfile_dir();
        std::fs::write(dir.join("config.toml"), "model = \"qwen3\"\n").unwrap();
        record_answer(&dir, false);
        let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(
            text.contains("model = \"qwen3\"") && text.contains("[feedback]\nshare_errors = false"),
            "{text}"
        );
        let state = load(&dir);
        assert!(state.asked && state.install_id.is_some());
        let cfg: crate::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.feedback.share_errors, Some(false));
    }
}
