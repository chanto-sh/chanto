//! Goals: keep working toward an objective, across many turns and context
//! windows, until it is verifiably met.
//!
//! 1. Criteria: outcome mode's first phase. The agent explores read-only and
//!    writes executable acceptance criteria, they run once as a baseline, and
//!    the user approves them. From then on they are locked: Chanto runs the
//!    copy recorded in goal.json, and any change to the criteria files is
//!    undone (and reported to the agent).
//! 2. Iterations: each is one agent turn (plan the next step, do it, run the
//!    relevant checks). After it, Chanto runs every criterion itself and
//!    appends the result and the agent's summary to progress.md.
//! 3. It ends when every criterion (and the project's checks) passes; when a
//!    limit is hit (iterations, iterations without a new passing criterion,
//!    cost, time); when the user pauses or stops it; or when the agent is
//!    blocked on something only the user can provide (it asks, and an
//!    unattended goal stops as `blocked`).
//!
//! State lives in `<chanto home>/goals/<id>/` (goal.json, criteria.json and
//! its check scripts, progress.md) rather than in the project: the
//! bookkeeping never shows up in `git status`, is never committed by a
//! background run, survives `git clean` and worktree removal, and sits outside
//! the workspace, so the agent can't edit its own criteria without a
//! permission prompt. On compaction the objective, the criteria and the tail
//! of the progress log are re-injected (`reminder`); after a restart
//! `/goal resume` reloads everything from that directory.

use crate::agent::{Agent, Event, TurnEnd};
use crate::outcome::{self, CheckRun, Criterion};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// `[goal]` in config.toml.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GoalConfig {
    /// Work iterations (agent turns) before the goal stops (default 20).
    pub max_iterations: Option<u32>,
    /// Iterations in a row without a new passing criterion before it stops (default 3).
    pub max_stale_iterations: Option<u32>,
    /// Budget in USD (default: none beyond the session budget).
    pub max_cost_usd: Option<f64>,
    /// Wall-clock limit in minutes (default 120).
    pub max_minutes: Option<u64>,
}

impl GoalConfig {
    pub fn merge(&mut self, o: GoalConfig) {
        macro_rules! take {
            ($($f:ident),*) => { $(if o.$f.is_some() { self.$f = o.$f; })* };
        }
        take!(
            max_iterations,
            max_stale_iterations,
            max_cost_usd,
            max_minutes
        );
    }

    pub fn limits(&self) -> Limits {
        Limits {
            max_iterations: self.max_iterations.unwrap_or(20).max(1),
            max_stale: self.max_stale_iterations.unwrap_or(3).max(1),
            max_cost_usd: self.max_cost_usd,
            max_minutes: Some(self.max_minutes.unwrap_or(120)).filter(|m| *m > 0),
        }
    }
}

/// Limits for one run of the goal loop (from the start, or from a resume).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Limits {
    pub max_iterations: u32,
    pub max_stale: u32,
    pub max_cost_usd: Option<f64>,
    pub max_minutes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GoalState {
    /// Writing and approving the acceptance criteria.
    Criteria,
    Active,
    /// Paused by the user (or interrupted); `/goal resume` continues.
    Paused,
    /// The agent needs something only the user can provide.
    Blocked,
    Done,
    /// A limit was hit, no progress was made, or the user stopped it.
    Stopped,
}

impl GoalState {
    pub fn label(self) -> &'static str {
        match self {
            GoalState::Criteria => "criteria",
            GoalState::Active => "active",
            GoalState::Paused => "paused",
            GoalState::Blocked => "blocked",
            GoalState::Done => "done",
            GoalState::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub objective: String,
    /// The project the goal works on.
    pub root: PathBuf,
    /// Where its state lives (`<home>/goals/<id>`).
    pub dir: PathBuf,
    pub state: GoalState,
    pub created: String,
    pub updated: String,
    /// The approved criteria: Chanto runs these, not whatever criteria.json says later.
    #[serde(default)]
    pub criteria: Vec<Criterion>,
    /// Contents of the criteria files at approval (relative path → text).
    #[serde(default)]
    pub locked: BTreeMap<String, String>,
    /// Criteria passing at the last run, and every criterion that has passed so far.
    #[serde(default)]
    pub passing: Vec<String>,
    #[serde(default)]
    pub ever_passed: Vec<String>,
    #[serde(default)]
    pub iterations: u32,
    /// Iterations in a row without a new passing criterion.
    #[serde(default)]
    pub stale: u32,
    pub limits: Limits,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub active_secs: u64,
    /// Why it stopped, paused or is blocked.
    #[serde(default)]
    pub reason: Option<String>,
    /// The user's answer when the agent was blocked, for the next iteration.
    #[serde(default)]
    pub guidance: Option<String>,
}

/// Where goals live: `<chanto home>/goals/<id>/`.
pub fn goals_dir() -> PathBuf {
    crate::config::home_dir().join("goals")
}

/// Files in a goal directory that Chanto itself writes (never locked).
const OWN_FILES: &[&str] = &[
    "goal.json",
    "goal.json.tmp",
    "progress.md",
    "control",
    "report.md",
];

impl Goal {
    pub fn save(&mut self) -> Result<()> {
        self.updated = chrono::Local::now().to_rfc3339();
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join("goal.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, self.dir.join("goal.json"))?;
        Ok(())
    }

    pub fn load(dir: &Path) -> Result<Goal> {
        let text = std::fs::read_to_string(dir.join("goal.json"))
            .map_err(|_| anyhow::anyhow!("no goal in {}", dir.display()))?;
        Ok(serde_json::from_str(&text)?)
    }

    pub fn progress_path(&self) -> PathBuf {
        self.dir.join("progress.md")
    }

    /// "◎ goal 2/5 · iteration 3": the status-line summary.
    pub fn status_line(&self) -> String {
        let mut s = format!("◎ goal {}/{}", self.passing.len(), self.criteria.len());
        match self.state {
            GoalState::Active if self.iterations > 0 => {
                s.push_str(&format!(" · iteration {}", self.iterations))
            }
            GoalState::Active => {}
            GoalState::Criteria => s = "◎ goal: writing criteria".into(),
            st => s.push_str(&format!(" · {}", st.label())),
        }
        s
    }

    /// Undo any change to the locked criteria files (and remove files added
    /// next to them). Returns what was changed.
    fn restore_locked(&self) -> Vec<String> {
        if self.locked.is_empty() {
            return vec![];
        }
        let mut changed = Vec::new();
        for (rel, text) in &self.locked {
            let p = self.dir.join(rel);
            if std::fs::read_to_string(&p).ok().as_deref() != Some(text.as_str()) {
                if let Some(parent) = p.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&p, text);
                changed.push(rel.clone());
            }
        }
        for (rel, _) in read_files(&self.dir) {
            if !self.locked.contains_key(&rel) {
                let _ = std::fs::remove_file(self.dir.join(&rel));
                changed.push(rel);
            }
        }
        changed
    }
}

/// Every file under `dir` (relative path → text), except Chanto's own.
fn read_files(dir: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let Ok(rel) = p.strip_prefix(dir) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if OWN_FILES.contains(&rel.as_str()) || out.len() >= 200 {
                continue;
            }
            if let Ok(t) = std::fs::read_to_string(&p)
                && t.len() <= 1 << 20
            {
                out.insert(rel, t);
            }
        }
    }
    out
}

/// Goals under `base`, newest first.
pub fn list(base: &Path) -> Vec<Goal> {
    let mut v: Vec<Goal> = std::fs::read_dir(base)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| Goal::load(&e.path()).ok())
        .collect();
    v.sort_by(|a, b| b.created.cmp(&a.created));
    v
}

/// The goal `id` (a unique prefix), or the newest unfinished goal of `root`.
pub fn find(base: &Path, id: Option<&str>, root: &Path) -> Result<Goal> {
    let all = list(base);
    match id.filter(|i| !i.is_empty()) {
        Some(id) => {
            let m: Vec<&Goal> = all.iter().filter(|g| g.id.starts_with(id)).collect();
            match m.as_slice() {
                [g] => Ok((*g).clone()),
                [] => bail!("no goal {id}"),
                _ => bail!("goal id {id} is ambiguous"),
            }
        }
        None => all
            .into_iter()
            .find(|g| g.root == root && g.state != GoalState::Done)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no unfinished goal for {}; start one with `/goal <objective>`",
                    root.display()
                )
            }),
    }
}

/// Re-injected after compaction: the objective, the criteria and the recent progress.
pub fn reminder(dir: &Path) -> Option<String> {
    let g = Goal::load(dir).ok()?;
    let mut s = format!(
        "<system-reminder>You are working toward a goal (id {}). Objective: {}\n\nAcceptance criteria (locked; chanto runs them after every iteration):\n",
        g.id, g.objective
    );
    for c in &g.criteria {
        s.push_str(&format!(
            "- {} {} {}: `{}`\n",
            if g.passing.contains(&c.id) {
                "✅"
            } else {
                "❌"
            },
            c.id,
            c.title,
            c.check
        ));
    }
    if let Some(p) = progress_tail(&g, 2500) {
        s.push_str(&format!(
            "\nProgress so far ({}):\n{p}\n",
            g.progress_path().display()
        ));
    }
    s.push_str(&format!(
        "chanto runs each criterion like this: `{}`.\n",
        reproduce(&g, "<check>")
    ));
    s.push_str("Keep working toward the objective; don't edit the criteria.</system-reminder>");
    Some(s)
}

fn progress_tail(g: &Goal, max: usize) -> Option<String> {
    let t = std::fs::read_to_string(g.progress_path()).ok()?;
    let t = t.trim();
    if t.is_empty() {
        return None;
    }
    if t.len() <= max {
        return Some(t.to_string());
    }
    let mut cut = t.len() - max;
    while !t.is_char_boundary(cut) {
        cut += 1;
    }
    Some(format!("…{}", &t[cut..]))
}

fn append_progress(g: &Goal, text: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(g.progress_path())
    {
        let _ = writeln!(f, "{text}");
    }
}

