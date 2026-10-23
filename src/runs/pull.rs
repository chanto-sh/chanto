//! `chanto runner connect <url>`: take runs from a remote queue (Chanto
//! cloud, or any server speaking `chanto_proto::runs`) and execute each one
//! as an ordinary background run in this process, so they get the same
//! worktree and branch, verified mode, budgets, OS sandbox and optional
//! container isolation as `chanto run --bg`.
//!
//! The runner only makes outbound HTTPS requests, so it works from behind a
//! firewall, and the code and model keys stay on this machine: it uses this
//! machine's git credentials and Chanto configuration.
//!
//! Per job: fetch the run's credentials → clone (into a cached bare
//! repository) → run → stream events and heartbeats → push the run's branch
//! → report. A server may hand out a git credential for the repository, the
//! org's secrets (environment for the run, masked, and passed to its shell
//! commands) and a model endpoint used instead of this machine's; without
//! them, the machine's own git credentials and configuration apply. Nobody watches these runs, so
//! permission prompts are denied with an explanation and questions get a
//! "use your judgment" answer. A runner that stops (Ctrl-C, crash) simply
//! loses its lease, and the server hands the run to another runner.

use super::manager::{Launcher, Manager, NewRun};
use super::{RunState, Store};
use anyhow::{Context, Result, bail};
use chanto_proto::ApiError;
use chanto_proto::runs::{
    AppendEvents, Claim, CompleteRun, HeartbeatReply, Job, ModelRuns, RegisterRunner,
    RunCredentials, RunResult, RunStatus, RunnerRegistered,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const USAGE: &str = "usage: chanto runner connect <url> [--registration-token <token>] [--name <name>]
                              [--label <label>]... [--mode <mode>] [--ephemeral] [--once] [--own-model]
Take runs from a Chanto cloud queue (or any compatible server) and run them here.
  --registration-token  registers this machine (once; the runner's own token is saved)
                        also CHANTO_RUNNER_REGISTRATION_TOKEN; the url can be CHANTO_CLOUD_URL
  --label               runs that require labels only go to runners with all of them
  --mode                permission mode for runs (default: [background] mode, else acceptEdits)
  --ephemeral           take one job, then deregister (for throwaway VMs and containers)
  --once                exit after one job
  --own-model           always use this machine's model configuration, even when the
                        server offers its model endpoint (also CHANTO_RUNNER_OWN_MODEL=1)";

/// Events are sent in batches this often (and at the end).
const FLUSH_EVERY: Duration = Duration::from_secs(2);
const MAX_BATCH: usize = 500;
/// Well inside a lease, so one slow heartbeat leaves time for the next.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);
const SUMMARY_MAX_CHARS: usize = 4000;

/// Saved registrations, one per server: `<home>/runner/connections.json` (0600).
#[derive(Debug, Default, Serialize, Deserialize)]
struct Connections {
    servers: Vec<Saved>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Saved {
    url: String,
    runner_id: String,
    token: String,
}

/// Where the runs this machine takes from a server are kept, worktrees included.
pub fn store_dir() -> PathBuf {
    crate::config::home_dir().join("runner").join("pulled")
}

fn connections_path() -> PathBuf {
    crate::config::home_dir()
        .join("runner")
        .join("connections.json")
}

fn load_connections() -> Connections {
    std::fs::read(connections_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_connection(saved: Saved) -> Result<()> {
    let mut all = load_connections();
    all.servers.retain(|s| s.url != saved.url);
    all.servers.push(saved);
    let path = connections_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    crate::util::write_private(&path, &serde_json::to_vec_pretty(&all)?)?;
    Ok(())
}

struct Opts {
    url: String,
    registration_token: Option<String>,
    name: String,
    labels: Vec<String>,
    mode: Option<String>,
    ephemeral: bool,
    once: bool,
    /// Ignore the model endpoint the server offers.
    own_model: bool,
}

fn parse(args: &[String]) -> Result<Opts> {
    let mut it = args.iter();
    let mut o = Opts {
        url: String::new(),
        registration_token: std::env::var("CHANTO_RUNNER_REGISTRATION_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()),
        name: sysinfo::System::host_name().unwrap_or_else(|| "chanto runner".into()),
        labels: vec![],
        mode: None,
        ephemeral: false,
        once: false,
        own_model: std::env::var("CHANTO_RUNNER_OWN_MODEL").is_ok_and(|v| v == "1" || v == "true"),
    };
    let mut url = std::env::var("CHANTO_CLOUD_URL").ok();
    while let Some(a) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .cloned()
                .with_context(|| format!("{flag} needs a value"))
        };
        match a.as_str() {
            "--registration-token" => o.registration_token = Some(value(a)?),
            "--name" => o.name = value(a)?,
            "--label" => o.labels.push(value(a)?),
            "--mode" => o.mode = Some(value(a)?),
            "--ephemeral" => o.ephemeral = true,
            "--once" => o.once = true,
            "--own-model" => o.own_model = true,
            "-h" | "--help" | "help" => bail!("{USAGE}"),
            s if s.starts_with('-') => bail!("unknown flag {s}\n{USAGE}"),
            s => url = Some(s.to_string()),
        }
    }
    o.url = url
        .filter(|u| !u.is_empty())
        .with_context(|| format!("which server? pass its URL\n{USAGE}"))?
        .trim_end_matches('/')
        .to_string();
    if !o.url.starts_with("https://")
        && !o.url.starts_with("http://localhost")
        && !o.url.starts_with("http://127.0.0.1")
    {
        bail!(
            "{} must use https:// (plain http only for localhost)",
            o.url
        );
    }
    if let Some(m) = &o.mode
        && crate::permissions::Mode::parse(m).is_none()
    {
        bail!("unknown mode {m}");
    }
    Ok(o)
}

struct Client {
    http: reqwest::Client,
    url: String,
    token: String,
}

impl Client {
    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<Option<T>> {
        self.post_within(path, body, None).await
    }

    /// [`Client::post`] with its own timeout instead of the client's.
    async fn post_within<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
        timeout: Option<Duration>,
    ) -> Result<Option<T>> {
        let mut req = self
            .http
            .post(format!("{}{path}", self.url))
            .bearer_auth(&self.token)
            .json(body);
        if let Some(t) = timeout {
            req = req.timeout(t);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow::Error::new(Transient).context(e))?;
        if resp.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }
        Ok(Some(resp.json().await?))
    }

    /// For calls answering 204: `Ok(())`, or the server's error.
    async fn post_empty(&self, path: &str, body: &impl Serialize) -> Result<()> {
        self.post::<Value>(path, body).await.map(|_| ())
    }
}

/// A failure worth retrying: the network, or the server's own error (5xx).
#[derive(Debug)]
struct Transient;

impl std::fmt::Display for Transient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("temporary failure")
    }
}

