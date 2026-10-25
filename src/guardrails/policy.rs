//! How checks run. Check commands come from project content (`package.json`
//! scripts, `conftest.py`, a `build.rs`) that the model or a cloned repo can
//! change, so they run as a Bash command does: under the session's sandbox,
//! with the same writable set (the project, temp dirs, tool caches) and with
//! secret-named variables withheld.

use super::*;
use std::sync::{OnceLock, RwLock};

struct Policy {
    sandbox: crate::config::SandboxConfig,
    secrets: crate::secrets::SecretsConfig,
}

static POLICY: RwLock<Option<Policy>> = RwLock::new(None);

/// Record the session's sandbox and secrets settings for checks (set at
/// startup, next to the Bash sandbox switch).
pub fn set_policy(sandbox: &crate::config::SandboxConfig, secrets: &crate::secrets::SecretsConfig) {
    *POLICY.write().unwrap_or_else(|e| e.into_inner()) = Some(Policy {
        sandbox: sandbox.clone(),
        secrets: secrets.clone(),
    });
}

/// The settings checks run under; the defaults (sandboxed when the sandbox is
/// on, secrets withheld) before a session sets them.
fn policy() -> (crate::config::SandboxConfig, crate::secrets::SecretsConfig) {
    match &*POLICY.read().unwrap_or_else(|e| e.into_inner()) {
        Some(p) => (p.sandbox.clone(), p.secrets.clone()),
        None => Default::default(),
    }
}

/// The command for check `cmd` in `cwd`, built by the Bash tool's builder
/// (never with the sandbox switched off for it). Keep the network lease, if
/// any, until the check ends.
pub(super) fn check_command(
    cmd: &str,
    cwd: &Path,
    env: &[(String, String)],
) -> anyhow::Result<(tokio::process::Command, Option<crate::network::Lease>)> {
    let (sandbox, secrets) = policy();
    check_command_with(cmd, cwd, env, sandbox, &secrets)
}

/// [`check_command`] for another command Chanto runs from someone else's
/// content (a runner's job script), with no extra environment.
pub fn script_command(
    cmd: &str,
    cwd: &Path,
) -> anyhow::Result<(tokio::process::Command, Option<crate::network::Lease>)> {
    check_command(cmd, cwd, &[])
}

fn check_command_with(
    cmd: &str,
    cwd: &Path,
    env: &[(String, String)],
    mut sandbox: crate::config::SandboxConfig,
    secrets: &crate::secrets::SecretsConfig,
) -> anyhow::Result<(tokio::process::Command, Option<crate::network::Lease>)> {
    // A team or goal worktree can sit outside the project: the check may
    // write there as Bash commands may write the project.
    let root = crate::permissions::project_root().unwrap_or_else(|| cwd.to_path_buf());
    if !cwd.starts_with(&root) {
        sandbox.allow_write.push(cwd.display().to_string());
    }
    // Windows' sandbox starts commands by relaunching the Chanto executable,
    // which a test binary isn't; tests there run checks unsandboxed (still
    // without secrets). Everywhere else the sandbox applies when it's on.
    let unsandboxed = cfg!(all(test, windows));
    let (mut c, lease, _) = crate::tools::bash::build_command_with(
        cmd,
        cwd,
        Some(&sandbox),
        Some(secrets),
        unsandboxed,
    )?;
    c.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    Ok((c, lease))
}

/// The first Python that has pytest (Homebrew's `python` may have it when
/// `python3` doesn't). Found once per process; until then it is probed again
/// each time, so installing pytest mid-session is noticed. The probe runs
/// from a directory the project can't plant a `pytest.py` in, with
/// `PYTHONSAFEPATH` keeping the working directory off `sys.path`, and it
/// only looks pytest up, so it never imports project code. (`-I` would also
/// drop user site-packages, where `pip install --user pytest` puts it.)
pub(super) fn pytest_python() -> Option<&'static str> {
    static FOUND: OnceLock<&'static str> = OnceLock::new();
    if let Some(py) = FOUND.get() {
        return Some(py);
    }
    let dir = neutral_dir();
    let py = ["python3", "python"]
        .into_iter()
        .find(|py| has_pytest(py, &dir))?;
    Some(FOUND.get_or_init(|| py))
}

fn has_pytest(py: &str, dir: &Path) -> bool {
    std::process::Command::new(py)
        .args(PYTEST_PROBE)
        .current_dir(dir)
        .env("PYTHONSAFEPATH", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Finds pytest without importing it (a found spec runs no package code).
const PYTEST_PROBE: [&str; 2] = [
    "-c",
    "import importlib.util, sys; sys.exit(importlib.util.find_spec('pytest') is None)",
];

/// A working directory outside any project that sandboxed commands can't
/// write to.
fn neutral_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:\\"))
    } else {
        PathBuf::from("/")
    }
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