// ------------------------------------------------------------------ control

/// The running goal of this process, for `/goal status|pause|stop`.
pub struct Control {
    pub id: String,
    pub objective: String,
    pause: AtomicBool,
    stop: AtomicBool,
    turn: Mutex<Option<CancellationToken>>,
    pub status: Mutex<String>,
}

static ACTIVE: Mutex<Option<Arc<Control>>> = Mutex::new(None);

pub fn active() -> Option<Arc<Control>> {
    ACTIVE.lock().unwrap().clone()
}

struct Unregister(String);

impl Drop for Unregister {
    fn drop(&mut self) {
        let mut a = ACTIVE.lock().unwrap();
        if a.as_ref().is_some_and(|c| c.id == self.0) {
            *a = None;
        }
    }
}

/// A pause or stop requested from another process (`chanto goal pause|stop`).
fn requested(dir: &Path) -> Option<String> {
    let s = std::fs::read_to_string(dir.join("control")).ok()?;
    let _ = std::fs::remove_file(dir.join("control"));
    Some(s.trim().to_string())
}

/// `/goal status|pause|stop|list` (they take effect at once, even mid-turn).
pub fn command(root: &Path, arg: &str) -> String {
    let base = goals_dir();
    let words: Vec<&str> = arg.split_whitespace().collect();
    match words.as_slice() {
        [] | ["status"] => match active() {
            Some(c) => format!("{} — {}\nGoal `{}` is running. `/goal pause` stops after this iteration, `/goal stop` stops now.", c.status.lock().unwrap(), c.objective, c.id),
            None => match find(&base, None, root).or_else(|_| list(&base).into_iter().find(|g| g.root == root).ok_or_else(|| anyhow::anyhow!("none"))) {
                Ok(g) => describe(&g),
                Err(_) => "No goal yet. `/goal <objective>` agrees acceptance criteria with you, then works until they all pass.".into(),
            },
        },
        ["status", id] => match find(&base, Some(id), root) {
            Ok(g) => describe(&g),
            Err(e) => format!("{e:#}"),
        },
        ["pause"] | ["stop"] => match active() {
            Some(c) => {
                if words[0] == "stop" {
                    c.stop.store(true, Ordering::SeqCst);
                    if let Some(t) = c.turn.lock().unwrap().as_ref() {
                        t.cancel();
                    }
                    format!("Stopping goal `{}`.", c.id)
                } else {
                    c.pause.store(true, Ordering::SeqCst);
                    format!("Goal `{}` pauses after this iteration; `/goal resume` continues it.", c.id)
                }
            }
            None => "No goal is running in this session.".into(),
        },
        ["list"] => {
            let goals: Vec<Goal> = list(&base).into_iter().filter(|g| g.root == root).collect();
            if goals.is_empty() {
                return "No goals for this project.".into();
            }
            let mut s = String::from("```\n");
            for g in goals.iter().take(20) {
                s.push_str(&format!("{:<20} {:<9} {:>5}  {}\n", g.id, g.state.label(), format!("{}/{}", g.passing.len(), g.criteria.len()), crate::util::one_line(&g.objective, 60)));
            }
            s.push_str("```\n`/goal resume <id>` continues one.");
            s
        }
        _ => "Usage: `/goal <objective>` · `/goal status|pause|stop|list` · `/goal resume [id]` · `/goal --bg <objective>`".into(),
    }
}

/// A goal's state in a few lines.
pub fn describe(g: &Goal) -> String {
    let mut s = format!(
        "Goal `{}` · **{}** · {}/{} criteria passing · {} iteration(s) · {} · {}\n{}",
        g.id,
        g.state.label(),
        g.passing.len(),
        g.criteria.len(),
        g.iterations,
        crate::util::usd(g.cost_usd),
        crate::util::duration(g.active_secs),
        g.objective
    );
    if let Some(r) = &g.reason {
        s.push_str(&format!("\nReason: {r}"));
    }
    for c in &g.criteria {
        s.push_str(&format!(
            "\n- {} {} {}",
            if g.passing.contains(&c.id) {
                "✅"
            } else {
                "❌"
            },
            c.id,
            c.title
        ));
    }
    s.push_str(&format!(
        "\nProgress log: `{}`",
        g.progress_path().display()
    ));
    if !matches!(g.state, GoalState::Done) {
        s.push_str(&format!("\n`/goal resume {}` continues it.", g.id));
    }
    s
}

// ------------------------------------------------------------------ the loop

fn status(agent: &Agent, g: &Goal) {
    let line = g.status_line();
    if let Some(c) = active() {
        *c.status.lock().unwrap() = line.clone();
    }
    agent.env.emit(Event::Status {
        key: "goal".into(),
        lines: vec![line],
    });
}

fn new_id() -> String {
    format!(
        "{}-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S"),
        &uuid::Uuid::new_v4().simple().to_string()[..4]
    )
}

// ------------------------------------------------------------------ criteria

/// How Chanto runs every criterion, the same for the baseline, after every
/// iteration and in the agent's instructions: from the project root, with
/// the goal directory and the project root in the environment, and the
/// project root on PYTHONPATH.
pub fn criteria_env(g: &Goal) -> Vec<(String, String)> {
    let root = g.root.display().to_string();
    let py = match std::env::var("PYTHONPATH") {
        Ok(p) if !p.is_empty() => format!("{root}{}{p}", if cfg!(windows) { ";" } else { ":" }),
        _ => root.clone(),
    };
    vec![
        ("CHANTO_GOAL_DIR".into(), g.dir.display().to_string()),
        ("CHANTO_PROJECT_ROOT".into(), root),
        ("PYTHONPATH".into(), py),
    ]
}

/// The shell line that runs `check` exactly as Chanto does.
fn reproduce(g: &Goal, check: &str) -> String {
    format!(
        "cd {} && CHANTO_GOAL_DIR={} CHANTO_PROJECT_ROOT={} PYTHONPATH={} {check}",
        g.root.display(),
        g.dir.display(),
        g.root.display(),
        g.root.display()
    )
}

/// The execution contract, stated wherever criteria are written or run.
fn contract(g: &Goal) -> String {
    format!(
        "How chanto runs each criterion (the baseline, and after every iteration):\n\
         - `check` is a shell command run with the PROJECT ROOT ({root}) as its working directory, not the goal directory.\n\
         - Its environment has CHANTO_GOAL_DIR={dir} (the goal directory), CHANTO_PROJECT_ROOT={root}, and the project root on PYTHONPATH.\n\
         - Refer to scripts in the goal directory through the variable, quoted: `python3 \"$CHANTO_GOAL_DIR/checks/test_x.py\"`. A bare `checks/...` path points into the project and won't be found.\n\
         - Scripts reach the project through the working directory or $CHANTO_PROJECT_ROOT; Python can `import` project modules directly (don't compute paths from the script's own location).\n\
         - Exit status 0 = met, anything else = not met. A check that can't run at all (missing script, command not found, import errors for project modules) is a broken criterion, not a failing one.\n\
         - Try a check exactly as chanto runs it: `{repro}`",
        root = g.root.display(),
        dir = g.dir.display(),
        repro = reproduce(g, "<check>")
    )
}

fn spec_prompt(g: &Goal) -> String {
    format!(
        "Goal mode, step 1: write acceptance criteria. Do NOT implement anything yet.\n\nThe user wants: {objective}\n\n\
         Explore the codebase (read-only) to learn how it's built, run and tested. Then define how we will KNOW the goal is met, as executable acceptance criteria the user can approve without reading code:\n\
         - Write them to {dir}/criteria.json as a JSON array: [{{\"id\": \"c1\", \"title\": \"<user-visible behavior, plain language>\", \"check\": \"<shell command; exit 0 = criterion met>\"}}].\n\
         - Put any test or helper scripts the checks need under {dir}/checks/. Only write files inside {dir}/ in this step.\n\
         - Cover the main behavior, the important edge cases and error handling, and anything the user explicitly asked for. 3-10 criteria; each check fast and deterministic (no network beyond localhost).\n\
         - Checks for new behavior must FAIL right now (nothing is built yet) and pass once it is correct. They must fail because the behavior is missing, never because the check itself can't run.\n\n\
         {contract}\n\nRun each check that way before you finish, and finish with a short summary of the criteria.",
        objective = g.objective,
        dir = g.dir.display(),
        contract = contract(g)
    )
}

fn fix_prompt(g: &Goal, problems: &[(String, String)], feedback: Option<&str>) -> String {
    let mut s = format!(
        "Goal criteria need fixing before they are locked. Do NOT change the project; only write inside {}/.\n\nObjective: {}\n\n",
        g.dir.display(),
        g.objective
    );
    if !problems.is_empty() {
        s.push_str(
            "These criteria can't run the way chanto runs them, so they don't test the project:\n",
        );
        for (id, why) in problems {
            s.push_str(&format!("- {id}: {why}\n"));
        }
    }
    if let Some(f) = feedback {
        s.push_str(&format!("\nThe user says: {f}\n"));
    }
    s.push_str(&format!(
        "\n{}\n\nFix {}/criteria.json and the scripts under {}/ so every check runs. Where the behavior is still missing it should fail with exit status 1 because of that, not because the check can't run. Keep criteria that are fine as they are. Finish with a short summary of what you changed.",
        contract(g),
        g.dir.display(),
        g.dir.display()
    ));
    s
}

/// Run the criteria under the contract.
pub async fn run_criteria(g: &Goal, criteria: &[Criterion]) -> Vec<CheckRun> {
    let env = criteria_env(g);
    let mut out = Vec::new();
    for c in criteria {
        let r = crate::guardrails::run_check_env(&c.check, &g.root, Duration::from_secs(300), &env)
            .await;
        out.push(CheckRun {
            criterion: c.clone(),
            ok: r.ok,
            output: r.output,
            secs: r.secs,
        });
    }
    out
}

