//! Git that Chanto runs itself, on the host and outside the sandbox: the
//! startup status, the TUI's branch, `/diff`, outcome and team diffs, and the
//! plumbing for worktrees and background runs. A repository's own config can
//! name programs git runs, and that config can come from an extracted archive
//! or be rewritten by a sandboxed command. Every such call starts from
//! [`command`], which switches off the programs the commands Chanto uses
//! would run: the fsmonitor hook, hooks, signing, filter and merge drivers
//! the repository defines, and submodules (each with a config of its own).
//! Diffs also need [`DIFF_ARGS`] (external diff and textconv drivers).
//! Not covered: transport settings (`core.sshCommand`, credential helpers)
//! for pushes and fetches to remotes the repository names.
//!
//! For a worktree Chanto made, the git directory comes from the repository's
//! own record of it, never from the worktree's `.git` file, which whatever
//! runs in the worktree can rewrite.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// `-c` overrides for every host git call. They also reach the git processes
/// git starts itself.
const BASE: [&str; 20] = [
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
    // A bare repository planted inside a project is never found by walking
    // up from the working directory.
    "-c",
    "safe.bareRepository=explicit",
    // Signing and verifying run `gpg.program`.
    "-c",
    "log.showSignature=false",
    "-c",
    "commit.gpgSign=false",
    "-c",
    "tag.gpgSign=false",
    "-c",
    "merge.verifySignatures=false",
    // A submodule's (or nested repository's) git runs with its own config.
    "-c",
    "diff.ignoreSubmodules=all",
    "-c",
    "status.submoduleSummary=false",
    "-c",
    "submodule.recurse=false",
];

/// For every diff Chanto runs: no external diff program or driver command,
/// no textconv driver, no submodule diffs.
pub const DIFF_ARGS: [&str; 3] = ["--no-ext-diff", "--no-textconv", "--ignore-submodules=all"];

/// `git` with the hardening flags, run in `dir` with no stdin.
pub fn command(dir: &Path) -> std::process::Command {
    with_git_dir(dir, None)
}

/// [`command`] with an explicit git directory and work tree (`None`: found
/// from `dir`). The drivers to neutralise are read from the same repository.
fn with_git_dir(dir: &Path, explicit: Option<(&Path, &Path)>) -> std::process::Command {
    let explicit: Vec<String> = explicit
        .map(|(gd, wt)| {
            vec![
                format!("--git-dir={}", gd.display()),
                format!("--work-tree={}", wt.display()),
            ]
        })
        .unwrap_or_default();
    let mut c = std::process::Command::new("git");
    c.args(BASE)
        .args(&explicit)
        .args(no_drivers(dir, &explicit))
        .current_dir(dir)
        .stdin(std::process::Stdio::null());
    c
}

