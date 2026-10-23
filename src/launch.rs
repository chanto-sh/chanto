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
mod tests {
    use super::*;

    #[test]
    fn restricted_drops_code_running_tools_unless_named() {
        let known: Vec<String> = [
            "Bash",
            "BashOutput",
            "Read",
            "Edit",
            "WebFetch",
            "WebSearch",
            "Browser",
            "Monitor",
            "Grep",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(kept_tools(None, false, false, &known), None);
        assert_eq!(
            kept_tools(None, false, true, &known).unwrap(),
            vec!["Read", "Edit", "WebSearch", "Grep"]
        );
        // --tools still names what it wants, Bash included.
        assert_eq!(
            kept_tools(Some("Bash,Read"), false, true, &known).unwrap(),
            vec!["Bash", "Read"]
        );
        assert_eq!(
            kept_tools(None, true, false, &known).unwrap(),
            vec!["Bash", "Read", "Edit"]
        );
        assert_eq!(
            kept_tools(None, true, true, &known).unwrap(),
            vec!["Read", "Edit"]
        );
        let safe = Launch {
            safe_mode: true,
            ..Default::default()
        };
        assert!(safe.no_customizations() && safe.skip_discovered_mcp() && safe.wants("user"));
        assert!(
            !Launch {
                restricted: true,
                ..Default::default()
            }
            .no_customizations()
        );
    }

    #[test]
    fn agents_flag_is_a_team_size_or_subagent_json() {
        assert!(matches!(parse_agents("3").unwrap(), AgentsFlag::Team(3)));
        let AgentsFlag::Defs(d) = parse_agents(
            r#"{"reviewer":{"description":"Reviews code","prompt":"You review.","tools":["Read","Grep"],"model":"inherit"},
                "fixer":{"description":"Fixes","prompt":"You fix.","tools":"Read, Edit","disallowedTools":"Bash"}}"#,
        )
        .unwrap() else {
            panic!("expected definitions")
        };
        let r = d.iter().find(|a| a.name == "reviewer").unwrap();
        assert_eq!(
            r.tools.as_deref(),
            Some(&["Read".to_string(), "Grep".to_string()][..])
        );
        assert_eq!(r.model.as_deref(), Some("inherit"));
        let f = d.iter().find(|a| a.name == "fixer").unwrap();
        assert_eq!(
            f.tools.as_deref(),
            Some(&["Read".to_string(), "Edit".to_string()][..])
        );
        assert_eq!(f.disallowed_tools, vec!["Bash".to_string()]);
        for bad in [
            r#"{"x":{"prompt":"p"}}"#,
            r#"["x"]"#,
            r#"{"x":{"description":"d","prompt":"p","tools":3}}"#,
            "{nope",
            "no-such-file.json",
        ] {
            assert!(parse_agents(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn agents_json_takes_the_frontmatter_fields() {
        let AgentsFlag::Defs(d) = parse_agents(
            r#"{"tester":{"description":"Tests","prompt":"You test.","permissionMode":"acceptEdits","maxTurns":7,"skills":["conventions"],
                "mcpServers":["github",{"playwright":{"type":"stdio","command":"npx","args":["-y","@playwright/mcp"]}}],
                "hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"./check.sh"}]}]},
                "memory":"project","background":true,"effort":"high","isolation":"worktree","initialPrompt":"/review","color":"Cyan","omitClaudeMd":true}}"#,
        )
        .unwrap() else {
            panic!("expected definitions")
        };
        let t = &d[0];
        assert_eq!(
            (
                t.permission_mode.as_deref(),
                t.max_turns,
                t.memory.as_deref(),
                t.effort.as_deref()
            ),
            (Some("acceptEdits"), Some(7), Some("project"), Some("high"))
        );
        assert_eq!(t.skills, vec!["conventions".to_string()]);
        assert!(matches!(&t.mcp_servers[0], crate::context::AgentMcp::Named(n) if n == "github"));
        assert!(
            matches!(&t.mcp_servers[1], crate::context::AgentMcp::Inline(n, c) if n == "playwright" && c.command == "npx")
        );
        assert_eq!(
            t.hooks.as_ref().unwrap()["PreToolUse"][0].command,
            "./check.sh"
        );
        assert!(t.background && t.omit_claude_md);
        assert_eq!(
            (
                t.isolation.as_deref(),
                t.initial_prompt.as_deref(),
                t.color.as_deref()
            ),
            (Some("worktree"), Some("/review"), Some("cyan"))
        );
        let dir =
            std::env::temp_dir().join(format!("chanto-initial-{}", uuid::Uuid::new_v4().simple()));
        let json = r#"{"tester":{"description":"Tests","prompt":"p","initialPrompt":"/review"}}"#;
        assert_eq!(
            initial_prompt(&dir, Some(json), "tester").as_deref(),
            Some("/review")
        );
        assert_eq!(initial_prompt(&dir, Some(json), "other"), None);
    }

    #[test]
    fn agents_json_can_come_from_a_file() {
        let d = std::env::temp_dir().join(format!(
            "chanto-agents-{}.json",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&d, r#"{"r":{"description":"d","prompt":"p"}}"#).unwrap();
        assert!(
            matches!(parse_agents(d.to_str().unwrap()).unwrap(), AgentsFlag::Defs(v) if v.len() == 1)
        );
        let _ = std::fs::remove_file(d);
    }

    #[test]
    fn settings_and_mcp_config_take_json_or_files() {
        let (_, t) = parse_settings(r#"{"permissions":{"allow":["Bash(ls:*)"],"defaultMode":"acceptEdits"},"model":"m1","env":{"A":"1"}}"#).unwrap();
        assert_eq!(t["mode"].as_str(), Some("acceptEdits"));
        assert_eq!(t["model"].as_str(), Some("m1"));
        assert_eq!(
            t["permissions"]["allow"].as_array().unwrap()[0].as_str(),
            Some("Bash(ls:*)")
        );
        assert!(parse_settings("[1]").is_err());
        let f = std::env::temp_dir().join(format!(
            "chanto-settings-{}.toml",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&f, "num_ctx = 777\n").unwrap();
        let (_, t) = parse_settings(f.to_str().unwrap()).unwrap();
        assert_eq!(t["num_ctx"].as_integer(), Some(777));
        let _ = std::fs::remove_file(f);

        let (_, s) = parse_mcp_config(r#"{"mcpServers":{"fs":{"command":"npx","args":["-y","x"]},"web":{"type":"http","url":"http://h/mcp"}}}"#).unwrap();
        assert_eq!(s["fs"].command, "npx");
        assert_eq!(s["web"].url.as_deref(), Some("http://h/mcp"));
        let (_, s) = parse_mcp_config(r#"{"fs":{"command":"npx"}}"#).unwrap();
        assert!(s.contains_key("fs"));
        assert!(parse_mcp_config(r#"{"mcpServers":{"x":{}}}"#).is_err());
        assert!(parse_mcp_config("/no/such/mcp.json").is_err());
    }

    #[test]
    fn sources_tools_and_session_ids() {
        assert_eq!(parse_sources("user, local").unwrap(), vec!["user", "local"]);
        assert!(parse_sources("user,global").is_err());
        let l = Launch {
            sources: Some(vec!["user".into()]),
            ..Default::default()
        };
        assert!(l.wants("user") && !l.wants("project") && Launch::default().wants("local"));
        assert_eq!(parse_tools("default"), None);
        assert_eq!(parse_tools(""), Some(vec![]));
        assert_eq!(
            parse_tools("Bash,Edit Read"),
            Some(vec!["Bash".to_string(), "Edit".into(), "Read".into()])
        );
        assert_eq!(
            parse_session_id("550E8400-E29B-41D4-A716-446655440000").unwrap(),
            "550e8400-e29b-41d4-a716-446655440000"
        );
        assert!(parse_session_id("abc").is_err());
    }

    #[test]
    fn plugin_dirs_load_one_plugin_or_a_folder_of_them() {
        let d =
            std::env::temp_dir().join(format!("chanto-plugdir-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(d.join("one/.claude-plugin")).unwrap();
        std::fs::write(
            d.join("one/.claude-plugin/plugin.json"),
            r#"{"name":"uno"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(d.join("two/commands")).unwrap();
        std::fs::create_dir_all(d.join("not-a-plugin")).unwrap();
        let one = parse_plugin_dir(d.join("one").to_str().unwrap()).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].name, "uno");
        let all = parse_plugin_dir(d.to_str().unwrap()).unwrap();
        assert_eq!(
            all.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            vec!["uno", "two"]
        );
        assert!(parse_plugin_dir(d.join("not-a-plugin").to_str().unwrap()).is_err());
        assert!(parse_plugin_dir("x.zip").is_err());
        let _ = std::fs::remove_dir_all(d);
    }
}
