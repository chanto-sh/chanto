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
