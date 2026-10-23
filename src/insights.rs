//! Usage insights: where a session's tokens and time went, and what to change.
//!
//! Most detectors read the transcript (so they work on saved sessions too);
//! [`Stats`] adds what only the live loop sees: provider token counts, model
//! and tool time, compactions, and prompt-prefix changes that make a local
//! model re-process its whole context (KV-cache breaks).

use crate::llm::{Message, Role, ToolCall};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Counters recorded by the agent loop; saved with the session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    pub total_cost_usd: f64,
    pub price_unknown: bool,
    pub model_calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub model_secs: f64,
    /// Time to first token, summed: mostly prompt processing on local models.
    pub prefill_secs: f64,
    pub tool_secs: f64,
    pub compactions: u64,
    /// Model replies and tool calls in the conversation, counted as they
    /// happen so compaction doesn't lose them (`num_turns`, `tool_calls`).
    pub turns: u64,
    pub tool_calls: u64,
    /// Calls whose prompt prefix differed from the previous call, by cause.
    pub cache_breaks: Vec<CacheBreak>,
    /// Hashes of the previous request (settings, tool list, then messages).
    #[serde(skip)]
    prev: Vec<u64>,
    /// Cause to attribute to the next prefix change (set by compaction etc).
    #[serde(skip)]
    pending_cause: Option<String>,
    #[serde(skip)]
    hinted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheBreak {
    pub cause: String,
    /// Tokens that had to be processed again.
    pub tokens: u64,
}

fn hash<T: std::hash::Hash>(v: &T) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

fn msg_hash(m: &Message) -> u64 {
    let calls: Vec<String> = m
        .tool_calls
        .iter()
        .map(|c| format!("{}{}", c.name, c.arguments))
        .collect();
    // Replayed reasoning goes out with the message, so a change to it moves
    // the prefix as much as a change to the text does.
    let items = m
        .provider_items
        .as_ref()
        .map(|p| serde_json::to_string(&p.items).unwrap_or_default());
    hash(&(
        format!("{:?}", m.role),
        &m.content,
        calls,
        m.images.len(),
        items,
    ))
}

fn tokens_of(s: &str) -> u64 {
    (s.len() as f64 / 3.5) as u64
}

impl Stats {
    /// Note why the prompt prefix is about to change (compaction, clearing).
    pub fn expect_break(&mut self, cause: &str) {
        self.pending_cause = Some(cause.to_string());
    }

    /// Record a request about to be sent; detects prefix changes. `settings`
    /// is whatever else shapes the rendered prompt (model, effort, thinking):
    /// a change there re-renders all of it.
    pub fn before_call(&mut self, settings: &str, tools_json: &str, messages: &[Message]) {
        let cur: Vec<u64> = [hash(&settings), hash(&tools_json)]
            .into_iter()
            .chain(messages.iter().map(msg_hash))
            .collect();
        let cause = self.pending_cause.take();
        if !self.prev.is_empty() {
            let same = cur
                .iter()
                .zip(&self.prev)
                .take_while(|(a, b)| a == b)
                .count();
            if same < self.prev.len() {
                let cause = cause.unwrap_or_else(|| match same {
                    0 => "request settings changed".into(),
                    1 => "tool list changed".into(),
                    2 => "system prompt changed".into(),
                    _ => "earlier messages edited".into(),
                });
                // Everything from the first difference on is processed again.
                let from = same.saturating_sub(2);
                let tokens = if same < 2 { tokens_of(tools_json) } else { 0 }
                    + crate::llm::estimate_tokens(&messages[from.min(messages.len())..]);
                self.cache_breaks.push(CacheBreak { cause, tokens });
            }
        }
        self.prev = cur;
        self.model_calls += 1;
    }

    pub fn after_call(
        &mut self,
        prompt_tokens: u64,
        completion_tokens: u64,
        secs: f64,
        prefill_secs: f64,
    ) {
        self.prompt_tokens += prompt_tokens;
        self.completion_tokens += completion_tokens;
        self.model_secs += secs;
        self.prefill_secs += prefill_secs;
    }

