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
    let d = std::env::temp_dir().join(format!("chanto-managed-{}", uuid::Uuid::new_v4().simple()));
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
        &serde_json::json!({"mkt": {"installLocation": mloc.display().to_string()}}).to_string(),
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
    let mut server = serde_json::json!({"command": "srv", "args": ["${CLAUDE_PLUGIN_DATA}/db"]});
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
