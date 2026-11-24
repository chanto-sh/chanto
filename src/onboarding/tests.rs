use super::*;
#[test]
fn free_models_are_filtered_for_agents() {
    let list = serde_json::json!({"data": [
        {"id": "a/big:free", "context_length": 1_000_000, "supported_parameters": ["tools"]},
        {"id": "b/coder:free", "context_length": 128_000, "supported_parameters": ["tools", "temperature"], "expiration_date": "2026-11-01T00:00:00Z"},
        {"id": "c/no-tools:free", "context_length": 200_000, "supported_parameters": ["temperature"]},
        {"id": "d/tiny-ctx:free", "context_length": 8_192, "supported_parameters": ["tools"]},
        {"id": "e/content-safety:free", "context_length": 128_000, "supported_parameters": ["tools"]},
        {"id": "f/paid", "context_length": 200_000, "supported_parameters": ["tools"]},
    ]});
    let got = free_agent_models(&list);
    assert_eq!(
        got.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["b/coder:free", "a/big:free"]
    );
    assert_eq!(
        got[0].label(),
        "b/coder:free — 128k context, free until 2026-11-01"
    );
    assert!(
        gemini_chat_model("gemini-3-flash")
            && !gemini_chat_model("gemini-embedding-001")
            && !gemini_chat_model("gemini-2.5-flash-preview-tts")
    );
}

#[test]
fn picker_is_centered_and_fits_content() {
    let area = picker_area(
        ratatui::layout::Rect::new(0, 0, 160, 50),
        &["model".into(), "Sign in\nChatGPT".into()],
    );
    assert_eq!(area.width, 78);
    assert_eq!(area.height, 5);
    assert_eq!(area.x, 41);
    assert_eq!(area.y, 22);
    // A label wider than the screen wraps instead of being cut off.
    let long = "x ".repeat(100);
    assert_eq!(
        picker_area(ratatui::layout::Rect::new(0, 0, 100, 50), &[long]).height,
        5
    );
    assert_eq!(split_label("Free\nGemini"), (vec!["Free"], "Gemini"));
    assert_eq!(split_label("model"), (vec![], "model"));
}

#[test]
fn saving_default_preserves_keys_comments_and_tables() {
    let dir = std::env::temp_dir().join(format!("chanto-save-{}", uuid::Uuid::new_v4()));
    let path = dir.join("config.toml");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&path, "# my config\nmodel = \"old\" # keep this comment\nnum_ctx = 65536\n[compat]\nclaude = false\n").unwrap();
    save_to(&path, "groq/openai/gpt-oss-120b", None, None).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let table: toml::Table = toml::from_str(&text).unwrap();
    assert_eq!(table["model"].as_str(), Some("groq/openai/gpt-oss-120b"));
    assert_eq!(table["num_ctx"].as_integer(), Some(65536));
    assert_eq!(table["compat"]["claude"].as_bool(), Some(false));
    assert!(text.contains("# my config") && text.contains("# keep this comment"));
    save_to(
        &path,
        "local",
        Some("openai"),
        Some("http://localhost:1234/v1"),
    )
    .unwrap();
    save_to(&path, "openai/gpt-5", Some("ollama"), None).unwrap();
    let table: toml::Table = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(!table.contains_key("base_url"));
    std::fs::remove_dir_all(dir).unwrap();
}
