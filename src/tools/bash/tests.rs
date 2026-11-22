#![cfg(any(target_os = "linux", windows))]

use super::*;

fn shell() -> BgShell {
    BgShell {
        id: "test".into(),
        command: "true".into(),
        cwd: std::env::temp_dir(),
        description: None,
        monitor: None,
        output: Mutex::new(String::new()),
        read_pos: Mutex::new(0),
        watch_pos: Mutex::new(0),
        exit: Mutex::new(None),
        pid: None,
        network: Mutex::new(None),
    }
}

#[test]
fn undo_config_changes_restores_the_files_and_reports_them() {
    let d = std::env::temp_dir().join(format!("chanto-undo-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(d.join(".git/hooks")).unwrap();
    let sh = shell();
    let guard = config_guard::ConfigGuard::take(&d);
    std::fs::write(d.join(".git/hooks/pre-commit"), "evil").unwrap();
    undo_config_changes(&sh, Some(guard));
    assert!(!d.join(".git/hooks/pre-commit").exists());
    let out = sh.output.lock().unwrap();
    // The note shows the path as the platform prints it (backslashes on Windows).
    assert!(out.contains("pre-commit"), "{out}");
    drop(out);
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn undo_config_changes_is_quiet_without_a_guard() {
    let sh = shell();
    undo_config_changes(&sh, None);
    assert!(sh.output.lock().unwrap().is_empty());
}
