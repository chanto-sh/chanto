//! Teams: a lead agent splits a task into independent sub-tasks (each owning
//! some files, to keep them from colliding) and runs them in parallel as
//! background runs. Each sub-task gets its own worktree and branch, its share
//! of the budget, verified mode and needs-input prompts, and shows up in the
//! web UI under the team. The lead reviews every result, merges the accepted
//! ones in plan order onto the team branch `chanto/team/<id>` (a sub-task
//! that conflicts is re-delegated once, on top of the work merged so far),
//! runs the project's checks on the merged result, and reports.
//!
//! The record lives in `<chanto home>/teams/<id>/team.json`, so `chanto
//! team resume <id>` picks up a team whose lead was interrupted: sub-tasks
//! keep running in the daemon meanwhile.

use crate::agent::{Agent, Env, Event, TurnEnd};
use crate::runs::{Run, RunState, git};
use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// `[team]` in config.toml.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TeamConfig {
    /// Sub-task runs at once (default 3; `--agents N`).
    pub agents: Option<usize>,
    /// Total budget in USD, split evenly across the sub-tasks (default 6).
    pub max_cost_usd: Option<f64>,
}

impl TeamConfig {
    pub fn merge(&mut self, o: TeamConfig) {
        if o.agents.is_some() {
            self.agents = o.agents;
        }
        if o.max_cost_usd.is_some() {
            self.max_cost_usd = o.max_cost_usd;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SubState {
    #[default]
    Planned,
    Running,
    /// Reviewed and accepted, waiting to be merged.
    Accepted,
    /// Rejected in review (after its retry).
    Rejected,
    Failed,
    Merged,
    /// Still conflicted after being re-delegated on top of the merged work.
    Conflict,
}

impl SubState {
    fn label(self) -> &'static str {
        match self {
            SubState::Planned => "planned",
            SubState::Running => "running",
            SubState::Accepted => "accepted",
            SubState::Rejected => "rejected",
            SubState::Failed => "failed",
            SubState::Merged => "merged",
            SubState::Conflict => "conflict",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            SubState::Planned => "…",
            SubState::Running => "●",
            SubState::Accepted => "✓",
            SubState::Merged => "✓",
            _ => "✗",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubTask {
    pub id: String,
    pub title: String,
    /// Complete instructions for the agent that does it.
    pub task: String,
    /// Files or directories (globs) it owns.
    #[serde(default)]
    pub owns: Vec<String>,
    #[serde(default)]
    pub state: SubState,
    /// Its runs, oldest first (a retry or a conflict re-delegation adds one).
    #[serde(default)]
    pub runs: Vec<String>,
    /// Feedback for the next attempt, or why it failed.
    #[serde(default)]
    pub note: Option<String>,
    /// Starting point of the next attempt (the merged work, after a conflict).
    #[serde(default)]
    pub start: Option<String>,
    #[serde(default)]
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TeamState {
    Running,
    Merging,
    Done,
    /// The lead stopped (Esc, closed terminal); `chanto team resume` continues.
    Interrupted,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckOutcome {
    pub command: String,
    pub ok: bool,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Team {
    pub id: String,
    pub task: String,
    /// The repository the team works on.
    pub repo: PathBuf,
    pub state: TeamState,
    pub created: String,
    /// Sub-task runs at once.
    pub agents: usize,
    pub budget_usd: f64,
    /// Where every sub-task starts: HEAD plus a snapshot of uncommitted changes.
    pub base: String,
    pub branch: String,
    /// The team branch's worktree, where merges and checks happen.
    pub worktree: PathBuf,
    pub subtasks: Vec<SubTask>,
    /// Options for the sub-task runs (model, provider, base_url, mode, verify, sandbox).
    #[serde(default)]
    pub run_options: Value,
    #[serde(default)]
    pub checks: Vec<CheckOutcome>,
}

pub fn teams_dir() -> PathBuf {
    crate::config::home_dir().join("teams")
}

impl Team {
    fn dir(&self) -> PathBuf {
        teams_dir().join(&self.id)
    }

    pub fn save(&self) -> Result<()> {
        let dir = self.dir();
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join("team.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, dir.join("team.json"))?;
        Ok(())
    }

    pub fn load(id: &str) -> Result<Team> {
        let all = list();
        let m: Vec<&Team> = all.iter().filter(|t| t.id.starts_with(id)).collect();
        match m.as_slice() {
            [t] if !id.is_empty() => Ok((*t).clone()),
            [] | [_] => bail!("no team {id}"),
            _ => bail!("team id {id} is ambiguous"),
        }
    }

    fn spent(&self) -> f64 {
        self.subtasks.iter().map(|s| s.cost_usd).sum()
    }

    /// Each sub-task's share of the budget.
    fn share(&self) -> f64 {
        self.budget_usd / self.subtasks.len().max(1) as f64
    }

    /// The status line and a line per sub-task.
    pub fn status_lines(&self, needs_input: &[String]) -> Vec<String> {
        let n = |s: SubState| self.subtasks.iter().filter(|t| t.state == s).count();
        let mut head = format!(
            "⧉ team {}/{} merged",
            n(SubState::Merged),
            self.subtasks.len()
        );
        if n(SubState::Running) > 0 {
            head.push_str(&format!(" · {} running", n(SubState::Running)));
        }
        if !needs_input.is_empty() {
            head.push_str(&format!(" · {} need input", needs_input.len()));
        }
        let mut lines = vec![head];
        for t in &self.subtasks {
            let run = t
                .runs
                .last()
                .map(|r| format!(" · run {r}"))
                .unwrap_or_default();
            let wait = if t.runs.last().is_some_and(|r| needs_input.contains(r)) {
                " · ⚠ needs input (/runs attach)"
            } else {
                ""
            };
            lines.push(format!(
                "  {} {} {} · {}{run}{wait}",
                t.state.icon(),
                t.id,
                crate::util::one_line(&t.title, 50),
                t.state.label()
            ));
        }
        lines
    }

    /// A finished team as a pseudo-run, so `runs::git::apply` can bring the
    /// team branch onto the user's current branch.
    fn as_run(&self) -> Run {
        let mut v = json!({
            "id": format!("team-{}", self.id), "task": self.task, "state": "done", "created": self.created,
            "source": {"kind": "local", "path": self.repo, "head": self.base},
            "worktree": self.worktree, "branch": self.branch, "base": self.base, "settings": {},
        });
        v["target"] = json!("local");
        serde_json::from_value(v).expect("team as run")
    }
}

pub fn list() -> Vec<Team> {
    let mut v: Vec<Team> = std::fs::read_dir(teams_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| std::fs::read(e.path().join("team.json")).ok())
        .filter_map(|b| serde_json::from_slice(&b).ok())
        .collect();
    v.sort_by(|a, b| b.created.cmp(&a.created));
    v
}

// ------------------------------------------------------------------ run host

/// Where sub-task runs are hosted: the daemon over HTTP, or a manager in
/// this process (the server itself, and tests).
#[async_trait]
pub trait Host: Send + Sync {
    async fn create(&self, body: Value) -> Result<Run>;
    async fn get(&self, id: &str) -> Result<Run>;
    async fn stop(&self, id: &str) -> Result<()>;
}

#[async_trait]
impl Host for crate::runs::cli::Client {
    async fn create(&self, body: Value) -> Result<Run> {
        Ok(serde_json::from_value(self.post("/runs", body).await?)?)
    }
    async fn get(&self, id: &str) -> Result<Run> {
        self.run(id).await
    }
    async fn stop(&self, id: &str) -> Result<()> {
        self.post(&format!("/runs/{id}/stop"), json!({}))
            .await
            .map(|_| ())
    }
}

#[async_trait]
impl Host for Arc<crate::runs::manager::Manager> {
    async fn create(&self, body: Value) -> Result<Run> {
        crate::runs::manager::Manager::create(self, serde_json::from_value(body)?).await
    }
    async fn get(&self, id: &str) -> Result<Run> {
        crate::runs::manager::Manager::get(self, id)
    }
    async fn stop(&self, id: &str) -> Result<()> {
        crate::runs::manager::Manager::stop(self, id).map(|_| ())
    }
}

// ------------------------------------------------------------------ control

/// The team this process is leading, for `/team status|stop`.
struct Control {
    id: String,
    stop: AtomicBool,
    lines: Mutex<Vec<String>>,
}

static ACTIVE: Mutex<Option<Arc<Control>>> = Mutex::new(None);

/// `/team status|stop|list` (immediate, even while the team runs).
pub async fn command(root: &Path, arg: &str) -> String {
    let active = ACTIVE.lock().unwrap().clone();
    match arg.split_whitespace().next().unwrap_or("status") {
        "stop" => match active {
            Some(c) => {
                c.stop.store(true, Ordering::SeqCst);
                format!("Stopping team `{}` and its running sub-tasks.", c.id)
            }
            None => "No team is running in this session.".into(),
        },
        "list" => {
            let teams: Vec<Team> = list().into_iter().filter(|t| t.repo == root).collect();
            if teams.is_empty() {
                return "No teams for this project. `/team <task>` starts one.".into();
            }
            let mut s = String::from("```\n");
            for t in teams.iter().take(20) {
                let merged = t.subtasks.iter().filter(|s| s.state == SubState::Merged).count();
                s.push_str(&format!("{:<8} {:<11} {:>5}  {}\n", t.id, format!("{:?}", t.state).to_lowercase(), format!("{merged}/{}", t.subtasks.len()), crate::util::one_line(&t.task, 60)));
            }
            s.push_str("```");
            s
        }
        _ => match active {
            Some(c) => format!("Team `{}`:\n```\n{}\n```\n`/team stop` stops it and its running sub-tasks.", c.id, c.lines.lock().unwrap().join("\n")),
            None => match list().into_iter().find(|t| t.repo == root) {
                Some(t) => format!("Last team `{}` ({:?}):\n```\n{}\n```", t.id, t.state, t.status_lines(&[]).join("\n")),
                None => "No team yet. `/team <task>` plans parallel sub-tasks, runs them in their own worktrees, then reviews and merges them.".into(),
            },
        },
    }
}

// ------------------------------------------------------------------ planning

fn plan_prompt(task: &str, agents: usize, max: usize) -> String {
    format!(
        r#"Team mode, step 1: plan the work for a team. Do NOT change anything.

The user wants: {task}

You lead a team of {agents} agents working in parallel, each in its own copy of the repository (a git worktree) on its own branch; their branches are merged afterwards. Explore the codebase (read-only), then split the work into independent sub-tasks that can be done at the same time without editing the same files:
- 1 to {max} sub-tasks. Fewer is better when the work doesn't split cleanly; one sub-task is fine.
- Give each sub-task the files or directories it owns (paths or globs like `src/parser/**`). No two sub-tasks may own the same file; a shared file (a module list, a changelog) belongs to exactly one of them.
- Each `task` is complete, self-contained instructions for an agent that sees nothing else: what to build, where, how to test it, and what not to touch.
- Order them so the ones others build on come first: they are merged in this order.

Reply with only this JSON:
{{"subtasks": [{{"id": "t1", "title": "<short title>", "task": "<full instructions>", "owns": ["<path or glob>"]}}]}}"#
    )
}

/// The JSON object in a model reply (fenced or bare).
fn json_in(text: &str) -> Option<Value> {
    let body = match text.find("```") {
        Some(i) => {
            let rest = &text[i + 3..];
            let rest = rest.trim_start_matches(|c: char| c.is_ascii_alphabetic());
            &rest[..rest.find("```").unwrap_or(rest.len())]
        }
        None => text,
    };
    let start = body.find('{')?;
    let end = body.rfind('}').map(|e| e + 1).unwrap_or(body.len());
    serde_json::from_str(crate::llm::repair_json(&body[start..end.max(start)]).trim()).ok()
}

/// Sub-tasks from the lead's reply.
pub fn parse_plan(text: &str, max: usize) -> Result<Vec<SubTask>> {
    let v = json_in(text).ok_or_else(|| anyhow::anyhow!("the plan isn't JSON"))?;
    let items = v["subtasks"]
        .as_array()
        .cloned()
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    let mut out: Vec<SubTask> = Vec::new();
    for (i, it) in items.iter().enumerate().take(max) {
        let task = it["task"].as_str().unwrap_or("").trim().to_string();
        if task.is_empty() {
            continue;
        }
        let mut id = it["id"]
            .as_str()
            .map(|s| s.trim().to_string())
            .filter(|s| {
                !s.is_empty()
                    && s.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            })
            .unwrap_or_else(|| format!("t{}", i + 1));
        if out.iter().any(|t| t.id == id) {
            id = format!("t{}", i + 1);
        }
        let title = it["title"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| crate::util::one_line(&task, 60));
        let owns = it["owns"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|o| o.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        out.push(SubTask {
            id,
            title,
            task,
            owns,
            state: SubState::Planned,
            runs: vec![],
            note: None,
            start: None,
            cost_usd: 0.0,
        });
    }
    if out.is_empty() {
        bail!("the plan has no sub-tasks");
    }
    Ok(out)
}

/// Pairs of sub-tasks whose file ownership overlaps.
pub fn overlaps(subtasks: &[SubTask]) -> Vec<(String, String, String)> {
    let prefix = |g: &str| {
        g.trim_start_matches("./")
            .split(['*', '?', '['])
            .next()
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string()
    };
    let mut out = Vec::new();
    for (i, a) in subtasks.iter().enumerate() {
        for b in &subtasks[i + 1..] {
            for ga in &a.owns {
                for gb in &b.owns {
                    let (pa, pb) = (prefix(ga), prefix(gb));
                    let under = |x: &str, y: &str| {
                        x == y || y.is_empty() || x.starts_with(&format!("{y}/"))
                    };
                    if under(&pa, &pb) || under(&pb, &pa) {
                        out.push((
                            a.id.clone(),
                            b.id.clone(),
                            if ga == gb {
                                ga.clone()
                            } else {
                                format!("{ga} / {gb}")
                            },
                        ));
                    }
                }
            }
        }
    }
    out
}

pub struct TeamOpts {
    pub agents: usize,
    pub budget_usd: f64,
    /// Options for the sub-task runs: model, provider, base_url, mode, verify, sandbox.
    pub run_options: Value,
}

/// `/team <task>`: plan with this session's agent, host the runs in the daemon.
pub async fn run_here(
    agent: &mut Agent,
    task: &str,
    agents: Option<usize>,
    cancel: CancellationToken,
) -> Result<String> {
    if task.trim().is_empty() {
        return Ok("Usage: `/team <task>`: the lead plans parallel sub-tasks, runs each as a background run in its own worktree, then reviews, merges and checks the result. `/team status`, `/team stop`.".into());
    }
    let cfg = &agent.env.config.team;
    let opts = TeamOpts {
        agents: agents.or(cfg.agents).unwrap_or(3).max(1),
        budget_usd: cfg.max_cost_usd.unwrap_or(6.0),
        run_options: json!({"model": agent.env.model()}),
    };
    let root = agent.env.root.clone();
    git::toplevel(&root)?;
    let client = crate::runs::cli::connect(true).await?;
    start(agent, task, opts, &client, cancel).await
}

/// Plan, get approval, then run the team to the end. Returns the report.
pub async fn start(
    agent: &mut Agent,
    task: &str,
    opts: TeamOpts,
    host: &dyn Host,
    cancel: CancellationToken,
) -> Result<String> {
    let env = agent.env.clone();
    let repo = git::toplevel(&env.root)?;
    let max = (opts.agents * 2).clamp(2, 8);
    env.emit(Event::Status {
        key: "team".into(),
        lines: vec!["⧉ team: planning".into()],
    });
    let prev = env.mode();
    env.set_mode(crate::permissions::Mode::Plan);
    let mut end = agent
        .run_turn(&plan_prompt(task, opts.agents, max), vec![], cancel.clone())
        .await;
    let mut plan = match &end {
        Ok(TurnEnd::Done(t)) => parse_plan(t, max),
        _ => Err(anyhow::anyhow!("interrupted")),
    };
    if plan.is_err() && matches!(end, Ok(TurnEnd::Done(_))) {
        end = agent
            .run_turn(
                "That plan couldn't be read. Reply with only the JSON object, nothing else.",
                vec![],
                cancel.clone(),
            )
            .await;
        if let Ok(TurnEnd::Done(t)) = &end {
            plan = parse_plan(t, max);
        }
    }
    env.set_mode(prev);
    agent.save_session();
    env.emit(Event::Status {
        key: "team".into(),
        lines: vec![],
    });
    let subtasks = match (end?, plan) {
        (TurnEnd::Interrupted, _) => bail!("interrupted while planning"),
        (_, p) => p?,
    };

    let mut q = format!(
        "**Team plan** for: {task}\n\n{} sub-task(s), {} at a time, budget ${:.2} (${:.2} each):\n",
        subtasks.len(),
        opts.agents,
        opts.budget_usd,
        opts.budget_usd / subtasks.len() as f64
    );
    for t in &subtasks {
        q.push_str(&format!(
            "- **{}** {}{}\n",
            t.id,
            t.title,
            if t.owns.is_empty() {
                String::new()
            } else {
                format!(" — owns `{}`", t.owns.join("`, `"))
            }
        ));
    }
    let clash = overlaps(&subtasks);
    if !clash.is_empty() {
        q.push_str("\n⚠ Overlapping ownership (these may conflict when merged): ");
        q.push_str(
            &clash
                .iter()
                .map(|(a, b, g)| format!("{a} and {b} ({g})"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        q.push('\n');
    }
    // Spend guard: what the sub-task runs may cost; the team budget caps it.
    let spec = opts.run_options["model"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| env.model());
    let shape = crate::spend::Shape::new(subtasks.len(), env.spend.calls_per_agent(), 20_000);
    let est = crate::spend::estimate(
        shape,
        &spec,
        &crate::spend::pricer(&env, &spec),
        env.spend.plan_rate(),
    );
    q.push_str(&format!(
        "\nEstimated: {} (the budget caps it at ${:.2}).\n",
        est.summary(),
        opts.budget_usd
    ));
    q.push_str("\nRun the team? Each sub-task runs as a background run in its own worktree; accepted results are merged onto a team branch.");
    let opts_q = vec!["Run the team".to_string(), "Cancel".to_string()];
    let approved = if env.interactive {
        matches!(
            env.frontend.choose(&q, &opts_q).await.as_deref(),
            Some("Run the team")
        )
    } else if est.over(&env.config.spend) && env.config.spend.stops_unattended() {
        env.emit(Event::Warn(format!("{q}\nNot started: the estimate is over the spend confirm threshold ([spend] non_interactive = \"stop\").")));
        false
    } else {
        env.emit(Event::Info(format!(
            "{q}\n(non-interactive: approved automatically)"
        )));
        true
    };
    if !approved {
        return Ok("Cancelled; nothing was started.".into());
    }

    // The shared base: HEAD plus the user's uncommitted changes, and the team branch.
    let id = crate::runs::new_id();
    let (r, tid) = (repo.clone(), id.clone());
    let (base, worktree, branch) =
        tokio::task::spawn_blocking(move || -> Result<(String, PathBuf, String)> {
            let head = git::git(&r, &["rev-parse", "--verify", "-q", "HEAD"]).map_err(|_| {
                anyhow::anyhow!(
                    "the repository at {} has no commits yet; commit once before starting a team",
                    r.display()
                )
            })?;
            let base = git::snapshot(&r, &format!("team-{tid}"), &head)?.unwrap_or(head);
            let branch = format!("chanto/team/{tid}");
            let wt = git::add_worktree(
                &r,
                &format!("team-{tid}"),
                &branch,
                &base,
                &crate::runs::worktrees_dir(),
            )?;
            Ok((base, wt, branch))
        })
        .await??;
    let team = Team {
        id,
        task: task.to_string(),
        repo,
        state: TeamState::Running,
        created: crate::runs::now(),
        agents: opts.agents,
        budget_usd: opts.budget_usd,
        base,
        branch,
        worktree,
        subtasks,
        run_options: opts.run_options,
        checks: vec![],
    };
    team.save()?;
    drive(&env, team, host, cancel).await
}

/// Continue a team whose lead stopped (`chanto team resume <id>`).
pub async fn resume(
    env: &Arc<Env>,
    id: &str,
    host: &dyn Host,
    cancel: CancellationToken,
) -> Result<String> {
    let mut team = Team::load(id)?;
    if team.state == TeamState::Done {
        return Ok(report(&team));
    }
    team.state = TeamState::Running;
    drive(env, team, host, cancel).await
}

fn run_body(team: &Team, t: &SubTask) -> Value {
    let mut task = format!(
        "[team {}/{}] {}\n\n{}\n\nYou are one of {} agents working in parallel for this overall task: {}\n",
        team.id,
        t.id,
        t.title,
        t.task,
        team.subtasks.len(),
        team.task
    );
    if !t.owns.is_empty() {
        task.push_str(&format!("You own: {}. Other agents are changing other files at the same time, so stay within yours unless the task can't be done otherwise.\n", t.owns.join(", ")));
    }
    if let Some(n) = &t.note {
        task.push_str(&format!("\n{n}\n"));
    }
    let mut body = team.run_options.clone();
    if !body.is_object() {
        body = json!({});
    }
    body["task"] = json!(task);
    body["cwd"] = json!(team.repo);
    body["start"] = json!(t.start.clone().unwrap_or_else(|| team.base.clone()));
    body["team"] = json!(team.id);
    body["max_cost_usd"] = json!((team.share() * 100.0).round() / 100.0);
    body
}

/// Ask the lead's model to accept or reject a finished sub-task.
async fn review(env: &Env, team: &Team, t: &SubTask, run: &Run) -> (bool, String) {
    let (r2, b, br) = (team.repo.clone(), run.base.clone(), run.branch.clone());
    let diff = tokio::task::spawn_blocking(move || {
        git::git(
            &r2,
            &[&["diff"][..], &crate::hostgit::DIFF_ARGS, &[&b, &br]].concat(),
        )
        .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    if diff.trim().is_empty() {
        return (false, "it made no changes".into());
    }
    let verify = run
        .verify
        .as_ref()
        .map(|v| format!("{} {}", v.status, v.detail))
        .unwrap_or_else(|| "not run".into());
    let prompt = format!(
        "Team review. You lead a team working on: {}\n\nSub-task {} ({}):\n{}\n\nIt owns: {}\nVerification in its worktree: {verify}\nIts final message:\n{}\n\nIts changes:\n```diff\n{}\n```\n\n\
         Accept if the changes do the sub-task correctly and stay within its files (merge conflicts are handled separately). \
         Reply with `ACCEPT` or `REJECT: <what must change>` on the first line.",
        team.task,
        t.id,
        t.title,
        t.task,
        if t.owns.is_empty() {
            "(not specified)".into()
        } else {
            t.owns.join(", ")
        },
        crate::util::truncate(run.result.as_deref().unwrap_or(""), 1500),
        crate::util::truncate_middle(&diff, 12_000)
    );
    let (provider, model) = env.resolve(&env.model());
    let req = crate::llm::ChatRequest {
        model,
        messages: vec![crate::llm::Message::user(prompt)],
        tools: vec![],
        think: Some(false),
        effort: None,
        temperature: Some(0.2),
        max_tokens: Some(1024),
        json: false,
    };
    match provider.chat(&req, &mut |_| {}).await {
        Ok(r) => {
            let first = r
                .text
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("")
                .trim_start_matches(['*', '`'])
                .to_string();
            if first.to_ascii_uppercase().starts_with("ACCEPT") {
                (true, String::new())
            } else {
                let why = first
                    .split_once(':')
                    .map(|(_, w)| w.trim().to_string())
                    .filter(|w| !w.is_empty())
                    .unwrap_or_else(|| crate::util::one_line(&r.text, 300));
                (false, why)
            }
        }
        // No review possible: fall back to the run's own verification.
        Err(e) => match run.verify.as_ref().map(|v| v.status.as_str()) {
            Some("failed") => (
                false,
                format!("its checks failed (review unavailable: {e})"),
            ),
            _ => (true, String::new()),
        },
    }
}

/// TeammateIdle for a sub-task whose run just finished: `Ok(feedback)` sends
/// it back to work (exit 2), `Err(reason)` stops it (`continue: false`).
async fn idle_hook(
    env: &Env,
    team: &Team,
    i: usize,
    run: &Run,
) -> Option<std::result::Result<String, String>> {
    let r = crate::hooks::teammate_idle(env, &team.subtasks[i].id, &team.id).await;
    if let Some(reason) = r.halt {
        return Some(Err(reason));
    }
    // Sent back at most a few times, so a hook can't keep a sub-task going forever.
    let feedback = r
        .block
        .filter(|_| team.subtasks[i].runs.len() < 5 && !run.branch.is_empty())?;
    Some(Ok(feedback))
}

fn status(env: &Env, ctl: &Control, team: &Team, needs_input: &[String]) {
    let lines = team.status_lines(needs_input);
    *ctl.lines.lock().unwrap() = lines.clone();
    env.emit(Event::Status {
        key: "team".into(),
        lines,
    });
}

/// Wait for a run to finish (or the team to be stopped).
async fn wait(
    host: &dyn Host,
    id: &str,
    ctl: &Control,
    cancel: &CancellationToken,
) -> Result<Option<Run>> {
    loop {
        let r = host.get(id).await?;
        if !r.state.is_active() {
            return Ok(Some(r));
        }
        if ctl.stop.load(Ordering::SeqCst) || cancel.is_cancelled() {
            return Ok(None);
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            _ = cancel.cancelled() => {}
        }
    }
}

/// Run sub-tasks, review, merge and check: the part `resume` repeats.
async fn drive(
    env: &Arc<Env>,
    mut team: Team,
    host: &dyn Host,
    cancel: CancellationToken,
) -> Result<String> {
    let ctl = Arc::new(Control {
        id: team.id.clone(),
        stop: AtomicBool::new(false),
        lines: Mutex::new(vec![]),
    });
    *ACTIVE.lock().unwrap() = Some(ctl.clone());
    struct Unregister(String);
    impl Drop for Unregister {
        fn drop(&mut self) {
            let mut a = ACTIVE.lock().unwrap();
            if a.as_ref().is_some_and(|c| c.id == self.0) {
                *a = None;
            }
        }
    }
    let _unregister = Unregister(team.id.clone());
    let res = drive_inner(env, &mut team, host, &ctl, &cancel).await;
    env.emit(Event::Status {
        key: "team".into(),
        lines: vec![],
    });
    if let Err(e) = &res {
        team.state = TeamState::Interrupted;
        team.save()?;
        bail!(
            "{e:#}\nThe team's sub-tasks keep running; `chanto team resume {}` continues.",
            team.id
        );
    }
    team.save()?;
    let text = report(&team);
    let _ = std::fs::write(team.dir().join("report.md"), &text);
    Ok(text)
}

async fn drive_inner(
    env: &Arc<Env>,
    team: &mut Team,
    host: &dyn Host,
    ctl: &Control,
    cancel: &CancellationToken,
) -> Result<()> {
    // 1. Parallel sub-task runs, each reviewed as it finishes.
    if team.state == TeamState::Running {
        loop {
            if ctl.stop.load(Ordering::SeqCst) {
                for t in team
                    .subtasks
                    .iter()
                    .filter(|t| t.state == SubState::Running)
                {
                    let _ = host.stop(t.runs.last().unwrap()).await;
                }
                team.state = TeamState::Cancelled;
                return Ok(());
            }
            if cancel.is_cancelled() {
                team.state = TeamState::Interrupted;
                return Ok(());
            }
            // Start sub-tasks while there's room and budget.
            let mut running = team
                .subtasks
                .iter()
                .filter(|t| t.state == SubState::Running)
                .count();
            for i in 0..team.subtasks.len() {
                if running >= team.agents || team.subtasks[i].state != SubState::Planned {
                    continue;
                }
                if !team.subtasks[i].runs.is_empty()
                    && team.spent() + team.share() > team.budget_usd + 1e-9
                {
                    team.subtasks[i].state = SubState::Rejected;
                    let n = team.subtasks[i].note.take().unwrap_or_default();
                    team.subtasks[i].note =
                        Some(format!("not retried: the team budget is spent. {n}"));
                    continue;
                }
                let run = host.create(run_body(team, &team.subtasks[i])).await?;
                let t = &mut team.subtasks[i];
                t.runs.push(run.id);
                t.state = SubState::Running;
                t.note = None;
                running += 1;
                team.save()?;
            }
            // Check on the running ones.
            let mut needs_input = Vec::new();
            for i in 0..team.subtasks.len() {
                if team.subtasks[i].state != SubState::Running {
                    continue;
                }
                let rid = team.subtasks[i].runs.last().cloned().unwrap_or_default();
                let r = host.get(&rid).await?;
                if r.state == RunState::NeedsInput {
                    needs_input.push(r.id.clone());
                }
                if r.state.is_active() {
                    continue;
                }
                team.subtasks[i].cost_usd += r.cost_usd;
                if r.state != RunState::Done {
                    let t = &mut team.subtasks[i];
                    t.state = SubState::Failed;
                    t.note = Some(format!(
                        "run {} {}: {}",
                        r.id,
                        r.state.label(),
                        r.error.clone().unwrap_or_default()
                    ));
                } else if let Some(stop) = idle_hook(env, team, i, &r).await {
                    // A TeammateIdle hook sent it back to work, or stopped it.
                    let t = &mut team.subtasks[i];
                    match stop {
                        Ok(feedback) => {
                            t.state = SubState::Planned;
                            t.note = Some(format!(
                                "You had finished this sub-task (run {}), but a TeammateIdle hook says:\n{feedback}\n\
                                 Your earlier work is on branch {}: run `git merge --ff-only {}` first to pick it up, then continue from there.",
                                r.id, r.branch, r.branch
                            ));
                        }
                        Err(reason) => {
                            t.state = SubState::Rejected;
                            t.note = Some(format!("stopped by a TeammateIdle hook: {reason}"));
                        }
                    }
                } else {
                    env.emit(Event::Busy(Some(format!(
                        "Reviewing {}…",
                        team.subtasks[i].id
                    ))));
                    let (ok, why) = review(env, team, &team.subtasks[i], &r).await;
                    env.emit(Event::Busy(None));
                    let t = &mut team.subtasks[i];
                    if ok {
                        t.state = SubState::Accepted;
                    } else if t.runs.len() < 2 {
                        // One retry, with the review as feedback.
                        t.state = SubState::Planned;
                        t.note = Some(format!(
                            "A previous attempt (run {}, branch {}) was rejected in review: {why}\nDo the sub-task again, addressing that.",
                            r.id, r.branch
                        ));
                    } else {
                        t.state = SubState::Rejected;
                        t.note = Some(format!("rejected in review: {why}"));
                    }
                }
                team.save()?;
            }
            status(env, ctl, team, &needs_input);
            if !team
                .subtasks
                .iter()
                .any(|t| matches!(t.state, SubState::Running | SubState::Planned))
            {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                _ = cancel.cancelled() => {}
            }
        }
        team.state = TeamState::Merging;
        team.save()?;
    }

    // 2. Merge the accepted results in plan order onto the team branch.
    if team.state == TeamState::Merging {
        for i in 0..team.subtasks.len() {
            if team.subtasks[i].state != SubState::Accepted {
                continue;
            }
            status(env, ctl, team, &[]);
            let rid = team.subtasks[i].runs.last().cloned().unwrap_or_default();
            let r = host.get(&rid).await?;
            match cherry_pick(&team.repo, &team.worktree, &r).await? {
                Ok(()) => team.subtasks[i].state = SubState::Merged,
                Err(files) => {
                    // Re-delegate once, on top of the merged work.
                    let fits = team.spent() + team.share() <= team.budget_usd + 1e-9;
                    if team.subtasks[i].runs.len() >= 3 || !fits {
                        let t = &mut team.subtasks[i];
                        t.state = SubState::Conflict;
                        t.note = Some(format!(
                            "conflicts in {}; not re-delegated ({})",
                            files,
                            if fits {
                                "already retried"
                            } else {
                                "budget spent"
                            }
                        ));
                        team.save()?;
                        continue;
                    }
                    let (repo, wt) = (team.repo.clone(), team.worktree.clone());
                    let tip = tokio::task::spawn_blocking(move || {
                        git::wt_git(&repo, &wt, &["rev-parse", "HEAD"])
                    })
                    .await??;
                    {
                        let t = &mut team.subtasks[i];
                        t.start = Some(tip);
                        t.note = Some(format!(
                            "Your earlier attempt (run {}, branch {}) conflicted with the work merged before it, in: {files}. \
                             Your worktree now starts from that merged work: redo the sub-task on top of it, keeping what the others did. \
                             `git diff {} {}` shows the earlier attempt.",
                            r.id, r.branch, r.base, r.branch
                        ));
                    }
                    let run = host.create(run_body(team, &team.subtasks[i])).await?;
                    team.subtasks[i].runs.push(run.id.clone());
                    team.subtasks[i].state = SubState::Running;
                    team.save()?;
                    status(env, ctl, team, &[]);
                    let Some(done) = wait(host, &run.id, ctl, cancel).await? else {
                        team.subtasks[i].state = SubState::Accepted;
                        team.state = if ctl.stop.load(Ordering::SeqCst) {
                            TeamState::Cancelled
                        } else {
                            TeamState::Interrupted
                        };
                        if ctl.stop.load(Ordering::SeqCst) {
                            let _ = host.stop(&run.id).await;
                        }
                        return Ok(());
                    };
                    team.subtasks[i].cost_usd += done.cost_usd;
                    let t = &mut team.subtasks[i];
                    if done.state != RunState::Done {
                        t.state = SubState::Conflict;
                        t.note = Some(format!(
                            "the re-delegated run {} {}",
                            done.id,
                            done.state.label()
                        ));
                    } else {
                        match cherry_pick(&team.repo, &team.worktree, &done).await? {
                            Ok(()) => {
                                t.state = SubState::Merged;
                                t.note = Some(format!("re-delegated after conflicts in {files}"));
                            }
                            Err(files) => {
                                t.state = SubState::Conflict;
                                t.note = Some(format!("still conflicts in {files}"));
                            }
                        }
                    }
                }
            }
            team.save()?;
        }

        // 3. The project's checks on the merged result.
        env.emit(Event::Busy(Some(
            "Running the project's checks on the merged result…".into(),
        )));
        let checks = if env.config.guardrails.checks.is_empty() {
            crate::guardrails::detect_checks(&team.worktree)
        } else {
            env.config.guardrails.checks.clone()
        };
        let timeout = Duration::from_secs(env.config.guardrails.timeout_secs.unwrap_or(600));
        team.checks.clear();
        for c in checks {
            let r = crate::guardrails::run_check(&c, &team.worktree, timeout).await;
            team.checks.push(CheckOutcome {
                command: c,
                ok: r.ok,
                output: crate::util::truncate_middle(r.output.trim(), 2000),
            });
        }
        env.emit(Event::Busy(None));
        team.state = TeamState::Done;
    }
    Ok(())
}

/// Cherry-pick a run's own commits onto the team branch. Err(files) on a
/// conflict (the cherry-pick is aborted, leaving the branch as it was).
async fn cherry_pick(
    repo: &Path,
    worktree: &Path,
    run: &Run,
) -> Result<std::result::Result<(), String>> {
    let (repo, wt, r) = (repo.to_path_buf(), worktree.to_path_buf(), run.clone());
    tokio::task::spawn_blocking(move || -> Result<std::result::Result<(), String>> {
        git::finalize(&r.source.path, &r.worktree, &r.id, &r.task)?;
        if git::commit_count(&repo, &r.base, &r.branch) == 0 {
            return Ok(Ok(()));
        }
        let range = format!("{}..{}", r.base, r.branch);
        if git::wt_git_ident(&repo, &wt, &["cherry-pick", "--allow-empty", &range]).is_ok() {
            return Ok(Ok(()));
        }
        let files = git::wt_git(&repo, &wt, &["diff", "--name-only", "--diff-filter=U"])
            .unwrap_or_default()
            .lines()
            .collect::<Vec<_>>()
            .join(", ");
        let _ = git::wt_git(&repo, &wt, &["cherry-pick", "--abort"]);
        Ok(Err(if files.is_empty() {
            "(unknown files)".into()
        } else {
            files
        }))
    })
    .await?
}

pub fn report(team: &Team) -> String {
    let merged = team
        .subtasks
        .iter()
        .filter(|t| t.state == SubState::Merged)
        .count();
    let checks_ok = team.checks.iter().all(|c| c.ok);
    let verdict = match team.state {
        TeamState::Done if merged == team.subtasks.len() && checks_ok => format!(
            "✅ All {merged} sub-task(s) merged onto `{}`{}.",
            team.branch,
            if team.checks.is_empty() {
                ""
            } else {
                "; the checks pass"
            }
        ),
        TeamState::Done => format!(
            "⚠ {merged}/{} sub-task(s) merged onto `{}`{}.",
            team.subtasks.len(),
            team.branch,
            if checks_ok {
                ""
            } else {
                "; the checks FAIL on the merged result"
            }
        ),
        TeamState::Cancelled => format!(
            "⚠ Stopped; {merged}/{} sub-task(s) merged.",
            team.subtasks.len()
        ),
        _ => format!(
            "⚠ Interrupted; `chanto team resume {}` continues ({merged}/{} merged so far).",
            team.id,
            team.subtasks.len()
        ),
    };
    let lead = team.subtasks.iter().map(|t| t.cost_usd).sum::<f64>();
    let mut s = format!(
        "# Team report\n\n**Task:** {}\n\n**Result:** {verdict}\n\nSub-task runs cost ${lead:.2} of ${:.2}.\n\n## Sub-tasks\n",
        team.task, team.budget_usd
    );
    for t in &team.subtasks {
        s.push_str(&format!(
            "\n- {} **{}** {} · {} · runs {}{}",
            t.state.icon(),
            t.id,
            t.title,
            t.state.label(),
            if t.runs.is_empty() {
                "-".into()
            } else {
                t.runs.join(", ")
            },
            t.note
                .as_ref()
                .map(|n| format!("\n  {}", crate::util::one_line(n, 300)))
                .unwrap_or_default()
        ));
    }
    if !team.checks.is_empty() {
        s.push_str("\n\n## Checks on the merged result\n");
        for c in &team.checks {
            s.push_str(&format!(
                "\n- {} `{}`",
                if c.ok { "✅" } else { "❌" },
                c.command
            ));
            if !c.ok {
                s.push_str(&format!(
                    "\n  ```\n{}\n  ```",
                    crate::util::truncate_middle(&c.output, 1200)
                ));
            }
        }
    }
    if merged > 0 {
        s.push_str(&format!(
            "\n\nBring it onto your branch with `chanto team apply {}` (or `git merge {}`).",
            team.id, team.branch
        ));
    }
    s.push('\n');
    s
}

// ------------------------------------------------------------------ CLI

pub const USAGE: &str = "Split a task into parallel sub-tasks run by separate agents, then review, merge and check them.

Usage:
  chanto team \"<task>\" [options]   plan, approve, run sub-tasks as background runs, merge onto chanto/team/<id>
  chanto team list                  this project's teams
  chanto team show <id>             a team's report
  chanto team resume <id>           continue a team whose lead stopped (its runs keep going)
  chanto team apply <id>            bring the team branch onto your current branch

Options:
  --agents <N>       sub-task runs at once (default: [team] agents, 3)
  --max-cost <USD>   total budget for the sub-task runs, split evenly (default: [team] max_cost_usd, 6)
  -m, --model, --provider, --base-url, --mode, --no-verify   applied to the sub-task runs

Sub-task runs show up in `chanto runs` and, grouped under the team, in the web UI.";

/// `chanto team "<task>" [--agents N] [--max-cost USD]`, `chanto team
/// list|show <id>|resume <id>|apply <id>`. Returns the exit code.
pub async fn cli(cli: crate::Cli) -> Result<i32> {
    let words: Vec<String> = cli.prompt.iter().skip(1).cloned().collect();
    let cwd = match &cli.cwd {
        Some(p) => crate::util::canonical(p)?,
        None => std::env::current_dir()?,
    };
    let arg = words.get(1).map(String::as_str).unwrap_or("");
    match (words.first().map(String::as_str), words.len()) {
        (Some("list"), 1) => {
            println!(
                "{}",
                command(&git::toplevel(&cwd).unwrap_or(cwd), "list")
                    .await
                    .replace("```", "")
                    .trim()
            );
            return Ok(0);
        }
        (Some("show"), 2) => {
            let t = Team::load(arg)?;
            println!("{}", crate::ui::render_markdown(&report(&t)));
            return Ok(0);
        }
        (Some("apply"), 2) => {
            let t = Team::load(arg)?;
            if t.state != TeamState::Done {
                bail!("team {} is {:?}; resume it first", t.id, t.state);
            }
            let run = t.as_run();
            println!(
                "{}",
                tokio::task::spawn_blocking(move || git::apply(&cwd, &run)).await??
            );
            return Ok(0);
        }
        _ => {}
    }
    let resume_id = (words.first().map(String::as_str) == Some("resume") && words.len() == 2)
        .then(|| arg.to_string());
    if cli.bg {
        bail!(
            "a team already runs its sub-tasks in the background; its lead runs here (`chanto team resume <id>` continues it)"
        );
    }
    use std::io::IsTerminal;
    let interactive = std::io::stdin().is_terminal();
    let fe = Arc::new(crate::plain::PlainFrontend::new(
        crate::plain::OutputFormat::Live,
        cli.verbose,
    ));
    let crate::Setup {
        env,
        mut agent,
        warnings,
    } = crate::setup(&cli, fe, interactive).await?;
    for w in warnings {
        crate::ui::warn(&w);
    }
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        c2.cancel();
    });
    let client = crate::runs::cli::connect(true).await?;
    let res = match resume_id {
        Some(id) => resume(&env, &id, &client, cancel).await,
        None => {
            let cfg = &env.config.team;
            let mut run_options = crate::runs::cli::new_run_body(&cwd, "", Some(&cli));
            if let Some(o) = run_options.as_object_mut() {
                o.remove("task");
                o.remove("cwd");
                o.remove("max_cost_usd");
            }
            let opts = TeamOpts {
                agents: cli.team_size().or(cfg.agents).unwrap_or(3).max(1),
                budget_usd: cli.max_cost.or(cfg.max_cost_usd).unwrap_or(6.0),
                run_options,
            };
            start(&mut agent, &words.join(" "), opts, &client, cancel).await
        }
    };
    env.shutdown().await;
    let text = res?;
    println!("\n{}", crate::ui::render_markdown(&text));
    Ok(if text.contains("**Result:** ✅") {
        0
    } else {
        1
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tests::{make_env_with, mock_server_routed, tc};
    use crate::permissions::Mode;
    use crate::runs::tests::{TestHome, make_repo, sh};

    fn write(path: &str, content: &str, done: &str) -> Vec<Value> {
        vec![
            json!({"tool_calls": tc("Write", json!({"file_path": path, "content": content}))}),
            json!({"content": done}),
        ]
    }

    fn plan(items: &[(&str, &str, &str, &str)]) -> Value {
        let subtasks: Vec<Value> = items.iter().map(|(id, title, task, owns)| json!({"id": id, "title": title, "task": task, "owns": [owns]})).collect();
        json!({"content": format!("Here is the plan:\n```json\n{}\n```", json!({"subtasks": subtasks}))})
    }

    fn opts(base: &str) -> TeamOpts {
        TeamOpts {
            agents: 2,
            budget_usd: 4.0,
            run_options: json!({"model": "mock", "provider": "openai", "base_url": base, "sandbox": false, "verify": false}),
        }
    }

    #[tokio::test]
    async fn team_runs_subtasks_in_parallel_and_merges_them() {
        let home = TestHome::new("team");
        let repo = make_repo(&home);
        let (base, seen) = mock_server_routed(vec![
            (
                "Team mode, step 1",
                vec![plan(&[
                    ("t1", "Alpha", "ALPHA-TASK: create alpha.txt", "alpha.txt"),
                    ("t2", "Beta", "BETA-TASK: create beta.txt", "beta.txt"),
                ])],
            ),
            (
                "Team review",
                vec![json!({"content": "ACCEPT"}), json!({"content": "ACCEPT"})],
            ),
            (
                "ALPHA-TASK",
                write("alpha.txt", "a\n", "Created alpha.txt."),
            ),
            ("BETA-TASK", write("beta.txt", "b\n", "Created beta.txt.")),
        ])
        .await;
        let m = home.manager();
        let env = make_env_with(&base, &repo, Mode::AcceptEdits, 32768, |c| {
            c.guardrails.checks = vec!["test -f alpha.txt && test -f beta.txt".into()]
        });
        let mut lead = Agent::new(env.clone());
        let report = start(
            &mut lead,
            "add alpha and beta files",
            opts(&base),
            &m,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            report.contains("All 2 sub-task(s) merged onto `chanto/team/")
                && report.contains("the checks pass"),
            "{report}"
        );
        assert_eq!(
            env.mode(),
            Mode::AcceptEdits,
            "mode restored after planning"
        );

        let team = list().into_iter().next().unwrap();
        assert_eq!(team.state, TeamState::Done);
        // One background run per sub-task, grouped under the team, from the shared base, with half the budget each.
        let runs: Vec<Run> = m
            .list()
            .into_iter()
            .filter(|r| r.team.as_deref() == Some(team.id.as_str()))
            .collect();
        assert_eq!(runs.len(), 2);
        assert!(
            runs.iter().all(|r| r.base == team.base
                && r.settings.max_cost_usd == 2.0
                && r.state == RunState::Done),
            "{runs:?}"
        );
        assert!(
            runs.iter()
                .any(|r| r.task.starts_with(&format!("[team {}/t1] Alpha", team.id)))
        );
        // Each result was reviewed before merging.
        assert_eq!(
            texts(&seen)
                .iter()
                .filter(|t| t.contains("Team review"))
                .count(),
            2
        );
        // The team branch has both results; checks ran there; the checkout is untouched.
        assert_eq!(
            sh(&repo, &["show", &format!("{}:alpha.txt", team.branch)]),
            "a"
        );
        assert_eq!(
            sh(&repo, &["show", &format!("{}:beta.txt", team.branch)]),
            "b"
        );
        assert_eq!(team.checks.len(), 1);
        assert!(team.checks[0].ok, "{:?}", team.checks);
        assert!(!repo.join("alpha.txt").exists());
        assert!(team.subtasks.iter().all(|t| t.state == SubState::Merged));
        // `chanto team apply`: the team's commits onto the current branch.
        let msg = git::apply(&repo, &team.as_run()).unwrap();
        assert!(msg.contains("Applied 2 commit(s)"), "{msg}");
        assert_eq!(
            crate::runs::tests::checked_out(repo.join("beta.txt")),
            "b\n"
        );
    }

    #[tokio::test]
    async fn conflicting_subtasks_are_redelegated_on_top_of_the_merged_work() {
        let home = TestHome::new("team-conflict");
        let repo = make_repo(&home);
        let (base, _) = mock_server_routed(vec![
            ("Team mode, step 1", vec![plan(&[("t1", "One", "ONE-TASK: write one into shared.txt", "shared.txt"), ("t2", "Two", "TWO-TASK: write two into shared.txt", "shared.txt")])]),
            ("Team review", vec![json!({"content": "ACCEPT"}), json!({"content": "ACCEPT"})]),
            // The re-delegated attempt starts from the merged work and keeps it.
            (
                "conflicted with the work merged before it",
                vec![
                    json!({"tool_calls": tc("Read", json!({"file_path": "shared.txt"}))}),
                    json!({"tool_calls": tc("Write", json!({"file_path": "shared.txt", "content": "one\ntwo\n"}))}),
                    json!({"content": "Kept one, added two."}),
                ],
            ),
            ("ONE-TASK", write("shared.txt", "one\n", "Wrote one.")),
            ("TWO-TASK", write("shared.txt", "two\n", "Wrote two.")),
        ])
        .await;
        let m = home.manager();
        let env = make_env_with(&base, &repo, Mode::AcceptEdits, 32768, |c| {
            c.guardrails.checks = vec!["grep -q one shared.txt && grep -q two shared.txt".into()]
        });
        let mut lead = Agent::new(env);
        let report = start(
            &mut lead,
            "write one and two",
            opts(&base),
            &m,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(report.contains("All 2 sub-task(s) merged"), "{report}");
        let team = list().into_iter().next().unwrap();
        let t2 = &team.subtasks[1];
        assert_eq!(t2.runs.len(), 2, "{t2:?}");
        assert!(
            t2.note
                .as_deref()
                .unwrap_or("")
                .contains("re-delegated after conflicts in shared.txt"),
            "{t2:?}"
        );
        // The retry started from the team branch with t1 merged, not from the original base.
        let retry = crate::runs::manager::Manager::get(&m, &t2.runs[1]).unwrap();
        assert_ne!(retry.base, team.base);
        assert_eq!(
            sh(&repo, &["show", &format!("{}:shared.txt", retry.base)]),
            "one"
        );
        assert_eq!(
            sh(&repo, &["show", &format!("{}:shared.txt", team.branch)]),
            "one\ntwo"
        );
        assert!(team.checks.iter().all(|c| c.ok), "{:?}", team.checks);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn teammate_idle_hook_sends_a_subtask_back_to_work() {
        let home = TestHome::new("team-idle");
        let repo = make_repo(&home);
        let (base, _) = mock_server_routed(vec![
            (
                "Team mode, step 1",
                vec![plan(&[(
                    "t1",
                    "Alpha",
                    "ALPHA-TASK: create alpha.txt",
                    "alpha.txt",
                )])],
            ),
            ("Team review", vec![json!({"content": "ACCEPT"})]),
            (
                "ALPHA-TASK",
                [
                    write("alpha.txt", "a\n", "Created alpha.txt."),
                    write("alpha.txt", "a\nb\n", "Added b."),
                ]
                .concat(),
            ),
        ])
        .await;
        let m = home.manager();
        let log = repo.join("..").join("idle.log");
        let marker = repo.join("..").join("idle-once");
        // The first time the teammate is about to go idle, send it back with feedback.
        let hook = format!(
            "cat >> '{log}'; echo >> '{log}'; [ -f '{m}' ] && exit 0; touch '{m}'; echo 'also add b' >&2; exit 2",
            log = log.display(),
            m = marker.display()
        );
        let env = make_env_with(&base, &repo, Mode::AcceptEdits, 32768, |c| {
            c.hooks.insert(
                "TeammateIdle".into(),
                vec![crate::config::HookConfig {
                    command: hook,
                    ..Default::default()
                }],
            );
        });
        let mut lead = Agent::new(env);
        let report = start(
            &mut lead,
            "add alpha",
            opts(&base),
            &m,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(report.contains("merged"), "{report}");
        let team = list().into_iter().next().unwrap();
        let t1 = &team.subtasks[0];
        assert_eq!(t1.runs.len(), 2, "sent back once: {t1:?}");
        let second = crate::runs::manager::Manager::get(&m, &t1.runs[1]).unwrap();
        assert!(
            second.task.contains("also add b") && second.task.contains("TeammateIdle"),
            "{}",
            second.task
        );
        let payloads: Vec<Value> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(payloads.len(), 2);
        assert_eq!(
            (
                payloads[0]["teammate_name"].as_str(),
                payloads[0]["team_name"].as_str()
            ),
            (Some("t1"), Some(team.id.as_str()))
        );
    }

    fn texts(seen: &Mutex<Vec<Value>>) -> Vec<String> {
        seen.lock()
            .unwrap()
            .iter()
            .map(|b| b["messages"].to_string())
            .collect()
    }

    #[test]
    fn plans_parse_and_overlaps_are_found() {
        let p = parse_plan("```json\n{\"subtasks\": [{\"id\": \"a\", \"title\": \"A\", \"task\": \"do a\", \"owns\": [\"src/a/**\"]}, {\"id\": \"a\", \"task\": \"do b\", \"owns\": [\"src/a/x.rs\"]}, {\"id\": \"c\", \"task\": \"\"}]}\n```", 8).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[1].id, "t2", "duplicate ids are renumbered");
        assert_eq!(
            overlaps(&p),
            vec![("a".into(), "t2".into(), "src/a/** / src/a/x.rs".into())]
        );
        assert!(parse_plan("no plan here", 8).is_err());
        let one = parse_plan(r#"{"subtasks": [{"task": "x", "owns": ["src/ab.rs"]}, {"task": "y", "owns": ["src/a.rs"]}]}"#, 1).unwrap();
        assert_eq!(one.len(), 1);
    }
}
