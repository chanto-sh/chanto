//! Read other agents' settings so Chanto works in projects already set up
//! for Claude Code or Codex. Each foreign file is translated into a TOML
//! table in Chanto's own schema and layered like a Chanto config file
//! (project layers go through the same trust gating).
//!
//! Claude Code: ~/.claude/settings.json, ~/.claude.json (MCP servers),
//! .claude/settings.json, .claude/settings.local.json.
//! Codex: ~/.codex/config.toml, .codex/config.toml (including `[hooks]`),
//! ~/.codex/hooks.json, .codex/hooks.json, ~/.codex/rules/*.rules (opt-in).
//! Claude Code managed settings (organization policy): applied above everything.
//! Other tools' instruction files: Gemini CLI (GEMINI.md), Cursor
//! (.cursor/rules, .cursorrules), GitHub Copilot (.github/copilot-instructions.md,
//! .github/instructions), Aider (.aider.conf.yml `read:`), opencode
//! (opencode.json `instructions`, .opencode/agent).

use crate::config::CompatConfig;
use crate::util::Canonical as _;
use serde_json::Value as Json;
use std::path::{Path, PathBuf};
use toml::{Table, Value};

/// Home directory used for other agents' files (overridable for tests).
pub fn home() -> PathBuf {
    std::env::var_os("CHANTO_COMPAT_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default())
}

pub fn claude_dir() -> PathBuf {
    home().join(".claude")
}

pub fn codex_dir() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".codex"))
}

fn read_json(p: &Path) -> Option<Json> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

fn read_toml(p: &Path) -> Option<Table> {
    toml::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

fn to_toml(v: &Json) -> Option<Value> {
    Some(match v {
        Json::Null => return None,
        Json::Bool(b) => Value::Boolean(*b),
        Json::Number(n) => n
            .as_i64()
            .map(Value::Integer)
            .or_else(|| n.as_f64().map(Value::Float))?,
        Json::String(s) => Value::String(s.clone()),
        Json::Array(a) => Value::Array(a.iter().filter_map(to_toml).collect()),
        Json::Object(o) => Value::Table(
            o.iter()
                .filter_map(|(k, v)| Some((k.clone(), to_toml(v)?)))
                .collect(),
        ),
    })
}

/// Claude Code settings.json → Chanto table.
fn from_claude_settings(v: &Json) -> Table {
    let mut t = Table::new();
    if let Some(p) = v.get("permissions") {
        let mut perm = Table::new();
        for k in ["allow", "deny", "ask", "additionalDirectories"] {
            if let Some(Value::Array(a)) = p.get(k).and_then(to_toml) {
                perm.insert(k.into(), Value::Array(a));
            }
        }
        t.insert("permissions".into(), Value::Table(perm));
        if let Some(m) = p.get("defaultMode").and_then(|m| m.as_str())
            && [
                "default",
                "acceptEdits",
                "plan",
                "auto",
                "bypassPermissions",
            ]
            .contains(&m)
        {
            t.insert("mode".into(), Value::String(m.into()));
        }
    }
    if let Some(Value::Table(h)) = v.get("hooks").and_then(to_toml) {
        t.insert("hooks".into(), Value::Table(h));
    }
    if let Some(b) = v.get("disableAllHooks").and_then(|b| b.as_bool()) {
        t.insert("disable_all_hooks".into(), Value::Boolean(b));
    }
    if let Some(Value::Table(e)) = v.get("env").and_then(to_toml) {
        t.insert(
            "env".into(),
            Value::Table(e.into_iter().filter(|(_, v)| v.is_str()).collect()),
        );
    }
    if let Some(o) = v.get("outputStyle").and_then(|o| o.as_str()) {
        t.insert("output_style".into(), Value::String(o.into()));
    }
    if let Some(h) = v.get("apiKeyHelper").and_then(|h| h.as_str()) {
        t.insert("api_key_helper".into(), Value::String(h.into()));
    }
    if let Some(c) = v.pointer("/statusLine/command").and_then(|c| c.as_str()) {
        t.insert("status_line".into(), Value::String(c.into()));
    }
    if let Some(b) = v.get("awaySummaryEnabled").and_then(|b| b.as_bool()) {
        t.insert("away_summary".into(), Value::Boolean(b));
    }
    if let Some(b) = v.get("bashEditDiffEnabled").and_then(|b| b.as_bool()) {
        t.insert("bash_diff".into(), Value::Boolean(b));
    }
    if let Some(b) = v.get("idleCompaction").and_then(|b| b.as_bool()) {
        t.insert("idle_compaction".into(), Value::Boolean(b));
    }
    if let Some(Value::Table(m)) = v.get("mcpServers").and_then(to_toml) {
        t.insert(
            "mcp_servers".into(),
            Value::Table(tag_servers(m, "Claude Code")),
        );
    }
    t
}

/// `--settings` JSON: Claude Code settings, plus `model` (a settings file
/// named on the command line is meant for this session's model).
pub fn cli_settings_layer(v: &Json) -> Table {
    let mut t = from_claude_settings(v);
    if let Some(m) = v.get("model").and_then(Json::as_str) {
        t.insert("model".into(), Value::String(m.into()));
    }
    t
}

/// A plugin in `root` (`--plugin-dir`): its `.claude-plugin/plugin.json`, or
/// at least one component folder. The name is the manifest's, else the folder's.
pub fn plugin_from_dir(root: &Path) -> Option<Plugin> {
    let manifest = read_json(&root.join(".claude-plugin").join("plugin.json"));
    let has_parts = ["commands", "skills", "agents", "hooks", "output-styles"]
        .iter()
        .any(|d| root.join(d).is_dir())
        || root.join(".mcp.json").is_file();
    if manifest.is_none() && !has_parts {
        return None;
    }
    let manifest = manifest.unwrap_or_else(|| Json::Object(Default::default()));
    let name = manifest["name"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| {
            root.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
    Some(Plugin {
        name,
        manifest: subst_root(&manifest, root),
        root: root.to_path_buf(),
    })
}

fn tag_servers(servers: Table, origin: &str) -> Table {
    servers
        .into_iter()
        .map(|(k, v)| match v {
            Value::Table(mut s) => {
                s.insert("imported_from".into(), Value::String(origin.into()));
                (k, Value::Table(s))
            }
            v => (k, v),
        })
        .collect()
}

/// Codex config.toml → Chanto table (MCP servers and providers only;
/// Codex model names are for OpenAI's API, not local servers).
fn from_codex_config(c: &Table) -> Table {
    let mut t = Table::new();
    if let Some(Value::Table(servers)) = c.get("mcp_servers") {
        let mut out = Table::new();
        for (name, s) in servers {
            let Value::Table(s) = s else { continue };
            let mut m = Table::new();
            for k in [
                "command",
                "args",
                "env",
                "url",
                "cwd",
                "enabled_tools",
                "disabled_tools",
            ] {
                if let Some(v) = s.get(k) {
                    m.insert(k.into(), v.clone());
                }
            }
            // `env_vars`: names (or `{ name = "X" }` tables) passed through from our environment.
            let names: Vec<Value> = match s.get("env_vars") {
                Some(Value::Array(a)) => a
                    .iter()
                    .filter_map(|v| {
                        v.as_str()
                            .or_else(|| v.get("name").and_then(|n| n.as_str()))
                    })
                    .map(|n| Value::String(n.into()))
                    .collect(),
                _ => vec![],
            };
            if !names.is_empty() {
                m.insert("env_vars".into(), Value::Array(names));
            }
            let secs = |k: &str, scale: f64| match s.get(k) {
                Some(Value::Integer(i)) => Some(*i as f64 / scale),
                Some(Value::Float(f)) => Some(*f / scale),
                _ => None,
            };
            if let Some(t) =
                secs("startup_timeout_sec", 1.0).or_else(|| secs("startup_timeout_ms", 1000.0))
            {
                m.insert("startup_timeout_sec".into(), Value::Float(t));
            }
            if let Some(t) = secs("tool_timeout_sec", 1.0) {
                m.insert("tool_timeout_sec".into(), Value::Float(t));
            }
            let mut headers = match s.get("http_headers") {
                Some(Value::Table(h)) => h.clone(),
                _ => Table::new(),
            };
            // `env_http_headers`: header name → environment variable holding its value.
            if let Some(Value::Table(h)) = s.get("env_http_headers") {
                for (name, var) in h {
                    if let Some(var) = var.as_str() {
                        headers.insert(name.clone(), Value::String(format!("${{{var}}}")));
                    }
                }
            }
            if let Some(var) = s.get("bearer_token_env_var").and_then(|v| v.as_str()) {
                headers.insert(
                    "Authorization".into(),
                    Value::String(format!("Bearer ${{{var}}}")),
                );
            }
            if !headers.is_empty() {
                m.insert("headers".into(), Value::Table(headers));
            }
            if s.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
                m.insert("disabled".into(), Value::Boolean(true));
            }
            out.insert(name.clone(), Value::Table(m));
        }
        t.insert(
            "mcp_servers".into(),
            Value::Table(tag_servers(out, "Codex")),
        );
    }
    if let Some(Value::Table(providers)) = c.get("model_providers") {
        let mut out = Table::new();
        for (name, p) in providers {
            let Some(url) = p.get("base_url").and_then(|v| v.as_str()) else {
                continue;
            };
            let mut m = Table::new();
            m.insert("kind".into(), Value::String("openai".into()));
            m.insert("base_url".into(), Value::String(url.into()));
            if let Some(key) = p
                .get("env_key")
                .and_then(|v| v.as_str())
                .and_then(|k| std::env::var(k).ok())
            {
                m.insert("api_key".into(), Value::String(key));
            }
            out.insert(name.clone(), Value::Table(m));
        }
        t.insert("providers".into(), Value::Table(out));
    }
    t
}

/// Codex hooks next to a config layer: `[hooks]` tables in its config.toml
/// and a `hooks.json` beside it, both in Claude Code's event / matcher /
/// handler layout. Off when `[features] hooks = false` (or the older
/// `codex_hooks`) is set in that config or the user's.
fn codex_hooks_layer(dir: &Path, config: Option<&Table>, user: Option<&Table>) -> Option<Table> {
    let off = |t: &Table| match t.get("features") {
        Some(Value::Table(f)) => ["hooks", "codex_hooks"]
            .iter()
            .any(|k| f.get(*k).and_then(Value::as_bool) == Some(false)),
        _ => false,
    };
    if config.is_some_and(off) || user.is_some_and(off) {
        return None;
    }
    let mut hooks = Table::new();
    let mut add = |h: &Table| {
        for (event, groups) in h {
            if let (Value::Array(g), Value::Array(all)) = (
                groups,
                hooks.entry(event.clone()).or_insert(Value::Array(vec![])),
            ) {
                all.extend(g.iter().cloned());
            }
        }
    };
    if let Some(Value::Table(h)) = config.and_then(|c| c.get("hooks")) {
        add(h);
    }
    if let Some(v) = read_json(&dir.join("hooks.json")) {
        let v = if v.get("hooks").is_some() {
            v["hooks"].clone()
        } else {
            v
        };
        if let Some(Value::Table(h)) = to_toml(&v) {
            add(&h);
        }
    }
    hooks.retain(|_, v| v.as_array().is_some_and(|a| !a.is_empty()));
    if hooks.is_empty() {
        return None;
    }
    let mut t = Table::new();
    t.insert("hooks".into(), Value::Table(hooks));
    Some(t)
}

/// Codex execpolicy rules: `prefix_rule(pattern=["git", "status"], decision="allow")`.
pub fn parse_codex_rules(text: &str) -> Vec<(String, String)> {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r#"prefix_rule\(\s*pattern\s*=\s*\[([^\]]*)\]\s*(?:,\s*decision\s*=\s*"(\w+)")?"#,
        )
        .unwrap()
    });
    static TOK: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#""((?:[^"\\]|\\.)*)""#).unwrap());
    RE.captures_iter(text)
        .filter_map(|c| {
            let words: Vec<String> = TOK
                .captures_iter(&c[1])
                .map(|w| w[1].replace("\\\"", "\""))
                .collect();
            // Only plain argv prefixes; `bash -lc <script>` entries are one-off scripts.
            if words.is_empty() || words.iter().any(|w| w.contains(' ') || w.contains(':')) {
                return None;
            }
            let decision = match c.get(2).map(|d| d.as_str()).unwrap_or("allow") {
                "allow" => "allow",
                "forbidden" => "deny",
                _ => "ask",
            };
            Some((decision.to_string(), format!("Bash({}:*)", words.join(" "))))
        })
        .collect()
}

