//! Scheduled and triggered jobs (`chanto runner`): agent tasks that start on
//! a cron or interval schedule, from a webhook, or on demand, each as an
//! ordinary background run (`runs/`), so they get the same isolation (their
//! own worktree and branch), budgets, verified mode, needs-input prompts,
//! notifications and web UI as `chanto run --bg`.
//!
//! - Jobs are defined in `$CHANTO_HOME/runner.toml` and in the
//!   `.chanto/runner.toml` of projects you enable (`chanto runner enable`;
//!   the project must be trusted, since its jobs run unattended).
//! - The scheduler lives in the daemon that hosts background runs
//!   (`chanto daemon start`): it rereads the job files every tick, so edits
//!   apply without a restart, and starts a run for each job that is due.
//! - Triggers: `chanto runner trigger <job>`, and `POST /runs/jobs/<job>/trigger`
//!   authenticated with the daemon's token or a per-job token (`chanto runner
//!   token <job>`), which GitHub can also use as its webhook secret.
//! - `chanto runner --once` is for external schedulers (cron, launchd,
//!   systemd timers, CI): it asks the daemon (started on demand) to start
//!   whatever is due, waits for those runs, and exits non-zero if one failed.
//!
//! A job never overlaps itself: while its last run is active it is skipped.

use crate::runs::manager::{Manager, NewRun};
use crate::runs::{Run, RunState};
use crate::util::Canonical as _;
use anyhow::{Context, Result, bail};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration as ChronoDuration, Local, NaiveDateTime, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the daemon looks for due jobs.
const TICK: Duration = Duration::from_secs(30);

// ------------------------------------------------------------- schedules

/// A parsed job schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// Every N seconds.
    Every(u64),
    /// A 5-field cron expression, evaluated in local time.
    Cron(Cron),
}

/// A 5-field cron expression: minute hour day-of-month month day-of-week.
///
/// Supported syntax: `*`, numbers, `a-b` ranges, `a,b,c` lists and `*/n` or
/// `a-b/n` steps. Day-of-week is `0-6` (Sunday = 0 and 7). When both
/// day-of-month and day-of-week are restricted, a day matches if *either* does,
/// as in Vixie cron.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minutes: Vec<u32>,
    hours: Vec<u32>,
    doms: Vec<u32>,
    months: Vec<u32>,
    dows: Vec<u32>,
    dom_restricted: bool,
    dow_restricted: bool,
}

fn parse_field(field: &str, min: u32, max: u32, name: &str) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (
                r,
                s.parse::<u32>()
                    .with_context(|| format!("bad step in {name} field: {part}"))?,
            ),
            None => (part, 1),
        };
        if step == 0 {
            bail!("step must be at least 1 in {name} field: {part}");
        }
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (
                a.trim()
                    .parse()
                    .with_context(|| format!("bad range in {name} field: {part}"))?,
                b.trim()
                    .parse()
                    .with_context(|| format!("bad range in {name} field: {part}"))?,
            )
        } else {
            let v: u32 = range
                .trim()
                .parse()
                .with_context(|| format!("bad value in {name} field: {part}"))?;
            // `5/15` means "from 5, every 15".
            (v, if step > 1 { max } else { v })
        };
        if lo > hi || lo < min || hi > max {
            bail!("{name} field out of range ({min}-{max}): {part}");
        }
        out.extend((lo..=hi).step_by(step as usize));
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Cron> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            bail!(
                "a cron schedule needs 5 fields (minute hour day month weekday), got {}: `{expr}`",
                fields.len()
            );
        }
        // Sunday is 0 or 7.
        let mut dows = parse_field(fields[4], 0, 7, "day-of-week")?;
        if dows.contains(&7) {
            dows.retain(|d| *d != 7);
            if !dows.contains(&0) {
                dows.insert(0, 0);
            }
        }
        Ok(Cron {
            minutes: parse_field(fields[0], 0, 59, "minute")?,
            hours: parse_field(fields[1], 0, 23, "hour")?,
            doms: parse_field(fields[2], 1, 31, "day-of-month")?,
            months: parse_field(fields[3], 1, 12, "month")?,
            dows,
            dom_restricted: fields[2] != "*",
            dow_restricted: fields[4] != "*",
        })
    }

    fn matches(&self, t: NaiveDateTime) -> bool {
        use chrono::Datelike;
        let dow = t.weekday().num_days_from_sunday();
        let day_ok = match (self.dom_restricted, self.dow_restricted) {
            (true, true) => self.doms.contains(&t.day()) || self.dows.contains(&dow),
            (true, false) => self.doms.contains(&t.day()),
            (false, true) => self.dows.contains(&dow),
            (false, false) => true,
        };
        day_ok
            && self.months.contains(&t.month())
            && self.hours.contains(&t.hour())
            && self.minutes.contains(&t.minute())
    }

    /// The first matching local time strictly after `from`, or `None` if the
    /// expression never matches (e.g. `0 0 30 2 *`).
    pub fn next_after(&self, from: DateTime<Local>) -> Option<DateTime<Local>> {
        use chrono::Datelike;
        let mut t = (from.naive_local() + ChronoDuration::minutes(1))
            .with_second(0)?
            .with_nanosecond(0)?;
        // Skip whole days and hours that can't match, so even a yearly
        // expression is found quickly; five years bounds impossible ones.
        let end = t + ChronoDuration::days(5 * 366);
        while t < end {
            let day_ok = self.months.contains(&t.month())
                && match (self.dom_restricted, self.dow_restricted) {
                    (true, true) => {
                        self.doms.contains(&t.day())
                            || self.dows.contains(&t.weekday().num_days_from_sunday())
                    }
                    (true, false) => self.doms.contains(&t.day()),
                    (false, true) => self.dows.contains(&t.weekday().num_days_from_sunday()),
                    (false, false) => true,
                };
            if !day_ok {
                t = (t.date() + ChronoDuration::days(1)).and_hms_opt(0, 0, 0)?;
                continue;
            }
            if !self.hours.contains(&t.hour()) {
                t = t.with_minute(0)? + ChronoDuration::hours(1);
                continue;
            }
            if self.matches(t) {
                // A time skipped by a DST change resolves to the earliest valid one.
                return Local.from_local_datetime(&t).earliest().or_else(|| {
                    Local
                        .from_local_datetime(&(t + ChronoDuration::hours(1)))
                        .earliest()
                });
            }
            t += ChronoDuration::minutes(1);
        }
        None
    }
}

