//! Which MCP servers may run, as Claude Code decides it:
//!
//! - organization policy: `allowedMcpServers` / `deniedMcpServers` (entries
//!   matching `serverName`, `serverCommand` or `serverUrl`), with
//!   `allowManagedMcpServersOnly` making the managed allowlist the only one;
//! - per-project toggles from `/mcp enable|disable` (`disabledMcpServers`);
//! - approval of a project's `.mcp.json` servers (`enabledMcpjsonServers`,
//!   `disabledMcpjsonServers`, `enableAllProjectMcpServers`).
//!
//! Chanto keeps its own per-project choices in `~/.chanto/mcp-projects.json`
//! (outside the repo, so a cloned project can't approve its own servers) and
//! also reads Claude Code's: managed settings, `~/.claude/settings.json`,
//! the project's `.claude/settings*.json` and `~/.claude.json`.

use crate::config::{CompatConfig, McpServerConfig};
use crate::util::Canonical as _;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Why a configured server is kept from starting.
#[derive(Debug, Clone, PartialEq)]
pub enum Gate {
    /// Turned off for this project with `/mcp disable`.
    Disabled,
    /// Turned off for this project in Claude Code (`~/.claude.json`).
    DisabledInClaude,
    /// Blocked by `allowedMcpServers` / `deniedMcpServers`.
    Policy(String),
}

impl Gate {
    pub fn describe(&self) -> String {
        match self {
            Gate::Disabled => "disabled for this project (`/mcp enable` turns it back on)".into(),
            Gate::DisabledInClaude => {
                "disabled for this project in Claude Code (re-enable it from Claude Code's /mcp)"
                    .into()
            }
            Gate::Policy(why) => why.clone(),
        }
    }
}

// ---------------------------------------------------------------- allow/deny lists

/// One `allowedMcpServers` / `deniedMcpServers` entry.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Name(String),
    Command(Vec<String>),
    Url(String),
}

impl Entry {
    pub fn parse(v: &Json) -> Option<Entry> {
        if let Some(n) = v.get("serverName").and_then(|n| n.as_str()) {
            return Some(Entry::Name(n.to_string()));
        }
        if let Some(c) = v.get("serverCommand").and_then(|c| c.as_array()) {
            let c: Vec<String> = c
                .iter()
                .filter_map(|a| a.as_str().map(str::to_string))
                .collect();
            return (!c.is_empty()).then_some(Entry::Command(c));
        }
        v.get("serverUrl")
            .and_then(|u| u.as_str())
            .map(|u| Entry::Url(u.to_string()))
    }

    fn matches(&self, name: &str, cfg: &McpServerConfig) -> bool {
        match self {
            Entry::Name(n) => n == name,
            Entry::Command(want) => {
                if cfg.url.is_some() || cfg.command.is_empty() {
                    return false;
                }
                let have: Vec<String> = std::iter::once(&cfg.command)
                    .chain(&cfg.args)
                    .cloned()
                    .collect();
                let ex = |v: &[String]| v.iter().map(|a| expand(a)).collect::<Vec<_>>();
                have == *want || ex(&have) == ex(want)
            }
            Entry::Url(pattern) => cfg
                .url
                .as_deref()
                .is_some_and(|u| url_matches(&expand(pattern), &expand(u))),
        }
    }
}

/// `${VAR}` and `${VAR:-default}` from the environment; unknown ones stay as written.
fn expand(s: &str) -> String {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}").unwrap()
    });
    RE.replace_all(s, |c: &regex::Captures| {
        std::env::var(&c[1])
            .ok()
            .or_else(|| c.get(2).map(|d| d.as_str().to_string()))
            .unwrap_or_else(|| c[0].to_string())
    })
    .into_owned()
}

/// `*` matches any run of characters (including none).
fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !text.starts_with(first) || text.len() < first.len() + last.len() || !text.ends_with(last) {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    for p in &parts[1..parts.len() - 1] {
        match rest.find(p) {
            Some(i) => rest = &rest[i + p.len()..],
            None => return false,
        }
    }
    true
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "https" | "wss" => Some(443),
        "http" | "ws" => Some(80),
        _ => None,
    }
}

