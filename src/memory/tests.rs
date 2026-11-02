use super::*;

#[test]
fn an_empty_memory_store_adds_no_reminder() {
    let home = std::env::temp_dir().join(format!("chanto-memory-{}", uuid::Uuid::new_v4()));
    let store = Store {
        root: home.join("project"),
        home: home.clone(),
    };
    assert_eq!(store.context().unwrap(), "");
    assert!(first_turn_context(&store, &[]).unwrap().is_none());
    store
        .save("user", "style", "Style", "reference", "prefers tabs")
        .unwrap();
    let index = first_turn_context(&store, &[]).unwrap().unwrap();
    assert!(index.starts_with("Memory index"));
    assert!(index.contains("user - style.md:"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn md_suffix_is_optional() {
    let home = std::env::temp_dir().join(format!("chanto-memory-{}", uuid::Uuid::new_v4()));
    let store = Store {
        root: home.join("project"),
        home: home.clone(),
    };
    store
        .save(
            "project",
            "deploy.md",
            "Deploy detail",
            "reference",
            "staging job",
        )
        .unwrap();
    assert_eq!(store.read("project", "deploy").unwrap().body, "staging job");
    assert_eq!(
        store.read("project", "deploy.md").unwrap().body,
        "staging job"
    );
    assert!(
        store
            .regenerate("project")
            .unwrap()
            .contains("- deploy.md:")
    );
    store.delete("project", "deploy.md").unwrap();
    assert!(store.read("project", "deploy").is_err());
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn lifecycle_scopes_secrets_and_budget() {
    let home = std::env::temp_dir().join(format!("chanto-memory-{}", uuid::Uuid::new_v4()));
    let store = Store {
        root: home.join("project"),
        home: home.clone(),
    };
    store
        .save(
            "user",
            "style",
            "Writing style",
            "user",
            "Keep prose concise",
        )
        .unwrap();
    store
        .save(
            "project",
            "deploy",
            "Deployment detail",
            "reference",
            "Deploys require the staging job",
        )
        .unwrap();
    assert!(store.dir("project").unwrap().starts_with(&home));
    let other = Store {
        root: home.join("other"),
        home: home.clone(),
    };
    assert!(other.read("project", "deploy").is_err());
    assert!(other.read("user", "style").is_ok());
    assert!(first_turn_context(&store, &[]).unwrap().is_some());
    assert!(
        first_turn_context(&store, &[crate::llm::Message::user("task")])
            .unwrap()
            .is_none()
    );
    assert!(store.read("project", "style").is_err());
    assert!(store.read("user", "deploy").is_err());
    let created = store.read("user", "style").unwrap().created;
    store
        .save(
            "user",
            "style",
            "Writing style",
            "user",
            "Use short paragraphs",
        )
        .unwrap();
    assert_eq!(store.read("user", "style").unwrap().created, created);
    store.undo("user").unwrap();
    assert_eq!(
        store.read("user", "style").unwrap().body,
        "Keep prose concise"
    );
    store.delete("user", "style").unwrap();
    assert!(!store.regenerate("user").unwrap().contains("style"));
    store.undo("user").unwrap();
    assert!(store.regenerate("user").unwrap().contains("style.md"));
    assert!(
        store
            .save(
                "project",
                "bad",
                "Credential",
                "project",
                "AKIAIOSFODNN7EXAMPLE"
            )
            .is_err()
    );
    assert!(
        store
            .save("project", "../escape", "Bad", "project", "text")
            .is_err()
    );
    store
        .save("project", "huge", &"界".repeat(5000), "project", "body")
        .unwrap();
    let context = store.context().unwrap();
    assert!(context.len() <= INDEX_BUDGET);
    assert!(
        !context.contains("Deploys require"),
        "index must not include bodies"
    );
    std::fs::remove_file(store.dir("project").unwrap().join("MEMORY.md")).unwrap();
    assert!(store.regenerate("project").unwrap().contains("deploy.md"));
    std::fs::remove_dir_all(home).unwrap();
}
