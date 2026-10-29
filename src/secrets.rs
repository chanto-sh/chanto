//! Keeping secrets away from the model and out of transcripts.
//!
//! 1. Known values: secret-named environment variables, Chanto's own API
//!    keys and the project's `.env`-style files are collected, and their
//!    exact values (plus base64 and URL-encoded forms) are masked as
//!    `[REDACTED:NAME]` in everything sent to the model and saved to disk.
//! 2. Known formats: regexes for common token formats (AWS, GitHub, Stripe,
//!    Slack, private keys, JWTs, ...) and secret-looking `KEY=value` pairs
//!    catch secrets Chanto wasn't told about.
//! 3. Prevention: reading secret files needs approval (permissions.rs), and
//!    secret-named variables aren't passed to shell commands (tools/bash.rs).

use aho_corasick::{AhoCorasick, MatchKind};
use base64::Engine;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SecretsConfig {
    /// Mask secrets in what's sent to the model and saved in transcripts.
    pub redact: bool,
    /// Don't pass secret-named environment variables to shell commands.
    pub scrub_env: bool,
    /// Variables to pass to shell commands anyway (names or globs, e.g. "GH_TOKEN", "AWS_*").
    pub pass_env: Vec<String>,
    /// Reading secret files (.env, keys, ~/.ssh, ~/.aws, ...) needs approval.
    pub protect_files: bool,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self {
            redact: true,
            scrub_env: true,
            pass_env: vec![],
            protect_files: true,
        }
    }
}

/// A credential held in memory: an API key, an OAuth token, an account token.
/// `Debug` shows only that it is set, so a struct holding one can be logged
/// or debug-printed without leaking it, and there is no `Display` to format
/// it into text by accident. [`Secret::expose`] is the one way to the value,
/// for where it has to go (a request header, the keychain). It serializes as
/// the plain string, so `auth.json` keeps its format.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// The value, for sending or storing it; never for logs or messages.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_empty() {
            "Secret(\"\")"
        } else {
            "Secret(***)"
        })
    }
}

/// Names of variables (and `.env` keys) that hold secrets.
fn strong_secret_name(name: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)(secret|token|passw(or)?d|passwd|pwd|api_?key|access_?key|private_?key|credential|auth|cookie|session_?key|signing_?key|client_?secret|webhook|dsn|database_url|conn(ection)?_?str)").unwrap()
    });
    re.is_match(name)
}

pub fn is_secret_name(name: &str) -> bool {
    // Common non-secrets that match the pattern.
    const NOT: &[&str] = &[
        "SSH_AUTH_SOCK",
        "GPG_AGENT_INFO",
        "XAUTHORITY",
        "AUTHOR",
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "CHANTO_SERVER_TOKEN_FILE",
        // Names of keys, not keys: database, cache and sort keys.
        "CACHE_KEY",
        "SORT_KEY",
        "PRIMARY_KEY",
        "PARTITION_KEY",
        "FOREIGN_KEY",
        "HASH_KEY",
        "RANGE_KEY",
        "TURBO_KEY",
    ];
    // A plain `*_KEY` (`STRIPE_KEY`, `ENCRYPTION_KEY`) is a key too; the
    // underscore keeps words that merely end in "key" (`MONKEY`) out.
    let upper = name.to_ascii_uppercase();
    (strong_secret_name(name) || upper.ends_with("_KEY") || upper == "KEY")
        && !NOT.contains(&upper.as_str())
        && !upper.ends_with("PUBLIC_KEY")
        && !upper.ends_with("PUBKEY")
        && !upper.ends_with("_PATH")
        && !upper.ends_with("_FILE")
        && !upper.ends_with("_DIR")
        && !upper.ends_with("_URL_PATH")
}

/// Looks random enough to be a credential rather than a word or config value.
fn secret_value(name: &str, value: &str) -> bool {
    is_secret_name(name) && (strong_secret_name(name) || high_entropy(value))
}

fn high_entropy(v: &str) -> bool {
    if v.len() < 16
        || !v.chars().any(|c| c.is_ascii_digit())
        || !v.chars().any(|c| c.is_ascii_alphabetic())
    {
        return false;
    }
    let mut counts = [0usize; 256];
    for b in v.bytes() {
        counts[b as usize] += 1;
    }
    let n = v.len() as f64;
    let h: f64 = counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| -(c as f64 / n) * (c as f64 / n).log2())
        .sum();
    h >= 3.5
}

/// A value worth masking: long enough, not a path, flag or plain number.
fn maskable(v: &str) -> bool {
    let v = v.trim();
    v.len() >= 8
        && !v.starts_with('/')
        && !v.starts_with('~')
        && !v.starts_with("./")
        && !v.chars().all(|c| c.is_ascii_digit())
        && !matches!(
            v.to_ascii_lowercase().as_str(),
            "true"
                | "false"
                | "enabled"
                | "disabled"
                | "localhost"
                | "changeme"
                | "password"
                | "development"
                | "production"
        )
}

struct Redactor {
    /// (label, value) from the environment and config, kept across refreshes.
    base: Vec<(String, String)>,
    /// (label, value) from the project's env files, replaced by each refresh.
    extra: Vec<(String, String)>,
    /// (value, label) pairs including encodings, longest first.
    values: Vec<(String, String)>,
    ac: Option<AhoCorasick>,
    /// Project secret files already read, with their modification times.
    files: BTreeMap<PathBuf, std::time::SystemTime>,
}

fn state() -> &'static RwLock<Arc<Redactor>> {
    static S: OnceLock<RwLock<Arc<Redactor>>> = OnceLock::new();
    S.get_or_init(|| {
        RwLock::new(Arc::new(Redactor {
            base: vec![],
            extra: vec![],
            values: vec![],
            ac: None,
            files: BTreeMap::new(),
        }))
    })
}

