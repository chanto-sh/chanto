//! Claude Code's launch flags that change what a session loads or how it
//! starts: `--settings`, `--setting-sources`, `--mcp-config`,
//! `--strict-mcp-config`, `--bare`, `--plugin-dir`, `--agents <json>`,
//! `--tools`, `--system-prompt[-file]`, `--append-system-prompt-file`,
//! `--session-id`, `--permission-prompt-tool`.
//!
//! The parts that config loading and directory scans need are kept in a
//! process-wide [`Launch`], set once at startup (tests pass one explicitly).

use crate::config::McpServerConfig;
use crate::context::AgentDef;
use anyhow::{Context, Result, bail};
use serde_json::Value as Json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What the command line asked to load, beyond the usual discovery.
#[derive(Debug, Clone, Default)]
pub struct Launch {
    /// `--settings`: (source label, Chanto config table), above every file
    /// layer except managed settings.
    pub settings: Vec<(String, toml::Table)>,
    /// `--setting-sources`: which of `user`, `project`, `local` to read
    /// (None: all of them).
    pub sources: Option<Vec<String>>,
    /// `--mcp-config`: (source label, servers).
    pub mcp: Vec<(String, BTreeMap<String, McpServerConfig>)>,
    /// `--strict-mcp-config`: only the `--mcp-config` servers.
    pub strict_mcp: bool,
    /// `--bare`: no auto-discovery of hooks, skills, commands, subagents,
    /// plugins, MCP servers, memory or instruction files.
    pub bare: bool,
    /// `--safe-mode`: the same discovery is skipped (plus custom themes,
    /// keybindings, the status line and language servers), but the built-in
    /// tools, the model and permissions stay as configured.
    pub safe_mode: bool,
    /// `--restricted`: no tools that run code, file tools kept to the
    /// working directories, only managed settings and `--settings`, no
    /// bypass mode and no cloud runs.
    pub restricted: bool,
    /// `--plugin-dir`: plugins for this session only.
    pub plugins: Vec<crate::compat::Plugin>,
    /// `--permission-prompt-tool`: the MCP tool that answers permission
    /// prompts in print mode.
    pub permission_prompt_tool: Option<String>,
    /// `--add-dir`: extra working directories for the whole session (as
    /// given; relative to the starting directory). They survive `/cd`.
    pub add_dirs: Vec<String>,
}

static LAUNCH: std::sync::OnceLock<Launch> = std::sync::OnceLock::new();

/// Record this process's launch flags (once, at startup).
pub fn set(l: Launch) {
    let _ = LAUNCH.set(l);
}

/// This process's launch flags (the defaults when none were set).
pub fn get() -> &'static Launch {
    static DEFAULT: std::sync::OnceLock<Launch> = std::sync::OnceLock::new();
    LAUNCH
        .get()
        .unwrap_or_else(|| DEFAULT.get_or_init(Launch::default))
}

impl Launch {
    /// Should config from this source (`user`, `project`, `local`) be read?
    pub fn wants(&self, source: &str) -> bool {
        self.sources
            .as_ref()
            .is_none_or(|s| s.iter().any(|x| x == source))
    }

    /// Hooks and MCP servers found on disk (not given on the command line) are skipped.
    pub fn skip_discovered_mcp(&self) -> bool {
        self.no_customizations() || self.strict_mcp
    }

    /// `--bare` or `--safe-mode`: nothing customizable is read from disk
    /// (hooks, skills, commands, subagents, plugins, MCP servers, memory,
    /// instruction files); only what the command line passes.
    pub fn no_customizations(&self) -> bool {
        self.bare || self.safe_mode
    }
}

/// A flag value that is either inline JSON or a path to a file holding it.
/// Returns (label for messages and `doctor`, text).
pub fn file_or_inline(flag: &str, value: &str) -> Result<(String, String)> {
    let t = value.trim_start();
    if t.starts_with('{') || t.starts_with('[') {
        return Ok((format!("{flag} (inline)"), value.to_string()));
    }
    let p = Path::new(value);
    let meta = std::fs::metadata(p).with_context(|| format!("{flag}: can't read {value}"))?;
    if !meta.is_file() {
        bail!("{flag}: {value} is not a regular file");
    }
    // Claude Code's limit for --settings files; plenty for the others too.
    if meta.len() > 2 * 1024 * 1024 {
        bail!("{flag}: {value} is larger than 2 MiB");
    }
    Ok((
        format!("{flag} {value}"),
        std::fs::read_to_string(p).with_context(|| format!("{flag}: can't read {value}"))?,
    ))
}

