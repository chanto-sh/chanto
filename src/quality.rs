//! The quality profile: `.chanto/quality.toml`, one committed file that says
//! what "correct" means in this repository, enforced by verified mode.
//!
//! - `[gate]` takes the `[guardrails]` settings (`enabled`, `scope`,
//!   `oracle`, …).
//! - `[[check]]` entries are the gate's commands; `when = "nightly"` or
//!   `"off"` keeps one out of the gate (CI can still run it).
//! - `[[rule]]` entries with a `regex` are pattern checks, run by
//!   `chanto quality check` on the lines a change adds (or on whole files,
//!   with `new_code_only = false`). The same command scans those lines for
//!   secrets unless `[secrets] scan = false`. The gate runs it too.
//! - `[review] no_touch` paths become `Edit(...)` deny rules.
//!
//! The profile is read as a project layer just below `.chanto/config.toml`,
//! so the same trust rules apply: in an untrusted folder its gate, checks
//! and denies are skipped like any project `[guardrails]` section.
//! `chanto prime` writes it (see [`prime`]).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

mod hook;
mod pattern;
mod prime;

pub use pattern::cli as check_cli;
pub use prime::cli as prime_cli;

/// Where the profile lives in a project.
pub fn path(root: &Path) -> PathBuf {
    root.join(".chanto").join("quality.toml")
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub version: u32,
    /// `[guardrails]` keys, passed through.
    pub gate: toml::Table,
    #[serde(rename = "check")]
    pub checks: Vec<Check>,
    #[serde(rename = "rule")]
    pub rules: Vec<Rule>,
    pub review: Review,
    pub secrets: SecretsScan,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Check {
    pub name: String,
    pub run: String,
    /// test | lint | typecheck | format | build (informational).
    pub kind: Option<String>,
    pub when: When,
    /// What the check did when the profile was written: passing | failing.
    pub baseline: Option<String>,
    /// How long that run took, in seconds.
    pub secs: Option<f32>,
    /// detected | asked | learned | user.
    pub source: Option<String>,
}

/// When a check runs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum When {
    /// In the done-gate, every turn that changed the project.
    #[default]
    Turn,
    /// Not in the gate: too slow for every turn; for scheduled CI.
    Nightly,
    /// Not run (for example, failing when the profile was written).
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Rule {
    pub id: String,
    pub title: String,
    /// A line matching this regex breaks the rule.
    pub regex: Option<String>,
    /// Globs (relative to the project root) the rule applies to; empty: all files.
    pub paths: Vec<String>,
    pub exclude: Vec<String>,
    /// Check only the lines a change adds (default), not existing code.
    pub new_code_only: bool,
    pub source: Option<String>,
}

impl Default for Rule {
    fn default() -> Self {
        Rule {
            id: String::new(),
            title: String::new(),
            regex: None,
            paths: Vec::new(),
            exclude: Vec::new(),
            new_code_only: true,
            source: None,
        }
    }
}

/// The built-in secrets scan in `chanto quality check`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SecretsScan {
    /// On by default: a profile has to turn it off.
    pub scan: bool,
    /// Globs of files not scanned (test fixtures with fake keys).
    pub exclude: Vec<String>,
}

impl Default for SecretsScan {
    fn default() -> Self {
        SecretsScan {
            scan: true,
            exclude: Vec::new(),
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Review {
    /// Paths the agent must not edit.
    pub no_touch: Vec<String>,
}

/// The `[gate]` keys that map onto `[guardrails]`.
const GATE_KEYS: [&str; 6] = [
    "enabled",
    "scope",
    "oracle",
    "protect_tests",
    "max_attempts",
    "timeout_secs",
];

pub fn parse(text: &str) -> Result<Profile> {
    Ok(toml::from_str(text)?)
}

pub fn load(root: &Path) -> Result<Option<Profile>> {
    let p = path(root);
    match std::fs::read_to_string(&p) {
        Ok(text) => parse(&text)
            .map(Some)
            .with_context(|| format!("parsing {}", p.display())),
        Err(_) => Ok(None),
    }
}

/// The profile in `root` as a config layer (source name, table), if there is one.
pub fn layer(root: &Path) -> Result<Option<(String, toml::Table)>> {
    let Some(profile) = load(root)? else {
        return Ok(None);
    };
    Ok(Some((
        path(root).display().to_string(),
        to_table(&profile, self_check_command().as_deref()),
    )))
}

/// The config table a profile stands for. `rule_check` is the command that
/// runs its pattern rules (`chanto quality check`).
pub fn to_table(profile: &Profile, rule_check: Option<&str>) -> toml::Table {
    let mut g = toml::Table::new();
    for k in GATE_KEYS {
        if let Some(v) = profile.gate.get(k) {
            g.insert(k.into(), v.clone());
        }
    }
    let checks: Vec<toml::Value> = profile
        .checks
        .iter()
        .filter(|c| c.when == When::Turn && !c.run.trim().is_empty())
        .map(|c| toml::Value::String(c.run.clone()))
        .collect();
    // No checks listed: the gate keeps detecting them.
    if !checks.is_empty() {
        g.insert("checks".into(), toml::Value::Array(checks));
    }
    if let Some(cmd) = rule_check
        && (profile.secrets.scan || profile.rules.iter().any(|r| r.regex.is_some()))
    {
        g.insert(
            "quality_checks".into(),
            toml::Value::Array(vec![toml::Value::String(cmd.into())]),
        );
    }
    let mut t = toml::Table::new();
    if !g.is_empty() {
        t.insert("guardrails".into(), toml::Value::Table(g));
    }
    if !profile.review.no_touch.is_empty() {
        let deny = profile
            .review
            .no_touch
            .iter()
            .map(|p| toml::Value::String(format!("Edit({p})")))
            .collect();
        let mut perm = toml::Table::new();
        perm.insert("deny".into(), toml::Value::Array(deny));
        t.insert("permissions".into(), toml::Value::Table(perm));
    }
    t
}

/// `<this chanto> quality check`, quoted for the shell checks run in.
fn self_check_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.to_str()?;
    Some(format!("{} quality check", quote_program(exe)))
}

fn quote_program(p: &str) -> String {
    use crate::shell::Flavor;
    match crate::shell::flavor() {
        Flavor::Posix => shell_words::quote(p).into_owned(),
        Flavor::PowerShell => format!("& {}", crate::shell::powershell_quote(p)),
        // Windows paths can't contain `"`.
        Flavor::Cmd => format!("\"{p}\""),
    }
}

#[cfg(test)]
#[path = "quality_tests.rs"]
mod tests;
