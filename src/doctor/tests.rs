use super::*;
#[test]
fn named_provider_settings_come_from_its_entry() {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "local".into(),
        toml::from_str("kind = \"openai\"\nbase_url = \"http://localhost:4000/v1\"").unwrap(),
    );
    cfg.origins
        .insert("providers".into(), "/home/u/.chanto/config.toml".into());
    cfg.origins
        .insert("model".into(), "/p/.chanto/config.toml".into());
    assert_eq!(
        setting_source(&cfg, "local/m", "base_url"),
        "/home/u/.chanto/config.toml"
    );
    assert_eq!(
        setting_source(&cfg, "local/m", "provider"),
        "/home/u/.chanto/config.toml"
    );
    assert_eq!(
        setting_source(&cfg, "ollama/qwen3:8b", "provider"),
        "/p/.chanto/config.toml"
    );
    assert_eq!(
        setting_source(&cfg, "qwen3:8b", "provider"),
        "built-in default"
    );
    assert_eq!(setting_source(&cfg, "local/m", "mode"), "built-in default");
}

#[test]
fn readable_report_and_home_paths() {
    assert_eq!(
        shorten("/tmp/home/.chanto/config.toml", "/tmp/home"),
        "~/.chanto/config.toml"
    );
    assert_eq!(
        shorten("/tmp/home-other/file", "/tmp/home"),
        "/tmp/home-other/file"
    );
    let r = json!({"settings": {"permissions": {"allow_count": 169, "deny_count": 0, "source": "settings.json"}, "model": {"value": "qwen3:8b", "source": "config.toml"}}});
    let text = render(&r);
    for section in [
        "Config layers",
        "Effective settings",
        "Model",
        "Sandbox",
        "MCP",
        "Local servers",
        "Tools",
        "Warnings",
    ] {
        assert!(text.contains(section));
    }
    assert!(text.contains("169 allow, 0 deny"));
    assert!(text.contains("← config.toml"));
    assert!(!text.contains("{\"") && !text.contains('\u{1b}'));
}