static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Values Chanto learns while running (sign-in and MCP OAuth tokens, a
/// source checkout's env files), kept when `init` runs again.
static LEARNED: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// Mask `value` from now on, like a secret from the environment. For
/// credentials obtained at run time: an OAuth access token, a refreshed one.
pub fn register(label: &str, value: &str) {
    register_all(vec![(label.to_string(), value.to_string())]);
}

fn register_all(pairs: Vec<(String, String)>) {
    let pairs: Vec<(String, String)> = pairs.into_iter().filter(|(_, v)| maskable(v)).collect();
    let mut learned = LEARNED.lock().unwrap_or_else(|e| e.into_inner());
    // Called on every request made with a sign-in token: nothing new is the
    // common case, and costs no copy of the known values.
    let known = |v: &str| {
        learned.iter().any(|(_, l)| l == v)
            && state()
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .base
                .iter()
                .any(|(_, b)| b == v)
    };
    if pairs.iter().all(|(_, v)| known(v)) {
        return;
    }
    let mut st = state().write().unwrap_or_else(|e| e.into_inner());
    let mut base = st.base.clone();
    for (label, value) in pairs {
        if !maskable(&value) {
            continue;
        }
        if !learned.iter().any(|(_, v)| v == &value) {
            learned.push((label.clone(), value.clone()));
        }
        if !base.iter().any(|(_, v)| v == &value) {
            base.push((label, value));
        }
    }
    if base.len() != st.base.len() {
        *st = Arc::new(build(base, st.extra.clone(), st.files.clone()));
    }
}

/// Also mask the env files of `root`, a checkout other than the one being
/// worked in: a background run works in a worktree, which has none of the
/// source checkout's gitignored `.env` files, though its commands and the
/// model can still come across their values.
pub fn add_project(root: &Path) {
    let mut pairs = Vec::new();
    // Not through the walk cache, which belongs to the worked-in checkout.
    for p in walk_env_files(root).1 {
        if let Ok(text) = std::fs::read_to_string(&p) {
            pairs.extend(env_file_values(&text));
        }
    }
    register_all(pairs);
}

fn variants(v: &str) -> Vec<String> {
    let mut out = vec![v.to_string()];
    let b64 = base64::engine::general_purpose::STANDARD.encode(v);
    out.push(b64.trim_end_matches('=').to_string());
    let url = urlencoding::encode(v).into_owned();
    if url != v {
        out.push(url);
    }
    out
}

fn build(
    base: Vec<(String, String)>,
    extra: Vec<(String, String)>,
    files: BTreeMap<PathBuf, std::time::SystemTime>,
) -> Redactor {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (label, v) in base.iter().chain(&extra) {
        for var in variants(v) {
            if var.len() >= 8 && !pairs.iter().any(|(p, _)| *p == var) {
                pairs.push((var, label.clone()));
            }
        }
    }
    pairs.sort_by_key(|p| std::cmp::Reverse(p.0.len()));
    let ac = (!pairs.is_empty())
        .then(|| {
            AhoCorasick::builder()
                .match_kind(MatchKind::LeftmostLongest)
                .build(pairs.iter().map(|p| &p.0))
                .ok()
        })
        .flatten();
    Redactor {
        base,
        extra,
        values: pairs,
        ac,
        files,
    }
}

/// Secret files in the project (`.env`, `.env.local`, `.dev.vars`, ...), not examples.
/// Env files found by the last walk, and the directories it read with their
/// modification times. Adding or removing an entry changes its directory's
/// mtime, so when none changed the previous result still holds.
type Walk = (
    PathBuf,
    Vec<(PathBuf, Option<std::time::SystemTime>)>,
    Vec<PathBuf>,
);
static LAST_WALK: std::sync::Mutex<Option<Walk>> = std::sync::Mutex::new(None);

/// Do the first prompt's refresh ahead of time, in the background: its env
/// files, and the pattern set compiled, which otherwise costs the first
/// redaction a couple of milliseconds on the turn's own thread.
pub fn warm(root: &Path) {
    for (_, re, _) in patterns() {
        let _ = re.is_match("");
    }
    let _ = assignment().is_match("");
    refresh(root);
}

fn project_env_files(root: &Path) -> Vec<PathBuf> {
    let mtime = |d: &Path| std::fs::metadata(d).and_then(|m| m.modified()).ok();
    if let Some((r, dirs, files)) = LAST_WALK.lock().unwrap().as_ref()
        && r == root
        && dirs.iter().all(|(d, t)| mtime(d) == *t)
    {
        return files.clone();
    }
    let (seen, out) = walk_env_files(root);
    *LAST_WALK.lock().unwrap() = Some((root.to_path_buf(), seen, out.clone()));
    out
}

/// Directories read, with their modification times.
type DirTimes = Vec<(PathBuf, Option<std::time::SystemTime>)>;

/// The env files under `root`, uncached.
fn walk_env_files(root: &Path) -> (DirTimes, Vec<PathBuf>) {
    let mtime = |d: &Path| std::fs::metadata(d).and_then(|m| m.modified()).ok();
    let mut out = Vec::new();
    let mut seen = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        seen.push((dir.clone(), mtime(&dir)));
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if depth < 3
                    && !matches!(
                        name.as_str(),
                        "node_modules"
                            | "target"
                            | ".git"
                            | "dist"
                            | "build"
                            | ".venv"
                            | "venv"
                            | "__pycache__"
                            | ".next"
                            | "vendor"
                    )
                {
                    stack.push((p, depth + 1));
                }
            } else if is_env_file(&name) {
                out.push(p);
            }
        }
    }
    (seen, out)
}

fn is_env_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let example = [
        "example",
        "sample",
        "template",
        "dist",
        "defaults",
        "schema",
        "test.example",
    ]
    .iter()
    .any(|s| lower.ends_with(&format!(".{s}")));
    !example
        && (lower == ".env"
            || lower.starts_with(".env.")
            || lower == ".dev.vars"
            || lower.ends_with(".env") && lower.len() > 4)
}

/// `KEY=value` pairs of an env file worth masking.
fn env_file_values(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
        let v = v.split(" #").next().unwrap_or(v).trim();
        if maskable(v) && (secret_value(k, v) || high_entropy(v)) {
            out.push((k.to_string(), v.to_string()));
        }
    }
    out
}

/// Collect known secret values: environment, Chanto's config and the project's env files.
pub fn init(cfg: &crate::config::Config, root: &Path) {
    ENABLED.store(cfg.secrets.redact, std::sync::atomic::Ordering::Relaxed);
    *SESSION_CONFIG.write().unwrap_or_else(|e| e.into_inner()) = Some(cfg.secrets.clone());
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in std::env::vars() {
        if secret_value(&k, &v) && maskable(&v) {
            values.insert(k, v);
        }
    }
    // A pulled run's org secrets are masked whatever they're called.
    for k in run_secret_names() {
        if let Some(v) = std::env::var(&k).ok().filter(|v| maskable(v)) {
            values.insert(k, v);
        }
    }
    let mut add = |label: &str, v: &Option<String>| {
        if let Some(v) = v.as_ref().filter(|v| maskable(v)) {
            values.insert(label.to_string(), v.clone());
        }
    };
    add("API_KEY", &cfg.api_key);
    add("BRAVE_API_KEY", &cfg.web.brave_api_key);
    add("TAVILY_API_KEY", &cfg.web.tavily_api_key);
    for (name, value) in &cfg.telemetry.headers {
        add(
            &format!("TELEMETRY_{}", name.to_uppercase()),
            &Some(value.clone()),
        );
        add(
            &format!("TELEMETRY_{}", name.to_uppercase()),
            &Some(value.trim_start_matches("Bearer ").to_string()),
        );
    }
    for (name, p) in &cfg.providers {
        add(&format!("{}_API_KEY", name.to_uppercase()), &p.api_key);
    }
    for (name, s) in &cfg.mcp_servers {
        for (k, v) in s.env.iter().chain(s.headers.iter()) {
            if is_secret_name(k) || k.eq_ignore_ascii_case("authorization") {
                add(
                    &format!("{}_{}", name.to_uppercase(), k.to_uppercase()),
                    &Some(v.trim_start_matches("Bearer ").to_string()),
                );
            }
        }
    }
    let mut base: Vec<(String, String)> = values.into_iter().collect();
    let learned = LEARNED.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = state().write().unwrap_or_else(|e| e.into_inner());
    for (k, v) in learned.iter() {
        if !base.iter().any(|(_, b)| b == v) {
            base.push((k.clone(), v.clone()));
        }
    }
    *st = Arc::new(build(base, vec![], BTreeMap::new()));
    drop(st);
    drop(learned);
    refresh(root);
}