/// `--settings <file-or-json>`: Claude Code settings JSON (or, for a `.toml`
/// file, Chanto config) as a config layer.
pub fn parse_settings(value: &str) -> Result<(String, toml::Table)> {
    let (label, text) = file_or_inline("--settings", value)?;
    if value.ends_with(".toml") && !value.trim_start().starts_with('{') {
        let t: toml::Table =
            toml::from_str(&text).with_context(|| format!("{label}: not valid TOML"))?;
        return Ok((label, t));
    }
    let v: Json =
        serde_json::from_str(&text).with_context(|| format!("{label}: not valid JSON"))?;
    if !v.is_object() {
        bail!("{label}: settings must be a JSON object");
    }
    Ok((label, crate::compat::cli_settings_layer(&v)))
}

/// `--setting-sources user,project,local`.
pub fn parse_sources(value: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for s in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if !matches!(s, "user" | "project" | "local") {
            bail!("--setting-sources: unknown source {s:?} (use user, project, local)");
        }
        out.push(s.to_string());
    }
    Ok(out)
}

/// `--mcp-config <file-or-json>`: `{"mcpServers": {...}}` (as in `.mcp.json`)
/// or just the servers object.
pub fn parse_mcp_config(value: &str) -> Result<(String, BTreeMap<String, McpServerConfig>)> {
    let (label, text) = file_or_inline("--mcp-config", value)?;
    let v: Json =
        serde_json::from_str(&text).with_context(|| format!("{label}: not valid JSON"))?;
    let servers = v.get("mcpServers").unwrap_or(&v);
    let Some(obj) = servers.as_object() else {
        bail!("{label}: expected {{\"mcpServers\": {{...}}}}")
    };
    let mut out = BTreeMap::new();
    for (name, s) in obj {
        let sc: McpServerConfig =
            serde_json::from_value(s.clone()).with_context(|| format!("{label}: server {name}"))?;
        if sc.command.is_empty() && sc.url.is_none() {
            bail!("{label}: server {name} needs a `command` or a `url`");
        }
        out.insert(name.clone(), sc);
    }
    Ok((label, out))
}

/// `--plugin-dir <path>`: a plugin directory (with `.claude-plugin/plugin.json`
/// or a `commands`/`skills`/`agents`/`hooks` folder), or a folder of them.
pub fn parse_plugin_dir(value: &str) -> Result<Vec<crate::compat::Plugin>> {
    let p = PathBuf::from(value);
    if p.extension().and_then(|e| e.to_str()) == Some("zip") {
        bail!(
            "--plugin-dir: .zip archives aren't supported yet; unpack {value} and pass the directory"
        );
    }
    if !p.is_dir() {
        bail!("--plugin-dir: {value} is not a directory");
    }
    let root = std::fs::canonicalize(&p).unwrap_or(p);
    if let Some(plugin) = crate::compat::plugin_from_dir(&root) {
        return Ok(vec![plugin]);
    }
    let mut subs: Vec<PathBuf> = std::fs::read_dir(&root)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    subs.sort();
    let plugins: Vec<_> = subs
        .iter()
        .filter_map(|d| crate::compat::plugin_from_dir(d))
        .collect();
    if plugins.is_empty() {
        bail!(
            "--plugin-dir: no plugin in {value} (expected .claude-plugin/plugin.json, or commands/, skills/, agents/ or hooks/)"
        );
    }
    Ok(plugins)
}

/// What `--agents` holds: Chanto's team size, or Claude Code's subagent definitions.
#[derive(Debug, Clone)]
pub enum AgentsFlag {
    Team(usize),
    Defs(Vec<AgentDef>),
}

