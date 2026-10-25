//! The daemon side of background runs: creates runs, queues them, supervises
//! one worker per active run, relays prompts and answers, and keeps the
//! records on disk so runs outlive terminals and daemon restarts.

use super::{Run, RunSettings, RunState, Source, Store, VerifyResult, git};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// How workers are started.
#[derive(Clone)]
pub enum Launcher {
    /// A child process of this executable (`chanto __run-worker <id>`).
    Process(PathBuf),
    /// A task in this process (tests: no binary to spawn).
    #[cfg(test)]
    InProcess,
}

struct WorkerConn {
    events: mpsc::UnboundedReceiver<Value>,
    input: mpsc::UnboundedSender<Value>,
    kill: CancellationToken,
}

impl Launcher {
    /// `env`: extra environment for the worker process only.
    fn launch(&self, run: &Run, store: &Store, env: &[(String, String)]) -> Result<WorkerConn> {
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<Value>();
        let kill = CancellationToken::new();
        match self {
            Launcher::Process(exe) => {
                let log = crate::util::append_private(&store.run_dir(&run.id).join("worker.log"))?;
                let mut child = tokio::process::Command::new(exe)
                    .arg("__run-worker")
                    .arg(&run.id)
                    .arg(&store.dir)
                    .current_dir(&run.worktree)
                    // The agent's shell commands have no business with the API token.
                    .env_remove("CHANTO_SERVER_TOKEN")
                    .envs(env.iter().map(|(k, v)| (k, v)))
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(log)
                    .kill_on_drop(true)
                    .spawn()?;
                let mut stdin = child.stdin.take().unwrap();
                let stdout = child.stdout.take().unwrap();
                let mut in_rx = in_rx;
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    while let Some(v) = in_rx.recv().await {
                        if stdin.write_all(format!("{v}\n").as_bytes()).await.is_err() {
                            break;
                        }
                        let _ = stdin.flush().await;
                    }
                    // Closing stdin tells the worker the daemon is going away.
                });
                let k = kill.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncBufReadExt;
                    let mut lines = tokio::io::BufReader::new(stdout).lines();
                    loop {
                        tokio::select! {
                            line = lines.next_line() => match line {
                                Ok(Some(l)) => {
                                    if let Ok(v) = serde_json::from_str::<Value>(&l) {
                                        let _ = ev_tx.send(v);
                                    }
                                }
                                _ => break,
                            },
                            _ = k.cancelled() => {
                                let _ = child.start_kill();
                                break;
                            }
                        }
                    }
                    let _ = child.wait().await;
                });
            }
            #[cfg(test)]
            Launcher::InProcess => {
                let _ = env;
                let run = run.clone();
                let k = kill.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        _ = super::worker::run(run, ev_tx, in_rx) => {}
                        _ = k.cancelled() => {}
                    }
                });
            }
        }
        Ok(WorkerConn {
            events: ev_rx,
            input: in_tx,
            kill,
        })
    }
}

struct Handle {
    run: Mutex<Run>,
    tx: broadcast::Sender<Value>,
    seq: AtomicU64,
    /// Input channel and kill switch of the live worker.
    worker: Mutex<Option<(mpsc::UnboundedSender<Value>, CancellationToken)>>,
    /// Signalled when the worker is gone.
    ended: tokio::sync::Notify,
    /// The last state (and request) a notification went out for.
    notified: Mutex<String>,
    /// Extra worker environment ([`NewRun::env`]); only in memory.
    env: Vec<(String, String)>,
}

