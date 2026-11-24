use super::*;
use serde_json::json;
use std::path::{Path, PathBuf};

fn ctx_in(dir: &Path) -> Ctx {
    let env = crate::agent::tests::make_env(
        "http://localhost:1",
        dir,
        crate::permissions::Mode::Default,
        32768,
    );
    Ctx {
        env,
        cancel: tokio_util::sync::CancellationToken::new(),
        depth: 0,
        call_id: None,
        cwd_override: None,
        reads: None,
    }
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chanto-exec-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "alpha\nbeta\n").unwrap();
    dir
}

#[test]
fn truncate_short_and_long() {
    assert_eq!(truncate("abc"), "abc");
    let long = "x".repeat(MAX_OUTPUT + 10);
    let out = truncate(&long);
    assert!(out.ends_with("... (output truncated)"));
    assert!(out.chars().count() < long.chars().count());
}

#[test]
fn on_path_finds_a_common_program() {
    let probe = if cfg!(windows) { "cmd" } else { "sh" };
    assert!(on_path(probe), "{probe} should be on PATH");
    assert!(!on_path("chanto-definitely-not-a-program"));
}

#[tokio::test]
async fn run_executes_python_and_returns_printed_output() {
    if program().is_none() {
        return;
    }
    let dir = scratch();
    let ctx = ctx_in(&dir);
    let out = Exec
        .run(
            json!({"script": "print(read('a.txt').strip().upper())\nprint(sorted(glob('*.txt')))\nprint(grep('beta', ['a.txt']))"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("ALPHA"), "{}", out.content);
    assert!(out.content.contains("a.txt"), "{}", out.content);
    assert!(out.content.contains("beta"), "{}", out.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn run_reports_a_failing_script() {
    if program().is_none() {
        return;
    }
    let dir = scratch();
    let ctx = ctx_in(&dir);
    let out = Exec
        .run(json!({"script": "raise ValueError('boom')"}), &ctx)
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.content.contains("boom"), "{}", out.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn run_blocks_imports_and_open() {
    if program().is_none() {
        return;
    }
    let dir = scratch();
    let ctx = ctx_in(&dir);
    let imported = Exec
        .run(json!({"script": "import os\nprint(os.getcwd())"}), &ctx)
        .await
        .unwrap();
    assert!(imported.is_error);
    assert!(
        imported.content.contains("module not allowed"),
        "{}",
        imported.content
    );
    let opened = Exec
        .run(json!({"script": "print(open('/etc/passwd').read())"}), &ctx)
        .await
        .unwrap();
    assert!(opened.is_error);
    assert!(
        opened.content.contains("NameError") || opened.content.contains("not defined"),
        "{}",
        opened.content
    );
    let _ = std::fs::remove_dir_all(&dir);
}
