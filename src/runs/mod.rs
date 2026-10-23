//! Background runs: a task the local daemon (`chanto serve`) works on by
//! itself, in its own git worktree and branch, while you keep working.
//!
//! - `Run` is the persisted record (`~/.chanto/runs/<id>/run.json`), with
//!   the run's event log next to it (`events.jsonl`).
//! - `manager` hosts runs in the daemon: queueing, one worker process per
//!   run, needs-input prompts, stop/resume, and restart recovery.
//! - `worker` is the agent side (`chanto __run-worker <id>`), speaking JSON
//!   lines with the daemon over stdin/stdout.
//! - `api` is the HTTP surface (`/runs...`), `cli` the commands that talk to it.
//! - `git` prepares worktrees (with a snapshot of uncommitted changes) and
//!   applies finished runs.
//! - `pull` takes runs from a remote queue (`chanto runner connect`) and
//!   hosts them in this process.
//! - `cloud` queues runs on Chanto cloud from the terminal (`chanto run
//!   --cloud`, `Chanto cloud login`) and follows them.
//!
//! A run's `source` says where its code came from. Only `local` (a checkout on
//! this machine) exists today; other kinds (a git URL, an uploaded bundle) and
//! other `target`s plug in there without changing the rest of the model.

pub mod api;
pub mod cli;
pub mod cloud;
pub mod container;
pub mod git;
pub mod manager;
pub mod notify;
pub mod pull;
#[cfg(test)]
pub(crate) mod tests;
pub mod worker;

use crate::permissions::Mode;
use crate::util::Canonical as _;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;

/// `[background]` in config.toml.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BackgroundConfig {
    /// Permission mode for runs (default acceptEdits).
    pub mode: Option<Mode>,
    /// In acceptEdits, run shell commands that stay inside the OS sandbox
    /// without asking (default true).
    pub allow_sandboxed_commands: Option<bool>,
    /// Shell commands run in the OS sandbox (default true).
    pub sandbox: Option<bool>,
    /// Verified mode (default true; `--no-verify` per run).
    pub verify: Option<bool>,
    /// Per-run budget in USD (default 2).
    pub max_cost_usd: Option<f64>,
    /// Wall-clock limit in minutes, not counting time waiting for input (default 60).
    pub max_minutes: Option<u64>,
    /// Permission prompts nobody answers are denied after this many minutes (default 1440).
    pub input_timeout_minutes: Option<u64>,
    /// Runs at once; the rest wait as `queued` (default 2).
    pub max_parallel: Option<usize>,
    /// Model for runs (default: the configured model).
    pub model: Option<String>,
    /// Run shell commands in a per-run container: "none" (default), "docker"
    /// or "podman" (see `container`).
    pub isolate: Option<String>,
    /// Container image (default: by project type, e.g. rust:1-bookworm).
    pub image: Option<String>,
    /// Container networking: "none" (default), "allowlist" ([sandbox]
    /// allowed_domains through Chanto's filtering proxy) or "full".
    pub network: Option<String>,
    /// Container CPU limit (e.g. 2 or 1.5).
    pub cpus: Option<f64>,
    /// Container memory limit (e.g. "4g").
    pub memory: Option<String>,
    /// Largest git bundle a client may upload, in MB (default 1024).
    pub max_bundle_mb: Option<u64>,
}

/// `[remote.<name>]`: how a run server gets the code.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RemoteConfig {
    /// A git remote (e.g. "origin") the server can fetch: the current branch is
    /// pushed there and the server fetches it. Without one (or with
    /// uncommitted changes), the code goes up as a git bundle.
    pub git_remote: Option<String>,
}

impl BackgroundConfig {
    pub fn merge(&mut self, o: BackgroundConfig) {
        macro_rules! take {
            ($($f:ident),*) => { $(if o.$f.is_some() { self.$f = o.$f; })* };
        }
        take!(
            mode,
            allow_sandboxed_commands,
            sandbox,
            verify,
            max_cost_usd,
            max_minutes,
            input_timeout_minutes,
            max_parallel,
            model,
            isolate,
            image,
            network,
            cpus,
            memory,
            max_bundle_mb
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunState {
    Queued,
    Running,
    /// Paused on a permission prompt or question.
    NeedsInput,
    Done,
    Failed,
    Cancelled,
    /// The daemon stopped while the run was active; resumable.
    Interrupted,
}

impl RunState {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            RunState::Queued | RunState::Running | RunState::NeedsInput
        )
    }
    pub fn label(self) -> &'static str {
        match self {
            RunState::Queued => "queued",
            RunState::Running => "running",
            RunState::NeedsInput => "needs-input",
            RunState::Done => "done",
            RunState::Failed => "failed",
            RunState::Cancelled => "cancelled",
            RunState::Interrupted => "interrupted",
        }
    }
}