/// An interval (`30s`, `10m`, `2h`, `1d`) or a 5-field cron expression.
pub fn parse_schedule(spec: &str) -> Result<Schedule> {
    let spec = spec.trim();
    if spec.is_empty() {
        bail!("empty schedule");
    }
    if spec.split_whitespace().count() == 5 {
        return Ok(Schedule::Cron(Cron::parse(spec)?));
    }
    // The unit is the last character, which needn't be one byte (`5分` panicked).
    let cut = spec.char_indices().last().map_or(0, |(i, _)| i);
    let (num, unit) = spec.split_at(cut);
    let n: u64 = num.trim().parse().with_context(|| {
        format!("bad schedule `{spec}` (use e.g. 30m, 2h, 1d or a cron expression)")
    })?;
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        other => bail!("unknown interval unit `{other}` in `{spec}` (use s, m, h or d)"),
    };
    let secs = n
        .checked_mul(scale)
        .with_context(|| format!("interval `{spec}` is too long"))?;
    // Every job is a whole agent run; a tighter loop is almost certainly a typo.
    if secs < 60 {
        bail!("interval `{spec}` is shorter than a minute");
    }
    Ok(Schedule::Every(secs))
}

/// When a job next fires. `last` is its last run (for cron jobs that never
/// ran: when the scheduler first saw them), so a slot missed while the daemon
/// was down fires once as soon as it is back. An interval job that never ran
/// is due right away.
pub fn next_due(
    schedule: &Schedule,
    last: Option<DateTime<Local>>,
    now: DateTime<Local>,
) -> Option<DateTime<Local>> {
    match schedule {
        Schedule::Every(secs) => Some(
            last.map(|t| t + ChronoDuration::seconds(*secs as i64))
                .unwrap_or(now),
        ),
        Schedule::Cron(cron) => cron.next_after(last.unwrap_or(now - ChronoDuration::minutes(1))),
    }
}

fn is_due(schedule: &Schedule, last: Option<DateTime<Local>>, now: DateTime<Local>) -> bool {
    next_due(schedule, last, now).is_some_and(|t| t <= now)
}

// -------------------------------------------------------------- job files

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    /// Unique across all job files; used in commands, URLs and run records.
    pub name: String,
    /// Interval or cron expression; without one the job only runs when triggered.
    #[serde(default)]
    pub schedule: Option<String>,
    /// The task for the agent.
    pub prompt: String,
    /// The checkout to run from: relative to the project for project jobs;
    /// required (absolute or `~/…`) for jobs in `$CHANTO_HOME/runner.toml`.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Run settings; unset ones come from `[background]`.
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub verify: Option<bool>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub max_minutes: Option<u64>,
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
    /// "goal": work on the prompt as a goal (criteria, then iterations until
    /// they pass) instead of a single task.
    #[serde(default)]
    pub kind: Option<String>,
    /// `false` pauses the schedule (triggers still work).
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Resolved when loaded: where the run starts, and the defining file.
    #[serde(skip)]
    pub repo: PathBuf,
    #[serde(skip)]
    pub file: PathBuf,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobFile {
    #[serde(default)]
    job: Vec<Job>,
}

/// `$CHANTO_HOME/runner/`: state, enabled projects, webhook tokens.
pub fn runner_dir() -> PathBuf {
    crate::config::home_dir().join("runner")
}

pub fn global_file() -> PathBuf {
    crate::config::home_dir().join("runner.toml")
}

pub fn project_file(repo: &Path) -> PathBuf {
    repo.join(".chanto").join("runner.toml")
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !n.starts_with('.')
}

fn expand_home(p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        Err(_) => p.to_path_buf(),
    }
}

/// Parse one job file. `project`: the project it belongs to (None: global).
fn read_jobs(path: &Path, project: Option<&Path>) -> Result<Vec<Job>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let file: JobFile = toml::from_str(&text).with_context(|| format!("{}", path.display()))?;
    let mut out = Vec::new();
    for mut job in file.job {
        let at = || format!("{}: job `{}`", path.display(), job.name);
        if !valid_name(&job.name) {
            bail!(
                "{}: job name `{}` must be letters, digits, `-`, `_` or `.`",
                path.display(),
                job.name
            );
        }
        if job.prompt.trim().is_empty() {
            bail!("{}: has no prompt", at());
        }
        if let Some(s) = &job.schedule {
            parse_schedule(s).with_context(at)?;
        }
        if let Some(m) = &job.mode
            && crate::permissions::Mode::parse(m).is_none()
        {
            bail!("{}: unknown mode `{m}`", at());
        }
        if let Some(k) = job.kind.as_deref().filter(|k| *k != "goal") {
            bail!("{}: unknown kind `{k}` (only \"goal\")", at());
        }
        job.repo = match (project, &job.cwd) {
            (Some(root), None) => root.to_path_buf(),
            (Some(root), Some(c)) => root.join(expand_home(c)),
            (None, Some(c)) if expand_home(c).is_absolute() => expand_home(c),
            (None, _) => bail!(
                "{}: needs `cwd`, the absolute path of the checkout to run in",
                at()
            ),
        };
        job.file = path.to_path_buf();
        out.push(job);
    }
    Ok(out)
}

fn projects_file() -> PathBuf {
    runner_dir().join("projects.json")
}