/// Why a failing criterion couldn't run at all (rather than finding the
/// behavior missing), if it looks that way.
pub fn infra_problem(g: &Goal, r: &CheckRun) -> Option<String> {
    if r.ok {
        return None;
    }
    let out = r.output.as_str();
    let lower = out.to_lowercase();
    let dir = g.dir.display().to_string();
    // A script named relative to the project root that only exists in the goal directory.
    for tok in r.criterion.check.split_whitespace() {
        let t = tok.trim_matches(|c| c == '"' || c == '\'' || c == ';' || c == '(' || c == ')');
        if t.starts_with('-') || t.contains('$') || t.contains('=') || Path::new(t).is_absolute() {
            continue;
        }
        let script = [".py", ".sh", ".js", ".mjs", ".cjs", ".ts", ".rb", ".pl"]
            .iter()
            .any(|e| t.ends_with(e));
        if (script || t.contains('/')) && !g.root.join(t).exists() && g.dir.join(t).exists() {
            return Some(format!(
                "`{t}` is in the goal directory, but checks run from the project root: write `\"$CHANTO_GOAL_DIR/{t}\"`"
            ));
        }
    }
    if lower.contains("can't open file") || lower.contains("cannot open file") {
        return Some(format!(
            "the check's script can't be opened: {}",
            crate::util::one_line(
                out.lines()
                    .find(|l| l.to_lowercase().contains("open file"))
                    .unwrap_or(out),
                200
            )
        ));
    }
    if lower.contains("command not found")
        || lower.contains("is not recognized as an internal or external command")
        || out.lines().any(|l| l.ends_with(": not found"))
    {
        return Some(format!(
            "a command in the check isn't available: {}",
            crate::util::one_line(
                out.lines()
                    .find(|l| l.contains("not found") || l.contains("not recognized"))
                    .unwrap_or(out),
                200
            )
        ));
    }
    // An import of a module the project does have: a path problem, not missing code.
    static MODULE_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"No module named '([A-Za-z0-9_]+)"#).unwrap()
    });
    if let Some(m) = MODULE_RE.captures(out).map(|c| c[1].to_string())
        && (g.root.join(format!("{m}.py")).exists() || g.root.join(&m).is_dir())
    {
        return Some(format!(
            "it can't import the project's `{m}` module (checks run from the project root, which is on PYTHONPATH)"
        ));
    }
    if (out.contains("SyntaxError") || out.contains("IndentationError")) && out.contains(&dir) {
        return Some("a script in the goal directory has a syntax error".into());
    }
    if out.starts_with("timed out after") {
        return Some(format!("it {}", out.trim()));
    }
    None
}

fn problems(g: &Goal, runs: &[CheckRun]) -> Vec<(String, String)> {
    runs.iter()
        .filter_map(|r| infra_problem(g, r).map(|p| (r.criterion.id.clone(), p)))
        .collect()
}

/// The outcome of agreeing on criteria.
enum Agreed {
    /// Approved and locked; the baseline run.
    Yes(Vec<CheckRun>),
    No(GoalState, String),
}

/// Have the agent write (or fix) the criteria in plan mode, run them under
/// the contract, let it repair criteria that can't run, and get approval.
/// On approval the criteria are locked.
async fn agree(
    agent: &mut Agent,
    goal: &mut Goal,
    mut prompt: String,
    cancel: &CancellationToken,
) -> Result<Agreed> {
    let env = agent.env.clone();
    crate::permissions::allow_plan_writes(&goal.dir);
    goal.locked.clear();
    let mut fixes = 0;
    loop {
        let (mark, t0) = (env.accounting.total(), Instant::now());
        let prev_mode = env.mode();
        env.set_mode(crate::permissions::Mode::Plan);
        let end = agent.run_turn(&prompt, vec![], cancel.clone()).await;
        env.set_mode(prev_mode);
        agent.save_session();
        goal.cost_usd += env.accounting.total() - mark;
        goal.active_secs += t0.elapsed().as_secs();
        match end {
            Ok(TurnEnd::Interrupted) => {
                return Ok(Agreed::No(
                    GoalState::Paused,
                    "interrupted while writing the criteria".into(),
                ));
            }
            Err(e) => {
                return Ok(Agreed::No(
                    GoalState::Paused,
                    format!("the criteria turn failed: {e:#}"),
                ));
            }
            Ok(TurnEnd::Done(_)) => {}
        }
        let criteria = match outcome::load(&goal.dir) {
            Ok(c) => c,
            Err(e) => return Ok(Agreed::No(GoalState::Stopped, format!("{e:#}"))),
        };
        env.emit(Event::Busy(Some(
            "Running the criteria before any change…".into(),
        )));
        let baseline = run_criteria(goal, &criteria).await;
        env.emit(Event::Busy(None));
        let broken = problems(goal, &baseline);
        let mut q = format!(
            "**Goal:** {}\n\n**Acceptance criteria** (done = all pass):\n",
            goal.objective
        );
        for b in &baseline {
            let flag = match broken.iter().find(|(id, _)| *id == b.criterion.id) {
                Some((_, why)) => format!("  ⚠ can't run: {why}"),
                None if b.ok => "  ⚠ already passes".into(),
                None => String::new(),
            };
            q.push_str(&format!(
                "- **{}** {}\n  `{}`{flag}\n",
                b.criterion.id, b.criterion.title, b.criterion.check
            ));
        }
        let l = &goal.limits;
        let fix = "Let the agent fix them".to_string();
        let opts = if broken.is_empty() {
            q.push_str(&format!(
                "\nApprove these? The agent then works in iterations until they all pass. The criteria are locked from now on. It stops after {} iterations, after {} in a row without a new passing criterion{}{}. (Or type what to change.)",
                l.max_iterations,
                l.max_stale,
                l.max_cost_usd.map(|c| format!(", at ${c:.2}")).unwrap_or_default(),
                l.max_minutes.map(|m| format!(", or after {m} minutes")).unwrap_or_default()
            ));
            vec!["Approve and start".to_string(), "Cancel".to_string()]
        } else {
            q.push_str(&format!(
                "\n{} criterion/criteria can't run the way chanto runs them (from the project root, with $CHANTO_GOAL_DIR), so they would fail whatever the agent does. Let the agent fix them before they are locked?",
                broken.len()
            ));
            vec![
                fix.clone(),
                "Approve anyway".to_string(),
                "Cancel".to_string(),
            ]
        };
        let answer = if env.interactive {
            env.frontend.question(&q, &opts).await
        } else if broken.is_empty() {
            env.emit(Event::Info(format!(
                "{q}\n(non-interactive: approved automatically)"
            )));
            Some(opts[0].clone())
        } else if fixes < 2 {
            env.emit(Event::Info(format!(
                "{q}\n(non-interactive: asking the agent to fix them)"
            )));
            Some(fix.clone())
        } else {
            let list = broken
                .iter()
                .map(|(id, why)| format!("{id}: {why}"))
                .collect::<Vec<_>>()
                .join("; ");
            return Ok(Agreed::No(
                GoalState::Stopped,
                format!(
                    "the criteria can't run as chanto runs them ({list}); fix them in `{}` and resume",
                    goal.dir.display()
                ),
            ));
        };
        match answer.as_deref() {
            Some("Approve and start") | Some("Approve anyway") => {
                goal.criteria = criteria;
                goal.locked = read_files(&goal.dir);
                return Ok(Agreed::Yes(baseline));
            }
            None | Some("Cancel") => {
                return Ok(Agreed::No(
                    GoalState::Stopped,
                    format!(
                        "criteria not approved (they are in `{}`)",
                        goal.dir.display()
                    ),
                ));
            }
            Some(a) if a == fix => prompt = fix_prompt(goal, &broken, None),
            Some(other) => prompt = fix_prompt(goal, &broken, Some(other)),
        }
        fixes += 1;
        if fixes > 5 {
            return Ok(Agreed::No(
                GoalState::Stopped,
                "the criteria couldn't be agreed after 5 rounds".into(),
            ));
        }
    }
}

/// Start a goal: agree on acceptance criteria, then work until they pass.
/// Returns the final state and the report.
pub async fn start(
    agent: &mut Agent,
    objective: &str,
    base: &Path,
    id: Option<String>,
    limits: Limits,
    cancel: CancellationToken,
) -> Result<(GoalState, String)> {
    let env = agent.env.clone();
    let root = agent
        .cwd_override
        .clone()
        .unwrap_or_else(|| env.root.clone());
    let id = id.unwrap_or_else(new_id);
    let dir = base.join(&id);
    std::fs::create_dir_all(&dir)?;
    let now = chrono::Local::now().to_rfc3339();
    let mut goal = Goal {
        id: id.clone(),
        objective: objective.trim().to_string(),
        root: root.clone(),
        dir: dir.clone(),
        state: GoalState::Criteria,
        created: now.clone(),
        updated: now,
        criteria: vec![],
        locked: BTreeMap::new(),
        passing: vec![],
        ever_passed: vec![],
        iterations: 0,
        stale: 0,
        limits,
        cost_usd: 0.0,
        active_secs: 0,
        reason: None,
        guidance: None,
    };
    goal.save()?;
    status(agent, &goal);
    let prompt = spec_prompt(&goal);
    let baseline = match agree(agent, &mut goal, prompt, &cancel).await? {
        Agreed::Yes(b) => b,
        Agreed::No(state, why) => {
            goal.state = state;
            goal.reason = Some(why.clone());
            goal.save()?;
            env.emit(Event::Status {
                key: "goal".into(),
                lines: vec![],
            });
            return Ok((
                state,
                format!(
                    "Goal `{}` {}: {why}\n{}",
                    goal.id,
                    state.label(),
                    describe(&goal)
                ),
            ));
        }
    };
    goal.passing = baseline
        .iter()
        .filter(|r| r.ok)
        .map(|r| r.criterion.id.clone())
        .collect();
    goal.ever_passed = goal.passing.clone();
    goal.state = GoalState::Active;
    append_progress(
        &goal,
        &format!(
            "# Goal {}\n\nObjective: {}\n\nApproved {}: {}/{} criteria already pass.\n",
            goal.id,
            goal.objective,
            chrono::Local::now().format("%Y-%m-%d %H:%M"),
            goal.passing.len(),
            goal.criteria.len()
        ),
    );
    goal.save()?;
    drive(agent, goal, baseline, cancel).await
}