/// `--agents N` (Chanto: team size) or `--agents '<json>'` (Claude Code:
/// `{"name": {"description", "prompt", "tools", "disallowedTools", "model"}}`).
/// A path to a JSON file works too.
pub fn parse_agents(value: &str) -> Result<AgentsFlag> {
    if let Ok(n) = value.trim().parse::<usize>() {
        return Ok(AgentsFlag::Team(n));
    }
    let (label, text) = file_or_inline("--agents", value)
        .context("--agents takes a number (team size) or subagent definitions as JSON")?;
    let v: Json = serde_json::from_str(&text)
        .with_context(|| format!("Invalid --agents configuration: {label} is not valid JSON"))?;
    let Some(obj) = v.as_object() else {
        bail!("Invalid --agents configuration: expected a JSON object of subagents by name")
    };
    let list = |v: &Json, field: &str, name: &str| -> Result<Option<Vec<String>>> {
        match v {
            Json::Null => Ok(None),
            Json::String(s) => Ok(Some(
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect(),
            )),
            Json::Array(a) => a
                .iter()
                .map(|t| {
                    t.as_str().map(str::to_string).ok_or_else(|| {
                        anyhow::anyhow!(
                            "Invalid --agents configuration: {name}.{field} must list tool names"
                        )
                    })
                })
                .collect::<Result<Vec<_>>>()
                .map(Some),
            _ => {
                bail!("Invalid --agents configuration: {name}.{field} must be a string or an array")
            }
        }
    };
    let mut defs = Vec::new();
    for (name, d) in obj {
        if !d.is_object() {
            bail!("Invalid --agents configuration: {name} must be an object");
        }
        let text = |k: &str| d.get(k).and_then(Json::as_str).map(str::to_string);
        let (Some(description), Some(prompt)) = (text("description"), text("prompt")) else {
            bail!("Invalid --agents configuration: {name} needs a `description` and a `prompt`");
        };
        defs.push(AgentDef {
            name: name.clone(),
            description,
            tools: list(&d["tools"], "tools", name)?,
            disallowed_tools: list(&d["disallowedTools"], "disallowedTools", name)?
                .unwrap_or_default(),
            model: text("model"),
            prompt,
            ..Default::default()
        });
        // The frontmatter fields, under the same keys.
        crate::context::agent_fields_from_json(defs.last_mut().unwrap(), d);
    }
    Ok(AgentsFlag::Defs(defs))
}

/// `--agent <name>`'s `initialPrompt`, from `--agents` JSON or the agent files.
pub fn initial_prompt(cwd: &std::path::Path, agents: Option<&str>, name: &str) -> Option<String> {
    let mut defs = crate::context::load_agents(cwd);
    if let Some(Ok(AgentsFlag::Defs(d))) = agents.map(parse_agents) {
        defs.retain(|a| !d.iter().any(|x| x.name.eq_ignore_ascii_case(&a.name)));
        defs.extend(d);
    }
    crate::util::find_named(&defs, name, |a| &a.name)?
        .initial_prompt
        .clone()
        .filter(|p| !p.trim().is_empty())
}

/// `--tools`: None for every built-in tool (`default`), otherwise the names
/// to keep (empty: none).
pub fn parse_tools(value: &str) -> Option<Vec<String>> {
    let v = value.trim();
    if v.eq_ignore_ascii_case("default") {
        return None;
    }
    Some(
        v.split([',', ' '])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// The tools a `--bare` session gets unless `--tools` says otherwise.
pub const BARE_TOOLS: &[&str] = &["Bash", "Read", "Edit"];

/// The built-in tools `--restricted` removes unless `--tools` names them:
/// everything that runs commands or code (shells, the browser, computer
/// use, cloud runs) and WebFetch, as in Claude Code.
pub const RESTRICTED_TOOLS: &[&str] = &[
    "Bash",
    "BashOutput",
    "KillShell",
    "Monitor",
    "Browser",
    "Computer",
    "ChantoCloud",
    "WebFetch",
];

/// The built-in tools a session keeps: `--tools` decides when given (`None`
/// means all of them), else `--bare`'s set, else all but `--restricted`'s.
/// `known` is every built-in tool's name.
pub fn kept_tools(
    tools: Option<&str>,
    bare: bool,
    restricted: bool,
    known: &[String],
) -> Option<Vec<String>> {
    match tools {
        Some(t) => parse_tools(t),
        None if bare => Some(
            BARE_TOOLS
                .iter()
                .map(|s| s.to_string())
                .filter(|t| !restricted || !RESTRICTED_TOOLS.contains(&t.as_str()))
                .collect(),
        ),
        None if restricted => Some(
            known
                .iter()
                .filter(|n| !RESTRICTED_TOOLS.iter().any(|r| r.eq_ignore_ascii_case(n)))
                .cloned()
                .collect(),
        ),
        None => None,
    }
}

/// `--safe-mode` (or `CLAUDE_CODE_SAFE_MODE=1`) and `--restricted` (or
/// `CLAUDE_CODE_RESTRICTED=1`), from the flags or the environment.
pub fn env_flag(var: &str) -> bool {
    std::env::var(var).is_ok_and(|v| matches!(v.trim(), "1" | "true" | "yes"))
}

/// Text for a system prompt flag: the value, or a file's contents.
pub fn read_prompt_file(flag: &str, path: &str) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("{flag}: can't read {path}"))
}

/// `--session-id`: must be a UUID, as in Claude Code.
pub fn parse_session_id(value: &str) -> Result<String> {
    let id = uuid::Uuid::parse_str(value.trim())
        .map_err(|_| anyhow::anyhow!("--session-id must be a valid UUID (got {value:?})"))?;
    Ok(id.hyphenated().to_string())
}

#[cfg(test)]
mod tests;