/// Pick up new or changed env files in the project (cheap when nothing changed).
pub fn refresh(root: &Path) {
    let files = project_env_files(root);
    let stamps: BTreeMap<PathBuf, std::time::SystemTime> = files
        .iter()
        .filter_map(|p| Some((p.clone(), std::fs::metadata(p).ok()?.modified().ok()?)))
        .collect();
    // Unchanged (most turns): no write lock, so redaction never waits on it.
    if stamps == state().read().unwrap_or_else(|e| e.into_inner()).files {
        return;
    }
    // Held to the end, so a value `register` adds meanwhile isn't lost.
    let mut st = state().write().unwrap_or_else(|e| e.into_inner());
    let cur = st.clone();
    if stamps == cur.files {
        return;
    }
    let mut extra = Vec::new();
    for p in stamps.keys() {
        if let Ok(text) = std::fs::read_to_string(p) {
            extra.extend(env_file_values(&text));
        }
    }
    *st = Arc::new(build(cur.base.clone(), extra, stamps));
}

/// Token formats worth masking wherever they appear: (label, regex, group to mask).
/// A token whose alphabet has `-` can end in one, where `\b` can't match, so
/// those end at the first character outside the alphabet instead.
fn patterns() -> &'static [(&'static str, Regex, usize)] {
    static P: OnceLock<Vec<(&'static str, Regex, usize)>> = OnceLock::new();
    P.get_or_init(|| {
        let p = |label: &'static str, re: &str, group: usize| (label, Regex::new(re).unwrap(), group);
        vec![
            p("PRIVATE_KEY", r"-----BEGIN [A-Z ]*PRIVATE KEY( BLOCK)?-----[\s\S]*?(-----END [A-Z ]*PRIVATE KEY( BLOCK)?-----|$)", 0),
            p("AWS_ACCESS_KEY", r"\b(AKIA|ASIA)[0-9A-Z]{16}\b", 0),
            p("AWS_SECRET", r#"(?i)aws_?secret_?access_?key["']?\s*[:=]\s*["']?([A-Za-z0-9/+=]{40})"#, 1),
            p("GITHUB_TOKEN", r"\b(gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{50,})\b", 0),
            p("GITLAB_TOKEN", r"\bglpat-[A-Za-z0-9_-]{20,}", 0),
            p("SLACK_TOKEN", r"\bxox[abposr]-[A-Za-z0-9-]{10,}", 0),
            p("SLACK_WEBHOOK", r"https://hooks\.slack\.com/services/[A-Za-z0-9/]+", 0),
            p("STRIPE_KEY", r"\b(sk|rk)_(live|test)_[A-Za-z0-9]{16,}\b", 0),
            p("ANTHROPIC_KEY", r"\bsk-ant-[A-Za-z0-9_-]{20,}", 0),
            p("OPENAI_KEY", r"\bsk-(proj-|svcacct-)?[A-Za-z0-9_-]{32,}", 0),
            p("GOOGLE_API_KEY", r"\b(AIza[0-9A-Za-z_-]{35})(?:[^0-9A-Za-z_-]|$)", 1),
            p("NPM_TOKEN", r"\bnpm_[A-Za-z0-9]{36}\b", 0),
            p("PYPI_TOKEN", r"\bpypi-AgE[A-Za-z0-9_-]{50,}", 0),
            p("HF_TOKEN", r"\bhf_[A-Za-z]{34,}\b", 0),
            p("SENDGRID_KEY", r"\b(SG\.[A-Za-z0-9_-]{22}\.[A-Za-z0-9_-]{43})(?:[^A-Za-z0-9_-]|$)", 1),
            p("DIGITALOCEAN_TOKEN", r"\bdop_v1_[a-f0-9]{64}\b", 0),
            p("JWT", r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}", 0),
            p("URL_PASSWORD", r"\b[a-z][a-z0-9+.-]*://[^\s:/@]+:([^\s@/]{3,})@", 1),
            p("BEARER_TOKEN", r"(?i)\bbearer\s+([A-Za-z0-9._~+/-]{20,}=*)", 1),
        ]
    })
}

/// `NAME_SECRET=value` / `"api_key": "value"` with a credential-looking value.
fn assignment() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r#"(?i)\b([A-Z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|API_?KEY|ACCESS_?KEY|PRIVATE_?KEY)[A-Z0-9_]*)["']?\s*[=:]\s*["']?([A-Za-z0-9/+_.=-]{16,})"#).unwrap()
    })
}

/// Mask secrets in `s`. Returns the text and the labels of what was masked.
pub fn redact_with_labels(s: &str) -> (Cow<'_, str>, Vec<String>) {
    if !ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
        return (Cow::Borrowed(s), vec![]);
    }
    redact_always_with_labels(s)
}

fn redact_always_with_labels(s: &str) -> (Cow<'_, str>, Vec<String>) {
    if s.is_empty() {
        return (Cow::Borrowed(s), vec![]);
    }
    redact_impl(s)
}

fn redact_impl(s: &str) -> (Cow<'_, str>, Vec<String>) {
    let r = state().read().unwrap().clone();
    let mut labels = Vec::new();
    let mut out: Cow<str> = Cow::Borrowed(s);
    if let Some(ac) = &r.ac
        && ac.is_match(out.as_ref())
    {
        let mut buf = String::with_capacity(out.len());
        let mut last = 0;
        for m in ac.find_iter(out.as_ref()) {
            let label = &r.values[m.pattern().as_usize()].1;
            buf.push_str(&out[last..m.start()]);
            buf.push_str(&format!("[REDACTED:{label}]"));
            labels.push(label.clone());
            last = m.end();
        }
        buf.push_str(&out[last..]);
        out = Cow::Owned(buf);
    }
    for (label, re, group) in patterns() {
        if re.is_match(out.as_ref()) {
            let replaced = re
                .replace_all(out.as_ref(), |c: &regex::Captures| {
                    labels.push(label.to_string());
                    let whole = c.get(0).unwrap();
                    match c.get(*group).filter(|_| *group > 0) {
                        Some(g) => format!(
                            "{}[REDACTED:{label}]{}",
                            &whole.as_str()[..g.start() - whole.start()],
                            &whole.as_str()[g.end() - whole.start()..]
                        ),
                        None => format!("[REDACTED:{label}]"),
                    }
                })
                .into_owned();
            out = Cow::Owned(replaced);
        }
    }
    let re = assignment();
    if re.is_match(out.as_ref()) {
        let replaced = re
            .replace_all(out.as_ref(), |c: &regex::Captures| {
                let whole = c.get(0).unwrap().as_str();
                let v = c.get(2).unwrap();
                if !high_entropy(v.as_str()) || v.as_str().starts_with("[REDACTED") {
                    return whole.to_string();
                }
                labels.push(c[1].to_uppercase());
                let start = v.start() - c.get(0).unwrap().start();
                format!("{}[REDACTED:{}]", &whole[..start], c[1].to_uppercase())
            })
            .into_owned();
        if replaced != out.as_ref() {
            out = Cow::Owned(replaced);
        }
    }
    labels.sort();
    labels.dedup();
    (out, labels)
}

