use super::*;

fn map(json: &str) -> (Keymap, Vec<String>) {
    let v: Value = serde_json::from_str(json).unwrap();
    let mut problems = Vec::new();
    let km = Keymap {
        bindings: parse(&v, "test", &mut problems),
        ..Default::default()
    };
    (km, problems)
}

fn k(s: &str) -> Key {
    parse_key(s).unwrap()
}

#[test]
fn parses_claude_codes_key_syntax() {
    assert_eq!(
        k("ctrl+K"),
        Key::new(KeyCode::Char('k'), KeyModifiers::CONTROL)
    );
    assert_eq!(
        k("shift+tab"),
        Key::of(&KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
    );
    assert_eq!(k("meta+p"), Key::new(KeyCode::Char('p'), KeyModifiers::ALT));
    assert_eq!(k("K"), k("k"), "case-insensitive, as in Claude Code");
    assert_eq!(
        k("shift+k"),
        Key::of(&KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT))
    );
    assert_eq!(k("escape"), k("esc"));
    assert!(parse_key("ctl+k").is_none());
}

#[test]
fn remaps_unbinds_and_chords() {
    let (mut km, problems) = map(r#"{"bindings": [
            {"context": "Chat", "bindings": {"ctrl+e": "chat:externalEditor", "ctrl+s": null, "ctrl+k ctrl+t": "app:toggleTodos", "ctrl+q": "chat:fastMode"}},
            {"context": "Confirmation", "bindings": {"y": "confirm:yes"}},
            {"context": "DiffPanel", "bindings": {"ctrl+x b": "app:cycleDiffBase"}}
        ]}"#);
    assert!(problems.is_empty(), "{problems:?}");
    let chat = [Context::Chat, Context::Global];
    assert_eq!(
        km.lookup(&chat, k("ctrl+e")),
        Lookup::Run(keys(&["ctrl+g"]))
    );
    assert_eq!(km.lookup(&chat, k("ctrl+s")), Lookup::Swallow);
    assert_eq!(km.lookup(&chat, k("ctrl+k")), Lookup::Pending);
    assert_eq!(
        km.lookup(&chat, k("ctrl+t")),
        Lookup::Run(keys(&["ctrl+t"]))
    );
    assert_eq!(
        km.lookup(&chat, k("ctrl+q")),
        Lookup::Run(Effect::Nothing),
        "a Claude Code action Chanto lacks"
    );
    assert_eq!(
        km.lookup(&chat, k("y")),
        Lookup::Default,
        "Confirmation bindings only in dialogs"
    );
    assert_eq!(
        km.lookup(&[Context::Confirmation, Context::Global], k("y")),
        Lookup::Run(keys(&["enter"]))
    );
    assert_eq!(km.lookup(&chat, k("ctrl+a")), Lookup::Default);
}

#[test]
fn reports_typos_and_reserved_keys() {
    let (_, problems) = map(
        r#"{"bindings": [{"context": "Chat", "bindings": {"ctrl+c": "chat:submit", "ctl+k": "chat:submit", "ctrl+u": "chat:sumbit"}}]}"#,
    );
    assert_eq!(problems.len(), 3, "{problems:?}");
    assert!(
        problems.iter().any(|p| p.contains("reserved"))
            && problems.iter().any(|p| p.contains("chat:sumbit"))
    );
}

#[test]
fn chantos_own_file_wins_and_edits_reload() {
    let dir = std::env::temp_dir().join(format!("chanto-keys-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let (claude, ours) = (dir.join("claude.json"), dir.join("chanto.json"));
    std::fs::write(
        &claude,
        r#"{"bindings": [{"context": "Chat", "bindings": {"ctrl+e": "chat:externalEditor"}}]}"#,
    )
    .unwrap();
    std::fs::write(
        &ours,
        r#"{"bindings": [{"context": "Chat", "bindings": {"ctrl+e": null}}]}"#,
    )
    .unwrap();
    let (mut km, _) = Keymap::load(vec![claude, ours.clone()]);
    assert_eq!(km.lookup(&[Context::Chat], k("ctrl+e")), Lookup::Swallow);
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(
        &ours,
        r#"{"bindings": [{"context": "Chat", "bindings": {"ctrl+e": "chat:newline"}}]}"#,
    )
    .unwrap();
    km.checked = None;
    assert!(km.refresh().is_some(), "the edit is picked up");
    assert_eq!(
        km.lookup(&[Context::Chat], k("ctrl+e")),
        Lookup::Run(keys(&["ctrl+j"]))
    );
    let _ = std::fs::remove_dir_all(dir);
}