/// What a client asks for in `POST /runs`.
#[derive(Debug, Default, Clone, serde::Deserialize)]
#[serde(default)]
pub struct NewRun {
    pub task: String,
    /// Local checkout to run from.
    pub cwd: Option<PathBuf>,
    /// Where the code comes from; only `{"kind": "local", "path": ...}` today.
    pub source: Option<Value>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub mode: Option<String>,
    pub verify: Option<bool>,
    pub sandbox: Option<bool>,
    pub max_cost_usd: Option<f64>,
    pub max_minutes: Option<u64>,
    /// Start from this commit of the local checkout instead of HEAD plus
    /// uncommitted changes (teams: the shared base, or the merged work so far).
    pub start: Option<String>,
    /// "goal": work toward the task as a goal (criteria, then iterations until
    /// they pass) instead of a single turn.
    pub kind: Option<String>,
    /// The team this run is a sub-task of.
    pub team: Option<String>,
    /// `/background`: start from this saved session's conversation (a file
    /// under Chanto's session store).
    pub from_session: Option<PathBuf>,
    /// Set by the scheduler (`chanto runner`); clients can't claim a job.
    #[serde(skip)]
    pub job: Option<String>,
    #[serde(skip)]
    pub trigger: Option<String>,
    /// Extra environment for the run's worker (a pulled run's org secrets and
    /// model gateway). Kept in memory only: never saved with the run, and
    /// not settable through the API.
    #[serde(skip)]
    pub env: Vec<(String, String)>,
    /// Extra environment for fetching a git source ([`git::auth_env`]); same rules.
    #[serde(skip)]
    pub git_env: Vec<(String, String)>,
}

/// The run's container isolation from `[background]`, checked up front so a
/// misconfigured or stopped engine fails the request, not the run later.
fn isolation(
    cfg: &crate::config::Config,
    dir: &std::path::Path,
) -> Result<Option<super::container::Isolation>> {
    let iso = super::container::Isolation::from_config(&cfg.background, dir)?;
    if let Some(i) = &iso {
        super::container::check_engine(&i.engine)?;
    }
    Ok(iso)
}

pub struct Manager {
    pub store: Store,
    launcher: Launcher,
    runs: Mutex<HashMap<String, Arc<Handle>>>,
    /// Cancelled when the daemon shuts down (ends event streams).
    pub shutdown: CancellationToken,
    max_parallel: usize,
    notifier: Mutex<Option<Arc<super::notify::Notifier>>>,
    /// Largest bundle upload accepted, in bytes.
    pub max_bundle_bytes: std::sync::atomic::AtomicUsize,
}

impl Manager {
    /// Load the saved runs. Runs that were active when the previous daemon
    /// went away are marked interrupted; queued ones start.
    pub fn open(store: Store, launcher: Launcher, max_parallel: usize) -> Arc<Manager> {
        let m = Arc::new(Manager {
            store,
            launcher,
            runs: Default::default(),
            shutdown: CancellationToken::new(),
            max_parallel: max_parallel.max(1),
            notifier: Default::default(),
            max_bundle_bytes: (1024usize << 20).into(),
        });
        let mut engines = std::collections::BTreeSet::new();
        for mut run in m.store.list() {
            if let Some(i) = &run.settings.isolate {
                engines.insert(i.engine.clone());
            }
            if matches!(run.state, RunState::Running | RunState::NeedsInput) {
                run.state = RunState::Interrupted;
                run.pending = None;
                run.error = Some("the chanto daemon stopped while the run was active; `chanto runs resume` continues it".into());
                let _ = m.store.save(&run);
            }
            let seq = m
                .store
                .events(&run.id)
                .last()
                .and_then(|e| e["seq"].as_u64())
                .unwrap_or(0);
            let (tx, _) = broadcast::channel(1024);
            let h = Arc::new(Handle {
                run: Mutex::new(run.clone()),
                tx,
                seq: AtomicU64::new(seq),
                worker: Mutex::new(None),
                ended: Default::default(),
                notified: Default::default(),
                env: vec![],
            });
            m.runs.lock().unwrap().insert(run.id.clone(), h);
        }
        // Containers left by a previous daemon (crash, kill): no run is
        // active yet, so every one labeled for this home is an orphan.
        if !engines.is_empty() {
            let dir = m.store.dir.clone();
            std::thread::spawn(move || {
                for e in engines {
                    super::container::sweep(&e, &dir);
                }
            });
        }
        m.schedule();
        m
    }

    /// Remove a run's container (and its relay and network), off the async runtime.
    async fn remove_container(run: &Run) {
        if let Some(i) = &run.settings.isolate {
            let (engine, id) = (i.engine.clone(), run.id.clone());
            let _ = tokio::task::spawn_blocking(move || super::container::remove_run(&engine, &id))
                .await;
        }
    }

    /// Send notifications when runs finish, fail or need input.
    pub fn set_notifier(&self, n: super::notify::Notifier) {
        *self.notifier.lock().unwrap() = Some(Arc::new(n));
    }

