use super::*;

#[test]
fn review_args() {
    let a = parse_review("high --fix --max-findings 7 main...feature").unwrap();
    assert_eq!(a.level.as_deref(), Some("high"));
    assert!(a.fix && !a.comment);
    assert_eq!(a.max_findings.as_deref(), Some("7"));
    assert_eq!(a.target, "main...feature");
    let a = parse_review("/fix-issue 123").unwrap();
    assert_eq!(
        a.target, "/fix-issue 123",
        "the rest is the target, even a command"
    );
    assert!(a.level.is_none());
    let a = parse_review("--comment 1234").unwrap();
    assert!(a.comment && a.target == "1234");
    assert!(parse_review("ultra").unwrap().ultra);
    assert_eq!(parse_review("low src/high").unwrap().target, "src/high");
    assert_eq!(parse_review("").unwrap(), ReviewArgs::default());
    assert!(parse_review("--max-findings lots").is_err());
    assert_eq!(
        parse_review("--max-findings all")
            .unwrap()
            .max_findings
            .as_deref(),
        Some("all")
    );
}

#[test]
fn review_prompts() {
    let a = parse_review("--fix").unwrap();
    let p = review_prompt(&a, "low", None);
    assert!(
        p.contains("at most 5") && p.contains("apply the fixes") && !p.contains("Do not modify")
    );
    let p = review_prompt(&parse_review("1234").unwrap(), "max", Some("all"));
    assert!(p.contains("`1234`") && p.contains("every finding") && p.contains("Do not modify"));
    assert!(
        review_prompt(&parse_review("--comment").unwrap(), "medium", Some("3")).contains("gh api")
    );
}

#[test]
fn tallies_commands_and_mcp_tools() {
    use crate::llm::{Message, Role, ToolCall};
    let call = |name: &str, args: serde_json::Value| ToolCall {
        id: "1".into(),
        name: name.into(),
        arguments: args,
    };
    let mut m = Message::new(Role::Assistant, String::new());
    m.tool_calls = vec![
        call(
            "Bash",
            serde_json::json!({"command": "git status --short && git diff | head -5"}),
        ),
        call(
            "Bash",
            serde_json::json!({"command": "FOO=1 cargo test -p x; ls -la"}),
        ),
        call(
            "Bash",
            serde_json::json!({"command": "cd src && git status"}),
        ),
        call("mcp__gh__list_issues", serde_json::json!({})),
        call("Read", serde_json::json!({"file_path": "x"})),
    ];
    let (bash, mcp) = tally(&[m]);
    assert_eq!(bash["git status"], 2);
    assert_eq!(bash["git diff"], 1);
    assert_eq!(bash["head"], 1);
    assert_eq!(bash["cargo test"], 1);
    assert_eq!(bash["ls"], 1);
    assert!(!bash.contains_key("cd"));
    assert_eq!(mcp["mcp__gh__list_issues"], 1);
}

#[test]
fn status_line_is_saved_keeping_the_rest() {
    let dir = std::env::temp_dir().join(format!("chanto-sl-{}", uuid::Uuid::new_v4().simple()));
    let p = dir.join("config.toml");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        &p,
        "# mine\nmodel = \"m\"\nstatus_line = \"old\"\n\n[permissions]\nallow = [\"Read\"]\n",
    )
    .unwrap();
    set_status_line(&p, "/home/me/.chanto/statusline.sh").unwrap();
    let text = std::fs::read_to_string(&p).unwrap();
    let t: toml::Table = toml::from_str(&text).unwrap();
    assert_eq!(
        t["status_line"].as_str(),
        Some("/home/me/.chanto/statusline.sh")
    );
    assert_eq!(t["permissions"]["allow"][0].as_str(), Some("Read"));
    assert!(text.starts_with("# mine"));
    set_status_line(&dir.join("new/config.toml"), "x").unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bundled_names_are_distinct_from_builtins() {
    for b in BUNDLED {
        assert!(
            !crate::commands::BUILTIN.iter().any(|c| c.name == b.name),
            "/{} is both built-in and bundled",
            b.name
        );
    }
    assert!(batch_prompt("do it", 4).contains("isolation: \"worktree\""));
    assert!(simplify_prompt("").contains("four reviewers"));
}

struct Quiet;

#[async_trait::async_trait]
impl crate::agent::Frontend for Quiet {
    fn emit(&self, _: crate::agent::Event) {}
    async fn permission(
        &self,
        _: crate::agent::PermissionRequest,
    ) -> crate::agent::PermissionReply {
        crate::agent::PermissionReply::Deny(None)
    }
    async fn question(&self, _: &str, _: &[String]) -> Option<String> {
        None
    }
}

