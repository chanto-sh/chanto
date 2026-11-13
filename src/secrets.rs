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

/// Names of variables (and `.env` keys) that hold secrets: any of these
/// words anywhere in the name, ignoring case. Plain substring checks rather
/// than a regex, since this runs for every environment variable at startup
/// and compiling a case-insensitive Unicode regex costs more than all of them.
fn strong_secret_name(name: &str) -> bool {
    const WORDS: &[&str] = &[
        "secret",
        "token",
        "password",
        "passwd",
        "pwd",
        "api_key",
        "apikey",
        "access_key",
        "accesskey",
        "private_key",
        "privatekey",
        "credential",
        "auth",
        "cookie",
        "session_key",
        "sessionkey",
        "signing_key",
        "signingkey",
        "webhook",
        "dsn",
        "database_url",
        "conn_str",
        "connstr",
        "connection_str",
        "connectionstr",
    ];
    // Case-insensitive matching folds the long s and the Kelvin sign to `s`
    // and `k`; fold them the same way so neither slips a name past.
    let lower = name.to_lowercase().replace('\u{17f}', "s");
    WORDS.iter().any(|w| lower.contains(w))
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
/// files, and the literals that decide which token patterns to run. Each
/// pattern itself compiles the first time a text could match it.
pub fn warm(root: &Path) {
    let _ = candidates("");
    refresh(root);
}

fn project_env_files(root: &Path) -> Vec<PathBuf> {
    let mtime = |d: &Path| std::fs::metadata(d).and_then(|m| m.modified()).ok();
    // Held through the walk: the first turn's refresh waits for the background
    // one and reuses its result instead of walking the project a second time.
    let mut last = LAST_WALK.lock().unwrap();
    if let Some((r, dirs, files)) = last.as_ref()
        && r == root
        && dirs.iter().all(|(d, t)| mtime(d) == *t)
    {
        return files.clone();
    }
    let (seen, out) = walk_env_files(root);
    *last = Some((root.to_path_buf(), seen, out.clone()));
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
    init_known(cfg);
    refresh(root);
}

/// Collect the secret values that need no walk of the project: environment and
/// Chanto's config. [`refresh`] adds the project's env files.
pub fn init_known(cfg: &crate::config::Config) {
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

/// Token formats worth masking wherever they appear: (label, regex, group to
/// mask, literals). A token whose alphabet has `-` can end in one, where `\b`
/// can't match, so those end at the first character outside the alphabet
/// instead.
///
/// The literals are what a match can't do without, compared ignoring ASCII
/// case: text with none of them can't match, so the regex isn't run (or even
/// compiled). `ASSIGNMENT` has its own entry, the last one.
const PATTERNS: [(&str, &str, usize, &[&str]); 20] = [
    (
        "PRIVATE_KEY",
        r"-----BEGIN [A-Z ]*PRIVATE KEY( BLOCK)?-----[\s\S]*?(-----END [A-Z ]*PRIVATE KEY( BLOCK)?-----|$)",
        0,
        &["-----BEGIN "],
    ),
    (
        "AWS_ACCESS_KEY",
        r"\b(AKIA|ASIA)[0-9A-Z]{16}\b",
        0,
        &["AKIA", "ASIA"],
    ),
    (
        "AWS_SECRET",
        r#"(?i)aws_?secret_?access_?key["']?\s*[:=]\s*["']?([A-Za-z0-9/+=]{40})"#,
        1,
        &["aws"],
    ),
    (
        "GITHUB_TOKEN",
        r"\b(gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{50,})\b",
        0,
        &["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"],
    ),
    (
        "GITLAB_TOKEN",
        r"\bglpat-[A-Za-z0-9_-]{20,}",
        0,
        &["glpat-"],
    ),
    (
        "SLACK_TOKEN",
        r"\bxox[abposr]-[A-Za-z0-9-]{10,}",
        0,
        &["xox"],
    ),
    (
        "SLACK_WEBHOOK",
        r"https://hooks\.slack\.com/services/[A-Za-z0-9/]+",
        0,
        &["hooks.slack.com/services/"],
    ),
    (
        "STRIPE_KEY",
        r"\b(sk|rk)_(live|test)_[A-Za-z0-9]{16,}\b",
        0,
        &["_live_", "_test_"],
    ),
    (
        "ANTHROPIC_KEY",
        r"\bsk-ant-[A-Za-z0-9_-]{20,}",
        0,
        &["sk-ant-"],
    ),
    (
        "OPENAI_KEY",
        r"\bsk-(proj-|svcacct-)?[A-Za-z0-9_-]{32,}",
        0,
        &["sk-"],
    ),
    (
        "GOOGLE_API_KEY",
        r"\b(AIza[0-9A-Za-z_-]{35})(?:[^0-9A-Za-z_-]|$)",
        1,
        &["AIza"],
    ),
    ("NPM_TOKEN", r"\bnpm_[A-Za-z0-9]{36}\b", 0, &["npm_"]),
    (
        "PYPI_TOKEN",
        r"\bpypi-AgE[A-Za-z0-9_-]{50,}",
        0,
        &["pypi-AgE"],
    ),
    ("HF_TOKEN", r"\bhf_[A-Za-z]{34,}\b", 0, &["hf_"]),
    (
        "SENDGRID_KEY",
        r"\b(SG\.[A-Za-z0-9_-]{22}\.[A-Za-z0-9_-]{43})(?:[^A-Za-z0-9_-]|$)",
        1,
        &["SG."],
    ),
    (
        "DIGITALOCEAN_TOKEN",
        r"\bdop_v1_[a-f0-9]{64}\b",
        0,
        &["dop_v1_"],
    ),
    (
        "JWT",
        r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
        0,
        &["eyJ"],
    ),
    (
        "URL_PASSWORD",
        r"\b[a-z][a-z0-9+.-]*://[^\s:/@]+:([^\s@/]{3,})@",
        1,
        &["://"],
    ),
    (
        "BEARER_TOKEN",
        r"(?i)\bbearer\s+([A-Za-z0-9._~+/-]{20,}=*)",
        1,
        &["bearer"],
    ),
    // `NAME_SECRET=value` / `"api_key": "value"` with a credential-looking value.
    (
        "ASSIGNMENT",
        r#"(?i)\b([A-Z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|API_?KEY|ACCESS_?KEY|PRIVATE_?KEY)[A-Z0-9_]*)["']?\s*[=:]\s*["']?([A-Za-z0-9/+_.=-]{16,})"#,
        0,
        &[
            "secret",
            "token",
            "passw",
            "apikey",
            "api_key",
            "accesskey",
            "access_key",
            "privatekey",
            "private_key",
        ],
    ),
];
/// How many of `PATTERNS` are token formats; `ASSIGNMENT` follows them.
const TOKENS: usize = 19;
const ASSIGNMENT: usize = TOKENS;

/// Pattern `i`, compiled the first time it's needed. Unless `text` holds a
/// character that case-insensitive matching folds into ASCII (`ſ`, the Kelvin
/// sign), its word boundaries are ASCII ones. Every `\b` in `PATTERNS` sits
/// next to an ASCII word character, where an ASCII boundary is found wherever
/// a Unicode one is, and also between it and a non-ASCII letter (so a token
/// glued to `é` is masked too); on ASCII text the two are identical. A
/// Unicode `\b` also stops the regex's DFAs on any non-ASCII text (a single
/// em dash), leaving the slowest engine to scan the whole message.
fn pattern(i: usize, text: &str) -> &'static Regex {
    static ASCII: [OnceLock<Regex>; PATTERNS.len()] = [const { OnceLock::new() }; PATTERNS.len()];
    static UNICODE: [OnceLock<Regex>; PATTERNS.len()] = [const { OnceLock::new() }; PATTERNS.len()];
    #[cfg(test)]
    let text = if tests::UNICODE_BOUNDARIES.with(|u| u.get()) {
        "\u{17f}"
    } else {
        text
    };
    if folds_to_ascii(text) {
        UNICODE[i].get_or_init(|| Regex::new(PATTERNS[i].1).unwrap())
    } else {
        ASCII[i].get_or_init(|| Regex::new(&PATTERNS[i].1.replace(r"\b", r"(?-u:\b)")).unwrap())
    }
}

/// Whether `s` has a character that case-insensitive matching folds into
/// ASCII: `ſ` (to `s`) or the Kelvin sign (to `k`). Substring searches, which
/// skip through the text, rather than a scan that decodes every character:
/// this runs for each pattern on every message.
fn folds_to_ascii(s: &str) -> bool {
    s.contains('\u{17f}') || s.contains('\u{212a}')
}

/// Which of `PATTERNS` could match `s`: those whose literals appear in it.
fn candidates(s: &str) -> [bool; PATTERNS.len()] {
    static AC: OnceLock<(AhoCorasick, Vec<usize>)> = OnceLock::new();
    let (ac, owner) = AC.get_or_init(|| {
        let mut lits = Vec::new();
        let mut owner = Vec::new();
        for (i, (_, _, _, l)) in PATTERNS.iter().enumerate() {
            for lit in l.iter() {
                lits.push(*lit);
                owner.push(i);
            }
        }
        let ac = AhoCorasick::builder()
            .ascii_case_insensitive(true)
            .build(&lits)
            .unwrap();
        (ac, owner)
    });
    let mut hit = [false; PATTERNS.len()];
    #[cfg(test)]
    if tests::UNFILTERED.with(|u| u.get()) {
        return [true; PATTERNS.len()];
    }
    // Case-insensitive regexes fold `ſ` to `s` and the Kelvin sign to `k`,
    // which no ASCII comparison sees: text with either runs every pattern.
    if folds_to_ascii(s) {
        return [true; PATTERNS.len()];
    }
    for m in ac.find_overlapping_iter(s) {
        let i = owner[m.pattern().as_usize()];
        // A keyword or `://` alone is common in prose (the system prompt
        // talks about secrets and tokens); these patterns also need what
        // must follow it, checked here so their regexes, which are the
        // costliest to compile, aren't built for text they can't match.
        hit[i] |= match PATTERNS[i].0 {
            "AWS_SECRET" | "ASSIGNMENT" => assignment_follows(&s[m.end()..]),
            "URL_PASSWORD" => password_follows(&s[m.end()..]),
            _ => true,
        };
    }
    hit
}

/// Whether `rest`, just after a credential keyword, can go on as
/// `ASSIGNMENT` and `AWS_SECRET` require: more of the name, an optional
/// quote, then `=` or `:`.
fn assignment_follows(rest: &str) -> bool {
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '_');
    let rest = rest.strip_prefix(['"', '\'']).unwrap_or(rest);
    rest.trim_start().starts_with(['=', ':'])
}