    fn notify(&self, h: &Handle, run: &Run) {
        let Some(n) = self.notifier.lock().unwrap().clone() else {
            return;
        };
        let key = format!(
            "{}|{}",
            run.state.label(),
            run.pending
                .as_ref()
                .map(|p| p["request_id"].to_string())
                .unwrap_or_default()
        );
        {
            let mut last = h.notified.lock().unwrap();
            if *last == key {
                return;
            }
            *last = key;
        }
        if let Some(note) = n.note(run) {
            tokio::spawn(async move { n.send(&note).await });
        }
    }

    fn handle(&self, id: &str) -> Result<Arc<Handle>> {
        let runs = self.runs.lock().unwrap();
        if let Some(h) = runs.get(id) {
            return Ok(h.clone());
        }
        let matches: Vec<_> = runs.iter().filter(|(k, _)| k.starts_with(id)).collect();
        match matches.as_slice() {
            [(_, h)] if !id.is_empty() => Ok((*h).clone()),
            [] | [_] => bail!("no run {id}"),
            _ => bail!("run id {id} is ambiguous"),
        }
    }

    pub fn get(&self, id: &str) -> Result<Run> {
        Ok(self.handle(id)?.run.lock().unwrap().clone())
    }

    pub fn list(&self) -> Vec<Run> {
        let mut v: Vec<Run> = self
            .runs
            .lock()
            .unwrap()
            .values()
            .map(|h| h.run.lock().unwrap().clone())
            .collect();
        v.sort_by(|a, b| b.created.cmp(&a.created));
        v
    }

    /// Details for `GET /runs/{id}`: the record, diff stat, commits, last messages.
    pub async fn detail(&self, id: &str) -> Result<Value> {
        let h = self.handle(id)?;
        let last_seq = h.seq.load(Ordering::SeqCst);
        let run = h.run.lock().unwrap().clone();
        let events = self.store.events(&run.id);
        let r = run.clone();
        let (stat, commits) = tokio::task::spawn_blocking(move || {
            (
                git::diff_stat(&r),
                git::commit_count(&r.source.path, &r.base, &r.branch),
            )
        })
        .await?;
        let mut v = run.to_json();
        v["diff_stat"] = json!(stat);
        v["commits"] = json!(commits);
        v["last_messages"] = json!(super::last_messages(&events, 3));
        v["last_seq"] = json!(last_seq);
        Ok(v)
    }

    pub fn subscribe(&self, id: &str) -> Result<(Run, broadcast::Receiver<Value>)> {
        let h = self.handle(id)?;
        let rx = h.tx.subscribe();
        let run = h.run.lock().unwrap().clone();
        Ok((run, rx))
    }

    /// Stamp, log and broadcast an event.
    fn publish(&self, h: &Handle, mut ev: Value) {
        // The worker masks what it sends; this catches what it couldn't
        // know, before the event is logged or reaches a subscriber.
        crate::secrets::redact_json(&mut ev);
        let id = h.run.lock().unwrap().id.clone();
        ev["seq"] = json!(h.seq.fetch_add(1, Ordering::SeqCst) + 1);
        ev["run_id"] = json!(id);
        if super::persisted(&ev) {
            self.store.append_event(&id, &ev);
        }
        let _ = h.tx.send(ev);
    }

    /// Save the record and tell subscribers about the new state.
    fn changed(&self, h: &Handle) {
        let run = h.run.lock().unwrap().clone();
        let _ = self.store.save(&run);
        self.publish(
            h,
            json!({"type": "run_state", "state": run.state, "run": run.to_json()}),
        );
        self.notify(h, &run);
    }

