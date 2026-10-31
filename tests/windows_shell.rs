#![cfg(windows)]

// The model's shell on an ordinary Windows machine, through the built binary:
// the runners have Git Bash installed, which can't start under the sandbox, so
// these fail if a command ever goes back to it there.

use std::path::{Path, PathBuf};
use std::process::Command;

struct Project {
    dir: PathBuf,
    root: PathBuf,
    home: PathBuf,
}

impl Project {
    fn new() -> Self {
        let id = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("chanto-shell-{id}-{nanos}"));
        let root = dir.join("project");
        let home = dir.join("home");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        Self { dir, root, home }
    }

    /// The Bash tool's output for `command`.
    fn bash(&self, command: &str) -> String {
        let args = serde_json::json!({"command": command, "description": "test"}).to_string();
        let out = Command::new(env!("CARGO_BIN_EXE_chanto"))
            .args(["--run-tool", "Bash", &args])
            .current_dir(&self.root)
            .env("CHANTO_HOME", &self.home)
            .env("CHANTO_NO_PRESENCE", "1")
            .env_remove("CHANTO_SHELL")
            .output()
            .unwrap();
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }

    fn sandbox_off(&self) {
        std::fs::write(
            self.home.join("config.toml"),
            "[sandbox]\nenabled = false\n",
        )
        .unwrap();
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A path outside the project and the temp dir, which the sandbox leaves writable.
fn outside(name: &str) -> PathBuf {
    let home = std::env::var_os("USERPROFILE").unwrap();
    Path::new(&home).join(format!("{name}-{}.txt", std::process::id()))
}

#[test]
fn sandboxed_commands_run_and_stay_inside_the_project() {
    let p = Project::new();
    let out = p.bash("Set-Content inside.txt hi");
    assert!(p.root.join("inside.txt").exists(), "{out}");
    assert!(!out.contains("fatal error"), "{out}");

    let file = outside("chanto-sandboxed");
    let out = p.bash(&format!("Set-Content '{}' hi", file.display()));
    let wrote = file.exists();
    let _ = std::fs::remove_file(&file);
    assert!(!wrote, "the sandbox let a write out: {out}");
    assert!(out.to_lowercase().contains("denied"), "{out}");
}

#[test]
fn sandboxed_commands_report_their_status() {
    let p = Project::new();
    let out = p.bash("cmd /c exit 7");
    assert!(out.contains("is_error: true") && out.contains('7'), "{out}");
    let out = p.bash("$PSVersionTable.PSVersion.Major");
    assert!(out.contains("is_error: false"), "{out}");
}

#[test]
fn without_the_sandbox_git_bash_still_runs_posix_commands() {
    let p = Project::new();
    p.sandbox_off();
    let file = outside("chanto-unsandboxed");
    let out = p.bash(&format!("echo hi > '{}' && echo bash-ran", file.display()));
    let wrote = file.exists();
    let _ = std::fs::remove_file(&file);
    assert!(out.contains("bash-ran"), "{out}");
    assert!(wrote, "{out}");
}