pub fn redact_always(s: &str) -> Cow<'_, str> {
    redact_always_with_labels(s).0
}
/// Storage checks remain enabled even when display redaction is disabled.
pub fn contains_secret(s: &str) -> bool {
    let r = state().read().unwrap().clone();
    r.ac.as_ref().is_some_and(|ac| ac.is_match(s))
        || patterns().iter().any(|(_, re, _)| re.is_match(s))
        || assignment().captures_iter(s).any(|c| high_entropy(&c[2]))
}

/// Sharing must redact even when local transcript redaction is disabled.
pub fn redact_for_export(s: &str) -> Cow<'_, str> {
    redact_always(s)
}

pub fn redact(s: &str) -> Cow<'_, str> {
    redact_with_labels(s).0
}

fn redact_value(v: &mut serde_json::Value) {
    redact_value_with(v, &redact);
}

fn redact_value_with(v: &mut serde_json::Value, f: &dyn Fn(&str) -> Cow<'_, str>) {
    match v {
        serde_json::Value::String(s) => {
            if let Cow::Owned(r) = f(s) {
                *s = r;
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(|v| redact_value_with(v, f)),
        serde_json::Value::Object(o) => o.values_mut().for_each(|v| redact_value_with(v, f)),
        _ => {}
    }
}

/// Mask secrets in every string of `v`, except top-level `"images"` (base64
/// data, where a pattern match would only corrupt the picture). For events
/// that leave the session: run logs, the web UI, a runner's server.
pub fn redact_json(v: &mut serde_json::Value) {
    if ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
        redact_json_with(v, &redact);
    }
}

/// [`redact_json`] whatever the `redact` setting: for what's sent to a
/// cloud server, like a share.
pub fn redact_json_always(v: &mut serde_json::Value) {
    redact_json_with(v, &redact_always);
}

fn redact_json_with(v: &mut serde_json::Value, f: &dyn Fn(&str) -> Cow<'_, str>) {
    match v {
        serde_json::Value::Object(o) => {
            for (k, v) in o.iter_mut() {
                if k != "images" {
                    redact_value_with(v, f);
                }
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(|v| redact_json_with(v, f)),
        v => redact_value_with(v, f),
    }
}

/// The messages of a request with secrets masked, or `None` when nothing
/// in them needs masking (or masking is off). Deterministic for the same
/// known secrets, so a masked prefix stays the same from call to call.
pub fn redact_request_messages(msgs: &[crate::llm::Message]) -> Option<Vec<crate::llm::Message>> {
    if !ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let mut out: Option<Vec<crate::llm::Message>> = None;
    for (i, m) in msgs.iter().enumerate() {
        let content = match redact(&m.content) {
            Cow::Owned(r) => Some(r),
            Cow::Borrowed(_) => None,
        };
        let args: Vec<Option<serde_json::Value>> = m
            .tool_calls
            .iter()
            .map(|c| {
                let mut a = c.arguments.clone();
                redact_value(&mut a);
                (a != c.arguments).then_some(a)
            })
            .collect();
        if content.is_none() && args.iter().all(Option::is_none) {
            continue;
        }
        let out = out.get_or_insert_with(|| msgs.to_vec());
        if let Some(c) = content {
            out[i].set_content(c);
        }
        for (j, a) in args.into_iter().enumerate() {
            if let Some(a) = a {
                out[i].tool_calls[j].arguments = a;
            }
        }
    }
    out
}

/// Mask secrets in messages (content and tool-call arguments) in place.
pub fn redact_messages(msgs: &mut [crate::llm::Message]) {
    if !ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    for m in msgs {
        if let Cow::Owned(r) = redact(&m.content) {
            m.set_content(r);
        }
        for c in &mut m.tool_calls {
            redact_value(&mut c.arguments);
        }
    }
}

/// Reuse redaction for unchanged messages. A message counts as unchanged
/// only when its length and a GMAC tag of its content match: a keyed MAC
/// (AES-256-GCM over the content as associated data, under a random key that
/// never leaves the process), so no input can be crafted to look unchanged,
/// and the chance of two different messages matching is under 2^-100 even
/// for megabytes of text. That keeps a 16-byte tag per message instead of a
/// copy of the whole conversation. Tool-call arguments are small, so they're
/// compared with their exact originals.
#[derive(Default)]
pub struct MessageRedactor {
    state: Option<Arc<Redactor>>,
    contents: Vec<Redacted>,
}

/// One message as last sent: what identifies its content, and its content
/// and tool-call arguments redacted, when that differs.
struct Redacted {
    content: Seen,
    redacted: Option<String>,
    arguments: Vec<(serde_json::Value, Option<serde_json::Value>)>,
}

#[derive(PartialEq)]
enum Seen {
    Tag(usize, [u8; 16]),
    /// No key could be made: the content itself, compared exactly.
    Exact(String),
}

impl Seen {
    fn of(content: &str) -> Self {
        use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
        static KEY: std::sync::OnceLock<Option<LessSafeKey>> = std::sync::OnceLock::new();
        let key = KEY.get_or_init(|| {
            // Straight from the OS: aws-lc's own generator spends ~30 ms of
            // CPU seeding itself on first use, which a plain-HTTP session
            // otherwise never pays.
            let mut k = [0u8; 32];
            getrandom::fill(&mut k).ok()?;
            Some(LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &k).ok()?))
        });
        // The fixed nonce is safe here: a GCM nonce must not repeat when
        // tags are seen by someone who could forge one, and these tags never
        // leave this process.
        key.as_ref()
            .and_then(|key| {
                key.seal_in_place_separate_tag(
                    Nonce::assume_unique_for_key([0; 12]),
                    Aad::from(content.as_bytes()),
                    &mut [],
                )
                .ok()
            })
            .and_then(|tag| tag.as_ref().try_into().ok())
            .map_or_else(
                || Seen::Exact(content.to_owned()),
                |t| Seen::Tag(content.len(), t),
            )
    }
}

impl MessageRedactor {
    pub fn redact(&mut self, msgs: &mut [crate::llm::Message]) {
        if !ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
            self.contents.clear();
            self.state = None;
            return;
        }
        let current = state().read().unwrap().clone();
        if !self
            .state
            .as_ref()
            .is_some_and(|old| Arc::ptr_eq(old, &current))
        {
            self.contents.clear();
            self.state = Some(current);
        }
        self.contents.truncate(msgs.len());
        for (i, m) in msgs.iter_mut().enumerate() {
            let seen = Seen::of(&m.content);
            if i >= self.contents.len() || self.contents[i].content != seen {
                let redacted = match redact(&m.content) {
                    Cow::Owned(s) => Some(s),
                    Cow::Borrowed(_) => None,
                };
                let entry = Redacted {
                    content: seen,
                    redacted,
                    arguments: Vec::new(),
                };
                if i < self.contents.len() {
                    self.contents[i] = entry;
                } else {
                    self.contents.push(entry);
                }
            }
            let entry = &mut self.contents[i];
            if let Some(s) = &entry.redacted {
                m.set_content(s.clone());
            }
            entry.arguments.truncate(m.tool_calls.len());
            for (j, c) in m.tool_calls.iter_mut().enumerate() {
                if entry
                    .arguments
                    .get(j)
                    .is_none_or(|(orig, _)| *orig != c.arguments)
                {
                    let mut redacted = c.arguments.clone();
                    redact_value(&mut redacted);
                    let redacted = (redacted != c.arguments).then_some(redacted);
                    let cached = (c.arguments.clone(), redacted);
                    if j < entry.arguments.len() {
                        entry.arguments[j] = cached;
                    } else {
                        entry.arguments.push(cached);
                    }
                }
                if let Some(r) = &entry.arguments[j].1 {
                    c.arguments.clone_from(r);
                }
            }
        }
    }
}