    /// One-time hint when a single tool output takes a big share of the context.
    pub fn live_hint(&mut self, tool: &str, content: &str, ctx: u64) -> Option<String> {
        let t = tokens_of(content);
        if self.hinted || ctx == 0 || t * 4 < ctx {
            return None;
        }
        self.hinted = true;
        Some(format!(
            "That {tool} output used ~{}k tokens ({}% of the context). {} /insights shows where context goes.",
            t / 1000,
            t * 100 / ctx,
            suggestion_for(tool)
        ))
    }
}

/// One thing that cost tokens or time, with a fix.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub kind: &'static str,
    pub title: String,
    pub count: u64,
    /// Estimated tokens that could be saved.
    pub tokens: u64,
    pub secs: f64,
    pub example: String,
    pub suggestion: String,
}

fn suggestion_for(tool: &str) -> &'static str {
    match tool {
        "Read" => "Grep for what you need first, then Read with offset/limit.",
        "Bash" | "BashOutput" => {
            "Filter command output (`| tail -50`, `| grep`, quiet flags like `-q`)."
        }
        "Grep" => "Narrow the pattern or path, or use files-only output first.",
        "WebFetch" | "Browser" => "Ask for the specific section instead of the whole page.",
        _ => "Ask the tool for less (filters, limits, narrower queries).",
    }
}

fn arg<'a>(c: &'a ToolCall, k: &str) -> &'a str {
    c.arguments[k].as_str().unwrap_or("")
}

fn is_error(content: &str) -> bool {
    content.starts_with("Error")
        || content.contains("\n[exit code ")
        || content.starts_with("[exit code ")
}

/// Paths a call may have changed.
fn written_paths(c: &ToolCall) -> Vec<String> {
    match c.name.as_str() {
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => vec![
            arg(c, "file_path").to_string(),
            arg(c, "notebook_path").to_string(),
        ],
        "apply_patch" => crate::tools::patch::paths(arg(c, "input")),
        _ => vec![],
    }
}

fn same_path(a: &str, b: &str) -> bool {
    !a.is_empty()
        && !b.is_empty()
        && (a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}")))
}

#[derive(Default)]
struct Acc {
    count: u64,
    tokens: u64,
    example: String,
    /// Tool of the worst example (drives the suggestion).
    tool: String,
    worst: u64,
}

impl Acc {
    fn add(&mut self, tokens: u64, example: impl FnOnce() -> String) {
        self.count += 1;
        self.tokens += tokens;
        if tokens >= self.worst {
            self.worst = tokens;
            self.example = example();
        }
    }
    fn add_tool(&mut self, tool: &str, tokens: u64, example: impl FnOnce() -> String) {
        if tokens >= self.worst {
            self.tool = tool.to_string();
        }
        self.add(tokens, example);
    }
}