/// Stdout of `git args` from [`command`], trimmed; None when git fails.
pub fn read(dir: &Path, args: &[&str]) -> Option<String> {
    let o = command(dir).args(args).output().ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// The driver commands Git LFS installs: they run the user's own `git-lfs`.
const LFS: [&str; 3] = [
    "git-lfs filter-process",
    "git-lfs clean -- %f",
    "git-lfs smudge -- %f",
];

/// `-c` overrides for the filter and merge drivers the repository's own
/// config defines (the user's global and system config are theirs, and Git
/// LFS's standard commands are kept): an empty filter command makes git pass
/// content through unfiltered, and a merge driver of `false` reports a
/// conflict. Reading config runs nothing.
fn no_drivers(dir: &Path, explicit: &[String]) -> Vec<String> {
    let Ok(o) = std::process::Command::new("git")
        .args(BASE)
        .args(explicit)
        .args([
            "config",
            "-z",
            "--show-scope",
            "--get-regexp",
            r"^(filter\..*\.(clean|smudge|process|required)|merge\..*\.driver)$",
        ])
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .output()
    else {
        return vec![];
    };
    let text = String::from_utf8_lossy(&o.stdout);
    let mut fields = text.split('\0');
    let mut out = Vec::new();
    while let (Some(scope), Some(entry)) = (fields.next(), fields.next()) {
        let (key, value) = entry.split_once('\n').unwrap_or((entry, ""));
        if matches!(scope, "global" | "system" | "command") || LFS.contains(&value) {
            continue;
        }
        // `required` off; a merge driver of `false` (a conflict); an empty
        // filter command (content unfiltered).
        let neutral = if key.ends_with(".required") || key.starts_with("merge.") {
            "false"
        } else {
            ""
        };
        out.extend(["-c".to_string(), format!("{key}={neutral}")]);
    }
    out
}

/// Whether `repo` itself is a bare repository (its own path, not one found
/// above it). Only for paths Chanto recorded, never a user's directory.
fn looks_bare(repo: &Path) -> bool {
    repo.join("HEAD").is_file() && repo.join("objects").is_dir() && !repo.join(".git").exists()
}

/// [`command`] on a repository Chanto recorded: [`bare`] when it is one.
pub fn for_repo(repo: &Path) -> std::process::Command {
    if looks_bare(repo) {
        bare(repo)
    } else {
        command(repo)
    }
}

fn common_dir(repo: &Path) -> Result<PathBuf> {
    let o = for_repo(repo)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .context("running git")?;
    if !o.status.success() {
        bail!("{} is not a git repository", repo.display());
    }
    let common = PathBuf::from(String::from_utf8_lossy(&o.stdout).trim());
    Ok(canonical(&common))
}

/// Canonical, without Windows' `\\?\` prefix: git refuses a verbatim
/// path as `--git-dir`.
fn canonical(p: &Path) -> PathBuf {
    crate::util::canonical(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The git directory the repository at `repo` keeps for its linked worktree
/// `worktree` (`<common dir>/worktrees/<name>`), found from the repository's
/// record of where each worktree is rather than from the worktree's `.git`.
/// Refused unless exactly one record names the worktree and that record's
/// `commondir` leads back to the repository's own git directory (git reads
/// config, objects and refs from wherever `commondir` points).
pub fn worktree_git_dir(repo: &Path, worktree: &Path) -> Result<PathBuf> {
    let common = common_dir(repo)?;
    let want = canonical(worktree);
    let found: Vec<PathBuf> = std::fs::read_dir(common.join("worktrees"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|gd| {
            // `gitdir` holds `<worktree>/.git`.
            std::fs::read_to_string(gd.join("gitdir"))
                .ok()
                .and_then(|g| Path::new(g.trim()).parent().map(canonical))
                .is_some_and(|at| at == want)
        })
        .collect();
    let gd = match found.as_slice() {
        [gd] => gd.clone(),
        [] => bail!(
            "{} is not a worktree of {}",
            worktree.display(),
            repo.display()
        ),
        _ => bail!(
            "{} has more than one worktree record for {}; run `git worktree prune`",
            repo.display(),
            worktree.display()
        ),
    };
    let commondir = std::fs::read_to_string(gd.join("commondir"))
        .with_context(|| format!("reading {}", gd.join("commondir").display()))?;
    let points = Path::new(commondir.trim());
    let points = if points.is_absolute() {
        points.to_path_buf()
    } else {
        gd.join(points)
    };
    if canonical(&points) != common {
        bail!(
            "the worktree record {} points at another repository ({})",
            gd.display(),
            points.display()
        );
    }
    Ok(gd)
}

/// [`command`] in the linked worktree `worktree` of `repo`, with its git
/// directory and work tree given explicitly.
pub fn in_worktree(repo: &Path, worktree: &Path) -> Result<std::process::Command> {
    let gd = worktree_git_dir(repo, worktree)?;
    Ok(with_git_dir(worktree, Some((&gd, worktree))))
}

/// [`command`] with an explicit git directory and work tree that Chanto keeps
/// itself (a runner script's checkout).
pub fn with_dirs(git_dir: &Path, work_tree: &Path) -> std::process::Command {
    with_git_dir(work_tree, Some((git_dir, work_tree)))
}

/// [`command`] on the bare repository `repo` (one Chanto keeps), named
/// explicitly since bare repositories aren't discovered.
pub fn bare(repo: &Path) -> std::process::Command {
    let gd = format!("--git-dir={}", repo.display());
    let mut c = std::process::Command::new("git");
    c.args(BASE)
        .arg(&gd)
        .args(no_drivers(repo, std::slice::from_ref(&gd)))
        .current_dir(repo)
        .stdin(std::process::Stdio::null());
    c
}

#[cfg(test)]
#[path = "hostgit_tests.rs"]
mod tests;