fn test_env(dir: &Path, interactive: bool, config: crate::config::Config) -> Arc<Env> {
    use crate::llm::{Provider, ProviderKind};
    Env::new(crate::agent::EnvInit {
        config,
        provider: Provider::new(ProviderKind::Openai, "http://127.0.0.1:9", None, 32768),
        model: "mock".into(),
        mode: crate::permissions::Mode::Default,
        vision: false,
        cwd: dir.to_path_buf(),
        tools: crate::tools::Registry::builtin(),
        mcp: vec![],
        frontend: Arc::new(Quiet),
        interactive,
    })
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "chanto-bundled-{name}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn prompt(o: Option<Outcome>) -> String {
    match o {
        Some(Outcome::Prompt(p)) => p,
        _ => panic!("expected a prompt"),
    }
}

fn shown(o: Outcome) -> String {
    match o {
        Outcome::Show(s) => s,
        _ => panic!("expected shown text"),
    }
}

#[tokio::test]
async fn run_dispatches_bundled_skills() {
    let _home = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let d = tmp("run");
    let env = test_env(&d, false, Default::default());
    let mut agent = Agent::new(env);
    assert!(run(&mut agent, "nope", "").await.is_none());
    assert!(prompt(run(&mut agent, "simplify", "src/x.rs").await).contains("`src/x.rs`"));
    assert!(prompt(run(&mut agent, "simplify", "").await).contains("git status --short"));
    let p = prompt(run(&mut agent, "debug", "  it crashes ").await);
    assert!(p.contains("The problem: it crashes") && !p.contains("session_id"));
    let p = prompt(run(&mut agent, "debug", "").await);
    assert!(p.contains("didn't describe the problem"));
    agent.session = Some(crate::session::Session::new(&d, "mock"));
    let id = agent.session.as_ref().unwrap().id.clone();
    assert!(prompt(run(&mut agent, "debug", "x").await).contains(&id));
    let p = prompt(run(&mut agent, "code-review", "low 42").await);
    assert!(p.contains("(low effort)") && p.contains("`42`"));
    let p = prompt(run(&mut agent, "fewer-permission-prompts", "").await);
    assert!(p.contains("(none)") && p.contains("Allow rules already in effect: none"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn batch_needs_instruction_and_repo() {
    let d = tmp("batch");
    let env = test_env(&d, false, Default::default());
    assert!(shown(batch(&env, "  ")).starts_with("Usage"));
    if !in_git_repo(&d) {
        assert!(shown(batch(&env, "do it")).contains("needs a git repository"));
    }
    let mut config = crate::config::Config::default();
    config
        .hooks
        .entry("WorktreeCreate".into())
        .or_default()
        .push(Default::default());
    let env = test_env(&d, false, config);
    match batch(&env, " migrate ") {
        Outcome::Prompt(p) => assert!(p.contains("> migrate\n")),
        _ => panic!("expected a prompt"),
    }
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&d)
            .status()
            .unwrap()
            .success()
    );
    assert!(in_git_repo(&d));
    let env = test_env(&d, false, Default::default());
    assert!(matches!(batch(&env, "x"), Outcome::Prompt(_)));
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn code_review_levels_notes_and_prefs() {
    let _home = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _ = std::fs::remove_file(prefs_path());
    let d = tmp("review");
    // Non-interactive: prefs are neither read for writing nor saved.
    let mut agent = Agent::new(test_env(&d, false, Default::default()));
    assert!(shown(code_review(&mut agent, "--max-findings 0").await).contains("--max-findings"));
    match code_review(&mut agent, "ultra").await {
        Outcome::Prompt(p) => assert!(p.contains("(max effort)")),
        _ => panic!("expected a prompt"),
    }
    match code_review(&mut agent, "").await {
        Outcome::Prompt(p) => assert!(p.contains("(medium effort)") && p.contains("at most 10")),
        _ => panic!("expected a prompt"),
    }
    assert!(!prefs_path().exists());

    // Interactive inside a subagent: prefs are saved, review runs in the foreground.
    let mut agent = Agent::new(test_env(&d, true, Default::default()));
    agent.depth = 1;
    match code_review(&mut agent, "xhigh --max-findings 3 main").await {
        Outcome::Prompt(p) => assert!(p.contains("(xhigh effort)") && p.contains("at most 3")),
        _ => panic!("expected a prompt"),
    }
    let saved = load_prefs();
    assert_eq!(saved.level.as_deref(), Some("xhigh"));
    assert_eq!(saved.max_findings.as_deref(), Some("3"));
    // The remembered level and limit apply when none is given.
    match code_review(&mut agent, "").await {
        Outcome::Prompt(p) => assert!(p.contains("(xhigh effort)") && p.contains("at most 3")),
        _ => panic!("expected a prompt"),
    }
    // `default` forgets the remembered limit.
    match code_review(&mut agent, "--max-findings default").await {
        Outcome::Prompt(p) => assert!(p.contains("at most 25")),
        _ => panic!("expected a prompt"),
    }
    assert!(load_prefs().max_findings.is_none());
    let _ = std::fs::remove_file(prefs_path());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn review_prompt_scopes_and_limits() {
    let a = ReviewArgs::default();
    for (level, n) in [
        ("low", 5),
        ("medium", 10),
        ("high", 15),
        ("xhigh", 25),
        ("max", 40),
    ] {
        assert_eq!(default_limit(level), n);
        assert!(review_prompt(&a, level, None).contains(&format!("at most {n}")));
    }
    assert!(review_prompt(&a, "high", None).contains("tidy-ups"));
    assert!(review_prompt(&a, "xhigh", None).contains("broad and deep"));
}

#[test]
fn parse_review_edge_cases() {
    let a = parse_review("ultra --post --no-post --comment pr").unwrap();
    assert!(a.ultra && a.comment && a.level.is_none());
    assert_eq!(a.target, "pr");
    // A second level word, or a level after ultra, starts the target.
    assert_eq!(parse_review("low high").unwrap().target, "high");
    assert_eq!(parse_review("ultra low").unwrap().target, "low");
    assert_eq!(parse_review("low ultra").unwrap().target, "ultra");
    assert!(parse_review("--max-findings").is_err());
    assert_eq!(
        parse_review("--max-findings 4")
            .unwrap()
            .max_findings
            .as_deref(),
        Some("4")
    );
}

#[test]
fn command_keys() {
    assert_eq!(command_key("  ").as_deref(), None);
    assert_eq!(command_key("echo hi"), None);
    assert_eq!(command_key("# note"), None);
    assert_eq!(command_key("$(foo)"), None);
    assert_eq!(command_key("A=1 B=2"), None);
    assert_eq!(command_key("git -C x log").as_deref(), Some("git"));
    assert_eq!(command_key("npm run build").as_deref(), Some("npm run"));
    assert_eq!(command_key("rg needle src").as_deref(), Some("rg"));
}

#[test]
fn fewer_prompts_reads_sessions_and_allow_rules() {
    use crate::llm::{Message, Role, ToolCall};
    let _home = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let d = tmp("fewer");
    let mut m = Message::new(Role::Assistant, String::new());
    m.tool_calls = vec![
        ToolCall {
            id: "1".into(),
            name: "Bash".into(),
            arguments: serde_json::json!({"command": "git log -3"}),
        },
        ToolCall {
            id: "2".into(),
            name: "mcp__fs__read".into(),
            arguments: serde_json::json!({}),
        },
    ];
    let mut s = crate::session::Session::new(&d, "mock");
    s.save(vec![m.clone()]).unwrap();
    let mut config = crate::config::Config::default();
    config.permissions.allow = vec!["Read".into()];
    let env = test_env(&d, false, config);
    *env.conversation.write().unwrap() = Arc::new(vec![m]);
    let p = fewer_prompts_prompt(&env);
    assert!(p.contains("`git log` ×"), "{p}");
    assert!(p.contains("`mcp__fs__read` ×"));
    assert!(p.contains("`Read`"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn statusline_writes_config_and_prompt() {
    let _home = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let d = tmp("statusline");
    let config = crate::config::home_dir().join("config.toml");
    let before = std::fs::read_to_string(&config).ok();
    let mut cfg = crate::config::Config::default();
    cfg.status_line = Some("old-cmd".into());
    let env = test_env(&d, false, cfg);
    let (script, cmd) = statusline_script();
    match statusline(&env, "") {
        Outcome::Prompt(p) => {
            assert!(p.contains("shell prompt") && p.contains("`old-cmd`"));
            assert!(p.contains(&script.display().to_string()));
        }
        _ => panic!("expected a prompt"),
    }
    let t: toml::Table = toml::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(t["status_line"].as_str(), Some(cmd.as_str()));
    let env = test_env(&d, false, Default::default());
    match statusline(&env, " git branch ") {
        Outcome::Prompt(p) => {
            assert!(p.contains("What the user wants: git branch") && !p.contains("previous"))
        }
        _ => panic!("expected a prompt"),
    }
    // Broken TOML is reported, not overwritten.
    std::fs::write(&config, "x = [").unwrap();
    assert!(shown(statusline(&env, "")).contains("isn't valid TOML"));
    match before {
        Some(b) => std::fs::write(&config, b).unwrap(),
        None => std::fs::remove_file(&config).unwrap(),
    }
    let _ = std::fs::remove_dir_all(&d);
}
