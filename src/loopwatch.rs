//! Spotting unproductive loops in a turn: the same failing command with the
//! same error, long runs of failing tool calls, and edits that flip a file
//! back to an earlier state. Retrying after a real change isn't flagged:
//! a different error, or any successful call, resets the evidence. Also
//! spots calls that hunt for a ready-made answer outside the project
//! ([`off_project_search`]).

use crate::llm::ToolCall;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};

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

/// The note added to the result of a call that searches outside the project.
pub fn off_project_note(what: &str) -> String {
    format!(
        "<system-reminder>This call {what}, outside the project. The answer comes from the code in the \
         working directory and its own dependencies, not from the disk, package caches, other checkouts \
         or the project's upstream history. Go back to the project's code and tests.</system-reminder>"
    )
}

/// What a call does to look for an answer outside the project, if it does:
/// a whole-disk or home-directory search (`find /`, `grep -r ~`, `rg … $HOME`,
/// `locate`), a search of the directories above the project, `pip download`
/// of another release, or `git fsck --lost-found`. The call still runs; the
/// caller only adds a note steering back to the project. `root` is the
/// project directory, `home` the user's home.
pub fn off_project_search(call: &ToolCall, root: &Path, home: Option<&Path>) -> Option<String> {
    let wide = |p: &str| wide_path(p, root, home);
    match call.name.as_str() {
        "Bash" => {
            let cmd = call.arguments["command"].as_str()?;
            crate::permissions::split_shell(cmd)
                .iter()
                .find_map(|part| off_project_command(part, &wide))
        }
        "Grep" | "Glob" => {
            let p = call.arguments["path"].as_str()?;
            wide(p).then(|| format!("searches `{p}`"))
        }
        _ => None,
    }
}

fn off_project_command(part: &str, wide: &dyn Fn(&str) -> bool) -> Option<String> {
    let words = shell_words::split(part).ok()?;
    // Leading `VAR=value` assignments and wrappers don't change what runs.
    let mut words = words
        .iter()
        .map(String::as_str)
        .skip_while(|w| {
            (w.contains('=') && !w.starts_with('-'))
                || matches!(*w, "sudo" | "command" | "time" | "nice")
        })
        .peekable();
    let prog = words.next()?;
    let prog = prog.rsplit('/').next().unwrap_or(prog);
    let args: Vec<&str> = words.collect();
    let searched = |paths: &[&str]| {
        paths
            .iter()
            .find(|p| wide(p))
            .map(|p| format!("searches `{p}`"))
    };
    match prog {
        // Start points come before the first expression.
        "find" | "gfind" => {
            let starts: Vec<&str> = args
                .iter()
                .copied()
                .skip_while(|a| matches!(*a, "-H" | "-L" | "-P" | "-E" | "-X" | "-s" | "-x" | "-d"))
                .take_while(|a| !a.starts_with('-') && !matches!(*a, "(" | "!" | "\\("))
                .collect();
            searched(&starts)
        }
        "grep" | "egrep" | "fgrep" | "ggrep" => {
            let recursive = args.iter().any(|a| {
                *a == "--recursive"
                    || *a == "--dereference-recursive"
                    || (a.starts_with('-') && !a.starts_with("--") && a.contains(['r', 'R']))
            });
            recursive
                .then(|| searched(&positional_after_pattern(&args)))
                .flatten()
        }
        "rg" | "ag" | "ack" | "fd" | "fdfind" => searched(&positional_after_pattern(&args)),
        "locate" | "plocate" | "mlocate" => Some("searches the whole disk".into()),
        "pip" | "pip3" => (args.first() == Some(&"download"))
            .then(|| "downloads another release of a package".into()),
        p if p.starts_with("python") => {
            let i = args.iter().position(|a| *a == "-m")?;
            (matches!(args.get(i + 1), Some(&"pip")) && args.get(i + 2) == Some(&"download"))
                .then(|| "downloads another release of a package".into())
        }
        "git" => {
            let i = args.iter().position(|a| *a == "fsck")?;
            args[i + 1..]
                .iter()
                .any(|a| matches!(*a, "--lost-found" | "--unreachable"))
                .then(|| "digs through git's unreachable objects".into())
        }
        _ => None,
    }
}