impl std::error::Error for Transient {}

fn is_transient(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<Transient>())
}

/// Delays between tries of a call that must get through: about a lease's
/// worth, after which the server has given the run away anyway.
const RETRY_DELAYS: [u64; 6] = [1, 2, 4, 8, 15, 30];

/// Calls `f` until it succeeds or fails for a reason retrying won't fix.
async fn retrying<T>(what: &str, mut f: impl AsyncFnMut() -> Result<T>) -> Result<T> {
    for delay in RETRY_DELAYS {
        match f().await {
            Err(e) if is_transient(&e) => {
                eprintln!(
                    "{}",
                    crate::ui::dim(&format!("! {what}: {e:#}; retrying in {delay}s"))
                );
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
            r => return r,
        }
    }
    f().await
}

/// The server said this runner no longer holds the run.
#[derive(Debug)]
struct LostLease;

impl std::fmt::Display for LostLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the server gave this run to another runner")
    }
}

impl std::error::Error for LostLease {}

async fn api_error(resp: reqwest::Response) -> anyhow::Error {
    let status = resp.status();
    let e = match resp.json::<ApiError>().await {
        Ok(e) if status == reqwest::StatusCode::CONFLICT => {
            return anyhow::Error::new(LostLease).context(e.message);
        }
        Ok(e) => anyhow::anyhow!("{status}: {}", e.message),
        Err(_) => anyhow::anyhow!("{status}"),
    };
    if status.is_server_error() {
        anyhow::Error::new(Transient).context(e)
    } else {
        e
    }
}

fn is_lost_lease(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<LostLease>())
}

async fn register(http: &reqwest::Client, o: &Opts) -> Result<Saved> {
    if let Some(saved) = load_connections()
        .servers
        .into_iter()
        .find(|s| s.url == o.url)
        .filter(|_| o.registration_token.is_none() && !o.ephemeral)
    {
        return Ok(saved);
    }
    let token = o.registration_token.clone().with_context(|| format!("this machine isn't registered with {}; pass --registration-token (from the org's runner settings)", o.url))?;
    let req = RegisterRunner {
        registration_token: token,
        name: o.name.clone(),
        labels: o.labels.clone(),
        version: Some(env!("CARGO_PKG_VERSION").into()),
        ephemeral: o.ephemeral,
    };
    let resp = http
        .post(format!("{}/v1/runners/register", o.url))
        .json(&req)
        .send()
        .await
        .context("registering the runner")?;
    if !resp.status().is_success() {
        return Err(api_error(resp).await.context("registering the runner"));
    }
    let r: RunnerRegistered = resp.json().await?;
    let saved = Saved {
        url: o.url.clone(),
        runner_id: r.runner_id,
        token: r.runner_token,
    };
    // An ephemeral runner's token is useless after its one job.
    if !o.ephemeral {
        save_connection(saved.clone())?;
    }
    eprintln!(
        "Registered as {:?} runner {} with {}",
        r.kind, saved.runner_id, o.url
    );
    Ok(saved)
}

