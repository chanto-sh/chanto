//! Spend guard: an estimate before work that multiplies cost (several
//! subagents at once, `/team`, `/loop`), quiet one-line notices when spend
//! runs hot (many parallel agents, a big file read again and again, a huge
//! context resent on every call, the same call repeated without progress, a
//! high $/min), and the breakdown and suggestions behind `/cost`.
//!
//! Budgets (session, day, month) are enforced by [`crate::cost::Accounting`];
//! this module only watches and explains. Unknown prices are never guessed:
//! estimates and notices fall back to tokens and plan-window percentages.

use crate::agent::{Env, Event};
use crate::llm::{ToolCall, Usage};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// `[spend]` in config.toml.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SpendConfig {
    /// Ask before fan-out estimated above this many USD (default 2.0).
    pub confirm_over_usd: Option<f64>,
    /// On plans with usage windows: ask above this % of the window (default 10).
    pub confirm_over_plan_percent: Option<f64>,
    /// Price and plan share both unknown: ask above this many tokens (default 5M).
    pub confirm_over_tokens: Option<u64>,
    /// Non-interactive runs (`-p`, background) over the threshold: "warn"
    /// (default: say so and continue) or "stop" (don't start the fan-out).
    pub non_interactive: Option<String>,
    /// Model calls a subagent is expected to make (default: learned from your
    /// past subagents, else 15).
    pub calls_per_agent: Option<u32>,
    /// The model offered by "use a cheaper model" (default: the next tier down
    /// of the same family, when its price is known).
    pub cheap_model: Option<String>,
    /// Agents at a time offered by "limit parallelism" (default 2).
    pub limit_agents: Option<usize>,
    /// Live notices while spend runs hot (default true).
    pub notices: Option<bool>,
    /// Notice when the session spends more than this many USD a minute (default 0.5).
    pub burn_usd_per_min: Option<f64>,
    /// Plans: notice above this % of the usage window an hour (default 25).
    pub burn_plan_percent_per_hour: Option<f64>,
    /// Notice at this many agents running at once (default 4).
    pub parallel_agents: Option<usize>,
    /// Notice when one large file is read this many times (default 3).
    pub repeated_reads: Option<u32>,
    /// Notice when calls keep resending a context above this % of the window (default 50).
    pub resent_context_percent: Option<f64>,
    /// Session budget in USD (same as top-level `max_cost_usd`, which wins).
    pub session_usd: Option<f64>,
    /// Daily budget in USD across sessions (same as `max_cost_usd_per_day`, which wins).
    pub day_usd: Option<f64>,
    /// Monthly budget in USD across sessions, from saved session usage.
    pub month_usd: Option<f64>,
}

impl SpendConfig {
    pub fn merge(&mut self, o: SpendConfig) {
        macro_rules! take {
            ($($f:ident),*) => { $(if o.$f.is_some() { self.$f = o.$f; })* };
        }
        take!(
            confirm_over_usd,
            confirm_over_plan_percent,
            confirm_over_tokens,
            non_interactive,
            calls_per_agent,
            cheap_model,
            limit_agents,
            notices,
            burn_usd_per_min,
            burn_plan_percent_per_hour,
            parallel_agents,
            repeated_reads,
            resent_context_percent,
            session_usd,
            day_usd,
            month_usd
        );
    }
    pub fn confirm_usd(&self) -> f64 {
        self.confirm_over_usd.unwrap_or(2.0)
    }
    pub fn confirm_percent(&self) -> f64 {
        self.confirm_over_plan_percent.unwrap_or(10.0)
    }
    pub fn confirm_tokens(&self) -> u64 {
        self.confirm_over_tokens.unwrap_or(5_000_000)
    }
    pub fn stops_unattended(&self) -> bool {
        self.non_interactive
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case("stop"))
    }
    pub fn limit(&self) -> usize {
        self.limit_agents.unwrap_or(2).max(1)
    }
    fn notices_on(&self) -> bool {
        self.notices.unwrap_or(true)
    }
    fn burn_usd(&self) -> f64 {
        self.burn_usd_per_min.unwrap_or(0.5)
    }
    fn burn_percent(&self) -> f64 {
        self.burn_plan_percent_per_hour.unwrap_or(25.0)
    }
    fn parallel(&self) -> usize {
        self.parallel_agents.unwrap_or(4).max(2)
    }
    fn reads(&self) -> u32 {
        self.repeated_reads.unwrap_or(3).max(2)
    }
    fn resent(&self) -> f64 {
        self.resent_context_percent.unwrap_or(50.0)
    }
}

// ------------------------------------------------------------------ estimate

/// Default model calls per subagent when nothing was learned yet. Agents that
/// explore a codebase commonly make 10 to 30; this errs high.
pub const DEFAULT_CALLS: u32 = 15;
/// Tokens a conversation grows by per call (tool results, the reply).
pub const GROWTH: u64 = 3_000;
/// Output tokens per call.
pub const OUTPUT: u64 = 600;

/// What is about to run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shape {
    pub agents: usize,
    /// Model calls each.
    pub calls: u32,
    /// Prompt tokens of each agent's first call.
    pub start_tokens: u64,
    pub growth: u64,
    pub output: u64,
}

impl Shape {
    pub fn new(agents: usize, calls: u32, start_tokens: u64) -> Shape {
        Shape {
            agents,
            calls: calls.max(1),
            start_tokens,
            growth: GROWTH,
            output: OUTPUT,
        }
    }
}

/// An estimate. `usd` is None when the model's price is unknown;
/// `plan_percent` is the share of a plan's usage window, when known.
#[derive(Debug, Clone, PartialEq)]
pub struct Estimate {
    pub shape: Shape,
    pub model: String,
    pub usd: Option<f64>,
    pub tokens: u64,
    pub plan_percent: Option<f64>,
    /// The plan window the percentage is of (`5h`, `weekly`).
    pub window: Option<String>,
}

/// Cache-aware: each agent's first call writes its prompt to the cache;
/// every later call reads the previous prompt from it and writes only what
/// was added. Providers without cache rates price those tokens as input
/// (see [`crate::cost::price`]), so this is right for them too. Each call is
/// priced on its own, so long-context surcharges apply where they would.
pub fn estimate(
    shape: Shape,
    model: &str,
    price: &dyn Fn(&Usage) -> Option<f64>,
    plan: Option<(f64, String)>,
) -> Estimate {
    let mut usd = Some(0.0);
    let mut tokens = 0;
    for i in 0..shape.calls.max(1) as u64 {
        let ctx = shape.start_tokens + shape.growth * i;
        let usage = if i == 0 {
            Usage {
                prompt_tokens: ctx,
                cache_write_tokens: ctx,
                completion_tokens: shape.output,
                ..Default::default()
            }
        } else {
            Usage {
                prompt_tokens: ctx,
                cached_tokens: ctx - shape.growth,
                cache_write_tokens: shape.growth,
                completion_tokens: shape.output,
                ..Default::default()
            }
        };
        tokens += ctx + shape.output;
        usd = usd.zip(price(&usage)).map(|(a, b)| a + b);
    }
    let n = shape.agents.max(1) as u64;
    let tokens = tokens * n;
    let (plan_percent, window) = match plan {
        Some((per_percent, w)) if per_percent > 0.0 => (Some(tokens as f64 / per_percent), Some(w)),
        _ => (None, None),
    };
    Estimate {
        shape,
        model: model.to_string(),
        usd: usd.map(|u| u * n as f64),
        tokens,
        plan_percent,
        window,
    }
}

pub fn tokens_short(t: u64) -> String {
    match t {
        t if t >= 1_000_000 => format!("{:.1}M", t as f64 / 1e6),
        t if t >= 1_000 => format!("{}k", t / 1000),
        t => t.to_string(),
    }
}

fn money(usd: f64) -> String {
    if usd >= 0.01 {
        format!("${usd:.2}")
    } else {
        crate::util::usd(usd)
    }
}

impl Estimate {
    /// `~$3.20`, `~2.1M tokens, ~12% of your 5h window`, `~2.1M tokens (price unknown)`.
    pub fn amount(&self) -> String {
        let t = tokens_short(self.tokens);
        match (self.usd, self.plan_percent) {
            (Some(u), _) if u > 0.0 => format!("~{} ({t} tokens)", money(u)),
            (_, Some(p)) => format!(
                "~{t} tokens, ~{p:.0}% of your {} window",
                self.window.as_deref().unwrap_or("plan")
            ),
            (Some(_), None) => format!("~{t} tokens (free: local model)"),
            (None, None) => format!("~{t} tokens (price unknown)"),
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "{} × ~{} calls on {}: {}",
            crate::util::plural(self.shape.agents, "agent"),
            self.shape.calls,
            self.model,
            self.amount()
        )
    }