/// The non-option arguments after the pattern (the paths searched), for
/// grep-like commands. With `-e`/`-f`/`--regexp` the pattern is an option's
/// value, so every non-option argument is a path.
fn positional_after_pattern<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut pattern_given = false;
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if matches!(a, "-e" | "-f" | "--regexp" | "--file") {
            pattern_given = true;
            i += 2;
            continue;
        }
        if a.starts_with("--regexp=") || a.starts_with("--file=") {
            pattern_given = true;
        } else if !a.starts_with('-') {
            out.push(a);
        }
        i += 1;
    }
    if !pattern_given && !out.is_empty() {
        out.remove(0);
    }
    out
}

/// A path that reaches far beyond the project: the filesystem root, the home
/// directory, a package cache in it, or a directory above the project (which
/// holds other checkouts).
fn wide_path(p: &str, root: &Path, home: Option<&Path>) -> bool {
    let expanded = if p == "~" || p == "$HOME" || p == "${HOME}" {
        match home {
            Some(h) => h.to_path_buf(),
            None => return true,
        }
    } else if let Some(rest) = ["~/", "$HOME/", "${HOME}/"]
        .iter()
        .find_map(|pre| p.strip_prefix(pre))
    {
        match home {
            Some(h) => h.join(rest),
            None => return false,
        }
    } else {
        root.join(p)
    };
    let p = lexical(&expanded);
    let root = lexical(root);
    if p == root || p.starts_with(&root) {
        return false;
    }
    let caches = |h: &Path| {
        let h = lexical(h);
        p == h || p.starts_with(h.join(".cache")) || p.starts_with(h.join("Library/Caches"))
    };
    p.parent().is_none() || home.is_some_and(caches) || root.starts_with(&p)
}

/// `.` and `..` resolved without touching the filesystem.
fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
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

    fn off(cmd: &str) -> Option<String> {
        let root = Path::new("/work/bench/proj");
        off_project_search(&bash("1", cmd), root, Some(Path::new("/home/me")))
    }

    #[test]
    fn searches_outside_the_project_are_spotted() {
        for cmd in [
            "find / -name pylinter.py -path '*pylint*'",
            "find ~ -name '*.whl'",
            "find .. -maxdepth 3 -name '*.json'",
            "find -L / -name x",
            "cd src && find / -name '*.py' 2>/dev/null | head",
            "find $HOME/.cache -name 'pytest*'",
            "ls && rg -l ignore_paths ~/Library/Caches",
            "grep -rn 'def ignore_paths' /",
            "grep -R pattern ~",
            "grep --recursive -l x $HOME",
            "rg -n 'recursive' /",
            "rg 'ignore-paths' ~/",
            "rg -e x /home/me",
            "sudo find / -name x",
            "locate pylinter.py",
            "pip download pylint==2.15.0 --no-deps --no-binary :all:",
            "pip3 download pytest",
            "python -m pip download pytest==5.0",
            "python3 -m pip download pytest",
            "git fsck --lost-found",
            "git fsck --unreachable --no-reflogs",
            "LC_ALL=C grep -r x /",
        ] {
            assert!(off(cmd).is_some(), "{cmd}");
        }
    }

    #[test]
    fn searches_inside_the_project_are_not() {
        for cmd in [
            "find . -name '*.py'",
            "find src tests -name conftest.py",
            "find /work/bench/proj/src -type f",
            "find /usr/include -name zlib.h",
            "grep -n x /etc/hosts",
            "grep x /",
            "grep -rn 'def ignore_paths' .",
            "grep -rn 'def ignore_paths' src/",
            "rg recursive",
            "rg -n recursive pylint/",
            "rg /",
            "pip install -e .",
            "pip show pylint",
            "python -m pip install pytest",
            "git fsck",
            "git log --oneline -5",
            "ls ~",
            "cat ~/.gitconfig",
        ] {
            assert_eq!(off(cmd), None, "{cmd}");
        }
    }

    #[test]
    fn the_search_tools_are_checked_too() {
        let root = Path::new("/work/proj");
        let home = Some(Path::new("/home/me"));
        let grep = |path: &str| ToolCall {
            id: "1".into(),
            name: "Grep".into(),
            arguments: json!({"pattern": "x", "path": path}),
        };
        assert!(off_project_search(&grep("/"), root, home).is_some());
        assert!(off_project_search(&grep("~"), root, home).is_some());
        assert!(off_project_search(&grep("/work"), root, home).is_some());
        assert_eq!(off_project_search(&grep("src"), root, home), None);
        assert_eq!(
            off_project_search(&grep("/work/proj/src"), root, home),
            None
        );
        // A project in the home directory itself is the project.
        assert_eq!(
            off_project_search(&grep("~"), Path::new("/home/me"), home),
            None
        );
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