pub async fn cli(args: &[String]) -> Result<()> {
    let o = parse(args)?;
    let http = crate::http::builder()
        .timeout(Duration::from_secs(90))
        .user_agent(concat!("chanto-runner/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let saved = register(&http, &o).await?;
    let client = Client {
        http,
        url: o.url.clone(),
        token: saved.token,
    };
    let exe = std::env::current_exe().context("finding the Chanto executable")?;
    let manager = Manager::open(Store::new(store_dir()), Launcher::Process(exe), 1);
    // Leftovers from a runner that stopped mid-run: their leases are gone.
    for r in manager.list() {
        if !r.state.is_active() {
            let _ = manager.remove(&r.id).await;
        }
    }
    let labels = if o.labels.is_empty() {
        String::new()
    } else {
        format!(" (labels: {})", o.labels.join(", "))
    };
    eprintln!(
        "Waiting for runs from {}{labels}. Ctrl-C stops; a run in progress goes back to the queue.",
        o.url
    );
    let mut backoff = Duration::from_secs(1);
    loop {
        let job: Option<Job> = tokio::select! {
            r = client.post("/v1/runner/claim", &Claim { wait_secs: 30 }) => match r {
                Ok(j) => { backoff = Duration::from_secs(1); j }
                // An ephemeral runner is done once it has taken its run.
                Err(e) if o.ephemeral && !is_transient(&e) => {
                    eprintln!("Claiming: {e:#}; this ephemeral runner is done.");
                    break;
                }
                Err(e) => {
                    eprintln!("{}", crate::ui::dim(&format!("! claiming: {e:#}; retrying in {}s", backoff.as_secs())));
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                    continue;
                }
            },
            _ = tokio::signal::ctrl_c() => break,
        };
        let Some(job) = job else { continue };
        eprintln!(
            "Run {} (attempt {}): {}",
            job.run_id,
            job.attempt,
            crate::util::one_line(&job.task, 80)
        );
        let done = tokio::select! {
            r = execute(&client, &manager, &job, o.mode.clone(), o.own_model) => r,
            _ = tokio::signal::ctrl_c() => {
                eprintln!("Stopping; the run goes back to the queue when its lease runs out.");
                for r in manager.list().into_iter().filter(|r| r.state.is_active()) {
                    let _ = manager.stop(&r.id);
                }
                manager.close().await;
                return Ok(());
            }
        };
        match done {
            Ok(status) => eprintln!("Run {} {}", job.run_id, status_label(status)),
            Err(e) if is_lost_lease(&e) => eprintln!("Run {}: {e:#}; dropped it", job.run_id),
            Err(e) => eprintln!("Run {}: {e:#}", job.run_id),
        }
        if o.once || o.ephemeral {
            break;
        }
    }
    manager.close().await;
    Ok(())
}

fn status_label(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Queued => "queued",
        RunStatus::Running => "running",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

/// The branch to fetch: the job's, else the repository's default branch.
fn resolve_ref(
    repo: &str,
    git_ref: Option<&str>,
    auth: &[(String, String)],
) -> Result<(String, Option<String>)> {
    match git_ref {
        // A commit: fetch the default branch and start from the commit on it.
        Some(r) if r.len() == 40 && r.bytes().all(|b| b.is_ascii_hexdigit()) => {
            Ok((default_branch(repo, auth)?, Some(r.to_string())))
        }
        Some(r) => Ok((r.to_string(), None)),
        None => Ok((default_branch(repo, auth)?, None)),
    }
}

fn default_branch(repo: &str, auth: &[(String, String)]) -> Result<String> {
    let out = super::git::git_with(
        &std::env::temp_dir(),
        &["ls-remote", "--symref", "--", repo, "HEAD"],
        auth,
    )
    .with_context(|| format!("reading {repo}'s default branch"))?;
    out.lines()
        .find_map(|l| {
            l.strip_prefix("ref: ")
                .and_then(|r| r.split_whitespace().next())
                .map(str::to_string)
        })
        .with_context(|| format!("{repo} has no default branch"))
}

/// The run's credentials. Servers that don't hand any out answer 404 or
/// 405: then the machine's own git credentials and configuration apply.
/// The path of a call about the job's run, saying which attempt it's for
/// (the server refuses calls left over from an earlier one).
fn run_path(job: &Job, what: &str) -> String {
    format!(
        "/v1/runner/runs/{}/{what}?attempt={}",
        job.run_id, job.attempt
    )
}

async fn credentials(client: &Client, job: &Job) -> Result<RunCredentials> {
    retrying("fetching the run's credentials", async || {
        fetch_credentials(client, job).await
    })
    .await
}

async fn fetch_credentials(client: &Client, job: &Job) -> Result<RunCredentials> {
    let resp = client
        .http
        .post(format!("{}{}", client.url, run_path(job, "credentials")))
        .bearer_auth(&client.token)
        .json(&json!({}))
        .send()
        .await
        .map_err(|e| anyhow::Error::new(Transient).context(e))?;
    match resp.status().as_u16() {
        404 | 405 => Ok(RunCredentials::default()),
        s if (200..300).contains(&s) => Ok(resp.json().await?),
        _ => Err(api_error(resp).await),
    }
}

/// Where to clone from, and the environment that authenticates git to it.
fn git_source(job: &Job, creds: &RunCredentials) -> (String, Vec<(String, String)>) {
    match &creds.git {
        Some(g) => (
            g.url.clone(),
            super::git::auth_env(&g.url, &g.username, &g.password),
        ),
        None => (job.repo.clone(), vec![]),
    }
}

/// The worker's environment: the org's secrets, listed in
/// `CHANTO_RUN_SECRETS` so the run's shell commands get them and they're
/// masked, and the model endpoint, if the server provides one and the runner
/// wasn't told to keep its own (`--own-model`). Secrets named `CHANTO_*`
/// would change Chanto itself, so they're left out.
fn worker_env(creds: &RunCredentials, own_model: bool) -> (Vec<(String, String)>, Vec<String>) {
    let (mut env, mut skipped, mut names) = (vec![], vec![], vec![]);
    for (k, v) in &creds.env {
        let valid = !k.is_empty()
            && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && !k.as_bytes()[0].is_ascii_digit();
        if !valid || k.starts_with("CHANTO_") || v.contains('\0') {
            skipped.push(k.clone());
            continue;
        }
        env.push((k.clone(), v.clone()));
        names.push(k.clone());
    }
    if !names.is_empty() {
        env.push(("CHANTO_RUN_SECRETS".into(), names.join(",")));
    }
    if let Some(m) = creds.model.as_ref().filter(|_| !own_model) {
        env.push(("CHANTO_PROVIDER".into(), m.provider.clone()));
        env.push(("CHANTO_BASE_URL".into(), m.base_url.clone()));
        env.push(("CHANTO_API_KEY".into(), m.api_key.clone()));
        if let Some(model) = &m.model {
            env.push(("CHANTO_MODEL".into(), model.clone()));
        }
    }
    // For the run's own commands and scripts (see secrets::withhold_env).
    if let Some(t) = &creds.tools {
        env.push(("CHANTO_CLOUD_URL".into(), t.url.clone()));
        env.push(("CHANTO_RUN_TOKEN".into(), t.token.clone()));
    }
    (env, skipped)
}

/// A git credential this close to expiring is fetched again before use.
const REFRESH_BEFORE: chrono::Duration = chrono::Duration::minutes(5);

fn expiring(creds: &RunCredentials) -> bool {
    creds
        .git
        .as_ref()
        .and_then(|g| g.expires_at)
        .is_some_and(|t| t - chrono::Utc::now() < REFRESH_BEFORE)
}

/// Runs one job to the end, keeping the lease, and reports it. Returns the
/// status reported, or an error if the run couldn't be reported.
async fn execute(
    client: &Client,
    manager: &Arc<Manager>,
    job: &Job,
    mode: Option<String>,
    own_model: bool,
) -> Result<RunStatus> {
    let id_path = |what: &str| run_path(job, what);
    let creds = match credentials(client, job).await {
        Ok(c) => Some(c),
        Err(e) if is_lost_lease(&e) => return Err(e),
        Err(e) => {
            let result = RunResult {
                summary: format!("Could not get the run's credentials: {e:#}"),
                branch: None,
                pr_url: None,
                pr_error: None,
                commit: None,
            };
            client
                .post_empty(
                    &id_path("complete"),
                    &CompleteRun {
                        status: RunStatus::Failed,
                        result,
                    },
                )
                .await?;
            return Ok(RunStatus::Failed);
        }
    }
    .unwrap_or_default();
    let (mut env, skipped) = worker_env(&creds, own_model);
    if let Some(checks) = &job.checks {
        env.push((chanto_proto::runs::CHECKS_ENV.into(), checks.join("\n")));
    }
    if !skipped.is_empty() {
        eprintln!(
            "{}",
            crate::ui::dim(&format!(
                "! left out org secrets the run can't use: {}",
                skipped.join(", ")
            ))
        );
    }
    let (url, auth) = git_source(job, &creds);
    let mut task = job.task.clone();
    let mut start_ref = job.git_ref.clone();
    // The script, if any: the deterministic part, before (or instead of) the model.
    let mut script: Option<ScriptRun> = None;
    if let Some(commands) = job.script.as_deref().filter(|s| !s.trim().is_empty()) {
        let outcome = match keep_lease(
            client,
            job,
            true,
            run_script(job, &url, &auth, &script_env(&env), commands),
        )
        .await
        {
            Err(e) if is_lost_lease(&e) => return Err(e),
            Err(e) if e.to_string() == CANCELLED => {
                let result = RunResult {
                    summary: "Cancelled while its script ran.".into(),
                    branch: None,
                    pr_url: None,
                    pr_error: None,
                    commit: None,
                };
                client
                    .post_empty(
                        &id_path("complete"),
                        &CompleteRun {
                            status: RunStatus::Cancelled,
                            result,
                        },
                    )
                    .await?;
                return Ok(RunStatus::Cancelled);
            }
            Err(e) => {
                let result = RunResult {
                    summary: format!("Could not run the script: {e:#}"),
                    branch: None,
                    pr_url: None,
                    pr_error: None,
                    commit: None,
                };
                client
                    .post_empty(
                        &id_path("complete"),
                        &CompleteRun {
                            status: RunStatus::Failed,
                            result,
                        },
                    )
                    .await?;
                return Ok(RunStatus::Failed);
            }
            Ok(o) => o,
        };
        let mut events = vec![
            json!({"type": "tool_start", "id": "script", "label": "Script"}),
            json!({"type": "tool_end", "id": "script", "is_error": !outcome.ok, "output": outcome.output, "display": outcome.headline()}),
        ];
        let _ = send_events(client, job, &mut events).await;
        let model = !job.task.trim().is_empty()
            && match job.model_runs {
                ModelRuns::Always => true,
                ModelRuns::OnChange => outcome.changed,
                ModelRuns::OnFailure => !outcome.ok,
                ModelRuns::Never => false,
            };
        if !model {
            // Script only: its changes, if any, are the run's branch. The
            // lease is kept until the server has the result.
            return keep_lease(client, job, false, async {
                let mut summary = outcome.report();
                let mut branch = None;
                let mut unpushed = false;
                if outcome.changed {
                    let name = format!("chanto/run/{}", &job.run_id[..8.min(job.run_id.len())]);
                    match outcome.push(&url, &auth, &name, &job.task).await {
                        Ok(()) => branch = Some(name),
                        Err(e) => {
                            unpushed = true;
                            summary.push_str(&format!("\n\nThe changes could not be pushed: {e:#}\nThey're kept on the runner's machine in {}.", outcome.dir.display()));
                        }
                    }
                } else {
                    summary.push_str("\n\nNo changes were made.");
                }
                let status = if outcome.ok && !unpushed { RunStatus::Succeeded } else { RunStatus::Failed };
                let summary: String = summary.chars().take(SUMMARY_MAX_CHARS).collect();
                let result = RunResult { summary, branch, pr_url: None, pr_error: None, commit: outcome.base.clone() };
                retrying("reporting the run", async || client.post_empty(&id_path("complete"), &CompleteRun { status, result: result.clone() }).await).await?;
                if !unpushed {
                    outcome.cleanup();
                }
                Ok(status)
            })
            .await;
        }
        // The model next, on top of the script's commit (a temporary branch).
        if outcome.changed {
            let name = format!("chanto/script/{}", &job.run_id[..8.min(job.run_id.len())]);
            if let Err(e) = keep_lease(
                client,
                job,
                false,
                outcome.push(&url, &auth, &name, &job.task),
            )
            .await
            {
                let result = RunResult {
                    summary: format!(
                        "{}\n\nThe script's changes could not be pushed: {e:#}",
                        outcome.report()
                    ),
                    branch: None,
                    pr_url: None,
                    pr_error: None,
                    commit: outcome.base.clone(),
                };
                client
                    .post_empty(
                        &id_path("complete"),
                        &CompleteRun {
                            status: RunStatus::Failed,
                            result,
                        },
                    )
                    .await?;
                outcome.cleanup();
                return Ok(RunStatus::Failed);
            }
            start_ref = Some(name.clone());
            script = Some(ScriptRun {
                outcome,
                temp_branch: Some(name),
            });
        } else {
            script = Some(ScriptRun {
                outcome,
                temp_branch: None,
            });
        }
        if let Some(s) = &script {
            task.push_str("\n\n");
            task.push_str(&s.outcome.note_for_model());
        }
    }
    let (repo, git_ref, a) = (url.clone(), start_ref.clone(), auth.clone());
    let prepared =
        tokio::task::spawn_blocking(move || resolve_ref(&repo, git_ref.as_deref(), &a)).await?;
    let run = match prepared {
        Ok((branch, commit)) => {
            let source = json!({"kind": "git", "url": url, "ref": branch, "commit": commit});
            manager
                .create(NewRun {
                    task: task.clone(),
                    source: Some(source),
                    mode,
                    env,
                    git_env: auth.clone(),
                    ..Default::default()
                })
                .await
        }
        Err(e) => Err(e),
    };
    let run = match run {
        Ok(r) => r,
        Err(e) => {
            let result = RunResult {
                summary: format!("Could not start the run: {e:#}"),
                branch: None,
                pr_url: None,
                pr_error: None,
                commit: None,
            };
            client
                .post_empty(
                    &id_path("complete"),
                    &CompleteRun {
                        status: RunStatus::Failed,
                        result,
                    },
                )
                .await?;
            return Ok(RunStatus::Failed);
        }
    };
    let (_, mut rx) = manager.subscribe(&run.id)?;
    let mut batch: Vec<Value> = Vec::new();
    let mut flush = tokio::time::interval(FLUSH_EVERY);
    let mut beat = tokio::time::interval(Duration::from_secs((job.lease_secs / 3).max(1)));
    let mut cancelled = false;
    let lost = |e: &anyhow::Error| is_lost_lease(e);
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(ev) => {
                    let state = ev["state"].as_str().and_then(|s| serde_json::from_value::<RunState>(json!(s)).ok());
                    match ev["type"].as_str() {
                        Some("permission_request") => {
                            let _ = manager.respond(&run.id, json!({"request_id": ev["request_id"], "decision": "deny",
                                "feedback": "This run is unattended, so nobody can approve this. Find another way within the permissions you have, or finish and explain what is blocked."}));
                        }
                        Some("question") => {
                            let _ = manager.respond(&run.id, json!({"request_id": ev["request_id"],
                                "answer": "This run is unattended, so nobody can answer. Use your best judgment and note the assumption in your summary."}));
                        }
                        _ => {}
                    }
                    if super::persisted(&ev) {
                        batch.push(ev.clone());
                    }
                    if ev["type"] == "run_state" && state.is_some_and(|s| !s.is_active()) {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = flush.tick() => {
                if let Err(e) = send_events(client, job, &mut batch).await
                    && lost(&e) { let _ = manager.stop(&run.id); return Err(e); }
            }
            // Short, so a hung server doesn't hold up events past a lease.
            _ = beat.tick() => match client.post_within::<HeartbeatReply>(&id_path("heartbeat"), &json!({}), Some(HEARTBEAT_TIMEOUT)).await {
                Ok(Some(r)) if r.cancel_requested && !cancelled => {
                    cancelled = true;
                    let _ = manager.stop(&run.id);
                }
                Err(e) if lost(&e) => { let _ = manager.stop(&run.id); return Err(e); }
                // A missed heartbeat is retried on the next tick, well within the lease.
                _ => {}
            },
        }
    }
    // From here to the server having the result (pushing can take a while),
    // the lease is kept, or the server would hand the finished run out again.
    keep_lease(
        client,
        job,
        false,
        finish(
            client,
            manager,
            job,
            &run.id,
            batch,
            cancelled,
            creds,
            script,
            (&url, &auth),
        ),
    )
    .await
}

/// The end of a model run: the last events, its branch pushed, the result
/// reported, and the run's files removed (unless the push failed).
#[allow(clippy::too_many_arguments)]
async fn finish(
    client: &Client,
    manager: &Arc<Manager>,
    job: &Job,
    run_id: &str,
    mut batch: Vec<Value>,
    cancelled: bool,
    mut creds: RunCredentials,
    script: Option<ScriptRun>,
    (url, auth): (&str, &[(String, String)]),
) -> Result<RunStatus> {
    let id_path = |what: &str| run_path(job, what);
    // Events that still can't be sent don't stop the result being reported.
    if let Err(e) = retrying("sending the run's events", async || {
        send_events(client, job, &mut batch).await
    })
    .await
    {
        if is_lost_lease(&e) {
            return Err(e);
        }
        eprintln!(
            "{}",
            crate::ui::dim(&format!(
                "! dropped {} of the run's events: {e:#}",
                batch.len()
            ))
        );
    }
    let finished = manager.get(run_id)?;
    let mut status = match finished.state {
        RunState::Done => RunStatus::Succeeded,
        RunState::Cancelled => RunStatus::Cancelled,
        _ if cancelled => RunStatus::Cancelled,
        _ => RunStatus::Failed,
    };
    let mut summary = finished
        .result
        .clone()
        .or(finished.error.clone())
        .unwrap_or_else(|| "The run ended without a summary.".into());
    // The run's directory goes away below, so keep the end of its log.
    if status == RunStatus::Failed
        && let Ok(log) = std::fs::read_to_string(manager.store.run_dir(run_id).join("worker.log"))
    {
        let tail: Vec<&str> = log.lines().rev().take(20).collect();
        if !tail.is_empty() {
            summary.push_str(&format!(
                "\n\nEnd of the worker log:\n{}",
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            ));
        }
    }
    if let Some(v) = finished.verify.as_ref().filter(|v| v.status != "off") {
        summary.push_str(&format!("\n\nVerification: {} {}", v.status, v.detail));
    }
    let mut branch = None;
    let mut unpushed = false;
    if status == RunStatus::Succeeded {
        // A GitHub token lasts an hour; runs can take longer.
        if expiring(&creds) {
            match credentials(client, job).await {
                Ok(c) => creds = c,
                Err(e) => eprintln!(
                    "{}",
                    crate::ui::dim(&format!(
                        "! refreshing the git credential: {e:#}; pushing with the old one"
                    ))
                ),
            }
        }
        let (r, (_, auth)) = (finished.clone(), git_source(job, &creds));
        match tokio::task::spawn_blocking(move || push_branch(&r, &auth)).await? {
            Ok(Some(b)) => branch = Some(b),
            Ok(None) => summary.push_str("\n\nNo changes were made."),
            // The work isn't anywhere the org can see it: not a success,
            // and the checkout stays for someone to recover.
            Err(e) => {
                unpushed = true;
                status = RunStatus::Failed;
                summary.push_str(&format!(
                    "\n\nThe changes are committed but could not be pushed: {e:#}\nThey're kept on the runner's machine in {} (branch {}).",
                    finished.source.path.display(),
                    finished.branch
                ));
            }
        }
    }
    let mut commit = Some(finished.source.head.clone()).filter(|h| !h.is_empty());
    if let Some(s) = &script {
        summary.push_str("\n\n");
        summary.push_str(&s.outcome.report());
        // Where the run started is before its script's commit.
        commit = s.outcome.base.clone().or(commit);
    }
    let summary: String = summary.chars().take(SUMMARY_MAX_CHARS).collect();
    let done = CompleteRun {
        status,
        result: RunResult {
            summary,
            branch,
            pr_url: None,
            pr_error: None,
            commit,
        },
    };
    let complete = id_path("complete");
    retrying("reporting the run", async || {
        client.post_empty(&complete, &done).await
    })
    .await?;
    if !unpushed {
        let _ = manager.remove(run_id).await;
    }
    if let Some(s) = script {
        if let Some(b) = &s.temp_branch {
            let _ = s.outcome.delete_branch(url, auth, b).await;
        }
        s.outcome.cleanup();
    }
    Ok(status)
}

// ------------------------------------------------------------------ scripts

const CANCELLED: &str = "cancelled";
/// Script output kept for the run's events and summary (its end).
const SCRIPT_OUTPUT_CHARS: usize = 2500;
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// A script that ran before the model, and the temporary branch holding its changes.
struct ScriptRun {
    outcome: ScriptOutcome,
    temp_branch: Option<String>,
}

/// What a run's script did, in a checkout of its own.
struct ScriptOutcome {
    dir: PathBuf,
    ok: bool,
    code: Option<i32>,
    /// The end of what it printed (stdout and stderr together).
    output: String,
    /// It left changes in the checkout.
    changed: bool,
    /// The commit it started from.
    base: Option<String>,
}

impl ScriptOutcome {
    fn headline(&self) -> String {
        match (self.ok, self.code) {
            (true, _) => format!(
                "Script finished{}",
                if self.changed { ", with changes" } else { "" }
            ),
            (false, Some(c)) => format!("Script failed (exit {c})"),
            (false, None) => "Script stopped (timed out or killed)".into(),
        }
    }

    /// For the run's summary.
    fn report(&self) -> String {
        let out = if self.output.trim().is_empty() {
            "(no output)".to_string()
        } else {
            self.output.trim_end().to_string()
        };
        format!(
            "**{}.**\n\n```\n{}\n```",
            self.headline(),
            out.replace("```", "ʼʼʼ")
        )
    }

    /// For the model's task: what ran before it.
    fn note_for_model(&self) -> String {
        format!(
            "Before you, this run's script ran: {}. {}\n\nIts output:\n```\n{}\n```",
            self.headline().to_lowercase(),
            if self.changed {
                "Its changes are committed on the branch you start from."
            } else {
                "It changed no files."
            },
            self.output.trim_end().replace("```", "ʼʼʼ")
        )
    }

    /// Commits its changes as `branch` and pushes them.
    async fn push(
        &self,
        url: &str,
        auth: &[(String, String)],
        branch: &str,
        task: &str,
    ) -> Result<()> {
        let (dir, url, auth, branch) = (
            self.dir.clone(),
            url.to_string(),
            auth.to_vec(),
            branch.to_string(),
        );
        let first = task
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .chars()
            .take(60)
            .collect::<String>();
        let message = if first.is_empty() {
            "Changes from the run's script".to_string()
        } else {
            format!("Script: {first}")
        };
        tokio::task::spawn_blocking(move || -> Result<()> {
            let id = [
                "-c",
                "user.name=chanto",
                "-c",
                "user.email=chanto@users.noreply.github.com",
            ];
            super::git::git_with(&dir, &["checkout", "-q", "-b", &branch], &[])?;
            super::git::git_with(&dir, &["add", "-A"], &[])?;
            super::git::git_with(
                &dir,
                &[&id[..], &["commit", "-q", "-m", &message]].concat(),
                &[],
            )?;
            super::git::git_with(
                &dir,
                &[
                    "push",
                    "-q",
                    "--",
                    &url,
                    &format!("HEAD:refs/heads/{branch}"),
                ],
                &auth,
            )?;
            Ok(())
        })
        .await?
    }

    async fn delete_branch(
        &self,
        url: &str,
        auth: &[(String, String)],
        branch: &str,
    ) -> Result<()> {
        let (dir, url, auth, branch) = (
            self.dir.clone(),
            url.to_string(),
            auth.to_vec(),
            branch.to_string(),
        );
        tokio::task::spawn_blocking(move || {
            super::git::git_with(
                &dir,
                &["push", "-q", "--", &url, &format!(":refs/heads/{branch}")],
                &auth,
            )
            .map(|_| ())
        })
        .await?
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// What a script sees: the run's secrets and run tools, not its model key.
fn script_env(env: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<_> = env
        .iter()
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "CHANTO_PROVIDER" | "CHANTO_BASE_URL" | "CHANTO_API_KEY" | "CHANTO_MODEL"
            )
        })
        .cloned()
        .collect();
    // Python's bytecode caches would otherwise count as the script's changes
    // in repositories that don't ignore them.
    out.push(("PYTHONDONTWRITEBYTECODE".into(), "1".into()));
    out
}

/// Clones the run's repository at its ref and runs the script there with bash
/// (`-e`: it stops at the first failing command).
async fn run_script(
    job: &Job,
    url: &str,
    auth: &[(String, String)],
    env: &[(String, String)],
    script: &str,
) -> Result<ScriptOutcome> {
    let dir = std::env::temp_dir().join(format!("chanto-script-{}", job.run_id));
    let _ = std::fs::remove_dir_all(&dir);
    let (d, u, a, r) = (
        dir.clone(),
        url.to_string(),
        auth.to_vec(),
        job.git_ref.clone(),
    );
    let base = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let parent = d.parent().context("no temporary directory")?;
        let name = d
            .file_name()
            .and_then(|n| n.to_str())
            .context("bad directory name")?;
        super::git::git_with(
            parent,
            &[
                "clone",
                "-q",
                "--no-tags",
                "--filter=blob:none",
                "--",
                &u,
                name,
            ],
            &a,
        )
        .context("cloning the repository")?;
        if let Some(r) = r.as_deref().filter(|r| !r.is_empty()) {
            super::git::git_with(&d, &["checkout", "-q", r, "--"], &a)
                .with_context(|| format!("checking out {r}"))?;
        }
        Ok(super::git::git_with(&d, &["rev-parse", "HEAD"], &[])
            .ok()
            .map(|s| s.trim().to_string()))
    })
    .await??;
    let bash = crate::shell::bash()
        .context("this script needs bash; on Windows install Git for Windows (Git Bash)")?;
    let mut cmd = tokio::process::Command::new(bash);
    cmd.args(["--noprofile", "--norc", "-eo", "pipefail", "-c"])
        .arg(format!("exec 2>&1\n{script}"))
        .current_dir(&dir)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().context("starting bash")?;
    let (ok, code, output) =
        match tokio::time::timeout(SCRIPT_TIMEOUT, child.wait_with_output()).await {
            Ok(out) => {
                let out = out.context("running the script")?;
                (
                    out.status.success(),
                    out.status.code(),
                    String::from_utf8_lossy(&out.stdout).into_owned(),
                )
            }
            Err(_) => (
                false,
                None,
                format!("(stopped after {} minutes)", SCRIPT_TIMEOUT.as_secs() / 60),
            ),
        };
    let tail: String = {
        let chars: Vec<char> = output.chars().collect();
        chars[chars.len().saturating_sub(SCRIPT_OUTPUT_CHARS)..]
            .iter()
            .collect()
    };
    let d = dir.clone();
    let changed = tokio::task::spawn_blocking(move || {
        super::git::git_with(&d, &["status", "--porcelain"], &[]).map(|s| !s.trim().is_empty())
    })
    .await?
    .unwrap_or(false);
    Ok(ScriptOutcome {
        dir,
        ok,
        code,
        output: tail,
        changed,
        base,
    })
}

