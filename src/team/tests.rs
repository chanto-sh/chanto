use super::*;
use crate::agent::tests::{make_env_with, mock_server_routed, tc};
use crate::permissions::Mode;
use crate::runs::tests::{TestHome, make_repo, sh};

fn write(path: &str, content: &str, done: &str) -> Vec<Value> {
    vec![
        json!({"tool_calls": tc("Write", json!({"file_path": path, "content": content}))}),
        json!({"content": done}),
    ]
}

fn plan(items: &[(&str, &str, &str, &str)]) -> Value {
    let subtasks: Vec<Value> = items.iter().map(|(id, title, task, owns)| json!({"id": id, "title": title, "task": task, "owns": [owns]})).collect();
    json!({"content": format!("Here is the plan:\n```json\n{}\n```", json!({"subtasks": subtasks}))})
}

fn opts(base: &str) -> TeamOpts {
    TeamOpts {
        agents: 2,
        budget_usd: 4.0,
        run_options: json!({"model": "mock", "provider": "openai", "base_url": base, "sandbox": false, "verify": false}),
    }
}

#[tokio::test]
async fn team_runs_subtasks_in_parallel_and_merges_them() {
    let home = TestHome::new("team");
    let repo = make_repo(&home);
    let (base, seen) = mock_server_routed(vec![
        (
            "Team mode, step 1",
            vec![plan(&[
                ("t1", "Alpha", "ALPHA-TASK: create alpha.txt", "alpha.txt"),
                ("t2", "Beta", "BETA-TASK: create beta.txt", "beta.txt"),
            ])],
        ),
        (
            "Team review",
            vec![json!({"content": "ACCEPT"}), json!({"content": "ACCEPT"})],
        ),
        (
            "ALPHA-TASK",
            write("alpha.txt", "a\n", "Created alpha.txt."),
        ),
        ("BETA-TASK", write("beta.txt", "b\n", "Created beta.txt.")),
    ])
    .await;
    let m = home.manager();
    let env = make_env_with(&base, &repo, Mode::AcceptEdits, 32768, |c| {
        c.guardrails.checks = vec!["test -f alpha.txt && test -f beta.txt".into()]
    });
    let mut lead = Agent::new(env.clone());
    let report = start(
        &mut lead,
        "add alpha and beta files",
        opts(&base),
        &m,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        report.contains("All 2 sub-task(s) merged onto `chanto/team/")
            && report.contains("the checks pass"),
        "{report}"
    );
    assert_eq!(
        env.mode(),
        Mode::AcceptEdits,
        "mode restored after planning"
    );

    let team = list().into_iter().next().unwrap();
    assert_eq!(team.state, TeamState::Done);
    // One background run per sub-task, grouped under the team, from the shared base, with half the budget each.
    let runs: Vec<Run> = m
        .list()
        .into_iter()
        .filter(|r| r.team.as_deref() == Some(team.id.as_str()))
        .collect();
    assert_eq!(runs.len(), 2);
    assert!(
        runs.iter().all(|r| r.base == team.base
            && r.settings.max_cost_usd == 2.0
            && r.state == RunState::Done),
        "{runs:?}"
    );
    assert!(
        runs.iter()
            .any(|r| r.task.starts_with(&format!("[team {}/t1] Alpha", team.id)))
    );
    // Each result was reviewed before merging.
    assert_eq!(
        texts(&seen)
            .iter()
            .filter(|t| t.contains("Team review"))
            .count(),
        2
    );
    // The team branch has both results; checks ran there; the checkout is untouched.
    assert_eq!(
        sh(&repo, &["show", &format!("{}:alpha.txt", team.branch)]),
        "a"
    );
    assert_eq!(
        sh(&repo, &["show", &format!("{}:beta.txt", team.branch)]),
        "b"
    );
    assert_eq!(team.checks.len(), 1);
    assert!(team.checks[0].ok, "{:?}", team.checks);
    assert!(!repo.join("alpha.txt").exists());
    assert!(team.subtasks.iter().all(|t| t.state == SubState::Merged));
    // `chanto team apply`: the team's commits onto the current branch.
    let msg = git::apply(&repo, &team.as_run()).unwrap();
    assert!(msg.contains("Applied 2 commit(s)"), "{msg}");
    assert_eq!(
        crate::runs::tests::checked_out(repo.join("beta.txt")),
        "b\n"
    );
}