/// Projects whose `.chanto/runner.toml` the scheduler reads.
pub fn enabled_projects() -> Vec<PathBuf> {
    std::fs::read(projects_file())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_projects(list: &[PathBuf]) -> Result<()> {
    std::fs::create_dir_all(runner_dir())?;
    std::fs::write(projects_file(), serde_json::to_vec_pretty(list)?)?;
    Ok(())
}

/// The jobs everywhere, and what's wrong with any that couldn't be used.
#[derive(Default)]
pub struct Jobs {
    pub jobs: Vec<Job>,
    pub problems: Vec<String>,
}

impl Jobs {
    pub fn get(&self, name: &str) -> Option<&Job> {
        self.jobs.iter().find(|j| j.name == name)
    }
}

/// Load the global jobs, then each enabled project's (plus `extra`, a project
/// named on the command line). A project job overrides a global one of the
/// same name; between two projects the first keeps it.
pub fn load(extra: Option<&Path>) -> Jobs {
    let mut out = Jobs::default();
    match read_jobs(&global_file(), None) {
        Ok(j) => out.jobs = j,
        Err(e) => out.problems.push(format!("{e:#}")),
    }
    let global: Vec<String> = out.jobs.iter().map(|j| j.name.clone()).collect();
    let mut projects = enabled_projects();
    if let Some(p) = extra
        && !projects.iter().any(|q| q == p)
    {
        projects.push(p.to_path_buf());
    }
    for root in projects {
        let file = project_file(&root);
        if !file.exists() {
            if extra != Some(root.as_path()) {
                out.problems
                    .push(format!("{} has no .chanto/runner.toml", root.display()));
            }
            continue;
        }
        // Its jobs run unattended, so only a trusted project's count.
        if !crate::config::is_trusted(&root) {
            out.problems.push(format!("{}: the project isn't trusted, so its jobs are ignored (open chanto there and trust it)", root.display()));
            continue;
        }
        match read_jobs(&file, Some(&root)) {
            Ok(jobs) => {
                for job in jobs {
                    match out.jobs.iter().position(|j| j.name == job.name) {
                        Some(i)
                            if global.contains(&job.name) && out.jobs[i].file == global_file() =>
                        {
                            out.jobs[i] = job
                        }
                        Some(i) => out.problems.push(format!(
                            "job `{}` in {} is also defined in {}; using the latter",
                            job.name,
                            file.display(),
                            out.jobs[i].file.display()
                        )),
                        None => out.jobs.push(job),
                    }
                }
            }
            Err(e) => out.problems.push(format!("{e:#}")),
        }
    }
    out
}

/// `chanto runner enable`: schedule the jobs of the project at `dir`.
pub fn enable(dir: &Path) -> Result<PathBuf> {
    let root = crate::runs::git::toplevel(dir)?;
    let root = root.canonical().unwrap_or(root);
    if !crate::config::is_trusted(&root) {
        bail!(
            "{} isn't trusted. Its jobs would run unattended, so trust it first: run `chanto --trust` there (or open chanto there and accept)",
            root.display()
        );
    }
    read_jobs(&project_file(&root), Some(&root))?;
    let mut list = enabled_projects();
    if !list.contains(&root) {
        list.push(root.clone());
        save_projects(&list)?;
    }
    Ok(root)
}

pub fn disable(dir: &Path) -> Result<Option<PathBuf>> {
    let root = crate::runs::git::toplevel(dir)
        .map(|r| r.canonical().unwrap_or(r))
        .unwrap_or_else(|_| dir.to_path_buf());
    let mut list = enabled_projects();
    let before = list.len();
    list.retain(|p| p != &root);
    if list.len() == before {
        return Ok(None);
    }
    save_projects(&list)?;
    Ok(Some(root))
}

// ------------------------------------------------------------------ state

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct JobState {
    /// When the scheduler first saw the job (the start of a cron job's history).
    pub first_seen: Option<String>,
    pub last_run: Option<String>,
    pub last_run_id: Option<String>,
    /// Why the last attempt couldn't start a run.
    pub last_error: Option<String>,
    pub runs: u64,
}

fn parse_time(s: &Option<String>) -> Option<DateTime<Local>> {
    s.as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Local))
}