    /// Over the configured threshold: USD when priced, else the plan share,
    /// else tokens. Free (local) work never is.
    pub fn over(&self, cfg: &SpendConfig) -> bool {
        match (self.usd, self.plan_percent) {
            (Some(u), _) if u > 0.0 => u >= cfg.confirm_usd(),
            (Some(_), None) => false,
            (_, Some(p)) => p >= cfg.confirm_percent(),
            (None, None) => self.tokens >= cfg.confirm_tokens(),
        }
    }

    fn threshold(&self, cfg: &SpendConfig) -> String {
        match (self.usd, self.plan_percent) {
            (Some(u), _) if u > 0.0 => {
                format!("over your {} confirm threshold", money(cfg.confirm_usd()))
            }
            (_, Some(_)) => format!(
                "over your {:.0}%-of-window confirm threshold",
                cfg.confirm_percent()
            ),
            _ => format!(
                "over your {} token confirm threshold",
                tokens_short(cfg.confirm_tokens())
            ),
        }
    }
}

/// An answer to the pre-flight question.
#[derive(Debug, Clone, PartialEq)]
pub enum Choice {
    Continue,
    /// Run the agents on this model instead.
    Cheaper(String),
    /// At most this many agents at once.
    Limit(usize),
    Cancel,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Preflight {
    /// Under the threshold (or free): nothing to say.
    Quiet,
    /// Non-interactive, policy "warn": say so and go on.
    Warn(String),
    /// Interactive: ask with these options.
    Ask {
        question: String,
        options: Vec<(String, Choice)>,
    },
    /// Non-interactive, policy "stop": don't start it.
    Stop(String),
}

/// What to do before starting `what` (e.g. "start 4 subagents").
pub fn decide(
    cfg: &SpendConfig,
    est: &Estimate,
    cheaper: Option<&Estimate>,
    what: &str,
    interactive: bool,
) -> Preflight {
    if !est.over(cfg) {
        return Preflight::Quiet;
    }
    let head = format!(
        "Spend check: {what} on {} (~{} calls each): {}, {}.",
        est.model,
        est.shape.calls,
        est.amount(),
        est.threshold(cfg)
    );
    if !interactive {
        return if cfg.stops_unattended() {
            Preflight::Stop(format!(
                "{head} Not started ([spend] non_interactive = \"stop\")."
            ))
        } else {
            Preflight::Warn(format!(
                "{head} Continuing ([spend] non_interactive = \"warn\")."
            ))
        };
    }
    let mut options = vec![(format!("Continue ({})", est.amount()), Choice::Continue)];
    if let Some(c) = cheaper {
        options.push((
            format!("Use {} for the agents ({})", c.model, c.amount()),
            Choice::Cheaper(c.model.clone()),
        ));
    }
    if est.shape.agents > cfg.limit() {
        options.push((
            format!(
                "Run at most {} at a time (same total, slower to burn, easier to stop)",
                cfg.limit()
            ),
            Choice::Limit(cfg.limit()),
        ));
    }
    options.push(("Cancel".into(), Choice::Cancel));
    Preflight::Ask {
        question: head,
        options,
    }
}

// --------------------------------------------------------------- the ledger

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Totals {
    pub calls: u64,
    pub prompt: u64,
    pub cached: u64,
    pub write: u64,
    pub output: u64,
}

impl Totals {
    fn add(&mut self, u: &Usage) {
        self.calls += 1;
        self.prompt += u.prompt_tokens;
        self.cached += u.cached_tokens;
        self.write += u.cache_write_tokens;
        self.output += u.completion_tokens;
    }
    /// One average call, for re-pricing on another model.
    fn per_call(&self) -> Usage {
        let n = self.calls.max(1);
        Usage {
            prompt_tokens: self.prompt / n,
            cached_tokens: self.cached / n,
            cache_write_tokens: self.write / n,
            completion_tokens: self.output / n,
            cost: None,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct Money {
    usd: f64,
    unknown: bool,
}

impl Money {
    fn add(&mut self, c: Option<f64>) {
        self.usd += c.unwrap_or(0.0);
        self.unknown |= c.is_none();
    }
    fn show(&self) -> String {
        if self.unknown {
            if self.usd == 0.0 {
                "price unknown".into()
            } else {
                format!("{} + unknown", money(self.usd))
            }
        } else {
            money(self.usd)
        }
    }
}

#[derive(Debug, Default, Clone)]
struct AgentLedger {
    label: String,
    model: String,
    sub: bool,
    totals: Totals,
    cost: Money,
    first: Option<Instant>,
    last: Option<Instant>,
    resent_streak: u32,
    /// (call hash, result hash) of recent tool calls.
    recent: VecDeque<(u64, u64)>,
}

#[derive(Debug, Default, Clone)]
struct ModelLedger {
    totals: Totals,
    cost: Money,
    /// The part spent by subagents.
    sub_totals: Totals,
    sub_cost: Money,
}

#[derive(Debug, Default, Clone)]
struct Reads {
    count: u32,
    tokens: u64,
}

#[derive(Default)]
struct Ledger {
    /// In start order; "main" is the session's own agent.
    agents: Vec<(String, AgentLedger)>,
    models: BTreeMap<String, ModelLedger>,
    tools: BTreeMap<String, (u64, u64)>,
    reads: HashMap<String, Reads>,
    /// (when, USD, tokens) of recent calls, for the burn rate.
    window: VecDeque<(Instant, f64, u64)>,
    /// Calls that resent a context above the threshold, and their prompt tokens.
    resent_calls: u64,
    resent_tokens: u64,
    peak_parallel: usize,
    tokens_total: u64,
    fired: HashSet<&'static str>,
    shown: Vec<String>,
    /// Plan learning: (used % of the window, tokens_total then), the window's key.
    plan_anchor: Option<(f64, u64, String)>,
    tokens_per_percent: Option<f64>,
    plan_window: Option<String>,
    /// The last pre-flight the user continued past: when, and its size.
    approved: Option<(Instant, f64)>,
}

impl Ledger {
    fn agent(&mut self, key: &str) -> &mut AgentLedger {
        if let Some(i) = self.agents.iter().position(|(k, _)| k == key) {
            return &mut self.agents[i].1;
        }
        self.agents.push((
            key.to_string(),
            AgentLedger {
                label: if key == MAIN {
                    "main agent".into()
                } else {
                    key.to_string()
                },
                sub: key != MAIN,
                ..Default::default()
            },
        ));
        &mut self.agents.last_mut().unwrap().1
    }
}

/// The ledger key of the session's own agent.
pub const MAIN: &str = "main";
/// The burn-rate window.
const WINDOW: Duration = Duration::from_secs(300);
/// Results at least this big count as a "large" read.
const LARGE_READ: u64 = 4_000;
/// Calls in a row above the resent-context threshold before it's flagged.
const RESENT_STREAK: u32 = 5;
/// The same call with the same result this many times in an agent's last dozen.
const SAME_CALL: usize = 4;

/// What the guard learned across sessions, kept in `~/.chanto/cache/spend.json`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Learned {
    /// Average model calls per subagent, and how many agents it's from.
    pub subagent_calls: Option<f64>,
    pub samples: u32,
    /// Tokens per 1% of a plan window, by `plan/window`.
    pub tokens_per_percent: BTreeMap<String, f64>,
}

fn learned_path() -> std::path::PathBuf {
    crate::config::home_dir().join("cache").join("spend.json")
}

impl Learned {
    pub fn load() -> Learned {
        std::fs::read(learned_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
    fn save(&self) {
        let p = learned_path();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(p, serde_json::to_vec(self).unwrap_or_default());
    }
    /// Fold in one finished subagent (a running average over the last ~20).
    pub fn add_agent(&mut self, calls: u64) {
        let n = self.samples.min(19) as f64;
        self.subagent_calls = Some(match self.subagent_calls {
            Some(avg) => (avg * n + calls as f64) / (n + 1.0),
            None => calls as f64,
        });
        self.samples += 1;
    }
}

/// The session's spend guard (one per [`crate::agent::Env`]).
pub struct Guard {
    pub cfg: SpendConfig,
    ledger: Mutex<Ledger>,
    learned: Mutex<Learned>,
    persist: bool,
    /// "Use a cheaper model for the agents": subagents run on this.
    subagent_model: RwLock<Option<String>>,
    /// "Limit parallelism": at most this many agents at once (0 = no cap).
    cap: AtomicUsize,
    running: Mutex<usize>,
    freed: tokio::sync::Notify,
}

/// Holds one of the agent slots [`Guard::slot`] hands out.
pub struct Slot<'a>(&'a Guard);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut r = self.0.running.lock().unwrap();
        *r = r.saturating_sub(1);
        self.0.freed.notify_waiters();
    }
}

fn hash<T: std::hash::Hash>(t: &T) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    t.hash(&mut h);
    h.finish()
}

fn tokens_of(s: &str) -> u64 {
    (s.len() as f64 / 3.5) as u64
}

impl Guard {
    pub fn new(cfg: &SpendConfig) -> Guard {
        // Tests never read or write what real sessions learned.
        let persist = !cfg!(test);
        Guard {
            cfg: cfg.clone(),
            ledger: Default::default(),
            learned: Mutex::new(if persist {
                Learned::load()
            } else {
                Learned::default()
            }),
            persist,
            subagent_model: RwLock::new(None),
            cap: AtomicUsize::new(0),
            running: Mutex::new(0),
            freed: tokio::sync::Notify::new(),
        }
    }

