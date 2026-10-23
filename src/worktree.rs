//! Git worktrees for isolated subagents: each gets its own checkout on a new
//! branch under `<repo>/.chanto/worktrees/<id>`, so parallel agents don't
//! step on each other or on the user's working tree.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub repo: PathBuf,
    /// Commit the branch started from.
    pub base: String,
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let o = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .context("running git")?;
    if !o.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&o.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Create a worktree for agent `id` from the current HEAD.
pub fn create(cwd: &Path, id: &str) -> Result<Worktree> {
    let repo = PathBuf::from(
        git(cwd, &["rev-parse", "--show-toplevel"])
            .context("worktree isolation needs a git repository")?,
    );
    let dir = repo.join(".chanto").join("worktrees");
    std::fs::create_dir_all(&dir)?;
    // Keep worktrees out of `git status` without touching the user's .gitignore.
    if let Ok(common) = git(&repo, &["rev-parse", "--git-common-dir"]) {
        let exclude = repo.join(common).join("info").join("exclude");
        let cur = std::fs::read_to_string(&exclude).unwrap_or_default();
        if !cur.lines().any(|l| l.trim() == "/.chanto/worktrees/") {
            let _ = std::fs::create_dir_all(exclude.parent().unwrap());
            let _ = std::fs::write(
                &exclude,
                format!(
                    "{cur}{}/.chanto/worktrees/\n",
                    if cur.is_empty() || cur.ends_with('\n') {
                        ""
                    } else {
                        "\n"
                    }
                ),
            );
        }
    }
    let base = git(&repo, &["rev-parse", "HEAD"])?;
    let path = dir.join(id);
    let branch = format!("chanto/{id}");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            &branch,
            &path.display().to_string(),
            "HEAD",
        ],
    )?;
    Ok(Worktree {
        path,
        branch,
        repo,
        base,
    })
}

impl Worktree {
    /// Uncommitted changes plus commits made on the branch.
    pub fn changes(&self) -> String {
        let status = git(&self.path, &["status", "--short"]).unwrap_or_default();
        let commits = git(
            &self.path,
            &["log", "--oneline", &format!("{}..HEAD", self.base)],
        )
        .unwrap_or_default();
        let mut s = String::new();
        if !status.is_empty() {
            s.push_str(&format!("Uncommitted changes:\n{status}\n"));
        }
        if !commits.is_empty() {
            s.push_str(&format!("Commits on {}:\n{commits}\n", self.branch));
        }
        s
    }

    /// Remove the worktree and its branch if the agent changed nothing.
    /// Returns true if it was removed.
    pub fn cleanup_if_unchanged(&self) -> bool {
        self.changes().is_empty() && self.remove().is_ok()
    }

    /// Remove the worktree and its branch, changes or not. On Windows a
    /// process that just exited (or a virus scan) can hold a file open for
    /// a moment, so a failed removal is retried briefly.
    pub fn remove(&self) -> Result<()> {
        let path = self.path.display().to_string();
        let mut tries = 0;
        loop {
            match git(&self.repo, &["worktree", "remove", "--force", &path]) {
                Ok(_) => break,
                Err(e) if tries >= 10 => return Err(e),
                Err(_) => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
        let _ = git(&self.repo, &["branch", "-D", &self.branch]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_cleanup() {
        let d = std::env::temp_dir().join(format!("chanto-wt-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(&d)
                    .output()
                    .unwrap()
                    .status
                    .success()
            )
        };
        run(&["init", "-q"]);
        run(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]);
        let wt = create(&d, "a1").unwrap();
        assert!(wt.path.join(".git").exists());
        assert!(wt.cleanup_if_unchanged());
        assert!(!wt.path.exists());
        let wt = create(&d, "a2").unwrap();
        std::fs::write(wt.path.join("new.txt"), "x").unwrap();
        assert!(wt.changes().contains("new.txt"));
        assert!(!wt.cleanup_if_unchanged());
        // The main checkout doesn't see the worktree directory as untracked.
        let st = Command::new("git")
            .args(["status", "--short"])
            .current_dir(&d)
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&st.stdout).trim().is_empty());
    }
}