    pub async fn create(self: &Arc<Self>, req: NewRun) -> Result<Run> {
        let task = req.task.trim().to_string();
        if task.is_empty() {
            bail!("`task` is required");
        }
        if let Some(m) = &req.mode
            && crate::permissions::Mode::parse(m).is_none()
        {
            bail!("unknown mode {m}");
        }
        if let Some(k) = req.kind.as_deref().filter(|k| *k != "goal") {
            bail!("unknown run kind {k} (only \"goal\")");
        }
        let kind = req
            .source
            .as_ref()
            .map(|s| s["kind"].as_str().unwrap_or("").to_string())
            .unwrap_or_else(|| "local".into());
        if let Some(p) = &req.from_session {
            // Only this machine's own saved sessions, never an arbitrary file.
            let store = crate::util::canonical(crate::config::home_dir().join("sessions"))
                .unwrap_or_default();
            let ok = kind == "local"
                && p.extension().is_some_and(|e| e == "json")
                && crate::util::canonical(p).is_ok_and(|c| c.starts_with(&store));
            if !ok {
                bail!("`from_session` must be a saved session of a local run");
            }
        }
        if req.start.is_some() && kind != "local" {
            bail!("`start` only applies to runs of a local checkout");
        }
        // Before the worktree is made under it.
        self.store.ensure_dir()?;
        let id = loop {
            let id = super::new_id();
            if !self.store.run_dir(&id).exists() {
                break id;
            }
        };
        let (prep, url, cfg, isolate) = if kind == "local" {
            let cwd = req
                .source
                .as_ref()
                .and_then(|s| s["path"].as_str().map(PathBuf::from))
                .or(req.cwd.clone())
                .ok_or_else(|| anyhow::anyhow!("`cwd` (the checkout to run from) is required"))?;
            let cwd = crate::util::canonical(&cwd)
                .map_err(|_| anyhow::anyhow!("{} is not a directory", cwd.display()))?;
            let (id, worktrees, start) = (
                id.clone(),
                self.store.dir.join("worktrees"),
                req.start.clone(),
            );
            tokio::task::spawn_blocking(move || -> Result<_> {
                let repo = git::toplevel(&cwd)?;
                let (cfg, _) =
                    crate::config::Config::load(&repo, crate::config::is_trusted(&repo))?;
                let iso = isolation(&cfg, &repo)?;
                Ok((
                    git::prepare(&cwd, &id, &worktrees, start.as_deref())?,
                    None,
                    cfg,
                    iso,
                ))
            })
            .await??
        } else {
            // Code from another machine: a git URL or an uploaded bundle,
            // checked out under this server's data directory, never trusted.
            let (id, dir, src, auth) = (
                id.clone(),
                self.store.dir.clone(),
                req.source.clone().unwrap_or_default(),
                req.git_env.clone(),
            );
            tokio::task::spawn_blocking(move || -> Result<_> {
                let (cfg, _) = crate::config::Config::load(&dir, false)?;
                isolation(&cfg, &dir)?;
                let (prep, url) = git::prepare_remote(&dir, &id, &src, &auth)?;
                // Pick the image from the code that arrived.
                let iso =
                    super::container::Isolation::from_config(&cfg.background, &prep.worktree)?;
                Ok((prep, url, cfg, iso))
            })
            .await??
        };
        let b = &cfg.background;
        let settings = RunSettings {
            model: req.model.or(b.model.clone()),
            provider: req.provider,
            base_url: req.base_url,
            mode: req.mode.unwrap_or_else(|| {
                serde_json::to_value(b.mode.unwrap_or(crate::permissions::Mode::AcceptEdits))
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or("acceptEdits".into())
            }),
            sandbox: req.sandbox.or(b.sandbox).unwrap_or(true),
            allow_sandboxed_commands: b.allow_sandboxed_commands.unwrap_or(true),
            verify: req.verify.or(b.verify).unwrap_or(true),
            max_cost_usd: req.max_cost_usd.or(b.max_cost_usd).unwrap_or(2.0),
            max_minutes: req.max_minutes.or(b.max_minutes).unwrap_or(60),
            input_timeout_minutes: b.input_timeout_minutes.unwrap_or(1440),
            isolate,
        };
        let run = Run {
            id: id.clone(),
            task,
            state: RunState::Queued,
            target: "local".into(),
            created: super::now(),
            started: None,
            finished: None,
            source: Source {
                kind,
                path: prep.repo,
                url,
                head: prep.head,
                head_ref: prep.head_ref,
                snapshot: prep.snapshot,
            },
            worktree: prep.worktree,
            branch: prep.branch,
            base: prep.base,
            settings,
            session_id: None,
            model: None,
            cost_usd: 0.0,
            verify: None,
            result: None,
            error: None,
            pending: None,
            resume: false,
            kind: req.kind,
            team: req.team,
            job: req.job,
            trigger: req.trigger,
            from_session: req.from_session,
        };
        self.store.save(&run)?;
        let (tx, _) = broadcast::channel(1024);
        let h = Arc::new(Handle {
            run: Mutex::new(run.clone()),
            tx,
            seq: AtomicU64::new(0),
            worker: Mutex::new(None),
            ended: Default::default(),
            notified: Default::default(),
            env: req.env,
        });
        self.runs.lock().unwrap().insert(id.clone(), h.clone());
        self.changed(&h);
        self.schedule();
        self.get(&id)
    }