/// Runs `work` while keeping the run's lease: heartbeats, and, if `cancel`,
/// stops it (an error saying so) when the run is cancelled.
async fn keep_lease<T>(
    client: &Client,
    job: &Job,
    cancel: bool,
    work: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let path = run_path(job, "heartbeat");
    let mut beat = tokio::time::interval(Duration::from_secs((job.lease_secs / 3).max(1)));
    tokio::pin!(work);
    loop {
        tokio::select! {
            r = &mut work => return r,
            _ = beat.tick() => match client.post_within::<HeartbeatReply>(&path, &json!({}), Some(HEARTBEAT_TIMEOUT)).await {
                Ok(Some(r)) if cancel && r.cancel_requested => bail!(CANCELLED),
                Err(e) if is_lost_lease(&e) => return Err(e),
                _ => {}
            },
        }
    }
}

/// Sends the batch in chunks; whatever the server didn't take for a reason
/// worth retrying stays in it. A chunk it refuses outright (a 4xx) is dropped,
/// so one bad event can't hold up the rest of the run's.
async fn send_events(client: &Client, job: &Job, batch: &mut Vec<Value>) -> Result<()> {
    let path = run_path(job, "events");
    while !batch.is_empty() {
        let n = batch.len().min(MAX_BATCH);
        match client
            .post_empty(
                &path,
                &AppendEvents {
                    events: batch[..n].to_vec(),
                },
            )
            .await
        {
            Ok(()) => {}
            Err(e) if is_transient(&e) || is_lost_lease(&e) => return Err(e),
            Err(e) => eprintln!(
                "{}",
                crate::ui::dim(&format!(
                    "! the server refused {n} of the run's events: {e:#}"
                ))
            ),
        }
        batch.drain(..n);
    }
    Ok(())
}