fn project_key_matches(key: &str, cwd: &Path) -> bool {
    Path::new(key) == cwd
}

/// User-level layers (trusted), lowest precedence first.
pub fn user_layers(cwd: &Path, compat: &CompatConfig) -> Vec<(String, bool, Table)> {
    let mut out = Vec::new();
    let cwd = cwd.canonical().unwrap_or_else(|_| cwd.to_path_buf());
    if compat.codex {
        let p = codex_dir().join("config.toml");
        let user = read_toml(&p);
        if let Some(c) = &user {
            out.push((p.display().to_string(), false, from_codex_config(c)));
        }
        if let Some(t) = codex_hooks_layer(&codex_dir(), user.as_ref(), user.as_ref()) {
            out.push((format!("Codex hooks ({})", codex_dir().display()), false, t));
        }
        if compat.codex_rules {
            let mut perm = Table::new();
            if let Ok(rd) = std::fs::read_dir(codex_dir().join("rules")) {
                for e in rd
                    .flatten()
                    .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("rules"))
                {
                    for (d, rule) in
                        parse_codex_rules(&std::fs::read_to_string(e.path()).unwrap_or_default())
                    {
                        if let Value::Array(a) = perm.entry(d).or_insert(Value::Array(vec![])) {
                            a.push(Value::String(rule));
                        }
                    }
                }
            }
            if !perm.is_empty() {
                let mut t = Table::new();
                t.insert("permissions".into(), Value::Table(perm));
                out.push((codex_dir().join("rules").display().to_string(), false, t));
            }
        }
    }
    if compat.claude {
        // ~/.claude.json: user-scope MCP servers and this project's local-scope ones.
        let p = home().join(".claude.json");
        if let Some(v) = read_json(&p) {
            let mut servers = serde_json::Map::new();
            if let Some(Json::Object(m)) = v.get("mcpServers") {
                servers.extend(m.clone());
            }
            if let Some(projects) = v.get("projects").and_then(|p| p.as_object()) {
                for (_, proj) in projects
                    .iter()
                    .filter(|(k, _)| project_key_matches(k, &cwd))
                {
                    if let Some(Json::Object(m)) = proj.get("mcpServers") {
                        servers.extend(m.clone());
                    }
                }
            }
            out.push((
                p.display().to_string(),
                false,
                from_claude_settings(&serde_json::json!({ "mcpServers": servers })),
            ));
        }
        let p = claude_dir().join("settings.json");
        if let Some(v) = read_json(&p) {
            out.push((p.display().to_string(), false, from_claude_settings(&v)));
        }
    }
    out
}

/// Project-level layers (subject to trust gating), lowest precedence first.
pub fn project_layers(cwd: &Path, compat: &CompatConfig) -> Vec<(String, bool, Table)> {
    let mut out = Vec::new();
    if compat.codex {
        let p = cwd.join(".codex").join("config.toml");
        let project = read_toml(&p);
        if let Some(c) = &project {
            out.push((p.display().to_string(), true, from_codex_config(c)));
        }
        // Project hooks are a risky key: they load only in a trusted folder.
        let dir = cwd.join(".codex");
        if dir != codex_dir()
            && let Some(t) = codex_hooks_layer(
                &dir,
                project.as_ref(),
                read_toml(&codex_dir().join("config.toml")).as_ref(),
            )
        {
            out.push((format!("Codex hooks ({})", dir.display()), true, t));
        }
    }
    if compat.claude {
        for name in ["settings.json", "settings.local.json"] {
            let p = cwd.join(".claude").join(name);
            if let Some(v) = read_json(&p) {
                out.push((p.display().to_string(), true, from_claude_settings(&v)));
            }
        }
    }
    out
}

/// The user's compat switches (their own config only: a project can't turn a
/// source on). Trust discovery must respect the same read-in-place opt-outs.
pub(crate) fn enabled() -> CompatConfig {
    let mut compat: CompatConfig = read_toml(&crate::config::home_dir().join("config.toml"))
        .and_then(|t| t.get("compat").cloned())
        .and_then(|v| v.try_into().ok())
        .unwrap_or_default();
    if std::env::var("CHANTO_NO_CLAUDE_CONFIG").as_deref() == Ok("1") {
        compat.claude = false;
    }
    if std::env::var("CHANTO_NO_CODEX_CONFIG").as_deref() == Ok("1") {
        compat.codex = false;
    }
    compat
}