/// Where a run's code comes from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    /// "local": a checkout on the daemon's machine; "git": fetched from a git
    /// URL; "bundle": uploaded by the client as a git bundle.
    pub kind: String,
    /// The repository's top-level directory (for git/bundle sources, the
    /// server's own bare repository).
    pub path: PathBuf,
    /// The URL a "git" source was fetched from.
    #[serde(default)]
    pub url: Option<String>,
    /// HEAD when the run started, and the branch it was on (if any).
    pub head: String,
    #[serde(default)]
    pub head_ref: Option<String>,
    /// Commit holding the uncommitted changes carried into the run (parent: `head`).
    #[serde(default)]
    pub snapshot: Option<String>,
}

/// Settings fixed when the run is created (request options over `[background]`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RunSettings {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub mode: String,
    pub sandbox: bool,
    pub allow_sandboxed_commands: bool,
    pub verify: bool,
    pub max_cost_usd: f64,
    pub max_minutes: u64,
    pub input_timeout_minutes: u64,
    /// Container isolation for the run's shell commands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isolate: Option<container::Isolation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerifyResult {
    /// passed | failed | skipped (verified mode on, but the gate didn't run) | off
    pub status: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub task: String,
    pub state: RunState,
    /// Where it runs: "local" (this daemon).
    #[serde(default = "local")]
    pub target: String,
    pub created: String,
    #[serde(default)]
    pub started: Option<String>,
    #[serde(default)]
    pub finished: Option<String>,
    pub source: Source,
    pub worktree: PathBuf,
    pub branch: String,
    /// The commit the run's own work starts from (the snapshot, else HEAD).
    pub base: String,
    pub settings: RunSettings,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub verify: Option<VerifyResult>,
    /// The final answer.
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// The open permission request or question (state needs-input).
    #[serde(default)]
    pub pending: Option<Value>,
    /// Start from the saved session when the run next leaves `queued`.
    #[serde(default)]
    pub resume: bool,
    /// "goal" for a goal run (`chanto goal --bg`); None for a plain task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The team this run is a sub-task of (`chanto team`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    /// The scheduled job (`chanto runner`) that started this run, and how it
    /// was set off: "schedule", "manual", "webhook" or "once".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    /// `/background`: the saved session whose conversation the run continues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_session: Option<PathBuf>,
}

fn local() -> String {
    "local".into()
}

impl Run {
    /// Seconds from start to finish (or now).
    pub fn duration_secs(&self) -> u64 {
        let parse = |s: &Option<String>| {
            s.as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        };
        let Some(start) = parse(&self.started) else {
            return 0;
        };
        let end = parse(&self.finished)
            .map(|d| d.with_timezone(&chrono::Local))
            .unwrap_or_else(chrono::Local::now);
        (end.signed_duration_since(start).num_seconds()).max(0) as u64
    }

    /// The API form: the record plus derived fields.
    pub fn to_json(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        v["duration_secs"] = json!(self.duration_secs());
        v["repo"] = json!(self.source.path);
        v["isolation"] = json!(self.isolation_label());
        v
    }

    /// How the run's commands are isolated, in one line.
    pub fn isolation_label(&self) -> String {
        match &self.settings.isolate {
            Some(i) => format!(
                "container ({}; {})",
                i.label(),
                container::container_name(&self.id)
            ),
            None if self.settings.sandbox => "worktree + OS sandbox".into(),
            None => "worktree only (sandbox off)".into(),
        }
    }
}

pub fn now() -> String {
    chrono::Local::now().to_rfc3339()
}

/// Run records live under `<home>/runs/<id>/`.
#[derive(Clone)]
pub struct Store {
    pub dir: PathBuf,
}