/// Pushes the run's commits as its branch on the repository it came from.
/// `None` when the run made no commits.
fn push_branch(run: &super::Run, auth: &[(String, String)]) -> Result<Option<String>> {
    let url = run
        .source
        .url
        .as_deref()
        .context("the run has no source URL")?;
    if super::git::commit_count(&run.source.path, &run.base, &run.branch) == 0 {
        return Ok(None);
    }
    super::git::git_with(
        &run.source.path,
        &[
            "push",
            "-q",
            "--",
            url,
            &format!("refs/heads/{0}:refs/heads/{0}", run.branch),
        ],
        auth,
    )?;
    Ok(Some(run.branch.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chanto_proto::runs::{GitCredential, ModelAccess};
    use std::collections::BTreeMap;

    fn job(repo: &str) -> Job {
        Job {
            run_id: "r1".into(),
            repo: repo.into(),
            git_ref: None,
            task: "t".into(),
            labels: vec![],
            attempt: 1,
            lease_secs: 60,
            script: None,
            model_runs: Default::default(),
            checks: None,
        }
    }

    fn creds(expires_in: Option<chrono::Duration>) -> RunCredentials {
        RunCredentials {
            git: Some(GitCredential {
                url: "https://github.com/acme/app.git".into(),
                username: "x-access-token".into(),
                password: "ghs_secret".into(),
                expires_at: expires_in.map(|d| chrono::Utc::now() + d),
            }),
            env: BTreeMap::from([
                ("DATABASE_URL".to_string(), "postgres://x".to_string()),
                ("CHANTO_MODEL".to_string(), "sneaky".to_string()),
                ("1BAD".to_string(), "x".to_string()),
            ]),
            model: Some(ModelAccess {
                provider: "anthropic".into(),
                base_url: "https://cloud.example/v1/model/anthropic".into(),
                api_key: "hrm_run".into(),
                model: None,
            }),
            tools: Some(chanto_proto::runs::RunTools {
                url: "https://cloud.example".into(),
                token: "crx_run".into(),
            }),
        }
    }

    /// A repository with one commit, as a clone URL.
    fn origin() -> (std::path::PathBuf, String) {
        let dir =
            std::env::temp_dir().join(format!("chanto-origin-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&dir)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "{args:?}"
            )
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("prices.json"), "{\"a\": 1}\n").unwrap();
        git(&["add", "-A"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "first",
        ]);
        let url = dir.to_string_lossy().to_string();
        (dir, url)
    }

    #[tokio::test]
    async fn scripts_run_in_a_checkout_and_say_what_they_did() {
        let (origin, url) = origin();
        let mut j = job(&url);
        j.run_id = format!("{}", uuid::Uuid::new_v4().simple());
        let env = vec![
            ("PRICE".to_string(), "2".to_string()),
            ("CHANTO_API_KEY".to_string(), "hrm_x".to_string()),
        ];

        // Changes files: the run sees them, and the model key never reaches the script.
        let o = run_script(&j, &url, &[], &script_env(&env), "echo \"{\\\"a\\\": $PRICE}\" > prices.json\necho \"key=${CHANTO_API_KEY:-none}\"\necho updated").await.unwrap();
        assert!(o.ok && o.changed && o.base.is_some(), "{}", o.output);
        assert!(
            o.output.contains("key=none") && o.output.contains("updated"),
            "{}",
            o.output
        );
        assert!(
            o.report().contains("Script finished, with changes")
                && o.note_for_model().contains("committed on the branch")
        );
        // Pushed as a branch of the origin.
        o.push(&url, &[], "chanto/run/test", "Keep prices current")
            .await
            .unwrap();
        let branches = std::process::Command::new("git")
            .args(["branch", "--list", "chanto/*"])
            .current_dir(&origin)
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&branches.stdout).contains("chanto/run/test"));
        o.cleanup();

        // Stops at the first failing command, and says so.
        let o = run_script(&j, &url, &[], &[], "echo before\nfalse\necho after")
            .await
            .unwrap();
        assert!(!o.ok && o.code == Some(1) && !o.changed);
        assert!(o.output.contains("before") && !o.output.contains("after"));
        assert!(o.report().contains("Script failed (exit 1)"));
        o.cleanup();

        // Nothing to change.
        let o = run_script(&j, &url, &[], &[], "echo prices are current")
            .await
            .unwrap();
        assert!(o.ok && !o.changed && o.note_for_model().contains("changed no files"));
        o.cleanup();
        let _ = std::fs::remove_dir_all(&origin);
    }

    #[test]
    fn worker_env_carries_secrets_and_the_model_endpoint() {
        let (env, skipped) = worker_env(&creds(None), false);
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("DATABASE_URL"), Some("postgres://x"));
        assert_eq!(get("CHANTO_RUN_SECRETS"), Some("DATABASE_URL"));
        assert_eq!(
            (
                get("CHANTO_PROVIDER"),
                get("CHANTO_BASE_URL"),
                get("CHANTO_API_KEY")
            ),
            (
                Some("anthropic"),
                Some("https://cloud.example/v1/model/anthropic"),
                Some("hrm_run")
            )
        );
        assert_eq!(
            get("CHANTO_MODEL"),
            None,
            "an org secret can't pick the model, and the server didn't"
        );
        assert_eq!(
            (get("CHANTO_CLOUD_URL"), get("CHANTO_RUN_TOKEN")),
            (Some("https://cloud.example"), Some("crx_run"))
        );
        assert_eq!(skipped, ["1BAD", "CHANTO_MODEL"]);
        let (env, skipped) = worker_env(&RunCredentials::default(), false);
        assert!(env.is_empty() && skipped.is_empty());
        // --own-model: the secrets still come, the server's model doesn't.
        let (env, _) = worker_env(&creds(None), true);
        assert!(env.iter().any(|(k, _)| k == "DATABASE_URL"));
        assert!(!env.iter().any(|(k, _)| k.starts_with("CHANTO_PROVIDER")
            || k == "CHANTO_BASE_URL"
            || k == "CHANTO_API_KEY"));
    }

    #[test]
    fn git_source_prefers_the_credential_url() {
        let (url, auth) = git_source(&job("git@github.com:acme/app.git"), &creds(None));
        assert_eq!(url, "https://github.com/acme/app.git");
        assert!(auth.contains(&(
            "GIT_CONFIG_KEY_0".to_string(),
            "http.https://github.com/acme/app.git.extraHeader".to_string()
        )));
        assert!(
            !auth.iter().any(|(_, v)| v.contains("ghs_secret")),
            "only base64 in the header value"
        );
        assert_eq!(
            git_source(
                &job("https://example.com/r.git"),
                &RunCredentials::default()
            ),
            ("https://example.com/r.git".to_string(), vec![])
        );
        assert!(expiring(&creds(Some(chrono::Duration::minutes(2)))));
        assert!(!expiring(&creds(Some(chrono::Duration::minutes(50)))) && !expiring(&creds(None)));
    }

    /// A server answering the credentials route with `status` and `body`.
    async fn server(status: u16, body: Value) -> Client {
        let app = axum::Router::new().route(
            "/v1/runner/runs/{id}/credentials",
            axum::routing::post(move || async move {
                (
                    axum::http::StatusCode::from_u16(status).unwrap(),
                    axum::Json(body),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Client {
            http: crate::http::client(),
            url,
            token: "t".into(),
        }
    }

    #[tokio::test]
    async fn credentials_from_old_and_new_servers() {
        let c = server(200, serde_json::to_value(creds(None)).unwrap()).await;
        let j = job("x");
        assert_eq!(credentials(&c, &j).await.unwrap(), creds(None));
        let c = server(404, json!({"error": "not_found", "message": "not found"})).await;
        assert_eq!(
            credentials(&c, &j).await.unwrap(),
            RunCredentials::default(),
            "an older server"
        );
        let c = server(409, json!({"error": "conflict", "message": "stop"})).await;
        assert!(is_lost_lease(&credentials(&c, &j).await.unwrap_err()));
    }

    /// A server error is retried; events stay queued until the server takes them.
    #[tokio::test]
    async fn transient_failures_are_retried_and_events_kept() {
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let (c1, c2) = (calls.clone(), calls.clone());
        let fail_first = move |n: u32| {
            if n == 0 {
                axum::http::StatusCode::SERVICE_UNAVAILABLE
            } else {
                axum::http::StatusCode::OK
            }
        };
        let app = axum::Router::new()
            .route(
                "/v1/runner/runs/{id}/credentials",
                axum::routing::post(move || async move {
                    let n = c1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (
                        fail_first(n),
                        axum::Json(serde_json::to_value(creds(None)).unwrap()),
                    )
                }),
            )
            .route(
                "/v1/runner/runs/{id}/events",
                axum::routing::post(move || async move {
                    let n = c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    match n {
                        2 => axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        4 => axum::http::StatusCode::BAD_REQUEST,
                        _ => axum::http::StatusCode::NO_CONTENT,
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let c = Client {
            http: crate::http::client(),
            url,
            token: "t".into(),
        };

        let j = job("x");
        assert_eq!(
            credentials(&c, &j).await.unwrap(),
            creds(None),
            "retried after a 503"
        );
        let mut batch = vec![json!({"type": "a"})];
        let e = send_events(&c, &j, &mut batch).await.unwrap_err();
        assert!(is_transient(&e));
        assert_eq!(batch.len(), 1, "kept for the next try");
        send_events(&c, &j, &mut batch).await.unwrap();
        assert!(batch.is_empty());
        // Refused outright (call 4): dropped, and the rest still go.
        let mut batch = vec![json!({"type": "b"})];
        send_events(&c, &j, &mut batch).await.unwrap();
        assert!(batch.is_empty());
    }

    /// git really sends the credential, and only as a header.
    #[tokio::test]
    async fn git_authenticates_with_the_header() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
        let s = seen.clone();
        let app =
            axum::Router::new().fallback(move |uri: axum::http::Uri, h: axum::http::HeaderMap| {
                let s = s.clone();
                async move {
                    let auth = h
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    s.lock().unwrap().push((uri.to_string(), auth));
                    axum::http::StatusCode::NOT_FOUND
                }
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/acme/app.git", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let auth = super::super::git::auth_env(&url, "x-access-token", "ghs_secret");
        let u = url.clone();
        let err = tokio::task::spawn_blocking(move || default_branch(&u, &auth))
            .await
            .unwrap()
            .unwrap_err();
        assert!(!format!("{err:#}").contains("ghs_secret"));
        let seen = seen.lock().unwrap();
        assert!(
            seen.iter()
                .any(|(path, _)| path.starts_with("/acme/app.git/info/refs")),
            "{seen:?}"
        );
        use base64::Engine as _;
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("x-access-token:ghs_secret")
        );
        assert!(seen.iter().all(|(_, a)| *a == expected), "{seen:?}");
    }
}