    /// Model calls to expect from a subagent: configured, else learned, else the default.
    pub fn calls_per_agent(&self) -> u32 {
        let learned = {
            let l = self.learned.lock().unwrap();
            l.subagent_calls
                .filter(|_| l.samples >= 3)
                .map(|c| c.ceil() as u32)
        };
        self.cfg
            .calls_per_agent
            .or(learned)
            .unwrap_or(DEFAULT_CALLS)
            .max(1)
    }

    /// Tokens per 1% of the current plan window and its label, once learned.
    pub fn plan_rate(&self) -> Option<(f64, String)> {
        let l = self.ledger.lock().unwrap();
        Some((
            l.tokens_per_percent?,
            l.plan_window.clone().unwrap_or_else(|| "plan".into()),
        ))
    }

    pub fn subagent_model(&self) -> Option<String> {
        self.subagent_model.read().unwrap().clone()
    }
    pub fn set_subagent_model(&self, m: Option<String>) {
        *self.subagent_model.write().unwrap() = m;
    }
    pub fn cap(&self) -> usize {
        self.cap.load(Ordering::Relaxed)
    }
    pub fn set_cap(&self, n: usize) {
        self.cap.store(n, Ordering::Relaxed);
        self.freed.notify_waiters();
    }

    /// Wait for an agent slot under the "limit parallelism" cap.
    pub async fn slot(&self, cancel: &tokio_util::sync::CancellationToken) -> Option<Slot<'_>> {
        loop {
            let freed = self.freed.notified();
            {
                let mut r = self.running.lock().unwrap();
                let cap = self.cap();
                if cap == 0 || *r < cap {
                    *r += 1;
                    return Some(Slot(self));
                }
            }
            tokio::select! {
                _ = freed => {}
                _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                _ = cancel.cancelled() => return None,
            }
        }
    }

    /// A pre-flight the user continued past: don't ask again for 10 minutes
    /// unless the next one is more than twice as big.
    pub fn approve(&self, size: f64) {
        self.ledger.lock().unwrap().approved = Some((Instant::now(), size));
    }
    pub fn recently_approved(&self, size: f64) -> bool {
        self.ledger
            .lock()
            .unwrap()
            .approved
            .is_some_and(|(at, s)| at.elapsed() < Duration::from_secs(600) && size <= s * 2.0)
    }

    pub fn agent_started(&self, key: &str, kind: &str, description: &str, model: &str) {
        let mut l = self.ledger.lock().unwrap();
        let a = l.agent(key);
        a.label = if description.is_empty() {
            kind.to_string()
        } else {
            format!("{kind}: {description}")
        };
        a.model = model.to_string();
    }