impl JobState {
    fn history_start(&self) -> Option<DateTime<Local>> {
        parse_time(&self.last_run).or_else(|| parse_time(&self.first_seen))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StateFile {
    pub jobs: BTreeMap<String, JobState>,
}

fn state_path() -> PathBuf {
    runner_dir().join("state.json")
}

pub fn load_state() -> StateFile {
    std::fs::read(state_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// When `job` next fires, as text.
fn next_label(job: &Job, st: Option<&JobState>, now: DateTime<Local>) -> String {
    let Some(spec) = &job.schedule else {
        return "on trigger".into();
    };
    if !job.enabled {
        return "paused".into();
    }
    let Ok(s) = parse_schedule(spec) else {
        return "invalid".into();
    };
    let last = match s {
        Schedule::Cron(_) => st.and_then(JobState::history_start),
        Schedule::Every(_) => st.and_then(|s| parse_time(&s.last_run)),
    };
    match next_due(&s, last, now) {
        Some(t) if t <= now => "due now".into(),
        Some(t) => t.format("%Y-%m-%d %H:%M").to_string(),
        None => "never".into(),
    }
}

// ---------------------------------------------------------- webhook tokens

fn tokens_file() -> PathBuf {
    runner_dir().join("tokens.json")
}

fn tokens() -> HashMap<String, String> {
    std::fs::read(tokens_file())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// The job's webhook token, created on first use (`rotate`: a new one).
pub fn job_token(job: &str, rotate: bool) -> Result<String> {
    let mut all = tokens();
    if let Some(t) = all.get(job).filter(|_| !rotate) {
        return Ok(t.clone());
    }
    let t = format!(
        "hjt_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    all.insert(job.to_string(), t.clone());
    std::fs::create_dir_all(runner_dir())?;
    let path = tokens_file();
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    // Owner-only: these let anyone who has one start a run.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    std::io::Write::write_all(&mut opts.open(&tmp)?, &serde_json::to_vec_pretty(&all)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(t)
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let (mut ipad, mut opad) = ([0x36u8; 64], [0x5cu8; 64]);
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let inner = Sha256::new()
        .chain_update(ipad)
        .chain_update(msg)
        .finalize();
    Sha256::new()
        .chain_update(opad)
        .chain_update(inner)
        .finalize()
        .into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether a webhook request carries `token`: as a bearer token, an
/// `X-Chanto-Token` header, a `token` query parameter, or a GitHub
/// `X-Hub-Signature-256` signature made with it as the secret.
fn carries_token(
    headers: &HeaderMap,
    query: &HashMap<String, String>,
    body: &[u8],
    token: &str,
) -> bool {
    let eq = |t: &str| crate::server::constant_time_eq(t.as_bytes(), token.as_bytes());
    let header = |name: &str| headers.get(name).and_then(|h| h.to_str().ok());
    header("authorization")
        .and_then(crate::server::bearer_token)
        .is_some_and(eq)
        || header("x-chanto-token").is_some_and(eq)
        || query.get("token").is_some_and(|t| eq(t))
        || header("x-hub-signature-256")
            .and_then(|h| h.strip_prefix("sha256="))
            .is_some_and(|sig| {
                crate::server::constant_time_eq(
                    sig.to_ascii_lowercase().as_bytes(),
                    hex(&hmac_sha256(token.as_bytes(), body)).as_bytes(),
                )
            })
}

// -------------------------------------------------------------- scheduler

/// The daemon side: fires due jobs as background runs.
pub struct Scheduler {
    m: Arc<Manager>,
    /// Read from disk on first use.
    state: Mutex<Option<StateFile>>,
    /// One pass over the jobs at a time (the timer, `--once`, triggers).
    pass: tokio::sync::Mutex<()>,
}

impl Scheduler {
    pub fn new(m: Arc<Manager>) -> Arc<Scheduler> {
        Arc::new(Scheduler {
            m,
            state: Mutex::new(None),
            pass: Default::default(),
        })
    }

    pub fn manager(&self) -> Arc<Manager> {
        self.m.clone()
    }

    /// Check for due jobs shortly after startup and then every tick, until the daemon stops.
    pub fn start(self: &Arc<Self>) {
        let s = self.clone();
        tokio::spawn(async move {
            let mut wait = Duration::from_secs(2);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = s.m.shutdown.cancelled() => return,
                }
                wait = TICK;
                let r = s.run_due(None, None, false, "schedule").await;
                for e in r["errors"].as_array().into_iter().flatten() {
                    eprintln!("chanto runner: {}", e.as_str().unwrap_or_default());
                }
            }
        });
    }

    fn save(&self, st: &StateFile) {
        let _ = std::fs::create_dir_all(runner_dir());
        let tmp = state_path().with_extension("json.tmp");
        if std::fs::write(&tmp, serde_json::to_vec_pretty(st).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, state_path());
        }
    }

    async fn jobs(extra: Option<PathBuf>) -> Jobs {
        tokio::task::spawn_blocking(move || load(extra.as_deref()))
            .await
            .unwrap_or_default()
    }

    /// The job's run that is still going, if any.
    fn active_run(&self, job: &str) -> Option<Run> {
        self.m
            .list()
            .into_iter()
            .find(|r| r.job.as_deref() == Some(job) && r.state.is_active())
    }

    /// Start a run for `job` now (unless its last one is still going).
    pub async fn fire(&self, job: &Job, trigger: &str) -> Result<Run> {
        if let Some(r) = self.active_run(&job.name) {
            bail!(
                "job {} is still running (run {}, {})",
                job.name,
                r.id,
                r.state.label()
            );
        }
        let req = NewRun {
            task: job.prompt.clone(),
            cwd: Some(job.repo.clone()),
            model: job.model.clone(),
            provider: job.provider.clone(),
            base_url: job.base_url.clone(),
            mode: job.mode.clone(),
            verify: job.verify,
            max_cost_usd: job.max_cost_usd,
            max_minutes: job.max_minutes,
            kind: job.kind.clone(),
            job: Some(job.name.clone()),
            trigger: Some(trigger.to_string()),
            ..Default::default()
        };
        let res = self.m.create(req).await;
        let mut guard = self.state.lock().unwrap();
        let st = guard.get_or_insert_with(load_state);
        let e = st.jobs.entry(job.name.clone()).or_default();
        // A job that can't start still counts as attempted, so a broken one
        // isn't retried every tick.
        e.last_run = Some(crate::runs::now());
        match &res {
            Ok(r) => {
                e.last_run_id = Some(r.id.clone());
                e.last_error = None;
                e.runs += 1;
            }
            Err(err) => e.last_error = Some(format!("{err:#}")),
        }
        self.save(st);
        res
    }

    /// Start every job that is due (`only`: just that job, schedule or not).
    /// `extra`: a project to include besides the enabled ones.
    pub async fn run_due(
        &self,
        extra: Option<PathBuf>,
        only: Option<String>,
        dry_run: bool,
        trigger: &str,
    ) -> Value {
        let _pass = self.pass.lock().await;
        let jobs = Self::jobs(extra).await;
        let now = Local::now();
        let mut due = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        {
            let mut guard = self.state.lock().unwrap();
            let st = guard.get_or_insert_with(load_state);
            let mut seen_new = false;
            for job in &jobs.jobs {
                let s = st.jobs.entry(job.name.clone()).or_default();
                if s.first_seen.is_none() {
                    s.first_seen = Some(now.to_rfc3339());
                    seen_new = true;
                }
                if let Some(o) = &only {
                    if *o == job.name {
                        due.push(job.clone());
                    }
                    continue;
                }
                let (Some(spec), true) = (&job.schedule, job.enabled) else {
                    continue;
                };
                let Ok(schedule) = parse_schedule(spec) else {
                    continue;
                };
                let last = match schedule {
                    Schedule::Cron(_) => s.history_start(),
                    Schedule::Every(_) => parse_time(&s.last_run),
                };
                if is_due(&schedule, last, now) {
                    due.push(job.clone());
                }
            }
            if seen_new && !dry_run {
                self.save(st);
            }
        }
        if let Some(o) = &only
            && due.is_empty()
        {
            errors.push(format!("no job named {o}"));
        }
        let (mut fired, mut skipped) = (Vec::new(), Vec::new());
        for job in due {
            if let Some(r) = self.active_run(&job.name) {
                skipped.push(
                    json!({"job": job.name, "reason": format!("still running (run {})", r.id)}),
                );
            } else if dry_run {
                skipped.push(json!({"job": job.name, "reason": "dry run"}));
            } else {
                match self.fire(&job, trigger).await {
                    Ok(r) => fired.push(json!({"job": job.name, "run": r.to_json()})),
                    Err(e) => errors.push(format!("job {}: {e:#}", job.name)),
                }
            }
        }
        json!({"fired": fired, "skipped": skipped, "errors": errors, "problems": jobs.problems})
    }

    /// `GET /runs/jobs`: every job with its schedule, next and last run.
    pub async fn list_json(&self, extra: Option<PathBuf>) -> Value {
        let jobs = Self::jobs(extra).await;
        let st = self
            .state
            .lock()
            .unwrap()
            .get_or_insert_with(load_state)
            .jobs
            .clone();
        let now = Local::now();
        let rows: Vec<Value> = jobs
            .jobs
            .iter()
            .map(|j| {
                let s = st.get(&j.name);
                let last = s.and_then(|s| s.last_run_id.clone()).and_then(|id| self.m.get(&id).ok());
                json!({"name": j.name, "schedule": j.schedule, "enabled": j.enabled, "repo": j.repo, "file": j.file,
                    "next": next_label(j, s, now), "last_run": s.and_then(|s| s.last_run.clone()),
                    "last_error": s.and_then(|s| s.last_error.clone()), "last": last.map(|r| r.to_json())})
            })
            .collect();
        json!({"jobs": rows, "problems": jobs.problems})
    }
}

// ------------------------------------------------------------------- HTTP

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({"error": msg.into()}))).into_response()
}

/// A project path from a request body (only accepted from the daemon's own clients).
fn project_of(body: &Value) -> Option<PathBuf> {
    body["project"]
        .as_str()
        .map(PathBuf::from)
        .and_then(|p| p.canonical().ok())
}

/// Routes behind the daemon's bearer token:
/// - `GET /runs/jobs[?project=<path>]` → `{jobs, problems}`
/// - `POST /runs/jobs/due` `{project?, job?, dry_run?}` → `{fired, skipped, errors, problems}`
pub fn router<S: Clone + Send + Sync + 'static>(s: Arc<Scheduler>) -> Router<S> {
    Router::new()
        .route("/runs/jobs", get(list_jobs))
        .route("/runs/jobs/due", post(due))
        .with_state(s)
}