#[tokio::test]
async fn conflicting_subtasks_are_redelegated_on_top_of_the_merged_work() {
    let home = TestHome::new("team-conflict");
    let repo = make_repo(&home);
    let (base, _) = mock_server_routed(vec![
        ("Team mode, step 1", vec![plan(&[("t1", "One", "ONE-TASK: write one into shared.txt", "shared.txt"), ("t2", "Two", "TWO-TASK: write two into shared.txt", "shared.txt")])]),
        ("Team review", vec![json!({"content": "ACCEPT"}), json!({"content": "ACCEPT"})]),
        // The re-delegated attempt starts from the merged work and keeps it.
        (
            "conflicted with the work merged before it",
            vec![
                json!({"tool_calls": tc("Read", json!({"file_path": "shared.txt"}))}),
                json!({"tool_calls": tc("Write", json!({"file_path": "shared.txt", "content": "one\ntwo\n"}))}),
                json!({"content": "Kept one, added two."}),
            ],
        ),
        ("ONE-TASK", write("shared.txt", "one\n", "Wrote one.")),
        ("TWO-TASK", write("shared.txt", "two\n", "Wrote two.")),
    ])
    .await;
    let m = home.manager();
    let env = make_env_with(&base, &repo, Mode::AcceptEdits, 32768, |c| {
        c.guardrails.checks = vec!["grep -q one shared.txt && grep -q two shared.txt".into()]
    });
    let mut lead = Agent::new(env);
    let report = start(
        &mut lead,
        "write one and two",
        opts(&base),
        &m,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(report.contains("All 2 sub-task(s) merged"), "{report}");
    let team = list().into_iter().next().unwrap();
    let t2 = &team.subtasks[1];
    assert_eq!(t2.runs.len(), 2, "{t2:?}");
    assert!(
        t2.note
            .as_deref()
            .unwrap_or("")
            .contains("re-delegated after conflicts in shared.txt"),
        "{t2:?}"
    );
    // The retry started from the team branch with t1 merged, not from the original base.
    let retry = crate::runs::manager::Manager::get(&m, &t2.runs[1]).unwrap();
    assert_ne!(retry.base, team.base);
    assert_eq!(
        sh(&repo, &["show", &format!("{}:shared.txt", retry.base)]),
        "one"
    );
    assert_eq!(
        sh(&repo, &["show", &format!("{}:shared.txt", team.branch)]),
        "one\ntwo"
    );
    assert!(team.checks.iter().all(|c| c.ok), "{:?}", team.checks);
}

#[cfg(unix)]
#[tokio::test]
async fn teammate_idle_hook_sends_a_subtask_back_to_work() {
    let home = TestHome::new("team-idle");
    let repo = make_repo(&home);
    let (base, _) = mock_server_routed(vec![
        (
            "Team mode, step 1",
            vec![plan(&[(
                "t1",
                "Alpha",
                "ALPHA-TASK: create alpha.txt",
                "alpha.txt",
            )])],
        ),
        ("Team review", vec![json!({"content": "ACCEPT"})]),
        (
            "ALPHA-TASK",
            [
                write("alpha.txt", "a\n", "Created alpha.txt."),
                write("alpha.txt", "a\nb\n", "Added b."),
            ]
            .concat(),
        ),
    ])
    .await;
    let m = home.manager();
    let log = repo.join("..").join("idle.log");
    let marker = repo.join("..").join("idle-once");
    // The first time the teammate is about to go idle, send it back with feedback.
    let hook = format!(
        "cat >> '{log}'; echo >> '{log}'; [ -f '{m}' ] && exit 0; touch '{m}'; echo 'also add b' >&2; exit 2",
        log = log.display(),
        m = marker.display()
    );
    let env = make_env_with(&base, &repo, Mode::AcceptEdits, 32768, |c| {
        c.hooks.insert(
            "TeammateIdle".into(),
            vec![crate::config::HookConfig {
                command: hook,
                ..Default::default()
            }],
        );
    });
    let mut lead = Agent::new(env);
    let report = start(
        &mut lead,
        "add alpha",
        opts(&base),
        &m,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(report.contains("merged"), "{report}");
    let team = list().into_iter().next().unwrap();
    let t1 = &team.subtasks[0];
    assert_eq!(t1.runs.len(), 2, "sent back once: {t1:?}");
    let second = crate::runs::manager::Manager::get(&m, &t1.runs[1]).unwrap();
    assert!(
        second.task.contains("also add b") && second.task.contains("TeammateIdle"),
        "{}",
        second.task
    );
    let payloads: Vec<Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(payloads.len(), 2);
    assert_eq!(
        (
            payloads[0]["teammate_name"].as_str(),
            payloads[0]["team_name"].as_str()
        ),
        (Some("t1"), Some(team.id.as_str()))
    );
}

fn texts(seen: &Mutex<Vec<Value>>) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|b| b["messages"].to_string())
        .collect()
}

#[test]
fn plans_parse_and_overlaps_are_found() {
    let p = parse_plan("```json\n{\"subtasks\": [{\"id\": \"a\", \"title\": \"A\", \"task\": \"do a\", \"owns\": [\"src/a/**\"]}, {\"id\": \"a\", \"task\": \"do b\", \"owns\": [\"src/a/x.rs\"]}, {\"id\": \"c\", \"task\": \"\"}]}\n```", 8).unwrap();
    assert_eq!(p.len(), 2);
    assert_eq!(p[1].id, "t2", "duplicate ids are renumbered");
    assert_eq!(
        overlaps(&p),
        vec![("a".into(), "t2".into(), "src/a/** / src/a/x.rs".into())]
    );
    assert!(parse_plan("no plan here", 8).is_err());
    let one = parse_plan(r#"{"subtasks": [{"task": "x", "owns": ["src/ab.rs"]}, {"task": "y", "owns": ["src/a.rs"]}]}"#, 1).unwrap();
    assert_eq!(one.len(), 1);
}