/// Did the user already trust this folder in Claude Code or Codex?
pub fn trusted_elsewhere(dir: &Path) -> bool {
    let compat = enabled();
    if let Some(v) = compat
        .claude
        .then(|| read_json(&home().join(".claude.json")))
        .flatten()
        && let Some(projects) = v.get("projects").and_then(|p| p.as_object())
        && projects.iter().any(|(k, p)| {
            crate::config::trust_covers(std::path::Path::new(k), dir)
                && p.get("hasTrustDialogAccepted").and_then(|b| b.as_bool()) == Some(true)
        })
    {
        return true;
    }
    // Codex trusts a project by its exact path or its git root, not every descendant.
    let git_root = dir.ancestors().find(|d| d.join(".git").exists());
    if let Some(Value::Table(projects)) = compat
        .codex
        .then(|| read_toml(&codex_dir().join("config.toml")))
        .flatten()
        .and_then(|t| t.get("projects").cloned())
    {
        let hit = |k: &String| Path::new(k) == dir || git_root.is_some_and(|g| Path::new(k) == g);
        if projects.iter().any(|(k, p)| {
            hit(k) && p.get("trust_level").and_then(|t| t.as_str()) == Some("trusted")
        }) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------- managed settings

#[cfg(test)]
thread_local! {
    /// Per-test managed settings directory (tests run on their own threads).
    pub static TEST_MANAGED_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Where Claude Code reads organization policy from: `managed-settings.json`
/// and `managed-settings.d/*.json`. `CHANTO_MANAGED_SETTINGS_DIR` overrides it
/// in debug builds only: a release build always reads the real policy.
pub fn managed_dir() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(d) = TEST_MANAGED_DIR.with(|d| d.borrow().clone()) {
            return d;
        }
    }
    if let Some(d) =
        std::env::var_os("CHANTO_MANAGED_SETTINGS_DIR").filter(|_| cfg!(debug_assertions))
    {
        return PathBuf::from(d);
    }
    // Keep tests independent of the machine's real policy.
    if cfg!(test) {
        return PathBuf::from("/nonexistent/chanto-test-managed-settings");
    }
    if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/ClaudeCode")
    } else if cfg!(windows) {
        PathBuf::from(r"C:\Program Files\ClaudeCode")
    } else {
        PathBuf::from("/etc/claude-code")
    }
}

/// Claude Code managed settings, translated: one layer per file
/// (`managed-settings.json`, then `managed-settings.d/*.json` alphabetically),
/// applied above every other layer.
#[derive(Debug, Default)]
pub struct Managed {
    pub layers: Vec<(String, Table)>,
    /// `allowManagedPermissionRulesOnly`: allow rules from other layers don't count.
    pub rules_only: bool,
    /// Files that exist but aren't a JSON object (Claude Code refuses to start on these).
    pub errors: Vec<String>,
    /// `permissions.defaultMode`: the mode every session starts in.
    pub default_mode: Option<String>,
    /// `permissions.disableBypassPermissionsMode: "disable"`: bypass mode can't be entered.
    pub disable_bypass: bool,
}

impl Managed {
    /// Allow rules the policy itself grants.
    pub fn allow_rules(&self) -> Vec<String> {
        self.layers
            .iter()
            .filter_map(|(_, t)| {
                t.get("permissions")
                    .and_then(|p| p.get("allow"))
                    .and_then(|a| a.as_array())
            })
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    }
}

/// Only the keys Chanto enforces: permission rules (`allow`/`deny`/`ask`,
/// `additionalDirectories`) and `env`. Everything else is ignored.
fn from_managed_settings(v: &Json) -> Table {
    let mut t = from_claude_settings(v);
    t.retain(|k, _| k == "permissions" || k == "env");
    t
}

/// The managed settings files in `dir`, in the order they apply.
pub fn managed_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = vec![dir.join("managed-settings.json")];
    if let Ok(rd) = std::fs::read_dir(dir.join("managed-settings.d")) {
        let mut extra: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| !n.starts_with('.'))
            })
            .collect();
        extra.sort();
        files.extend(extra);
    }
    files
}

pub fn managed_settings(dir: &Path) -> Managed {
    let mut out = Managed::default();
    for p in managed_files(dir) {
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        match serde_json::from_str::<Json>(&text) {
            Ok(v) if v.is_object() => {
                if let Some(b) = v.get("allowManagedPermissionRulesOnly").and_then(|b| b.as_bool()) {
                    out.rules_only = b;
                }
                if let Some(m) = v.pointer("/permissions/defaultMode").and_then(|m| m.as_str()) {
                    out.default_mode = Some(m.to_string());
                }
                if v.pointer("/permissions/disableBypassPermissionsMode").and_then(|d| d.as_str()) == Some("disable") {
                    out.disable_bypass = true;
                }
                out.layers.push((format!("{} (managed)", p.display()), from_managed_settings(&v)));
            }
            _ => out.errors.push(format!("Managed settings {} could not be parsed as a JSON object; fix it (Claude Code refuses to start with it too)", p.display())),
        }
    }
    out
}

// ---------------------------------------------------------------- plugins

/// An installed, enabled Claude Code plugin.
#[derive(Debug, Clone)]
pub struct Plugin {
    pub name: String,
    pub root: PathBuf,
    /// plugin.json merged over its marketplace entry (which may define
    /// components inline, e.g. `lspServers` for the official LSP plugins).
    pub manifest: Json,
}

/// Replace `${CLAUDE_PLUGIN_ROOT}` in every string of a JSON value.
fn subst_root(v: &Json, root: &Path) -> Json {
    subst_var(v, "${CLAUDE_PLUGIN_ROOT}", root)
}

/// Replace `var` (e.g. `${CLAUDE_PLUGIN_DATA}`) in every string of a JSON value.
fn subst_var(v: &Json, var: &str, value: &Path) -> Json {
    match v {
        Json::String(s) => Json::String(s.replace(var, &value.display().to_string())),
        Json::Array(a) => Json::Array(a.iter().map(|x| subst_var(x, var, value)).collect()),
        Json::Object(o) => Json::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), subst_var(x, var, value)))
                .collect(),
        ),
        x => x.clone(),
    }
}

/// A plugin's persistent data directory (`CLAUDE_PLUGIN_DATA`), under the
/// Chanto home. Created when something of the plugin first runs.
pub fn plugin_data_dir(name: &str) -> PathBuf {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    crate::config::home_dir()
        .join("plugin-data")
        .join(safe.trim_start_matches('.'))
}

/// Add `env` entries (without overriding ones already set) to every hook
/// handler of a hooks object (`{"Event": [{"matcher", "hooks": [...]}]}`).
fn hooks_with_env(hooks: &mut Json, env: &serde_json::Map<String, Json>) {
    for groups in hooks
        .as_object_mut()
        .into_iter()
        .flat_map(|o| o.values_mut())
    {
        for group in groups.as_array_mut().into_iter().flatten() {
            for handler in group
                .get_mut("hooks")
                .and_then(Json::as_array_mut)
                .into_iter()
                .flatten()
            {
                add_env(handler, env);
            }
        }
    }
}

