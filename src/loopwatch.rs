//! Spotting unproductive loops in a turn: the same failing command with the
//! same error, long runs of failing tool calls, and edits that flip a file
//! back to an earlier state. Retrying after a real change isn't flagged:
//! a different error, or any successful call, resets the evidence.

use crate::llm::ToolCall;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;

/// The same failing command with the same error this many times.
const SAME_FAILURE: usize = 3;
/// This many failing tool calls in a row.
const FAILURE_RUN: usize = 8;
/// A file returned to an earlier state this many times.
const REVERTS: usize = 2;

#[derive(Default)]
pub struct LoopWatch {
    failures: HashMap<u64, usize>,
    failure_run: usize,
    file_states: HashMap<String, Vec<u64>>,
    reverts: HashMap<String, usize>,
}

fn hash<T: Hash>(t: &T) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    t.hash(&mut h);
    h.finish()
}

/// The tail of an error, without numbers (timings, PIDs, line counts) or
/// spacing differences, so the same error compares equal.
fn normalize_error(content: &str) -> String {
    let tail: String = content
        .chars()
        .rev()
        .take(400)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    tail.chars()
        .filter(|c| !c.is_ascii_digit())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

impl LoopWatch {
    /// Look at one batch: the calls, and per call (id, is_error, result text).
    /// Returns a description of the loop when one is detected.
    pub fn observe(
        &mut self,
        calls: &[ToolCall],
        results: &[(String, bool, String)],
        cwd: &Path,
    ) -> Option<String> {
        let mut found = None;
        // The file is read after the whole batch ran, so several calls that
        // touch the same file all see its one final state.
        let mut seen = HashSet::new();
        for (id, is_error, content) in results {
            let Some(call) = calls.iter().find(|c| &c.id == id) else {
                continue;
            };
            if *is_error {
                self.failure_run += 1;
                let what = match call.name.as_str() {
                    "Bash" => call.arguments["command"].as_str().unwrap_or("").to_string(),
                    _ => format!("{}{}", call.name, call.arguments),
                };
                let n = self
                    .failures
                    .entry(hash(&(what.clone(), normalize_error(content))))
                    .or_insert(0);
                *n += 1;
                if *n >= SAME_FAILURE {
                    found = Some(format!(
                        "`{}` has failed {} times with the same error",
                        crate::util::one_line(&what, 80),
                        n
                    ));
                } else if self.failure_run >= FAILURE_RUN {
                    found = Some(format!(
                        "the last {} tool calls all failed",
                        self.failure_run
                    ));
                }
                continue;
            }
            self.failure_run = 0;
            if matches!(call.name.as_str(), "Edit" | "Write" | "MultiEdit") {
                let Some(p) = call.arguments["file_path"].as_str() else {
                    continue;
                };
                let path = crate::util::resolve(cwd, p);
                let key = path.display().to_string();
                // Many calls can hit one file in a batch; after the batch it
                // only has one state, and counting that state once per call
                // would look like a file being reverted.
                if !seen.insert(key.clone()) {
                    continue;
                }
                let Ok(bytes) = std::fs::read(&path) else {
                    continue;
                };
                let h = hash(&bytes);
                let states = self.file_states.entry(key.clone()).or_default();
                // A call that left the file exactly as it was (a no-op edit,
                // or one that was blocked without an error label) isn't
                // evidence of undoing earlier work.
                if states.last() == Some(&h) {
                    continue;
                }
                // Back to a state from before the last one: undoing earlier work.
                if states.len() >= 2 && states[..states.len() - 1].contains(&h) {
                    let r = self.reverts.entry(key.clone()).or_insert(0);
                    *r += 1;
                    if *r >= REVERTS {
                        found = Some(format!(
                            "{} keeps being edited back to earlier versions",
                            crate::util::one_line(p, 80)
                        ));
                    }
                }
                states.push(h);
            }
        }
        if found.is_some() {
            // The next detection needs fresh evidence.
            *self = LoopWatch::default();
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash(id: &str, cmd: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "Bash".into(),
            arguments: json!({"command": cmd}),
        }
    }

    #[test]
    fn same_failure_three_times_is_a_loop_but_changing_errors_are_not() {
        let cwd = std::env::temp_dir();
        let mut w = LoopWatch::default();
        let c = [bash("1", "cargo test")];
        let fail = |n: u32| {
            vec![(
                "1".to_string(),
                true,
                format!("Error: test parse failed after {n}ms\n[exit code 101]"),
            )]
        };
        assert!(w.observe(&c, &fail(12), &cwd).is_none());
        assert!(
            w.observe(&c, &fail(40), &cwd).is_none(),
            "timings differ, same error"
        );
        assert!(
            w.observe(&c, &fail(7), &cwd)
                .unwrap()
                .contains("failed 3 times")
        );
        // Progress: each run fails differently.
        let mut w = LoopWatch::default();
        for err in [
            "a.rs:3 expected i32",
            "b.rs:9 missing field",
            "c.rs:1 unused import",
            "d.rs:4 borrow error",
        ] {
            assert!(
                w.observe(&c, &[("1".into(), true, format!("Error: {err}"))], &cwd)
                    .is_none()
            );
        }
    }

    #[test]
    fn long_failure_runs_and_flip_flopping_edits() {
        let dir = std::env::temp_dir().join(format!("chanto-loop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut w = LoopWatch::default();
        let mut hit = None;
        for i in 0..FAILURE_RUN {
            let c = [bash("1", &format!("try {i}"))];
            hit = w.observe(
                &c,
                &[(
                    "1".into(),
                    true,
                    format!("Error: attempt {i} {}", "x".repeat(i)),
                )],
                &dir,
            );
        }
        assert!(hit.unwrap().contains("all failed"));
        // A -> B -> A -> B -> A: undone twice.
        let mut w = LoopWatch::default();
        let f = dir.join("f.txt");
        let edit = ToolCall {
            id: "e".into(),
            name: "Write".into(),
            arguments: json!({"file_path": f.display().to_string()}),
        };
        let mut hit = None;
        for content in ["A", "B", "A", "B"] {
            std::fs::write(&f, content).unwrap();
            hit = hit.or(w.observe(
                std::slice::from_ref(&edit),
                &[("e".into(), false, "ok".into())],
                &dir,
            ));
        }
        assert!(hit.unwrap().contains("back to earlier versions"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_batch_touching_a_file_many_times_is_not_a_revert() {
        let dir = std::env::temp_dir().join(format!("chanto-loop-batch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("README.md");
        std::fs::write(&f, "final\n").unwrap();
        let mut w = LoopWatch::default();
        // Four calls to the same file in one batch; the file is read after the
        // batch, so all four see `final` and must not look like a revert.
        let calls: Vec<ToolCall> = (0..4)
            .map(|i| ToolCall {
                id: i.to_string(),
                name: "Edit".into(),
                arguments: json!({"file_path": f.display().to_string()}),
            })
            .collect();
        let results: Vec<(String, bool, String)> = (0..4)
            .map(|i| (i.to_string(), false, "ok".into()))
            .collect();
        assert!(w.observe(&calls, &results, &dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_no_op_edit_is_not_a_revert() {
        let dir = std::env::temp_dir().join(format!("chanto-loop-noop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("README.md");
        std::fs::write(&f, "unchanged\n").unwrap();
        let edit = ToolCall {
            id: "e".into(),
            name: "Edit".into(),
            arguments: json!({"file_path": f.display().to_string()}),
        };
        let mut w = LoopWatch::default();
        for _ in 0..4 {
            assert!(
                w.observe(
                    std::slice::from_ref(&edit),
                    &[("e".into(), false, "ok".into())],
                    &dir
                )
                .is_none()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
