//! Git plumbing for background runs.
//!
//! A run works in `<home>/runs/worktrees/<project-key>/<id>`, a worktree
//! (outside the project, so nothing that walks the project sees it) on the new
//! branch `chanto/run/<id>`. Uncommitted changes (staged, unstaged and untracked,
//! minus ignored files) come along as a snapshot commit on top of HEAD, built
//! with a throwaway index (`git add -A` into a temporary `GIT_INDEX_FILE`,
//! `write-tree`, `commit-tree`), so the user's working copy, index and stash
//! are never touched. The run's own work is everything after that commit.

use crate::util::Canonical as _;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

fn command(dir: &Path, args: &[&str]) -> Command {
    // Run directories are Chanto's own records: a bare one is named explicitly.
    let mut c = crate::hostgit::for_repo(dir);
    longpaths(&mut c);
    c.args(args);
    c
}

/// Run worktrees live under the Chanto home, a few levels deeper than the
/// project; let Git for Windows write paths past MAX_PATH there.
fn longpaths(c: &mut Command) {
    if cfg!(windows) {
        c.args(["-c", "core.longpaths=true"]);
    }
}

/// `git` in a run's worktree, with the git directory that `repo` records for
/// it. Never the worktree's own `.git` file: the run can rewrite that to
/// point at a repository whose config runs programs.
pub(crate) fn wt_git(repo: &Path, worktree: &Path, args: &[&str]) -> Result<String> {
    let mut c = crate::hostgit::in_worktree(repo, worktree)?;
    longpaths(&mut c);
    c.args(args);
    output(c, args)
}

/// `git` with a git directory and work tree Chanto keeps itself (a runner
/// script's checkout), and [`git_with`]'s transport limits.
pub(crate) fn git_dirs_with(
    git_dir: &Path,
    work_tree: &Path,
    args: &[&str],
    env: &[(String, String)],
) -> Result<String> {
    let mut c = crate::hostgit::with_dirs(git_dir, work_tree);
    longpaths(&mut c);
    c.args(args)
        .env("GIT_ALLOW_PROTOCOL", "file:http:https:ssh")
        .env("GIT_TERMINAL_PROMPT", "0")
        .envs(env.iter().map(|(k, v)| (k, v)));
    output(c, args)
}

/// [`wt_git`] with the committer identity [`git_ident`] adds.
pub(crate) fn wt_git_ident(repo: &Path, worktree: &Path, args: &[&str]) -> Result<String> {
    let mut all = ident(repo);
    all.extend_from_slice(args);
    wt_git(repo, worktree, &all)
}

pub(crate) fn git_env(dir: &Path, args: &[&str], env: &[(&str, &Path)]) -> Result<String> {
    let mut c = command(dir, args);
    for (k, v) in env {
        c.env(k, v);
    }
    output(c, args)
}

/// `git` with extra environment, e.g. [`auth_env`].
/// Also for URLs a server supplied: only the usual transports (no `ext::`
/// or `fd::`), and never a credential prompt.
pub(crate) fn git_with(dir: &Path, args: &[&str], env: &[(String, String)]) -> Result<String> {
    let mut c = command(dir, args);
    c.env("GIT_ALLOW_PROTOCOL", "file:http:https:ssh")
        .env("GIT_TERMINAL_PROMPT", "0");
    c.envs(env.iter().map(|(k, v)| (k, v)));
    output(c, args)
}