fn add_env(target: &mut Json, env: &serde_json::Map<String, Json>) {
    let Some(o) = target.as_object_mut() else {
        return;
    };
    if let Some(e) = o
        .entry("env")
        .or_insert_with(|| Json::Object(Default::default()))
        .as_object_mut()
    {
        for (k, v) in env {
            e.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
}

fn marketplace_entry(marketplace: &str, plugin: &str) -> Option<(Json, PathBuf)> {
    let known = read_json(&claude_dir().join("plugins").join("known_marketplaces.json"))?;
    let loc = PathBuf::from(known.get(marketplace)?.get("installLocation")?.as_str()?);
    let m = read_json(&loc.join(".claude-plugin").join("marketplace.json"))?;
    let entry = m["plugins"]
        .as_array()?
        .iter()
        .find(|p| p["name"] == plugin)?
        .clone();
    Some((entry, loc))
}

/// Plugins enabled in Claude Code settings (user, plus project when `trusted`)
/// and installed for this scope.
pub fn plugins(cwd: &Path, trusted: bool) -> Vec<Plugin> {
    plugins_with_compat(cwd, trusted, true)
}

pub fn plugins_with_compat(cwd: &Path, trusted: bool, claude: bool) -> Vec<Plugin> {
    // `--plugin-dir` plugins come last (they win a name clash); `--bare` loads only them.
    let launch = crate::launch::get();
    if launch.no_customizations() {
        return launch.plugins.clone();
    }
    let mut out = installed_plugins(cwd, trusted, claude);
    out.extend(launch.plugins.iter().cloned());
    out
}

fn installed_plugins(cwd: &Path, trusted: bool, claude: bool) -> Vec<Plugin> {
    let mut enabled: std::collections::BTreeMap<String, bool> = Default::default();
    let mut settings = vec![claude_dir().join("settings.json")];
    if trusted {
        settings.push(cwd.join(".claude").join("settings.json"));
        settings.push(cwd.join(".claude").join("settings.local.json"));
    }
    for v in settings.iter().filter_map(|p| read_json(p)) {
        if let Some(o) = v["enabledPlugins"].as_object() {
            for (k, on) in o {
                enabled.insert(k.clone(), on.as_bool().unwrap_or(false));
            }
        }
    }
    let installed =
        read_json(&claude_dir().join("plugins").join("installed_plugins.json")).unwrap_or_default();
    let cwd = cwd.canonical().unwrap_or_else(|_| cwd.to_path_buf());
    let mut out = Vec::new();
    for (id, on) in enabled.into_iter().filter(|(_, on)| claude && *on) {
        let _ = on;
        let Some(entries) = installed["plugins"][&id].as_array() else {
            continue;
        };
        // A user-scope install, or a project/local install for this directory.
        let Some(inst) = entries.iter().find(|e| {
            e["scope"] == "user"
                || trusted
                    && e["projectPath"]
                        .as_str()
                        .is_some_and(|p| Path::new(p) == cwd)
        }) else {
            continue;
        };
        let (name, market) = id.split_once('@').unwrap_or((id.as_str(), ""));
        let root = PathBuf::from(inst["installPath"].as_str().unwrap_or_default());
        let mut manifest = Json::Object(Default::default());
        if let Some((entry, loc)) = marketplace_entry(market, name) {
            manifest = entry;
            // Some marketplaces install from their own checkout.
            if !root.is_dir()
                && let Some(src) = manifest["source"].as_str()
            {
                let alt = loc.join(src);
                if alt.is_dir() {
                    let m = read_json(&alt.join(".claude-plugin").join("plugin.json"))
                        .unwrap_or_default();
                    if let (Some(dst), Some(src)) = (manifest.as_object_mut(), m.as_object()) {
                        dst.extend(src.clone());
                    }
                    out.push(Plugin {
                        name: name.to_string(),
                        manifest: subst_root(&manifest, &alt),
                        root: alt,
                    });
                    continue;
                }
            }
        }
        if let (Some(dst), Some(src)) = (
            manifest.as_object_mut(),
            read_json(&root.join(".claude-plugin").join("plugin.json"))
                .as_ref()
                .and_then(|m| m.as_object()),
        ) {
            dst.extend(src.clone());
        }
        if root.is_dir() {
            out.push(Plugin {
                name: name.to_string(),
                manifest: subst_root(&manifest, &root),
                root,
            });
        }
    }
    for mut plugin in crate::plugin::loaded(&crate::plugin::root()).unwrap_or_default() {
        let id = format!(
            "{}@{}",
            plugin.name,
            plugin
                .manifest
                .get("_marketplace")
                .and_then(Json::as_str)
                .unwrap_or("")
        );
        if enabled_override(cwd.as_path(), trusted, &id) == Some(false) {
            continue;
        }
        plugin.manifest = subst_root(&plugin.manifest, &plugin.root);
        out.push(plugin);
    }
    out
}

fn enabled_override(cwd: &Path, trusted: bool, id: &str) -> Option<bool> {
    let mut paths = vec![claude_dir().join("settings.json")];
    if trusted {
        paths.extend([
            cwd.join(".claude/settings.json"),
            cwd.join(".claude/settings.local.json"),
        ]);
    }
    paths
        .iter()
        .filter_map(|p| read_json(p).and_then(|v| v["enabledPlugins"][id].as_bool()))
        .next_back()
}

impl Plugin {
    /// Describe components without executing plugin code.
    pub fn contents(&self) -> (String, bool) {
        let mut lines = Vec::new();
        for key in ["commands", "skills", "agents"] {
            let paths: Vec<_> = self
                .dirs(key)
                .iter()
                .flat_map(|d| {
                    std::fs::read_dir(d)
                        .into_iter()
                        .flatten()
                        .filter_map(Result::ok)
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                })
                .collect();
            lines.push(format!(
                "{key}: {}",
                if paths.is_empty() {
                    "none".into()
                } else {
                    paths.join(", ")
                }
            ));
        }
        let mut risky = false;
        for (key, file) in [
            ("hooks", "hooks/hooks.json"),
            ("mcpServers", ".mcp.json"),
            ("lspServers", ".lsp.json"),
        ] {
            // A declared or present component needs consent even if malformed.
            let present = !self.manifest[key].is_null() || self.root.join(file).exists();
            risky |= present;
            let value = self.component(key, file);
            let root = format!("{}/", self.root.display());
            let shown = match value {
                Some(v) => component_summary(key, &v).replace(&root, ""),
                None if present => "declared (unreadable)".into(),
                None => "none".into(),
            };
            lines.push(format!("{key}: {shown}"));
        }
        // Skills and commands that run a command or widen permissions when
        // invoked need the same consent as hooks.
        let invoked = self.invoked_effects();
        risky |= !invoked.is_empty();
        lines.push(format!(
            "runs when invoked: {}",
            if invoked.is_empty() {
                "none".into()
            } else {
                invoked.join("; ")
            }
        ));
        (lines.join("\n"), risky)
    }

    /// What the plugin's commands and skills do on their own when invoked:
    /// inline !`cmd` blocks (run outside the sandbox), frontmatter `hooks`
    /// and pre-approved `allowed-tools`. One entry per file, e.g.
    /// "skills/deploy/SKILL.md: !`git push`, allowed-tools".
    fn invoked_effects(&self) -> Vec<String> {
        static INLINE: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new(r"(^|\s)!`([^`]+)`").unwrap());
        fn md_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let Ok(ty) = e.file_type() else { continue };
                let p = e.path();
                if ty.is_dir() && depth < 8 {
                    md_files(&p, depth + 1, out);
                } else if ty.is_file() && p.extension().is_some_and(|x| x == "md") {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        for key in ["commands", "skills"] {
            for d in self.dirs(key) {
                md_files(&d, 0, &mut files);
            }
        }
        files.sort();
        let root = format!("{}/", self.root.display());
        let mut out = Vec::new();
        for f in files {
            let Ok(text) = std::fs::read_to_string(&f) else {
                continue;
            };
            let mut found: Vec<String> = INLINE
                .captures_iter(crate::context::strip_frontmatter(&text))
                .map(|c| format!("!`{}`", crate::util::one_line(&c[2], 80)))
                .collect();
            for (k, _) in crate::context::frontmatter(&text) {
                if matches!(k.as_str(), "hooks" | "allowed-tools" | "allowed_tools") {
                    found.push(k);
                }
            }
            if !found.is_empty() {
                let name = f.display().to_string().replace(&root, "");
                out.push(format!("{name}: {}", found.join(", ")));
            }
        }
        out
    }

    /// Directories for a component (`commands`, `agents`, `skills`): the
    /// default folder plus any paths the manifest adds.
    pub fn dirs(&self, key: &str) -> Vec<PathBuf> {
        let mut v = vec![self.root.join(key)];
        match &self.manifest[key] {
            Json::String(p) => v.push(self.root.join(p)),
            Json::Array(a) => v.extend(
                a.iter()
                    .filter_map(|p| p.as_str())
                    .map(|p| self.root.join(p)),
            ),
            _ => {}
        }
        v.retain(|p| p.is_dir());
        v
    }

    /// Inline object or a JSON file path for `key`, else the default file.
    fn component(&self, key: &str, default_file: &str) -> Option<Json> {
        let from_file = |p: PathBuf| read_json(&p).map(|v| subst_root(&v, &self.root));
        match &self.manifest[key] {
            Json::Object(_) => Some(self.manifest[key].clone()),
            Json::String(p) => from_file(self.root.join(p)),
            _ => from_file(self.root.join(default_file)),
        }
    }

    /// Hooks, MCP servers and language servers as a Chanto config table.
    fn layer(&self) -> Table {
        let mut t = Table::new();
        // Plugin hooks and stdio MCP servers get CLAUDE_PLUGIN_ROOT and
        // CLAUDE_PLUGIN_DATA, and `${CLAUDE_PLUGIN_DATA}` is substituted.
        let data = plugin_data_dir(&self.name);
        let mut env = serde_json::Map::new();
        env.insert(
            "CLAUDE_PLUGIN_ROOT".into(),
            Json::String(self.root.display().to_string()),
        );
        env.insert(
            "CLAUDE_PLUGIN_DATA".into(),
            Json::String(data.display().to_string()),
        );
        if let Some(h) = self.component("hooks", "hooks/hooks.json") {
            let h = if h.get("hooks").is_some() {
                h["hooks"].clone()
            } else {
                h
            };
            let mut h = subst_var(&h, "${CLAUDE_PLUGIN_DATA}", &data);
            hooks_with_env(&mut h, &env);
            if let Some(Value::Table(h)) = to_toml(&h) {
                t.insert("hooks".into(), Value::Table(h));
            }
        }
        if let Some(m) = self.component("mcpServers", ".mcp.json") {
            let m = if m.get("mcpServers").is_some() {
                m["mcpServers"].clone()
            } else {
                m
            };
            let mut m = subst_var(&m, "${CLAUDE_PLUGIN_DATA}", &data);
            let mut stdio = false;
            for s in m.as_object_mut().into_iter().flat_map(|o| o.values_mut()) {
                if s.get("command").is_some() {
                    stdio = true;
                    add_env(s, &env);
                }
            }
            if stdio {
                // The server may expect its data directory to exist.
                let _ = std::fs::create_dir_all(&data);
            }
            if let Some(Value::Table(m)) = to_toml(&m) {
                t.insert(
                    "mcp_servers".into(),
                    Value::Table(tag_servers(m, &format!("plugin {}", self.name))),
                );
            }
        }
        if let Some(l) = self.component("lspServers", ".lsp.json") {
            let mut servers = Table::new();
            for (name, s) in l.as_object().into_iter().flatten() {
                let mut m = Table::new();
                if let Some(c) = s["command"].as_str() {
                    m.insert("command".into(), Value::String(c.into()));
                }
                if let Some(Value::Array(a)) = s.get("args").and_then(to_toml) {
                    m.insert("args".into(), Value::Array(a));
                }
                let exts: Vec<Value> = s["extensionToLanguage"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(e, _)| Value::String(e.trim_start_matches('.').to_string()))
                    .collect();
                m.insert("extensions".into(), Value::Array(exts));
                servers.insert(name.clone(), Value::Table(m));
            }
            if !servers.is_empty() {
                let mut lsp = Table::new();
                lsp.insert("servers".into(), Value::Table(servers));
                t.insert("lsp".into(), Value::Table(lsp));
            }
        }
        t
    }
}

static ACTIVE: std::sync::RwLock<Vec<Plugin>> = std::sync::RwLock::new(Vec::new());

/// Plugins in use this session (set once at startup).
pub fn set_active_plugins(p: Vec<Plugin>) {
    *ACTIVE.write().unwrap() = p;
}

pub fn active_plugins() -> Vec<Plugin> {
    ACTIVE.read().unwrap().clone()
}

/// Config layers contributed by enabled plugins (user-installed, so trusted).
pub fn plugin_layers(
    cwd: &Path,
    compat: &CompatConfig,
    trusted: bool,
) -> Vec<(String, bool, Table)> {
    plugins_with_compat(cwd, trusted, compat.claude)
        .iter()
        .map(|p| (format!("plugin {}", p.name), false, p.layer()))
        .filter(|(_, _, t)| !t.is_empty())
        .collect()
}

/// Serializes tests that point CHANTO_COMPAT_HOME at a temp dir.
#[cfg(test)]
pub static TEST_HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub fn tests_translate(v: &Json) -> Value {
    Value::Table(from_claude_settings(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_settings_translate() {
        let v: Json = serde_json::from_str(
            r#"{"permissions":{"allow":["Bash(npm test:*)"],"deny":["Read(./.env)"],"defaultMode":"acceptEdits"},
                "hooks":{"PostToolUse":[{"matcher":"Edit|Write","hooks":[{"type":"command","command":"fmt","timeout":5}]}]},
                "env":{"FOO":"1"},"statusLine":{"type":"command","command":"echo hi"},"bashEditDiffEnabled":false}"#,
        )
        .unwrap();
        let t = from_claude_settings(&v);
        let mut cfg: crate::config::Config = Value::Table(t).try_into().unwrap();
        assert_eq!(cfg.permissions.allow, vec!["Bash(npm test:*)"]);
        assert_eq!(cfg.permissions.deny, vec!["Read(./.env)"]);
        assert_eq!(cfg.mode, Some(crate::permissions::Mode::AcceptEdits));
        assert_eq!(cfg.env["FOO"], "1");
        assert_eq!(cfg.status_line.as_deref(), Some("echo hi"));
        assert_eq!(cfg.bash_diff, Some(false));
        let h = &mut cfg.hooks;
        assert_eq!(h["PostToolUse"][0].hooks[0].command, "fmt");
    }

    #[test]
    fn codex_config_translate() {
        let c: Table = toml::from_str(
            "model = \"gpt-5\"\n[mcp_servers.docs]\ncommand = \"npx\"\nargs = [\"-y\", \"docs\"]\n[mcp_servers.remote]\nurl = \"https://x/mcp\"\nbearer_token_env_var = \"TOK\"\nenabled = false\n[model_providers.ollama]\nbase_url = \"http://localhost:11434/v1\"\n",
        )
        .unwrap();
        let cfg: crate::config::Config = Value::Table(from_codex_config(&c)).try_into().unwrap();
        assert_eq!(cfg.mcp_servers["docs"].command, "npx");
        assert_eq!(
            cfg.mcp_servers["remote"].headers["Authorization"],
            "Bearer ${TOK}"
        );
        assert!(cfg.mcp_servers["remote"].disabled);
        assert_eq!(
            cfg.providers["ollama"].base_url,
            "http://localhost:11434/v1"
        );
        assert!(cfg.model.is_none());
    }

    #[test]
    fn codex_mcp_extra_fields() {
        let c: Table = toml::from_str(
            r#"
[mcp_servers.gh]
command = "gh-mcp"
cwd = "/work"
env_vars = ["GITHUB_TOKEN", { name = "GH_HOST", source = "local" }]
startup_timeout_ms = 2500
tool_timeout_sec = 90
enabled_tools = ["search", "read"]
disabled_tools = ["read"]
[mcp_servers.api]
url = "https://x/mcp"
env_http_headers = { "X-Api-Key" = "API_KEY_VAR" }
startup_timeout_sec = 5
"#,
        )
        .unwrap();
        let cfg: crate::config::Config = Value::Table(from_codex_config(&c)).try_into().unwrap();
        let gh = &cfg.mcp_servers["gh"];
        assert_eq!(gh.cwd.as_deref(), Some("/work"));
        assert_eq!(gh.env_vars, vec!["GITHUB_TOKEN", "GH_HOST"]);
        assert_eq!(gh.startup_timeout_sec, Some(2.5));
        assert_eq!(gh.tool_timeout_sec, Some(90.0));
        assert_eq!(
            gh.enabled_tools.as_deref(),
            Some(&["search".to_string(), "read".to_string()][..])
        );
        assert_eq!(gh.disabled_tools, vec!["read"]);
        let api = &cfg.mcp_servers["api"];
        assert_eq!(api.headers["X-Api-Key"], "${API_KEY_VAR}");
        assert_eq!(api.startup_timeout_sec, Some(5.0));
    }

    #[test]
    fn managed_settings_read_and_translate() {
        let d =
            std::env::temp_dir().join(format!("chanto-managed-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(d.join("managed-settings.d")).unwrap();
        std::fs::write(
            d.join("managed-settings.json"),
            r#"{"permissions":{"deny":["Bash(curl:*)"],"allow":["Read"],"defaultMode":"bypassPermissions"},"env":{"ORG":"1"},"hooks":{"Stop":[]},"model":"x"}"#,
        )
        .unwrap();
        std::fs::write(
            d.join("managed-settings.d/20-b.json"),
            r#"{"permissions":{"deny":["WebFetch"]},"env":{"ORG":"2"}}"#,
        )
        .unwrap();
        std::fs::write(
            d.join("managed-settings.d/10-a.json"),
            r#"{"allowManagedPermissionRulesOnly":true}"#,
        )
        .unwrap();
        std::fs::write(d.join("managed-settings.d/.hidden.json"), "not json").unwrap();
        std::fs::write(d.join("managed-settings.d/notes.txt"), "ignored").unwrap();
        let m = managed_settings(&d);
        assert!(m.errors.is_empty(), "{:?}", m.errors);
        assert_eq!(m.layers.len(), 3);
        assert!(m.layers[1].0.contains("10-a.json") && m.layers[2].0.contains("20-b.json"));
        assert!(m.rules_only);
        assert_eq!(m.allow_rules(), vec!["Read"]);
        assert_eq!(m.default_mode.as_deref(), Some("bypassPermissions"));
        assert!(!m.disable_bypass);
        // Only permissions and env are taken.
        let t = &m.layers[0].1;
        assert!(t.get("hooks").is_none() && t.get("mode").is_none() && t.get("model").is_none());
        assert_eq!(t["env"]["ORG"].as_str(), Some("1"));
        // A later file can turn bypass off.
        std::fs::write(
            d.join("managed-settings.d/25-c.json"),
            r#"{"permissions":{"disableBypassPermissionsMode":"disable","defaultMode":"plan"}}"#,
        )
        .unwrap();
        let m = managed_settings(&d);
        assert!(m.disable_bypass && m.default_mode.as_deref() == Some("plan"));
        std::fs::write(d.join("managed-settings.d/30-bad.json"), "[1]").unwrap();
        assert_eq!(managed_settings(&d).errors.len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn managed_settings_win_over_every_layer() {
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!(
            "chanto-managed-load-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let (managed, home, proj) = (root.join("managed"), root.join("home"), root.join("proj"));
        for d in [&managed, &home, &proj.join(".chanto")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(managed.join("managed-settings.json"), r#"{"permissions":{"deny":["Bash(rm:*)"],"allow":["Grep"],"defaultMode":"plan","disableBypassPermissionsMode":"disable"},"env":{"ORG_VAR":"managed"},"allowManagedPermissionRulesOnly":true}"#).unwrap();
        std::fs::write(home.join("config.toml"), "mode = \"bypassPermissions\"\nenv = { ORG_VAR = \"user\" }\n[permissions]\nallow = [\"Bash\"]\n").unwrap();
        std::fs::write(
            proj.join(".chanto/config.toml"),
            "[permissions]\ndeny = [\"Edit\"]\n",
        )
        .unwrap();
        TEST_MANAGED_DIR.with(|d| *d.borrow_mut() = Some(managed.clone()));
        let prev_home = std::env::var_os("CHANTO_HOME");
        unsafe {
            std::env::set_var("CHANTO_HOME", &home);
            std::env::set_var("CHANTO_COMPAT_HOME", &root);
        }
        let r = crate::config::Config::load(&proj, true);
        unsafe {
            match prev_home {
                Some(h) => std::env::set_var("CHANTO_HOME", h),
                None => std::env::remove_var("CHANTO_HOME"),
            }
        }
        TEST_MANAGED_DIR.with(|d| *d.borrow_mut() = None);
        let (cfg, _) = r.unwrap();
        assert_eq!(cfg.env["ORG_VAR"], "managed");
        assert!(cfg.permissions.deny.iter().any(|r| r == "Bash(rm:*)"));
        assert!(cfg.permissions.deny.iter().any(|r| r == "Edit"));
        // allowManagedPermissionRulesOnly: the user's `Bash` allow no longer counts.
        assert_eq!(cfg.permissions.allow, vec!["Grep"]);
        assert!(
            cfg.layers
                .last()
                .is_some_and(|l| l.contains("managed-settings.json"))
        );
        // The policy's defaultMode beats the user's own mode, and bypass is off.
        assert_eq!(cfg.mode, Some(crate::permissions::Mode::Plan));
        assert_eq!(cfg.policy_mode, Some(crate::permissions::Mode::Plan));
        assert!(cfg.policy_no_bypass);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plugins_load() {
        let h = std::env::temp_dir().join(format!("chanto-plug-{}", uuid::Uuid::new_v4().simple()));
        let w = |p: &str, t: &str| {
            let p = h.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, t).unwrap();
        };
        let root = h.join(".claude/plugins/cache/mkt/tool/1.0.0");
        let mloc = h.join(".claude/plugins/marketplaces/mkt");
        w(
            ".claude/settings.json",
            r#"{"enabledPlugins": {"tool@mkt": true, "off@mkt": false}}"#,
        );
        // Build the JSON with serde: on Windows the install path contains
        // backslashes, which must be escaped inside a JSON string.
        w(
            ".claude/plugins/installed_plugins.json",
            &serde_json::json!({
                "version": 2,
                "plugins": {
                    "tool@mkt": [{"scope": "user", "installPath": root.display().to_string()}],
                    "off@mkt": [{"scope": "user", "installPath": "/nope"}],
                },
            })
            .to_string(),
        );
        w(
            ".claude/plugins/known_marketplaces.json",
            &serde_json::json!({"mkt": {"installLocation": mloc.display().to_string()}})
                .to_string(),
        );
        w(
            ".claude/plugins/marketplaces/mkt/.claude-plugin/marketplace.json",
            r#"{"plugins": [{"name": "tool", "lspServers": {"zls": {"command": "zls", "extensionToLanguage": {".zig": "zig"}}}}]}"#,
        );
        w(
            ".claude/plugins/cache/mkt/tool/1.0.0/.claude-plugin/plugin.json",
            r#"{"name": "tool"}"#,
        );
        w(
            ".claude/plugins/cache/mkt/tool/1.0.0/commands/ship.md",
            "---\ndescription: Ship it\n---\nShip $ARGUMENTS",
        );
        w(
            ".claude/plugins/cache/mkt/tool/1.0.0/agents/fixer.md",
            "---\nname: fixer\ndescription: Fixes\n---\nYou fix.",
        );
        w(
            ".claude/plugins/cache/mkt/tool/1.0.0/skills/style/SKILL.md",
            "---\nname: style\ndescription: Style guide\n---\nUse tabs.",
        );
        w(
            ".claude/plugins/cache/mkt/tool/1.0.0/hooks/hooks.json",
            r#"{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "python3 ${CLAUDE_PLUGIN_ROOT}/hooks/check.py"}]}]}}"#,
        );
        w(
            ".claude/plugins/cache/mkt/tool/1.0.0/.mcp.json",
            r#"{"docs": {"type": "http", "url": "https://example.com/mcp"}}"#,
        );
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("CHANTO_COMPAT_HOME", &h) };
        let cwd = h.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let ps = plugins(&cwd, false);
        assert_eq!(ps.len(), 1);
        let t = ps[0].layer();
        let cfg: crate::config::Config = Value::Table(t).try_into().unwrap();
        assert_eq!(cfg.lsp.servers["zls"].extensions, vec!["zig"]);
        assert_eq!(
            cfg.mcp_servers["docs"].imported_from.as_deref(),
            Some("plugin tool")
        );
        let hook = &cfg.hooks["PreToolUse"][0].hooks[0].command;
        assert!(hook.contains(&root.display().to_string()), "{hook}");
        let env = &cfg.hooks["PreToolUse"][0].hooks[0].env;
        assert_eq!(env["CLAUDE_PLUGIN_ROOT"], root.display().to_string());
        assert_eq!(
            PathBuf::from(&env["CLAUDE_PLUGIN_DATA"]),
            plugin_data_dir("tool")
        );
        set_active_plugins(ps);
        assert!(
            crate::context::load_commands(&cwd)
                .iter()
                .any(|c| c.name == "tool:ship")
        );
        assert!(
            crate::context::load_agents(&cwd)
                .iter()
                .any(|a| a.name == "tool:fixer")
        );
        let skills = crate::context::load_skills(&cwd);
        assert!(crate::util::find_named(&skills, "style", |s| &s.name).is_some());
        set_active_plugins(vec![]);
    }

    #[test]
    fn plugin_env_and_data_substitution() {
        let mut env = serde_json::Map::new();
        env.insert("CLAUDE_PLUGIN_ROOT".into(), Json::String("/p".into()));
        let mut hooks = serde_json::json!({"Stop": [{"hooks": [
            {"type": "command", "command": "x", "env": {"CLAUDE_PLUGIN_ROOT": "/mine"}},
            {"type": "command", "command": "y"}
        ]}]});
        hooks_with_env(&mut hooks, &env);
        assert_eq!(
            hooks["Stop"][0]["hooks"][0]["env"]["CLAUDE_PLUGIN_ROOT"],
            "/mine"
        );
        assert_eq!(
            hooks["Stop"][0]["hooks"][1]["env"]["CLAUDE_PLUGIN_ROOT"],
            "/p"
        );
        let mut server =
            serde_json::json!({"command": "srv", "args": ["${CLAUDE_PLUGIN_DATA}/db"]});
        add_env(&mut server, &env);
        assert_eq!(server["env"]["CLAUDE_PLUGIN_ROOT"], "/p");
        let v = subst_var(&server, "${CLAUDE_PLUGIN_DATA}", Path::new("/data"));
        assert_eq!(v["args"][0], "/data/db");
        assert!(plugin_data_dir("a/b").ends_with("a-b"));
    }

    #[test]
    fn codex_hooks_load() {
        let d = std::env::temp_dir().join(format!(
            "chanto-codex-hooks-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("hooks.json"), r#"{"hooks": {"PreToolUse": [{"matcher": "^Bash$", "hooks": [{"type": "command", "command": "from-json"}]}]}}"#).unwrap();
        let c: Table = toml::from_str("[[hooks.PreToolUse]]\nmatcher = \"^Bash$\"\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"from-toml\"\ntimeout = 5\n").unwrap();
        let t = codex_hooks_layer(&d, Some(&c), None).unwrap();
        let mut cfg: crate::config::Config = Value::Table(t).try_into().unwrap();
        cfg.flatten_hooks_for_test();
        let cmds: Vec<&str> = cfg.hooks["PreToolUse"]
            .iter()
            .map(|h| h.command.as_str())
            .collect();
        assert_eq!(cmds, vec!["from-toml", "from-json"]);
        assert_eq!(cfg.hooks["PreToolUse"][0].timeout_secs, Some(5));
        // `[features] hooks = false` (or the older `codex_hooks`) turns them off.
        let off: Table = toml::from_str("[features]\nhooks = false\n").unwrap();
        assert!(codex_hooks_layer(&d, Some(&c), Some(&off)).is_none());
        let legacy: Table = toml::from_str("[features]\ncodex_hooks = false\n").unwrap();
        assert!(codex_hooks_layer(&d, None, Some(&legacy)).is_none());
        assert!(codex_hooks_layer(&d.join("missing"), None, None).is_none());
    }

    #[test]
    fn codex_rules_parse() {
        let r = parse_codex_rules(
            "prefix_rule(pattern=[\"git\", \"status\"], decision=\"allow\")\nprefix_rule(pattern=[\"rm\"], decision=\"forbidden\")\nprefix_rule(pattern=[\"/bin/zsh\", \"-lc\", \"a b\"], decision=\"allow\")\n",
        );
        assert_eq!(
            r,
            vec![
                ("allow".into(), "Bash(git status:*)".into()),
                ("deny".into(), "Bash(rm:*)".into())
            ]
        );
    }
}

/// What a hooks/MCP/LSP component runs, one line per entry, for install
/// confirmations.
fn component_summary(key: &str, v: &Json) -> String {
    let mut out = Vec::new();
    if key == "hooks" {
        let hooks = if v["hooks"].is_object() {
            &v["hooks"]
        } else {
            v
        };
        for (event, groups) in hooks.as_object().into_iter().flatten() {
            for g in groups.as_array().into_iter().flatten() {
                let matcher = g["matcher"]
                    .as_str()
                    .map(|m| format!(" [{m}]"))
                    .unwrap_or_default();
                for h in g["hooks"].as_array().into_iter().flatten() {
                    let cond = h["if"]
                        .as_str()
                        .map(|c| format!(" if {c}"))
                        .unwrap_or_default();
                    let what = h["command"]
                        .as_str()
                        .or(h["url"].as_str())
                        .or(h["prompt"].as_str())
                        .unwrap_or("?");
                    out.push(format!("\n  {event}{matcher}{cond}: {what}"));
                }
            }
        }
    } else {
        let servers = if v["mcpServers"].is_object() {
            &v["mcpServers"]
        } else {
            v
        };
        for (name, s) in servers.as_object().into_iter().flatten() {
            let args = s["args"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Json::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            let what = s["command"]
                .as_str()
                .map(|c| format!("{c} {args}"))
                .or(s["url"].as_str().map(String::from))
                .unwrap_or_default();
            out.push(format!("\n  {name}: {}", what.trim()));
        }
    }
    if out.is_empty() {
        "none".into()
    } else {
        out.concat()
    }
}

/// Foreign configuration errors are diagnostic only; never expose file contents.
pub fn diagnostics(cwd: &Path, compat: &CompatConfig) -> Vec<String> {
    let mut out = Vec::new();
    for (enabled, p, json) in [
        (compat.codex, codex_dir().join("config.toml"), false),
        (compat.claude, home().join(".claude.json"), true),
        (compat.claude, claude_dir().join("settings.json"), true),
        (compat.codex, cwd.join(".codex/config.toml"), false),
        (compat.codex, codex_dir().join("hooks.json"), true),
        (compat.codex, cwd.join(".codex/hooks.json"), true),
        (compat.claude, cwd.join(".claude/settings.json"), true),
        (compat.claude, cwd.join(".claude/settings.local.json"), true),
    ] {
        if !p.exists() {
            continue;
        }
        if !enabled {
            out.push(format!("Skipped {} (compat disabled)", p.display()));
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&p) {
            let error = if json {
                serde_json::from_str::<Json>(&text)
                    .err()
                    .map(|e| format!("line {}, column {}", e.line(), e.column()))
            } else {
                toml::from_str::<Table>(&text).err().map(|e| {
                    let pos = e.span().map(|s| s.start).unwrap_or(0).min(text.len());
                    let prefix = &text[..text.floor_char_boundary(pos)];
                    format!(
                        "line {}, column {}",
                        prefix.bytes().filter(|b| *b == b'\n').count() + 1,
                        prefix.rsplit('\n').next().unwrap_or("").len() + 1
                    )
                })
            };
            if let Some(e) = error {
                out.push(format!(
                    "Ignored {}: parse error at {e}; fix the file or disable compat",
                    p.display()
                ));
            }
        }
    }
    out
}

// ---------------------------------------------------------------- other tools' instruction files

/// When an instruction file from another tool applies.
#[derive(Debug, Clone, PartialEq)]
pub enum Apply {
    /// Always in context.
    Always,
    /// Added the first time the agent touches a file matching one of these
    /// globs (relative to the directory holding the rule's tool folder).
    Globs(Vec<String>),
    /// Listed with its description; the model reads it when relevant
    /// (Cursor's "agent requested" rules).
    OnRequest(String),
}

#[derive(Debug, Clone)]
pub struct ForeignRule {
    pub path: PathBuf,
    /// The tool the file belongs to, for listings.
    pub tool: &'static str,
    pub apply: Apply,
}

/// User-level instruction files of other tools (always the user's own).
pub fn foreign_user_instructions(c: &CompatConfig) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if c.gemini {
        v.push(home().join(".gemini").join("GEMINI.md"));
    }
    if c.opencode {
        v.push(home().join(".config").join("opencode").join("AGENTS.md"));
    }
    v.retain(|p| p.is_file());
    v
}

/// Per-directory instruction files of other tools, loaded hierarchically like
/// AGENTS.md (Gemini CLI's GEMINI.md: from the repository root down, and in
/// subdirectories once the agent works there).
pub fn foreign_dir_files(dir: &Path, c: &CompatConfig) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if c.gemini {
        v.push(dir.join("GEMINI.md"));
    }
    v.retain(|p| p.is_file());
    v
}

/// Rule files other tools keep in `dir` (one directory between the
/// repository root and the working directory).
pub fn foreign_rules(dir: &Path, c: &CompatConfig) -> Vec<ForeignRule> {
    let mut out = rule_files(dir, c);
    if c.aider {
        out.extend(aider_reads(dir).into_iter().map(|path| ForeignRule {
            path,
            tool: "Aider",
            apply: Apply::Always,
        }));
    }
    if c.opencode {
        out.extend(
            opencode_instructions(dir)
                .into_iter()
                .map(|path| ForeignRule {
                    path,
                    tool: "opencode",
                    apply: Apply::Always,
                }),
        );
    }
    out
}

/// Cursor and Copilot rule files in `dir` (the only path-scoped kinds; cheap
/// enough to check whenever the agent touches a file).
pub fn rule_files(dir: &Path, c: &CompatConfig) -> Vec<ForeignRule> {
    let mut out = Vec::new();
    let mut push = |path: PathBuf, tool: &'static str, apply: Apply| {
        out.push(ForeignRule { path, tool, apply })
    };
    if c.cursor {
        let legacy = dir.join(".cursorrules");
        if legacy.is_file() {
            push(legacy, "Cursor", Apply::Always);
        }
        for p in files_ending(&dir.join(".cursor").join("rules"), &[".mdc", ".md"]) {
            if let Some(a) = std::fs::read_to_string(&p)
                .ok()
                .and_then(|t| cursor_apply(&t))
            {
                push(p, "Cursor", a);
            }
        }
    }
    if c.copilot {
        let p = dir.join(".github").join("copilot-instructions.md");
        if p.is_file() {
            push(p, "Copilot", Apply::Always);
        }
        for p in files_ending(
            &dir.join(".github").join("instructions"),
            &[".instructions.md"],
        ) {
            if let Some(a) = std::fs::read_to_string(&p)
                .ok()
                .and_then(|t| copilot_apply(&t))
            {
                push(p, "Copilot", a);
            }
        }
    }
    out
}

/// Cursor rule frontmatter: `alwaysApply: true` → always; `globs` → when a
/// matching file is touched; only a `description` → listed for the model;
/// none (a manual `@rule`) → not loaded.
pub fn cursor_apply(text: &str) -> Option<Apply> {
    let fm = crate::context::frontmatter(text);
    if crate::context::fm_get(&fm, "alwaysApply").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
        return Some(Apply::Always);
    }
    if let Some(Ok(g)) = crate::context::fm_list(&fm, "globs")
        && !g.is_empty()
    {
        return Some(Apply::Globs(g));
    }
    crate::context::fm_get(&fm, "description").map(Apply::OnRequest)
}

/// Copilot `*.instructions.md`: `applyTo` globs (`**` = everywhere). Files
/// without `applyTo` are attached by hand in Copilot, so not loaded.
pub fn copilot_apply(text: &str) -> Option<Apply> {
    let fm = crate::context::frontmatter(text);
    let globs = crate::context::fm_list(&fm, "applyTo")?.ok()?;
    if globs
        .iter()
        .any(|g| matches!(g.as_str(), "**" | "*" | "**/*"))
    {
        return Some(Apply::Always);
    }
    (!globs.is_empty()).then_some(Apply::Globs(globs))
}

/// Files below `dir` (recursively, sorted) whose names end with a suffix.
fn files_ending(dir: &Path, suffixes: &[&str]) -> Vec<PathBuf> {
    fn walk(dir: &Path, suffixes: &[&str], depth: usize, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in paths {
            let name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            if p.is_dir() {
                if depth < 4 && !name.starts_with('.') {
                    walk(&p, suffixes, depth + 1, out);
                }
            } else if suffixes.iter().any(|s| name.ends_with(s)) {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, suffixes, 0, &mut out);
    out
}

/// A path a project file names, if it stays inside the project directory.
fn inside(dir: &Path, rel: &str) -> Option<PathBuf> {
    if rel.contains("://") || rel.starts_with('~') {
        return None;
    }
    let p = dir.join(rel);
    let root = dir.canonical().ok()?;
    (p.is_file() && p.canonical().ok()?.starts_with(&root)).then_some(p)
}

/// Aider: files `.aider.conf.yml` lists under `read:` (a name or a list),
/// typically CONVENTIONS.md. Aider loads nothing it isn't told to.
pub fn aider_reads(dir: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(dir.join(".aider.conf.yml")) else {
        return vec![];
    };
    let fm = crate::context::frontmatter(&format!("---\n{text}\n---\n"));
    let Some(Ok(files)) = crate::context::fm_list(&fm, "read") else {
        return vec![];
    };
    files.iter().filter_map(|f| inside(dir, f)).collect()
}

/// opencode: `instructions` in `opencode.json` (or `.jsonc`): paths or globs
/// relative to the project. URLs are skipped.
pub fn opencode_instructions(dir: &Path) -> Vec<PathBuf> {
    let Some(v) = ["opencode.json", "opencode.jsonc"]
        .iter()
        .find_map(|n| read_jsonc(&dir.join(n)))
    else {
        return vec![];
    };
    let mut out = Vec::new();
    for entry in v["instructions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Json::as_str)
    {
        if entry.contains(['*', '?', '[', '{']) {
            let Ok(glob) = globset::Glob::new(entry) else {
                continue;
            };
            let m = glob.compile_matcher();
            let mut found: Vec<PathBuf> = ignore::WalkBuilder::new(dir)
                .max_depth(Some(8))
                .build()
                .flatten()
                .take(5000)
                .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
                .map(|e| e.into_path())
                .filter(|p| p.strip_prefix(dir).is_ok_and(|r| m.is_match(r)))
                .collect();
            found.sort();
            out.extend(found);
        } else if let Some(p) = inside(dir, entry) {
            out.push(p);
        }
    }
    out
}

/// JSON with `//` line comments (opencode.jsonc).
fn read_jsonc(p: &Path) -> Option<Json> {
    let text = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&text).ok().or_else(|| {
        let stripped: Vec<&str> = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect();
        serde_json::from_str(&stripped.join("\n")).ok()
    })
}

/// opencode subagents: `.opencode/agent/*.md` (or `agents/`) in `dirs`, after
/// `~/.config/opencode/agent`. `mode: primary` agents are top-level modes in
/// opencode, not subagents, so they're skipped. A `tools:` map of
/// `name: false` entries becomes `disallowedTools`.
pub fn opencode_agents(dirs: &[PathBuf]) -> Vec<crate::context::AgentDef> {
    let user = home().join(".config").join("opencode");
    let mut out: Vec<crate::context::AgentDef> = Vec::new();
    for base in std::iter::once(&user).chain(dirs.iter()) {
        for sub in ["agent", "agents"] {
            for p in files_ending(&base.join(sub), &[".md"]) {
                let Ok(text) = std::fs::read_to_string(&p) else {
                    continue;
                };
                if let Some(mut a) = opencode_agent(&p, &text) {
                    a.project = base != &user;
                    out.retain(|o| o.name != a.name);
                    out.push(a);
                }
            }
        }
    }
    out
}

fn opencode_agent(p: &Path, text: &str) -> Option<crate::context::AgentDef> {
    let fm = crate::context::frontmatter(text);
    if crate::context::fm_get(&fm, "mode").as_deref() == Some("primary")
        || crate::context::fm_get(&fm, "disable").as_deref() == Some("true")
    {
        return None;
    }
    // `tools:` is a map (`write: false`), which the frontmatter parser skips.
    let mut disallowed = Vec::new();
    let head = text
        .strip_prefix("---")
        .and_then(|r| r.find("\n---").map(|e| &r[..e]))
        .unwrap_or("");
    let mut in_tools = false;
    for line in head.lines() {
        if !line.starts_with([' ', '\t']) {
            in_tools = line.trim_end() == "tools:";
            continue;
        }
        let Some((k, v)) = line.trim().split_once(':').filter(|_| in_tools) else {
            continue;
        };
        if v.trim() != "false" {
            continue;
        }
        let name = match k.trim().trim_matches('"') {
            "bash" => "Bash",
            "edit" | "patch" => "Edit",
            "write" => "Write",
            "read" => "Read",
            "grep" => "Grep",
            "glob" => "Glob",
            "list" => "Ls",
            "webfetch" => "WebFetch",
            "todowrite" => "TodoWrite",
            _ => continue,
        };
        disallowed.push(name.to_string());
    }
    Some(crate::context::AgentDef {
        name: p.file_stem()?.to_string_lossy().to_string(),
        description: crate::context::fm_get(&fm, "description").unwrap_or_default(),
        tools: None,
        disallowed_tools: disallowed,
        model: crate::context::fm_get(&fm, "model"),
        prompt: crate::context::strip_frontmatter(text).to_string(),
        path: p.to_path_buf(),
        ..Default::default()
    })
}

#[cfg(test)]
mod foreign_tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("chanto-foreign-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn w(d: &Path, p: &str, t: &str) {
        let p = d.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, t).unwrap();
    }

    #[test]
    fn cursor_rule_kinds() {
        assert_eq!(
            cursor_apply("---\nalwaysApply: true\nglobs: *.ts\n---\nx"),
            Some(Apply::Always)
        );
        assert_eq!(
            cursor_apply(
                "---\ndescription:\nglobs: *.ts, src/**/*.{tsx,jsx}\nalwaysApply: false\n---\nx"
            ),
            Some(Apply::Globs(vec![
                "*.ts".into(),
                "src/**/*.{tsx,jsx}".into()
            ]))
        );
        assert_eq!(
            cursor_apply("---\ndescription: API style guide\nalwaysApply: false\n---\nx"),
            Some(Apply::OnRequest("API style guide".into()))
        );
        assert_eq!(cursor_apply("---\nalwaysApply: false\n---\nmanual"), None);
        assert_eq!(cursor_apply("no frontmatter"), None);
    }

    #[test]
    fn copilot_apply_to() {
        assert_eq!(
            copilot_apply("---\napplyTo: \"**\"\n---\nx"),
            Some(Apply::Always)
        );
        assert_eq!(
            copilot_apply("---\napplyTo: \"**/*.ts,**/*.tsx\"\n---\nx"),
            Some(Apply::Globs(vec!["**/*.ts".into(), "**/*.tsx".into()]))
        );
        assert_eq!(copilot_apply("---\ndescription: x\n---\nx"), None);
    }

    #[test]
    fn rules_from_every_tool() {
        let d = tmp();
        w(&d, ".cursorrules", "legacy");
        w(
            &d,
            ".cursor/rules/ts.mdc",
            "---\nglobs: web/**/*.ts\n---\nts",
        );
        w(
            &d,
            ".cursor/rules/sub/always.mdc",
            "---\nalwaysApply: true\n---\nalways",
        );
        w(&d, ".github/copilot-instructions.md", "copilot");
        w(
            &d,
            ".github/instructions/py.instructions.md",
            "---\napplyTo: '**/*.py'\n---\npy",
        );
        w(
            &d,
            ".aider.conf.yml",
            "model: x\nread:\n  - CONVENTIONS.md\n  - ../outside.md\n",
        );
        w(&d, "CONVENTIONS.md", "conventions");
        w(&d, "NOT_READ.md", "x");
        w(
            &d,
            "opencode.json",
            r#"{"instructions": ["CONTRIBUTING.md", "docs/*.md", "https://example.com/x.md"]}"#,
        );
        w(&d, "CONTRIBUTING.md", "contrib");
        w(&d, "docs/a.md", "a");
        let rules = foreign_rules(&d, &CompatConfig::default());
        let names: Vec<String> = rules
            .iter()
            .map(|r| {
                r.path
                    .strip_prefix(&d)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(
            names,
            [
                ".cursorrules",
                ".cursor/rules/sub/always.mdc",
                ".cursor/rules/ts.mdc",
                ".github/copilot-instructions.md",
                ".github/instructions/py.instructions.md",
                "CONVENTIONS.md",
                "CONTRIBUTING.md",
                "docs/a.md"
            ]
        );
        assert_eq!(rules[2].apply, Apply::Globs(vec!["web/**/*.ts".into()]));
        assert_eq!(rules[4].apply, Apply::Globs(vec!["**/*.py".into()]));
        // Each source has its own switch.
        let off = CompatConfig {
            cursor: false,
            copilot: false,
            aider: false,
            opencode: false,
            ..Default::default()
        };
        assert!(foreign_rules(&d, &off).is_empty());
        w(&d, "GEMINI.md", "gemini");
        assert_eq!(foreign_dir_files(&d, &CompatConfig::default()).len(), 1);
        assert!(
            foreign_dir_files(
                &d,
                &CompatConfig {
                    gemini: false,
                    ..Default::default()
                }
            )
            .is_empty()
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn opencode_agent_files() {
        let text = "---\ndescription: Reviews code\nmodel: anthropic/claude-sonnet-4\ntools:\n  write: false\n  bash: false\n  read: true\n---\nYou review.";
        let a = opencode_agent(Path::new("/x/.opencode/agent/review.md"), text).unwrap();
        assert_eq!(
            (a.name.as_str(), a.description.as_str(), a.model.as_deref()),
            ("review", "Reviews code", Some("anthropic/claude-sonnet-4"))
        );
        assert_eq!(a.disallowed_tools, vec!["Write", "Bash"]);
        assert!(a.tools.is_none() && a.prompt.trim() == "You review.");
        assert!(opencode_agent(Path::new("b.md"), "---\nmode: primary\n---\nx").is_none());
    }
}
