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

#[test]
fn local_prefixes_resolve_to_their_default_servers() {
    let cfg = Config::default();
    let (p, m, name) = resolve(&cfg, "lmstudio/qwen");
    assert_eq!((m.as_str(), name.as_str()), ("qwen", "lmstudio"));
    assert!(p.base_url.contains("1234"), "{}", p.base_url);
    let (p, _, name) = resolve(&cfg, "llamacpp/qwen");
    assert_eq!(name, "llamacpp");
    assert!(p.base_url.contains("8080"), "{}", p.base_url);
    let (p, _, name) = resolve(&cfg, "ollama/qwen3:8b");
    assert_eq!(name, "ollama");
    assert_eq!(p.kind, crate::llm::ProviderKind::Ollama);
    // A bare model uses the top-level provider.
    let (_, m, name) = resolve(&cfg, "qwen3:8b");
    assert_eq!((m.as_str(), name.as_str()), ("qwen3:8b", "ollama"));
    for (kind, want) in [
        (crate::llm::ProviderKind::Anthropic, "anthropic"),
        (crate::llm::ProviderKind::Openai, "openai"),
        (crate::llm::ProviderKind::OpenaiResponses, "openai"),
    ] {
        let cfg = Config {
            provider: kind,
            ..Config::default()
        };
        assert_eq!(resolve(&cfg, "m").2, want);
    }
    // An unknown prefix is part of the model name.
    let (_, m, _) = resolve(&cfg, "nosuch/model");
    assert_eq!(m, "nosuch/model");
}

#[test]
fn credentials_name_their_source() {
    let mut cfg = Config::default();
    assert_eq!(credential(&cfg, "ollama/qwen"), None);
    assert_eq!(credential(&cfg, "lmstudio/qwen"), None);
    cfg.providers.insert(
        "keyed".into(),
        toml::from_str("kind = \"openai\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"k\"")
            .unwrap(),
    );
    assert_eq!(
        credential(&cfg, "keyed/m").as_deref(),
        Some("configured provider key")
    );
    cfg.origins
        .insert("providers".into(), "/x/config.toml".into());
    assert_eq!(
        credential(&cfg, "keyed/m").as_deref(),
        Some("/x/config.toml")
    );

    let mut cfg = Config {
        provider: crate::llm::ProviderKind::Openai,
        base_url: Some("http://127.0.0.1:9/v1".into()),
        api_key: Some("k".into()),
        ..Config::default()
    };
    assert_eq!(credential(&cfg, "m").as_deref(), Some("configured key"));
    cfg.origins
        .insert("api_key".into(), "/y/config.toml".into());
    assert_eq!(credential(&cfg, "m").as_deref(), Some("/y/config.toml"));
    cfg.api_key = None;
    cfg.api_key_helper = Some("echo k".into());
    assert_eq!(credential(&cfg, "m").as_deref(), Some("helper"));
}

#[test]
fn endpoints_lose_credentials_and_queries() {
    assert_eq!(
        endpoint("https://user:pw@api.example.com/v1?key=x#frag"),
        "https://api.example.com/v1"
    );
    assert_eq!(endpoint("not a url"), "invalid URL");
}

#[test]
fn values_read_as_plain_words() {
    assert_eq!(human(&Value::Null), "none");
    assert_eq!(human(&json!(true)), "yes");
    assert_eq!(human(&json!(false)), "no");
    assert_eq!(human(&json!([])), "none");
    assert_eq!(human(&json!(["a", 1, null])), "a, 1, none");
    assert_eq!(human(&json!(3)), "3");
    assert_eq!(shorten("/h", "/h"), "~");
}

#[test]
fn mcp_servers_list_where_they_are_configured() {
    let mut cfg = Config::default();
    assert_eq!(mcp_lines(&json!(mcp_sources(&cfg)), "/h"), "none");
    for name in ["a", "b", "c"] {
        cfg.mcp_servers
            .insert(name.into(), toml::from_str("command = \"x\"").unwrap());
    }
    cfg.origins
        .insert("mcp_servers.a".into(), "/h/.mcp.json".into());
    cfg.origins
        .insert("mcp_servers".into(), "/p/config.toml".into());
    let sources = mcp_sources(&cfg);
    assert_eq!(sources["a"], "/h/.mcp.json");
    assert_eq!(sources["b"], "/p/config.toml");
    assert_eq!(
        mcp_lines(&json!(sources), "/h"),
        "a  ← ~/.mcp.json\n  b  ← /p/config.toml\n  c  ← /p/config.toml"
    );
    cfg.origins.clear();
    assert_eq!(mcp_sources(&cfg)["c"], "unknown");
}

