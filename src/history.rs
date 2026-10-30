//! Private working-tree snapshots. No command uses the project's git directory.
use crate::util::Canonical as _;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryConfig {
    pub max_file_bytes: u64,
    pub max_snapshot_bytes: u64,
    pub timeout_ms: u64,
    pub retention_days: u64,
}
impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            max_file_bytes: 5 * 1024 * 1024,
            max_snapshot_bytes: 100 * 1024 * 1024,
            timeout_ms: 2000,
            retention_days: 30,
        }
    }
}

/// How long a project's first snapshot may take (see `Shadow::in_dir`).
const FIRST_SNAPSHOT_MS: u64 = 30_000;

pub struct Shadow {
    root: PathBuf,
    repo: PathBuf,
    index: PathBuf,
    start: Instant,
    config: HistoryConfig,
    /// No index yet: the next snapshot is the project's first, which stores
    /// every file.
    first: bool,
}
/// One changed entry: path, blob in the compared tree, blob now, and whether
/// either side is a symlink.
type ChangedBlob = (String, Option<String>, Option<String>, bool);

impl Shadow {
    pub fn new(root: &Path, config: &HistoryConfig) -> Result<Self> {
        Self::in_dir(&crate::config::home_dir().join("history"), root, config)
    }
    /// Open the shadow repository for `root` under `base` (normally `~/.chanto/history`).
    pub fn in_dir(base: &Path, root: &Path, config: &HistoryConfig) -> Result<Self> {
        let root = root.canonical()?;
        let repo = repo_dir(base, &root);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&repo)?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(&repo)?;
        let mut config = config.clone();
        // The first snapshot of a project hashes every file (seconds on a big
        // one); later ones only look at what changed. A first snapshot cut off
        // at the usual budget would never finish, and every session would warn.
        // It runs alongside the turn, so a longer one-time allowance costs nothing.
        let first = !repo.join("index").exists();
        if first {
            config.timeout_ms = config.timeout_ms.max(FIRST_SNAPSHOT_MS);
        }
        let s = Self {
            root,
            index: repo.join("index"),
            repo,
            start: Instant::now(),
            config,
            first,
        };
        if !s.repo.join("HEAD").exists() {
            // Two sessions starting in a new project at once both ran `git
            // init`, and the loser's ("cannot copy … File exists") left it
            // without shell-change rewind for the whole session.
            let lock = std::fs::File::create(s.repo.join("init-lock"))?;
            lock.lock()?;
            if !s.repo.join("HEAD").exists() {
                s.git(&["init", "--bare", s.repo.to_str().unwrap()])?;
                CREATED.lock().unwrap().insert(s.repo.clone());
            }
        }
        s.clear_stale_locks(Duration::from_secs(120));
        Ok(s)
    }
    /// Git leaves `index.lock` behind when it's killed mid-write (a snapshot
    /// over its time budget, or Chanto itself killed), and from then on every
    /// snapshot of the project fails. No snapshot runs anywhere near `older_than`,
    /// so a lock that old has no live owner.
    fn clear_stale_locks(&self, older_than: Duration) {
        for name in ["index.lock", "discovery-index.lock"] {
            let lock = self.repo.join(name);
            let old = std::fs::metadata(&lock)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age >= older_than);
            if old {
                let _ = std::fs::remove_file(&lock);
            }
        }
    }
    fn git(&self, args: &[&str]) -> Result<Vec<u8>> {
        self.git_with_input(args, None)
    }
    /// `git` with `input` (written to a file first) as its stdin.
    fn git_with_input(&self, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
        let input_path = input.map(|_| self.repo.join(format!("input-{}", uuid::Uuid::new_v4())));
        let stdin = match (input, &input_path) {
            (Some(bytes), Some(p)) => {
                let opened = std::fs::write(p, bytes).and_then(|_| std::fs::File::open(p));
                match opened {
                    Ok(f) => Stdio::from(f),
                    Err(e) => {
                        let _ = std::fs::remove_file(p);
                        return Err(e.into());
                    }
                }
            }
            _ => Stdio::null(),
        };
        let result = self.git_run(args, stdin);
        if let Some(p) = input_path {
            let _ = std::fs::remove_file(p);
        }
        result
    }
    fn git_run(&self, args: &[&str], stdin: Stdio) -> Result<Vec<u8>> {
        let out = self.repo.join(format!("output-{}", uuid::Uuid::new_v4()));
        let file = std::fs::File::create(&out)?;
        let err_path = self.repo.join(format!("stderr-{}", uuid::Uuid::new_v4()));
        let err_file = std::fs::File::create(&err_path)?;
        let mut cmd = Command::new("git");
        let discovery = args.contains(&"--others");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(key);
            }
        }
        if args.first() != Some(&"init") {
            cmd.env("GIT_WORK_TREE", &self.root);
        }
        let child = cmd
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.autocrlf=false",
            ])
            .args(args)
            .current_dir(&self.root)
            .env("GIT_DIR", &self.repo)
            .env(
                "GIT_INDEX_FILE",
                if discovery {
                    self.repo.join("discovery-index")
                } else {
                    self.index.clone()
                },
            )
            .stdin(stdin)
            .stdout(Stdio::from(file))
            .stderr(Stdio::from(err_file))
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                let _ = std::fs::remove_file(&out);
                let _ = std::fs::remove_file(&err_path);
                // Said as "No such file or directory (os error 2)" otherwise.
                if e.kind() == std::io::ErrorKind::NotFound && self.root.is_dir() {
                    bail!("git isn't installed (or isn't on PATH)");
                }
                return Err(e.into());
            }
        };
        let result = loop {
            if let Some(status) = child.try_wait()? {
                break if status.success() {
                    std::fs::read(&out).map_err(Into::into)
                } else {
                    // Git's own first line says what went wrong (a held lock, a corrupt index).
                    let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
                    let why = stderr
                        .lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .unwrap_or("no message");
                    Err(anyhow::anyhow!(
                        "shadow git {} failed: {why}",
                        args.iter()
                            .find(|a| !a.starts_with('-') && !a.contains('='))
                            .unwrap_or(&"")
                    ))
                };
            }
            if self.start.elapsed() >= Duration::from_millis(self.config.timeout_ms) {
                // Git for Windows' git.exe is a launcher for the real git; kill
                // the tree so no orphan keeps the index locked or the project busy.
                #[cfg(windows)]
                let _ = Command::new("taskkill")
                    .args(["/PID", &child.id().to_string(), "/T", "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = child.kill();
                let _ = child.wait();
                // We killed the lock's owner; leaving it would fail every later snapshot.
                self.clear_stale_locks(Duration::ZERO);
                break Err(anyhow::anyhow!("snapshot time budget exceeded"));
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        let _ = std::fs::remove_file(out);
        let _ = std::fs::remove_file(err_path);
        result
    }
    fn paths(&self) -> Result<Vec<String>> {
        let bytes = self.git(&[
            "ls-files",
            "--others",
            "--exclude-standard",
            "--exclude=node_modules/",
            "--exclude=target/",
            "--exclude=.venv/",
            "-z",
        ])?;
        let mut size = 0;
        let mut paths = Vec::new();
        // Chanto's own directory, when the project contains it (started in
        // ~): its sessions and this very shadow repository aren't project files.
        // (The shadow repository is `<chanto home>/history/<hash>`.)
        let own = self
            .repo
            .parent()
            .and_then(Path::parent)
            .and_then(|h| h.canonical().ok())
            .and_then(|h| {
                h.strip_prefix(&self.root)
                    .ok()
                    .map(|r| format!("{}/", r.to_string_lossy()))
            });
        for raw in bytes.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            if self.start.elapsed() >= Duration::from_millis(self.config.timeout_ms) {
                bail!("snapshot time budget exceeded");
            }
            let p = std::str::from_utf8(raw)?;
            if p.split('/')
                .any(|s| matches!(s, ".git" | "node_modules" | "target" | ".venv"))
            {
                continue;
            }
            if own.as_deref().is_some_and(|o| o != "/" && p.starts_with(o)) {
                continue;
            }
            let meta = match std::fs::symlink_metadata(self.root.join(p)) {
                Ok(m) => m,
                // Deleted since git listed it (a temp file): nothing to keep.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if meta.is_dir() {
                continue;
            }
            if meta.len() > self.config.max_file_bytes {
                continue;
            }
            size += meta.len();
            if size > self.config.max_snapshot_bytes {
                bail!("snapshot size budget exceeded");
            }
            paths.push(p.to_owned());
        }
        Ok(paths)
    }
    /// Stage the listed paths into the shadow index. `update-index --stdin`
    /// rather than `git add --pathspec-from-file`: matching every file against
    /// every pathspec is quadratic (28 s for 30k files), this is linear, and
    /// files whose stat data is unchanged aren't hashed again. A file deleted
    /// between listing and adding (another process's temp file) is dropped
    /// from the index by `--remove` and from `paths` here.
    ///
    /// The first snapshot stores every file. Written as one loose object
    /// each, that took 18-25 s for 30k small files; with a big-file threshold
    /// of one byte git streams them all into a single pack instead (2.5 s).
    /// Same blob ids, so later snapshots and rewinds read them as usual. Only
    /// the first: for the handful of files a later snapshot stores, loose
    /// objects are cheap and a pack per snapshot would pile up.
    fn add_paths(&self, paths: &mut Vec<String>) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let input: Vec<u8> = paths
            .iter()
            .flat_map(|p| p.bytes().chain(std::iter::once(0)))
            .collect();
        let mut args = vec!["update-index", "--add", "--remove", "-z", "--stdin"];
        if self.first {
            args.splice(0..0, ["-c", "core.bigFileThreshold=1"]);
        }
        self.git_with_input(&args, Some(&input))?;
        paths.retain(|p| std::fs::symlink_metadata(self.root.join(p)).is_ok());
        Ok(())
    }
    pub fn snapshot(&self) -> Result<String> {
        // Serialize the complete index transaction across threads and processes.
        let lock = std::fs::File::create(self.repo.join("snapshot-lock"))?;
        while let Err(error) = lock.try_lock() {
            if let std::fs::TryLockError::Error(error) = error {
                return Err(error.into());
            }
            if self.start.elapsed() >= Duration::from_millis(self.config.timeout_ms) {
                bail!("snapshot time budget exceeded");
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        // Checked under the lock: with no index before this snapshot, the
        // index ends up holding exactly the paths added, so none need removing.
        let fresh = !self.index.exists();
        let mut paths = self.paths()?;
        let result = self.add_paths(&mut paths);
        if !paths.is_empty() {
            result?;
        }
        let removed: Vec<u8> = if fresh {
            Vec::new()
        } else {
            let present: std::collections::BTreeSet<_> =
                paths.iter().map(String::as_bytes).collect();
            let indexed = self.git(&["ls-files", "-z"])?;
            indexed
                .split(|b| *b == 0)
                .filter(|p| !p.is_empty() && !present.contains(p))
                .flat_map(|p| p.iter().copied().chain(std::iter::once(0)))
                .collect()
        };
        if !removed.is_empty() {
            // Linear, like `add_paths` (`git rm --pathspec-from-file` isn't).
            self.git_with_input(
                &["update-index", "--force-remove", "-z", "--stdin"],
                Some(&removed),
            )?;
        }
        let tree = String::from_utf8(self.git(&["write-tree"])?)?
            .trim()
            .to_owned();
        self.git(&["update-ref", &format!("refs/checkpoints/{tree}"), &tree])?;
        Ok(tree)
    }
    fn entries(&self, tree: &str) -> Result<BTreeMap<String, (String, String)>> {
        let bytes = self.git(&["ls-tree", "-r", "-z", tree])?;
        let mut entries = BTreeMap::new();
        for line in bytes.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            let line = std::str::from_utf8(line)?;
            let (head, path) = line
                .split_once('\t')
                .ok_or_else(|| anyhow::anyhow!("invalid tree"))?;
            let parts: Vec<_> = head.split_whitespace().collect();
            if Path::new(path)
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
                || path.split('/').any(|s| s == ".git")
            {
                bail!("unsafe snapshot path");
            }
            entries.insert(path.into(), (parts[0].into(), parts[2].into()));
        }
        Ok(entries)
    }
    pub fn changes(&self, tree: &str) -> Result<(usize, usize, usize)> {
        let current = self.snapshot()?;
        let old = self.entries(tree)?;
        let now = self.entries(&current)?;
        Ok((
            old.iter()
                .filter(|(p, v)| now.get(*p).is_some_and(|n| n != *v))
                .count(),
            now.keys().filter(|p| !old.contains_key(*p)).count(),
            old.keys().filter(|p| !now.contains_key(*p)).count(),
        ))
    }
    /// Paths that differ between `tree` and the working tree now, with their
    /// contents in `tree` (None: the path is new). Fails on a symlink.
    pub fn changed_since(&self, tree: &str) -> Result<Vec<(String, Option<Vec<u8>>)>> {
        let current = self.snapshot()?;
        let old = self.entries(tree)?;
        let now = self.entries(&current)?;
        let mut out = Vec::new();
        for p in old
            .keys()
            .chain(now.keys())
            .collect::<std::collections::BTreeSet<_>>()
        {
            if old.get(p) == now.get(p) {
                continue;
            }
            let before = match old.get(p) {
                Some((mode, _)) if mode == "120000" => bail!("symlink changed: {p}"),
                Some((_, blob)) => Some(self.git(&["cat-file", "blob", blob])?),
                None => None,
            };
            out.push((p.clone(), before));
        }
        Ok(out)
    }
    /// Paths that differ between `tree` and the working tree now (taking a
    /// new snapshot): (path, blob in `tree`, blob now, either is a symlink),
    /// a missing blob meaning the file was added or deleted.
    pub fn changed_blobs(&self, tree: &str) -> Result<Vec<ChangedBlob>> {
        let current = self.snapshot()?;
        let old = self.entries(tree)?;
        let now = self.entries(&current)?;
        let mut out = Vec::new();
        for p in old
            .keys()
            .chain(now.keys())
            .collect::<std::collections::BTreeSet<_>>()
        {
            let (a, b) = (old.get(p), now.get(p));
            if a == b {
                continue;
            }
            let link = [a, b].iter().flatten().any(|(mode, _)| mode == "120000");
            out.push((
                p.clone(),
                a.map(|(_, blob)| blob.clone()),
                b.map(|(_, blob)| blob.clone()),
                link,
            ));
        }
        Ok(out)
    }
    /// A stored blob's contents, when it is at most `max` bytes.
    pub fn blob(&self, id: &str, max: u64) -> Result<Option<Vec<u8>>> {
        let size: u64 = String::from_utf8(self.git(&["cat-file", "-s", id])?)?
            .trim()
            .parse()?;
        if size > max {
            return Ok(None);
        }
        Ok(Some(self.git(&["cat-file", "blob", id])?))
    }
    fn restorable(&self, p: &str) -> Result<bool> {
        let path = self.root.join(p);
        let mut parent = path.parent();
        while let Some(dir) = parent {
            if dir == self.root {
                break;
            }
            if std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
                bail!("refusing symlinked parent: {}", dir.display());
            }
            parent = dir.parent();
        }
        if self.git(&["check-ignore", "--no-index", "--", p]).is_ok() {
            return Ok(false);
        }
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.len() > self.config.max_file_bytes) {
            return Ok(false);
        }
        Ok(true)
    }
    pub fn restore_summary(&self, tree: &str) -> Result<String> {
        let current = self.snapshot()?;
        let old = self.entries(tree)?;
        let now = self.entries(&current)?;
        let mut paths: Vec<_> = now
            .keys()
            .filter(|p| !old.contains_key(*p))
            .map(|p| format!("Delete: {p}"))
            .collect();
        paths.extend(
            old.iter()
                .filter(|(p, v)| now.get(*p) != Some(*v))
                .map(|(p, _)| format!("Write: {p}")),
        );
        let mut eligible = Vec::new();
        for path in paths {
            let (_, p) = path.split_once(": ").unwrap();
            if self.restorable(p)? {
                eligible.push(path);
            }
        }
        let mut paths = eligible;
        let extra = paths.len().saturating_sub(15);
        paths.truncate(15);
        if extra > 0 {
            paths.push(format!("+{extra} more"));
        }
        Ok(format!(
            "This deletes files that weren't there then and puts back the ones that changed:\n{}",
            paths.join("\n")
        ))
    }
    pub fn restore(&self, tree: &str) -> Result<usize> {
        Ok(self.restore_saved(tree)?.0)
    }
    pub fn restore_saved(&self, tree: &str) -> Result<(usize, String)> {
        let current = self.snapshot()?;
        // The snapshot deadline bounds capture cost, not the number of files
        // a user explicitly asks us to restore.
        let worker = Self::in_dir(
            self.repo.parent().unwrap(),
            &self.root,
            &HistoryConfig {
                timeout_ms: 120000,
                ..self.config.clone()
            },
        )?;
        let count = worker.restore_tree(tree, &current)?;
        Ok((count, current))
    }
    fn restore_tree(&self, tree: &str, current: &str) -> Result<usize> {
        let old = self.entries(tree)?;
        let now = self.entries(current)?;
        let mut plan = Vec::new();
        // Skip paths now ignored, oversized, or under a symlinked directory.
        for p in old
            .keys()
            .chain(now.keys())
            .collect::<std::collections::BTreeSet<_>>()
        {
            if old.get(p) == now.get(p) {
                continue;
            }
            if !self.restorable(p)? {
                continue;
            }
            let path = self.root.join(p);
            let content = old
                .get(p)
                .map(|(mode, blob)| {
                    self.git(&["cat-file", "blob", blob])
                        .map(|bytes| (mode.clone(), bytes))
                })
                .transpose()?;
            plan.push((path, content));
        }
        for (path, _) in &plan {
            if std::fs::symlink_metadata(path).is_ok_and(|m| !m.is_dir()) {
                std::fs::remove_file(path)?;
            }
        }
        for (path, content) in &plan {
            if let Some((mode, bytes)) = content {
                if path.is_dir() {
                    std::fs::remove_dir(path)?;
                }
                // Empty directories left by removed paths may block file restoration.
                std::fs::create_dir_all(path.parent().unwrap())?;
                #[cfg(unix)]
                if mode == "120000" {
                    use std::os::unix::ffi::OsStrExt;
                    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(bytes), path)?;
                } else {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::write(path, bytes)?;
                    std::fs::set_permissions(
                        path,
                        std::fs::Permissions::from_mode(if mode == "100755" {
                            0o755
                        } else {
                            0o644
                        }),
                    )?;
                }
                #[cfg(not(unix))]
                {
                    // No executable bit to restore; symlinks come back as plain files.
                    let _ = mode;
                    std::fs::write(path, bytes)?;
                }
            }
        }
        Ok(plan.len())
    }
}