/// Environment that makes git send HTTP basic credentials to `url` (and
/// paths below it) without putting them in an argument, the URL or a config
/// file: `GIT_CONFIG_*` sets `http.<url>.extraHeader` for that command only.
pub(crate) fn auth_env(url: &str, username: &str, password: &str) -> Vec<(String, String)> {
    use base64::Engine as _;
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
    [
        ("GIT_TERMINAL_PROMPT", "0".to_string()),
        ("GIT_CONFIG_COUNT", "1".to_string()),
        ("GIT_CONFIG_KEY_0", format!("http.{url}.extraHeader")),
        (
            "GIT_CONFIG_VALUE_0",
            format!("Authorization: Basic {basic}"),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

fn output(mut c: Command, args: &[&str]) -> Result<String> {
    let o = c.output().context("running git")?;
    if !o.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&o.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim_end().to_string())
}

pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String> {
    git_env(dir, args, &[])
}

/// Committer identity flags when the repository has none configured.
fn ident(dir: &Path) -> Vec<&'static str> {
    if git(dir, &["config", "user.email"]).is_ok_and(|e| !e.is_empty()) {
        vec![]
    } else {
        vec![
            "-c",
            "user.name=chanto",
            "-c",
            "user.email=chanto@localhost",
        ]
    }
}

pub(crate) fn git_ident(dir: &Path, args: &[&str]) -> Result<String> {
    let mut all = ident(dir);
    all.extend_from_slice(args);
    git(dir, &all)
}

/// The repository's top-level directory, or a clear refusal.
pub fn toplevel(cwd: &Path) -> Result<PathBuf> {
    match git(cwd, &["rev-parse", "--show-toplevel"]) {
        Ok(p) if !p.is_empty() => Ok(PathBuf::from(p)),
        _ => bail!(
            "{} is not inside a git repository. Background runs work in their own git worktree and branch, \
             so they need one: run `git init` and commit, or start the run from a repository.",
            cwd.display()
        ),
    }
}

/// The shared `.git` directory (the same for every worktree of a repository).
fn common_dir(repo: &Path) -> Result<PathBuf> {
    let d = PathBuf::from(git(repo, &["rev-parse", "--git-common-dir"])?);
    let d = if d.is_absolute() { d } else { repo.join(d) };
    Ok(d.canonical().unwrap_or(d))
}

pub struct Prepared {
    pub repo: PathBuf,
    pub worktree: PathBuf,
    pub branch: String,
    pub base: String,
    pub head: String,
    pub head_ref: Option<String>,
    pub snapshot: Option<String>,
}

/// Commit the working tree's current state (tracked and untracked files)
/// without touching the user's index; None when it matches HEAD.
pub fn snapshot(repo: &Path, id: &str, head: &str) -> Result<Option<String>> {
    let index = common_dir(repo)?.join(format!("chanto-run-{id}.index"));
    let env = [("GIT_INDEX_FILE", index.as_path())];
    let result = (|| -> Result<Option<String>> {
        git_env(repo, &["read-tree", head], &env)?;
        git_env(repo, &["add", "-A", "."], &env)?;
        let tree = git_env(repo, &["write-tree"], &env)?;
        if tree == git(repo, &["rev-parse", &format!("{head}^{{tree}}")])? {
            return Ok(None);
        }
        let msg = format!("Uncommitted changes at the start of background run {id}");
        Ok(Some(git_ident(
            repo,
            &["commit-tree", &tree, "-p", head, "-m", &msg],
        )?))
    })();
    let _ = std::fs::remove_file(&index);
    result
}

/// Create the run's branch and worktree (under `worktrees`) from HEAD plus
/// uncommitted changes, or from the commit `start` when given (a team's
/// shared base, or its merged work so far).
pub fn prepare(cwd: &Path, id: &str, worktrees: &Path, start: Option<&str>) -> Result<Prepared> {
    let repo = toplevel(cwd)?;
    if let Some(start) = start {
        let commit = git(
            &repo,
            &[
                "rev-parse",
                "--verify",
                "-q",
                &format!("{start}^{{commit}}"),
            ],
        )
        .ok()
        .filter(|c| !start.starts_with('-') && !c.is_empty())
        .ok_or_else(|| anyhow::anyhow!("`start` {start} is not a commit in {}", repo.display()))?;
        let branch = format!("chanto/run/{id}");
        let worktree = add_worktree(&repo, id, &branch, &commit, worktrees)?;
        return Ok(Prepared {
            repo,
            worktree,
            branch,
            base: commit.clone(),
            head: commit,
            head_ref: None,
            snapshot: None,
        });
    }
    let head = git(&repo, &["rev-parse", "--verify", "-q", "HEAD"])
        .ok()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| anyhow::anyhow!("the repository at {} has no commits yet; commit once before starting a background run", repo.display()))?;
    let head_ref = git(&repo, &["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .filter(|r| !r.is_empty());
    let snapshot = snapshot(&repo, id, &head)?;
    let base = snapshot.clone().unwrap_or_else(|| head.clone());
    let branch = format!("chanto/run/{id}");
    let worktree = add_worktree(&repo, id, &branch, &base, worktrees)?;
    Ok(Prepared {
        repo,
        worktree,
        branch,
        base,
        head,
        head_ref,
        snapshot,
    })
}

/// The per-project directory name for run worktrees: the project's folder
/// name plus a hash of its full path. Kept short (unlike session keys, which
/// carry most of the path) because Windows git refuses a worktree whose
/// `$GIT_DIR` would pass MAX_PATH, and the worktree's own files nest below it.
fn worktree_key(repo: &Path) -> String {
    let full = repo.display().to_string();
    let mut h: u64 = 0xcbf29ce484222325;
    for b in full.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    let name: String = repo
        .file_name()
        .map(|n| {
            n.to_string_lossy()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .take(24)
                .collect()
        })
        .unwrap_or_default();
    if name.is_empty() {
        format!("{:08x}", h as u32)
    } else {
        format!("{name}-{:08x}", h as u32)
    }
}

/// `git worktree add` the run's branch at `base`, under `worktrees`.
pub fn add_worktree(
    repo: &Path,
    id: &str,
    branch: &str,
    base: &str,
    worktrees: &Path,
) -> Result<PathBuf> {
    let dir = worktrees.join(worktree_key(repo));
    std::fs::create_dir_all(&dir)?;
    let worktree = dir.join(id);
    git(
        repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            branch,
            &worktree.display().to_string(),
            base,
        ],
    )?;
    Ok(worktree)
}

/// What a client sends a run server: HEAD, its branch, and the snapshot of
/// uncommitted changes (if any), all from the checkout at `cwd`.
pub struct LocalState {
    pub repo: PathBuf,
    pub head: String,
    pub head_ref: Option<String>,
    pub snapshot: Option<String>,
}

pub fn local_state(cwd: &Path) -> Result<LocalState> {
    let repo = toplevel(cwd)?;
    let head = git(&repo, &["rev-parse", "--verify", "-q", "HEAD"])
        .ok()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| anyhow::anyhow!("the repository at {} has no commits yet; commit once before starting a background run", repo.display()))?;
    let head_ref = git(&repo, &["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .filter(|r| !r.is_empty());
    let snapshot = snapshot(&repo, &super::new_id(), &head)?;
    Ok(LocalState {
        repo,
        head,
        head_ref,
        snapshot,
    })
}

/// Write a git bundle of `tip` (all its history) under the ref `name`.
pub fn create_bundle(repo: &Path, tip: &str, name: &str) -> Result<PathBuf> {
    let r = format!("refs/chanto/bundle/{name}");
    let out = std::env::temp_dir().join(format!("chanto-{name}-{}.bundle", super::new_id()));
    git(repo, &["update-ref", &r, tip])?;
    let res = git(
        repo,
        &["bundle", "create", "-q", &out.display().to_string(), &r],
    );
    let _ = git(repo, &["update-ref", "-d", &r]);
    res?;
    Ok(out)
}

fn ensure_bare(repo: &Path) -> Result<()> {
    if !repo.join("HEAD").exists() {
        std::fs::create_dir_all(repo)?;
        git(repo, &["init", "-q", "--bare"])?;
    }
    Ok(())
}

fn check_ref(repo: &Path, r: &str) -> Result<()> {
    if r.is_empty()
        || r.starts_with('-')
        || git(repo, &["check-ref-format", "--allow-onelevel", r]).is_err()
    {
        bail!("invalid git ref `{r}`");
    }
    Ok(())
}

/// Server side of a remote run: fetch the code (from a git URL or an
/// uploaded bundle) into a bare repository under `store_dir/repos`, then add
/// the run's worktree from it.
/// `auth`: extra environment for fetching a git URL ([`auth_env`]).
pub fn prepare_remote(
    store_dir: &Path,
    id: &str,
    src: &serde_json::Value,
    auth: &[(String, String)],
) -> Result<(Prepared, Option<String>)> {
    let repos = store_dir.join("repos");
    let target = format!("refs/chanto/src/{id}");
    let (repo, url) = match src["kind"].as_str() {
        Some("git") => {
            let url = src["url"]
                .as_str()
                .filter(|u| !u.is_empty() && !u.starts_with('-'))
                .ok_or_else(|| anyhow::anyhow!("git source needs a `url`"))?;
            let r = src["ref"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("git source needs a `ref` (branch name)"))?;
            let repo = repos.join(format!(
                "{}.git",
                crate::session::project_key(Path::new(url))
            ));
            ensure_bare(&repo)?;
            check_ref(&repo, r)?;
            let from = if r.starts_with("refs/") {
                r.to_string()
            } else {
                format!("refs/heads/{r}")
            };
            git_with(
                &repo,
                &[
                    "fetch",
                    "-q",
                    "--no-tags",
                    "--",
                    url,
                    &format!("+{from}:{target}"),
                ],
                auth,
            )?;
            (repo, Some(url.to_string()))
        }
        Some("bundle") => {
            let bid = src["bundle_id"]
                .as_str()
                .filter(|b| !b.is_empty() && b.chars().all(|c| c.is_ascii_alphanumeric()))
                .ok_or_else(|| {
                    anyhow::anyhow!("bundle source needs the `bundle_id` from POST /runs/bundles")
                })?;
            let r = src["ref"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("bundle source needs the bundle's `ref`"))?;
            let file = store_dir.join("bundles").join(format!("{bid}.bundle"));
            if !file.exists() {
                bail!("no uploaded bundle {bid}");
            }
            let repo = repos.join("uploads.git");
            ensure_bare(&repo)?;
            check_ref(&repo, r)?;
            let res = git(
                &repo,
                &[
                    "fetch",
                    "-q",
                    "--no-tags",
                    &file.display().to_string(),
                    &format!("+{r}:{target}"),
                ],
            );
            let _ = std::fs::remove_file(&file);
            res?;
            (repo, None)
        }
        other => bail!("unsupported run source kind {other:?}"),
    };
    let tip = git(&repo, &["rev-parse", &target])?;
    let base = match src["commit"].as_str() {
        Some(c) if git(&repo, &["cat-file", "-e", &format!("{c}^{{commit}}")]).is_ok() => {
            c.to_string()
        }
        Some(c) => bail!("commit {c} isn't on the fetched ref"),
        None => tip,
    };
    let branch = format!("chanto/run/{id}");
    let worktree = add_worktree(&repo, id, &branch, &base, &store_dir.join("worktrees"))?;
    let head = src["head"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| base.clone());
    let snapshot = src["snapshot"]
        .as_str()
        .map(str::to_string)
        .filter(|s| *s == base);
    let head_ref = src["head_ref"]
        .as_str()
        .or(src["ref"].as_str().filter(|_| src["kind"] == "git"))
        .map(str::to_string);
    Ok((
        Prepared {
            repo,
            worktree,
            branch,
            base,
            head,
            head_ref,
            snapshot,
        },
        url,
    ))
}

/// A bundle of the run's own commits (prerequisite: its base, which the
/// client that started it has).
pub fn run_bundle(run: &super::Run) -> Result<PathBuf> {
    let repo = &run.source.path;
    finalize(&run.source.path, &run.worktree, &run.id, &run.task)?;
    if commit_count(repo, &run.base, &run.branch) == 0 {
        bail!(
            "run {} made no changes; there is nothing to download",
            run.id
        );
    }
    let out =
        std::env::temp_dir().join(format!("chanto-run-{}-{}.bundle", run.id, super::new_id()));
    git(
        repo,
        &[
            "bundle",
            "create",
            "-q",
            &out.display().to_string(),
            &format!("refs/heads/{}", run.branch),
            &format!("^{}", run.base),
        ],
    )?;
    Ok(out)
}

/// Client side: fetch a downloaded run bundle into the checkout at `cwd` as
/// the run's branch, and return the run as if it had run here.
pub fn import_run(cwd: &Path, run: &super::Run, bundle: &Path) -> Result<super::Run> {
    let repo = toplevel(cwd)?;
    check_imported(run)?;
    // Fetch into a private ref, never with `+` onto a branch: the server
    // named the branch, and a forced fetch could reset any branch it chose.
    let r = format!("refs/heads/{}", run.branch);
    let staging = format!("refs/chanto/import/{}", run.id);
    let _ = git(&repo, &["update-ref", "-d", &staging]);
    git_with(
        &repo,
        &[
            "-c",
            "transfer.fsckObjects=true",
            "fetch",
            "-q",
            "--no-tags",
            "--",
            &bundle.display().to_string(),
            &format!("{r}:{staging}"),
        ],
        &[],
    )?;
    let tip = git(&repo, &["rev-parse", "--verify", "-q", &staging]);
    let _ = git(&repo, &["update-ref", "-d", &staging]);
    let tip = tip?;
    // The run's base must be a commit of this checkout, below its work.
    if git(&repo, &["merge-base", "--is-ancestor", &run.base, &tip]).is_err() {
        bail!(
            "run {}'s base {} isn't a commit of this repository below its work",
            run.id,
            run.base
        );
    }
    match git(&repo, &["rev-parse", "--verify", "-q", &r]) {
        Ok(had) if had == tip => {}
        Ok(_) => bail!(
            "branch {} already exists here with other commits; delete it (`git branch -D {}`) to fetch run {} again",
            run.branch,
            run.branch,
            run.id
        ),
        // Empty old value: create only, failing if the branch appeared meanwhile.
        Err(_) => {
            git(&repo, &["update-ref", &r, &tip, ""])?;
        }
    }
    let mut local = run.clone();
    local.source.path = repo;
    local.worktree = PathBuf::new();
    Ok(local)
}

/// Refuse a run from a server whose id, branch or base could steer local git:
/// the branch must be the run's own `chanto/run/<id>` and the base a full
/// commit hash (so neither can pass for an option or another ref).
pub(crate) fn check_imported(run: &super::Run) -> Result<()> {
    if run.id.is_empty()
        || run.id.len() > 64
        || !run
            .id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("the server sent an invalid run id {:?}", run.id);
    }
    if run.branch != format!("chanto/run/{}", run.id) {
        bail!(
            "run {} has branch {:?}; only chanto/run/{} can be fetched",
            run.id,
            run.branch,
            run.id
        );
    }
    if !matches!(run.base.len(), 40 | 64) || !run.base.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("run {} has an invalid base commit {:?}", run.id, run.base);
    }
    Ok(())
}

