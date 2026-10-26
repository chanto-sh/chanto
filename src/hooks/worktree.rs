//! Worktree hooks: a WorktreeCreate / WorktreeRemove hook can place a run
//! in a git worktree. This resolves the target path from the hook's output
//! and creates or removes the worktree (the detached forms run the hook
//! without blocking the caller).

use super::*;

/// Where a WorktreeCreate hook says the worktree is: the last non-empty
/// stdout line of a command hook, or an http hook's `worktreePath`. A
/// relative path is resolved against `cwd`; absolute paths with `.`/`..` are
/// refused, and it must be a directory.
fn worktree_path(r: &HookResult, cwd: &std::path::Path) -> Result<PathBuf, String> {
    if let Some(why) = &r.block {
        return Err(format!("WorktreeCreate hook failed: {why}"));
    }
    if r.failed || r.timed_out {
        return Err("WorktreeCreate hook failed".into());
    }
    let from_json = r.json.iter().rev().find_map(|v| {
        v["hookSpecificOutput"]["worktreePath"]
            .as_str()
            .map(str::to_string)
    });
    let line = from_json.or_else(|| {
        r.stdout.iter().rev().find_map(|o| {
            crate::tools::bash::strip_ansi(o)
                .lines()
                .rev()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(str::to_string)
        })
    });
    let Some(line) = line else {
        return Err("WorktreeCreate hook printed no worktree path".into());
    };
    let raw = PathBuf::from(&line);
    if raw.is_absolute()
        && raw.components().any(|c| {
            matches!(
                c,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(format!(
            "WorktreeCreate hook returned a path with `.` or `..` in it: {line}"
        ));
    }
    let path = if raw.is_absolute() {
        raw
    } else {
        crate::permissions::normalize(&cwd.join(raw))
    };
    if !path.is_dir() {
        return Err(format!(
            "WorktreeCreate hook returned {}, which is not a directory",
            path.display()
        ));
    }
    Ok(path)
}

/// WorktreeCreate inside a session (an isolated subagent): `None` when no
/// hook is configured (git makes the worktree), else the hook's path.
pub async fn worktree_create(env: &Env, name: &str) -> Option<Result<PathBuf, String>> {
    if !has(env, "WorktreeCreate") {
        return None;
    }
    let r = fire(env, "WorktreeCreate", None, json!({"name": name})).await;
    Some(worktree_path(&r, &env.cwd()))
}

/// WorktreeCreate before a session starts (`--worktree`).
pub fn worktree_create_detached(
    hooks: &crate::config::HooksConfig,
    cwd: &std::path::Path,
    name: &str,
) -> Option<Result<PathBuf, String>> {
    if hooks.get("WorktreeCreate").is_none_or(|h| h.is_empty()) {
        return None;
    }
    let r = fire_detached(hooks, "WorktreeCreate", cwd, json!({"name": name}));
    Some(worktree_path(&r, cwd))
}

/// WorktreeRemove for a worktree a WorktreeCreate hook made. `None` when no
/// hook is configured; else whether it's gone (a non-zero exit only fails
/// the removal if the directory is still there).
pub fn worktree_remove_detached(
    hooks: &crate::config::HooksConfig,
    cwd: &std::path::Path,
    path: &std::path::Path,
) -> Option<bool> {
    if hooks.get("WorktreeRemove").is_none_or(|h| h.is_empty()) {
        return None;
    }
    let r = fire_detached(hooks, "WorktreeRemove", cwd, json!({"worktree_path": path}));
    Some(!(r.failed || r.timed_out || r.block.is_some()) || !path.exists())
}

/// On leaving a `--worktree` session whose worktree a WorktreeCreate hook
/// made: when git sees no changes in it, remove it through the WorktreeRemove
/// hooks or, without any, as `git worktree remove --force` would.
pub fn finish_hook_worktree(
    hooks: &crate::config::HooksConfig,
    cwd: &std::path::Path,
    dir: &std::path::Path,
) {
    let git = |args: &[&str], at: &std::path::Path| {
        crate::hostgit::command(at)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
    };
    if git(&["status", "--porcelain", "--ignore-submodules=all"], dir)
        .is_some_and(|o| !o.stdout.is_empty())
    {
        eprintln!("Changes are in {}; kept it.", dir.display());
        return;
    }
    let removed = match worktree_remove_detached(hooks, cwd, dir) {
        Some(gone) => gone,
        None => {
            git(
                &["worktree", "remove", "--force", &dir.display().to_string()],
                cwd,
            )
            .is_some()
                || !dir.exists()
        }
    };
    if removed {
        eprintln!("No changes in {}; removed it.", dir.display());
    } else {
        eprintln!("worktree removal failed, kept: {}", dir.display());
    }
}