impl Store {
    pub fn default_dir() -> PathBuf {
        crate::config::home_dir().join("runs")
    }

    pub fn new(dir: PathBuf) -> Store {
        Store { dir }
    }

    pub fn run_dir(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    pub fn events_path(&self, id: &str) -> PathBuf {
        self.run_dir(id).join("events.jsonl")
    }

    pub fn save(&self, run: &Run) -> Result<()> {
        let dir = self.run_dir(&run.id);
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join("run.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(run)?)?;
        std::fs::rename(&tmp, dir.join("run.json"))?;
        Ok(())
    }

    pub fn load(&self, id: &str) -> Result<Run> {
        let path = self.run_dir(id).join("run.json");
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn list(&self) -> Vec<Run> {
        let mut out: Vec<Run> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| self.load(&e.file_name().to_string_lossy()).ok())
            .collect();
        out.sort_by(|a, b| b.created.cmp(&a.created));
        out
    }

    pub fn append_event(&self, id: &str, ev: &Value) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.events_path(id))
        {
            let _ = writeln!(f, "{ev}");
        }
    }

    pub fn events(&self, id: &str) -> Vec<Value> {
        std::fs::read_to_string(self.events_path(id))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    pub fn remove(&self, id: &str) {
        let _ = std::fs::remove_dir_all(self.run_dir(id));
    }
}

/// Where run worktrees live: `<home>/runs/worktrees/<project-key>/<id>`,
/// outside the user's project so test runners, build globs, indexers and
/// file watchers never see a second copy of it.
pub fn worktrees_dir() -> PathBuf {
    Store::default_dir().join("worktrees")
}

/// The run stores whose worktrees are run checkouts: this machine's own
/// runs, and the runs `chanto runner connect` takes from a server.
fn run_stores() -> [PathBuf; 2] {
    [Store::default_dir(), pull::store_dir()]
}

/// For a path inside a run's worktree: the worktree root and the path within it.
pub fn split_worktree_path(p: &std::path::Path) -> Option<(PathBuf, PathBuf)> {
    split_in_store(p).map(|(_, wt, inner)| (wt, inner))
}

/// The run store holding `p`'s worktree, that worktree, and the path within it.
fn split_in_store(p: &std::path::Path) -> Option<(PathBuf, PathBuf, PathBuf)> {
    run_stores().into_iter().find_map(|store| {
        let root = store.join("worktrees");
        let canon = root.canonical().ok();
        let rest = p
            .strip_prefix(&root)
            .ok()
            .or_else(|| canon.as_ref().and_then(|c| p.strip_prefix(c).ok()))?;
        let mut comps = rest.components();
        let (key, id) = (comps.next()?, comps.next()?);
        Some((
            store.clone(),
            root.join(key).join(id),
            comps.as_path().to_path_buf(),
        ))
    })
}

/// The source checkout of the run whose worktree contains `p`. Run worktrees
/// are trusted exactly when their source repository is.
pub fn source_repo_of(p: &std::path::Path) -> Option<PathBuf> {
    let (store, wt, _) = split_in_store(p)?;
    let id = wt.file_name()?.to_string_lossy().to_string();
    let run = Store::new(store).load(&id).ok()?;
    Some(run.source.path)
}

/// Events worth keeping in the log (streaming noise is live-only).
pub fn persisted(ev: &Value) -> bool {
    !matches!(
        ev["type"].as_str(),
        Some("reasoning" | "tool_progress" | "busy" | "usage")
    )
}

/// The last few assistant messages, rebuilt from the event log.
pub fn last_messages(events: &[Value], n: usize) -> Vec<String> {
    let mut msgs: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ev in events {
        match ev["type"].as_str() {
            Some("assistant_start") => cur.clear(),
            Some("text") => cur.push_str(ev["text"].as_str().unwrap_or("")),
            Some("assistant_end") => {
                if !cur.trim().is_empty() {
                    msgs.push(cur.trim().to_string());
                }
                cur.clear();
            }
            _ => {}
        }
    }
    if !cur.trim().is_empty() {
        msgs.push(cur.trim().to_string());
    }
    let skip = msgs.len().saturating_sub(n);
    msgs.split_off(skip)
}

/// A fresh short run id.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// Human duration: 45s, 3m12s, 1h05m.
pub fn fmt_duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}
