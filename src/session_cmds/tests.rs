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

/// Answers every question with `0` (or declines with `None`).
struct Answer(Option<&'static str>);

#[async_trait::async_trait]
impl crate::agent::Frontend for Answer {
    fn emit(&self, _: crate::agent::Event) {}
    async fn permission(
        &self,
        _: crate::agent::PermissionRequest,
    ) -> crate::agent::PermissionReply {
        crate::agent::PermissionReply::Deny(None)
    }
    async fn question(&self, _: &str, _: &[String]) -> Option<String> {
        self.0.map(str::to_string)
    }
}

fn test_env(
    dir: &std::path::Path,
    interactive: bool,
    answer: Option<&'static str>,
) -> std::sync::Arc<Env> {
    use crate::llm::{Provider, ProviderKind};
    Env::new(crate::agent::EnvInit {
        config: Default::default(),
        provider: Provider::new(ProviderKind::Openai, "http://127.0.0.1:9", None, 32768),
        model: "mock".into(),
        mode: Mode::Default,
        vision: false,
        cwd: dir.to_path_buf(),
        tools: crate::tools::Registry::builtin(),
        mcp: vec![],
        frontend: std::sync::Arc::new(Answer(answer)),
        interactive,
    })
}

fn shown(o: Outcome) -> String {
    match o {
        Outcome::Show(s) => s,
        _ => panic!("expected shown text"),
    }
}

/// An isolated `CHANTO_HOME` for the test's lifetime (hold TEST_HOME_LOCK).
struct Home {
    dir: std::path::PathBuf,
    previous: Option<std::ffi::OsString>,
}
impl Home {
    fn new(name: &str) -> Home {
        let dir = std::env::temp_dir().join(format!(
            "chanto-session-cmds-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = crate::util::canonical(&dir).unwrap();
        let previous = std::env::var_os("CHANTO_HOME");
        // SAFETY: test-only, under TEST_HOME_LOCK.
        unsafe { std::env::set_var("CHANTO_HOME", &dir) };
        Home { dir, previous }
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        // SAFETY: as in `new`.
        unsafe {
            match self.previous.take() {
                Some(v) => std::env::set_var("CHANTO_HOME", v),
                None => std::env::remove_var("CHANTO_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn plan_enters_plan_mode_and_starts_a_task() {
    let home = std::env::temp_dir();
    let agent = Agent::new(test_env(&home, false, None));
    assert!(shown(plan(&agent, "  ")).starts_with("Plan mode"));
    assert_eq!(agent.env.mode(), Mode::Plan);
    match plan(&agent, " add a cache ") {
        Outcome::Prompt(p) => assert_eq!(p, "add a cache"),
        _ => panic!("expected a prompt"),
    }
}

#[test]
fn autocompact_reports_sets_and_resets() {
    let env = test_env(&std::env::temp_dir(), false, None);
    let now = shown(autocompact(&env, ""));
    assert!(now.contains("from `compact_threshold`"), "{now}");
    assert!(shown(autocompact(&env, "120k")).contains("120000 tokens"));
    assert!(shown(autocompact(&env, "")).contains("at 120000 tokens for this session"));
    let capped = shown(autocompact(&env, "10m"));
    assert!(capped.contains("above 95%"), "{capped}");
    assert!(shown(autocompact(&env, "500")).contains("too small"));
    assert!(shown(autocompact(&env, "lots")).starts_with("Usage"));
    assert!(shown(autocompact(&env, "off")).contains("off for this session"));
    assert_eq!(env.autocompact.load(Ordering::Relaxed), u64::MAX);
    assert!(shown(autocompact(&env, "")).contains("off for this session"));
    assert!(shown(autocompact(&env, "auto")).starts_with("Auto-compaction back to"));
    assert_eq!(env.autocompact.load(Ordering::Relaxed), 0);
}

#[test]
fn away_summary_follows_env_then_config() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let env = test_env(&std::env::temp_dir(), false, None);
    let saved: Vec<_> = ["CHANTO_AWAY_SUMMARY", "CLAUDE_CODE_ENABLE_AWAY_SUMMARY"]
        .map(|v| (v, std::env::var_os(v)))
        .into();
    // SAFETY: test-only, under TEST_HOME_LOCK.
    unsafe {
        std::env::remove_var("CHANTO_AWAY_SUMMARY");
        std::env::remove_var("CLAUDE_CODE_ENABLE_AWAY_SUMMARY");
    }
    let default = away_summary_enabled(&env);
    unsafe { std::env::set_var("CLAUDE_CODE_ENABLE_AWAY_SUMMARY", "0") };
    let off = away_summary_enabled(&env);
    unsafe { std::env::set_var("CHANTO_AWAY_SUMMARY", "true") };
    let on = away_summary_enabled(&env);
    for (k, v) in saved {
        unsafe {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
    assert!(default && !off && on);
}

#[tokio::test]
async fn recap_needs_a_conversation() {
    let env = test_env(&std::env::temp_dir(), false, None);
    let err = recap(&env, &[Message::system("s")]).await.unwrap_err();
    assert_eq!(err.to_string(), "Nothing to recap yet.");
}

#[tokio::test]
async fn cd_moves_the_session_and_its_file() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = Home::new("cd");
    let (a, b) = (home.dir.join("a"), home.dir.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("file.txt"), "").unwrap();
    std::fs::write(b.join("AGENTS.md"), "be nice").unwrap();
    let mut agent = Agent::new(test_env(&a, false, None));
    assert!(shown(cd(&mut agent, " ").await).starts_with("Working directory"));
    assert!(shown(cd(&mut agent, "missing").await).starts_with("No such directory"));
    assert!(shown(cd(&mut agent, "file.txt").await).starts_with("Not a directory"));
    assert!(shown(cd(&mut agent, ".").await).starts_with("Already in"));

    agent.session = Some(crate::session::Session::new(&a, "mock"));
    agent.messages.push(Message::user("hello"));
    agent.save_session();
    let before = agent.session.as_ref().unwrap().path();
    let out = shown(cd(&mut agent, "../b").await);
    assert!(
        out.starts_with("Moved to") && out.contains("AGENTS.md"),
        "{out}"
    );
    assert_eq!(agent.env.cwd(), b);
    let after = agent.session.as_ref().unwrap().path();
    assert!(after.exists(), "{}", after.display());
    assert!(before == after || !before.exists());
    assert!(agent.messages.last().unwrap().content.contains("(/cd)"));
}

#[tokio::test]
async fn cd_into_an_untrusted_directory_asks_first() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = Home::new("cd-trust");
    let (a, b) = (home.dir.join("a"), home.dir.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    // Another tool's trust settings may already cover the temp directory.
    if !crate::config::is_trusted(&b) {
        let mut agent = Agent::new(test_env(&a, true, None));
        assert!(shown(cd(&mut agent, "../b").await).starts_with("Stayed in"));
        assert_eq!(agent.env.cwd(), a);
    }
    let mut agent = Agent::new(test_env(&a, true, Some("Yes, trust it and move")));
    let out = shown(cd(&mut agent, "../b").await);
    assert!(out.starts_with("Moved to"), "{out}");
    assert!(crate::config::is_trusted(&b));
}

#[tokio::test]
async fn manage_archives_and_deletes_other_sessions() {
    use crate::session::Session;
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = Home::new("manage");
    let cwd = home.dir.join("p");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut other = Session::new(&cwd, "m");
    other.title = "Other work".into();
    other
        .save(vec![Message::system("s"), Message::user("other")])
        .unwrap();
    let mut agent = Agent::new(test_env(&cwd, false, None));
    agent.session = Some(Session::new(&cwd, "mock"));
    agent.messages = vec![Message::system("s"), Message::user("mine")];
    agent.save_session();
    let mine = agent.session.as_ref().unwrap().id.clone();

    assert!(shown(manage(&mut agent, "archive", " ").await).starts_with("Usage"));
    assert!(shown(manage(&mut agent, "archive", "zzzz-none").await).starts_with("No session"));
    let out = shown(manage(&mut agent, "archive", &other.id).await);
    assert!(out.starts_with("Archived **Other work**"), "{out}");
    assert!(Session::load(&cwd, &other.id).unwrap().0.archived);
    let out = shown(manage(&mut agent, "unarchive", &other.id).await);
    assert!(out.starts_with("Unarchived"), "{out}");
    // The open session archives through its own save.
    shown(manage(&mut agent, "archive", &mine).await);
    assert!(agent.session.as_ref().unwrap().archived);
    assert!(
        shown(manage(&mut agent, "delete", &mine).await).starts_with("That's the open session")
    );
    let out = shown(manage(&mut agent, "delete", &other.id).await);
    assert!(out.starts_with("Deleted session **Other work**"), "{out}");
    assert!(!other.path().exists());
}

#[tokio::test]
async fn interactive_delete_can_be_cancelled() {
    use crate::session::Session;
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = Home::new("manage-ask");
    let cwd = home.dir.join("p");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut other = Session::new(&cwd, "m");
    other
        .save(vec![Message::system("s"), Message::user("other")])
        .unwrap();
    let mut agent = Agent::new(test_env(&cwd, true, Some("Cancel")));
    assert_eq!(
        shown(manage(&mut agent, "delete", &other.id).await),
        "Kept the session."
    );
    assert!(other.path().exists());
    let mut agent = Agent::new(test_env(&cwd, true, Some("Delete")));
    let out = shown(manage(&mut agent, "delete", &other.id).await);
    assert!(out.starts_with("Deleted session"), "{out}");
    assert!(!other.path().exists());
}

#[tokio::test]
async fn background_needs_a_saved_conversation() {
    let mut agent = Agent::new(test_env(&std::env::temp_dir(), false, None));
    assert!(shown(background(&mut agent, " ").await).starts_with("Nothing to move"));
    assert!(shown(background(&mut agent, "do it").await).contains("hasn't been saved"));
    assert!(take_exit_note().is_none());
}

#[test]
fn sessions_cli_help_and_bad_verbs() {
    let cwd = std::env::temp_dir();
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(sessions_cli(&cwd, &args(&["-h"])).unwrap(), SESSIONS_USAGE);
    let err = sessions_cli(&cwd, &args(&["delete"])).unwrap_err();
    assert!(err.to_string().contains("usage: chanto sessions delete"));
    assert!(sessions_cli(&cwd, &args(&["--search"])).is_err());
}