    /// A subagent finished: learn how many calls such agents make.
    pub fn agent_finished(&self, key: &str) {
        let calls = self
            .ledger
            .lock()
            .unwrap()
            .agents
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, a)| a.totals.calls)
            .unwrap_or(0);
        if calls == 0 {
            return;
        }
        let mut learned = self.learned.lock().unwrap();
        learned.add_agent(calls);
        if self.persist {
            learned.save();
        }
    }

    fn fire(&self, l: &mut Ledger, kind: &'static str, msg: String) -> Option<String> {
        if !self.cfg.notices_on() || !l.fired.insert(kind) {
            return None;
        }
        l.shown.push(msg.clone());
        Some(msg)
    }

    /// Agents running at once (including the main one's subagents in the
    /// background). Notices once at the configured count.
    pub fn parallel(&self, running: usize) -> Option<String> {
        let mut l = self.ledger.lock().unwrap();
        l.peak_parallel = l.peak_parallel.max(running);
        if running < self.cfg.parallel() {
            return None;
        }
        let msg = format!(
            "Spend: {running} agents running in parallel; each resends its own context on every call, so cost grows with the count. `/cost limit {}` caps how many run at once.",
            self.cfg.limit()
        );
        self.fire(&mut l, "parallel", msg)
    }

    /// A model call finished. `cost` is None when its price is unknown.
    pub fn model_call(
        &self,
        key: &str,
        model: &str,
        usage: &Usage,
        cost: Option<f64>,
        context_max: u64,
    ) -> Vec<String> {
        self.model_call_at(key, model, usage, cost, context_max, Instant::now())
    }

    pub fn model_call_at(
        &self,
        key: &str,
        model: &str,
        usage: &Usage,
        cost: Option<f64>,
        context_max: u64,
        now: Instant,
    ) -> Vec<String> {
        let mut out = vec![];
        let mut l = self.ledger.lock().unwrap();
        let tokens = usage.prompt_tokens + usage.completion_tokens;
        l.tokens_total += tokens;
        let sub = key != MAIN;
        let m = l.models.entry(model.to_string()).or_default();
        m.totals.add(usage);
        m.cost.add(cost);
        if sub {
            m.sub_totals.add(usage);
            m.sub_cost.add(cost);
        }
        let resent_at = (context_max as f64 * self.cfg.resent() / 100.0) as u64;
        let big = context_max > 0 && usage.prompt_tokens >= resent_at.max(1);
        if big {
            l.resent_calls += 1;
            l.resent_tokens += usage.prompt_tokens;
        }
        let a = l.agent(key);
        a.totals.add(usage);
        a.cost.add(cost);
        if a.model.is_empty() {
            a.model = model.to_string();
        }
        a.first.get_or_insert(now);
        a.last = Some(now);
        a.resent_streak = if big { a.resent_streak + 1 } else { 0 };
        let streak = a.resent_streak;
        let label = a.label.clone();
        if streak >= RESENT_STREAK {
            let pct = usage.prompt_tokens * 100 / context_max.max(1);
            let per_call = match (usage.cached_tokens, cost) {
                (c, Some(usd)) if c > 0 && usd > 0.0 => {
                    format!(", ~{} a call even with most of it cached", money(usd))
                }
                (_, Some(usd)) if usd > 0.0 => format!(", ~{} a call", money(usd)),
                _ => String::new(),
            };
            let msg = format!(
                "Spend: the {label}'s context is ~{} tokens ({pct}% of the window) and is resent on every call{per_call}. `/compact` or a fresh session cuts that.",
                tokens_short(usage.prompt_tokens)
            );
            out.extend(self.fire(&mut l, "resent", msg));
        }
        l.window.push_back((now, cost.unwrap_or(0.0), tokens));
        while l
            .window
            .front()
            .is_some_and(|(t, _, _)| now.duration_since(*t) > WINDOW)
        {
            l.window.pop_front();
        }
        // The rate over the window, counted over at least a minute so one
        // expensive call doesn't read as a runaway.
        let span = l
            .window
            .front()
            .map(|(t, _, _)| now.duration_since(*t))
            .unwrap_or_default()
            .max(Duration::from_secs(60));
        let mins = span.as_secs_f64() / 60.0;
        let usd: f64 = l.window.iter().map(|(_, c, _)| c).sum();
        let toks: u64 = l.window.iter().map(|(_, _, t)| t).sum();
        let top = l
            .agents
            .iter()
            .filter(|(_, a)| a.last.is_some_and(|t| now.duration_since(t) <= WINDOW))
            .count();
        let who = if top > 1 {
            format!(" across {top} agents")
        } else {
            String::new()
        };
        if usd / mins >= self.cfg.burn_usd() {
            let msg = format!(
                "Spend: ~{}/min over the last {}{who} (above {}/min). `/cost` shows where it goes.",
                money(usd / mins),
                crate::util::duration(span.as_secs()),
                money(self.cfg.burn_usd())
            );
            out.extend(self.fire(&mut l, "burn", msg));
        } else if let Some(per) = l
            .tokens_per_percent
            .filter(|_| cost.is_none() || cost == Some(0.0))
        {
            let per_hour = toks as f64 / per / mins * 60.0;
            if per_hour >= self.cfg.burn_percent() {
                let w = l.plan_window.clone().unwrap_or_else(|| "plan".into());
                let msg = format!(
                    "Spend: using ~{per_hour:.0}% of your {w} window an hour{who} (~{} tokens/min). `/cost` shows where it goes.",
                    tokens_short((toks as f64 / mins) as u64)
                );
                out.extend(self.fire(&mut l, "burn", msg));
            }
        }
        out
    }

    /// A tool call finished in agent `key`.
    pub fn tool_call(
        &self,
        key: &str,
        call: &ToolCall,
        content: &str,
        is_error: bool,
    ) -> Vec<String> {
        let mut out = vec![];
        let mut l = self.ledger.lock().unwrap();
        let tokens = tokens_of(content);
        let t = l.tools.entry(call.name.clone()).or_default();
        t.0 += 1;
        t.1 += tokens;
        if call.name == "Read"
            && !is_error
            && tokens >= LARGE_READ
            && let Some(path) = call.arguments["file_path"].as_str()
        {
            let r = l.reads.entry(path.to_string()).or_default();
            r.count += 1;
            r.tokens += tokens;
            let count = r.count;
            if count >= self.cfg.reads() {
                let msg = format!(
                    "Spend: {} (~{} tokens) has been read {count} times; every copy stays in context and is resent on each call. Read the part you need (offset/limit) or keep one summary.",
                    crate::util::one_line(path, 80),
                    tokens_short(tokens)
                );
                out.extend(self.fire(&mut l, "reads", msg));
            }
        }
        let key_hash = hash(&(call.name.as_str(), call.arguments.to_string()));
        let result_hash = hash(&content);
        let a = l.agent(key);
        a.recent.push_back((key_hash, result_hash));
        if a.recent.len() > 12 {
            a.recent.pop_front();
        }
        let same = a
            .recent
            .iter()
            .filter(|r| **r == (key_hash, result_hash))
            .count();
        let label = a.label.clone();
        if same >= SAME_CALL {
            let what = match call.name.as_str() {
                "Bash" => format!(
                    "`{}`",
                    crate::util::one_line(call.arguments["command"].as_str().unwrap_or(""), 60)
                ),
                n => format!("the same {n} call"),
            };
            let msg = format!(
                "Spend: {what} ran {same} times in the {label} with the same result: no progress, but each repeat is another model call over the whole context."
            );
            out.extend(self.fire(&mut l, "repeat", msg));
        }
        out
    }

    /// The provider reported its usage windows: learn tokens per 1% of the
    /// primary window from how fast it fills as this session uses tokens.
    pub fn limits(&self, s: &crate::llm::limits::Snapshot) {
        let Some(w) = s.primary.as_ref() else { return };
        let key = format!("{}/{}", s.plan_type.as_deref().unwrap_or("plan"), w.label());
        let mut l = self.ledger.lock().unwrap();
        l.plan_window = Some(w.label());
        if l.tokens_per_percent.is_none() {
            l.tokens_per_percent = self
                .learned
                .lock()
                .unwrap()
                .tokens_per_percent
                .get(&key)
                .copied();
        }
        let total = l.tokens_total;
        match l.plan_anchor.clone() {
            Some((p, t, k)) if k == key && w.used_percent >= p + 1.0 => {
                let rate = (total - t) as f64 / (w.used_percent - p);
                if rate > 0.0 {
                    let r = l
                        .tokens_per_percent
                        .map(|old| old * 0.5 + rate * 0.5)
                        .unwrap_or(rate);
                    l.tokens_per_percent = Some(r);
                    let mut learned = self.learned.lock().unwrap();
                    learned.tokens_per_percent.insert(key.clone(), r);
                    if self.persist {
                        learned.save();
                    }
                }
                l.plan_anchor = Some((w.used_percent, total, key));
            }
            Some((p, _, k)) if k == key && w.used_percent >= p => {}
            _ => l.plan_anchor = Some((w.used_percent, total, key)),
        }
    }

    /// Notices shown so far (for `/cost`).
    pub fn shown(&self) -> Vec<String> {
        self.ledger.lock().unwrap().shown.clone()
    }

    /// `/cost`: where this session's spend went, and what would cut it.
    /// `reprice(model, usage)` prices a call on another model (None: unknown);
    /// `cheaper(model)` names a cheaper model to suggest.
    pub fn report(
        &self,
        context_max: u64,
        reprice: &dyn Fn(&str, &Usage) -> Option<f64>,
        cheaper: &dyn Fn(&str) -> Option<String>,
    ) -> String {
        let l = self.ledger.lock().unwrap();
        let mut out = String::new();
        if l.agents.is_empty() {
            return "_No model calls yet this session._".into();
        }
        let now = Instant::now();
        out.push_str("## By agent\n");
        for (_, a) in &l.agents {
            let mins = match (a.first, a.last) {
                (Some(f), Some(t)) => {
                    t.duration_since(f)
                        .max(Duration::from_secs(60))
                        .as_secs_f64()
                        / 60.0
                }
                _ => 1.0,
            };
            let rate = if a.cost.usd > 0.0 {
                format!(" · ~{}/min", money(a.cost.usd / mins))
            } else {
                String::new()
            };
            let live = if a
                .last
                .is_some_and(|t| now.duration_since(t) < Duration::from_secs(30))
                && a.sub
            {
                " (active)"
            } else {
                ""
            };
            out.push_str(&format!(
                "- **{}**{live} on {}: {} calls · {} in ({} cached) · {} out · {}{rate}\n",
                a.label,
                if a.model.is_empty() {
                    "?"
                } else {
                    a.model.as_str()
                },
                a.totals.calls,
                tokens_short(a.totals.prompt),
                tokens_short(a.totals.cached),
                tokens_short(a.totals.output),
                a.cost.show()
            ));
        }
        out.push_str("\n## By model\n");
        for (m, x) in &l.models {
            out.push_str(&format!(
                "- **{m}**: {} calls · {} in ({} cached, {} written to cache) · {} out · {}\n",
                x.totals.calls,
                tokens_short(x.totals.prompt),
                tokens_short(x.totals.cached),
                tokens_short(x.totals.write),
                tokens_short(x.totals.output),
                x.cost.show()
            ));
        }
        let mut tools: Vec<_> = l.tools.iter().collect();
        tools.sort_by_key(|a| std::cmp::Reverse(a.1.1));
        if !tools.is_empty() {
            out.push_str("\n## By tool (results added to context)\n");
            for (name, (calls, tokens)) in tools.iter().take(8) {
                out.push_str(&format!(
                    "- {name}: {calls} calls · ~{} tokens\n",
                    tokens_short(*tokens)
                ));
            }
        }
        let mut reads: Vec<_> = l.reads.iter().filter(|(_, r)| r.count >= 2).collect();
        reads.sort_by_key(|a| std::cmp::Reverse(a.1.tokens));
        if !reads.is_empty() {
            out.push_str("\n## Large files read more than once\n");
            for (p, r) in reads.iter().take(5) {
                out.push_str(&format!(
                    "- {} × {} · ~{} tokens in all\n",
                    crate::util::one_line(p, 80),
                    r.count,
                    tokens_short(r.tokens)
                ));
            }
        }
        // Suggestions from what was observed.
        let mut tips = vec![];
        let total: f64 = l.models.values().map(|m| m.cost.usd).sum();
        for (m, x) in &l.models {
            if x.sub_totals.calls == 0 || x.sub_cost.usd <= 0.0 {
                continue;
            }
            let Some(c) = cheaper(m) else { continue };
            let per = x.sub_totals.per_call();
            if let (Some(now_cost), Some(alt)) = (reprice(m, &per), reprice(&c, &per))
                && alt < now_cost
                && now_cost > 0.0
            {
                let cut = (1.0 - alt / now_cost) * 100.0;
                let share = if total > 0.0 {
                    format!(
                        " ({:.0}% of this session's spend)",
                        x.sub_cost.usd / total * 100.0
                    )
                } else {
                    String::new()
                };
                tips.push(format!(
                        "Subagents ran on {m} for {}{share}; {c} would have cut that by ~{cut:.0}%. `/cost cheaper` switches them for this session (or set `subagent_model`).",
                        money(x.sub_cost.usd)
                    ));
            }
        }
        if l.resent_calls >= RESENT_STREAK as u64 {
            let avg = l.resent_tokens / l.resent_calls;
            tips.push(format!(
                "Context was resent {} times at ~{} tokens ({}%+ of the window): `/compact` or start fresh when a task is done.",
                l.resent_calls,
                tokens_short(avg),
                (avg * 100 / context_max.max(1)).min(100)
            ));
        }
        if l.peak_parallel >= 2 {
            let cap = self.cap();
            tips.push(format!(
                "Up to {} agents ran in parallel{}.",
                l.peak_parallel,
                if cap > 0 {
                    format!("; now capped at {cap} at a time")
                } else {
                    format!(": `/cost limit {}` caps that", self.cfg.limit())
                }
            ));
        }
        if let Some((p, r)) = reads.first().filter(|(_, r)| r.count >= self.cfg.reads()) {
            tips.push(format!(
                "{} was read {} times: read only the part you need, or keep one summary.",
                crate::util::one_line(p, 60),
                r.count
            ));
        }
        if !tips.is_empty() {
            out.push_str("\n## Suggestions\n");
            for t in tips {
                out.push_str(&format!("- {t}\n"));
            }
        }
        out
    }
}