/// Files whose contents are credentials: reading them needs approval.
pub fn is_secret_path(p: &Path) -> bool {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if is_env_file(&name) {
        return true;
    }
    let by_name = matches!(
        name.as_str(),
        ".netrc"
            | ".npmrc"
            | ".pypirc"
            | ".git-credentials"
            | "credentials"
            | "credentials.json"
            | ".pgpass"
            | "keychain-db"
            | ".htpasswd"
    ) || name.starts_with("id_rsa") && !name.ends_with(".pub")
        || name.starts_with("id_ed25519") && !name.ends_with(".pub")
        || name.starts_with("id_ecdsa") && !name.ends_with(".pub")
        || name.starts_with("id_dsa") && !name.ends_with(".pub")
        || name.starts_with("service-account") && name.ends_with(".json")
        || [
            ".pem",
            ".key",
            ".p12",
            ".pfx",
            ".jks",
            ".keystore",
            ".kdbx",
            ".ppk",
        ]
        .iter()
        .any(|e| name.ends_with(e))
        || [
            "secrets.json",
            "secrets.yaml",
            "secrets.yml",
            "secrets.toml",
            "secret.json",
        ]
        .contains(&name.as_str());
    if by_name {
        return true;
    }
    // Lower-cased (macOS and Windows match names ignoring case), and with a
    // trailing `/` so a directory itself (`~/.aws`) counts like its contents.
    let s = format!("{}/", p.to_string_lossy().replace('\\', "/").to_lowercase());
    // Our own credentials wherever the Chanto home lives (XDG, CHANTO_HOME):
    // provider logins, the server token, MCP OAuth tokens, runner tokens.
    if is_own_credential(p) {
        return true;
    }
    [
        "/.ssh/",
        "/.aws/",
        "/.gnupg/",
        "/.config/gh/",
        "/.docker/config.json",
        "/.kube/",
        "/Library/Keychains/",
        "/.azure/",
        "/.config/gcloud/",
        "/.password-store/",
        "/.local/share/keyrings/",
        // Credential stores of agent CLIs, including our own: provider API keys,
        // OAuth access/refresh tokens.
        "/.chanto/auth.json",
        "/.claude/.credentials.json",
        "/.codex/auth.json",
    ]
    .iter()
    .any(|d| s.contains(&d.to_lowercase()))
}

fn is_own_credential(p: &Path) -> bool {
    let home = crate::config::home_dir();
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let parent = p.parent();
    let in_dir = |d: &Path| parent.is_some_and(|x| same_dir(x, d));
    (matches!(name.as_str(), "auth.json" | "server.json") && in_dir(&home))
        || (matches!(name.as_str(), "tokens.json" | "connections.json")
            && in_dir(&home.join("runner")))
        || name == "mcp-auth" && in_dir(&home)
        || in_dir(&home.join("mcp-auth"))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
}

/// A secret file or directory, judged as written and where it really leads
/// (a project file that is a link to `~/.ssh/id_ed25519` is the key), or a
/// directory that holds the usual credential stores (the home folder itself,
/// or one of its parents), since searching it reaches them.
pub fn is_secret_target(p: &Path) -> bool {
    if is_secret_path(p)
        || crate::permissions::real_path(p).is_some_and(|r| r != p && is_secret_path(&r))
    {
        return true;
    }
    let real = crate::permissions::real_path(p).unwrap_or_else(|| p.to_path_buf());
    dirs::home_dir().is_some_and(|h| h.starts_with(&real) || h.starts_with(p))
}

/// Well-known credential files a shell glob could reach (`~/.d*/config.json`).
fn known_secret_files() -> Vec<PathBuf> {
    let Some(h) = dirs::home_dir() else {
        return vec![];
    };
    [
        ".ssh/id_rsa",
        ".ssh/id_ed25519",
        ".ssh/id_ecdsa",
        ".aws/credentials",
        ".aws/config",
        ".docker/config.json",
        ".kube/config",
        ".config/gh/hosts.yml",
        ".netrc",
        ".npmrc",
        ".pypirc",
        ".pgpass",
        ".git-credentials",
        ".config/gcloud/credentials.db",
        ".chanto/auth.json",
        ".claude/.credentials.json",
        ".codex/auth.json",
    ]
    .iter()
    .map(|f| h.join(f))
    .chain(std::iter::once(crate::config::home_dir().join("auth.json")))
    .collect()
}

/// Mentions of secret files in a shell command (`cat .env`, `grep KEY ~/.aws/credentials`).
pub fn command_touches_secret_file(cmd: &str) -> Option<String> {
    let words = shell_words::split(cmd)
        .unwrap_or_else(|_| cmd.split_whitespace().map(str::to_string).collect());
    words
        .iter()
        .flat_map(|w| w.split(['=', ':', '>', '<', '(', ')', ';', '|', '&', '\'', '"', ',']))
        .filter(|w| !w.is_empty() && !w.starts_with('-'))
        .find(|w| {
            let expanded = match w.strip_prefix("~/") {
                Some(rest) => dirs::home_dir()
                    .map(|h| h.join(rest))
                    .unwrap_or_else(|| PathBuf::from(w)),
                None if *w == "~" => dirs::home_dir().unwrap_or_else(|| PathBuf::from(w)),
                None => PathBuf::from(w),
            };
            if is_secret_path(&expanded)
                || crate::permissions::real_path(&expanded).is_some_and(|r| is_secret_path(&r))
            {
                return true;
            }
            // A glob is judged by the credential files it could match.
            let text = expanded.to_string_lossy();
            text.contains(['*', '?', '['])
                && globset::GlobBuilder::new(&text)
                    .case_insensitive(true)
                    .literal_separator(true)
                    .build()
                    .is_ok_and(|g| {
                        let m = g.compile_matcher();
                        known_secret_files().iter().any(|f| m.is_match(f))
                    })
        })
        .map(str::to_string)
}

/// Org secrets given to a pulled run (`CHANTO_RUN_SECRETS`, set for the
/// worker by `chanto runner connect`): its shell commands get them, since
/// that is what they're for, and their values are masked like any secret.
/// `CHANTO_*` names never count, so the model key stays withheld.
pub fn run_secret_names() -> Vec<String> {
    std::env::var("CHANTO_RUN_SECRETS")
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|n| !n.is_empty() && !n.starts_with("CHANTO_"))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Should this variable be withheld from shell commands?
pub fn withhold_env(name: &str, cfg: &SecretsConfig) -> bool {
    // A cloud run's token for the server's run tools is meant for its
    // commands: it reaches nothing else and dies with the run.
    name != "CHANTO_RUN_TOKEN"
        && cfg.scrub_env
        && is_secret_name(name)
        && !cfg.pass_env.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => p == name,
        })
        && !run_secret_names().iter().any(|n| n == name)
}

