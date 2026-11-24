use super::*;

#[test]
fn loads_builtin_and_custom_styles() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let d = std::env::temp_dir().join(format!("chanto-styles-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(d.join(".chanto/output-styles")).unwrap();
    std::fs::write(
        d.join(".chanto/output-styles/terse.md"),
        "---\nname: Terse\ndescription: One-line answers\n---\nAnswer in one line.\n",
    )
    .unwrap();
    let names = |styles: &[OutputStyle]| styles.iter().map(|s| s.name.clone()).collect::<Vec<_>>();

    // Untrusted folder: the built-ins load, but the project style is skipped
    // — a project style can shadow a built-in and its body reaches the
    // system prompt, so it needs folder trust (like skills/commands/agents).
    let untrusted = names(&load_output_styles(&d));
    for b in ["Explanatory", "Learning", "Concise", "Proactive"] {
        assert!(
            untrusted.contains(&b.to_string()),
            "built-in {b} missing: {untrusted:?}"
        );
    }
    assert!(
        !untrusted.contains(&"Terse".to_string()),
        "untrusted project style must not load: {untrusted:?}"
    );

    // Trusting the folder lets its style load (into the isolated test home).
    crate::config::trust(&d).unwrap();
    let styles = load_output_styles(&d);
    let t = styles
        .iter()
        .find(|s| s.name == "Terse")
        .expect("trusted project style loads");
    assert_eq!(t.body, "Answer in one line.");
    assert_eq!(t.description, "One-line answers");
    let _ = std::fs::remove_dir_all(&d);
}