/// The next tier down of the same family, for "use a cheaper model".
/// Keeps any provider prefix. None when there's no known cheaper sibling.
pub fn sibling(spec: &str) -> Option<String> {
    let (prefix, model) = match spec.rsplit_once('/') {
        Some((p, m)) => (format!("{p}/"), m),
        None => (String::new(), spec),
    };
    let next = match model {
        "claude-opus-5-5" => "claude-sonnet-5-5",
        "claude-sonnet-5-5" => "claude-haiku-5-5",
        "claude-opus-4-6" | "claude-opus-4-5" | "claude-opus-4-1" | "claude-opus-4" => {
            "claude-sonnet-4-6"
        }
        "claude-sonnet-4-6" | "claude-sonnet-4-5" | "claude-sonnet-4" => "claude-haiku-4-5",
        "gpt-6-astra" => "gpt-6.1-sol",
        "gpt-6.1-sol" | "gpt-6-sol" => "gpt-6-luna",
        "gpt-5" => "gpt-5-mini",
        "gpt-4.1" => "gpt-4.1-mini",
        "gpt-4o" => "gpt-4o-mini",
        "gemini-2.5-pro" => "gemini-2.5-flash",
        "deepseek-v4-pro" => "deepseek-v4-flash",
        _ => return None,
    };
    Some(format!("{prefix}{next}"))
}

/// Today's spend by model, from the model-call log.
pub fn today(log: &str) -> String {
    let mut by: BTreeMap<String, (u64, u64, f64, bool)> = BTreeMap::new();
    for v in log
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["event"] == "model_call" && v["data"]["status"] == "ok")
    {
        let d = &v["data"];
        let e = by
            .entry(format!(
                "{}/{}",
                d["provider"].as_str().unwrap_or("?"),
                d["model"].as_str().unwrap_or("?")
            ))
            .or_default();
        e.0 += 1;
        e.1 += d["input_tokens"].as_u64().unwrap_or(0) + d["output_tokens"].as_u64().unwrap_or(0);
        match d["cost"].as_f64() {
            Some(c) => e.2 += c,
            None => e.3 = true,
        }
    }
    if by.is_empty() {
        return "_No model calls logged today._".into();
    }
    let total: f64 = by.values().map(|v| v.2).sum();
    let mut out = format!("## Today (all sessions): {}\n", money(total));
    for (m, (calls, tokens, usd, unknown)) in by {
        out.push_str(&format!(
            "- **{m}**: {calls} calls · {} tokens · {}\n",
            tokens_short(tokens),
            Money { usd, unknown }.show()
        ));
    }
    out
}

// ------------------------------------------------------- session glue

/// Prices a call on `spec` as this session is billed for it: None when the
/// price is unknown, and for plan logins (ChatGPT) that have no per-token price.
pub fn pricer<'a>(env: &'a Env, spec: &str) -> impl Fn(&Usage) -> Option<f64> + use<'a> {
    let (p, model) = env.resolve(spec);
    price_on(env, &p, &model)
}

/// [`pricer`] for a provider already resolved.
pub fn price_on<'a>(
    env: &'a Env,
    p: &crate::llm::Provider,
    model: &str,
) -> impl Fn(&Usage) -> Option<f64> + use<'a> {
    let billing = p.billing_id();
    let model = model.to_string();
    let plan = (p.login.as_deref() == Some("chatgpt") || billing == "chatgpt")
        && !env.accounting.has_override(&billing, &model);
    move |u: &Usage| {
        if plan {
            None
        } else {
            env.accounting.estimate(&billing, &model, u).0
        }
    }
}

/// A cheaper model to offer for subagents on `spec`: `[spend] cheap_model`,
/// else the next tier down of the family, when its price is known and lower.
pub fn cheaper(env: &Env, spec: &str) -> Option<String> {
    let configured = env.config.spend.cheap_model.clone().filter(|m| m != spec);
    let c = configured.clone().or_else(|| sibling(spec))?;
    let sample = Usage {
        prompt_tokens: 20_000,
        cached_tokens: 15_000,
        cache_write_tokens: 5_000,
        completion_tokens: 600,
        cost: None,
    };
    let alt = pricer(env, &c)(&sample);
    match (pricer(env, spec)(&sample), alt) {
        (Some(now), Some(alt)) => (alt < now).then_some(c),
        // Today's price is unknown: only what you named.
        (None, _) => configured,
        (Some(_), None) => None,
    }
}

/// The model a new subagent of `kind` will run on.
pub fn subagent_spec(env: &Env, kind: &str) -> String {
    if let Some(m) = env.spend.subagent_model() {
        return m;
    }
    let def = crate::agent::find_agent_def(env, kind);
    if def.is_none() && kind.eq_ignore_ascii_case("oracle") {
        return env
            .config
            .oracle_model
            .clone()
            .unwrap_or_else(|| env.model());
    }
    if let Some(m) = def.and_then(|d| d.model) {
        if m.eq_ignore_ascii_case("inherit") {
            return env.model();
        }
        return crate::agent::subagent_model(env, &env.model(), &m);
    }
    env.config
        .subagent_model
        .clone()
        .unwrap_or_else(|| env.model())
}

/// The footer marker for a guard notice: two words at most, since the
/// transcript line right above it carries the explanation.
fn guard_label(cause: &str) -> &'static str {
    let c = cause.to_ascii_lowercase();
    if c.starts_with("the same ") || (c.contains(" ran ") && c.contains(" times")) {
        "repeat"
    } else if c.contains("agents running in parallel") {
        "parallel"
    } else if c.contains("context is") {
        "context"
    } else if c.contains("/min over") {
        "burn rate"
    } else if c.contains("window an hour") {
        "rate"
    } else if c.contains("has been read") {
        "re-reads"
    } else {
        "guard"
    }
}

/// Show guard notices: one line in the transcript, and a marker in the footer.
pub fn notify(env: &Env, notes: Vec<String>) {
    for n in notes {
        let cause = n.trim_start_matches("Spend: ");
        env.emit(Event::Status {
            key: "spend".into(),
            lines: vec![format!("$ {}", guard_label(cause))],
        });
        env.emit(Event::Info(n));
    }
}

/// Before `agents` subagents of `kind` start, with `running` already going:
/// estimate them and, over the threshold, ask or follow the non-interactive
/// policy. `Err(reason)` means they shouldn't start.
pub async fn preflight_agents(
    env: &Env,
    kind: &str,
    agents: usize,
    running: usize,
    start_tokens: u64,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let spec = subagent_spec(env, kind);
    let shape = Shape::new(agents, env.spend.calls_per_agent(), start_tokens);
    let plan = env.spend.plan_rate();
    let est = estimate(shape, &spec, &pricer(env, &spec), plan.clone());
    let cheap = cheaper(env, &spec).map(|c| estimate(shape, &c, &pricer(env, &c), plan));
    let what = match running {
        0 => format!("starting {}", crate::util::plural(agents, "subagent")),
        n => format!(
            "starting {} with {n} already running",
            crate::util::plural(agents, "subagent")
        ),
    };
    run_preflight(env, &est, cheap.as_ref(), &what, cancel).await
}

fn size(e: &Estimate) -> f64 {
    e.usd
        .filter(|u| *u > 0.0)
        .or(e.plan_percent)
        .unwrap_or(e.tokens as f64)
}

/// Act on [`decide`]: ask through the frontend, apply the choice.
pub async fn run_preflight(
    env: &Env,
    est: &Estimate,
    cheap: Option<&Estimate>,
    what: &str,
    cancel: &CancellationToken,
) -> Result<(), String> {
    if env.interactive && est.over(&env.config.spend) && env.spend.recently_approved(size(est)) {
        return Ok(());
    }
    match decide(&env.config.spend, est, cheap, what, env.interactive) {
        Preflight::Quiet => Ok(()),
        Preflight::Warn(m) => {
            env.emit(Event::Warn(m));
            Ok(())
        }
        Preflight::Stop(m) => {
            env.emit(Event::Warn(m.clone()));
            Err(format!(
                "{m} Do the work with fewer subagents, or directly."
            ))
        }
        Preflight::Ask { question, options } => {
            let labels: Vec<String> = options.iter().map(|(l, _)| l.clone()).collect();
            let reply = tokio::select! {
                _ = cancel.cancelled() => None,
                r = env.frontend.choose(&question, &labels) => r,
            };
            let choice = reply
                .and_then(|r| options.into_iter().find(|(l, _)| *l == r).map(|(_, c)| c))
                .unwrap_or(Choice::Cancel);
            match choice {
                Choice::Continue => {
                    env.spend.approve(size(est));
                    Ok(())
                }
                Choice::Cheaper(m) => {
                    env.spend.set_subagent_model(Some(m.clone()));
                    env.spend
                        .approve(cheap.map(size).unwrap_or_else(|| size(est)));
                    env.emit(Event::Info(format!("Subagents run on {m} for the rest of this session (`/cost cheaper off` undoes it).")));
                    Ok(())
                }
                Choice::Limit(n) => {
                    env.spend.set_cap(n);
                    env.spend.approve(size(est));
                    env.emit(Event::Info(format!("At most {n} agents run at once for the rest of this session (`/cost limit off` lifts it).")));
                    Ok(())
                }
                Choice::Cancel => Err(format!(
                    "Not started: the user cancelled {what} after a cost estimate ({}). Do the work directly or with fewer subagents, or ask the user.",
                    est.amount()
                )),
            }
        }
    }
}