async fn list_jobs(
    State(s): State<Arc<Scheduler>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let extra = q
        .get("project")
        .map(PathBuf::from)
        .and_then(|p| p.canonical().ok());
    Json(s.list_json(extra).await).into_response()
}

async fn due(State(s): State<Arc<Scheduler>>, body: Option<Json<Value>>) -> Response {
    let body = body.map(|b| b.0).unwrap_or_default();
    let only = body["job"].as_str().map(str::to_string);
    Json(
        s.run_due(project_of(&body), only, body["dry_run"] == true, "once")
            .await,
    )
    .into_response()
}

/// `POST /runs/jobs/{name}/trigger`, outside the bearer-token layer so
/// webhooks can call it: authenticated by the daemon's token or the job's own
/// (see [`carries_token`]). Answers 201 with the run, 409 while the job's
/// last run is still going. A GitHub `ping` is acknowledged without a run.
pub fn public_router<S: Clone + Send + Sync + 'static>(
    s: Arc<Scheduler>,
    server_token: Arc<String>,
) -> Router<S> {
    Router::new()
        .route(
            "/runs/jobs/{name}/trigger",
            post(trigger).layer(axum::extract::DefaultBodyLimit::max(10 << 20)),
        )
        .with_state((s, server_token))
}

async fn trigger(
    State((s, server_token)): State<(Arc<Scheduler>, Arc<String>)>,
    UrlPath(name): UrlPath<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let local = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(crate::server::bearer_token)
        .is_some_and(|t| crate::server::constant_time_eq(t.as_bytes(), server_token.as_bytes()));
    let hook = !local
        && valid_name(&name)
        && tokens()
            .get(&name)
            .is_some_and(|t| carries_token(&headers, &q, &body, t));
    if !local && !hook {
        return err(StatusCode::UNAUTHORIZED, "missing or wrong token");
    }
    if headers.get("x-github-event").and_then(|h| h.to_str().ok()) == Some("ping") {
        return Json(json!({"ok": true, "job": name})).into_response();
    }
    // Only the daemon's own clients may point at a project or say how they triggered.
    let req: Value = if local {
        serde_json::from_slice(&body).unwrap_or_default()
    } else {
        Value::Null
    };
    let jobs = Scheduler::jobs(project_of(&req)).await;
    let Some(job) = jobs.get(&name) else {
        let hint = jobs
            .problems
            .first()
            .map(|p| format!(" ({p})"))
            .unwrap_or_default();
        return err(StatusCode::NOT_FOUND, format!("no job named {name}{hint}"));
    };
    let how = if local {
        req["trigger"].as_str().unwrap_or("manual").to_string()
    } else {
        "webhook".into()
    };
    match s.fire(job, &how).await {
        Ok(r) => (StatusCode::CREATED, Json(r.to_json())).into_response(),
        Err(e) if e.to_string().contains("still running") => {
            err(StatusCode::CONFLICT, format!("{e:#}"))
        }
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

// -------------------------------------------------------------------- CLI

const USAGE: &str = "usage: chanto runner [list]              jobs, schedules, next and last runs
       chanto runner status              the daemon, enabled projects, problems, jobs
       chanto runner enable [dir]        schedule this project's .chanto/runner.toml jobs
       chanto runner disable [dir]       stop scheduling them
       chanto runner trigger <job> [--wait]  start a job now
       chanto runner logs <job> [-n N]   a job's runs, and the latest one in detail
       chanto runner token <job> [--rotate]  the job's webhook token (and how to use it)
       chanto runner --once [--job <job>] [--dry-run] [--no-wait]
                                          start what is due and wait for it (for cron, launchd, systemd, CI)
       chanto runner connect <url> [--registration-token <t>]  take runs from a chanto cloud queue
Jobs run as background runs in the chanto daemon (`chanto daemon start`); see `chanto runs`.";

/// The project the command was run in, if it is a git checkout.
fn here() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let root = crate::runs::git::toplevel(&cwd).ok()?;
    Some(root.canonical().unwrap_or(root))
}

fn short_path(p: &Path) -> String {
    let home = dirs::home_dir().unwrap_or_default();
    match p.strip_prefix(&home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => p.display().to_string(),
    }
}

/// The jobs table, from the files and saved state (no daemon needed).
pub fn table(jobs: &Jobs, here: Option<&Path>) -> String {
    if jobs.jobs.is_empty() {
        return format!(
            "No jobs. Define them in {} or a project's .chanto/runner.toml (then `chanto runner enable` there).",
            short_path(&global_file())
        );
    }
    let st = load_state();
    let store = crate::runs::Store::new(crate::runs::Store::default_dir());
    let enabled = enabled_projects();
    let now = Local::now();
    let mut s = format!(
        "{:<20}  {:<14}  {:<16}  {:<34}  {}\n",
        "JOB", "SCHEDULE", "NEXT", "LAST RUN", "PROJECT"
    );
    for j in &jobs.jobs {
        let js = st.jobs.get(&j.name);
        let last = match js {
            Some(js) if js.last_error.is_some() => format!(
                "{} couldn't start",
                parse_time(&js.last_run)
                    .map(|t| t.format("%m-%d %H:%M").to_string())
                    .unwrap_or_default()
            ),
            Some(js) => match (&js.last_run_id, parse_time(&js.last_run)) {
                (Some(id), Some(t)) => {
                    let state = store.load(id).map(|r| r.state.label()).unwrap_or("removed");
                    format!("{} {id} {state}", t.format("%m-%d %H:%M"))
                }
                _ => "-".into(),
            },
            None => "-".into(),
        };
        let mut project = short_path(&j.repo);
        let from_here = here.is_some_and(|h| j.file == project_file(h));
        if from_here && !enabled.iter().any(|p| Some(p.as_path()) == here) {
            project.push_str("  (not enabled)");
        }
        s.push_str(&format!(
            "{:<20}  {:<14}  {:<16}  {:<34}  {}\n",
            j.name,
            j.schedule.as_deref().unwrap_or("-"),
            next_label(j, js, now),
            last,
            project
        ));
    }
    for p in &jobs.problems {
        s.push_str(&format!("\n! {p}"));
    }
    s.trim_end().to_string()
}

async fn daemon_line() -> String {
    match crate::runs::cli::connect(false).await {
        Ok(c) => format!("Schedules are live: the chanto daemon is running at {}.", c.url),
        Err(_) => "The chanto daemon isn't running, so nothing fires on schedule: `chanto daemon start` (or let cron run `chanto runner --once`).".into(),
    }
}

async fn wait_for_runs(
    c: &crate::runs::cli::Client,
    mut ids: Vec<(String, String)>,
) -> Result<bool> {
    let mut ok = true;
    let mut told = std::collections::HashSet::new();
    while !ids.is_empty() {
        let mut still = Vec::new();
        for (job, id) in ids {
            let r = c.run(&id).await?;
            match r.state {
                RunState::Queued | RunState::Running => still.push((job, id)),
                RunState::NeedsInput => {
                    if told.insert(id.clone()) {
                        println!(
                            "job {job}: run {id} needs input; answer with `chanto attach {id}`"
                        );
                    }
                    still.push((job, id));
                }
                s => {
                    let verify = r
                        .verify
                        .as_ref()
                        .filter(|v| v.status != "off")
                        .map(|v| format!(", verify {}", v.status))
                        .unwrap_or_default();
                    println!(
                        "job {job}: run {id} {}{verify} ({}, {})",
                        s.label(),
                        crate::runs::fmt_duration(r.duration_secs()),
                        crate::util::usd(r.cost_usd)
                    );
                    if let Some(e) = &r.error {
                        println!("  {e}");
                    }
                    if let Some(res) = &r.result {
                        println!("  {}", crate::util::one_line(res, 200));
                    }
                    ok &= s == RunState::Done;
                }
            }
        }
        ids = still;
        if !ids.is_empty() {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    Ok(ok)
}

/// `chanto runner ...`
pub async fn cli(args: &[String]) -> Result<()> {
    // Before the flags below: `connect` has its own `--once`.
    if args.first().map(String::as_str) == Some("connect") {
        return crate::runs::pull::cli(&args[1..]).await;
    }
    let flag = |f: &str| args.iter().any(|a| a == f);
    let value = |f: &str| {
        args.iter()
            .position(|a| a == f)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    if flag("--once") {
        let c = crate::runs::cli::connect(true).await?;
        let body = json!({"project": here(), "job": value("--job"), "dry_run": flag("--dry-run")});
        let v = c.post("/runs/jobs/due", body).await?;
        for p in v["problems"].as_array().into_iter().flatten() {
            eprintln!(
                "{}",
                crate::ui::dim(&format!("! {}", p.as_str().unwrap_or_default()))
            );
        }
        for s in v["skipped"].as_array().into_iter().flatten() {
            println!(
                "job {}: skipped ({})",
                s["job"].as_str().unwrap_or("?"),
                s["reason"].as_str().unwrap_or("")
            );
        }
        let mut started = Vec::new();
        for f in v["fired"].as_array().into_iter().flatten() {
            let (job, id) = (
                f["job"].as_str().unwrap_or("?").to_string(),
                f["run"]["id"].as_str().unwrap_or("?").to_string(),
            );
            println!("job {job}: started background run {id} (chanto attach {id})");
            started.push((job, id));
        }
        let errors: Vec<String> = v["errors"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e.as_str().map(str::to_string))
            .collect();
        if started.is_empty()
            && errors.is_empty()
            && v["skipped"].as_array().is_none_or(|s| s.is_empty())
        {
            println!("No jobs are due.");
        }
        let ok = flag("--no-wait") || wait_for_runs(&c, started).await?;
        if !errors.is_empty() {
            bail!("{}", errors.join("\n"));
        }
        if !ok {
            bail!("a job's run did not finish cleanly");
        }
        return Ok(());
    }
    let sub = args.first().map(String::as_str).unwrap_or("list");
    let arg = args.get(1).filter(|a| !a.starts_with('-')).cloned();
    match sub {
        "list" | "ls" => {
            let here = here();
            println!("{}", table(&load(here.as_deref()), here.as_deref()));
        }
        "status" => {
            println!("{}", daemon_line().await);
            let projects = enabled_projects();
            if projects.is_empty() {
                println!("Enabled projects: none (`chanto runner enable` in a project)");
            } else {
                println!(
                    "Enabled projects: {}",
                    projects
                        .iter()
                        .map(|p| short_path(p))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            let here = here();
            println!("\n{}", table(&load(here.as_deref()), here.as_deref()));
        }
        "enable" => {
            let dir = arg.map(PathBuf::from).unwrap_or(std::env::current_dir()?);
            let root = enable(&dir)?;
            let n = read_jobs(&project_file(&root), Some(&root))?.len();
            println!(
                "Scheduling {n} job(s) from {}.",
                short_path(&project_file(&root))
            );
            // Schedules only fire while the daemon runs; start it now.
            match crate::runs::cli::connect(true).await {
                Ok(c) => println!(
                    "The chanto daemon is running at {}; `chanto runner list` shows what's next.",
                    c.url
                ),
                Err(e) => println!(
                    "Couldn't start the chanto daemon ({e:#}); `chanto daemon start` does."
                ),
            }
        }
        "disable" => {
            let dir = arg.map(PathBuf::from).unwrap_or(std::env::current_dir()?);
            match disable(&dir)? {
                Some(root) => println!("No longer scheduling the jobs in {}.", short_path(&root)),
                None => println!("{} wasn't enabled.", dir.display()),
            }
        }
        "trigger" | "run" => {
            let job =
                arg.ok_or_else(|| anyhow::anyhow!("usage: chanto runner trigger <job> [--wait]"))?;
            let c = crate::runs::cli::connect(true).await?;
            let r: Run = serde_json::from_value(
                c.post(
                    &format!("/runs/jobs/{job}/trigger"),
                    json!({"project": here(), "trigger": "manual"}),
                )
                .await?,
            )?;
            println!(
                "job {job}: started background run {} on branch {} (chanto attach {})",
                r.id, r.branch, r.id
            );
            if flag("--wait") && !wait_for_runs(&c, vec![(job, r.id)]).await? {
                bail!("the run did not finish cleanly");
            }
        }
        "logs" | "log" => {
            let job =
                arg.ok_or_else(|| anyhow::anyhow!("usage: chanto runner logs <job> [-n N]"))?;
            let n: usize = value("-n").and_then(|v| v.parse().ok()).unwrap_or(10);
            let daemon = crate::runs::cli::connect(false).await.ok();
            let mut runs: Vec<Run> = match &daemon {
                Some(c) => serde_json::from_value(c.get("/runs").await?["runs"].clone())
                    .unwrap_or_default(),
                None => crate::runs::Store::new(crate::runs::Store::default_dir()).list(),
            };
            runs.retain(|r| r.job.as_deref() == Some(job.as_str()));
            if runs.is_empty() {
                println!("Job {job} has no runs yet (or they were removed).");
                return Ok(());
            }
            runs.truncate(n);
            println!("{}\n", crate::runs::cli::table(&runs));
            let latest = &runs[0];
            let detail = match &daemon {
                Some(c) => c.get(&format!("/runs/{}", latest.id)).await?,
                None => {
                    let store = crate::runs::Store::new(crate::runs::Store::default_dir());
                    let mut v = latest.to_json();
                    v["last_messages"] =
                        json!(crate::runs::last_messages(&store.events(&latest.id), 3));
                    v
                }
            };
            println!("{}", crate::runs::cli::detail_text(&detail));
        }
        "token" => {
            let job =
                arg.ok_or_else(|| anyhow::anyhow!("usage: chanto runner token <job> [--rotate]"))?;
            let jobs = load(here().as_deref());
            if jobs.get(&job).is_none() {
                bail!("no job named {job}; `chanto runner list` shows them");
            }
            let t = job_token(&job, flag("--rotate"))?;
            let url = crate::runs::cli::connect(false)
                .await
                .map(|c| c.url)
                .unwrap_or_else(|_| "http://<daemon>".into());
            println!(
                "Webhook token for job {job}{}:\n  {t}\n\nTrigger it with:\n  curl -X POST -H 'Authorization: Bearer {t}' {url}/runs/jobs/{job}/trigger\n\
                 For a GitHub webhook, use {url}/runs/jobs/{job}/trigger as the payload URL and the token as its secret.\n\
                 The local daemon listens on loopback only: expose it (a tunnel, a reverse proxy, or `chanto serve --runs` on a server) for outside callers.",
                if flag("--rotate") {
                    " (the old one no longer works)"
                } else {
                    ""
                }
            );
        }
        "help" | "-h" | "--help" => println!("{USAGE}"),
        other => bail!("unknown subcommand `{other}`\n{USAGE}"),
    }
    Ok(())
}

/// `/schedule [list | run <job>]` in a session.
pub async fn slash(root: &Path, arg: &str) -> String {
    let words: Vec<&str> = arg.split_whitespace().collect();
    let root = root.to_path_buf();
    match words.as_slice() {
        [] | ["list"] => {
            let r = root.clone();
            let jobs = tokio::task::spawn_blocking(move || load(Some(&r)))
                .await
                .unwrap_or_default();
            format!(
                "```\n{}\n```\n{}\n`/schedule run <job>` starts one now · `chanto runner` manages them",
                table(&jobs, Some(&root)),
                daemon_line().await
            )
        }
        ["run" | "trigger", job] => {
            let res: Result<Run> = async {
                let c = crate::runs::cli::connect(true).await?;
                Ok(serde_json::from_value(
                    c.post(
                        &format!("/runs/jobs/{job}/trigger"),
                        json!({"project": root, "trigger": "manual"}),
                    )
                    .await?,
                )?)
            }
            .await;
            match res {
                Ok(r) => format!(
                    "Started job `{job}` as background run `{}`. `/runs attach {}` follows it here.",
                    r.id, r.id
                ),
                Err(e) => format!("Couldn't start job {job}: {e:#}"),
            }
        }
        _ => "Usage: `/schedule [list | run <job>]`".into(),
    }
}

#[cfg(test)]
mod cli_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Local> {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
            .unwrap()
            .and_local_timezone(Local)
            .unwrap()
    }

    fn fmt(t: Option<DateTime<Local>>) -> String {
        t.unwrap().format("%Y-%m-%d %H:%M").to_string()
    }

    #[test]
    fn intervals() {
        assert_eq!(parse_schedule("90s").unwrap(), Schedule::Every(90));
        assert_eq!(parse_schedule("10m").unwrap(), Schedule::Every(600));
        assert_eq!(parse_schedule("2h").unwrap(), Schedule::Every(7200));
        assert_eq!(parse_schedule("1d").unwrap(), Schedule::Every(86400));
        assert!(parse_schedule("30s").is_err(), "under a minute");
        assert!(parse_schedule("0m").is_err());
        // A multibyte unit or a huge count is an error, not a panic.
        assert!(parse_schedule("5分").is_err());
        assert!(parse_schedule("é").is_err());
        assert!(parse_schedule("18446744073709551615d").is_err());
        assert!(parse_schedule("5x").is_err());
        assert!(parse_schedule("").is_err());
    }

    #[test]
    fn cron_fields() {
        let c = Cron::parse("*/15 9-17 * * 1-5").unwrap();
        assert_eq!(c.minutes, vec![0, 15, 30, 45]);
        assert_eq!(c.hours, (9..=17).collect::<Vec<_>>());
        assert_eq!(c.dows, vec![1, 2, 3, 4, 5]);
        let c = Cron::parse("5/20 0,12 1 */3 *").unwrap();
        assert_eq!(c.minutes, vec![5, 25, 45]);
        assert_eq!(c.hours, vec![0, 12]);
        assert_eq!(c.months, vec![1, 4, 7, 10]);
        assert_eq!(
            Cron::parse("0 9 * * 5-7").unwrap().dows,
            vec![0, 5, 6],
            "7 is Sunday"
        );
        assert!(Cron::parse("0 0 * *").is_err());
        assert!(Cron::parse("60 0 * * *").is_err());
        assert!(Cron::parse("0 24 * * *").is_err());
        assert!(Cron::parse("*/0 0 * * *").is_err());
    }

    #[test]
    fn cron_next_times() {
        // 2026-10-11 is a Sunday.
        assert_eq!(
            fmt(Cron::parse("0 9 * * 0")
                .unwrap()
                .next_after(at("2026-10-11 00:00:00"))),
            "2026-10-11 09:00"
        );
        assert_eq!(
            fmt(Cron::parse("0 9 * * 7")
                .unwrap()
                .next_after(at("2026-10-11 00:00:00"))),
            "2026-10-11 09:00"
        );
        let c = Cron::parse("30 3 * * *").unwrap();
        assert_eq!(
            fmt(c.next_after(at("2026-06-01 08:00:00"))),
            "2026-06-02 03:30"
        );
        assert_eq!(
            fmt(c.next_after(at("2026-06-01 03:00:00"))),
            "2026-06-01 03:30"
        );
        assert_eq!(
            fmt(c.next_after(at("2026-06-01 03:30:00"))),
            "2026-06-02 03:30",
            "strictly after"
        );
        assert_eq!(
            fmt(Cron::parse("0 0 1 * *")
                .unwrap()
                .next_after(at("2026-06-15 12:00:00"))),
            "2026-07-01 00:00"
        );
        assert_eq!(
            fmt(Cron::parse("0 0 1 1 *")
                .unwrap()
                .next_after(at("2026-06-15 12:00:00"))),
            "2027-01-01 00:00"
        );
        assert_eq!(
            fmt(Cron::parse("0 0 29 2 *")
                .unwrap()
                .next_after(at("2026-03-01 00:00:00"))),
            "2028-02-29 00:00"
        );
        // Vixie cron: "the 13th OR a Friday". 2026-10-13 is a Tuesday.
        let c = Cron::parse("0 0 13 * 5").unwrap();
        assert_eq!(
            fmt(c.next_after(at("2026-10-12 00:00:00"))),
            "2026-10-13 00:00"
        );
        assert_eq!(
            fmt(c.next_after(at("2026-10-13 01:00:00"))),
            "2026-10-16 00:00"
        );
        assert!(
            Cron::parse("0 0 30 2 *")
                .unwrap()
                .next_after(at("2026-01-01 00:00:00"))
                .is_none(),
            "Feb 30 never comes"
        );
    }

    #[test]
    fn due_times() {
        let now = at("2026-06-01 12:00:00");
        let every = Schedule::Every(600);
        assert!(
            is_due(&every, None, now),
            "an interval job that never ran is due"
        );
        assert!(!is_due(
            &every,
            Some(now - ChronoDuration::seconds(100)),
            now
        ));
        assert!(is_due(
            &every,
            Some(now - ChronoDuration::seconds(601)),
            now
        ));
        let cron = Schedule::Cron(Cron::parse("0 3 * * *").unwrap());
        assert!(
            !is_due(&cron, None, now),
            "a new cron job waits for its slot"
        );
        assert!(!is_due(&cron, Some(at("2026-06-01 03:00:00")), now));
        assert!(
            is_due(&cron, Some(at("2026-05-30 03:00:00")), now),
            "a missed slot fires once"
        );
        assert!(
            is_due(&cron, Some(at("2026-05-31 20:00:00")), now),
            "first seen before last night's slot"
        );
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // A key longer than the block size is hashed first (test case 6).
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn webhook_tokens_in_any_of_their_forms() {
        let t = "hjt_secret";
        let body = br#"{"action":"opened"}"#;
        let h = |pairs: &[(&'static str, String)]| {
            let mut m = HeaderMap::new();
            for (k, v) in pairs {
                m.insert(*k, v.parse().unwrap());
            }
            m
        };
        let none = HashMap::new();
        assert!(carries_token(
            &h(&[("authorization", format!("Bearer {t}"))]),
            &none,
            body,
            t
        ));
        assert!(carries_token(
            &h(&[("x-chanto-token", t.into())]),
            &none,
            body,
            t
        ));
        assert!(carries_token(
            &HeaderMap::new(),
            &HashMap::from([("token".to_string(), t.to_string())]),
            body,
            t
        ));
        let sig = format!("sha256={}", hex(&hmac_sha256(t.as_bytes(), body)));
        assert!(carries_token(
            &h(&[("x-hub-signature-256", sig.clone())]),
            &none,
            body,
            t
        ));
        assert!(
            !carries_token(&h(&[("x-hub-signature-256", sig)]), &none, b"tampered", t),
            "the signature covers the body"
        );
        assert!(!carries_token(
            &h(&[("authorization", "Bearer nope".into())]),
            &none,
            body,
            t
        ));
        assert!(!carries_token(&HeaderMap::new(), &none, body, t));
    }

    #[test]
    fn job_files_parse_strictly() {
        let dir =
            std::env::temp_dir().join(format!("chanto-jobs-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("runner.toml");
        let w = |s: &str| std::fs::write(&f, s).unwrap();
        w(
            "[[job]]\nname = \"nightly\"\nschedule = \"0 3 * * *\"\nprompt = \"Review\"\nverify = true\nmax_minutes = 20\n\n[[job]]\nname = \"triage\"\nprompt = \"Triage\"\ncwd = \"sub\"\nenabled = false\n",
        );
        let jobs = read_jobs(&f, Some(&dir)).unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(
            (
                jobs[0].repo.clone(),
                jobs[0].verify,
                jobs[0].max_minutes,
                jobs[0].enabled
            ),
            (dir.clone(), Some(true), Some(20), true)
        );
        assert_eq!(
            (jobs[1].repo.clone(), jobs[1].enabled),
            (dir.join("sub"), false)
        );
        // Global jobs must say where to run.
        assert!(format!("{:#}", read_jobs(&f, None).unwrap_err()).contains("needs `cwd`"));
        w("[[job]]\nname = \"x\"\nprompt = \"p\"\ntimeout = 5\n");
        assert!(
            format!("{:#}", read_jobs(&f, Some(&dir)).unwrap_err()).contains("timeout"),
            "unknown keys are errors, not ignored"
        );
        w("[[job]]\nname = \"a b\"\nprompt = \"p\"\n");
        assert!(read_jobs(&f, Some(&dir)).is_err());
        w("[[job]]\nname = \"x\"\nprompt = \"p\"\nschedule = \"every day\"\n");
        assert!(read_jobs(&f, Some(&dir)).is_err());
        w("[[job]]\nname = \"x\"\nprompt = \"p\"\nmode = \"reckless\"\n");
        assert!(read_jobs(&f, Some(&dir)).is_err());
        // A job can run its prompt as a goal; other kinds are refused.
        w("[[job]]\nname = \"g\"\nprompt = \"p\"\nkind = \"goal\"\n");
        assert_eq!(
            read_jobs(&f, Some(&dir)).unwrap()[0].kind.as_deref(),
            Some("goal")
        );
        w("[[job]]\nname = \"g\"\nprompt = \"p\"\nkind = \"epic\"\n");
        assert!(
            format!("{:#}", read_jobs(&f, Some(&dir)).unwrap_err()).contains("unknown kind `epic`")
        );
        assert!(
            read_jobs(&dir.join("missing.toml"), None)
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