/// The session's secrets settings, for children started without a config at hand.
static SESSION_CONFIG: RwLock<Option<SecretsConfig>> = RwLock::new(None);

/// Variables in this process's environment to withhold from a child Chanto
/// starts on the project's behalf (a language server, say), under the
/// session's settings (the defaults before a session sets them).
pub fn withheld_env_names() -> Vec<String> {
    let cfg = SESSION_CONFIG
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default();
    std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| withhold_env(k, &cfg))
        .collect()
}

#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_key_values_need_entropy_and_public_keys_are_not_secret() {
        // Withheld from commands either way, but only a random-looking value
        // is masked: `APP_KEY=my-app` is a name.
        for name in ["APP_KEY", "ENCRYPTION_KEY"] {
            assert!(is_secret_name(name));
            assert!(!secret_value(name, "ordinary-config-value"));
            assert!(secret_value(name, "aB2cD3eF4gH5iJ6kL7mN8"));
        }
        // Names of keys, not keys.
        for name in [
            "MONKEY",
            "SSH_PUBLIC_KEY",
            "service_pubkey",
            "CACHE_KEY",
            "SORT_KEY",
            "PRIMARY_KEY",
            "TURBO_KEY",
        ] {
            assert!(!is_secret_name(name));
        }
        assert!(secret_value("API_KEY", "opaque-credential"));
    }

    #[test]
    fn json_images_exclusion_does_not_hide_nested_arguments() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        with_values(&[("TEST_TOKEN", "nested-secret-value")]);
        let mut value = serde_json::json!({
            "images": ["nested-secret-value"],
            "arguments": {"images": "nested-secret-value"}
        });
        redact_json(&mut value);
        assert_eq!(value["images"][0], "nested-secret-value");
        assert_eq!(value["arguments"]["images"], "[REDACTED:TEST_TOKEN]");
    }

    #[test]
    fn registration_deduplicates_batch_and_preserves_existing_base() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        with_values(&[("BASE_TOKEN", "registration-base-value")]);
        register_all(vec![
            ("BASE_TOKEN".into(), "registration-base-value".into()),
            ("NEW_TOKEN".into(), "registration-new-value".into()),
            ("OTHER_TOKEN".into(), "registration-new-value".into()),
        ]);
        let st = state().read().unwrap();
        for value in ["registration-base-value", "registration-new-value"] {
            assert_eq!(st.base.iter().filter(|(_, v)| v == value).count(), 1);
        }
    }

    /// Warming compiles the pattern set ahead of the first prompt; what that
    /// prompt masks must be the same either way.
    #[test]
    fn warming_the_patterns_still_masks_secrets() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        with_values(&[]);
        warm(&std::env::temp_dir().join("chanto-warm-no-such-project"));
        let (out, labels) = redact_with_labels("export KEY_ID=AKIAIOSFODNN7EXAMPLE");
        assert!(out.contains("[REDACTED:AWS_ACCESS_KEY]"), "{out}");
        assert_eq!(labels, vec!["AWS_ACCESS_KEY".to_string()]);
    }

    fn with_values(vals: &[(&str, &str)]) {
        let m: Vec<(String, String)> = vals
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        *state().write().unwrap() = Arc::new(build(m, vec![], BTreeMap::new()));
        ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    #[test]
    fn run_secrets_reach_commands_and_are_masked() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = SecretsConfig::default();
        // SAFETY: TEST_LOCK serializes the tests that touch these variables.
        unsafe {
            std::env::set_var("ORG_DEPLOY_TOKEN", "deploy-token-value-1");
            std::env::set_var("ORG_STAGING_HOST", "staging-host-value-2");
            std::env::remove_var("CHANTO_RUN_SECRETS");
        }
        assert!(
            withhold_env("ORG_DEPLOY_TOKEN", &cfg),
            "secret-named: withheld by default"
        );
        unsafe {
            std::env::set_var(
                "CHANTO_RUN_SECRETS",
                "ORG_DEPLOY_TOKEN, ORG_STAGING_HOST,CHANTO_API_KEY",
            )
        };
        assert_eq!(
            run_secret_names(),
            ["ORG_DEPLOY_TOKEN", "ORG_STAGING_HOST"],
            "CHANTO_* never counts"
        );
        assert!(
            !withhold_env("ORG_DEPLOY_TOKEN", &cfg),
            "org secrets reach the run's commands"
        );
        assert!(
            withhold_env("CHANTO_API_KEY", &cfg),
            "the model key never does"
        );
        assert!(withhold_env("OTHER_API_KEY", &cfg));

        let root = std::env::temp_dir().join(format!(
            "chanto-run-secrets-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        init(&crate::config::Config::default(), &root);
        let masked = redact("deploy-token-value-1 staging-host-value-2");
        assert!(
            !masked.contains("deploy-token-value-1") && !masked.contains("staging-host-value-2"),
            "{masked}"
        );
        unsafe {
            std::env::remove_var("CHANTO_RUN_SECRETS");
            std::env::remove_var("ORG_DEPLOY_TOKEN");
            std::env::remove_var("ORG_STAGING_HOST");
        }
        with_values(&[]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn message_cache_tracks_edits_and_secret_changes() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        with_values(&[]);
        let mut cache = MessageRedactor::default();
        let mut msgs = vec![
            crate::llm::Message::user("ordinary text"),
            crate::llm::Message::user("later-known-secret"),
        ];
        let check = |cache: &mut MessageRedactor, msgs: &[crate::llm::Message]| {
            let mut expected = msgs.to_vec();
            redact_messages(&mut expected);
            let mut actual = msgs.to_vec();
            cache.redact(&mut actual);
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        };
        check(&mut cache, &msgs);
        check(&mut cache, &msgs);
        with_values(&[("NEW_KEY", "later-known-secret")]);
        check(&mut cache, &msgs);
        msgs[0].set_content("changed later-known-secret");
        msgs[0].tool_calls.push(crate::llm::ToolCall {
            id: "1".into(),
            name: "Bash".into(),
            arguments: serde_json::json!({"command": "later-known-secret"}),
        });
        check(&mut cache, &msgs);
        msgs[0].tool_calls[0].arguments = serde_json::json!({"nested": ["later-known-secret"]});
        check(&mut cache, &msgs);
        // Unchanged arguments holding a secret learned since: masked now too.
        msgs[0].tool_calls.push(crate::llm::ToolCall {
            id: "2".into(),
            name: "Read".into(),
            arguments: serde_json::json!({"file_path": "/x/second-secret-value"}),
        });
        check(&mut cache, &msgs);
        with_values(&[
            ("NEW_KEY", "later-known-secret"),
            ("SECOND", "second-secret-value"),
        ]);
        check(&mut cache, &msgs);
        msgs[0].tool_calls.truncate(1);
        check(&mut cache, &msgs);
        msgs.remove(0);
        check(&mut cache, &msgs);
        msgs.clear();
        check(&mut cache, &msgs);
        msgs.push(crate::llm::Message::user("later-known-secret"));
        ENABLED.store(false, std::sync::atomic::Ordering::Relaxed);
        check(&mut cache, &msgs);
        ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
        check(&mut cache, &msgs);
        with_values(&[]);
    }

    #[test]
    fn a_message_counts_as_unchanged_only_with_the_same_content() {
        let a = Seen::of("token = abc123");
        assert!(matches!(a, Seen::Tag(14, _)));
        assert!(a == Seen::of("token = abc123"));
        // Same length, one byte different.
        assert!(a != Seen::of("token = abc124"));
        assert!(Seen::of("") != Seen::of("\0"));
        // A same-length edit to an earlier message is scanned again.
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        with_values(&[("KEY", "secret-value-1")]);
        let mut cache = MessageRedactor::default();
        let mut msgs = vec![crate::llm::Message::user("plain-value-1 here")];
        cache.redact(&mut msgs.clone());
        msgs[0].set_content("secret-value-1 her");
        assert_eq!(msgs[0].content.len(), "plain-value-1 here".len());
        let mut sent = msgs.clone();
        cache.redact(&mut sent);
        assert!(
            !sent[0].content.contains("secret-value-1"),
            "{}",
            sent[0].content
        );
        with_values(&[]);
    }

    #[test]
    #[ignore = "manual per-call redaction benchmark"]
    fn benchmark_message_redaction() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        with_values(&[("KEY", "known-secret-value")]);
        let text =
            "A typical tool result contains source code, paths, and ordinary prose.\n".repeat(32);
        let msgs: Vec<_> = (0..500)
            .map(|_| crate::llm::Message::user(text.clone()))
            .collect();
        let mut cache = MessageRedactor::default();
        cache.redact(&mut msgs.clone());
        let start = std::time::Instant::now();
        for _ in 0..30 {
            let mut copy = msgs.clone();
            redact_messages(&mut copy);
            std::hint::black_box(copy);
        }
        let regular = start.elapsed();
        let start = std::time::Instant::now();
        for _ in 0..30 {
            let mut copy = msgs.clone();
            cache.redact(&mut copy);
            std::hint::black_box(copy);
        }
        eprintln!(
            "500 messages, {} bytes, clone + redact per call: regular {:.3} ms, cached {:.3} ms",
            text.len() * msgs.len(),
            regular.as_secs_f64() * 1000.0 / 30.0,
            start.elapsed().as_secs_f64() * 1000.0 / 30.0
        );
        with_values(&[]);
    }

    #[test]
    fn masks_known_values_and_their_encodings() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        with_values(&[
            ("STRIPE_KEY", "rk_live_51Habcdefghijklmnop"),
            ("DB_PASSWORD", "hunter2-correct-horse"),
        ]);
        let b64 = base64::engine::general_purpose::STANDARD.encode("hunter2-correct-horse");
        let text = format!("key rk_live_51Habcdefghijklmnop and {b64}");
        let (out, labels) = redact_with_labels(&text);
        assert!(
            !out.contains("rk_live_51H") && !out.contains(b64.trim_end_matches('=')),
            "{out}"
        );
        assert!(
            out.contains("[REDACTED:STRIPE_KEY]") && out.contains("[REDACTED:DB_PASSWORD]"),
            "{out}"
        );
        assert_eq!(labels, vec!["DB_PASSWORD", "STRIPE_KEY"]);
        with_values(&[]);
    }

    #[test]
    fn a_secret_debug_prints_masked_and_serializes_plain() {
        let cred = crate::auth::Credential::Oauth {
            access: "at-123".into(),
            refresh: "rt-456".into(),
            expires: 1,
            client_id: "app".into(),
        };
        let shown = format!("{cred:?}");
        assert!(shown.contains("Secret(***)") && shown.contains("app"));
        assert!(!shown.contains("at-123") && !shown.contains("rt-456"));
        assert_eq!(format!("{:?}", Secret::from("")), "Secret(\"\")");
        // `auth.json` and the keychain keep the plain value.
        let json = serde_json::to_value(&cred).unwrap();
        assert_eq!(json["access"], "at-123");
        let back: crate::auth::Credential = serde_json::from_value(json).unwrap();
        let crate::auth::Credential::Oauth { refresh, .. } = back else {
            panic!("not an OAuth credential");
        };
        assert_eq!(refresh.expose(), "rt-456");
    }

    #[test]
    fn learned_values_survive_init_and_refresh() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = |n: &str| {
            let d = std::env::temp_dir().join(format!(
                "chanto-learned-{n}-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&d).unwrap();
            d
        };
        let (worktree, source) = (tmp("worktree"), tmp("source"));
        std::fs::write(
            source.join(".env"),
            "DB_PASSWORD=source-only-pw-7c6b5a
",
        )
        .unwrap();
        let cfg = crate::config::Config::default();
        init(&cfg, &worktree);
        register("SIGNIN_TOKEN", "learned-token-5f4e3d2c1b");
        register("SHORT", "abc");
        add_project(&source);
        // Starting over (a worker's setup) and rescanning the worktree keep them.
        init(&cfg, &worktree);
        refresh(&worktree);
        let out = redact("learned-token-5f4e3d2c1b source-only-pw-7c6b5a abc");
        assert_eq!(
            out, "[REDACTED:SIGNIN_TOKEN] [REDACTED:DB_PASSWORD] abc",
            "too short to mask safely: abc"
        );
        with_values(&[]);
        let _ = std::fs::remove_dir_all(&worktree);
        let _ = std::fs::remove_dir_all(&source);
    }

    #[test]
    fn plain_key_names_are_secret() {
        for name in ["STRIPE_KEY", "ENCRYPTION_KEY", "master_key", "KEY"] {
            assert!(is_secret_name(name), "{name}");
        }
        for name in [
            "MONKEY",
            "KEYBOARD",
            "TURKEY_COUNT",
            "SSH_KEY_PATH",
            "KEYCHAIN",
            "CACHE_KEY",
            "SORT_KEY",
            "PRIMARY_KEY",
            "SSH_PUBLIC_KEY",
            "GPG_PUBKEY",
        ] {
            assert!(!is_secret_name(name), "{name}");
        }
    }

    #[test]
    fn requests_and_events_are_masked_but_images_kept() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        with_values(&[("DEPLOY_TOKEN", "event-secret-9a8b7c6d")]);
        let clean = vec![crate::llm::Message::user("nothing here")];
        assert!(redact_request_messages(&clean).is_none());
        let mut call = crate::llm::Message::assistant("");
        call.tool_calls.push(crate::llm::ToolCall {
            id: "1".into(),
            name: "Bash".into(),
            arguments: serde_json::json!({"command": "echo event-secret-9a8b7c6d"}),
        });
        let msgs = vec![clean[0].clone(), call];
        let masked = redact_request_messages(&msgs).unwrap();
        assert_eq!(*masked[0].content, "nothing here");
        assert_eq!(
            masked[1].tool_calls[0].arguments["command"],
            "echo [REDACTED:DEPLOY_TOKEN]"
        );
        // Masking what's already masked changes nothing (a stable prefix).
        assert!(redact_request_messages(&masked).is_none());

        let mut ev = serde_json::json!({"type": "tool_end", "output": "event-secret-9a8b7c6d",
            "nested": [{"label": "x event-secret-9a8b7c6d"}], "images": ["event-secret-9a8b7c6d"]});
        redact_json(&mut ev);
        assert_eq!(ev["output"], "[REDACTED:DEPLOY_TOKEN]");
        assert_eq!(ev["nested"][0]["label"], "x [REDACTED:DEPLOY_TOKEN]");
        assert_eq!(ev["images"][0], "event-secret-9a8b7c6d");
        with_values(&[]);
    }

    #[test]
    fn masks_token_formats_but_not_ordinary_code() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        with_values(&[]);
        // A failure names the format; the sample itself stays out of the log.
        for (format, sample) in [
            ("AWS access key", "AKIAIOSFODNN7EXAMPLE"),
            ("GitHub token", "ghp_0123456789abcdefghijklmnopqrstuvwxyzAB"),
            ("Slack token", "xoxb-1234567890-abcdefghij"),
            // Formats whose last character can be `-`.
            ("Slack token ending in -", "xoxb-123456789-"),
            ("GitLab token ending in -", "glpat-abcdefghij012345678-"),
            (
                "Google API key ending in -",
                // Split so secret scanners don't take the fake for a real key.
                concat!("AIza", "SyA0123456789abcdefghijklmnopqrstu-"),
            ),
            (
                "SendGrid key ending in -",
                "SG.abcdefghij0123456789ab.abcdefghij0123456789abcdefghij0123456789ab-",
            ),
            ("Anthropic key", "sk-ant-api03-abcdefghijklmnopqrstuv"),
            (
                "JWT",
                "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
            ),
            (
                "OpenSSH private key",
                "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----",
            ),
        ] {
            assert!(
                redact(sample).starts_with("[REDACTED:"),
                "{format} not redacted"
            );
        }
        assert_eq!(
            redact("postgres://app:s3cretPass@db:5432/x"),
            "postgres://app:[REDACTED:URL_PASSWORD]@db:5432/x"
        );
        assert_eq!(
            redact("API_KEY=Zm9vYmFyMTIzNDU2Nzg5MGFiY2Rl"),
            "API_KEY=[REDACTED:API_KEY]"
        );
        for fine in [
            "const token = getToken();",
            "PASSWORD_MIN_LENGTH=12",
            "api_key = os.environ[\"API_KEY\"]",
            "let secret_name = \"database-password\";",
            "git commit -m 'Rotate tokens' && cargo test",
            "SESSION_TIMEOUT=3600",
        ] {
            assert_eq!(redact(fine), fine);
        }
    }

    #[test]
    fn secret_paths_ignore_case_trailing_slashes_links_and_globs() {
        let home = dirs::home_dir().unwrap();
        // Directories themselves, any case.
        for p in [
            "/Users/a/.aws",
            "/Users/a/.SSH/id_rsa",
            "/Users/a/.Kube/config",
            "/Users/a/.GNUPG",
        ] {
            assert!(is_secret_path(Path::new(p)), "{p}");
        }
        // Searching the home folder (or above) reaches every credential store.
        assert!(is_secret_target(&home));
        // An ancestor of the home directory also reaches its credential stores.
        if let Some(parent) = home.parent() {
            assert!(is_secret_target(parent), "{}", parent.display());
        }
        assert!(!is_secret_target(&home.join("projects/app/src")));
        // A project file that links to a key is the key.
        let d =
            std::env::temp_dir().join(format!("chanto-secret-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(d.join(".ssh")).unwrap();
        std::fs::write(d.join(".ssh/id_ed25519"), "k").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(d.join(".ssh/id_ed25519"), d.join("notes.txt")).unwrap();
            assert!(is_secret_target(&d.join("notes.txt")));
        }
        assert!(!is_secret_target(&d.join("README.md")));
        let _ = std::fs::remove_dir_all(&d);
        // Shell globs are judged by the credential files they could match.
        for c in [
            "cat ~/.d*/config.json",
            "cat ~/.ss?/id_*",
            "grep -r . ~/.kube",
            "cat ~/.AWS/credentials",
        ] {
            assert!(command_touches_secret_file(c).is_some(), "{c}");
        }
        for c in ["cat src/*.rs", "ls ~/projects/*", "grep -r TODO ."] {
            assert_eq!(command_touches_secret_file(c), None, "{c}");
        }
        // Our own credentials wherever the Chanto home is.
        assert!(is_secret_path(&crate::config::home_dir().join("auth.json")));
    }

    #[test]
    fn env_files_and_secret_paths() {
        let vals = env_file_values(
            "# comment\nexport API_KEY=\"abc123def456ghi7\"\nPORT=3000\nNODE_ENV=production\nSESSION=Q2hhbmdlTWVQbGVhc2UxMjM0NTY3OA\nDEBUG=true\n",
        );
        let keys: Vec<&str> = vals.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["API_KEY", "SESSION"]);
        for p in [
            ".env",
            "app/.env.local",
            ".dev.vars",
            "certs/server.key",
            "/Users/a/.ssh/id_ed25519",
            "/Users/a/.aws/credentials",
            "x/service-account-prod.json",
        ] {
            assert!(is_secret_path(Path::new(p)), "{p}");
        }
        for p in [
            ".env.example",
            "src/env.ts",
            "id_ed25519.pub",
            "keys.rs",
            "README.md",
            ".envrc.sample",
        ] {
            assert!(!is_secret_path(Path::new(p)), "{p}");
        }
        assert_eq!(
            command_touches_secret_file("cat .env | grep KEY").as_deref(),
            Some(".env")
        );
        assert!(command_touches_secret_file("grep -n token ~/.aws/credentials").is_some());
        assert!(command_touches_secret_file("cp .env.example .env").is_some());
        assert!(command_touches_secret_file("cargo test && cat .env.example").is_none());
        let cfg = SecretsConfig {
            pass_env: vec!["GH_TOKEN".into(), "AWS_*".into()],
            ..Default::default()
        };
        assert!(
            withhold_env("OPENAI_API_KEY", &cfg)
                && !withhold_env("GH_TOKEN", &cfg)
                && !withhold_env("AWS_SECRET_ACCESS_KEY", &cfg)
                && !withhold_env("PATH", &cfg)
        );
        // A cloud run's commands get its run-tools token, never its model key.
        let cfg = SecretsConfig::default();
        assert!(!withhold_env("CHANTO_RUN_TOKEN", &cfg) && withhold_env("CHANTO_API_KEY", &cfg));
        // Chanto's own token files, wherever its home is.
        let home = crate::config::home_dir();
        for f in [
            "auth.json",
            "server.json",
            "mcp-auth",
            "mcp-auth/github-0123456789abcdef.json",
            "runner/tokens.json",
            "runner/connections.json",
        ] {
            assert!(is_secret_path(&home.join(f)), "{f}");
        }
        assert!(!is_secret_path(&home.join("config.toml")));
        assert!(!is_secret_path(&home.join("runs/abc/run.json")));
    }
}