/// Continue a paused, blocked or stopped goal (`id`: a unique prefix, or
/// the newest unfinished goal of this project). Limits count from here.
pub async fn resume(
    agent: &mut Agent,
    base: &Path,
    id: Option<&str>,
    limits: Option<Limits>,
    cancel: CancellationToken,
) -> Result<(GoalState, String)> {
    let root = agent
        .cwd_override
        .clone()
        .unwrap_or_else(|| agent.env.root.clone());
    let mut goal = find(base, id, &root)?;
    if active().is_some_and(|c| c.id == goal.id) {
        bail!("goal {} is already running", goal.id);
    }
    if let Some(l) = limits {
        goal.limits = l;
    }
    match goal.state {
        GoalState::Done => {
            return Ok((
                GoalState::Done,
                format!("Goal `{}` is already done.\n{}", goal.id, describe(&goal)),
            ));
        }
        GoalState::Criteria => {
            let objective = goal.objective.clone();
            let _ = std::fs::remove_dir_all(&goal.dir);
            return start(agent, &objective, base, Some(goal.id), goal.limits, cancel).await;
        }
        _ => {}
    }
    goal.stale = 0;
    let criteria_problem = goal
        .reason
        .as_deref()
        .and_then(|r| r.strip_prefix("criteria problem: "))
        .map(str::to_string);
    goal.reason = None;
    append_progress(
        &goal,
        &format!(
            "\n_Resumed {}._\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M")
        ),
    );
    let mut last = vec![];
    if let Some(why) = criteria_problem {
        // Stopped on its criteria: fix and re-agree them before working on.
        let why = why
            .split(". The project may")
            .next()
            .unwrap_or(&why)
            .to_string();
        match reagree(agent, &mut goal, &[("criteria".into(), why)], None, &cancel).await? {
            Ok(runs) => last = runs,
            Err((state, why)) => {
                goal.state = state;
                goal.reason = Some(why.clone());
                goal.save()?;
                return Ok((
                    state,
                    format!(
                        "Goal `{}` {}: {why}\n{}",
                        goal.id,
                        state.label(),
                        describe(&goal)
                    ),
                ));
            }
        }
        goal.save()?;
    }
    drive(agent, goal, last, cancel).await
}

