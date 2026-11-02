use super::*;

#[test]
fn sessions_cli_searches_archives_and_deletes() {
    use crate::session::Session;
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("chanto-sessions-cli-{}", uuid::Uuid::new_v4()));
    let previous = std::env::var_os("CHANTO_HOME");
    // SAFETY: test-only, under TEST_HOME_LOCK.
    unsafe { std::env::set_var("CHANTO_HOME", &home) };
    let cwd = home.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("keep.txt"), "project file").unwrap();
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mut auth = Session::new(&cwd, "m");
    auth.title = "Auth refactor".into();
    auth.save(vec![
        Message::system("s"),
        Message::user("move login to OAuth tokens"),
    ])
    .unwrap();
    let mut docs = Session::new(&cwd, "m");
    docs.save(vec![
        Message::system("s"),
        Message::user("write the README"),
    ])
    .unwrap();
    let out = sessions_cli(&cwd, &[]).unwrap();
    assert!(out.contains(&auth.id) && out.contains(&docs.id), "{out}");
    // Search covers the title and the first prompt, all words, any case.
    let out = sessions_cli(&cwd, &args(&["--search", "oauth LOGIN"])).unwrap();
    assert!(out.contains(&auth.id) && !out.contains(&docs.id), "{out}");
    assert!(
        sessions_cli(&cwd, &args(&["--search", "auth"]))
            .unwrap()
            .contains(&auth.id)
    );
    assert!(
        sessions_cli(&cwd, &args(&["--search", "nothing-like-it"]))
            .unwrap()
            .starts_with("No saved sessions here match")
    );
    // Archived: hidden by default and from -c, listed with --all.
    sessions_cli(&cwd, &args(&["archive", &auth.id])).unwrap();
    let out = sessions_cli(&cwd, &[]).unwrap();
    assert!(!out.contains(&auth.id), "{out}");
    let out = sessions_cli(&cwd, &args(&["--all"])).unwrap();
    assert!(
        out.contains(&auth.id) && out.contains("[archived]"),
        "{out}"
    );
    assert_eq!(Session::latest(&cwd).0.map(|s| s.id), Some(docs.id.clone()));
    assert!(Session::load(&cwd, &auth.id).unwrap().0.archived);
    sessions_cli(&cwd, &args(&["unarchive", &auth.id])).unwrap();
    assert!(sessions_cli(&cwd, &[]).unwrap().contains(&auth.id));
    // Delete: the transcript goes, project files stay; an ambiguous prefix is refused.
    assert!(sessions_cli(&cwd, &args(&["delete", &auth.id[..4]])).is_err());
    let out = sessions_cli(&cwd, &args(&["delete", &auth.id])).unwrap();
    assert!(out.starts_with("Deleted session"), "{out}");
    assert!(!auth.path().exists() && docs.path().exists());
    assert_eq!(
        std::fs::read_to_string(cwd.join("keep.txt")).unwrap(),
        "project file"
    );
    assert!(sessions_cli(&cwd, &args(&["delete", &auth.id])).is_err());
    assert!(sessions_cli(&cwd, &args(&["--bogus"])).is_err());
    unsafe {
        if let Some(previous) = previous {
            std::env::set_var("CHANTO_HOME", previous);
        } else {
            std::env::remove_var("CHANTO_HOME");
        }
    }
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn autocompact_sizes() {
    assert_eq!(parse_tokens("500k"), Some(500_000));
    assert_eq!(parse_tokens("1.5M"), Some(1_500_000));
    assert_eq!(parse_tokens("120_000"), Some(120_000));
    assert_eq!(parse_tokens("lots"), None);
    assert_eq!(parse_tokens("-5k"), None);
}

#[test]
fn user_turns_skip_reminders() {
    let m = vec![
        Message::user("fix it"),
        Message::user("<system-reminder>x</system-reminder>"),
        Message::assistant("ok"),
        Message::user("thanks"),
    ];
    assert_eq!(user_turns(&m), 2);
}