/// Whether `rest`, just after `://`, starts with `user:password@` as
/// `URL_PASSWORD` requires.
fn password_follows(rest: &str) -> bool {
    let user_end = rest.find(|c: char| c.is_whitespace() || matches!(c, ':' | '/' | '@'));
    let Some(pw) = user_end
        .filter(|&i| i > 0 && rest[i..].starts_with(':'))
        .map(|i| &rest[i + 1..])
    else {
        return false;
    };
    pw.find(|c: char| c.is_whitespace() || matches!(c, '@' | '/'))
        .is_some_and(|j| pw[j..].starts_with('@') && pw[..j].chars().count() >= 3)
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
    // Which patterns could match; worked out again whenever a replacement
    // changes the text, since later patterns run on the result.
    let mut hit = candidates(out.as_ref());
    for (i, &(label, _, group, _)) in PATTERNS[..TOKENS].iter().enumerate() {
        if !hit[i] {
            continue;
        }
        let re = pattern(i, out.as_ref());
        if re.is_match(out.as_ref()) {
            let replaced = re
                .replace_all(out.as_ref(), |c: &regex::Captures| {
                    labels.push(label.to_string());
                    let whole = c.get(0).unwrap();
                    match c.get(group).filter(|_| group > 0) {
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
            hit = candidates(out.as_ref());
        }
    }
    // Fetched only when it could match: compiling it is the costliest step.
    let re = || pattern(ASSIGNMENT, out.as_ref());
    if hit[ASSIGNMENT] && re().is_match(out.as_ref()) {
        let re = re();
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
    let hit = candidates(s);
    r.ac.as_ref().is_some_and(|ac| ac.is_match(s))
        || (0..TOKENS).any(|i| hit[i] && pattern(i, s).is_match(s))
        || (hit[ASSIGNMENT]
            && pattern(ASSIGNMENT, s)
                .captures_iter(s)
                .any(|c| high_entropy(&c[2])))
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

/// Reuse redaction for unchanged messages. Each entry holds a reference to
/// the content it was made from (an `Arc`, not a copy). While it does, that
/// text can't change: an edit makes a new allocation, or copies one that's
/// shared. So a message whose content is the same allocation is unchanged,
/// at no cost per turn, and no input can be crafted to look unchanged; a
/// different allocation counts as unchanged only when its text is equal.
/// Tool-call arguments are small, so they're compared with their exact
/// originals.
#[derive(Default)]
pub struct MessageRedactor {
    state: Option<Arc<Redactor>>,
    contents: Vec<Redacted>,
}

/// One message as last sent: its content, and its content and tool-call
/// arguments redacted, when that differs.
struct Redacted {
    content: Arc<String>,
    redacted: Option<Arc<String>>,
    arguments: Vec<(serde_json::Value, Option<serde_json::Value>)>,
}

impl Redacted {
    fn holds(&self, content: &Arc<String>) -> bool {
        Arc::ptr_eq(&self.content, content) || self.content == *content
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
            if self.contents.get(i).is_none_or(|e| !e.holds(&m.content)) {
                let redacted = match redact(&m.content) {
                    Cow::Owned(s) => Some(Arc::new(s)),
                    Cow::Borrowed(_) => None,
                };
                let entry = Redacted {
                    content: m.content.clone(),
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
                m.content = s.clone();
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
mod tests;