/// Commit whatever the run left uncommitted in its worktree (of `repo`).
/// Returns whether a commit was made.
pub fn finalize(repo: &Path, worktree: &Path, id: &str, task: &str) -> Result<bool> {
    if worktree.as_os_str().is_empty() || !worktree.exists() {
        return Ok(false);
    }
    wt_git(repo, worktree, &["add", "-A", "."])?;
    if wt_git(
        repo,
        worktree,
        &["status", "--porcelain", "--ignore-submodules=all"],
    )?
    .is_empty()
    {
        return Ok(false);
    }
    let msg = format!(
        "{}\n\nBackground run {id}.",
        crate::util::one_line(task, 72)
    );
    wt_git_ident(repo, worktree, &["commit", "-q", "--no-verify", "-m", &msg])?;
    Ok(true)
}

/// Commits the run made (after its base).
pub fn commit_count(repo: &Path, base: &str, branch: &str) -> usize {
    git(repo, &["rev-list", "--count", &format!("{base}..{branch}")])
        .ok()
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0)
}

/// `git diff --stat` of the run's work, plus anything still uncommitted.
pub fn diff_stat(run: &super::Run) -> String {
    let repo = &run.source.path;
    let mut s = git(
        repo,
        &[
            &["diff", "--stat"][..],
            &crate::hostgit::DIFF_ARGS,
            &[&run.base, &run.branch],
        ]
        .concat(),
    )
    .unwrap_or_default();
    if run.worktree.exists() {
        let dirty = wt_git(
            repo,
            &run.worktree,
            &["status", "--short", "--ignore-submodules=all"],
        )
        .unwrap_or_default();
        if !dirty.trim().is_empty() {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(&format!("Uncommitted in the run's worktree:\n{dirty}"));
        }
    }
    s
}