    /// Start queued runs while there's room.
    fn schedule(self: &Arc<Self>) {
        if self.shutdown.is_cancelled() {
            return;
        }
        let handles: Vec<Arc<Handle>> = self.runs.lock().unwrap().values().cloned().collect();
        let busy = handles
            .iter()
            .filter(|h| h.worker.lock().unwrap().is_some())
            .count();
        let mut queued: Vec<&Arc<Handle>> = handles
            .iter()
            .filter(|h| {
                h.run.lock().unwrap().state == RunState::Queued
                    && h.worker.lock().unwrap().is_none()
            })
            .collect();
        queued.sort_by_key(|h| h.run.lock().unwrap().created.clone());
        for h in queued
            .into_iter()
            .take(self.max_parallel.saturating_sub(busy))
        {
            self.start(h.clone());
        }
    }

    fn start(self: &Arc<Self>, h: Arc<Handle>) {
        let run = {
            let mut r = h.run.lock().unwrap();
            r.state = RunState::Running;
            r.error = None;
            r.finished = None;
            if r.started.is_none() {
                r.started = Some(super::now());
            }
            r.clone()
        };
        let _ = self.store.save(&run);
        let conn = match self.launcher.launch(&run, &self.store, &h.env) {
            Ok(c) => c,
            Err(e) => {
                {
                    let mut r = h.run.lock().unwrap();
                    r.state = RunState::Failed;
                    r.error = Some(format!("could not start the worker: {e:#}"));
                    r.finished = Some(super::now());
                }
                self.changed(&h);
                return;
            }
        };
        // The handle owns the only input sender: dropping it (daemon
        // shutdown) is how the worker learns the daemon is going away.
        let WorkerConn {
            mut events,
            input,
            kill,
        } = conn;
        *h.worker.lock().unwrap() = Some((input, kill.clone()));
        self.changed(&h);
        let m = self.clone();
        tokio::spawn(async move {
            let mut ended = false;
            while let Some(ev) = events.recv().await {
                if ev["type"] == "run_end" {
                    m.finish(&h, &ev).await;
                    ended = true;
                    break;
                }
                m.on_event(&h, ev);
            }
            kill.cancel();
            if !ended {
                let mut r = h.run.lock().unwrap();
                r.pending = None;
                r.finished = Some(super::now());
                if m.shutdown.is_cancelled() {
                    r.state = RunState::Interrupted;
                    r.error = Some("the chanto daemon stopped while the run was active; `chanto runs resume` continues it".into());
                } else {
                    r.state = RunState::Failed;
                    r.error = Some(format!(
                        "the worker exited unexpectedly (see {})",
                        m.store.run_dir(&r.id).join("worker.log").display()
                    ));
                }
            }
            // The container lives as long as the worker; resume makes a new one.
            let run = h.run.lock().unwrap().clone();
            Manager::remove_container(&run).await;
            *h.worker.lock().unwrap() = None;
            if !ended {
                m.changed(&h);
            }
            h.ended.notify_waiters();
            m.schedule();
        });
    }

    fn on_event(&self, h: &Handle, ev: Value) {
        match ev["type"].as_str() {
            Some("run_started") => {
                {
                    let mut r = h.run.lock().unwrap();
                    r.session_id = ev["session_id"]
                        .as_str()
                        .map(str::to_string)
                        .or(r.session_id.take());
                    r.model = ev["model"].as_str().map(str::to_string);
                    r.resume = false;
                }
                let run = h.run.lock().unwrap().clone();
                let _ = self.store.save(&run);
                self.publish(h, ev);
            }
            Some("permission_request" | "question") => {
                {
                    let mut r = h.run.lock().unwrap();
                    r.state = RunState::NeedsInput;
                    r.pending = Some(ev.clone());
                }
                self.publish(h, ev);
                self.changed(h);
            }
            Some("request_done") => {
                let clear = {
                    let mut r = h.run.lock().unwrap();
                    let open = r
                        .pending
                        .as_ref()
                        .is_some_and(|p| p["request_id"] == ev["request_id"]);
                    if open {
                        r.pending = None;
                        r.state = RunState::Running;
                    }
                    open
                };
                self.publish(h, ev);
                if clear {
                    self.changed(h);
                }
            }
            Some("usage") => {
                if let Some(c) = ev["total_cost_usd"].as_f64() {
                    let run = {
                        let mut r = h.run.lock().unwrap();
                        r.cost_usd = c;
                        r.clone()
                    };
                    let _ = self.store.save(&run);
                }
                self.publish(h, ev);
            }
            _ => self.publish(h, ev),
        }
    }