/// Estimate for `/loop`: one iteration of the main agent on the current
/// context, and how many run a day.
pub fn loop_estimate(env: &Env, every: Option<Duration>) -> (Estimate, u64) {
    let ctx = crate::llm::estimate_tokens(&env.conversation.read().unwrap()).max(8_000);
    let spec = env.model();
    // A self-paced loop runs every 1 to 60 minutes; assume every 10.
    let secs = every.map(|d| d.as_secs()).unwrap_or(600).max(60);
    let runs = (86_400 / secs).max(1);
    let shape = Shape {
        agents: runs as usize,
        calls: 6,
        start_tokens: ctx,
        growth: GROWTH,
        output: OUTPUT,
    };
    (
        estimate(shape, &spec, &pricer(env, &spec), env.spend.plan_rate()),
        runs,
    )
}

/// `/loop`'s note about cost, when it's worth saying: e.g. "Spend: ~288 runs
/// a day at ~$0.05 each ≈ $14.40/day".
pub fn loop_note(env: &Env, every: Option<Duration>) -> Option<(String, bool)> {
    let (day, runs) = loop_estimate(env, every);
    let per_run = Estimate {
        usd: day.usd.map(|u| u / runs as f64),
        tokens: day.tokens / runs,
        plan_percent: day.plan_percent.map(|p| p / runs as f64),
        shape: Shape {
            agents: 1,
            ..day.shape
        },
        ..day.clone()
    };
    if day.usd == Some(0.0) {
        return None;
    }
    let note = format!(
        "Spend: ~{runs} runs a day at {} each ≈ {} a day if it runs all day.",
        per_run.amount(),
        day.amount()
    );
    Some((note, day.over(&env.config.spend)))
}