/// Bring a finished run's commits onto the branch checked out at `cwd`.
///
/// Clean working tree: cherry-pick the run's commits (the snapshot of your
/// uncommitted changes, if any, is left out). Uncommitted changes: apply the
/// run's diff to the working tree instead, leaving it uncommitted.
pub fn apply(cwd: &Path, run: &super::Run) -> Result<String> {
    let here = toplevel(cwd)?;
    if common_dir(&here)? != common_dir(&run.source.path)? {
        bail!(
            "run {} belongs to {}, not this repository",
            run.id,
            run.source.path.display()
        );
    }
    if git(&here, &["rev-parse", "--abbrev-ref", "HEAD"]).is_ok_and(|b| b == run.branch) {
        bail!(
            "you are on the run's own branch {}; check out the branch to apply it to",
            run.branch
        );
    }
    finalize(&run.source.path, &run.worktree, &run.id, &run.task)?;
    let n = commit_count(&here, &run.base, &run.branch);
    if n == 0 {
        return Ok(format!("Run {} made no changes; nothing to apply.", run.id));
    }
    let dirty = !git(
        &here,
        &[
            "status",
            "--porcelain",
            "--untracked-files=no",
            "--ignore-submodules=all",
        ],
    )?
    .is_empty();
    if dirty {
        let diff = git(
            &here,
            &[
                &["diff", "--binary"][..],
                &crate::hostgit::DIFF_ARGS,
                &[&run.base, &run.branch],
            ]
            .concat(),
        )? + "\n";
        let mut child = command(&here, &["apply", "--whitespace=nowarn", "-"])
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()?;
        std::io::Write::write_all(child.stdin.as_mut().unwrap(), diff.as_bytes())?;
        drop(child.stdin.take());
        let o = child.wait_with_output()?;
        if !o.status.success() {
            bail!(
                "couldn't apply run {} on top of your uncommitted changes:\n{}\nCommit or stash them, then run `chanto runs apply {}` again.",
                run.id,
                String::from_utf8_lossy(&o.stderr).trim(),
                run.id
            );
        }
        return Ok(format!(
            "Applied run {}'s changes ({n} commit(s) on {}) to your working tree, uncommitted, since it has local changes.",
            run.id, run.branch
        ));
    }
    let range = format!("{}..{}", run.base, run.branch);
    if let Err(e) = git_ident(&here, &["cherry-pick", &range]) {
        let _ = git(&here, &["cherry-pick", "--abort"]);
        bail!(
            "{e}\nThe cherry-pick was aborted; nothing changed. Resolve by hand with `git cherry-pick {range}`."
        );
    }
    let onto = git(&here, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|_| "HEAD".into());
    Ok(format!(
        "Applied {n} commit(s) from {} onto {onto}.",
        run.branch
    ))
}

