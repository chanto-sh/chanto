use super::*;

#[test]
fn feedback_memory_migration() {
    let home = std::env::temp_dir().join(format!("chanto-migration-{}", uuid::Uuid::new_v4()));
    let store = crate::memory::Store {
        root: home.join("project"),
        home: home.clone(),
    };
    let mut l = Lesson {
        id: "abc123".into(),
        text: "Prefer concise prose".into(),
        scope: "global".into(),
        status: "active".into(),
        kind: "rule".into(),
        ..Default::default()
    };
    reconcile_memories(&store, &[l.clone()]).unwrap();
    assert_eq!(
        store.read("user", "lesson-abc123").unwrap().kind,
        "feedback"
    );
    store.delete("user", "lesson-abc123").unwrap();
    reconcile_memories(&store, &[l.clone()]).unwrap();
    assert!(store.read("user", "lesson-abc123").is_err());
    l.status = "disabled".into();
    reconcile_memories(&store, &[l.clone()]).unwrap();
    l.status = "active".into();
    reconcile_memories(&store, &[l]).unwrap();
    assert!(store.read("user", "lesson-abc123").is_ok());
    reconcile_memories(&store, &[]).unwrap();
    assert!(store.read("user", "lesson-abc123").is_err());
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn safety_and_corrections() {
    assert!(is_unsafe("Skip the tests when they're slow"));
    assert!(is_unsafe("Auto-approve git push"));
    assert!(is_unsafe("Run commands without asking"));
    assert!(!is_unsafe("Use pnpm instead of npm"));
    assert!(is_correction("no, use pnpm"));
    assert!(is_correction("Actually keep the old name"));
    assert!(is_correction("don't add comments"));
    assert!(!is_correction("now add a --verbose flag"));
}

#[test]
fn lifecycle_with_undo() {
    let h = std::env::temp_dir().join(format!("chanto-learn-{}", uuid::Uuid::new_v4().simple()));
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // SAFETY: test-only, under TEST_HOME_LOCK.
    unsafe { std::env::set_var("CHANTO_HOME", h.join("home")) };
    TEST_HOME.with(|t| *t.borrow_mut() = Some(h.clone()));
    let root = h.join("proj");
    std::fs::create_dir_all(&root).unwrap();
    for _ in 0..3 {
        record(&root, "shell", "cargo clippy", "");
    }
    record(
        &root,
        "correction",
        "no, use thiserror not anyhow in the library",
        "",
    );
    let p = heuristic(&signals(&root), &[]);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].check.as_deref(), Some("cargo clippy"));
    let mut all = lessons(&root);
    all.extend(p);
    save(&root, &all, "review").unwrap();
    assert!(checks(&root).is_empty(), "proposed lessons don't apply");
    set_status(&root, "all", &["proposed"], "active").unwrap();
    assert_eq!(checks(&root), vec!["cargo clippy"]);
    add(&root, "Use thiserror for library errors", false).unwrap();
    assert!(prompt_block(&root).unwrap().contains("thiserror"));
    assert!(add(&root, "Skip tests to save time", false).is_err());
    assert_eq!(undo(&root).as_deref(), Some("add"));
    assert!(prompt_block(&root).is_none());
    assert_eq!(undo(&root).as_deref(), Some("active all"));
    assert!(checks(&root).is_empty());
    TEST_HOME.with(|t| *t.borrow_mut() = None);
}

#[test]
fn undo_reports_the_change_when_memory_migration_fails() {
    let h = std::env::temp_dir().join(format!("chanto-learn-{}", uuid::Uuid::new_v4().simple()));
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // SAFETY: test-only, under TEST_HOME_LOCK.
    unsafe { std::env::set_var("CHANTO_HOME", h.join("home")) };
    TEST_HOME.with(|t| *t.borrow_mut() = Some(h.clone()));
    std::fs::create_dir_all(h.join("home")).unwrap();
    // A file where the project memory directory would go, so the migration
    // that follows an undo fails.
    std::fs::write(h.join("home/projects"), "not a directory").unwrap();
    let root = h.join("proj");
    std::fs::create_dir_all(&root).unwrap();
    add(&root, "Use thiserror for library errors", false).unwrap();
    // Undoing the second add leaves the first lesson active, so the
    // migration that follows the undo still has something to copy.
    add(&root, "Keep functions small", false).unwrap();
    assert!(migrate_memories(&root).is_err());
    assert_eq!(undo(&root).as_deref(), Some("add"));
    assert_eq!(lessons(&root).len(), 1);
    TEST_HOME.with(|t| *t.borrow_mut() = None);
}