    async fn finish(&self, h: &Handle, ev: &Value) {
        let state: RunState =
            serde_json::from_value(ev["state"].clone()).unwrap_or(RunState::Failed);
        // A finished run's work is committed to its branch, ready to apply,
        // before anyone sees it as done.
        if state == RunState::Done {
            let r = h.run.lock().unwrap().clone();
            if let Ok(Err(e)) =
                tokio::task::spawn_blocking(move || git::finalize(&r.worktree, &r.id, &r.task))
                    .await
            {
                self.publish(h, json!({"type": "warning", "message": format!("could not commit the run's changes: {e:#}")}));
            }
        }
        {
            let mut r = h.run.lock().unwrap();
            r.state = state;
            r.pending = None;
            r.finished = Some(super::now());
            r.result = ev["result"].as_str().map(str::to_string);
            r.error = ev["error"].as_str().map(str::to_string);
            if let Some(c) = ev["cost_usd"].as_f64() {
                r.cost_usd = c;
            }
            if let Ok(v) = serde_json::from_value::<VerifyResult>(ev["verify"].clone()) {
                r.verify = Some(v);
            }
            if let Some(s) = ev["session_id"].as_str() {
                r.session_id = Some(s.into());
            }
        }
        self.changed(h);
    }

    pub fn respond(&self, id: &str, mut body: Value) -> Result<()> {
        let h = self.handle(id)?;
        let pending = h
            .run
            .lock()
            .unwrap()
            .pending
            .clone()
            .ok_or_else(|| anyhow::anyhow!("run {id} has no open request"))?;
        let rid = body["request_id"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| {
                pending["request_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            });
        if pending["request_id"] != rid.as_str() {
            bail!("no open request {rid} (it may have been answered already)");
        }
        if pending["type"] == "permission_request"
            && !matches!(
                body["decision"].as_str(),
                Some("allow" | "allow_always" | "deny")
            )
        {
            bail!("`decision` must be allow, allow_always or deny");
        }
        body["type"] = json!("respond");
        body["request_id"] = json!(rid);
        let input = h
            .worker
            .lock()
            .unwrap()
            .as_ref()
            .map(|(i, _)| i.clone())
            .ok_or_else(|| anyhow::anyhow!("run {id} has no live worker"))?;
        input
            .send(body)
            .map_err(|_| anyhow::anyhow!("run {id}'s worker is gone"))?;
        Ok(())
    }

    pub fn stop(self: &Arc<Self>, id: &str) -> Result<Run> {
        let h = self.handle(id)?;
        let worker = h.worker.lock().unwrap().clone();
        match worker {
            Some((input, kill)) => {
                let _ = input.send(json!({"type": "stop"}));
                // A worker that doesn't wind down in time is killed.
                let h2 = h.clone();
                tokio::spawn(async move {
                    let ended = h2.ended.notified();
                    tokio::select! {
                        _ = ended => {}
                        _ = tokio::time::sleep(Duration::from_secs(15)) => kill.cancel(),
                    }
                });
            }
            None => {
                {
                    let mut r = h.run.lock().unwrap();
                    if matches!(
                        r.state,
                        RunState::Done | RunState::Failed | RunState::Cancelled
                    ) {
                        bail!("run {} already finished ({})", r.id, r.state.label());
                    }
                    r.state = RunState::Cancelled;
                    r.resume = false;
                    r.finished = Some(super::now());
                }
                self.changed(&h);
            }
        }
        Ok(h.run.lock().unwrap().clone())
    }

    /// Continue an interrupted (or failed/cancelled) run from its saved session.
    pub fn resume(self: &Arc<Self>, id: &str) -> Result<Run> {
        let h = self.handle(id)?;
        {
            let mut r = h.run.lock().unwrap();
            if r.state.is_active() {
                bail!("run {} is {}", r.id, r.state.label());
            }
            if !r.worktree.exists() {
                bail!("run {}'s worktree is gone", r.id);
            }
            r.resume = r.session_id.is_some();
            r.state = RunState::Queued;
            r.error = None;
            r.result = None;
            r.verify = None;
        }
        self.changed(&h);
        self.schedule();
        Ok(h.run.lock().unwrap().clone())
    }

    /// Delete a run: its worktree, branch and record.
    pub async fn remove(&self, id: &str) -> Result<()> {
        let h = self.handle(id)?;
        let run = h.run.lock().unwrap().clone();
        // A finished run's worker may still be winding down (removing its
        // container); give it a moment.
        if !run.state.is_active() {
            for _ in 0..300 {
                let ended = h.ended.notified();
                if h.worker.lock().unwrap().is_none() {
                    break;
                }
                let _ = tokio::time::timeout(Duration::from_millis(100), ended).await;
            }
        }
        if run.state.is_active() || h.worker.lock().unwrap().is_some() {
            bail!("run {} is {}; stop it first", run.id, run.state.label());
        }
        Manager::remove_container(&run).await;
        let r = run.clone();
        tokio::task::spawn_blocking(move || git::remove(&r)).await?;
        self.runs.lock().unwrap().remove(&run.id);
        self.store.remove(&run.id);
        Ok(())
    }

    /// Keep an uploaded git bundle for a run that's about to be created.
    pub async fn save_bundle(&self, bytes: &[u8]) -> Result<String> {
        if !bytes.starts_with(b"# v2 git bundle") && !bytes.starts_with(b"# v3 git bundle") {
            bail!("not a git bundle");
        }
        self.store.ensure_dir()?;
        let dir = self.store.dir.join("bundles");
        std::fs::create_dir_all(&dir)?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        tokio::fs::write(dir.join(format!("{id}.bundle")), bytes).await?;
        Ok(id)
    }

    /// A git bundle of the run's commits, for `chanto runs apply --on`.
    pub async fn bundle(&self, id: &str) -> Result<Vec<u8>> {
        let run = self.get(id)?;
        if run.state.is_active() {
            bail!("run {} is still {}", run.id, run.state.label());
        }
        let path = tokio::task::spawn_blocking(move || git::run_bundle(&run)).await??;
        let bytes = tokio::fs::read(&path).await;
        let _ = std::fs::remove_file(&path);
        Ok(bytes?)
    }

    /// Apply a finished run onto its source checkout's current branch, or
    /// push it and open a pull request (`POST /runs/{id}/apply`).
    pub async fn apply(&self, id: &str, pr: bool) -> Result<String> {
        let run = self.get(id)?;
        if run.state.is_active() {
            bail!(
                "run {} is still {}; wait for it to finish or stop it first",
                run.id,
                run.state.label()
            );
        }
        if run.source.kind != "local" {
            bail!(
                "run {}'s code came from another machine; apply it there with `chanto runs apply --on <server> {}`{}",
                run.id,
                run.id,
                if pr { " --pr" } else { "" }
            );
        }
        let out = tokio::task::spawn_blocking(move || {
            if pr {
                git::open_pr(&run, "origin")
            } else {
                git::apply(&run.source.path, &run)
            }
        })
        .await??;
        Ok(out)
    }

    /// Daemon shutdown: workers save their sessions and end as interrupted.
    pub async fn close(&self) {
        self.shutdown.cancel();
        let handles: Vec<Arc<Handle>> = self.runs.lock().unwrap().values().cloned().collect();
        let mut waits = Vec::new();
        for h in &handles {
            let w = h.worker.lock().unwrap().take();
            if let Some((input, kill)) = w {
                let notified = h.ended.notified();
                drop(input);
                waits.push((h.clone(), kill, notified));
            }
        }
        for (_, kill, notified) in waits {
            if tokio::time::timeout(Duration::from_secs(10), notified)
                .await
                .is_err()
            {
                kill.cancel();
            }
        }
        // Anything still marked active is recorded as interrupted.
        for h in handles {
            let mut r = h.run.lock().unwrap();
            if matches!(r.state, RunState::Running | RunState::NeedsInput) {
                r.state = RunState::Interrupted;
                r.pending = None;
                let _ = self.store.save(&r);
            }
        }
    }
}