/// Push the run's branch and open a pull request with `gh`.
pub fn open_pr(run: &super::Run, remote: &str) -> Result<String> {
    let repo = &run.source.path;
    finalize(&run.source.path, &run.worktree, &run.id, &run.task)?;
    if commit_count(repo, &run.base, &run.branch) == 0 {
        bail!(
            "run {} made no changes; there is nothing to open a pull request for",
            run.id
        );
    }
    git(repo, &["push", "-u", remote, &run.branch])?;
    let title = crate::util::one_line(&run.task, 72);
    let mut body = format!("Background run `{}`.\n\n{}", run.id, run.task);
    if let Some(v) = &run.verify {
        body.push_str(&format!("\n\nVerification: {} {}", v.status, v.detail));
    }
    if run.source.snapshot.is_some() {
        body.push_str(
            "\n\nThe first commit is a snapshot of uncommitted changes the run started from.",
        );
    }
    let mut args = vec![
        "pr",
        "create",
        "--head",
        run.branch.as_str(),
        "--title",
        title.as_str(),
        "--body",
        body.as_str(),
    ];
    if let Some(b) = &run.source.head_ref {
        args.extend(["--base", b.as_str()]);
    }
    let o = Command::new("gh")
        .args(&args)
        .current_dir(repo)
        .output()
        .context("running gh (install the GitHub CLI to open pull requests)")?;
    if !o.status.success() {
        bail!(
            "gh pr create: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Remove the run's worktree and branch.
pub fn remove(run: &super::Run) {
    let repo = &run.source.path;
    if run.worktree.exists() {
        let _ = git(
            repo,
            &[
                "worktree",
                "remove",
                "--force",
                &run.worktree.display().to_string(),
            ],
        );
        // The source repository may be gone; the directory goes regardless.
        let _ = std::fs::remove_dir_all(&run.worktree);
    }
    if let Some(parent) = run.worktree.parent() {
        let _ = std::fs::remove_dir(parent); // only when no other run of the project is left
    }
    let _ = git(repo, &["worktree", "prune"]);
    let _ = git(repo, &["branch", "-D", &run.branch]);
}

#[cfg(test)]
#[path = "git_tests.rs"]
mod tests;