fn iteration_prompt(g: &Goal, last: &[CheckRun], notes: &[String]) -> String {
    let mut s = format!(
        "Goal mode, iteration {}. You keep working on this goal across iterations until every acceptance criterion passes.\n\nObjective: {}\n\nAcceptance criteria (locked; chanto runs them after every iteration, from `{}`):\n",
        g.iterations,
        g.objective,
        g.dir.display()
    );
    for c in &g.criteria {
        let ok = g.passing.contains(&c.id);
        s.push_str(&format!(
            "- {} {} {}: `{}`\n",
            if ok { "✅" } else { "❌" },
            c.id,
            c.title,
            c.check
        ));
        if let Some(r) = last.iter().find(|r| r.criterion.id == c.id && !r.ok) {
            let out = crate::util::truncate_middle(r.output.trim(), 600);
            if !out.is_empty() {
                s.push_str(&format!(
                    "  last output:\n  ```\n{}\n  ```\n",
                    out.lines()
                        .map(|l| format!("  {l}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
            }
        }
    }
    if let Some(p) = progress_tail(g, 2500) {
        s.push_str(&format!(
            "\nProgress log so far ({}):\n{p}\n",
            g.progress_path().display()
        ));
    }
    for n in notes {
        s.push_str(&format!("\n⚠ {n}\n"));
    }
    s.push_str(&format!("\n{}\n", contract(g)));
    s.push_str(
        "\nThis iteration: make a short plan for the failing criteria, implement the next concrete step, and run the relevant checks yourself, the way chanto runs them. \
         Finish with a brief summary of what you changed and what remains; it goes into the progress log for the next iteration. \
         Don't edit the criteria or their scripts. If you are sure a criterion itself is wrong or can't run as chanto runs it (not that the project is wrong), \
         end with a line `CRITERIA PROBLEM: <criterion id and why>` and the user can unlock it. If you are blocked on something only the user can provide \
         (a credential, a decision, access), end with a line `BLOCKED: <what you need>`.",
    );
    s
}

/// Run iterations until the goal is met, a limit is hit, or it is paused,
/// stopped or blocked.
async fn drive(
    agent: &mut Agent,
    mut goal: Goal,
    mut last: Vec<CheckRun>,
    cancel: CancellationToken,
) -> Result<(GoalState, String)> {
    let env = agent.env.clone();
    let ctl = Arc::new(Control {
        id: goal.id.clone(),
        objective: goal.objective.clone(),
        pause: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        turn: Mutex::new(None),
        status: Mutex::new(String::new()),
    });
    *ACTIVE.lock().unwrap() = Some(ctl.clone());
    let _unregister = Unregister(goal.id.clone());
    agent.goal = Some(goal.dir.clone());
    goal.state = GoalState::Active;
    goal.save()?;
    let started = Instant::now();
    let cost0 = env.accounting.total();
    let deadline = goal.limits.max_minutes.map(|m| Duration::from_secs(m * 60));
    let protect = env.config.guardrails.protect_tests != Some(false);
    let tests = protect.then(|| crate::guardrails::snapshot(&goal.root));
    env.emit(Event::Busy(Some("Running the project's checks…".into())));
    let project_before = run_project_checks(&env, &goal.root).await;
    env.emit(Event::Busy(None));
    let mut notes: Vec<String> = goal
        .guidance
        .take()
        .map(|g| vec![format!("The user's answer to what you needed: {g}")])
        .into_iter()
        .flatten()
        .collect();
    let mut run_iterations = 0u32;
    let mut time_up = false;
    let (state, reason) = loop {
        status(agent, &goal);
        match requested(&goal.dir).as_deref() {
            Some("stop") => ctl.stop.store(true, Ordering::SeqCst),
            Some("pause") => ctl.pause.store(true, Ordering::SeqCst),
            _ => {}
        }
        if ctl.stop.load(Ordering::SeqCst) {
            break (GoalState::Stopped, "stopped by you".to_string());
        }
        if ctl.pause.load(Ordering::SeqCst) || cancel.is_cancelled() {
            break (GoalState::Paused, "paused by you".to_string());
        }
        if run_iterations >= goal.limits.max_iterations {
            break (
                GoalState::Stopped,
                format!(
                    "iteration limit reached ({} iterations)",
                    goal.limits.max_iterations
                ),
            );
        }
        if let Some(max) = goal
            .limits
            .max_cost_usd
            .filter(|m| env.accounting.total() - cost0 >= *m)
        {
            break (
                GoalState::Stopped,
                format!("goal budget of ${max:.2} reached"),
            );
        }
        if deadline.is_some_and(|d| started.elapsed() >= d) {
            break (
                GoalState::Stopped,
                format!(
                    "time limit of {} minutes reached",
                    goal.limits.max_minutes.unwrap_or(0)
                ),
            );
        }
        run_iterations += 1;
        goal.iterations += 1;
        goal.save()?;
        status(agent, &goal);
        let (mark, t) = (env.accounting.total(), Instant::now());
        let turn = cancel.child_token();
        *ctl.turn.lock().unwrap() = Some(turn.clone());
        let timer = deadline.map(|d| {
            let (turn, left) = (turn.clone(), d.saturating_sub(started.elapsed()));
            tokio::spawn(async move {
                tokio::time::sleep(left).await;
                turn.cancel();
            })
        });
        let res = agent
            .run_turn(
                &iteration_prompt(&goal, &last, &notes),
                vec![],
                turn.clone(),
            )
            .await;
        if let Some(t) = timer {
            time_up = t.is_finished();
            t.abort();
        }
        *ctl.turn.lock().unwrap() = None;
        agent.save_session();
        goal.cost_usd += env.accounting.total() - mark;
        goal.active_secs += t.elapsed().as_secs();
        let text = match res {
            Err(e) => break (GoalState::Paused, format!("the iteration failed: {e:#}")),
            Ok(TurnEnd::Interrupted) if ctl.stop.load(Ordering::SeqCst) => {
                break (GoalState::Stopped, "stopped by you".into());
            }
            Ok(TurnEnd::Interrupted) if time_up => {
                break (
                    GoalState::Stopped,
                    format!(
                        "time limit of {} minutes reached",
                        goal.limits.max_minutes.unwrap_or(0)
                    ),
                );
            }
            Ok(TurnEnd::Interrupted) => break (GoalState::Paused, "interrupted".into()),
            Ok(TurnEnd::Done(t)) if t.starts_with("USD budget reached") => {
                break (GoalState::Stopped, crate::util::one_line(&t, 200));
            }
            Ok(TurnEnd::Done(t)) => t,
        };

        // Evidence: Chanto runs the locked criteria itself.
        notes.clear();
        let tampered = goal.restore_locked();
        agent.forget_written(&goal.dir);
        if !tampered.is_empty() {
            notes.push(format!(
                "You changed the locked acceptance criteria ({}); chanto restored them. Make the criteria pass by changing the project, never the criteria.",
                tampered.join(", ")
            ));
        }
        env.emit(Event::Busy(Some("Running the acceptance criteria…".into())));
        last = run_criteria(&goal, &goal.criteria).await;
        let weakened = tests.as_ref().map(|s| s.violations()).unwrap_or_default();
        if !weakened.is_empty() {
            notes.push(format!("Existing tests were weakened or deleted, so this doesn't count: {}. Restore them; fix the code, not the tests.", weakened.join("; ")));
        }
        let passing: Vec<String> = last
            .iter()
            .filter(|r| r.ok)
            .map(|r| r.criterion.id.clone())
            .collect();
        let new: Vec<String> = passing
            .iter()
            .filter(|id| !goal.ever_passed.contains(id))
            .cloned()
            .collect();
        goal.ever_passed.extend(new.iter().cloned());
        goal.passing = passing;
        let honest = tampered.is_empty() && weakened.is_empty();
        let mut met = honest && goal.passing.len() == goal.criteria.len();
        if met {
            // Done also means the project's own checks still pass.
            let failed = project_checks(&env, &goal.root, &project_before).await;
            met = failed.is_empty();
            notes.extend(failed);
        }
        env.emit(Event::Busy(None));
        let blocked = text
            .lines()
            .find_map(|l| {
                l.trim()
                    .trim_start_matches(['*', '_'])
                    .strip_prefix("BLOCKED:")
            })
            .map(|s| s.trim().to_string());
        let progressed = honest && !new.is_empty();
        goal.stale = if progressed || met { 0 } else { goal.stale + 1 };
        let mut entry = format!(
            "\n## Iteration {} · {} · {}/{} passing{}\n{}\n",
            goal.iterations,
            chrono::Local::now().format("%Y-%m-%d %H:%M"),
            goal.passing.len(),
            goal.criteria.len(),
            if new.is_empty() {
                String::new()
            } else {
                format!(" (new: {})", new.join(", "))
            },
            crate::util::truncate(text.trim(), 1200)
        );
        let failing: Vec<&str> = goal
            .criteria
            .iter()
            .filter(|c| !goal.passing.contains(&c.id))
            .map(|c| c.id.as_str())
            .collect();
        if !failing.is_empty() {
            entry.push_str(&format!("Still failing: {}\n", failing.join(", ")));
        }
        for n in &notes {
            entry.push_str(&format!("⚠ {}\n", crate::util::one_line(n, 300)));
        }
        append_progress(&goal, &entry);
        if met {
            break (GoalState::Done, String::new());
        }
        // A criterion that can't run (or that the agent shows is wrong) isn't
        // the project's problem: iterating would only end in the no-progress stop.
        let mut broken = problems(&goal, &last);
        if let Some(c) = text.lines().find_map(|l| {
            l.trim()
                .trim_start_matches(['*', '_'])
                .strip_prefix("CRITERIA PROBLEM:")
        }) {
            broken.push(("the agent".into(), c.trim().to_string()));
        }
        if !broken.is_empty() {
            let list = broken
                .iter()
                .map(|(id, why)| format!("{id}: {why}"))
                .collect::<Vec<_>>()
                .join("; ");
            append_progress(&goal, &format!("⚠ Criteria problem: {list}\n"));
            if !env.interactive {
                break (
                    GoalState::Blocked,
                    format!(
                        "criteria problem: {list}. The project may already be right; `/goal resume {}` lets the agent fix the criteria for you to approve again",
                        goal.id
                    ),
                );
            }
            let q = format!(
                "**Criteria problem** in goal `{}`: {list}\n\nThe project may already be right, but a locked criterion can't show it. Unlock the criteria so the agent fixes them (you approve them again)? (Or type what to change.)",
                goal.id
            );
            let opts = vec![
                "Unlock and fix the criteria".to_string(),
                "Keep going".to_string(),
                "Stop the goal".to_string(),
            ];
            let feedback = match env.frontend.question(&q, &opts).await.as_deref() {
                None | Some("Stop the goal") => {
                    break (GoalState::Stopped, format!("criteria problem: {list}"));
                }
                Some("Keep going") => {
                    notes.push(format!("chanto flagged a criteria problem ({list}); the user chose to keep the criteria. Make them pass as they are."));
                    None
                }
                Some("Unlock and fix the criteria") => Some(None),
                Some(other) => Some(Some(other.to_string())),
            };
            if let Some(feedback) = feedback {
                match reagree(agent, &mut goal, &broken, feedback.as_deref(), &cancel).await? {
                    Ok(runs) => {
                        last = runs;
                        if goal.passing.len() == goal.criteria.len()
                            && project_checks(&env, &goal.root, &project_before)
                                .await
                                .is_empty()
                        {
                            break (GoalState::Done, String::new());
                        }
                        goal.save()?;
                        continue;
                    }
                    Err(stop) => break stop,
                }
            }
        }
        if let Some(why) = blocked {
            if !env.interactive {
                break (GoalState::Blocked, why);
            }
            let q = format!(
                "**Goal blocked:** {why}\n\nGoal: {}\n\nAnswer to unblock it (the agent continues with your answer), or pause or stop the goal.",
                goal.objective
            );
            let opts = vec!["Pause the goal".to_string(), "Stop the goal".to_string()];
            match env.frontend.question(&q, &opts).await {
                None => break (GoalState::Blocked, why),
                Some(a) if a == "Pause the goal" => {
                    break (GoalState::Paused, format!("blocked: {why}"));
                }
                Some(a) if a == "Stop the goal" => {
                    break (GoalState::Stopped, format!("blocked: {why}"));
                }
                Some(a) => {
                    append_progress(
                        &goal,
                        &format!("The user answered: {}\n", crate::util::one_line(&a, 500)),
                    );
                    notes.push(format!("You were blocked ({why}). The user's answer: {a}"));
                    goal.stale = 0;
                }
            }
        }
        if goal.stale >= goal.limits.max_stale {
            let still: Vec<String> = last
                .iter()
                .filter(|r| !r.ok)
                .map(|r| {
                    match r
                        .output
                        .trim()
                        .lines()
                        .last()
                        .filter(|l| !l.trim().is_empty())
                    {
                        Some(l) => {
                            format!("{} ({})", r.criterion.id, crate::util::one_line(l, 120))
                        }
                        None => r.criterion.id.clone(),
                    }
                })
                .collect();
            break (
                GoalState::Stopped,
                format!(
                    "no progress: {} iteration(s) in a row without a new passing criterion{}",
                    goal.stale,
                    if still.is_empty() {
                        String::new()
                    } else {
                        format!("; still failing: {}", still.join(", "))
                    }
                ),
            );
        }
        goal.save()?;
    };
    goal.state = state;
    goal.reason = (!reason.is_empty()).then_some(reason.clone());
    if state == GoalState::Blocked {
        goal.guidance = None;
    }
    append_progress(
        &goal,
        &format!(
            "\n**{}**{}\n",
            state.label(),
            if reason.is_empty() {
                String::new()
            } else {
                format!(": {reason}")
            }
        ),
    );
    goal.save()?;
    agent.goal = None;
    env.emit(Event::Status {
        key: "goal".into(),
        lines: vec![],
    });
    let text = report(&goal, &last);
    let _ = std::fs::write(goal.dir.join("report.md"), &text);
    Ok((state, text))
}

/// The project's own checks (verified mode's, or detected): (command, passed, output).
async fn run_project_checks(env: &crate::agent::Env, root: &Path) -> Vec<(String, bool, String)> {
    let checks = if env.config.guardrails.checks.is_empty() {
        crate::guardrails::detect_checks(root)
    } else {
        env.config.guardrails.checks.clone()
    };
    let timeout = Duration::from_secs(env.config.guardrails.timeout_secs.unwrap_or(600));
    let mut out = Vec::new();
    for c in checks {
        let r = crate::guardrails::run_check(&c, root, timeout).await;
        out.push((c, r.ok, r.output));
    }
    out
}

/// Done also means no project check that passed when the goal's work began
/// fails now. A check that already failed then (or can't run here, like a
/// test runner that finds no tests) doesn't hold the goal back.
async fn project_checks(
    env: &crate::agent::Env,
    root: &Path,
    before: &[(String, bool, String)],
) -> Vec<String> {
    run_project_checks(env, root)
        .await
        .into_iter()
        .filter(|(c, ok, _)| {
            !ok && before
                .iter()
                .find(|(b, _, _)| b == c)
                .is_none_or(|(_, was_ok, _)| *was_ok)
        })
        .map(|(c, _, out)| {
            format!(
                "Every criterion passes, but the project check `{c}` fails (it passed before):\n{}",
                crate::util::truncate_middle(out.trim(), 1500)
            )
        })
        .collect()
}

/// Unlock the criteria, have the agent fix them, and lock them again once
/// approved. Ok: the new run of the criteria; Err: why the goal ends.
async fn reagree(
    agent: &mut Agent,
    goal: &mut Goal,
    broken: &[(String, String)],
    feedback: Option<&str>,
    cancel: &CancellationToken,
) -> Result<std::result::Result<Vec<CheckRun>, (GoalState, String)>> {
    let prompt = fix_prompt(goal, broken, feedback);
    let runs = match agree(agent, goal, prompt, cancel).await? {
        Agreed::Yes(runs) => runs,
        Agreed::No(state, why) => return Ok(Err((state, why))),
    };
    goal.passing = runs
        .iter()
        .filter(|r| r.ok)
        .map(|r| r.criterion.id.clone())
        .collect();
    for id in &goal.passing {
        if !goal.ever_passed.contains(id) {
            goal.ever_passed.push(id.clone());
        }
    }
    goal.stale = 0;
    append_progress(
        goal,
        &format!(
            "\n_Criteria fixed and approved again: {}/{} pass._\n",
            goal.passing.len(),
            goal.criteria.len()
        ),
    );
    Ok(Ok(runs))
}

/// The final report: verdict, each criterion with its evidence, and how to continue.
pub fn report(g: &Goal, last: &[CheckRun]) -> String {
    let verdict = match g.state {
        GoalState::Done => format!(
            "✅ Done: all {} acceptance criteria pass.",
            g.criteria.len()
        ),
        st => format!(
            "⚠ {} with {}/{} criteria passing: {}.",
            crate::util::capitalize(st.label()),
            g.passing.len(),
            g.criteria.len(),
            g.reason.as_deref().unwrap_or("")
        ),
    };
    let mut s = format!(
        "# Goal report\n\n**Objective:** {}\n\n**Result:** {verdict}\n\n{} iteration(s) · {} · {}\n\n## Acceptance criteria\n",
        g.objective,
        g.iterations,
        crate::util::usd(g.cost_usd),
        crate::util::duration(g.active_secs)
    );
    for c in &g.criteria {
        let ok = g.passing.contains(&c.id);
        s.push_str(&format!(
            "\n- {} **{}** {} `{}`",
            if ok { "✅" } else { "❌" },
            c.id,
            c.title,
            c.check
        ));
        if let Some(r) = last.iter().find(|r| r.criterion.id == c.id && !r.ok) {
            s.push_str(&format!(
                "\n  ```\n{}\n  ```",
                crate::util::truncate_middle(r.output.trim(), 800)
            ));
        }
    }
    s.push_str(&format!(
        "\n\nProgress log: `{}`",
        g.progress_path().display()
    ));
    if g.state != GoalState::Done {
        s.push_str(&format!(
            "\nContinue with `/goal resume {id}` (or `chanto goal resume {id}`).",
            id = g.id
        ));
    }
    s.push('\n');
    s
}

// ------------------------------------------------------------------ CLI

pub const USAGE: &str = "Work toward an objective until agreed, locked acceptance criteria pass.

Usage:
  chanto goal \"<objective>\" [options]   agree on criteria, then work in iterations until they pass
  chanto goal --bg \"<objective>\"        the same, as a background run (approve with `chanto attach`)
  chanto goal list                       this project's goals
  chanto goal status [id]                one goal's state and criteria
  chanto goal resume [id]                continue a paused, blocked or stopped goal
  chanto goal pause|stop [id]            ask a running goal to pause or stop after its iteration

Options:
  --max-iterations <N>   stop after N work iterations (default: [goal] max_iterations, 20)
  --max-cost <USD>       session budget
  -m, --model, --provider, --base-url, --mode   as for `chanto`

Criteria run from the project root with CHANTO_GOAL_DIR and CHANTO_PROJECT_ROOT set
(the project root is also on PYTHONPATH). State lives in ~/.chanto/goals/<id>/.
Limits: [goal] max_iterations, max_stale_iterations, max_cost_usd, max_minutes.";

/// `chanto goal "<objective>" [--bg] [--max-iterations N]`, `chanto goal
/// list|status|resume|pause|stop [id]`. Returns the exit code.
pub async fn cli(cli: crate::Cli) -> Result<i32> {
    let words: Vec<String> = cli.prompt.iter().skip(1).cloned().collect();
    let root = match &cli.cwd {
        Some(p) => crate::util::canonical(p)?,
        None => std::env::current_dir()?,
    };
    let base = goals_dir();
    let id_arg = words.get(1).map(String::as_str);
    match words.first().map(String::as_str) {
        None => bail!(
            "usage: chanto goal \"<objective>\" [--bg] [--max-iterations N] | list | status [id] | resume [id] | pause [id] | stop [id]"
        ),
        Some("list") if words.len() == 1 => {
            println!(
                "{}",
                command(&root, "list")
                    .replace("```\n", "")
                    .replace("```", "")
                    .replace('`', "")
            );
            return Ok(0);
        }
        Some("status") if words.len() <= 2 => {
            let g = find(&base, id_arg, &root).or_else(|_| {
                list(&base)
                    .into_iter()
                    .find(|g| g.root == root)
                    .ok_or_else(|| anyhow::anyhow!("no goals for {}", root.display()))
            })?;
            println!("{}", describe(&g).replace("**", "").replace('`', ""));
            return Ok(0);
        }
        Some(cmd @ ("pause" | "stop")) if words.len() <= 2 => {
            let g = find(&base, id_arg, &root)?;
            std::fs::write(g.dir.join("control"), cmd)?;
            println!(
                "Asked goal {} to {cmd}; it does at the end of its current iteration.",
                g.id
            );
            return Ok(0);
        }
        _ => {}
    }
    let resume_id = (words.first().map(String::as_str) == Some("resume") && words.len() <= 2)
        .then(|| id_arg.unwrap_or("").to_string());
    let mut cfg_limits = crate::config::Config::load(&root, crate::config::is_trusted(&root))
        .map(|(c, _)| c.goal.limits())
        .unwrap_or_else(|_| GoalConfig::default().limits());
    if let Some(n) = cli.max_iterations {
        cfg_limits.max_iterations = n.max(1);
    }
    if cli.bg {
        if resume_id.is_some() {
            bail!("a background goal continues with `chanto runs resume <id>`");
        }
        let task = words.join(" ");
        let mut body = crate::runs::cli::new_run_body(&root, &task, Some(&cli));
        body["kind"] = serde_json::json!("goal");
        let run = crate::runs::cli::start(body).await?;
        println!(
            "Started goal {id} as a background run on branch {}.\n  chanto attach {id}       approve the criteria, answer prompts\n  chanto runs show {id}    progress\n  chanto runs apply {id}   bring its commits onto your branch",
            run.branch,
            id = run.id
        );
        return Ok(0);
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
    let res = match resume_id {
        Some(id) => resume(&mut agent, &base, Some(&id), Some(cfg_limits), cancel).await,
        None => {
            start(
                &mut agent,
                &words.join(" "),
                &base,
                None,
                cfg_limits,
                cancel,
            )
            .await
        }
    };
    env.shutdown().await;
    let (state, text) = res?;
    println!("\n{}", crate::ui::render_markdown(&text));
    Ok(if state == GoalState::Done { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tests::{make_env_with, mock_server, tc, tmpdir};
    use crate::permissions::Mode;
    use serde_json::json;

    fn limits(max_iterations: u32, max_stale: u32) -> Limits {
        Limits {
            max_iterations,
            max_stale,
            max_cost_usd: None,
            max_minutes: None,
        }
    }

    fn texts(seen: &Mutex<Vec<serde_json::Value>>) -> Vec<String> {
        seen.lock()
            .unwrap()
            .iter()
            .map(|b| b["messages"].to_string())
            .collect()
    }

    #[tokio::test]
    async fn goal_works_until_the_criteria_pass_and_keeps_them_locked() {
        let (dir, store) = (tmpdir("goal"), tmpdir("goal-store"));
        let crit = store.join("g1/criteria.json").display().to_string();
        let criteria = r#"[{"id": "c1", "title": "a exists", "check": "test -f a.txt"}, {"id": "c2", "title": "b says done", "check": "grep -q done b.txt"}]"#;
        let (base, seen) = mock_server(vec![
            // Criteria phase, in plan mode: the project is read-only, the goal directory isn't.
            json!({"tool_calls": tc("Write", json!({"file_path": "a.txt", "content": "early\n"}))}),
            json!({"tool_calls": tc("Write", json!({"file_path": crit, "content": criteria}))}),
            json!({"content": "Two criteria."}),
            // Iteration 1: loosens the criteria (undone), then does part of the work.
            json!({"tool_calls": tc("Read", json!({"file_path": crit}))}),
            json!({"tool_calls": tc("Write", json!({"file_path": crit, "content": "[{\"id\": \"c1\", \"title\": \"x\", \"check\": \"true\"}]"}))}),
            json!({"tool_calls": tc("Write", json!({"file_path": "a.txt", "content": "a\n"}))}),
            json!({"content": "Created a.txt; b.txt remains."}),
            // Iteration 2: the rest.
            json!({"tool_calls": tc("Write", json!({"file_path": "b.txt", "content": "done\n"}))}),
            json!({"content": "Created b.txt."}),
        ])
        .await;
        let env = make_env_with(&base, &dir, Mode::AcceptEdits, 32768, |_| {});
        let mut agent = Agent::new(env.clone());
        let (state, report) = start(
            &mut agent,
            "make a.txt and b.txt",
            &store,
            Some("g1".into()),
            limits(10, 3),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(state, GoalState::Done, "{report}");
        assert!(
            report.contains("Done: all 2 acceptance criteria pass"),
            "{report}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "a\n",
            "plan mode kept the project read-only"
        );
        assert_eq!(env.mode(), Mode::AcceptEdits);
        // The loosened criteria were put back, and the agent was told.
        assert_eq!(std::fs::read_to_string(&crit).unwrap(), criteria);
        let reqs = texts(&seen);
        assert_eq!(reqs.len(), 9);
        assert!(
            reqs[7].contains("You changed the locked acceptance criteria (criteria.json)"),
            "{}",
            reqs[7]
        );
        assert!(
            reqs[7].contains("iteration 2") && reqs[7].contains("Created a.txt; b.txt remains."),
            "progress carried into the next iteration"
        );
        // Progress and state on disk.
        let g = Goal::load(&store.join("g1")).unwrap();
        assert_eq!(
            (g.state, g.iterations, g.passing.len()),
            (GoalState::Done, 2, 2)
        );
        let progress = std::fs::read_to_string(g.progress_path()).unwrap();
        assert!(
            progress.contains("## Iteration 1")
                && progress.contains("## Iteration 2 ")
                && progress.contains("(new: c2)"),
            "{progress}"
        );
        assert!(
            progress.contains("locked acceptance criteria"),
            "{progress}"
        );
        assert!(agent.goal.is_none());
    }

    #[tokio::test]
    async fn goal_stops_without_progress_on_limits_and_when_blocked_then_resumes_after_a_restart() {
        let (dir, store) = (tmpdir("goal-stop"), tmpdir("goal-stop-store"));
        let crit = store.join("g2/criteria.json").display().to_string();
        let (base, seen) = mock_server(vec![
            json!({"tool_calls": tc("Write", json!({"file_path": crit, "content": r#"[{"id": "c1", "title": "never", "check": "test -f never.txt"}]"#}))}),
            json!({"content": "One criterion."}),
            json!({"content": "Tried one thing."}),
            json!({"content": "Tried another thing."}),
            // After a restart: one more iteration, then blocked.
            json!({"content": "Tried again."}),
            json!({"content": "I can't go on.\nBLOCKED: an API key for the payment sandbox"}),
        ])
        .await;
        let mk = || Agent::new(make_env_with(&base, &dir, Mode::AcceptEdits, 32768, |_| {}));
        let mut agent = mk();
        let (state, report) = start(
            &mut agent,
            "create never.txt somehow",
            &store,
            Some("g2".into()),
            limits(10, 2),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(state, GoalState::Stopped);
        assert!(report.contains("no progress: 2 iteration(s) in a row without a new passing criterion; still failing: c1"), "{report}");
        assert!(report.contains("/goal resume g2"), "{report}");
        let g = Goal::load(&store.join("g2")).unwrap();
        assert_eq!((g.state, g.iterations), (GoalState::Stopped, 2));

        // A new process: everything comes back from the goal directory.
        drop(agent);
        let mut agent = mk();
        let (state, report) = resume(
            &mut agent,
            &store,
            Some("g2"),
            Some(limits(1, 5)),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(state, GoalState::Stopped);
        assert!(
            report.contains("iteration limit reached (1 iterations)"),
            "{report}"
        );
        let last = texts(&seen).pop().unwrap();
        assert!(
            last.contains("iteration 3")
                && last.contains("Tried another thing.")
                && last.contains("create never.txt somehow"),
            "{last}"
        );

        // A spent budget stops before any model call.
        let before = seen.lock().unwrap().len();
        let l = Limits {
            max_cost_usd: Some(0.0),
            ..limits(5, 5)
        };
        let (state, report) = resume(
            &mut agent,
            &store,
            Some("g2"),
            Some(l),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(state, GoalState::Stopped);
        assert!(report.contains("goal budget of $0.00 reached"), "{report}");
        assert_eq!(seen.lock().unwrap().len(), before);

        // Blocked, unattended: it stops and says what it needs.
        let (state, report) = resume(
            &mut agent,
            &store,
            None,
            Some(limits(5, 5)),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(state, GoalState::Blocked);
        assert!(
            report.contains("an API key for the payment sandbox"),
            "{report}"
        );
        assert_eq!(Goal::load(&store.join("g2")).unwrap().iterations, 4);
    }

    #[tokio::test]
    async fn goal_survives_compaction() {
        let (dir, store) = (tmpdir("goal-compact"), tmpdir("goal-compact-store"));
        let crit = store.join("g3/criteria.json").display().to_string();
        let (base, seen) = mock_server(vec![
            json!({"tool_calls": tc("Write", json!({"file_path": crit, "content": r#"[{"id": "c1", "title": "a exists", "check": "test -f a.txt"}]"#}))}),
            json!({"content": "One criterion."}),
            // A big context: the next step compacts the conversation first.
            json!({"tool_calls": tc("Write", json!({"file_path": "a.txt", "content": "a\n"})), "prompt_tokens": 7900}),
            json!({"content": "SUMMARY OF THE WORK"}),
            json!({"content": "Wrote a.txt."}),
        ])
        .await;
        let env = make_env_with(&base, &dir, Mode::AcceptEdits, 8192, |_| {});
        let mut agent = Agent::new(env);
        let (state, report) = start(
            &mut agent,
            "make the alpha file",
            &store,
            Some("g3".into()),
            limits(5, 3),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(state, GoalState::Done, "{report}");
        let reqs = texts(&seen);
        assert_eq!(reqs.len(), 5);
        // After compaction the objective, criteria and progress come back.
        let after = &reqs[4];
        assert!(after.contains("SUMMARY OF THE WORK"), "{after}");
        assert!(
            after.contains("You are working toward a goal (id g3)")
                && after.contains("Objective: make the alpha file"),
            "{after}"
        );
        assert!(
            after.contains("c1 a exists") && after.contains("Progress so far"),
            "{after}"
        );
        assert!(
            !after.contains("Goal mode, iteration 1"),
            "the original prompt was compacted away"
        );
    }

    /// A frontend that answers questions from a script and records them.
    struct Answers(
        Mutex<Vec<(String, Vec<String>)>>,
        Mutex<std::collections::VecDeque<&'static str>>,
    );

    #[async_trait::async_trait]
    impl crate::agent::Frontend for Answers {
        fn emit(&self, _: Event) {}
        async fn permission(
            &self,
            _: crate::agent::PermissionRequest,
        ) -> crate::agent::PermissionReply {
            crate::agent::PermissionReply::Allow
        }
        async fn question(&self, q: &str, options: &[String]) -> Option<String> {
            self.0
                .lock()
                .unwrap()
                .push((q.to_string(), options.to_vec()));
            self.1.lock().unwrap().pop_front().map(str::to_string)
        }
    }

    fn interactive_env(base: &str, dir: &Path, fe: Arc<Answers>) -> Arc<crate::agent::Env> {
        let mut config = crate::config::Config::default();
        config.provider = crate::llm::ProviderKind::Openai;
        crate::agent::Env::new(crate::agent::EnvInit {
            config,
            provider: crate::llm::Provider::new(
                crate::llm::ProviderKind::Openai,
                base,
                None,
                32768,
            ),
            model: "mock".into(),
            mode: Mode::AcceptEdits,
            vision: false,
            cwd: dir.to_path_buf(),
            tools: crate::tools::Registry::builtin(),
            mcp: vec![],
            frontend: fe,
            interactive: true,
        })
    }

    /// The reported bug: the check script lives in the goal directory and
    /// imports the project; the criterion first names it relative to the
    /// goal directory. The baseline catches that, the agent fixes it before
    /// locking, and once the project is fixed the goal completes.
    #[tokio::test]
    async fn criteria_scripts_in_the_goal_dir_can_test_the_project() {
        let (dir, store) = (tmpdir("goal-calc"), tmpdir("goal-calc-store"));
        std::fs::write(dir.join("calc.py"), "def add(a, b):\n    return a - b\n").unwrap();
        std::fs::write(
            dir.join("test_calc.py"),
            "from calc import add\n\ndef test_add():\n    assert add(1, 1) == 2\n",
        )
        .unwrap();
        let g = store.join("g5");
        let script = "import sys\nimport calc\n\ncase = sys.argv[1]\nif case == 'add':\n    assert calc.add(2, 3) == 5, calc.add(2, 3)\nelif case == 'multiply':\n    assert calc.multiply(6, 7) == 42\nprint('ok', case)\n";
        let crit = |prefix: &str| {
            format!(
                r#"[{{"id": "c1", "title": "add adds", "check": "python3 {prefix}checks/run_checks.py add"}}, {{"id": "c2", "title": "multiply exists", "check": "python3 {prefix}checks/run_checks.py multiply"}}]"#
            )
        };
        let crit_path = g.join("criteria.json").display().to_string();
        let (base, seen) = mock_server(vec![
            json!({"tool_calls": tc("Write", json!({"file_path": g.join("checks/run_checks.py").display().to_string(), "content": script}))}),
            json!({"tool_calls": tc("Write", json!({"file_path": crit_path, "content": crit("")}))}),
            json!({"content": "Two criteria."}),
            // Fix round: the baseline found the script path broken.
            json!({"tool_calls": tc("Read", json!({"file_path": crit_path}))}),
            json!({"tool_calls": tc("Write", json!({"file_path": crit_path, "content": crit("\\\"$CHANTO_GOAL_DIR\\\"/")}))}),
            json!({"content": "Fixed the paths."}),
            // Iteration 1 fixes the project.
            json!({"tool_calls": tc("Read", json!({"file_path": "calc.py"}))}),
            json!({"tool_calls": tc("Write", json!({"file_path": "calc.py", "content": "def add(a, b):\n    return a + b\n\n\ndef multiply(a, b):\n    return a * b\n"}))}),
            json!({"content": "add adds now, and multiply exists."}),
        ])
        .await;
        let mut agent = Agent::new(make_env_with(&base, &dir, Mode::AcceptEdits, 32768, |_| {}));
        let (state, report) = start(
            &mut agent,
            "fix add, add multiply",
            &store,
            Some("g5".into()),
            limits(5, 3),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(state, GoalState::Done, "{report}");
        let reqs = texts(&seen);
        // The criteria prompt states the contract.
        assert!(
            reqs[0].contains("PROJECT ROOT") && reqs[0].contains("$CHANTO_GOAL_DIR/checks/"),
            "{}",
            reqs[0]
        );
        // The fix round named the problem.
        assert!(
            reqs[3].contains("can't run the way chanto runs them")
                && reqs[3].contains("c1: `checks/run_checks.py` is in the goal directory"),
            "{}",
            reqs[3]
        );
        let g = Goal::load(&g).unwrap();
        assert_eq!((g.iterations, g.passing.len()), (1, 2));
        assert!(g.criteria[0].check.contains("$CHANTO_GOAL_DIR"));
    }

    #[tokio::test]
    async fn done_needs_project_checks_that_passed_before_to_still_pass() {
        let (dir, store) = (tmpdir("goal-proj"), tmpdir("goal-proj-store"));
        std::fs::write(dir.join("state.txt"), "fine\n").unwrap();
        let crit_path = store.join("g8/criteria.json").display().to_string();
        let (base, seen) = mock_server(vec![
            json!({"tool_calls": tc("Write", json!({"file_path": crit_path, "content": r#"[{"id": "c1", "title": "a", "check": "test -f a.txt"}]"#}))}),
            json!({"content": "One."}),
            // Iteration 1 meets the criterion but breaks a check that passed.
            json!({"tool_calls": tc("Write", json!({"file_path": "a.txt", "content": "a\n"}))}),
            json!({"tool_calls": tc("Read", json!({"file_path": "state.txt"}))}),
            json!({"tool_calls": tc("Write", json!({"file_path": "state.txt", "content": "BROKEN\n"}))}),
            json!({"content": "Done, I think."}),
            json!({"tool_calls": tc("Write", json!({"file_path": "state.txt", "content": "fine\n"}))}),
            json!({"content": "Repaired state.txt."}),
        ])
        .await;
        // `false` already fails before the work: it doesn't hold the goal back.
        let env = make_env_with(&base, &dir, Mode::AcceptEdits, 32768, |c| {
            c.guardrails.checks = vec!["false".into(), "! grep -q BROKEN state.txt".into()]
        });
        let mut agent = Agent::new(env);
        let (state, report) = start(
            &mut agent,
            "make a.txt",
            &store,
            Some("g8".into()),
            limits(5, 3),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(state, GoalState::Done, "{report}");
        let reqs = texts(&seen);
        assert!(
            reqs[6].contains(
                "the project check `! grep -q BROKEN state.txt` fails (it passed before)"
            ),
            "{}",
            reqs[6]
        );
        assert!(!reqs[6].contains("check `false`"));
        assert_eq!(Goal::load(&store.join("g8")).unwrap().iterations, 2);
    }

    #[tokio::test]
    async fn criteria_that_cant_run_are_flagged_at_approval_and_can_be_unlocked_later() {
        let (dir, store) = (tmpdir("goal-infra"), tmpdir("goal-infra-store"));
        let crit_path = store.join("g6/criteria.json").display().to_string();
        let (base, seen) = mock_server(vec![
            json!({"tool_calls": tc("Write", json!({"file_path": crit_path, "content": r#"[{"id": "c1", "title": "done", "check": "no-such-tool-xyz --verify done.txt"}]"#}))}),
            json!({"content": "One criterion."}),
            // Iteration 1: does the work; the criterion still can't run.
            json!({"tool_calls": tc("Write", json!({"file_path": "done.txt", "content": "ok\n"}))}),
            json!({"content": "Created done.txt."}),
            // Unlocked: the agent fixes the criterion.
            json!({"tool_calls": tc("Read", json!({"file_path": crit_path}))}),
            json!({"tool_calls": tc("Write", json!({"file_path": crit_path, "content": r#"[{"id": "c1", "title": "done", "check": "test -f done.txt"}]"#}))}),
            json!({"content": "Uses test -f now."}),
        ])
        .await;
        let fe = Arc::new(Answers(
            Mutex::new(vec![]),
            Mutex::new(
                [
                    "Approve anyway",
                    "Unlock and fix the criteria",
                    "Approve and start",
                ]
                .into_iter()
                .collect(),
            ),
        ));
        let mut agent = Agent::new(interactive_env(&base, &dir, fe.clone()));
        let (state, report) = start(
            &mut agent,
            "make done.txt",
            &store,
            Some("g6".into()),
            limits(5, 3),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(state, GoalState::Done, "{report}");
        let qs = fe.0.lock().unwrap().clone();
        assert_eq!(qs.len(), 3, "{qs:?}");
        // 1. Approval flags the criterion that can't run and offers a fix.
        assert!(
            qs[0]
                .0
                .contains("⚠ can't run: a command in the check isn't available")
                && qs[0].1.contains(&"Let the agent fix them".to_string()),
            "{qs:?}"
        );
        // 2. After the work, the broken criterion is raised as a criteria problem, not looped on.
        assert!(
            qs[1].0.contains("Criteria problem")
                && qs[1]
                    .0
                    .contains("c1: a command in the check isn't available"),
            "{qs:?}"
        );
        // 3. The fixed criteria are approved again, then pass.
        assert!(
            qs[2].0.contains("`test -f done.txt`") && !qs[2].0.contains("can't run"),
            "{qs:?}"
        );
        assert_eq!(texts(&seen).len(), 7);
        let progress = std::fs::read_to_string(store.join("g6/progress.md")).unwrap();
        assert!(
            progress.contains("Criteria fixed and approved again: 1/1 pass"),
            "{progress}"
        );
    }

    #[tokio::test]
    async fn unattended_goals_stop_when_criteria_cant_be_made_to_run() {
        let (dir, store) = (tmpdir("goal-infra2"), tmpdir("goal-infra2-store"));
        let crit_path = store.join("g7/criteria.json").display().to_string();
        let (base, seen) = mock_server(vec![
            json!({"tool_calls": tc("Write", json!({"file_path": crit_path, "content": r#"[{"id": "c1", "title": "x", "check": "no-such-tool-xyz"}]"#}))}),
            json!({"content": "One."}),
            json!({"content": "Tried to fix."}),
            json!({"content": "Tried again."}),
        ])
        .await;
        let mut agent = Agent::new(make_env_with(&base, &dir, Mode::AcceptEdits, 32768, |_| {}));
        let (state, report) = start(
            &mut agent,
            "anything",
            &store,
            Some("g7".into()),
            limits(5, 3),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(state, GoalState::Stopped);
        assert!(report.contains("the criteria can't run as chanto runs them (c1: a command in the check isn't available"), "{report}");
        assert_eq!(
            texts(&seen).len(),
            4,
            "two fix rounds, then it stops without iterating"
        );
    }

    fn test_goal(root: &Path, dir: &Path) -> Goal {
        Goal {
            id: "g".into(),
            objective: "o".into(),
            root: root.to_path_buf(),
            dir: dir.to_path_buf(),
            state: GoalState::Active,
            created: String::new(),
            updated: String::new(),
            criteria: vec![],
            locked: BTreeMap::new(),
            passing: vec![],
            ever_passed: vec![],
            iterations: 0,
            stale: 0,
            limits: limits(1, 1),
            cost_usd: 0.0,
            active_secs: 0,
            reason: None,
            guidance: None,
        }
    }

    #[tokio::test]
    async fn infra_problems_are_told_apart_from_missing_behavior() {
        let (root, dir) = (tmpdir("infra-root"), tmpdir("infra-goal"));
        std::fs::write(root.join("calc.py"), "def add(a, b):\n    return a - b\n").unwrap();
        std::fs::create_dir_all(dir.join("checks")).unwrap();
        std::fs::write(
            dir.join("checks/t.py"),
            "import calc\nassert calc.add(2, 3) == 5\n",
        )
        .unwrap();
        std::fs::write(dir.join("checks/bad.py"), "def broken(:\n").unwrap();
        std::fs::write(dir.join("checks/other.py"), "import not_in_project\n").unwrap();
        let g = test_goal(&root, &dir);
        let crit = |id: &str, check: &str| Criterion {
            id: id.into(),
            title: id.into(),
            check: check.into(),
        };
        let cs = vec![
            crit("rel", "python3 checks/t.py"),
            crit("nocmd", "no-such-tool-xyz --x"),
            crit("syntax", "python3 \"$CHANTO_GOAL_DIR/checks/bad.py\""),
            crit("ok-fails", "python3 \"$CHANTO_GOAL_DIR/checks/t.py\""),
            crit("missing-data", "grep -q done out.txt"),
            crit(
                "missing-module",
                "python3 \"$CHANTO_GOAL_DIR/checks/other.py\"",
            ),
            crit("project-import", "cd / && python3 -c 'import calc'"),
        ];
        let runs = run_criteria(&g, &cs).await;
        let p = |id: &str| infra_problem(&g, runs.iter().find(|r| r.criterion.id == id).unwrap());
        assert!(
            p("rel")
                .unwrap()
                .contains("`checks/t.py` is in the goal directory"),
            "{:?}",
            p("rel")
        );
        assert!(
            p("nocmd").unwrap().contains("isn't available"),
            "{:?}",
            runs[1].output
        );
        assert!(
            p("syntax").unwrap().contains("syntax error"),
            "{:?}",
            runs[2].output
        );
        // Missing behavior: the check runs and the assertion fails (the project is on PYTHONPATH).
        assert!(
            !runs[3].ok && p("ok-fails").is_none(),
            "{:?}",
            runs[3].output
        );
        assert!(p("missing-data").is_none());
        // A module the project doesn't have could be the work still to do.
        assert!(p("missing-module").is_none(), "{:?}", runs[5].output);
        // Python started elsewhere still finds the project through PYTHONPATH.
        assert!(runs[6].ok, "{:?}", runs[6].output);
        let bad = CheckRun {
            criterion: crit("imp", "python3 x.py"),
            ok: false,
            output: "ModuleNotFoundError: No module named 'calc'".into(),
            secs: 0.0,
        };
        assert!(
            infra_problem(&g, &bad)
                .unwrap()
                .contains("can't import the project's `calc` module")
        );
        // The contract is the same everywhere: the environment and the reproduction line.
        let env = criteria_env(&g);
        assert_eq!(
            env[0],
            ("CHANTO_GOAL_DIR".to_string(), dir.display().to_string())
        );
        assert!(contract(&g).contains(&format!(
            "cd {} && CHANTO_GOAL_DIR={}",
            root.display(),
            dir.display()
        )));
    }

    #[test]
    fn status_and_commands() {
        let g = Goal {
            id: "g".into(),
            objective: "o".into(),
            root: PathBuf::from("/nowhere"),
            dir: PathBuf::from("/nowhere/g"),
            state: GoalState::Active,
            created: String::new(),
            updated: String::new(),
            criteria: vec![
                Criterion {
                    id: "c1".into(),
                    title: "t".into(),
                    check: "true".into(),
                },
                Criterion {
                    id: "c2".into(),
                    title: "u".into(),
                    check: "false".into(),
                },
            ],
            locked: BTreeMap::new(),
            passing: vec!["c1".into()],
            ever_passed: vec![],
            iterations: 3,
            stale: 0,
            limits: limits(1, 1),
            cost_usd: 0.0,
            active_secs: 0,
            reason: None,
            guidance: None,
        };
        assert_eq!(g.status_line(), "◎ goal 1/2 · iteration 3");
        assert!(
            crate::commands::is_immediate("/goal pause")
                && crate::commands::is_immediate("/goal")
                && crate::commands::is_immediate("/team status")
        );
        assert!(
            !crate::commands::is_immediate("/goal make it fast")
                && !crate::commands::is_immediate("/goal resume")
                && !crate::commands::is_immediate("/team add a cache")
        );
        let d = GoalConfig::default().limits();
        assert_eq!(
            (d.max_iterations, d.max_stale, d.max_cost_usd, d.max_minutes),
            (20, 3, None, Some(120))
        );
    }
}
