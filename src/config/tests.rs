/// Someone who copies the template must get the defaults, not switch
/// something off: every setting it spells out says what happens anyway.
#[test]
fn saving_the_network_policy_keeps_the_rest_of_the_config() {
    let d = std::env::temp_dir().join(format!("chanto-net-{}", uuid::Uuid::new_v4().simple()));
    let p = d.join("config.toml");
    persist_sandbox_network(&p, "allowlist").unwrap();
    assert!(
        std::fs::read_to_string(&p)
            .unwrap()
            .contains("network = \"allowlist\"")
    );
    std::fs::write(
        &p,
        "model = \"m\" # mine\n[sandbox]\nnetwork = \"deny\"\nallow_write = [\"/x\"]\n",
    )
    .unwrap();
    persist_sandbox_network(&p, "allow").unwrap();
    let text = std::fs::read_to_string(&p).unwrap();
    assert!(
        text.contains("network = \"allow\"") && !text.contains("deny"),
        "{text}"
    );
    assert!(
        text.contains("# mine") && text.contains("allow_write"),
        "{text}"
    );
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn the_example_config_only_states_defaults() {
    let parsed: Config = toml::from_str(EXAMPLE_CONFIG).unwrap();
    let (parsed, default) = (
        serde_json::to_value(parsed).unwrap(),
        serde_json::to_value(Config::default()).unwrap(),
    );
    for (section, default) in default.as_object().unwrap() {
        if default.is_object() {
            assert_eq!(parsed[section], *default, "[{section}] in EXAMPLE_CONFIG");
        }
    }
}

use super::*;

#[test]
fn trust_does_not_cascade_from_broad_roots() {
    let home = home_dir_real().unwrap_or_else(|| PathBuf::from("/home/u"));
    let project = home.join("code/app");
    // A trusted project covers itself and its subtree.
    assert!(trust_covers(&project, &project));
    assert!(trust_covers(&project, &project.join("src/lib.rs")));
    // Trusting the home directory (or an ancestor, or /) does not.
    assert!(!trust_covers(&home, &project));
    assert!(!trust_covers(Path::new("/"), &project));
    if let Some(parent) = home.parent() {
        assert!(!trust_covers(parent, &project));
    }
    // But the broad path itself is still trusted when it is the cwd.
    assert!(trust_covers(&home, &home));
}

#[test]
fn home_dir_follows_xdg_unless_legacy_exists() {
    let h = std::env::temp_dir().join(format!("chanto-xdg-{}", uuid::Uuid::new_v4().simple()));
    let xdg = h.join("xdg");
    assert_eq!(resolve_home(Some(&h), None), h.join(".chanto"));
    assert_eq!(resolve_home(Some(&h), Some(&xdg)), xdg.join("chanto"));
    // Relative XDG paths are ignored, per the spec.
    assert_eq!(
        resolve_home(Some(&h), Some(Path::new("rel"))),
        h.join(".chanto")
    );
    // An existing ~/.chanto keeps working.
    std::fs::create_dir_all(h.join(".chanto")).unwrap();
    assert_eq!(resolve_home(Some(&h), Some(&xdg)), h.join(".chanto"));
    let _ = std::fs::remove_dir_all(&h);
}

#[test]
fn updates_merge_preserves_opt_out_in_unrelated_layers() {
    let mut cfg = super::Config::default();
    let raw: toml::Table = toml::from_str("[updates]\ncheck = false").unwrap();
    let layer = toml::from_str("[updates]\ncheck = false").unwrap();
    cfg.merge(layer, &raw);
    cfg.merge(super::Config::default(), &toml::Table::new());
    assert!(!cfg.updates.check);
}

#[test]
fn copy_on_select_accepts_both_spellings() {
    let camel: Config = toml::from_str("copyOnSelect = false").unwrap();
    assert_eq!(camel.copy_on_select, Some(false));
    let snake: Config = toml::from_str("copy_on_select = true").unwrap();
    assert_eq!(snake.copy_on_select, Some(true));
}

#[test]
fn optimizations_are_off_by_default_and_layer() {
    let mut cfg = Config::default();
    assert!(cfg.optimize.enabled().is_empty());
    for text in [
        "[optimize]\nconcise_prose = true\n",
        "[optimize]\nquiet = true\n",
        "[optimize]\nprovenance = true\n",
        "model = 'x'\n",
    ] {
        let raw: toml::Table = toml::from_str(text).unwrap();
        cfg.merge(toml::Value::Table(raw.clone()).try_into().unwrap(), &raw);
    }
    assert_eq!(
        cfg.optimize.enabled(),
        ["concise_prose", "quiet", "provenance"]
    );
    let mut o = OptimizeConfig::default();
    assert_eq!(o.apply_env("concise, provenance,bogus"), ["bogus"]);
    assert_eq!(o.enabled(), ["concise_prose", "provenance"]);
    o.apply_env("none");
    assert!(o.enabled().is_empty());
    o.apply_env("all");
    assert_eq!(o.enabled().len(), 3);
    o.apply_env("none");
    assert_eq!(o.apply_env("quiet"), Vec::<String>::new());
    assert_eq!(o.enabled(), ["quiet"]);
    assert_eq!(o.apply_env("provenance"), Vec::<String>::new());
    assert_eq!(o.enabled(), ["quiet", "provenance"]);
}

#[test]
fn memory_and_cost_settings_survive_layering() {
    let mut cfg = Config::default();
    for text in [
        "max_cost_usd = 2.0\n[memory]\nauto = false\nscope = 'user'\n[pricing.custom]\ninput = 1.0\noutput = 2.0\n",
        "max_cost_usd_per_day = 5.0\n[memory]\nscope = 'project'\n",
    ] {
        let raw: toml::Table = toml::from_str(text).unwrap();
        cfg.merge(toml::Value::Table(raw.clone()).try_into().unwrap(), &raw);
    }
    assert!(!cfg.memory.auto);
    assert_eq!(cfg.memory.scope, "project");
    assert_eq!(cfg.max_cost_usd, Some(2.0));
    assert_eq!(cfg.max_cost_usd_per_day, Some(5.0));
    assert!(cfg.pricing.contains_key("custom"));
}

#[test]
fn newer_settings_merge_from_config_files() {
    let d = std::env::temp_dir().join(format!("chanto-merge-{}", uuid::Uuid::new_v4().simple()));
    let _home_lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
    std::fs::create_dir_all(d.join(".chanto")).unwrap();
    std::fs::write(
        d.join(".chanto/config.toml"),
        "output_style = \"Learning\"\napi_key_helper = \"echo k\"\nprimer = false\n[permissions]\nfast_path = true\n[secrets]\nredact = false\nscrub_env = false\nprotect_files = false\npass_env = [\"GH_TOKEN\"]\n",
    )
    .unwrap();
    let (cfg, _) = Config::load(&d, true).unwrap();
    assert_eq!(cfg.output_style.as_deref(), Some("Learning"));
    assert_eq!(cfg.api_key_helper.as_deref(), Some("echo k"));
    assert_eq!(cfg.primer, Some(false));
    assert!(cfg.permissions.fast_path);
    assert!(!cfg.secrets.redact && !cfg.secrets.scrub_env && !cfg.secrets.protect_files);
    assert_eq!(cfg.secrets.pass_env, vec!["GH_TOKEN".to_string()]);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn misspelt_keys_are_reported() {
    let d = std::env::temp_dir().join(format!("chanto-keys-{}", uuid::Uuid::new_v4().simple()));
    let _home_lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
    std::fs::create_dir_all(d.join(".chanto")).unwrap();
    std::fs::write(
        d.join(".chanto/config.toml"),
        "modle = \"x\"\nmodel = \"y\"\n[guardrails]\nenabled = true\n",
    )
    .unwrap();
    let (cfg, _) = Config::load(&d, true).unwrap();
    let notes = cfg.diagnostics.join("\n");
    assert!(
        notes.contains("Unknown setting `modle`") && notes.contains("did you mean `model`"),
        "{notes}"
    );
    assert!(
        !notes.contains("`guardrails`") && !notes.contains("Unknown setting `model`"),
        "{notes}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn provider_without_kind_and_a_v1_url_is_openai() {
    let d = std::env::temp_dir().join(format!("chanto-kind-{}", uuid::Uuid::new_v4().simple()));
    let _home_lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
    std::fs::create_dir_all(d.join(".chanto")).unwrap();
    std::fs::write(
        d.join(".chanto/config.toml"),
        "[providers.proxy]\nbase_url = \"http://localhost:4000/v1/\"\n[providers.box]\nbase_url = \"http://gpu-box:11434\"\n[providers.vllm]\nkind = \"anthropic\"\nbase_url = \"http://h/v1\"\n",
    )
    .unwrap();
    let (cfg, _) = Config::load(&d, true).unwrap();
    assert_eq!(cfg.providers["proxy"].kind, ProviderKind::Openai);
    assert_eq!(cfg.providers["box"].kind, ProviderKind::Ollama);
    assert_eq!(cfg.providers["vllm"].kind, ProviderKind::Anthropic);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_top_level_v1_url_is_openai_unless_a_provider_is_named() {
    let d = std::env::temp_dir().join(format!("chanto-top-kind-{}", uuid::Uuid::new_v4().simple()));
    let _home_lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
    std::fs::create_dir_all(d.join(".chanto")).unwrap();
    std::fs::write(
        d.join(".chanto/config.toml"),
        "base_url = \"http://localhost:4000/v1/\"\nmodel = \"bench-model\"\n",
    )
    .unwrap();
    let (cfg, _) = Config::load(&d, true).unwrap();
    assert_eq!(cfg.provider, ProviderKind::Openai);
    // An explicitly named provider keeps its native API.
    std::fs::write(
        d.join(".chanto/config.toml"),
        "provider = \"ollama\"\nbase_url = \"http://localhost:4000/v1/\"\nmodel = \"bench-model\"\n",
    )
    .unwrap();
    let (cfg, _) = Config::load(&d, true).unwrap();
    assert_eq!(cfg.provider, ProviderKind::Ollama);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn profiles_apply_on_top_and_respect_trust() {
    let d = std::env::temp_dir().join(format!("chanto-prof-{}", uuid::Uuid::new_v4().simple()));
    let _home_lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
    std::fs::create_dir_all(d.join(".chanto")).unwrap();
    std::fs::write(
        d.join(".chanto/config.toml"),
        "model = \"base\"\nprofile = \"fast\"\n[profiles.fast]\nmodel = \"small\"\nnum_ctx = 4096\n[profiles.yolo]\nmode = \"bypassPermissions\"\n",
    )
    .unwrap();
    // The project selects "fast" (trusted project).
    let (cfg, _) = Config::load(&d, true).unwrap();
    assert_eq!(cfg.model.as_deref(), Some("small"));
    assert_eq!(cfg.num_ctx, Some(4096));
    // --profile overrides; an untrusted project's profile can't set risky keys.
    unsafe { std::env::set_var("CHANTO_PROFILE", "yolo") };
    let (cfg, skipped) = Config::load(&d, false).unwrap();
    assert!(cfg.mode.is_none(), "untrusted profile set mode");
    assert!(
        skipped
            .iter()
            .any(|s| s.starts_with("mode") && s.contains("profiles.yolo")),
        "{skipped:?}"
    );
    unsafe { std::env::set_var("CHANTO_PROFILE", "nope") };
    assert!(
        Config::load(&d, true)
            .unwrap_err()
            .to_string()
            .contains("unknown profile")
    );
    unsafe { std::env::remove_var("CHANTO_PROFILE") };
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_home_directory_is_not_its_own_untrusted_project() {
    // Started in ~, ~/.chanto/config.toml is the user's config. It was also
    // read as an untrusted project's, with a warning to `--trust` all of ~.
    let d = std::env::temp_dir().join(format!("chanto-cfg-home-{}", uuid::Uuid::new_v4().simple()));
    let _home_lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
    std::fs::create_dir_all(d.join(".chanto")).unwrap();
    std::fs::write(d.join(".chanto/config.toml"), "mode = \"acceptEdits\"\n").unwrap();
    let saved = std::env::var_os("CHANTO_HOME");
    unsafe { std::env::set_var("CHANTO_HOME", d.join(".chanto")) };
    let loaded = Config::load(&d, false);
    unsafe {
        match saved {
            Some(h) => std::env::set_var("CHANTO_HOME", h),
            None => std::env::remove_var("CHANTO_HOME"),
        }
    }
    let (cfg, skipped) = loaded.unwrap();
    assert_eq!(cfg.mode, Some(Mode::AcceptEdits));
    assert!(skipped.is_empty(), "{skipped:?}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn untrusted_project_settings_are_skipped() {
    let d = std::env::temp_dir().join(format!("chanto-cfg-{}", uuid::Uuid::new_v4().simple()));
    // Don't pick up the real ~/.claude or ~/.codex.
    let _home_lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
    std::fs::create_dir_all(d.join(".chanto")).unwrap();
    std::fs::write(
        d.join(".chanto/config.toml"),
        "mode = \"bypassPermissions\"\nnum_ctx = 1234\nclassifier_model = \"tiny:1b\"\n[optimize]\ntoken_classifier = true\nconcise_prose = true\n[[hooks.PreToolUse]]\ncommand = \"touch /tmp/x\"\n[permissions]\nallow = [\"Bash\"]\ndeny = [\"Bash(rm:*)\"]\nauto_deny_after_secs = 0\n[sandbox]\nenabled = false\n",
    )
    .unwrap();
    std::fs::write(
        d.join(".mcp.json"),
        r#"{"mcpServers":{"evil":{"command":"sh"}}}"#,
    )
    .unwrap();
    let (cfg, skipped) = Config::load(&d, false).unwrap();
    assert!(cfg.hooks.is_empty());
    assert!(cfg.mode.is_none());
    assert!(!cfg.mcp_servers.contains_key("evil"));
    assert!(!cfg.permissions.allow.iter().any(|r| r == "Bash"));
    assert!(cfg.permissions.deny.iter().any(|r| r == "Bash(rm:*)"));
    assert_eq!(cfg.num_ctx, Some(1234));
    // Nor pick the model that judges auto-mode actions, or let a
    // one-token answer settle them.
    assert_eq!(cfg.classifier_model, None);
    assert_eq!(cfg.optimize.token_classifier, None);
    assert_eq!(cfg.optimize.concise_prose, Some(true));
    // A repo must not be able to make an unanswered prompt wait forever.
    assert_eq!(cfg.permissions.auto_deny_after_secs, None);
    assert!(skipped.len() >= 5);
    let (cfg, skipped) = Config::load(&d, true).unwrap();
    assert_eq!(cfg.hooks["PreToolUse"].len(), 1);
    assert!(cfg.mcp_servers.contains_key("evil"));
    assert_eq!(cfg.permissions.auto_deny_after_secs, Some(0));
    assert_eq!(cfg.classifier_model.as_deref(), Some("tiny:1b"));
    assert_eq!(cfg.optimize.token_classifier, Some(true));
    assert!(skipped.is_empty());
}

#[test]
fn launch_flags_add_settings_and_servers_and_pick_sources() {
    use crate::launch::{Launch, parse_mcp_config, parse_settings};
    let d = std::env::temp_dir().join(format!("chanto-launch-{}", uuid::Uuid::new_v4().simple()));
    let _home_lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
    std::fs::create_dir_all(d.join(".chanto")).unwrap();
    std::fs::write(d.join(".chanto/config.toml"), "num_ctx = 1000\nmodel = \"project\"\n[[hooks.PreToolUse]]\ncommand = \"true\"\n[mcp_servers.proj]\ncommand = \"p\"\n").unwrap();
    std::fs::write(d.join(".chanto/config.local.toml"), "max_turns = 7\n").unwrap();
    std::fs::write(
        d.join(".mcp.json"),
        r#"{"mcpServers":{"fromfile":{"command":"sh"}}}"#,
    )
    .unwrap();
    let settings =
        parse_settings(r#"{"model":"cli","permissions":{"deny":["Bash(rm:*)"]}}"#).unwrap();
    let mcp = parse_mcp_config(r#"{"mcpServers":{"cli":{"command":"c"}}}"#).unwrap();

    // --settings wins over project files; --mcp-config adds to the discovered servers.
    let l = Launch {
        settings: vec![settings.clone()],
        mcp: vec![mcp.clone()],
        ..Default::default()
    };
    let (cfg, _) = Config::load_with(&d, true, &l).unwrap();
    assert_eq!(cfg.model.as_deref(), Some("cli"));
    assert_eq!(cfg.num_ctx, Some(1000));
    assert!(cfg.permissions.deny.iter().any(|r| r == "Bash(rm:*)"));
    assert!(
        ["proj", "fromfile", "cli"]
            .iter()
            .all(|s| cfg.mcp_servers.contains_key(*s)),
        "{:?}",
        cfg.mcp_servers.keys()
    );

    // --strict-mcp-config: only the command line's servers.
    let l = Launch {
        mcp: vec![mcp.clone()],
        strict_mcp: true,
        ..Default::default()
    };
    let (cfg, _) = Config::load_with(&d, true, &l).unwrap();
    assert_eq!(cfg.mcp_servers.keys().collect::<Vec<_>>(), vec!["cli"]);
    assert_eq!(cfg.hooks["PreToolUse"].len(), 1);

    // --bare: no hooks or discovered servers, but other settings still load.
    let l = Launch {
        bare: true,
        ..Default::default()
    };
    let (cfg, _) = Config::load_with(&d, true, &l).unwrap();
    assert!(cfg.hooks.is_empty() && cfg.mcp_servers.is_empty());
    assert_eq!(cfg.num_ctx, Some(1000));

    // --setting-sources local: no project files.
    let l = Launch {
        sources: Some(vec!["local".into()]),
        ..Default::default()
    };
    let (cfg, _) = Config::load_with(&d, true, &l).unwrap();
    assert_eq!(cfg.num_ctx, None);
    assert_eq!(cfg.max_turns, Some(7));
    assert!(!cfg.mcp_servers.contains_key("fromfile"));
    let l = Launch {
        sources: Some(vec!["project".into()]),
        ..Default::default()
    };
    let (cfg, _) = Config::load_with(&d, true, &l).unwrap();
    assert_eq!((cfg.num_ctx, cfg.max_turns), (Some(1000), None));
    unsafe { std::env::remove_var("CHANTO_COMPAT_HOME") };
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn hook_types_and_conditions_flatten() {
    let mut cfg: Config = toml::from_str(
        r#"
[[hooks.PreToolUse]]
matcher = "Bash"
[[hooks.PreToolUse.hooks]]
type = "command"
command = "check"
if = "Bash(git *)"
[[hooks.PreToolUse.hooks]]
type = "http"
url = "http://localhost:9/hook"
allowedEnvVars = ["TOKEN"]
headers = { Authorization = "Bearer $TOKEN" }
timeout = 5
onFailure = "block"
[[hooks.PreToolUse.hooks]]
type = "prompt"
prompt = "is this ok?"
model = "small"
continueOnBlock = true
[[hooks.PreToolUse.hooks]]
type = "mcp_tool"
server = "sec"
tool = "scan"
input = { path = "${tool_input.file_path}" }
[[hooks.PreToolUse.hooks]]
type = "mcp_tool"
server = "no-tool"
[[hooks.PostToolUse]]
[[hooks.PostToolUse.hooks]]
command = "node"
args = ["x.js", "--fix"]
async = true
asyncRewake = true
shell = "powershell"
[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "agent"
prompt = "verify"
[[hooks.Stop.hooks]]
type = "wasm"
"#,
    )
    .unwrap();
    let dropped = flatten_hooks(&mut cfg.hooks);
    assert_eq!(
        dropped,
        vec![
            "PreToolUse hook of type `mcp_tool`".to_string(),
            "Stop hook of type `wasm`".to_string()
        ]
    );
    let pre = &cfg.hooks["PreToolUse"];
    assert_eq!(pre.len(), 4);
    assert_eq!(
        (pre[0].kind.as_str(), pre[0].command.as_str()),
        ("command", "check")
    );
    assert_eq!(pre[0].condition.as_deref(), Some("Bash(git *)"));
    assert_eq!(pre[1].kind, "http");
    assert_eq!(pre[1].matcher, "Bash");
    assert_eq!(pre[1].url, "http://localhost:9/hook");
    assert_eq!(pre[1].allowed_env_vars, vec!["TOKEN".to_string()]);
    assert_eq!(pre[1].headers["Authorization"], "Bearer $TOKEN");
    assert_eq!(pre[1].timeout_secs, Some(5));
    assert_eq!(pre[1].on_failure.as_deref(), Some("block"));
    assert_eq!(
        (
            pre[2].kind.as_str(),
            pre[2].prompt.as_str(),
            pre[2].model.as_deref(),
            pre[2].continue_on_block
        ),
        ("prompt", "is this ok?", Some("small"), true)
    );
    assert_eq!(
        (pre[3].server.as_str(), pre[3].tool.as_str()),
        ("sec", "scan")
    );
    assert_eq!(
        pre[3].input.as_ref().unwrap()["path"],
        "${tool_input.file_path}"
    );
    let post = &cfg.hooks["PostToolUse"][0];
    assert_eq!(
        (
            post.kind.as_str(),
            post.args.clone().unwrap(),
            post.run_async,
            post.async_rewake,
            post.shell.as_deref()
        ),
        (
            "command",
            vec!["x.js".to_string(), "--fix".to_string()],
            true,
            true,
            Some("powershell")
        )
    );
    // `/hooks` lists what each one runs: an http hook has no command.
    assert_eq!(pre[0].describe(), "check (if Bash(git *))");
    assert_eq!(pre[1].describe(), "POST http://localhost:9/hook");
    assert_eq!(pre[3].describe(), "mcp sec/scan");
    assert_eq!(post.describe(), "node x.js --fix");
    assert_eq!(cfg.hooks["Stop"].len(), 1);
    assert_eq!(cfg.hooks["Stop"][0].kind, "agent");
    // Native flat entries still work, including http ones.
    let mut native: Config = toml::from_str("[[hooks.PostToolUse]]\ncommand = \"fmt\"\n[[hooks.PostToolUse]]\ntype = \"http\"\nurl = \"http://x/h\"\n").unwrap();
    assert!(flatten_hooks(&mut native.hooks).is_empty());
    assert_eq!(native.hooks["PostToolUse"].len(), 2);
    // Claude's JSON layout, as settings.json writes it.
    let json: HooksConfig = serde_json::from_str(r#"{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"x","onFailure":"block","statusMessage":"checking"}]}]}"#).unwrap();
    assert_eq!(
        json["PreToolUse"][0].hooks[0].on_failure.as_deref(),
        Some("block")
    );
}

#[test]
fn disable_all_hooks_spares_managed_hooks() {
    let hook = |c: &str| HookConfig {
        kind: "command".into(),
        command: c.into(),
        ..Default::default()
    };
    let mut cfg = Config {
        disable_all_hooks: Some(true),
        ..Default::default()
    };
    cfg.hooks
        .insert("Stop".into(), vec![hook("user"), hook("managed")]);
    let mut managed = HooksConfig::new();
    managed.insert("Stop".into(), vec![hook("managed")]);
    cfg.apply_disable_all_hooks(&managed, false);
    assert_eq!(
        cfg.hooks["Stop"],
        vec![hook("managed")],
        "only managed settings can turn managed hooks off"
    );
    cfg.apply_disable_all_hooks(&managed, true);
    assert!(cfg.hooks.is_empty());
    // Claude Code settings carry it as `disableAllHooks`.
    let t = toml::Table::try_from(serde_json::json!({"disableAllHooks": true})).unwrap();
    let c: Config = toml::Value::Table(t).try_into().unwrap();
    assert_eq!(c.disable_all_hooks, Some(true));
}

#[test]
fn model_sizes_come_from_the_name() {
    assert_eq!(model_billions("qwen3:4b"), Some(4.0));
    assert_eq!(model_billions("google/gemma-3-4b-it"), Some(4.0));
    assert_eq!(
        model_billions("meta-llama/llama-3.1-8b-instruct"),
        Some(8.0)
    );
    assert_eq!(model_billions("openai/gpt-oss-20b"), Some(20.0));
    assert_eq!(model_billions("qwen3.5-0.8b"), Some(0.8));
    assert_eq!(model_billions("deepseek-flash"), None);
    assert_eq!(model_billions("gemma-3n-e2b"), None);
    assert_eq!(model_billions("qwen3-30b-a3b"), Some(30.0));
}
