use super::*;
use crate::agent::tests::{make_env_with, mock_server, tc, tmpdir};
use crate::permissions::Mode;
use serde_json::json;

fn limits(max_iterations: u32, max_stale: u32) -> Limits {
    Limits {
        max_iterations,
        max_stale,
        max_cost_usd: None,
        max_minutes: None,
    }
}

fn texts(seen: &Mutex<Vec<serde_json::Value>>) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|b| b["messages"].to_string())
        .collect()
}

#[tokio::test]
async fn goal_works_until_the_criteria_pass_and_keeps_them_locked() {
    let (dir, store) = (tmpdir("goal"), tmpdir("goal-store"));
    let crit = store.join("g1/criteria.json").display().to_string();
    let criteria = r#"[{"id": "c1", "title": "a exists", "check": "test -f a.txt"}, {"id": "c2", "title": "b says done", "check": "grep -q done b.txt"}]"#;
    let (base, seen) = mock_server(vec![
        // Criteria phase, in plan mode: the project is read-only, the goal directory isn't.
        json!({"tool_calls": tc("Write", json!({"file_path": "a.txt", "content": "early\n"}))}),
        json!({"tool_calls": tc("Write", json!({"file_path": crit, "content": criteria}))}),
        json!({"content": "Two criteria."}),
        // Iteration 1: loosens the criteria (undone), then does part of the work.
        json!({"tool_calls": tc("Read", json!({"file_path": crit}))}),
        json!({"tool_calls": tc("Write", json!({"file_path": crit, "content": "[{\"id\": \"c1\", \"title\": \"x\", \"check\": \"true\"}]"}))}),
        json!({"tool_calls": tc("Write", json!({"file_path": "a.txt", "content": "a\n"}))}),
        json!({"content": "Created a.txt; b.txt remains."}),
        // Iteration 2: the rest.
        json!({"tool_calls": tc("Write", json!({"file_path": "b.txt", "content": "done\n"}))}),
        json!({"content": "Created b.txt."}),
    ])
    .await;
    let env = make_env_with(&base, &dir, Mode::AcceptEdits, 32768, |_| {});
    let mut agent = Agent::new(env.clone());
    let (state, report) = start(
        &mut agent,
        "make a.txt and b.txt",
        &store,
        Some("g1".into()),
        limits(10, 3),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(state, GoalState::Done, "{report}");
    assert!(
        report.contains("Done: all 2 acceptance criteria pass"),
        "{report}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "a\n",
        "plan mode kept the project read-only"
    );
    assert_eq!(env.mode(), Mode::AcceptEdits);
    // The loosened criteria were put back, and the agent was told.
    assert_eq!(std::fs::read_to_string(&crit).unwrap(), criteria);
    let reqs = texts(&seen);
    assert_eq!(reqs.len(), 9);
    assert!(
        reqs[7].contains("You changed the locked acceptance criteria (criteria.json)"),
        "{}",
        reqs[7]
    );
    assert!(
        reqs[7].contains("iteration 2") && reqs[7].contains("Created a.txt; b.txt remains."),
        "progress carried into the next iteration"
    );
    // Progress and state on disk.
    let g = Goal::load(&store.join("g1")).unwrap();
    assert_eq!(
        (g.state, g.iterations, g.passing.len()),
        (GoalState::Done, 2, 2)
    );
    let progress = std::fs::read_to_string(g.progress_path()).unwrap();
    assert!(
        progress.contains("## Iteration 1")
            && progress.contains("## Iteration 2 ")
            && progress.contains("(new: c2)"),
        "{progress}"
    );
    assert!(
        progress.contains("locked acceptance criteria"),
        "{progress}"
    );
    assert!(agent.goal.is_none());
}

#[tokio::test]
async fn goal_stops_without_progress_on_limits_and_when_blocked_then_resumes_after_a_restart() {
    let (dir, store) = (tmpdir("goal-stop"), tmpdir("goal-stop-store"));
    let crit = store.join("g2/criteria.json").display().to_string();
    let (base, seen) = mock_server(vec![
        json!({"tool_calls": tc("Write", json!({"file_path": crit, "content": r#"[{"id": "c1", "title": "never", "check": "test -f never.txt"}]"#}))}),
        json!({"content": "One criterion."}),
        json!({"content": "Tried one thing."}),
        json!({"content": "Tried another thing."}),
        // After a restart: one more iteration, then blocked.
        json!({"content": "Tried again."}),
        json!({"content": "I can't go on.\nBLOCKED: an API key for the payment sandbox"}),
    ])
    .await;
    let mk = || Agent::new(make_env_with(&base, &dir, Mode::AcceptEdits, 32768, |_| {}));
    let mut agent = mk();
    let (state, report) = start(
        &mut agent,
        "create never.txt somehow",
        &store,
        Some("g2".into()),
        limits(10, 2),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(state, GoalState::Stopped);
    assert!(report.contains("no progress: 2 iteration(s) in a row without a new passing criterion; still failing: c1"), "{report}");
    assert!(report.contains("/goal resume g2"), "{report}");
    let g = Goal::load(&store.join("g2")).unwrap();
    assert_eq!((g.state, g.iterations), (GoalState::Stopped, 2));

    // A new process: everything comes back from the goal directory.
    drop(agent);
    let mut agent = mk();
    let (state, report) = resume(
        &mut agent,
        &store,
        Some("g2"),
        Some(limits(1, 5)),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(state, GoalState::Stopped);
    assert!(
        report.contains("iteration limit reached (1 iterations)"),
        "{report}"
    );
    let last = texts(&seen).pop().unwrap();
    assert!(
        last.contains("iteration 3")
            && last.contains("Tried another thing.")
            && last.contains("create never.txt somehow"),
        "{last}"
    );

    // A spent budget stops before any model call.
    let before = seen.lock().unwrap().len();
    let l = Limits {
        max_cost_usd: Some(0.0),
        ..limits(5, 5)
    };
    let (state, report) = resume(
        &mut agent,
        &store,
        Some("g2"),
        Some(l),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(state, GoalState::Stopped);
    assert!(report.contains("goal budget of $0.00 reached"), "{report}");
    assert_eq!(seen.lock().unwrap().len(), before);

    // Blocked, unattended: it stops and says what it needs.
    let (state, report) = resume(
        &mut agent,
        &store,
        None,
        Some(limits(5, 5)),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(state, GoalState::Blocked);
    assert!(
        report.contains("an API key for the payment sandbox"),
        "{report}"
    );
    assert_eq!(Goal::load(&store.join("g2")).unwrap().iterations, 4);
}

#[tokio::test]
async fn goal_survives_compaction() {
    let (dir, store) = (tmpdir("goal-compact"), tmpdir("goal-compact-store"));
    let crit = store.join("g3/criteria.json").display().to_string();
    let (base, seen) = mock_server(vec![
        json!({"tool_calls": tc("Write", json!({"file_path": crit, "content": r#"[{"id": "c1", "title": "a exists", "check": "test -f a.txt"}]"#}))}),
        json!({"content": "One criterion."}),
        // A big context: the next step compacts the conversation first.
        json!({"tool_calls": tc("Write", json!({"file_path": "a.txt", "content": "a\n"})), "prompt_tokens": 7900}),
        json!({"content": "SUMMARY OF THE WORK"}),
        json!({"content": "Wrote a.txt."}),
    ])
    .await;
    let env = make_env_with(&base, &dir, Mode::AcceptEdits, 8192, |_| {});
    let mut agent = Agent::new(env);
    let (state, report) = start(
        &mut agent,
        "make the alpha file",
        &store,
        Some("g3".into()),
        limits(5, 3),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(state, GoalState::Done, "{report}");
    let reqs = texts(&seen);
    assert_eq!(reqs.len(), 5);
    // After compaction the objective, criteria and progress come back.
    let after = &reqs[4];
    assert!(after.contains("SUMMARY OF THE WORK"), "{after}");
    assert!(
        after.contains("You are working toward a goal (id g3)")
            && after.contains("Objective: make the alpha file"),
        "{after}"
    );
    assert!(
        after.contains("c1 a exists") && after.contains("Progress so far"),
        "{after}"
    );
    assert!(
        !after.contains("Goal mode, iteration 1"),
        "the original prompt was compacted away"
    );
}

/// A frontend that answers questions from a script and records them.
struct Answers(
    Mutex<Vec<(String, Vec<String>)>>,
    Mutex<std::collections::VecDeque<&'static str>>,
);

#[async_trait::async_trait]
impl crate::agent::Frontend for Answers {
    fn emit(&self, _: Event) {}
    async fn permission(
        &self,
        _: crate::agent::PermissionRequest,
    ) -> crate::agent::PermissionReply {
        crate::agent::PermissionReply::Allow
    }
    async fn question(&self, q: &str, options: &[String]) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .push((q.to_string(), options.to_vec()));
        self.1.lock().unwrap().pop_front().map(str::to_string)
    }
}

fn interactive_env(base: &str, dir: &Path, fe: Arc<Answers>) -> Arc<crate::agent::Env> {
    let mut config = crate::config::Config::default();
    config.provider = crate::llm::ProviderKind::Openai;
    crate::agent::Env::new(crate::agent::EnvInit {
        config,
        provider: crate::llm::Provider::new(crate::llm::ProviderKind::Openai, base, None, 32768),
        model: "mock".into(),
        mode: Mode::AcceptEdits,
        vision: false,
        cwd: dir.to_path_buf(),
        tools: crate::tools::Registry::builtin(),
        mcp: vec![],
        frontend: fe,
        interactive: true,
    })
}

/// The reported bug: the check script lives in the goal directory and
/// imports the project; the criterion first names it relative to the
/// goal directory. The baseline catches that, the agent fixes it before
/// locking, and once the project is fixed the goal completes.
#[tokio::test]
async fn criteria_scripts_in_the_goal_dir_can_test_the_project() {
    let (dir, store) = (tmpdir("goal-calc"), tmpdir("goal-calc-store"));
    std::fs::write(dir.join("calc.py"), "def add(a, b):\n    return a - b\n").unwrap();
    std::fs::write(
        dir.join("test_calc.py"),
        "from calc import add\n\ndef test_add():\n    assert add(1, 1) == 2\n",
    )
    .unwrap();
    let g = store.join("g5");
    let script = "import sys\nimport calc\n\ncase = sys.argv[1]\nif case == 'add':\n    assert calc.add(2, 3) == 5, calc.add(2, 3)\nelif case == 'multiply':\n    assert calc.multiply(6, 7) == 42\nprint('ok', case)\n";
    let crit = |prefix: &str| {
        format!(
            r#"[{{"id": "c1", "title": "add adds", "check": "python3 {prefix}checks/run_checks.py add"}}, {{"id": "c2", "title": "multiply exists", "check": "python3 {prefix}checks/run_checks.py multiply"}}]"#
        )
    };
    let crit_path = g.join("criteria.json").display().to_string();
    let (base, seen) = mock_server(vec![
        json!({"tool_calls": tc("Write", json!({"file_path": g.join("checks/run_checks.py").display().to_string(), "content": script}))}),
        json!({"tool_calls": tc("Write", json!({"file_path": crit_path, "content": crit("")}))}),
        json!({"content": "Two criteria."}),
        // Fix round: the baseline found the script path broken.
        json!({"tool_calls": tc("Read", json!({"file_path": crit_path}))}),
        json!({"tool_calls": tc("Write", json!({"file_path": crit_path, "content": crit("\\\"$CHANTO_GOAL_DIR\\\"/")}))}),
        json!({"content": "Fixed the paths."}),
        // Iteration 1 fixes the project.
        json!({"tool_calls": tc("Read", json!({"file_path": "calc.py"}))}),
        json!({"tool_calls": tc("Write", json!({"file_path": "calc.py", "content": "def add(a, b):\n    return a + b\n\n\ndef multiply(a, b):\n    return a * b\n"}))}),
        json!({"content": "add adds now, and multiply exists."}),
    ])
    .await;
    let mut agent = Agent::new(make_env_with(&base, &dir, Mode::AcceptEdits, 32768, |_| {}));
    let (state, report) = start(
        &mut agent,
        "fix add, add multiply",
        &store,
        Some("g5".into()),
        limits(5, 3),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(state, GoalState::Done, "{report}");
    let reqs = texts(&seen);
    // The criteria prompt states the contract.
    assert!(
        reqs[0].contains("PROJECT ROOT") && reqs[0].contains("$CHANTO_GOAL_DIR/checks/"),
        "{}",
        reqs[0]
    );
    // The fix round named the problem.
    assert!(
        reqs[3].contains("can't run the way chanto runs them")
            && reqs[3].contains("c1: `checks/run_checks.py` is in the goal directory"),
        "{}",
        reqs[3]
    );
    let g = Goal::load(&g).unwrap();
    assert_eq!((g.iterations, g.passing.len()), (1, 2));
    assert!(g.criteria[0].check.contains("$CHANTO_GOAL_DIR"));
}

#[tokio::test]
async fn done_needs_project_checks_that_passed_before_to_still_pass() {
    let (dir, store) = (tmpdir("goal-proj"), tmpdir("goal-proj-store"));
    std::fs::write(dir.join("state.txt"), "fine\n").unwrap();
    let crit_path = store.join("g8/criteria.json").display().to_string();
    let (base, seen) = mock_server(vec![
        json!({"tool_calls": tc("Write", json!({"file_path": crit_path, "content": r#"[{"id": "c1", "title": "a", "check": "test -f a.txt"}]"#}))}),
        json!({"content": "One."}),
        // Iteration 1 meets the criterion but breaks a check that passed.
        json!({"tool_calls": tc("Write", json!({"file_path": "a.txt", "content": "a\n"}))}),
        json!({"tool_calls": tc("Read", json!({"file_path": "state.txt"}))}),
        json!({"tool_calls": tc("Write", json!({"file_path": "state.txt", "content": "BROKEN\n"}))}),
        json!({"content": "Done, I think."}),
        json!({"tool_calls": tc("Write", json!({"file_path": "state.txt", "content": "fine\n"}))}),
        json!({"content": "Repaired state.txt."}),
    ])
    .await;
    // `false` already fails before the work: it doesn't hold the goal back.
    let env = make_env_with(&base, &dir, Mode::AcceptEdits, 32768, |c| {
        c.guardrails.checks = vec!["false".into(), "! grep -q BROKEN state.txt".into()]
    });
    let mut agent = Agent::new(env);
    let (state, report) = start(
        &mut agent,
        "make a.txt",
        &store,
        Some("g8".into()),
        limits(5, 3),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(state, GoalState::Done, "{report}");
    let reqs = texts(&seen);
    assert!(
        reqs[6].contains("the project check `! grep -q BROKEN state.txt` fails (it passed before)"),
        "{}",
        reqs[6]
    );
    assert!(!reqs[6].contains("check `false`"));
    assert_eq!(Goal::load(&store.join("g8")).unwrap().iterations, 2);
}

#[tokio::test]
async fn criteria_that_cant_run_are_flagged_at_approval_and_can_be_unlocked_later() {
    let (dir, store) = (tmpdir("goal-infra"), tmpdir("goal-infra-store"));
    let crit_path = store.join("g6/criteria.json").display().to_string();
    let (base, seen) = mock_server(vec![
        json!({"tool_calls": tc("Write", json!({"file_path": crit_path, "content": r#"[{"id": "c1", "title": "done", "check": "no-such-tool-xyz --verify done.txt"}]"#}))}),
        json!({"content": "One criterion."}),
        // Iteration 1: does the work; the criterion still can't run.
        json!({"tool_calls": tc("Write", json!({"file_path": "done.txt", "content": "ok\n"}))}),
        json!({"content": "Created done.txt."}),
        // Unlocked: the agent fixes the criterion.
        json!({"tool_calls": tc("Read", json!({"file_path": crit_path}))}),
        json!({"tool_calls": tc("Write", json!({"file_path": crit_path, "content": r#"[{"id": "c1", "title": "done", "check": "test -f done.txt"}]"#}))}),
        json!({"content": "Uses test -f now."}),
    ])
    .await;
    let fe = Arc::new(Answers(
        Mutex::new(vec![]),
        Mutex::new(
            [
                "Approve anyway",
                "Unlock and fix the criteria",
                "Approve and start",
            ]
            .into_iter()
            .collect(),
        ),
    ));
    let mut agent = Agent::new(interactive_env(&base, &dir, fe.clone()));
    let (state, report) = start(
        &mut agent,
        "make done.txt",
        &store,
        Some("g6".into()),
        limits(5, 3),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(state, GoalState::Done, "{report}");
    let qs = fe.0.lock().unwrap().clone();
    assert_eq!(qs.len(), 3, "{qs:?}");
    // 1. Approval flags the criterion that can't run and offers a fix.
    assert!(
        qs[0]
            .0
            .contains("⚠ can't run: a command in the check isn't available")
            && qs[0].1.contains(&"Let the agent fix them".to_string()),
        "{qs:?}"
    );
    // 2. After the work, the broken criterion is raised as a criteria problem, not looped on.
    assert!(
        qs[1].0.contains("Criteria problem")
            && qs[1]
                .0
                .contains("c1: a command in the check isn't available"),
        "{qs:?}"
    );
    // 3. The fixed criteria are approved again, then pass.
    assert!(
        qs[2].0.contains("`test -f done.txt`") && !qs[2].0.contains("can't run"),
        "{qs:?}"
    );
    assert_eq!(texts(&seen).len(), 7);
    let progress = std::fs::read_to_string(store.join("g6/progress.md")).unwrap();
    assert!(
        progress.contains("Criteria fixed and approved again: 1/1 pass"),
        "{progress}"
    );
}

#[tokio::test]
async fn unattended_goals_stop_when_criteria_cant_be_made_to_run() {
    let (dir, store) = (tmpdir("goal-infra2"), tmpdir("goal-infra2-store"));
    let crit_path = store.join("g7/criteria.json").display().to_string();
    let (base, seen) = mock_server(vec![
        json!({"tool_calls": tc("Write", json!({"file_path": crit_path, "content": r#"[{"id": "c1", "title": "x", "check": "no-such-tool-xyz"}]"#}))}),
        json!({"content": "One."}),
        json!({"content": "Tried to fix."}),
        json!({"content": "Tried again."}),
    ])
    .await;
    let mut agent = Agent::new(make_env_with(&base, &dir, Mode::AcceptEdits, 32768, |_| {}));
    let (state, report) = start(
        &mut agent,
        "anything",
        &store,
        Some("g7".into()),
        limits(5, 3),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(state, GoalState::Stopped);
    assert!(
        report.contains(
            "the criteria can't run as chanto runs them (c1: a command in the check isn't available"
        ),
        "{report}"
    );
    assert_eq!(
        texts(&seen).len(),
        4,
        "two fix rounds, then it stops without iterating"
    );
}

fn test_goal(root: &Path, dir: &Path) -> Goal {
    Goal {
        id: "g".into(),
        objective: "o".into(),
        root: root.to_path_buf(),
        dir: dir.to_path_buf(),
        state: GoalState::Active,
        created: String::new(),
        updated: String::new(),
        criteria: vec![],
        locked: BTreeMap::new(),
        passing: vec![],
        ever_passed: vec![],
        iterations: 0,
        stale: 0,
        limits: limits(1, 1),
        cost_usd: 0.0,
        active_secs: 0,
        reason: None,
        guidance: None,
    }
}

#[tokio::test]
async fn infra_problems_are_told_apart_from_missing_behavior() {
    let (root, dir) = (tmpdir("infra-root"), tmpdir("infra-goal"));
    std::fs::write(root.join("calc.py"), "def add(a, b):\n    return a - b\n").unwrap();
    std::fs::create_dir_all(dir.join("checks")).unwrap();
    std::fs::write(
        dir.join("checks/t.py"),
        "import calc\nassert calc.add(2, 3) == 5\n",
    )
    .unwrap();
    std::fs::write(dir.join("checks/bad.py"), "def broken(:\n").unwrap();
    std::fs::write(dir.join("checks/other.py"), "import not_in_project\n").unwrap();
    let g = test_goal(&root, &dir);
    let crit = |id: &str, check: &str| Criterion {
        id: id.into(),
        title: id.into(),
        check: check.into(),
    };
    let cs = vec![
        crit("rel", "python3 checks/t.py"),
        crit("nocmd", "no-such-tool-xyz --x"),
        crit("syntax", "python3 \"$CHANTO_GOAL_DIR/checks/bad.py\""),
        crit("ok-fails", "python3 \"$CHANTO_GOAL_DIR/checks/t.py\""),
        crit("missing-data", "grep -q done out.txt"),
        crit(
            "missing-module",
            "python3 \"$CHANTO_GOAL_DIR/checks/other.py\"",
        ),
        crit("project-import", "cd / && python3 -c 'import calc'"),
    ];
    let runs = run_criteria(&g, &cs).await;
    let p = |id: &str| infra_problem(&g, runs.iter().find(|r| r.criterion.id == id).unwrap());
    assert!(
        p("rel")
            .unwrap()
            .contains("`checks/t.py` is in the goal directory"),
        "{:?}",
        p("rel")
    );
    assert!(
        p("nocmd").unwrap().contains("isn't available"),
        "{:?}",
        runs[1].output
    );
    assert!(
        p("syntax").unwrap().contains("syntax error"),
        "{:?}",
        runs[2].output
    );
    // Missing behavior: the check runs and the assertion fails (the project is on PYTHONPATH).
    assert!(
        !runs[3].ok && p("ok-fails").is_none(),
        "{:?}",
        runs[3].output
    );
    assert!(p("missing-data").is_none());
    // A module the project doesn't have could be the work still to do.
    assert!(p("missing-module").is_none(), "{:?}", runs[5].output);
    // Python started elsewhere still finds the project through PYTHONPATH.
    assert!(runs[6].ok, "{:?}", runs[6].output);
    let bad = CheckRun {
        criterion: crit("imp", "python3 x.py"),
        ok: false,
        output: "ModuleNotFoundError: No module named 'calc'".into(),
        secs: 0.0,
    };
    assert!(
        infra_problem(&g, &bad)
            .unwrap()
            .contains("can't import the project's `calc` module")
    );
    // The contract is the same everywhere: the environment and the reproduction line.
    let env = criteria_env(&g);
    assert_eq!(
        env[0],
        ("CHANTO_GOAL_DIR".to_string(), dir.display().to_string())
    );
    assert!(contract(&g).contains(&format!(
        "cd {} && CHANTO_GOAL_DIR={}",
        root.display(),
        dir.display()
    )));
}

#[test]
fn status_and_commands() {
    let g = Goal {
        id: "g".into(),
        objective: "o".into(),
        root: PathBuf::from("/nowhere"),
        dir: PathBuf::from("/nowhere/g"),
        state: GoalState::Active,
        created: String::new(),
        updated: String::new(),
        criteria: vec![
            Criterion {
                id: "c1".into(),
                title: "t".into(),
                check: "true".into(),
            },
            Criterion {
                id: "c2".into(),
                title: "u".into(),
                check: "false".into(),
            },
        ],
        locked: BTreeMap::new(),
        passing: vec!["c1".into()],
        ever_passed: vec![],
        iterations: 3,
        stale: 0,
        limits: limits(1, 1),
        cost_usd: 0.0,
        active_secs: 0,
        reason: None,
        guidance: None,
    };
    assert_eq!(g.status_line(), "◎ goal 1/2 · iteration 3");
    assert!(
        crate::commands::is_immediate("/goal pause")
            && crate::commands::is_immediate("/goal")
            && crate::commands::is_immediate("/team status")
    );
    assert!(
        !crate::commands::is_immediate("/goal make it fast")
            && !crate::commands::is_immediate("/goal resume")
            && !crate::commands::is_immediate("/team add a cache")
    );
    let d = GoalConfig::default().limits();
    assert_eq!(
        (d.max_iterations, d.max_stale, d.max_cost_usd, d.max_minutes),
        (20, 3, None, Some(120))
    );
}