/// `/cost [today | cheaper [model|off] | limit [N|off]]`.
pub fn command(env: &Env, arg: &str) -> Option<String> {
    let mut words = arg.split_whitespace();
    let what = words.next()?;
    let rest = words.next();
    Some(match (what, rest) {
        ("today", _) => {
            let log = std::fs::read_to_string(crate::config::home_dir().join("logs").join(
                format!("chanto-{}.log", chrono::Local::now().format("%Y-%m-%d")),
            ))
            .unwrap_or_default();
            today(&log)
        }
        ("cheaper", Some("off")) => {
            env.spend.set_subagent_model(None);
            "Subagents use their usual model again.".into()
        }
        ("cheaper", m) => {
            let base = subagent_spec(env, "general-purpose");
            match m.map(str::to_string).or_else(|| cheaper(env, &base)) {
                Some(m) => {
                    env.spend.set_subagent_model(Some(m.clone()));
                    format!(
                        "Subagents run on **{m}** for the rest of this session (was {base}). `/cost cheaper off` undoes it."
                    )
                }
                None => format!(
                    "No cheaper model with a known price for {base}: name one (`/cost cheaper <model>`) or set `[spend] cheap_model`."
                ),
            }
        }
        ("limit", Some("off")) => {
            env.spend.set_cap(0);
            "No limit on agents at once.".into()
        }
        ("limit", n) => {
            let n = n
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(env.config.spend.limit())
                .max(1);
            env.spend.set_cap(n);
            format!(
                "At most {n} agents run at once for the rest of this session. `/cost limit off` lifts it."
            )
        }
        _ => "Usage: `/cost` · `/cost today` · `/cost cheaper [model|off]` · `/cost limit [N|off]`"
            .into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn opus(u: &Usage) -> Option<f64> {
        crate::cost::price("anthropic", "claude-opus-5-5", u, &BTreeMap::new())
    }

    #[test]
    fn guard_labels_stay_short_but_say_what_fired() {
        for (cause, label) in [
            (
                "the same Read call ran 4 times in the main agent with the same result",
                "repeat",
            ),
            (
                "3 agents running in parallel; each resends its own context",
                "parallel",
            ),
            (
                "the MAIN's context is ~48000 tokens (60% of the window)",
                "context",
            ),
            ("~$0.42/min over the last 5 minutes", "burn rate"),
            ("using ~12% of your 5h window an hour", "rate"),
            (
                "src/lib.rs (~9000 tokens) has been read 5 times",
                "re-reads",
            ),
            ("something else entirely", "guard"),
        ] {
            assert_eq!(guard_label(cause), label, "{cause}");
        }
    }

    #[test]
    fn estimates_are_cache_aware() {
        let shape = Shape {
            agents: 2,
            calls: 2,
            start_tokens: 10_000,
            growth: 1_000,
            output: 100,
        };
        let e = estimate(shape, "claude-opus-5-5", &opus, None);
        // Call 1 writes 10k to the cache ($5/M) + 100 out ($20/M); call 2 reads
        // 10k ($0.2/M), writes 1k, and 11k + 100 tokens are counted.
        let one = 10_000.0 * 5.0 / 1e6
            + 100.0 * 20.0 / 1e6
            + (10_000.0 * 0.2 + 1_000.0 * 5.0 + 100.0 * 20.0) / 1e6;
        assert!((e.usd.unwrap() - 2.0 * one).abs() < 1e-9, "{:?}", e.usd);
        assert_eq!(e.tokens, 2 * (10_100 + 11_100));
        // Without caching (every call at the input rate) it would cost more.
        let flat = |u: &Usage| {
            Some((u.prompt_tokens as f64 * 4.0 + u.completion_tokens as f64 * 20.0) / 1e6)
        };
        assert!(estimate(shape, "x", &flat, None).usd.unwrap() > e.usd.unwrap());
        assert!(
            e.amount().starts_with("~$0.")
                && e.summary()
                    .contains("2 agents × ~2 calls on claude-opus-5-5"),
            "{}",
            e.summary()
        );
    }

    #[test]
    fn unknown_prices_show_tokens_and_the_plan_share() {
        let none = |_: &Usage| None;
        let shape = Shape::new(4, 10, 20_000);
        let e = estimate(shape, "gpt-6.1-sol", &none, None);
        assert_eq!(e.usd, None);
        assert!(
            e.amount().contains("price unknown") && !e.amount().contains('$'),
            "{}",
            e.amount()
        );
        // A plan where 1% of the 5h window is ~100k tokens.
        let p = estimate(shape, "gpt-6.1-sol", &none, Some((100_000.0, "5h".into())));
        let pct = p.plan_percent.unwrap();
        assert!((pct - p.tokens as f64 / 100_000.0).abs() < 1e-9);
        assert!(p.amount().contains("% of your 5h window"), "{}", p.amount());
        // Local models are free and never over a threshold.
        let free = estimate(shape, "qwen", &|_: &Usage| Some(0.0), None);
        assert!(free.amount().contains("free") && !free.over(&SpendConfig::default()));
    }

    #[test]
    fn the_confirmation_fires_above_the_threshold_only() {
        let cfg = SpendConfig::default();
        let shape = Shape::new(4, 15, 15_000);
        let big = estimate(shape, "claude-opus-5-5", &opus, None);
        assert!(big.usd.unwrap() > 2.0, "{:?}", big.usd);
        let small = estimate(Shape::new(2, 3, 5_000), "claude-opus-5-5", &opus, None);
        assert_eq!(
            decide(&cfg, &small, None, "start 2 subagents", true),
            Preflight::Quiet
        );
        let haiku = estimate(
            shape,
            "claude-haiku-5-5",
            &|u: &Usage| crate::cost::price("anthropic", "claude-haiku-5-5", u, &BTreeMap::new()),
            None,
        );
        match decide(&cfg, &big, Some(&haiku), "start 4 subagents", true) {
            Preflight::Ask { question, options } => {
                assert!(
                    question.contains("start 4 subagents") && question.contains("$2.00"),
                    "{question}"
                );
                let choices: Vec<_> = options.iter().map(|(_, c)| c.clone()).collect();
                assert_eq!(
                    choices,
                    vec![
                        Choice::Continue,
                        Choice::Cheaper("claude-haiku-5-5".into()),
                        Choice::Limit(2),
                        Choice::Cancel
                    ]
                );
            }
            other => panic!("{other:?}"),
        }
        // Non-interactive: warn by default, stop when asked to; never silent.
        assert!(
            matches!(decide(&cfg, &big, None, "x", false), Preflight::Warn(m) if m.contains("Continuing"))
        );
        let stop = SpendConfig {
            non_interactive: Some("stop".into()),
            ..Default::default()
        };
        assert!(
            matches!(decide(&stop, &big, None, "x", false), Preflight::Stop(m) if m.contains("Not started"))
        );
        assert_eq!(decide(&stop, &small, None, "x", false), Preflight::Quiet);
        // A higher threshold lets it through; plan share and tokens have their own.
        let lax = SpendConfig {
            confirm_over_usd: Some(1000.0),
            ..Default::default()
        };
        assert_eq!(decide(&lax, &big, None, "x", true), Preflight::Quiet);
        let plan = estimate(shape, "m", &|_: &Usage| None, Some((50_000.0, "5h".into())));
        assert!(plan.over(&cfg), "{:?}", plan.plan_percent);
        let unknown = estimate(Shape::new(1, 2, 1_000), "m", &|_: &Usage| None, None);
        assert!(!unknown.over(&cfg));
    }

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments: args,
        }
    }

    fn quiet_cfg() -> SpendConfig {
        SpendConfig {
            calls_per_agent: Some(10),
            ..Default::default()
        }
    }

    #[test]
    fn detectors_fire_once_each() {
        let g = Guard::new(&quiet_cfg());
        // Parallel agents: at the threshold, once.
        assert!(g.parallel(3).is_none());
        assert!(
            g.parallel(5)
                .unwrap()
                .contains("5 agents running in parallel")
        );
        assert!(g.parallel(8).is_none(), "only once");
        // The same large file read again and again.
        let big = "x".repeat(40_000);
        let read = call("Read", json!({"file_path": "src/big.rs"}));
        assert!(g.tool_call(MAIN, &read, &big, false).is_empty());
        assert!(
            g.tool_call(MAIN, &read, &"y".repeat(40_000), false)
                .is_empty()
        );
        let n = g.tool_call(MAIN, &read, &"z".repeat(40_000), false);
        assert!(
            n.len() == 1 && n[0].contains("src/big.rs") && n[0].contains("3 times"),
            "{n:?}"
        );
        assert!(
            g.tool_call(MAIN, &read, &big, false)
                .iter()
                .all(|m| !m.contains("src/big.rs"))
        );
        // Small files can be read as often as needed.
        let g2 = Guard::new(&quiet_cfg());
        for i in 0..5 {
            assert!(
                g2.tool_call(
                    MAIN,
                    &call("Read", json!({"file_path": "a"})),
                    &rand_text(10 + i),
                    false
                )
                .is_empty()
            );
        }
        // A loop: the same call with the same result.
        let grep = call("Grep", json!({"pattern": "fn main"}));
        let mut fired = vec![];
        for _ in 0..6 {
            fired.extend(g.tool_call("agent-1", &grep, "src/main.rs:1", false));
        }
        assert_eq!(fired.len(), 1, "{fired:?}");
        assert!(fired[0].contains("4 times") && fired[0].contains("no progress"));
        // The same call with new results each time is progress.
        let g3 = Guard::new(&quiet_cfg());
        for i in 0..6 {
            assert!(
                g3.tool_call(MAIN, &grep, &format!("result {i}"), false)
                    .is_empty()
            );
        }
    }

    fn rand_text(n: usize) -> String {
        "a".repeat(n)
    }

    #[test]
    fn huge_resent_context_and_burn_rate_fire_once() {
        let g = Guard::new(&quiet_cfg());
        let t0 = Instant::now();
        let u = Usage {
            prompt_tokens: 150_000,
            cached_tokens: 140_000,
            completion_tokens: 500,
            ..Default::default()
        };
        let mut notes = vec![];
        for i in 0..8 {
            notes.extend(g.model_call_at(
                MAIN,
                "claude-opus-5-5",
                &u,
                Some(0.05),
                200_000,
                t0 + Duration::from_secs(i * 20),
            ));
        }
        let resent: Vec<_> = notes
            .iter()
            .filter(|n| n.contains("resent on every call"))
            .collect();
        assert_eq!(resent.len(), 1, "{notes:?}");
        assert!(
            resent[0].contains("150k") && resent[0].contains("75%"),
            "{}",
            resent[0]
        );
        // $0.40 over ~2.3 minutes is under $0.50/min: no burn notice.
        assert!(!notes.iter().any(|n| n.contains("/min over")), "{notes:?}");
        // Several agents spending fast: one burn notice.
        let mut burn = vec![];
        for i in 0..20 {
            let key = format!("agent-{}", i % 5);
            burn.extend(g.model_call_at(
                &key,
                "claude-opus-5-5",
                &Usage {
                    prompt_tokens: 20_000,
                    ..Default::default()
                },
                Some(0.2),
                200_000,
                t0 + Duration::from_secs(160 + i * 3),
            ));
        }
        let b: Vec<_> = burn.iter().filter(|n| n.contains("/min over")).collect();
        assert_eq!(b.len(), 1, "{burn:?}");
        assert!(b[0].contains("across"), "{}", b[0]);
        // Small contexts never trip the resent detector.
        let g2 = Guard::new(&quiet_cfg());
        for i in 0..10 {
            assert!(
                g2.model_call_at(
                    MAIN,
                    "m",
                    &Usage {
                        prompt_tokens: 5_000,
                        ..Default::default()
                    },
                    Some(0.001),
                    200_000,
                    t0 + Duration::from_secs(i * 30)
                )
                .is_empty()
            );
        }
    }

    #[test]
    fn plan_windows_teach_tokens_per_percent() {
        use crate::llm::limits::{Snapshot, Window};
        let g = Guard::new(&quiet_cfg());
        let snap = |p: f64| Snapshot {
            primary: Some(Window {
                used_percent: p,
                window_minutes: Some(300),
                resets_at: None,
            }),
            secondary: None,
            plan_type: Some("plus".into()),
        };
        g.limits(&snap(10.0));
        g.model_call(
            MAIN,
            "gpt-6.1-sol",
            &Usage {
                prompt_tokens: 190_000,
                completion_tokens: 10_000,
                ..Default::default()
            },
            None,
            400_000,
        );
        g.limits(&snap(12.0));
        let (per, w) = g.plan_rate().unwrap();
        assert!((per - 100_000.0).abs() < 1e-6 && w == "5h", "{per} {w}");
        // A reset (the percentage drops) starts a new anchor rather than a negative rate.
        g.limits(&snap(1.0));
        assert!((g.plan_rate().unwrap().0 - 100_000.0).abs() < 1e-6);
    }

    #[test]
    fn cost_breakdown_explains_a_recorded_session() {
        let g = Guard::new(&quiet_cfg());
        let t0 = Instant::now();
        g.agent_started(
            "t1",
            "Explore",
            "map the parser",
            "anthropic/claude-opus-5-5",
        );
        g.agent_started(
            "t2",
            "Explore",
            "map the lexer",
            "anthropic/claude-opus-5-5",
        );
        g.parallel(2);
        let main = Usage {
            prompt_tokens: 180_000,
            cached_tokens: 170_000,
            completion_tokens: 800,
            ..Default::default()
        };
        for i in 0..6 {
            let c = opus(&main);
            g.model_call_at(
                MAIN,
                "anthropic/claude-opus-5-5",
                &main,
                c,
                200_000,
                t0 + Duration::from_secs(i * 60),
            );
        }
        let sub = Usage {
            prompt_tokens: 30_000,
            cached_tokens: 25_000,
            cache_write_tokens: 5_000,
            completion_tokens: 600,
            ..Default::default()
        };
        for k in ["t1", "t2"] {
            for i in 0..5 {
                g.model_call_at(
                    k,
                    "anthropic/claude-opus-5-5",
                    &sub,
                    opus(&sub),
                    200_000,
                    t0 + Duration::from_secs(i * 10),
                );
            }
        }
        g.tool_call(
            "t1",
            &call("Read", json!({"file_path": "src/parser.rs"})),
            &"p".repeat(30_000),
            false,
        );
        g.tool_call(
            "t2",
            &call("Read", json!({"file_path": "src/parser.rs"})),
            &"p".repeat(30_000),
            false,
        );
        let reprice = |m: &str, u: &Usage| {
            crate::cost::price(
                "anthropic",
                m.trim_start_matches("anthropic/"),
                u,
                &BTreeMap::new(),
            )
        };
        let r = g.report(200_000, &reprice, &|m: &str| sibling(m));
        assert!(
            r.contains("## By agent")
                && r.contains("Explore: map the parser")
                && r.contains("main agent"),
            "{r}"
        );
        assert!(
            r.contains("## By model") && r.contains("anthropic/claude-opus-5-5"),
            "{r}"
        );
        assert!(
            r.contains("## By tool") && r.contains("Read: 2 calls"),
            "{r}"
        );
        assert!(r.contains("src/parser.rs × 2"), "{r}");
        assert!(
            r.contains("Subagents ran on anthropic/claude-opus-5-5")
                && r.contains("anthropic/claude-sonnet-5-5 would have cut that by ~"),
            "{r}"
        );
        assert!(
            r.contains("Context was resent 6 times at ~180k tokens"),
            "{r}"
        );
        assert!(r.contains("Up to 2 agents ran in parallel"), "{r}");
    }

    #[test]
    fn learned_calls_need_a_few_samples() {
        let mut l = Learned::default();
        for c in [10, 20, 30] {
            l.add_agent(c);
        }
        assert!(
            (l.subagent_calls.unwrap() - 20.0).abs() < 1e-9,
            "{:?}",
            l.subagent_calls
        );
        assert_eq!(
            sibling("anthropic/claude-opus-5-5").as_deref(),
            Some("anthropic/claude-sonnet-5-5")
        );
        assert_eq!(sibling("gpt-6-luna"), None);
    }

    #[test]
    fn today_sums_the_log_by_model() {
        let log = [
            json!({"event": "model_call", "data": {"provider": "anthropic", "model": "m", "input_tokens": 1000, "output_tokens": 10, "cost": 0.5, "status": "ok"}}),
            json!({"event": "model_call", "data": {"provider": "anthropic", "model": "m", "input_tokens": 1000, "output_tokens": 10, "cost": 0.25, "status": "ok"}}),
            json!({"event": "model_call", "data": {"provider": "x", "model": "y", "input_tokens": 5, "output_tokens": 5, "cost": null, "status": "ok"}}),
            json!({"event": "tool_call", "data": {}}),
        ]
        .map(|v| v.to_string())
        .join("\n");
        let t = today(&log);
        assert!(
            t.contains("$0.75")
                && t.contains("anthropic/m**: 2 calls")
                && t.contains("price unknown"),
            "{t}"
        );
    }

    #[tokio::test]
    async fn the_parallelism_cap_holds_agents_back() {
        let g = Guard::new(&quiet_cfg());
        g.set_cap(1);
        let cancel = tokio_util::sync::CancellationToken::new();
        let a = g.slot(&cancel).await.unwrap();
        let waited = tokio::time::timeout(Duration::from_millis(100), g.slot(&cancel)).await;
        assert!(waited.is_err(), "a second agent waits");
        drop(a);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), g.slot(&cancel))
                .await
                .is_ok()
        );
    }

    /// A frontend that answers the spend question with the option starting
    /// with `pick`, and keeps what it was asked and shown.
    struct Picker {
        pick: &'static str,
        asked: Mutex<Vec<(String, Vec<String>)>>,
        events: Mutex<Vec<Event>>,
    }

    #[async_trait::async_trait]
    impl crate::agent::Frontend for Picker {
        fn emit(&self, e: Event) {
            self.events.lock().unwrap().push(e);
        }
        async fn permission(
            &self,
            _: crate::agent::PermissionRequest,
        ) -> crate::agent::PermissionReply {
            crate::agent::PermissionReply::Allow
        }
        async fn question(&self, q: &str, options: &[String]) -> Option<String> {
            self.asked
                .lock()
                .unwrap()
                .push((q.to_string(), options.to_vec()));
            options.iter().find(|o| o.starts_with(self.pick)).cloned()
        }
    }

    /// A turn that launches three Task agents at once, against a mock server
    /// whose calls are priced high enough to cross the threshold.
    async fn fan_out(
        pick: &'static str,
        interactive: bool,
        policy: Option<&str>,
        confirm_usd: f64,
    ) -> (Arc<Picker>, Arc<Mutex<Vec<serde_json::Value>>>, String) {
        use crate::agent::tests::{mock_server_routed, tmpdir};
        let calls: Vec<_> = ["A", "B", "C"]
            .iter()
            .enumerate()
            .map(|(i, k)| json!({"index": i, "id": format!("t{k}"), "type": "function", "function": {"name": "Task", "arguments": json!({"description": format!("dig {k}"), "prompt": format!("dig {k}"), "subagent_type": "general-purpose"}).to_string()}}))
            .collect();
        let (base, seen) = mock_server_routed(vec![
            (
                "fan out",
                vec![json!({"tool_calls": calls}), json!({"content": "all done"})],
            ),
            ("dig A", vec![json!({"content": "A found"})]),
            ("dig B", vec![json!({"content": "B found"})]),
            ("dig C", vec![json!({"content": "C found"})]),
        ])
        .await;
        let dir = tmpdir("spend-fanout");
        let mut config = crate::config::Config::default();
        config.provider = crate::llm::ProviderKind::Openai;
        // The mock server is local; price it like an expensive hosted model.
        config.pricing.insert(
            "local/mock".into(),
            crate::cost::Price {
                input: 50.0,
                output: 100.0,
                cached_input: Some(5.0),
                cache_write: Some(60.0),
            },
        );
        config.spend = SpendConfig {
            calls_per_agent: Some(15),
            confirm_over_usd: Some(confirm_usd),
            non_interactive: policy.map(str::to_string),
            ..Default::default()
        };
        let fe = Arc::new(Picker {
            pick,
            asked: Default::default(),
            events: Default::default(),
        });
        let env = Env::new(crate::agent::EnvInit {
            config,
            provider: crate::llm::Provider::new(
                crate::llm::ProviderKind::Openai,
                &base,
                None,
                32768,
            ),
            model: "mock".into(),
            mode: crate::permissions::Mode::Bypass,
            vision: false,
            cwd: dir.clone(),
            tools: crate::tools::Registry::builtin(),
            mcp: vec![],
            frontend: fe.clone(),
            interactive,
        });
        let mut agent = crate::agent::Agent::new(env.clone());
        let end = agent
            .run_turn("fan out please", vec![], CancellationToken::new())
            .await
            .unwrap();
        let text = match end {
            crate::agent::TurnEnd::Done(t) => t,
            _ => panic!("interrupted"),
        };
        let tool_results: Vec<String> = agent
            .messages
            .iter()
            .filter(|m| m.role == crate::llm::Role::Tool)
            .map(|m| m.content.clone())
            .collect();
        let _ = std::fs::remove_dir_all(dir);
        (fe, seen, format!("{text}\n{}", tool_results.join("\n")))
    }

    fn subagent_requests(seen: &Mutex<Vec<serde_json::Value>>) -> usize {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|b| b["messages"].to_string().contains("dig "))
            .filter(|b| !b["messages"].to_string().contains("fan out"))
            .count()
    }

    #[tokio::test]
    async fn a_fan_out_over_the_threshold_asks_first() {
        let (fe, seen, out) = fan_out("Cancel", true, None, 2.0).await;
        let asked = fe.asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 1, "{asked:?}");
        assert!(
            asked[0].0.contains("starting 3 subagents")
                && asked[0].0.contains("over your $2.00 confirm threshold"),
            "{}",
            asked[0].0
        );
        assert!(
            asked[0].1.iter().any(|o| o.starts_with("Continue (~$")),
            "{:?}",
            asked[0].1
        );
        assert!(
            asked[0]
                .1
                .iter()
                .any(|o| o.starts_with("Run at most 2 at a time")),
            "{:?}",
            asked[0].1
        );
        // Cancelled: no subagent ever called the model, and the model was told why.
        assert_eq!(subagent_requests(&seen), 0);
        assert!(
            out.contains("Not started: the user cancelled starting 3 subagents"),
            "{out}"
        );

        // Continue: they run, and the ledger knows them.
        let (fe, seen, out) = fan_out("Continue", true, None, 2.0).await;
        assert_eq!(fe.asked.lock().unwrap().len(), 1);
        assert_eq!(subagent_requests(&seen), 3, "{out}");
        assert!(out.contains("A found") && out.contains("C found"), "{out}");

        // Below the threshold nobody is asked.
        let (fe, seen, _) = fan_out("Cancel", true, None, 1_000_000.0).await;
        assert!(fe.asked.lock().unwrap().is_empty());
        assert_eq!(subagent_requests(&seen), 3);
    }

    #[tokio::test]
    async fn unattended_fan_outs_follow_the_policy_and_say_so() {
        // Default policy: warn and continue.
        let (fe, seen, _) = fan_out("Cancel", false, None, 2.0).await;
        assert!(fe.asked.lock().unwrap().is_empty());
        assert_eq!(subagent_requests(&seen), 3);
        let warned = fe.events.lock().unwrap().iter().any(|e| matches!(e, Event::Warn(w) if w.contains("Spend check") && w.contains("Continuing")));
        assert!(warned, "a non-interactive run must not continue silently");
        // "stop": not started, with a warning and the reason for the model.
        let (fe, seen, out) = fan_out("Cancel", false, Some("stop"), 2.0).await;
        assert_eq!(subagent_requests(&seen), 0);
        assert!(
            out.contains("Not started ([spend] non_interactive = \"stop\")"),
            "{out}"
        );
        assert!(
            fe.events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, Event::Warn(w) if w.contains("Not started")))
        );
    }

    #[tokio::test]
    async fn the_cheaper_choice_moves_the_agents() {
        let g = Guard::new(&quiet_cfg());
        assert_eq!(g.subagent_model(), None);
        g.set_subagent_model(Some("anthropic/claude-haiku-5-5".into()));
        assert_eq!(
            g.subagent_model().as_deref(),
            Some("anthropic/claude-haiku-5-5")
        );
        g.set_cap(2);
        assert_eq!(g.cap(), 2);
    }
}