#[test]
fn render_shows_every_part_of_a_full_report() {
    let r = json!({
        "version": "1.2.3",
        "binary": "/bin/chanto",
        "config_home": "/h/.chanto",
        "layers": ["/h/.chanto/config.toml"],
        "settings": {
            "mcp_servers": {"configured": ["fs"], "source": "a"},
            "sandbox": {"enabled": true, "source": "b", "network_description": "off"},
        },
        "resolved_model": {"model": "m", "reachable": false},
        "tools": {"rg": true},
        "cloud": {"account": "someone"},
        "sandbox": "macOS seatbelt",
        "folder_trusted": true,
        "error_reports": "off (default)",
        "mcp_sources": {"fs": "/srv/mcp.json"},
        "local_servers": [{"provider": "ollama", "model": "qwen", "source": "http://localhost:11434"}],
        "warnings": ["careful"],
    });
    let text = render(&r);
    for want in [
        "chanto 1.2.3",
        // Paths under the home folder show as `~/…`, and where home is
        // differs by platform (the Windows runner shortens these too).
        "bin/chanto",
        "h/.chanto/config.toml",
        "mcp_servers  fs",
        "sandbox      yes",
        "reachable          no",
        "chanto cloud",
        "account            someone",
        "Network: off",
        "Folder trusted: yes",
        "fs  ← ",
        "srv/mcp.json",
        "ollama       qwen  ← http://localhost:11434",
        "  careful",
    ] {
        assert!(text.contains(want), "missing {want:?} in\n{text}");
    }
    assert!(!text.contains("none yet") && !text.contains("none found"));
    let empty = render(&json!({}));
    assert!(empty.contains("none yet") && empty.contains("none found"));
}

#[tokio::test]
async fn cli_rejects_unknown_arguments() {
    let e = cli(&["--yaml".into()]).await.unwrap_err();
    assert_eq!(e.to_string(), "Usage: chanto doctor [--json]");
}

/// A loopback model server: Ollama's `/api/show` reports tool support and
/// the OpenAI `/models` listing refuses the key.
async fn mock_server() -> String {
    use axum::routing::{get, post};
    let app = axum::Router::new()
        .route(
            "/api/show",
            post(|| async { axum::Json(json!({"capabilities": ["completion", "tools"]})) }),
        )
        .route(
            "/v1/models",
            get(|| async {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    axum::Json(json!({"error": {"message": "bad key"}})),
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn report_checks_an_ollama_model_and_flags_small_context() {
    let base = mock_server().await;
    let cfg = Config {
        base_url: Some(base.clone()),
        num_ctx: Some(4096),
        model: Some("qwen3:8b".into()),
        ..Config::default()
    };
    let r = report(&cfg, None).await;
    assert_eq!(r["resolved_model"]["reachable"], true);
    assert_eq!(r["resolved_model"]["native_tools"], true);
    assert_eq!(r["settings"]["model"]["value"], "qwen3:8b");
    assert_eq!(r["settings"]["provider"]["value"], "ollama");
    let warnings = r["warnings"].to_string();
    assert!(warnings.contains("Ollama context <32k"), "{warnings}");
    // A session selection is named as the model's source.
    let r = report(&cfg, Some("other:1b")).await;
    assert_eq!(r["settings"]["model"]["source"], "session selection");
    assert!(render(&r).contains("Effective settings"));
}

#[tokio::test]
async fn report_explains_a_refused_key_and_a_missing_model() {
    let base = mock_server().await;
    let cfg = Config {
        provider: crate::llm::ProviderKind::Openai,
        base_url: Some(format!("{base}/v1")),
        api_key: Some("wrong".into()),
        ..Config::default()
    };
    let r = report(&cfg, Some("gpt-x")).await;
    assert_eq!(r["resolved_model"]["reachable"], false);
    assert_eq!(r["resolved_model"]["credential_source"], "configured key");
    let warnings = r["warnings"].to_string();
    assert!(!warnings.contains("Provider unreachable"), "{warnings}");

    // Nothing listening and no model chosen.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let cfg = Config {
        base_url: Some(format!("http://{closed}")),
        ..Config::default()
    };
    let r = report(&cfg, None).await;
    let warnings = r["warnings"].to_string();
    assert!(warnings.contains("No model is set up"), "{warnings}");
    assert!(warnings.contains("Provider unreachable"), "{warnings}");
}

#[test]
fn home_is_shortened_only_when_known() {
    assert_eq!(shorten("/bin/chanto", ""), "/bin/chanto");
    assert_eq!(shorten("/h/.chanto", "/h"), "~/.chanto");
    assert_eq!(shorten("/hx/.chanto", "/h"), "/hx/.chanto");
    assert_eq!(
        shorten(r"C:\Users\me\.chanto\config.toml", r"C:\Users\me"),
        r"~/.chanto\config.toml"
    );
}
