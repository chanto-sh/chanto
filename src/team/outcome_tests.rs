//! Teams whose sub-tasks don't simply pass: rejected in review, failed runs,
//! failing checks, and a team stopped while its sub-tasks run.

use super::*;
use crate::agent::tests::{make_env_with, mock_server_routed, tc};
use crate::permissions::Mode;
use crate::runs::tests::{TestHome, make_repo};

fn plan(items: &[(&str, &str, &str, &str)]) -> Value {
    let subtasks: Vec<Value> = items
        .iter()
        .map(|(id, title, task, owns)| json!({"id": id, "title": title, "task": task, "owns": [owns]}))
        .collect();
    json!({"content": format!("```json\n{}\n```", json!({"subtasks": subtasks}))})
}

fn write(path: &str, content: &str, done: &str) -> Vec<Value> {
    vec![
        json!({"tool_calls": tc("Write", json!({"file_path": path, "content": content}))}),
        json!({"content": done}),
    ]
}

fn opts(base: &str) -> TeamOpts {
    TeamOpts {
        agents: 2,
        budget_usd: 4.0,
        run_options: json!({"model": "mock", "provider": "openai", "base_url": base, "sandbox": false, "verify": false}),
    }
}

#[tokio::test]
async fn a_rejected_subtask_is_retried_once_then_reported_with_failing_checks() {
    let home = TestHome::new("team-reject");
    let repo = make_repo(&home);
    // A request goes to the first route whose key its first user message
    // contains. The review prompt quotes the sub-task, so "Team review" must
    // come before the sub-task routes.
    let (base, _) = mock_server_routed(vec![
        (
            "Team mode, step 1",
            vec![
                // Prose first: the lead is asked again for just the JSON.
                json!({"content": "I'd do it in one step."}),
                plan(&[("t1", "Gamma", "GAMMA-TASK: create gamma.txt", "gamma.txt")]),
            ],
        ),
        (
            "Team review",
            vec![
                json!({"content": "REJECT: no tests"}),
                json!({"content": "REJECT: still no tests"}),
            ],
        ),
        (
            "GAMMA-TASK",
            [
                write("gamma.txt", "g\n", "Created gamma.txt."),
                write("gamma.txt", "g2\n", "Created gamma.txt again."),
            ]
            .concat(),
        ),
    ])
    .await;
    let m = home.manager();
    let env = make_env_with(&base, &repo, Mode::AcceptEdits, 32768, |c| {
        c.guardrails.checks = vec!["printf 'out-%s\\n' ran && false".into()]
    });
    let mut lead = Agent::new(env.clone());
    let report_text = start(
        &mut lead,
        "add gamma",
        opts(&base),
        &m,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let team = list().into_iter().next().unwrap();
    assert_eq!(team.state, TeamState::Done);
    assert!(
        report_text.contains(&format!(
            "**Result:** ⚠ 0/1 sub-task(s) merged onto `{}`; the checks FAIL on the merged result.",
            team.branch
        )),
        "{report_text}"
    );
    let t1 = &team.subtasks[0];
    assert_eq!(t1.state, SubState::Rejected);
    assert_eq!(t1.runs.len(), 2, "retried once: {t1:?}");
    assert_eq!(
        t1.note.as_deref(),
        Some("rejected in review: still no tests")
    );
    // The retry carried the first review as feedback.
    let second = crate::runs::manager::Manager::get(&m, &t1.runs[1]).unwrap();
    assert!(
        second.task.contains("was rejected in review: no tests"),
        "{}",
        second.task
    );
    assert!(report_text.contains("- ✗ **t1** Gamma · rejected · runs "));
    assert!(report_text.contains("\n- ❌ `printf 'out-%s\\n' ran && false`"));
    assert!(
        report_text.contains("out-ran"),
        "the failing output is shown"
    );
    assert!(
        !report_text.contains("chanto team apply"),
        "nothing merged, nothing to apply"
    );
    // The report is kept, and resuming a finished team just shows it.
    assert_eq!(
        std::fs::read_to_string(team.dir().join("report.md")).unwrap(),
        report_text
    );
    assert_eq!(
        resume(&env, &team.id, &m, CancellationToken::new())
            .await
            .unwrap(),
        report_text
    );
    let listed = command(&repo, "list").await;
    let row = listed
        .lines()
        .find(|l| l.starts_with(&team.id))
        .expect("the team is listed");
    assert!(
        row.contains("done") && row.contains("0/1") && row.contains("add gamma"),
        "{row}"
    );
    let status = command(&repo, "status").await;
    assert!(
        status.starts_with(&format!("Last team `{}` (Done):", team.id)),
        "{status}"
    );
    assert_eq!(
        command(&repo, "stop").await,
        "No team is running in this session."
    );
}

#[tokio::test]
async fn stopping_a_team_stops_its_running_subtasks() {
    let home = TestHome::new("team-stop");
    let repo = make_repo(&home);
    // A request goes to the first route whose key its first user message
    // contains; each sub-task's task text names only its own route.
    let (base, _) = mock_server_routed(vec![
        (
            "Team mode, step 1",
            vec![plan(&[
                ("t1", "Waits", "WAIT-TASK: build it", "build.txt"),
                ("t2", "Breaks", "FAIL-TASK: break it", "broken.txt"),
            ])],
        ),
        // A command that needs permission: the run waits for input.
        (
            "WAIT-TASK",
            vec![json!({"tool_calls": tc("Bash", json!({"command": "make build-it"}))})],
        ),
        (
            "FAIL-TASK",
            vec![
                json!({"http_status": 400, "body": "{\"error\": {\"message\": \"bad request\"}}"}),
            ],
        ),
    ])
    .await;
    let m = home.manager();
    let env = make_env_with(&base, &repo, Mode::Default, 32768, |_| {});
    let mut lead = Agent::new(env.clone());
    let run = start(
        &mut lead,
        "build and break",
        opts(&base),
        &m,
        CancellationToken::new(),
    );
    let control = async {
        // Stop once the team has seen one sub-task fail and one wait for input.
        let lines = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let s = command(&repo, "status").await;
                if s.contains("need input") && s.contains("✗ t2 Breaks · failed") {
                    return s;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the sub-tasks never reached their states");
        assert!(lines.starts_with("Team `"), "{lines}");
        assert!(lines.contains("· ⚠ needs input (/runs attach)"), "{lines}");
        let said = command(&repo, "stop").await;
        assert!(
            said.starts_with("Stopping team `") && said.ends_with("` and its running sub-tasks."),
            "{said}"
        );
    };
    let (report_text, ()) = tokio::join!(run, control);
    let report_text = report_text.unwrap();
    assert!(
        report_text.contains("**Result:** ⚠ Stopped; 0/2 sub-task(s) merged."),
        "{report_text}"
    );
    let team = list().into_iter().next().unwrap();
    assert_eq!(team.state, TeamState::Cancelled);
    let t2 = &team.subtasks[1];
    assert_eq!(t2.state, SubState::Failed);
    assert!(
        t2.note
            .as_deref()
            .is_some_and(|n| n.starts_with(&format!("run {} failed: ", t2.runs[0]))),
        "{t2:?}"
    );
    // The waiting run was stopped with the team.
    let waiting = crate::runs::manager::Manager::get(&m, &team.subtasks[0].runs[0]).unwrap();
    let stopped =
        crate::runs::tests::wait_for(&m, &waiting.id, "stopped", |r| !r.state.is_active()).await;
    assert_eq!(stopped.state, RunState::Cancelled, "{stopped:?}");
    assert_eq!(
        command(&repo, "stop").await,
        "No team is running in this session."
    );
}

/// A lead stopped while it waits for a sub-task it re-delegated after a
/// conflict: `team resume` must not merge that run until it has finished,
/// and must not merge it at all if it didn't finish.
#[tokio::test]
async fn resume_does_not_merge_a_redelegated_run_that_has_not_finished() {
    let home = TestHome::new("team-resume-redelegated");
    let repo = make_repo(&home);
    let (base, _) = mock_server_routed(vec![
        (
            "Team mode, step 1",
            vec![plan(&[
                ("t1", "One", "ONE-TASK: write one into shared.txt", "shared.txt"),
                ("t2", "Two", "TWO-TASK: write two into shared.txt", "shared.txt"),
            ])],
        ),
        (
            "Team review",
            vec![json!({"content": "ACCEPT"}), json!({"content": "ACCEPT"})],
        ),
        // The re-delegated attempt writes half its work, then waits for input.
        (
            "conflicted with the work merged before it",
            vec![
                json!({"tool_calls": tc("Write", json!({"file_path": "shared.txt", "content": "one\nhalf\n"}))}),
                json!({"tool_calls": tc("Bash", json!({"command": "make finish-two"}))}),
            ],
        ),
        ("ONE-TASK", write("shared.txt", "one\n", "Wrote one.")),
        ("TWO-TASK", write("shared.txt", "two\n", "Wrote two.")),
    ])
    .await;
    let m = home.manager();
    let env = make_env_with(&base, &repo, Mode::AcceptEdits, 32768, |c| {
        c.guardrails.checks = vec!["true".into()]
    });
    let mut lead = Agent::new(env.clone());
    let shared_on_team_branch = |team: &Team| {
        crate::runs::tests::sh(&repo, &["show", &format!("{}:shared.txt", team.branch)])
    };

    // 1. Stop the lead while it waits for the re-delegated run.
    let cancel = CancellationToken::new();
    let run = start(
        &mut lead,
        "write one and two",
        opts(&base),
        &m,
        cancel.clone(),
    );
    let stop_lead = async {
        let rid = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let redelegated = list()
                    .first()
                    .and_then(|t| t.subtasks.get(1).and_then(|s| s.runs.get(1).cloned()));
                if let Some(rid) = redelegated
                    && crate::runs::manager::Manager::get(&m, &rid)
                        .is_ok_and(|r| r.state == RunState::NeedsInput)
                {
                    return rid;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("t2 was never re-delegated");
        cancel.cancel();
        rid
    };
    let (first, rid) = tokio::join!(run, stop_lead);
    assert!(first.unwrap().contains("Interrupted"));
    let team = list().into_iter().next().unwrap();
    assert_eq!(team.state, TeamState::Interrupted);
    assert_eq!(team.subtasks[0].state, SubState::Merged);
    assert_eq!(
        team.subtasks[1].state,
        SubState::Running,
        "{:?}",
        team.subtasks[1]
    );

    // 2. Resume while that run still waits: it is not merged.
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    let (second, ()) = tokio::join!(resume(&env, &team.id, &m, cancel), async {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        c2.cancel();
    });
    assert!(second.unwrap().contains("Interrupted"));
    let team = list().into_iter().next().unwrap();
    assert_eq!(
        team.subtasks[1].state,
        SubState::Running,
        "{:?}",
        team.subtasks[1]
    );
    assert_eq!(shared_on_team_branch(&team), "one", "half-done work merged");
    let live = crate::runs::manager::Manager::get(&m, &rid).unwrap();
    assert!(live.state.is_active(), "{live:?}");

    // A record from before this was fixed has the sub-task Accepted with its
    // run still going: resuming waits for that run instead of merging it.
    let mut old = team.clone();
    old.subtasks[1].state = SubState::Accepted;
    old.state = TeamState::Interrupted;
    old.save().unwrap();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    let (again, ()) = tokio::join!(resume(&env, &team.id, &m, cancel), async {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        c2.cancel();
    });
    assert!(again.unwrap().contains("Interrupted"));
    let team = list().into_iter().next().unwrap();
    assert_eq!(
        team.subtasks[1].state,
        SubState::Running,
        "{:?}",
        team.subtasks[1]
    );
    assert_eq!(shared_on_team_branch(&team), "one", "half-done work merged");

    // 3. The run is stopped unfinished: resuming reports the sub-task failed.
    crate::runs::manager::Manager::stop(&m, &rid).unwrap();
    crate::runs::tests::wait_for(&m, &rid, "stopped", |r| !r.state.is_active()).await;
    let third = resume(&env, &team.id, &m, CancellationToken::new())
        .await
        .unwrap();
    assert!(third.contains("1/2 sub-task(s) merged"), "{third}");
    let team = list().into_iter().next().unwrap();
    assert_eq!(team.state, TeamState::Done);
    assert_eq!(
        team.subtasks[1].state,
        SubState::Failed,
        "{:?}",
        team.subtasks[1]
    );
    assert_eq!(shared_on_team_branch(&team), "one");
}
