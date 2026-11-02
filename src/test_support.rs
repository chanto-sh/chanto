//! Helpers shared by unit tests.

use std::path::PathBuf;

/// Run one test in a child process with its own `CHANTO_HOME`, `HOME` and
/// process-wide state (project root, permission rules, secret masking).
///
/// Call it first with the test's full path; `true` means the child already ran
/// it and passed, so the parent returns. The child's output must report the
/// test as run, so a mistyped path fails instead of matching nothing.
pub(crate) fn isolated(test_path: &str) -> bool {
    isolated_with(test_path, "")
}

/// [`isolated`], with extra lines for the child's `config.toml`.
pub(crate) fn isolated_with(test_path: &str, config: &str) -> bool {
    if std::env::var_os("CHANTO_ISOLATED_TEST_CHILD").is_some() {
        return false;
    }
    let home = temp_home();
    std::fs::write(
        home.join("config.toml"),
        format!("{config}\n[compat]\nclaude = false\ncodex = false\n"),
    )
    .unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_path, "--nocapture", "--test-threads=1"])
        .env("CHANTO_HOME", &home)
        .env("CHANTO_COMPAT_HOME", &home)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("CHANTO_ISOLATED_TEST_CHILD", "1")
        .env_remove("CHANTO_BASE_URL")
        .env_remove("CHANTO_MODEL")
        .env_remove("CHANTO_SERVER_TOKEN")
        .env_remove("CHANTO_SERVER_TOKEN_FILE")
        .env_remove("CHANTO_API_KEY")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&home);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "{test_path} in a child process:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    true
}

fn temp_home() -> PathBuf {
    let home =
        std::env::temp_dir().join(format!("chanto-isolated-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&home).unwrap();
    home
}

/// Write an executable script a test runs in place of a real program.
#[cfg(unix)]
pub(crate) fn shim(path: &std::path::Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A file just written can briefly be "busy" for exec while another test's
    // fork still holds it open (ETXTBSY on Linux). No arguments is each shim's
    // no-op, so running it here only waits that out.
    for _ in 0..50 {
        match std::process::Command::new(path).output() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            _ => break,
        }
    }
}