/// Claude Code's `serverUrl` matching: `*` wildcards anywhere; hosts compare
/// case-insensitively without a trailing dot; with no port in the pattern, a
/// host written in full matches only the scheme's default port, a host with a
/// `*` any port; with no path, any path.
pub fn url_matches(pattern: &str, url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some((scheme, rest)) = pattern.split_once("://") else {
        return false;
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/*"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !p.contains(']') => (h, Some(p)),
        _ => (authority, None),
    };
    if !glob(scheme, u.scheme()) {
        return false;
    }
    let want_host = host.trim_end_matches('.').to_ascii_lowercase();
    let have_host = u
        .host_str()
        .unwrap_or("")
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if !glob(&want_host, &have_host) {
        return false;
    }
    let have_port = u.port_or_known_default();
    let port_ok = match port {
        Some("*") => true,
        Some(p) => p.parse::<u16>().ok() == have_port,
        None if host.contains('*') => true,
        None => have_port == default_port(u.scheme()),
    };
    let mut have_path = u.path().to_string();
    if let Some(q) = u.query() {
        have_path = format!("{have_path}?{q}");
    }
    port_ok && (glob(path, &have_path) || glob(path, u.path()))
}

/// The merged allowlist and denylist.
#[derive(Debug, Default, Clone)]
pub struct Policy {
    /// `None`: no allowlist anywhere, so everything not denied may run.
    pub allowed: Option<Vec<Entry>>,
    pub denied: Vec<Entry>,
}

fn entries(v: &Json, key: &str) -> Option<Vec<Entry>> {
    v.get(key)
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(Entry::parse).collect())
}

impl Policy {
    /// Merge settings documents: denylists from all of them; allowlists from
    /// all of them too, unless managed settings set `allowManagedMcpServersOnly`,
    /// when only the managed allowlist counts.
    pub fn merge(managed: &[Json], others: &[Json]) -> Policy {
        let managed_only = managed.iter().any(|v| {
            v.get("allowManagedMcpServersOnly")
                .and_then(|b| b.as_bool())
                == Some(true)
        });
        let mut p = Policy::default();
        let allow_sources: Vec<&Json> = if managed_only {
            managed.iter().collect()
        } else {
            managed.iter().chain(others).collect()
        };
        for v in allow_sources {
            if let Some(list) = entries(v, "allowedMcpServers") {
                p.allowed.get_or_insert_with(Vec::new).extend(list);
            }
        }
        for v in managed.iter().chain(others) {
            p.denied
                .extend(entries(v, "deniedMcpServers").unwrap_or_default());
        }
        p
    }

    /// `Err(why)` when the policy keeps this server from running.
    pub fn check(&self, name: &str, cfg: &McpServerConfig) -> Result<(), String> {
        if self.denied.iter().any(|e| e.matches(name, cfg)) {
            return Err("blocked by your organization's MCP policy (deniedMcpServers)".into());
        }
        let Some(allowed) = &self.allowed else {
            return Ok(());
        };
        let remote = cfg.url.is_some();
        let typed = |e: &Entry| {
            if remote {
                matches!(e, Entry::Url(_))
            } else {
                matches!(e, Entry::Command(_))
            }
        };
        let has_typed = allowed.iter().any(typed);
        let ok = allowed.iter().any(|e| {
            (typed(e) || (!has_typed && matches!(e, Entry::Name(_)))) && e.matches(name, cfg)
        });
        if ok {
            Ok(())
        } else {
            Err("not on your organization's MCP allowlist (allowedMcpServers)".into())
        }
    }
}

// ---------------------------------------------------------------- Chanto's per-project choices

/// One project's entry in `~/.chanto/mcp-projects.json`, in `~/.claude.json`'s shape.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ProjectMcp {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub disabled_mcp_servers: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub enabled_mcpjson_servers: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub disabled_mcpjson_servers: Vec<String>,
    /// What each approved `.mcp.json` server looked like when it was approved,
    /// by digest: a changed one has to be approved again.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub mcpjson_digests: BTreeMap<String, String>,
}

fn state_file() -> PathBuf {
    crate::config::home_dir().join("mcp-projects.json")
}

fn project_key(cwd: &Path) -> String {
    cwd.canonical()
        .unwrap_or_else(|_| cwd.to_path_buf())
        .display()
        .to_string()
}

fn read_state() -> BTreeMap<String, ProjectMcp> {
    std::fs::read(state_file())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn project_state(cwd: &Path) -> ProjectMcp {
    read_state().remove(&project_key(cwd)).unwrap_or_default()
}

/// Change this project's entry and save it.
pub fn update_project(cwd: &Path, f: impl FnOnce(&mut ProjectMcp)) -> anyhow::Result<()> {
    let mut all = read_state();
    let key = project_key(cwd);
    let entry = all.entry(key.clone()).or_default();
    f(entry);
    if *entry == ProjectMcp::default() {
        all.remove(&key);
    }
    std::fs::create_dir_all(crate::config::home_dir())?;
    std::fs::write(state_file(), serde_json::to_vec_pretty(&all)?)?;
    Ok(())
}

fn set(list: &mut Vec<String>, name: &str, on: bool) {
    list.retain(|n| n != name);
    if on {
        list.push(name.to_string());
    }
}

/// `/mcp disable` (`off`) or `/mcp enable` for a configured server.
pub fn set_disabled(cwd: &Path, name: &str, off: bool) -> anyhow::Result<()> {
    update_project(cwd, |p| set(&mut p.disabled_mcp_servers, name, off))
}

/// Approve (or reject) a project `.mcp.json` server for this project, as it
/// is now.
pub fn set_mcpjson_approval(cwd: &Path, name: &str, approve: bool) -> anyhow::Result<()> {
    let digest = mcpjson_servers(cwd)
        .and_then(|s| s.get(name).map(server_digest))
        .filter(|_| approve);
    update_project(cwd, |p| {
        set(&mut p.enabled_mcpjson_servers, name, approve);
        set(&mut p.disabled_mcpjson_servers, name, !approve);
        match digest {
            Some(d) => p.mcpjson_digests.insert(name.to_string(), d),
            None => p.mcpjson_digests.remove(name),
        };
    })
}

/// Remember these `.mcp.json` servers as approved, as they are now.
pub fn pin_mcpjson(cwd: &Path, pins: BTreeMap<String, String>) -> anyhow::Result<()> {
    update_project(cwd, |p| p.mcpjson_digests.extend(pins))
}

/// A server's `.mcp.json` entry as a digest. Everything in it counts: command,
/// arguments, URL, headers and environment can each change what runs.
pub fn server_digest(server: &Json) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(server.to_string())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn mcpjson_servers(cwd: &Path) -> Option<serde_json::Map<String, Json>> {
    read_json(&cwd.join(".mcp.json"))?
        .get("mcpServers")?
        .as_object()
        .cloned()
}

// ---------------------------------------------------------------- sources

fn read_json(p: &Path) -> Option<Json> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

/// Managed settings documents (`managed-settings.json`, then `managed-settings.d/*.json`).
fn managed_docs() -> Vec<Json> {
    crate::compat::managed_files(&crate::compat::managed_dir())
        .iter()
        .filter_map(|p| read_json(p))
        .filter(|v| v.is_object())
        .collect()
}

/// This project's entry in Claude Code's `~/.claude.json`.
fn claude_project(cwd: &Path) -> Option<Json> {
    let v = read_json(&crate::compat::home().join(".claude.json"))?;
    let dir = cwd.canonical().unwrap_or_else(|_| cwd.to_path_buf());
    v.get("projects")?
        .as_object()?
        .iter()
        .find(|(k, _)| Path::new(k) == dir || Path::new(k) == cwd)
        .map(|(_, p)| p.clone())
}

fn names(v: &Json, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|a| a.as_array())
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect()
}

/// The allow/deny policy for this folder. Project settings files contribute
/// denylist entries always, allowlist entries only once the folder is trusted.
pub fn policy(cwd: &Path, compat: &CompatConfig, trusted: bool) -> Policy {
    let mut others = Vec::new();
    if compat.claude {
        others.extend(read_json(
            &crate::compat::claude_dir().join("settings.json"),
        ));
        for name in ["settings.json", "settings.local.json"] {
            if let Some(mut v) = read_json(&cwd.join(".claude").join(name)) {
                if !trusted && let Some(o) = v.as_object_mut() {
                    o.remove("allowedMcpServers");
                }
                others.push(v);
            }
        }
    }
    Policy::merge(&managed_docs(), &others)
}

/// What to do with a project `.mcp.json` server.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Approval {
    Approved,
    /// Not approved yet: `/mcp enable <name>` (or trusting the folder) approves it.
    Pending,
    /// New or changed since this project's servers were approved:
    /// `/mcp enable <name>` approves it as it is now.
    Changed,
    /// Named in `disabledMcpjsonServers`.
    Rejected,
}

/// Approvals for this folder's `.mcp.json`, from every source.
#[derive(Debug, Default)]
pub struct Approvals {
    /// Chanto trusts the folder itself: every server is approved.
    pub trusted_here: bool,
    /// The folder is trusted (by Chanto or by Claude Code).
    pub trusted: bool,
    /// Approvals from outside the repository: they count even untrusted.
    user_all: bool,
    user: Vec<String>,
    /// Approvals committed in the project's `.claude/settings*.json`: they
    /// count only once the folder is trusted.
    repo_all: bool,
    repo: Vec<String>,
    rejected: Vec<String>,
    /// What this project's approved servers looked like (`ProjectMcp`).
    digests: BTreeMap<String, String>,
}

impl Approvals {
    pub fn load(cwd: &Path, compat: &CompatConfig, trusted_here: bool, trusted: bool) -> Approvals {
        let mut a = Approvals {
            trusted_here,
            trusted,
            ..Default::default()
        };
        let mine = project_state(cwd);
        a.user.extend(mine.enabled_mcpjson_servers);
        a.rejected.extend(mine.disabled_mcpjson_servers);
        a.digests = mine.mcpjson_digests;
        let mut user_docs = managed_docs();
        let mut repo_docs = Vec::new();
        if compat.claude {
            user_docs.extend(read_json(
                &crate::compat::claude_dir().join("settings.json"),
            ));
            user_docs.extend(claude_project(cwd));
            for name in ["settings.json", "settings.local.json"] {
                repo_docs.extend(read_json(&cwd.join(".claude").join(name)));
            }
        }
        for v in &user_docs {
            a.user_all |= v
                .get("enableAllProjectMcpServers")
                .and_then(|b| b.as_bool())
                == Some(true);
            a.user.extend(names(v, "enabledMcpjsonServers"));
            a.rejected.extend(names(v, "disabledMcpjsonServers"));
        }
        for v in &repo_docs {
            a.repo_all |= v
                .get("enableAllProjectMcpServers")
                .and_then(|b| b.as_bool())
                == Some(true);
            a.repo.extend(names(v, "enabledMcpjsonServers"));
            // A rejection only ever narrows, so it counts from anywhere.
            a.rejected.extend(names(v, "disabledMcpjsonServers"));
        }
        a
    }

    pub fn decide(&self, name: &str) -> Approval {
        let has = |l: &[String]| l.iter().any(|n| n == name);
        if has(&self.rejected) {
            Approval::Rejected
        } else if self.trusted_here
            // A project's `.mcp.json` server runs a command the repo ships, so
            // it needs the folder trusted — even when the enable comes from the
            // user's own settings (`enableAllProjectMcpServers` or a named
            // server). Otherwise opening a cloned repo could start its server.
            || (self.trusted
                && (self.user_all || has(&self.user) || self.repo_all || has(&self.repo)))
        {
            Approval::Approved
        } else {
            Approval::Pending
        }
    }
}

impl Approvals {
    /// [`Approvals::decide`] for a server as `.mcp.json` has it now. Trusting a
    /// folder approves what it held then, not whatever a later pull puts there.
    /// The one exception is the user's own `enableAllProjectMcpServers`, which
    /// asks for exactly that. A project with nothing recorded yet starts from
    /// what's there: the first run in a trusted folder, or after upgrading.
    pub fn is_recorded(&self, name: &str, server: &Json) -> bool {
        self.digests
            .get(name)
            .is_some_and(|d| *d == server_digest(server))
    }

    pub fn decide_server(&self, name: &str, server: &Json) -> Approval {
        let decision = self.decide(name);
        if decision != Approval::Approved || self.user_all {
            return decision;
        }
        match self.digests.get(name) {
            Some(digest) if *digest == server_digest(server) => Approval::Approved,
            Some(_) => Approval::Changed,
            None if self.digests.is_empty() || self.user.iter().any(|n| n == name) => {
                Approval::Approved
            }
            None => Approval::Changed,
        }
    }
}

/// Mark servers that policy blocks or that are turned off for this project.
pub fn apply(
    servers: &mut BTreeMap<String, McpServerConfig>,
    cwd: &Path,
    compat: &CompatConfig,
    trusted: bool,
) {
    let policy = policy(cwd, compat, trusted);
    let mine = project_state(cwd);
    let claude_off = if compat.claude {
        claude_project(cwd)
            .map(|p| names(&p, "disabledMcpServers"))
            .unwrap_or_default()
    } else {
        vec![]
    };
    gate_servers(servers, &policy, &mine.disabled_mcp_servers, &claude_off);
}

fn gate_servers(
    servers: &mut BTreeMap<String, McpServerConfig>,
    policy: &Policy,
    off: &[String],
    claude_off: &[String],
) {
    for (name, s) in servers.iter_mut() {
        s.gate = match policy.check(name, s) {
            Err(why) => Some(Gate::Policy(why)),
            Ok(()) if off.contains(name) => Some(Gate::Disabled),
            Ok(()) if claude_off.contains(name) => Some(Gate::DisabledInClaude),
            Ok(()) => None,
        };
    }
}

/// Servers in this folder's `.mcp.json` that didn't load: (name, why).
pub fn unloaded_mcpjson(
    cwd: &Path,
    loaded: &BTreeMap<String, McpServerConfig>,
    approvals: &Approvals,
) -> Vec<(String, Approval)> {
    let Some(v) = read_json(&cwd.join(".mcp.json")) else {
        return vec![];
    };
    let Some(servers) = v.get("mcpServers").and_then(|s| s.as_object()) else {
        return vec![];
    };
    servers
        .iter()
        .filter(|(n, _)| !loaded.contains_key(*n))
        .map(|(n, s)| (n.clone(), approvals.decide_server(n, s)))
        .filter(|(_, a)| *a != Approval::Approved)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stdio(cmd: &[&str]) -> McpServerConfig {
        McpServerConfig {
            command: cmd[0].into(),
            args: cmd[1..].iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }
    fn remote(url: &str) -> McpServerConfig {
        McpServerConfig {
            url: Some(url.into()),
            ..Default::default()
        }
    }

    #[test]
    fn url_patterns_follow_claude_code() {
        assert!(url_matches(
            "https://mcp.example.com/*",
            "https://mcp.example.com/api"
        ));
        assert!(url_matches(
            "https://Mcp.Example.com/*",
            "https://mcp.example.com./api"
        ));
        assert!(url_matches(
            "https://mcp.example.com",
            "https://mcp.example.com/any/path"
        ));
        assert!(
            !url_matches(
                "https://mcp.example.com/*",
                "https://mcp.example.com:8443/api"
            ),
            "full host: default port only"
        );
        assert!(url_matches(
            "https://mcp.example.com:8443/*",
            "https://mcp.example.com:8443/api"
        ));
        assert!(url_matches(
            "https://mcp.example.com:*/*",
            "https://mcp.example.com:8443/api"
        ));
        assert!(
            url_matches("https://*.example.com/*", "https://a.example.com:9000/x"),
            "wildcard host: any port"
        );
        assert!(!url_matches(
            "https://*.example.com/*",
            "https://example.org/x"
        ));
        assert!(url_matches(
            "http://localhost:*/*",
            "http://localhost:3000/mcp"
        ));
        assert!(url_matches(
            "*://mcp.example.com/*",
            "http://mcp.example.com/x"
        ));
        assert!(!url_matches(
            "*://mcp.example.com/*",
            "http://mcp.example.com:8080/x"
        ));
        assert!(
            !url_matches(
                "https://mcp.example.com/api/*",
                "https://mcp.example.com/API/x"
            ),
            "paths are case-sensitive"
        );
    }

    #[test]
    fn denylist_wins_and_allowlist_is_by_type() {
        let deny = json!({"deniedMcpServers": [{"serverName": "evil"}, {"serverUrl": "https://bad.example.com/*"}]});
        let allow = json!({"allowedMcpServers": [{"serverCommand": ["npx", "-y", "ok"]}, {"serverName": "named"}, {"serverUrl": "https://good.example.com/*"}]});
        let p = Policy::merge(std::slice::from_ref(&allow), std::slice::from_ref(&deny));
        assert!(
            p.check("evil", &stdio(&["npx", "-y", "ok"])).is_err(),
            "deny by name beats an allowed command"
        );
        assert!(
            p.check("x", &remote("https://bad.example.com/mcp"))
                .is_err()
        );
        assert!(p.check("x", &stdio(&["npx", "-y", "ok"])).is_ok());
        assert!(
            p.check("x", &stdio(&["npx", "ok"])).is_err(),
            "commands match exactly"
        );
        // A name counts only when the list has no entries of the server's own kind.
        assert!(p.check("named", &stdio(&["other"])).is_err());
        assert!(
            p.check("x", &remote("https://good.example.com/mcp"))
                .is_ok()
        );
        let names_only = Policy::merge(
            &[json!({"allowedMcpServers": [{"serverName": "named"}]})],
            &[],
        );
        assert!(names_only.check("named", &stdio(&["anything"])).is_ok());
        assert!(names_only.check("other", &stdio(&["anything"])).is_err());
        // Unset allows everything; an empty list allows nothing.
        assert!(Policy::merge(&[], &[]).check("x", &stdio(&["a"])).is_ok());
        assert!(
            Policy::merge(&[json!({"allowedMcpServers": []})], &[])
                .check("x", &stdio(&["a"]))
                .is_err()
        );
    }

    #[test]
    fn managed_only_ignores_other_allowlists() {
        let managed = json!({"allowManagedMcpServersOnly": true, "allowedMcpServers": [{"serverName": "corp"}]});
        let user = json!({"allowedMcpServers": [{"serverName": "mine"}], "deniedMcpServers": [{"serverName": "corp2"}]});
        let p = Policy::merge(std::slice::from_ref(&managed), std::slice::from_ref(&user));
        assert!(p.check("mine", &stdio(&["a"])).is_err());
        assert!(p.check("corp", &stdio(&["a"])).is_ok());
        assert_eq!(p.denied.len(), 1, "denylists still merge from every scope");
        let soft = Policy::merge(
            &[json!({"allowedMcpServers": [{"serverName": "corp"}]})],
            &[user],
        );
        assert!(
            soft.check("mine", &stdio(&["a"])).is_ok(),
            "without the lock, users can broaden it"
        );
    }

    #[test]
    fn a_trusted_folder_asks_again_when_a_server_changes_or_appears() {
        let first = json!({"command": "linter"});
        let later = json!({"command": "linter", "args": ["--other"]});
        let mut a = Approvals {
            trusted: true,
            trusted_here: true,
            ..Default::default()
        };
        assert_eq!(
            a.decide_server("lint", &first),
            Approval::Approved,
            "nothing recorded yet: starts from what's there"
        );
        a.digests.insert("lint".into(), server_digest(&first));
        assert_eq!(a.decide_server("lint", &first), Approval::Approved);
        assert_eq!(a.decide_server("lint", &later), Approval::Changed);
        assert_eq!(a.decide_server("added", &first), Approval::Changed);
        // A repository's own settings can't approve what it just changed.
        a.repo = vec!["lint".into()];
        assert_eq!(a.decide_server("lint", &later), Approval::Changed);
        // The user's own blanket setting is what it says.
        a.user_all = true;
        assert_eq!(a.decide_server("lint", &later), Approval::Approved);
        // An untrusted folder still doesn't start anything.
        let none = Approvals::default();
        assert_eq!(none.decide_server("lint", &first), Approval::Pending);
    }

    #[test]
    fn mcpjson_approvals() {
        let mut a = Approvals {
            user: vec!["mine".into()],
            repo: vec!["repo".into()],
            rejected: vec!["no".into()],
            ..Default::default()
        };
        assert_eq!(
            a.decide("mine"),
            Approval::Pending,
            "a project server runs repo-shipped code: untrusted folder asks even when the user named it"
        );
        assert_eq!(
            a.decide("repo"),
            Approval::Pending,
            "a cloned repo can't approve its own servers"
        );
        assert_eq!(a.decide("other"), Approval::Pending);
        a.trusted = true;
        assert_eq!(a.decide("mine"), Approval::Approved);
        assert_eq!(a.decide("repo"), Approval::Approved);
        assert_eq!(a.decide("other"), Approval::Pending);
        a.trusted_here = true;
        assert_eq!(a.decide("other"), Approval::Approved);
        assert_eq!(
            a.decide("no"),
            Approval::Rejected,
            "disabledMcpjsonServers always rejects"
        );
        // enableAllProjectMcpServers doesn't start a cloned repo's servers
        // until the folder is trusted.
        let mut all = Approvals {
            user_all: true,
            ..Default::default()
        };
        assert_eq!(all.decide("anything"), Approval::Pending);
        all.trusted = true;
        assert_eq!(all.decide("anything"), Approval::Approved);
    }

    #[test]
    fn gates_and_project_state() {
        let _g = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir =
            std::env::temp_dir().join(format!("chanto-mcppol-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        set_disabled(&dir, "a", true).unwrap();
        set_mcpjson_approval(&dir, "b", true).unwrap();
        let st = project_state(&dir);
        assert_eq!(st.disabled_mcp_servers, vec!["a"]);
        assert_eq!(st.enabled_mcpjson_servers, vec!["b"]);
        let mut servers: BTreeMap<String, McpServerConfig> = [
            ("a".into(), stdio(&["x"])),
            ("c".into(), stdio(&["y"])),
            ("d".into(), stdio(&["z"])),
        ]
        .into();
        let p = Policy::merge(
            &[json!({"deniedMcpServers": [{"serverCommand": ["y"]}]})],
            &[],
        );
        gate_servers(
            &mut servers,
            &p,
            &st.disabled_mcp_servers,
            &["d".to_string()],
        );
        assert_eq!(servers["a"].gate, Some(Gate::Disabled));
        assert!(matches!(servers["c"].gate, Some(Gate::Policy(_))));
        assert_eq!(servers["d"].gate, Some(Gate::DisabledInClaude));
        set_disabled(&dir, "a", false).unwrap();
        set_mcpjson_approval(&dir, "b", false).unwrap();
        let st = project_state(&dir);
        assert!(st.disabled_mcp_servers.is_empty() && st.enabled_mcpjson_servers.is_empty());
        assert_eq!(st.disabled_mcpjson_servers, vec!["b"]);
        update_project(&dir, |p| *p = ProjectMcp::default()).unwrap();
        assert!(
            !read_state().contains_key(&project_key(&dir)),
            "empty entries are dropped"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