/// Analyze a transcript (and, for live or recent sessions, its stats).
pub fn analyze(messages: &[Message], stats: &Stats, ctx: u64) -> Vec<Finding> {
    let ctx = ctx.max(1);
    let calls: HashMap<&str, &ToolCall> = messages
        .iter()
        .flat_map(|m| m.tool_calls.iter())
        .map(|c| (c.id.as_str(), c))
        .collect();
    let (mut big, mut noisy, mut whole, mut reread, mut failing) = (
        Acc::default(),
        Acc::default(),
        Acc::default(),
        Acc::default(),
        Acc::default(),
    );
    // (path, offset, limit) -> index of the last read still valid.
    let mut reads: HashMap<(String, String, String), u64> = HashMap::new();
    // Full reads of large files, waiting to see how much of them gets edited: (path, tokens).
    let mut full_reads: Vec<(String, u64)> = vec![];
    let mut failed: HashMap<String, u32> = HashMap::new();
    let mut seen_full: std::collections::HashSet<String> = Default::default();
    for m in messages {
        if m.role == Role::Assistant {
            for c in &m.tool_calls {
                for p in written_paths(c) {
                    reads.retain(|(rp, _, _), _| !same_path(rp, &p));
                    // How much of a fully read file did the edit touch?
                    if let Some(i) = full_reads.iter().position(|(rp, _)| same_path(rp, &p)) {
                        let (path, t) = full_reads.remove(i);
                        let touched = tokens_of(&c.arguments.to_string());
                        if touched * 4 < t {
                            whole.add(t.saturating_sub(600), || {
                                format!("{path} (~{}k tokens read, ~{} edited)", t / 1000, touched)
                            });
                            seen_full.insert(path);
                        }
                    }
                    failed.clear();
                }
                if c.name == "Bash" {
                    // A command can change files; only drop reads it names.
                    let cmd = arg(c, "command");
                    reads.retain(|(rp, _, _), _| {
                        !rp.rsplit('/')
                            .next()
                            .is_some_and(|f| !f.is_empty() && cmd.contains(f))
                    });
                }
            }
            continue;
        }
        if m.role != Role::Tool || m.content.starts_with("[old tool result cleared") {
            continue;
        }
        let Some(call) = m.tool_call_id.as_deref().and_then(|id| calls.get(id)) else {
            continue;
        };
        let t = tokens_of(&m.content);
        let label = || crate::util::one_line(&format!("{} {}", call.name, call.arguments), 90);
        match call.name.as_str() {
            "Read" => {
                let path = arg(call, "file_path").to_string();
                let key = (
                    path.clone(),
                    call.arguments["offset"].to_string(),
                    call.arguments["limit"].to_string(),
                );
                if !is_error(&m.content) {
                    if reads.contains_key(&key) {
                        reread.add(t, || format!("{path} (~{}k tokens)", t.max(1000) / 1000));
                        continue;
                    }
                    reads.insert(key, t);
                    if call.arguments["offset"].is_null()
                        && call.arguments["limit"].is_null()
                        && t > 1500
                    {
                        full_reads.push((path, t));
                        continue;
                    }
                }
            }
            "Bash" => {
                let cmd = arg(call, "command").to_string();
                if is_error(&m.content) {
                    let n = failed.entry(cmd.clone()).or_insert(0);
                    *n += 1;
                    if *n > 1 {
                        failing.add(t + 100, || crate::util::one_line(&cmd, 80));
                        continue;
                    }
                }
                if t * 10 <= ctx && t > 2000 {
                    noisy.add(t.saturating_sub(500), || crate::util::one_line(&cmd, 80));
                    continue;
                }
            }
            _ => {}
        }
        if t * 10 > ctx {
            big.add_tool(&call.name, t, || {
                format!(
                    "{} (~{}k tokens, {}% of context)",
                    label(),
                    t / 1000,
                    t * 100 / ctx
                )
            });
        }
    }
    // Large full reads never edited stay in context too; count the biggest as bloat.
    for (path, t) in full_reads {
        if t * 10 > ctx && !seen_full.contains(&path) {
            big.add_tool("Read", t, || {
                format!(
                    "Read {path} (~{}k tokens, {}% of context)",
                    t / 1000,
                    t * 100 / ctx
                )
            });
        }
    }

    let mut out = vec![];
    let avg_out = stats
        .completion_tokens
        .checked_div(stats.model_calls)
        .unwrap_or(300)
        .max(100);
    let avg_secs = if stats.model_calls > 0 {
        stats.model_secs / stats.model_calls as f64
    } else {
        0.0
    };
    if big.count > 0 {
        out.push(Finding {
            kind: "large-outputs",
            title: "Tool outputs taking over 10% of the context".into(),
            count: big.count,
            tokens: big.tokens,
            secs: 0.0,
            example: big.example,
            suggestion: suggestion_for(&big.tool).into(),
        });
    }
    if whole.count > 0 {
        out.push(Finding {
            kind: "whole-file-reads",
            title: "Whole large files read, then only a small part edited".into(),
            count: whole.count,
            tokens: whole.tokens,
            secs: 0.0,
            example: whole.example,
            suggestion: "Grep for the symbol, then Read with offset/limit around it.".into(),
        });
    }
    if reread.count > 0 {
        out.push(Finding {
            kind: "repeated-reads",
            title: "Files read again without changing in between".into(),
            count: reread.count,
            tokens: reread.tokens,
            secs: 0.0,
            example: reread.example,
            suggestion: "The earlier read is still in context; refer back to it (or Read only the lines needed).".into(),
        });
    }
    if noisy.count > 0 {
        out.push(Finding {
            kind: "noisy-commands",
            title: "Commands with long output".into(),
            count: noisy.count,
            tokens: noisy.tokens,
            secs: 0.0,
            example: noisy.example,
            suggestion: suggestion_for("Bash").into(),
        });
    }
    if failing.count > 0 {
        out.push(Finding {
            kind: "repeated-failures",
            title: "The same failing command rerun with no edits in between".into(),
            count: failing.count,
            tokens: failing.tokens + failing.count * avg_out,
            secs: failing.count as f64 * avg_secs,
            example: failing.example,
            suggestion: "Read the error and change something before rerunning; an unchanged command fails the same way.".into(),
        });
    }
    // Wasted turns flagged by the loop's own reminders.
    let reminders: [(&str, &'static str, &str, &str); 4] = [
        (
            "cut off at the output token limit",
            "truncated",
            "Responses cut off at the output limit",
            "Write big files in several Edit calls; raise max output if the model allows.",
        ),
        (
            "Your last response was empty",
            "empty",
            "Empty model responses",
            "Usually a thinking run that used up the output; try `/think off` or a larger num_ctx.",
        ),
        (
            "same tool call",
            "loops",
            "Identical tool calls repeated (loop guard)",
            "The model was stuck; a clearer instruction or a stronger model helps.",
        ),
        (
            "verification failed",
            "gate-retries",
            "Verification retries (checks failed when the model said it was done)",
            "Ask it to run the checks itself before finishing; recurring failures may need a hint.",
        ),
    ];
    for (needle, kind, title, suggestion) in reminders {
        let n = messages
            .iter()
            .filter(|m| {
                m.role == Role::User
                    && m.content.contains("<system-reminder>")
                    && m.content.contains(needle)
            })
            .count() as u64;
        if n > 0 {
            out.push(Finding {
                kind,
                title: title.into(),
                count: n,
                tokens: n * avg_out,
                secs: n as f64 * avg_secs,
                example: String::new(),
                suggestion: suggestion.into(),
            });
        }
    }
    let compactions = stats.compactions.max(messages.iter().any(|m| {
        m.content
            .starts_with("This session is being continued from a previous conversation")
    }) as u64);
    if compactions > 0 {
        out.push(Finding {
            kind: "compactions",
            title: "Context compactions".into(),
            count: compactions,
            // Each one summarizes about a full context and rebuilds the prompt cache.
            tokens: compactions * ctx * 3 / 4,
            secs: compactions as f64 * avg_secs * 2.0,
            example: String::new(),
            suggestion: "Keep outputs small (see above) so context lasts longer; raise num_ctx if memory allows.".into(),
        });
    }
    let mut by_cause: Vec<(String, u64, u64)> = vec![];
    for b in stats
        .cache_breaks
        .iter()
        .filter(|b| b.cause != "compaction")
    {
        match by_cause.iter_mut().find(|(c, _, _)| *c == b.cause) {
            Some(e) => {
                e.1 += 1;
                e.2 += b.tokens;
            }
            None => by_cause.push((b.cause.clone(), 1, b.tokens)),
        }
    }
    if !by_cause.is_empty() {
        let (n, t): (u64, u64) = by_cause.iter().fold((0, 0), |a, b| (a.0 + b.1, a.1 + b.2));
        let causes = by_cause
            .iter()
            .map(|(c, n, _)| format!("{c} ×{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        // Prompt processing speed: from stats when available, else a typical local rate.
        let pps = if stats.prefill_secs > 0.5 {
            stats.prompt_tokens as f64 / stats.prefill_secs
        } else {
            500.0
        };
        out.push(Finding {
            kind: "cache-breaks",
            title: "Prompt-cache breaks (the model re-read its context)".into(),
            count: n,
            tokens: t,
            secs: t as f64 / pps.max(1.0),
            example: causes,
            suggestion: "Avoid switching mode, model, effort, tools or MCP servers mid-session; local models re-process everything after the first change.".into(),
        });
    }
    out.sort_by(|a, b| b.tokens.cmp(&a.tokens).then(b.secs.total_cmp(&a.secs)));
    out
}

/// Merge findings of several sessions by kind.
pub fn merge(all: Vec<Vec<Finding>>) -> Vec<Finding> {
    let mut out: Vec<Finding> = vec![];
    for f in all.into_iter().flatten() {
        match out.iter_mut().find(|o| o.kind == f.kind) {
            Some(o) => {
                o.count += f.count;
                o.tokens += f.tokens;
                o.secs += f.secs;
                if o.example.is_empty() {
                    o.example = f.example;
                }
            }
            None => out.push(f),
        }
    }
    out.sort_by(|a, b| b.tokens.cmp(&a.tokens).then(b.secs.total_cmp(&a.secs)));
    out
}

fn k(t: u64) -> String {
    if t >= 1000 {
        format!("{:.1}k", t as f64 / 1000.0)
    } else {
        t.to_string()
    }
}

/// Markdown report. `scope` describes what was analyzed.
pub fn report(scope: &str, findings: &[Finding], stats: &Stats, messages: usize) -> String {
    let mut out = format!("## Insights: {scope}\n\n");
    out += &format!(
        "{}{}\n\n",
        crate::util::usd(stats.total_cost_usd),
        if stats.price_unknown {
            " + price unknown"
        } else {
            ""
        }
    );
    if stats.model_calls > 0 {
        out += &format!(
            "{} model calls · {} tokens in, {} out · model {:.0}s (prompt processing {:.0}s), tools {:.0}s\n\n",
            stats.model_calls,
            k(stats.prompt_tokens),
            k(stats.completion_tokens),
            stats.model_secs,
            stats.prefill_secs,
            stats.tool_secs
        );
    } else {
        out += &format!("{messages} messages (no timing recorded)\n\n");
    }
    if findings.is_empty() {
        out += "Nothing wasteful found.\n";
        return out;
    }
    for (i, f) in findings.iter().enumerate() {
        let mut cost = format!("~{} tokens", k(f.tokens));
        if f.secs >= 1.0 {
            cost += &format!(", ~{:.0}s", f.secs);
        }
        out += &format!("{}. **{}** ({}×, {cost})", i + 1, f.title, f.count);
        if !f.example.is_empty() {
            out += &format!(" — e.g. {}", f.example);
        }
        out += &format!("\n   {}\n", f.suggestion);
    }
    out += "\nToken figures are estimates of what a change would save.\n";
    out
}

/// `/stats`: sessions, prompts, tokens and cost from the saved sessions, in
/// this project and everywhere, over the last 7 and 30 days, then by day and
/// by model. A session counts on the day it was last active, under the model
/// it last used. `now` is a Unix timestamp.
pub fn usage(
    sessions: &[(crate::session::Session, usize)],
    here: &std::path::Path,
    now: i64,
) -> String {
    #[derive(Default)]
    struct Tally {
        sessions: u64,
        prompts: u64,
        tokens_in: u64,
        tokens_out: u64,
        cost: f64,
        unknown: bool,
    }
    impl Tally {
        fn add(&mut self, s: &crate::session::Session, prompts: usize) {
            self.sessions += 1;
            self.prompts += prompts as u64;
            self.tokens_in += s.stats.prompt_tokens;
            self.tokens_out += s.stats.completion_tokens;
            self.cost += s.stats.total_cost_usd;
            self.unknown |= s.stats.price_unknown;
        }
        fn cells(&self) -> String {
            use crate::util::short_num;
            format!(
                "{} | {} | {} / {} | {}{}",
                self.sessions,
                self.prompts,
                short_num(self.tokens_in),
                short_num(self.tokens_out),
                crate::util::usd(self.cost),
                if self.unknown { " + unpriced" } else { "" }
            )
        }
    }
    let age_days = |s: &crate::session::Session| {
        chrono::DateTime::parse_from_rfc3339(&s.updated)
            .map(|t| (now - t.timestamp()) as f64 / 86_400.0)
            .unwrap_or(f64::MAX)
    };
    let mut out = String::from(
        "## Usage\n\n| | sessions | prompts | tokens in / out | cost |\n|---|---|---|---|---|\n",
    );
    for (label, here_only) in [("This project", true), ("All projects", false)] {
        for days in [7.0, 30.0] {
            let mut t = Tally::default();
            for (s, n) in sessions
                .iter()
                .filter(|(s, _)| age_days(s) < days && (!here_only || s.cwd == here))
            {
                t.add(s, *n);
            }
            out += &format!("| {label}, last {days} days | {} |\n", t.cells());
        }
    }
    let mut by_day: std::collections::BTreeMap<String, Tally> = Default::default();
    let mut by_model: HashMap<String, Tally> = HashMap::new();
    for (s, n) in sessions {
        let age = age_days(s);
        if age < 7.0 {
            by_day
                .entry(s.updated.get(..10).unwrap_or("").to_string())
                .or_default()
                .add(s, *n);
        }
        if age < 30.0 {
            by_model.entry(s.model.clone()).or_default().add(s, *n);
        }
    }
    if !by_day.is_empty() {
        out += "\n### By day (all projects)\n\n| day | sessions | prompts | tokens in / out | cost |\n|---|---|---|---|---|\n";
        for (day, t) in by_day.iter().rev() {
            out += &format!("| {day} | {} |\n", t.cells());
        }
    }
    if !by_model.is_empty() {
        let mut models: Vec<(String, Tally)> = by_model.into_iter().collect();
        models.sort_by(|a, b| {
            b.1.cost
                .total_cmp(&a.1.cost)
                .then(b.1.sessions.cmp(&a.1.sessions))
        });
        out += "\n### By model (all projects, last 30 days)\n\n| model | sessions | prompts | tokens in / out | cost |\n|---|---|---|---|---|\n";
        for (model, t) in &models {
            out += &format!("| {model} | {} |\n", t.cells());
        }
    }
    out += "\nFrom saved sessions, each counted on its last active day. `/cost` shows this session; `/insights` where its tokens went.";
    out
}

/// `chanto insights [--session <id>] [--last N]`: report on saved sessions.
pub fn run_cli(args: &[String]) -> anyhow::Result<String> {
    let cwd = std::env::current_dir()?;
    let (cfg, _) = crate::config::Config::load(&cwd, crate::config::is_trusted_here(&cwd))?;
    let ctx = cfg.num_ctx() as u64;
    let mut session = None;
    let mut last = 10usize;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--session" | "-s" => session = it.next().cloned(),
            "--last" | "-n" => last = it.next().and_then(|n| n.parse().ok()).unwrap_or(last),
            "-h" | "--help" => return Ok("Usage: chanto insights [--session <id>] [--last N]   (default: last 10 sessions here)".into()),
            s => anyhow::bail!("unknown argument {s}"),
        }
    }
    let loaded: Vec<(crate::session::Session, Vec<Message>)> = match session {
        Some(id) => vec![crate::session::Session::load(&cwd, &id)?],
        None => crate::session::Session::list(&cwd)
            .into_iter()
            .take(last)
            .filter_map(|s| crate::session::Session::load(&cwd, &s.id).ok())
            .collect(),
    };
    if loaded.is_empty() {
        return Ok("No saved sessions for this directory.".into());
    }
    let mut total = Stats::default();
    let mut msgs = 0;
    let all: Vec<Vec<Finding>> = loaded
        .iter()
        .map(|(s, m)| {
            total.total_cost_usd += s.stats.total_cost_usd;
            total.price_unknown |= s.stats.price_unknown;
            total.model_calls += s.stats.model_calls;
            total.prompt_tokens += s.stats.prompt_tokens;
            total.completion_tokens += s.stats.completion_tokens;
            total.model_secs += s.stats.model_secs;
            total.prefill_secs += s.stats.prefill_secs;
            total.tool_secs += s.stats.tool_secs;
            msgs += m.len();
            analyze(m, &s.stats, ctx)
        })
        .collect();
    let scope = if loaded.len() == 1 {
        format!("session {}", loaded[0].0.id)
    } else {
        format!("last {} sessions in this project", loaded.len())
    };
    Ok(report(&scope, &merge(all), &total, msgs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str, name: &str, args: serde_json::Value) -> Message {
        let mut m = Message::assistant("");
        m.tool_calls = vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args,
        }];
        m
    }
    fn result(id: &str, name: &str, content: &str) -> Message {
        Message::tool(
            &ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: json!({}),
            },
            content.to_string(),
        )
    }
    fn kinds(f: &[Finding]) -> Vec<&str> {
        f.iter().map(|f| f.kind).collect()
    }

    #[test]
    fn whole_file_read_then_small_edit() {
        let big = "x".repeat(20_000);
        let msgs = vec![
            call("1", "Read", json!({"file_path": "src/lib.rs"})),
            result("1", "Read", &big),
            call(
                "2",
                "Edit",
                json!({"file_path": "src/lib.rs", "old_string": "a - b", "new_string": "a + b"}),
            ),
            result("2", "Edit", "ok"),
        ];
        let f = analyze(&msgs, &Stats::default(), 32768);
        assert_eq!(kinds(&f), vec!["whole-file-reads"], "{f:?}");
        assert!(f[0].tokens > 4000 && f[0].example.contains("src/lib.rs"));
    }

    #[test]
    fn large_outputs_and_noisy_commands() {
        let msgs = vec![
            call("1", "Grep", json!({"pattern": "fn"})),
            result("1", "Grep", &"m\n".repeat(4000)),
            call("2", "Bash", json!({"command": "cargo build"})),
            result("2", "Bash", &"Compiling x\n".repeat(900)),
        ];
        let f = analyze(&msgs, &Stats::default(), 32768);
        assert_eq!(
            kinds(&f),
            vec!["noisy-commands"],
            "Grep at 7% isn't flagged; the 3k-token build log is"
        );
        let f = analyze(&msgs, &Stats::default(), 8192);
        assert!(kinds(&f).contains(&"large-outputs"), "{f:?}");
        let big = f.iter().find(|f| f.kind == "large-outputs").unwrap();
        assert_eq!(big.count, 2);
        assert!(
            big.suggestion.contains("Filter command output"),
            "advice follows the largest output: {big:?}"
        );
    }

    #[test]
    fn repeated_reads_unless_changed() {
        let body = "line\n".repeat(200);
        let msgs = vec![
            call("1", "Read", json!({"file_path": "a.py"})),
            result("1", "Read", &body),
            call("2", "Read", json!({"file_path": "a.py"})),
            result("2", "Read", &body),
            call("3", "Write", json!({"file_path": "a.py", "content": "y"})),
            result("3", "Write", "ok"),
            call("4", "Read", json!({"file_path": "a.py"})),
            result("4", "Read", &body),
            call(
                "5",
                "Read",
                json!({"file_path": "a.py", "offset": 10, "limit": 5}),
            ),
            result("5", "Read", "part"),
        ];
        let f = analyze(&msgs, &Stats::default(), 32768);
        let r = f.iter().find(|f| f.kind == "repeated-reads").unwrap();
        assert_eq!(
            r.count, 1,
            "re-read after a write and partial reads are fine"
        );
    }

    #[test]
    fn repeated_failures_reminders_and_compaction() {
        let fail = "FAILED test_x\n[exit code 1]";
        let mut msgs = vec![
            Message::user(
                "This session is being continued from a previous conversation that ran out of context. Summary...",
            ),
            call("1", "Bash", json!({"command": "pytest"})),
            result("1", "Bash", fail),
            call("2", "Bash", json!({"command": "pytest"})),
            result("2", "Bash", fail),
            call("3", "Bash", json!({"command": "pytest"})),
            result("3", "Bash", fail),
        ];
        msgs.push(Message::user("<system-reminder>You have made the same tool call 3 times in a row with identical arguments</system-reminder>"));
        msgs.push(Message::user(
            "<system-reminder>You are not done: verification failed.\n\nx</system-reminder>",
        ));
        msgs.push(Message::user(
            "<system-reminder>Your last response was empty. Continue</system-reminder>",
        ));
        let f = analyze(&msgs, &Stats::default(), 32768);
        let ks = kinds(&f);
        for k in [
            "repeated-failures",
            "loops",
            "gate-retries",
            "empty",
            "compactions",
        ] {
            assert!(ks.contains(&k), "{k} missing from {ks:?}");
        }
        assert_eq!(
            f.iter()
                .find(|f| f.kind == "repeated-failures")
                .unwrap()
                .count,
            2
        );
        assert_eq!(f[0].kind, "compactions", "ranked by tokens");
        // An edit between failures resets the count.
        let msgs = vec![
            call("1", "Bash", json!({"command": "pytest"})),
            result("1", "Bash", fail),
            call(
                "2",
                "Edit",
                json!({"file_path": "x.py", "old_string": "a", "new_string": "b"}),
            ),
            result("2", "Edit", "ok"),
            call("3", "Bash", json!({"command": "pytest"})),
            result("3", "Bash", fail),
        ];
        assert!(analyze(&msgs, &Stats::default(), 32768).is_empty());
    }

    #[test]
    fn cache_breaks_from_prefix_changes() {
        let mut s = Stats::default();
        let mut msgs = vec![Message::system("sys"), Message::user("hi")];
        s.before_call("m", "[tools]", &msgs);
        msgs.push(Message::assistant("hello"));
        msgs.push(Message::user("more"));
        s.before_call("m", "[tools]", &msgs);
        assert!(s.cache_breaks.is_empty(), "appending keeps the prefix");
        msgs[0] = Message::system("sys v2");
        s.before_call("m", "[tools]", &msgs);
        s.before_call("m", "[tools, new]", &msgs);
        msgs[2].content = "[old tool result cleared]".into();
        s.expect_break("old tool results cleared");
        s.before_call("m", "[tools, new]", &msgs);
        // A different effort re-renders the prompt even with the same messages.
        s.before_call("m low", "[tools, new]", &msgs);
        let causes: Vec<_> = s.cache_breaks.iter().map(|b| b.cause.as_str()).collect();
        assert_eq!(
            causes,
            vec![
                "system prompt changed",
                "tool list changed",
                "old tool results cleared",
                "request settings changed"
            ]
        );
        let f = analyze(&[], &s, 32768);
        assert_eq!(kinds(&f), vec!["cache-breaks"]);
        assert_eq!(f[0].count, 4);
        let r = report("test", &f, &s, 0);
        assert!(
            r.contains("Prompt-cache breaks") && r.contains("system prompt changed ×1"),
            "{r}"
        );
    }

    #[test]
    fn live_hint_once() {
        let mut s = Stats::default();
        assert!(s.live_hint("Read", "small", 32768).is_none());
        let big = "x".repeat(40_000);
        assert!(
            s.live_hint("Read", &big, 32768)
                .unwrap()
                .contains("offset/limit")
        );
        assert!(
            s.live_hint("Read", &big, 32768).is_none(),
            "only once per session"
        );
    }

    #[test]
    fn usage_tallies_by_window_day_and_model() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-14T12:00:00+00:00")
            .unwrap()
            .timestamp();
        let sess = |cwd: &str, model: &str, updated: &str, cost: f64| {
            let mut s = crate::session::Session::new(std::path::Path::new(cwd), model);
            s.updated = updated.into();
            s.stats.total_cost_usd = cost;
            s.stats.prompt_tokens = 1000;
            (s, 2usize)
        };
        let all = vec![
            sess("/p", "a", "2026-10-14T10:00:00+00:00", 1.0),
            sess("/q", "b", "2026-10-12T10:00:00+00:00", 2.0),
            sess("/p", "a", "2026-09-30T10:00:00+00:00", 4.0),
            sess("/p", "a", "2026-08-01T10:00:00+00:00", 8.0),
        ];
        let out = usage(&all, std::path::Path::new("/p"), now);
        assert!(
            out.contains("| This project, last 7 days | 1 | 2 | 1.0k / 0 | $1.00 |"),
            "{out}"
        );
        assert!(
            out.contains("| This project, last 30 days | 2 | 4 | 2.0k / 0 | $5.00 |"),
            "{out}"
        );
        assert!(
            out.contains("| All projects, last 7 days | 2 | 4 | 2.0k / 0 | $3.00 |"),
            "{out}"
        );
        assert!(
            out.contains("| All projects, last 30 days | 3 | 6 | 3.0k / 0 | $7.00 |"),
            "{out}"
        );
        assert!(
            out.contains("| 2026-10-14 | 1 |")
                && out.contains("| 2026-10-12 | 1 |")
                && !out.contains("| 2026-09-30 |"),
            "{out}"
        );
        // By model, costliest first; the August session is out of range.
        assert!(
            out.find("| a | 2 |").unwrap() < out.find("| b | 1 |").unwrap(),
            "{out}"
        );
        // Under a dollar, cents would round cheap models to $0.00.
        let cheap = usage(
            &[sess("/p", "a", "2026-10-14T10:00:00+00:00", 0.0123)],
            std::path::Path::new("/p"),
            now,
        );
        assert!(cheap.contains("| $0.0123 |"), "{cheap}");
    }
}