/// Where the shadow repository of the project at `root` (canonical) lives.
fn repo_dir(base: &Path, root: &Path) -> PathBuf {
    let mut hash = 0xcbf29ce484222325u64;
    for b in root.as_os_str().as_encoded_bytes() {
        hash = (hash ^ *b as u64).wrapping_mul(0x100000001b3);
    }
    base.join(format!("{hash:016x}"))
}

/// Shadow repositories this process created: every checkpoint in one belongs
/// to a session still running here, so there is nothing to prune in it yet.
static CREATED: std::sync::Mutex<std::collections::BTreeSet<PathBuf>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// Prune unreachable checkpoint refs after saving, at most once a day per project.
pub fn maintain(root: &Path, config: &HistoryConfig) -> Result<()> {
    // No snapshot taken in this project yet (its turns only read): nothing
    // to prune, and no reason to create the repository just to look.
    let base = crate::config::home_dir().join("history");
    if !repo_dir(&base, &root.canonical()?).join("HEAD").exists() {
        return Ok(());
    }
    let shadow = Shadow::new(root, config)?;
    let marker = shadow.repo.join("maintenance");
    if std::fs::metadata(&marker)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|d| d < Duration::from_secs(86400))
    {
        return Ok(());
    }
    let cutoff =
        chrono::Utc::now() - chrono::Duration::days(config.retention_days.min(36500) as i64);
    if !CREATED.lock().unwrap().contains(&shadow.repo) {
        let live: std::collections::HashSet<_> = crate::session::Session::list(root)
            .into_iter()
            .filter(|s| chrono::DateTime::parse_from_rfc3339(&s.updated).is_ok_and(|t| t > cutoff))
            .flat_map(|s| s.checkpoints.into_iter().filter_map(|c| c.tree))
            .collect();
        let refs = String::from_utf8(shadow.git(&[
            "for-each-ref",
            "--format=%(refname)",
            "refs/checkpoints/",
        ])?)?;
        for reference in refs.lines() {
            if !live.contains(reference.trim_start_matches("refs/checkpoints/")) {
                shadow.git(&["update-ref", "-d", reference])?;
            }
        }
        shadow.git(&["gc", "--auto"])?;
    }
    std::fs::write(&marker, b"")?;
    std::fs::write(
        shadow.repo.join("project"),
        root.as_os_str().as_encoded_bytes(),
    )?;
    // Repositories belonging only to deleted/expired sessions are removed.
    for entry in std::fs::read_dir(shadow.repo.parent().unwrap())?.flatten() {
        if entry.path() == shadow.repo {
            continue;
        }
        let Ok(project) = std::fs::read_to_string(entry.path().join("project")) else {
            continue;
        };
        if crate::session::Session::list(Path::new(&project))
            .iter()
            .any(|s| chrono::DateTime::parse_from_rfc3339(&s.updated).is_ok_and(|t| t > cutoff))
        {
            continue;
        }
        // Avoid racing an active, not-yet-saved turn in another project.
        if std::fs::metadata(entry.path().join("maintenance"))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|d| d > Duration::from_secs(86400))
        {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fresh_snapshot(shadow: &Shadow) -> String {
        shadow.git(&["read-tree", "--empty"]).unwrap();
        let paths = shadow.paths().unwrap();
        let list = shadow.repo.join("benchmark-paths");
        std::fs::write(
            &list,
            paths
                .iter()
                .flat_map(|p| p.bytes().chain(std::iter::once(0)))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        if !paths.is_empty() {
            shadow
                .git(&[
                    "add",
                    "-A",
                    "--pathspec-from-file",
                    list.to_str().unwrap(),
                    "--pathspec-file-nul",
                ])
                .unwrap();
        }
        std::fs::remove_file(list).unwrap();
        let tree = String::from_utf8(shadow.git(&["write-tree"]).unwrap())
            .unwrap()
            .trim()
            .to_owned();
        shadow
            .git(&["update-ref", &format!("refs/checkpoints/{tree}"), &tree])
            .unwrap();
        tree
    }

    #[test]
    fn reused_index_matches_fresh_trees() {
        // An explicit base, not CHANTO_HOME: other tests read the environment concurrently.
        let base =
            std::env::temp_dir().join(format!("chanto-history-home-{}", uuid::Uuid::new_v4()));
        let dir = std::env::temp_dir().join(format!("chanto-index-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = HistoryConfig {
            timeout_ms: 120000,
            ..Default::default()
        };
        for step in 0..8 {
            match step {
                0 => {
                    std::fs::write(dir.join("a"), "first").unwrap();
                }
                1 => {
                    std::fs::write(dir.join("a"), "modified").unwrap();
                    std::fs::write(dir.join("b"), "new").unwrap();
                    std::fs::write(dir.join("large"), "small").unwrap();
                }
                2 => {
                    std::fs::rename(dir.join("b"), dir.join("renamed")).unwrap();
                }
                3 => {
                    std::fs::remove_file(dir.join("a")).unwrap();
                }
                4 => {
                    std::fs::write(dir.join(".gitignore"), "renamed\n").unwrap();
                }
                5 => {
                    std::fs::write(dir.join("renamed"), vec![0; 6 * 1024 * 1024]).unwrap();
                    std::fs::write(dir.join("large"), vec![0; 6 * 1024 * 1024]).unwrap();
                }
                6 => {
                    std::fs::remove_file(dir.join(".gitignore")).unwrap();
                    std::fs::write(dir.join("renamed"), "eligible again").unwrap();
                }
                _ => {
                    std::fs::write(dir.join(".gitignore"), "").unwrap();
                    std::fs::remove_file(dir.join(".gitignore")).unwrap();
                    std::fs::remove_file(dir.join("renamed")).unwrap();
                    std::fs::remove_file(dir.join("large")).unwrap();
                }
            }
            let shadow = Shadow::in_dir(&base, &dir, &config).unwrap();
            let reused = shadow.snapshot().unwrap();
            assert_eq!(
                reused,
                fresh_snapshot(&Shadow::in_dir(&base, &dir, &config).unwrap()),
                "step {step}"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn restore_preview_lists_deletions_first_and_limits_paths() {
        // An explicit base, not CHANTO_HOME: other tests read the environment concurrently.
        let base =
            std::env::temp_dir().join(format!("chanto-history-home-{}", uuid::Uuid::new_v4()));
        let dir = std::env::temp_dir().join(format!("chanto-preview-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("changed"), "before").unwrap();
        let config = HistoryConfig {
            timeout_ms: 120000,
            ..Default::default()
        };
        let tree = Shadow::in_dir(&base, &dir, &config)
            .unwrap()
            .snapshot()
            .unwrap();
        std::fs::write(dir.join("changed"), "after").unwrap();
        for i in 0..16 {
            std::fs::write(dir.join(format!("editor-{i:02}")), "user").unwrap();
        }
        let shadow = Shadow::in_dir(&base, &dir, &config).unwrap();
        let summary = shadow.restore_summary(&tree).unwrap();
        assert_eq!(
            summary.lines().filter(|p| p.starts_with("Delete:")).count(),
            15
        );
        assert!(summary.ends_with("+2 more"));
        std::fs::remove_file(dir.join("editor-15")).unwrap();
        std::fs::remove_file(dir.join("editor-14")).unwrap();
        let summary = Shadow::in_dir(&base, &dir, &config)
            .unwrap()
            .restore_summary(&tree)
            .unwrap();
        assert!(summary.ends_with("Write: changed"));
        assert!(summary.find("Delete:").unwrap() < summary.find("Write:").unwrap());
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_git_that_cannot_start_leaves_no_files_behind() {
        // Each failed start (no git, or the project gone) left an output and a
        // stderr file in the shadow repository.
        let base = std::env::temp_dir().join(format!("chanto-nogit-{}", uuid::Uuid::new_v4()));
        let dir = std::env::temp_dir().join(format!("chanto-nogit-proj-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = HistoryConfig {
            timeout_ms: 120000,
            ..Default::default()
        };
        let shadow = Shadow::in_dir(&base, &dir, &config).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let e = shadow.snapshot().unwrap_err().to_string();
        assert!(!e.contains("git isn't installed"), "{e}");
        let leftovers: Vec<_> = std::fs::read_dir(&shadow.repo)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("output-") || n.starts_with("stderr-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn first_snapshot_is_packed_and_restores_exactly() {
        // The first snapshot streams every file into one pack (fast on big
        // projects); it must record the same tree `git add` would, and
        // rewinding to it must bring back every byte and mode.
        let base = std::env::temp_dir().join(format!("chanto-first-{}", uuid::Uuid::new_v4()));
        let dir = std::env::temp_dir().join(format!("chanto-first-proj-{}", uuid::Uuid::new_v4()));
        for d in 0..10 {
            std::fs::create_dir_all(dir.join(format!("src/m{d}"))).unwrap();
            for f in 0..30 {
                std::fs::write(
                    dir.join(format!("src/m{d}/f{f}.rs")),
                    format!("fn f{f}() {{ /* {d} */ }}\n"),
                )
                .unwrap();
            }
        }
        std::fs::write(dir.join("empty"), "").unwrap();
        std::fs::write(
            dir.join("bin.dat"),
            (0..=255u8).cycle().take(70_000).collect::<Vec<u8>>(),
        )
        .unwrap();
        std::fs::write(dir.join("name with spaces & $(x).txt"), "odd\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(dir.join("run.sh"), "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            std::os::unix::fs::symlink("empty", dir.join("link")).unwrap();
        }
        let config = HistoryConfig {
            timeout_ms: 120000,
            ..Default::default()
        };
        let shadow = Shadow::in_dir(&base, &dir, &config).unwrap();
        assert!(shadow.first);
        let tree = shadow.snapshot().unwrap();
        let packs = std::fs::read_dir(shadow.repo.join("objects/pack"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".pack"))
            .count();
        assert!(packs >= 1, "the first snapshot is written as a pack");
        let leftovers: Vec<_> = std::fs::read_dir(&shadow.repo)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("input-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let later = Shadow::in_dir(&base, &dir, &config).unwrap();
        assert!(!later.first);
        assert_eq!(
            later.snapshot().unwrap(),
            tree,
            "an unchanged project gives the same tree"
        );
        assert_eq!(
            fresh_snapshot(&later),
            tree,
            "same tree as a plain `git add`"
        );
        // Change, add and delete files, then rewind.
        let originals: Vec<(PathBuf, Vec<u8>)> = [
            "src/m3/f7.rs",
            "bin.dat",
            "empty",
            "name with spaces & $(x).txt",
        ]
        .iter()
        .map(|p| (dir.join(p), std::fs::read(dir.join(p)).unwrap()))
        .collect();
        std::fs::write(dir.join("src/m3/f7.rs"), "changed").unwrap();
        std::fs::write(dir.join("bin.dat"), "short").unwrap();
        std::fs::remove_file(dir.join("name with spaces & $(x).txt")).unwrap();
        std::fs::write(dir.join("src/new.rs"), "new").unwrap();
        Shadow::in_dir(&base, &dir, &config)
            .unwrap()
            .restore(&tree)
            .unwrap();
        for (p, bytes) in &originals {
            assert_eq!(&std::fs::read(p).unwrap(), bytes, "{}", p.display());
        }
        assert!(!dir.join("src/new.rs").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.join("run.sh"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
            assert_eq!(
                std::fs::read_link(dir.join("link")).unwrap(),
                Path::new("empty")
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn sessions_starting_together_share_one_shadow_repository() {
        let base = std::env::temp_dir().join(format!("chanto-init-race-{}", uuid::Uuid::new_v4()));
        let dir =
            std::env::temp_dir().join(format!("chanto-init-race-proj-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        let config = HistoryConfig {
            timeout_ms: 120000,
            ..Default::default()
        };
        let opened: Vec<Result<Shadow>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| s.spawn(|| Shadow::in_dir(&base, &dir, &config)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for o in opened {
            o.unwrap().snapshot().unwrap();
        }
        // A listed file that's gone by the time it's added (a concurrent
        // session's temp file) is skipped, not a failed snapshot.
        let shadow = Shadow::in_dir(&base, &dir, &config).unwrap();
        let mut paths = vec!["a.txt".to_string(), ".a.txt.123.tmp".to_string()];
        shadow.add_paths(&mut paths).unwrap();
        assert_eq!(paths, vec!["a.txt".to_string()]);
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_project_holding_chanto_home_leaves_it_out() {
        // Started in ~, the snapshot took in ~/.chanto (sessions, and the
        // shadow repository itself) and failed on its vanishing temp files.
        let dir = std::env::temp_dir().join(format!("chanto-home-proj-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join(".hh/sessions")).unwrap();
        let dir = dir.canonical().unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join(".hh/sessions/s.json"), "{}").unwrap();
        let config = HistoryConfig {
            timeout_ms: 120000,
            ..Default::default()
        };
        let shadow = Shadow::in_dir(&dir.join(".hh/history"), &dir, &config).unwrap();
        shadow.snapshot().unwrap();
        let listed = String::from_utf8(shadow.git(&["ls-files"]).unwrap()).unwrap();
        assert_eq!(listed.trim(), "a.txt");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[ignore = "first-snapshot measurement; set CHANTO_BENCH_DIR"]
    fn first_snapshot_benchmark() {
        let Ok(root) = std::env::var("CHANTO_BENCH_DIR") else {
            return;
        };
        let base = std::env::temp_dir().join(format!("chanto-bench-home-{}", uuid::Uuid::new_v4()));
        let config = HistoryConfig {
            timeout_ms: 600_000,
            ..Default::default()
        };
        let t = Instant::now();
        let s = Shadow::in_dir(&base, Path::new(&root), &config).unwrap();
        let open = t.elapsed();
        let t = Instant::now();
        let mut paths = s.paths().unwrap();
        let list = t.elapsed();
        let t = Instant::now();
        s.add_paths(&mut paths).unwrap();
        let add = t.elapsed();
        let t = Instant::now();
        let s = Shadow::in_dir(&base, Path::new(&root), &config).unwrap();
        s.snapshot().unwrap();
        let second = t.elapsed();
        eprintln!(
            "files {} open {open:?} list {list:?} add {add:?}; next snapshot {second:?}",
            paths.len()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    #[ignore = "snapshot performance measurement"]
    fn snapshot_benchmark() {
        let _guard = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let synthetic = std::env::temp_dir().join(format!("chanto-bench-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&synthetic).unwrap();
        for i in 0..20000 {
            std::fs::write(synthetic.join(format!("file-{i}")), vec![b'x'; 4096]).unwrap();
        }
        let config = HistoryConfig {
            timeout_ms: 120000,
            ..Default::default()
        };
        for root in [std::env::current_dir().unwrap(), synthetic.clone()] {
            let mut before = Vec::new();
            let mut after = Vec::new();
            Shadow::new(&root, &config).unwrap().snapshot().unwrap();
            for _ in 0..7 {
                let s = Shadow::new(&root, &config).unwrap();
                let start = Instant::now();
                fresh_snapshot(&s);
                before.push(start.elapsed().as_secs_f64() * 1000.0);
                let start = Instant::now();
                Shadow::new(&root, &config).unwrap().snapshot().unwrap();
                after.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            before.sort_by(f64::total_cmp);
            after.sort_by(f64::total_cmp);
            eprintln!(
                "{}: empty-index {:.1} ms; reused-index {:.1} ms (median of 7, warm)",
                root.display(),
                before[3],
                after[3]
            );
        }
        let s = Shadow::new(&synthetic, &config).unwrap();
        std::fs::remove_dir_all(&s.repo).unwrap();
        std::fs::remove_dir_all(synthetic).unwrap();
    }

    #[test]
    fn shell_changes_and_git_isolation() {
        // An explicit base, not CHANTO_HOME: other tests read the environment concurrently.
        let base =
            std::env::temp_dir().join(format!("chanto-shadow-home-{}", uuid::Uuid::new_v4()));
        let dir = std::env::temp_dir().join(format!("chanto-shadow-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            out.stdout
        };
        git(&["init", "-q"]);
        // Git for Windows defaults to autocrlf, which would rewrite the files on stash.
        git(&["config", "core.autocrlf", "false"]);
        std::fs::write(dir.join(".gitignore"), "ignored\n").unwrap();
        for name in ["edit", "shell", "deleted"] {
            std::fs::write(dir.join(name), "before\n").unwrap();
        }
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "initial",
        ]);
        std::fs::write(dir.join("edit"), "stashed content").unwrap();
        git(&["stash", "push", "-qm", "saved change"]);
        let head = git(&["rev-parse", "HEAD"]);
        let index = std::fs::read(dir.join(".git/index")).unwrap();
        let refs = git(&["show-ref"]);
        let stash = git(&["stash", "list"]);
        std::fs::write(dir.join("ignored"), "ignored before").unwrap();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("node_modules/cache"), "before").unwrap();
        std::fs::write(dir.join("large"), vec![0; 6 * 1024 * 1024]).unwrap();
        let config = HistoryConfig {
            timeout_ms: 10000,
            ..Default::default()
        };
        let shadow = Shadow::in_dir(&base, &dir, &config).unwrap();
        let tree = shadow.snapshot().unwrap();
        std::fs::write(dir.join("edit"), "edited").unwrap();
        // What `sed -i` and friends do: write a replacement and rename it into place.
        std::fs::write(dir.join("shell.tmp"), "after\n").unwrap();
        std::fs::rename(dir.join("shell.tmp"), dir.join("shell")).unwrap();
        std::fs::write(dir.join("new"), "created\n").unwrap();
        std::fs::remove_file(dir.join("deleted")).unwrap();
        std::fs::write(dir.join("ignored"), "ignored after").unwrap();
        std::fs::write(dir.join("node_modules/cache"), "after").unwrap();
        std::fs::write(dir.join("large"), vec![1; 6 * 1024 * 1024]).unwrap();
        let shadow = Shadow::in_dir(&base, &dir, &config).unwrap();
        assert_eq!(shadow.changes(&tree).unwrap(), (2, 1, 1));
        assert_eq!(shadow.restore(&tree).unwrap(), 4);
        for name in ["edit", "shell", "deleted"] {
            assert_eq!(std::fs::read_to_string(dir.join(name)).unwrap(), "before\n");
        }
        assert!(!dir.join("new").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("ignored")).unwrap(),
            "ignored after"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("node_modules/cache")).unwrap(),
            "after"
        );
        assert_eq!(std::fs::read(dir.join("large")).unwrap()[0], 1);
        assert_eq!(head, git(&["rev-parse", "HEAD"]));
        assert_eq!(index, std::fs::read(dir.join(".git/index")).unwrap());
        assert_eq!(refs, git(&["show-ref"]));
        assert_eq!(stash, git(&["stash", "list"]));
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    #[ignore = "manual snapshot overhead measurement on the current project"]
    fn snapshot_overhead() {
        let _guard = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var_os("CHANTO_HOME");
        let home = std::env::temp_dir().join(format!("chanto-measure-{}", uuid::Uuid::new_v4()));
        unsafe {
            std::env::set_var("CHANTO_HOME", &home);
        }
        let root = std::env::current_dir().unwrap();
        for i in 0..6 {
            let start = Instant::now();
            let shadow = Shadow::new(&root, &HistoryConfig::default()).unwrap();
            let tree = shadow.snapshot().unwrap();
            println!(
                "snapshot {i}: {:.1} ms ({tree})",
                start.elapsed().as_secs_f64() * 1000.0
            );
        }
        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("CHANTO_HOME", previous);
            } else {
                std::env::remove_var("CHANTO_HOME");
            }
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_projects_first_snapshot_gets_a_longer_allowance() {
        let base =
            std::env::temp_dir().join(format!("chanto-shadow-home-{}", uuid::Uuid::new_v4()));
        let dir = std::env::temp_dir().join(format!("chanto-shadow-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..50 {
            std::fs::write(dir.join(format!("f{i}.txt")), "x".repeat(10_000)).unwrap();
        }
        // A budget no snapshot could meet: the first one still completes,
        // since it can't get faster until it has run once.
        let tight = HistoryConfig {
            timeout_ms: 0,
            ..Default::default()
        };
        assert!(
            Shadow::in_dir(&base, &dir, &tight)
                .unwrap()
                .snapshot()
                .is_ok()
        );
        // Later snapshots are held to the configured budget again.
        std::fs::write(dir.join("f0.txt"), "changed").unwrap();
        assert!(
            Shadow::in_dir(&base, &dir, &tight)
                .unwrap()
                .snapshot()
                .unwrap_err()
                .to_string()
                .contains("time budget")
        );
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lock_left_by_a_killed_git_does_not_break_later_snapshots() {
        let base =
            std::env::temp_dir().join(format!("chanto-shadow-home-{}", uuid::Uuid::new_v4()));
        let dir = std::env::temp_dir().join(format!("chanto-shadow-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "one").unwrap();
        let config = HistoryConfig {
            timeout_ms: 10000,
            ..Default::default()
        };
        let shadow = Shadow::in_dir(&base, &dir, &config).unwrap();
        assert!(shadow.snapshot().is_ok());
        // A fresh lock may belong to a snapshot running in another session: git's reason is reported.
        let lock = shadow.repo.join("index.lock");
        std::fs::write(&lock, "").unwrap();
        std::fs::write(dir.join("a.txt"), "two").unwrap();
        let err = Shadow::in_dir(&base, &dir, &config)
            .unwrap()
            .snapshot()
            .unwrap_err()
            .to_string();
        assert!(err.contains("index.lock"), "{err}");
        // An old one has no live owner and is cleared.
        let old = std::time::SystemTime::now() - Duration::from_secs(600);
        std::fs::File::options()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(
            Shadow::in_dir(&base, &dir, &config)
                .unwrap()
                .snapshot()
                .is_ok()
        );
        assert!(!lock.exists());
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn budgets_and_empty_project() {
        // An explicit base, not CHANTO_HOME: other tests read the environment concurrently.
        let base =
            std::env::temp_dir().join(format!("chanto-shadow-home-{}", uuid::Uuid::new_v4()));
        let dir = std::env::temp_dir().join(format!("chanto-shadow-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = HistoryConfig {
            timeout_ms: 10000,
            ..Default::default()
        };
        let shadow = Shadow::in_dir(&base, &dir, &config).unwrap();
        assert!(shadow.snapshot().is_ok());
        std::fs::write(dir.join("file"), "content").unwrap();
        let limited = Shadow::in_dir(
            &base,
            &dir,
            &HistoryConfig {
                max_snapshot_bytes: 1,
                ..config.clone()
            },
        )
        .unwrap();
        assert!(
            limited
                .snapshot()
                .unwrap_err()
                .to_string()
                .contains("size budget")
        );
        let limited = Shadow::in_dir(
            &base,
            &dir,
            &HistoryConfig {
                timeout_ms: 0,
                ..config
            },
        )
        .unwrap();
        assert!(limited.snapshot().is_err());
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn maintenance_leaves_a_project_without_snapshots_alone() {
        let _guard = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var_os("CHANTO_HOME");
        let home =
            std::env::temp_dir().join(format!("chanto-maint-{}", uuid::Uuid::new_v4().simple()));
        let dir = home.join("project");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("CHANTO_HOME", &home) };
        maintain(&dir, &HistoryConfig::default()).unwrap();
        let repo = repo_dir(&home.join("history"), &dir.canonical().unwrap());
        assert!(!repo.exists(), "maintenance created a repository");
        // Once a snapshot made one, maintenance runs as before.
        Shadow::new(&dir, &HistoryConfig::default()).unwrap();
        maintain(&dir, &HistoryConfig::default()).unwrap();
        assert!(repo.join("maintenance").exists());
        unsafe {
            match previous {
                Some(p) => std::env::set_var("CHANTO_HOME", p),
                None => std::env::remove_var("CHANTO_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&home);
    }
}
