//! `chanto goal "<objective>"` in the foreground, end to end against a mock
//! model, then the subcommands that read the goal it left behind.

use super::*;
use crate::agent::tests::{mock_server_with, tc, tmpdir};
use crate::test_support::isolated;
use serde_json::{Value, json};

fn cli_with(words: &[&str], root: &Path) -> crate::Cli {
    let mut args = vec!["chanto", "-C", root.to_str().unwrap()];
    args.extend_from_slice(words);
    <crate::Cli as clap::Parser>::parse_from(args)
}

/// The goal directory the criteria prompt names.
fn goal_dir(req: &Value) -> Option<String> {
    req["messages"].as_array()?.iter().find_map(|m| {
        let text = m["content"].as_str()?;
        let (_, rest) = text.split_once("Write them to ")?;
        rest.split_once("/criteria.json")
            .map(|(d, _)| d.to_string())
    })
}

#[test]
fn a_goal_runs_in_the_foreground_until_its_criteria_pass() {
    if isolated("goal::cli_run_tests::a_goal_runs_in_the_foreground_until_its_criteria_pass") {
        return;
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let root = crate::util::canonical(tmpdir("goal-cli-run")).unwrap();
        let step = Arc::new(Mutex::new(0usize));
        let (base, seen) = mock_server_with(Arc::new(move |req: &Value| {
            let mut n = step.lock().unwrap();
            *n += 1;
            Some(match *n {
                1 => {
                    let dir = goal_dir(req).expect("the criteria prompt names the goal dir");
                    json!({"tool_calls": tc("Write", json!({
                        "file_path": format!("{dir}/criteria.json"),
                        "content": r#"[{"id": "c1", "title": "a exists", "check": "test -f a.txt"}]"#,
                    }))})
                }
                2 => json!({"content": "One criterion."}),
                3 => json!({"tool_calls": tc("Write", json!({"file_path": "a.txt", "content": "a\n"}))}),
                4 => json!({"content": "Created a.txt."}),
                _ => return None,
            })
        }))
        .await;
        let model = [
            "--provider",
            "openai",
            "--base-url",
            base.as_str(),
            "-m",
            "mock",
            "--mode",
            "acceptEdits",
        ];
        let words = [&model[..], &["goal", "create a.txt"]].concat();
        assert_eq!(
            cli(cli_with(&words, &root)).await.unwrap(),
            0,
            "a goal that's met exits 0"
        );
        assert_eq!(seen.lock().unwrap().len(), 4);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "a\n");
        let goals = list(&goals_dir());
        assert_eq!(goals.len(), 1);
        let g = &goals[0];
        assert_eq!((g.state, g.passing.len()), (GoalState::Done, 1));
        assert_eq!(g.root, root);

        // The subcommands find it.
        assert_eq!(cli(cli_with(&["goal", "list"], &root)).await.unwrap(), 0);
        assert_eq!(cli(cli_with(&["goal", "status"], &root)).await.unwrap(), 0);
        assert_eq!(
            cli(cli_with(&["goal", "status", &g.id], &root))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            cli(cli_with(&["goal", "pause", &g.id], &root))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            std::fs::read_to_string(g.dir.join("control")).unwrap(),
            "pause"
        );
        let e = cli(cli_with(&["goal", "stop"], &root)).await.unwrap_err();
        assert!(
            e.to_string().starts_with("no unfinished goal for "),
            "without an id only an unfinished goal is stopped: {e}"
        );
        assert_eq!(
            cli(cli_with(&["goal", "stop", &g.id], &root))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            std::fs::read_to_string(g.dir.join("control")).unwrap(),
            "stop"
        );
        let listed = command(&root, "list");
        assert!(listed.contains(&g.id), "{listed}");
    });
}
