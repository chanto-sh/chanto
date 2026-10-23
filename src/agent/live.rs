//! Live subagents, for the TUI's agents panel: what each one is doing now,
//! its tokens so far, and a bounded copy of its transcript so the panel can
//! show it while it runs. Fed from the events subagents already emit (their
//! tool calls carry the Task call's id as `parent`), plus their text and usage,
//! which never reach the frontend.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Transcript entries kept per agent; older ones are dropped.
pub const MAX_LOG: usize = 400;
/// Finished agents kept for viewing; a failed or stopped one also stays in
/// the panel this long, a successful one leaves at once.
pub const LINGER: Duration = Duration::from_secs(30);
const MAX_FINISHED: usize = 16;

#[derive(Clone, Debug, PartialEq)]
pub enum LogItem {
    /// The task it was given.
    Prompt(String),
    Text(String),
    Tool {
        id: String,
        label: String,
        started: Instant,
        done: Option<(Duration, bool, String)>,
    },
}

#[derive(Clone, Debug)]
pub struct LiveAgent {
    /// The Task call that started it (its tool events' `parent`).
    pub key: String,
    /// Its agent id (`agent-1a2b3c`), as background agents are known.
    pub id: String,
    pub kind: String,
    pub description: String,
    pub background: bool,
    pub started: Instant,
    /// When it ended, and whether it succeeded.
    pub ended: Option<(Instant, bool)>,
    /// What it's doing now: its latest tool call, or line of text.
    pub activity: String,
    /// Input plus output tokens so far.
    pub tokens: u64,
    pub log: VecDeque<LogItem>,
    /// Bumped on every change (for render caches).
    pub version: u64,
}

impl LiveAgent {
    pub fn running(&self) -> bool {
        self.ended.is_none()
    }

    /// Whether it still has a row in the panel at `now`.
    pub fn listed(&self, now: Instant) -> bool {
        match self.ended {
            None => true,
            Some((_, true)) => false,
            Some((at, false)) => now.duration_since(at) < LINGER,
        }
    }

    pub fn elapsed(&self) -> Duration {
        match self.ended {
            Some((at, _)) => at.duration_since(self.started),
            None => self.started.elapsed(),
        }
    }

    fn push(&mut self, item: LogItem) {
        self.log.push_back(item);
        while self.log.len() > MAX_LOG {
            self.log.pop_front();
        }
    }
}

#[derive(Default)]
pub struct Registry {
    agents: Mutex<Vec<LiveAgent>>,
}

impl Registry {
    /// An agent starts (or a background one is resumed under the same key).
    pub fn start(
        &self,
        key: &str,
        id: &str,
        kind: &str,
        description: &str,
        prompt: &str,
        background: bool,
    ) {
        let mut v = self.agents.lock().unwrap();
        if let Some(a) = v.iter_mut().find(|a| a.key == key) {
            a.started = Instant::now();
            a.ended = None;
            a.activity.clear();
            a.push(LogItem::Prompt(prompt.to_string()));
            a.version += 1;
            return;
        }
        // Forget the oldest finished agents beyond a few.
        let finished = v.iter().filter(|a| !a.running()).count();
        if finished >= MAX_FINISHED
            && let Some(i) = v.iter().position(|a| !a.running())
        {
            v.remove(i);
        }
        let mut a = LiveAgent {
            key: key.into(),
            id: id.into(),
            kind: kind.into(),
            description: description.into(),
            background,
            started: Instant::now(),
            ended: None,
            activity: String::new(),
            tokens: 0,
            log: VecDeque::new(),
            version: 0,
        };
        a.push(LogItem::Prompt(prompt.to_string()));
        v.push(a);
    }

    /// The user sent agent `id` a message (shown in its transcript).
    pub fn message(&self, id: &str, text: &str) {
        let mut v = self.agents.lock().unwrap();
        if let Some(a) = v.iter_mut().rev().find(|a| a.id == id) {
            a.push(LogItem::Prompt(text.to_string()));
            a.version += 1;
        }
    }

    pub fn finish(&self, key: &str, ok: bool) {
        self.update(key, |a| a.ended = Some((Instant::now(), ok)));
    }

    /// Hide a finished agent's row now (`x` on it).
    pub fn dismiss(&self, key: &str) {
        self.update(key, |a| {
            if let Some((at, _)) = a.ended {
                a.ended = Some((at, true));
            }
        });
    }

    fn update(&self, key: &str, f: impl FnOnce(&mut LiveAgent)) {
        if let Some(a) = self
            .agents
            .lock()
            .unwrap()
            .iter_mut()
            .find(|a| a.key == key)
        {
            f(a);
            a.version += 1;
        }
    }

    pub fn text(&self, key: &str, chunk: &str) {
        self.update(key, |a| {
            match a.log.back_mut() {
                Some(LogItem::Text(t)) => t.push_str(chunk),
                _ => a.push(LogItem::Text(chunk.to_string())),
            }
            if let Some(LogItem::Text(t)) = a.log.back()
                && let Some(line) = t.lines().rev().map(str::trim).find(|l| !l.is_empty())
            {
                a.activity = line
                    .trim_start_matches(['#', '*', '-', ' '])
                    .trim_end_matches(['*', ':'])
                    .to_string();
            }
        });
    }

    pub fn tool_start(&self, key: &str, id: &str, label: &str) {
        self.update(key, |a| {
            a.activity = label.to_string();
            a.push(LogItem::Tool {
                id: id.into(),
                label: label.into(),
                started: Instant::now(),
                done: None,
            });
        });
    }

    pub fn tool_end(&self, key: &str, id: &str, display: &str, is_error: bool) {
        self.update(key, |a| {
            if let Some(LogItem::Tool { started, done, .. }) = a
                .log
                .iter_mut()
                .rev()
                .find(|i| matches!(i, LogItem::Tool { id: t, .. } if t == id))
            {
                *done = Some((started.elapsed(), is_error, display.to_string()));
            }
        });
    }

    pub fn tokens(&self, key: &str, tokens: u64) {
        self.update(key, |a| a.tokens = tokens);
    }

    /// Agents running now (foreground and background).
    pub fn running_count(&self) -> usize {
        self.agents
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.running())
            .count()
    }

    /// Everything known, oldest first.
    pub fn snapshot(&self) -> Vec<LiveAgent> {
        self.agents.lock().unwrap().clone()
    }

    /// One agent, if its version moved past `seen`.
    pub fn get_if_newer(&self, key: &str, seen: Option<u64>) -> Option<LiveAgent> {
        self.agents
            .lock()
            .unwrap()
            .iter()
            .find(|a| a.key == key && Some(a.version) != seen)
            .cloned()
    }

    /// The rows the panel lists now.
    pub fn rows(&self, now: Instant) -> Vec<Row> {
        self.agents
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.listed(now))
            .map(|a| Row {
                key: a.key.clone(),
                id: a.id.clone(),
                kind: a.kind.clone(),
                activity: if a.activity.is_empty() {
                    a.description.clone()
                } else {
                    a.activity.clone()
                },
                secs: a.elapsed().as_secs(),
                tokens: a.tokens,
                status: match a.ended {
                    None => RowStatus::Running,
                    Some((_, true)) => RowStatus::Done,
                    Some((_, false)) => RowStatus::Failed,
                },
                background: a.background,
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RowStatus {
    Running,
    Done,
    Failed,
}

/// An agent's row in the panel.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub key: String,
    pub id: String,
    pub kind: String,
    pub activity: String,
    pub secs: u64,
    pub tokens: u64,
    pub status: RowStatus,
    pub background: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_activity_tokens_and_log() {
        let r = Registry::default();
        r.start(
            "call_1",
            "agent-abc",
            "Explore",
            "Find the parser",
            "Look for the parser",
            false,
        );
        assert_eq!(r.rows(Instant::now())[0].activity, "Find the parser");
        r.tool_start("call_1", "t1", "Grep(fn parse)");
        assert_eq!(r.rows(Instant::now())[0].activity, "Grep(fn parse)");
        r.tool_end("call_1", "t1", "3 matches", false);
        r.text("call_1", "Found it.\n## Reading ");
        r.text("call_1", "parser.rs");
        r.tokens("call_1", 1234);
        let row = &r.rows(Instant::now())[0];
        assert_eq!(row.activity, "Reading parser.rs");
        assert_eq!(row.tokens, 1234);
        let a = &r.snapshot()[0];
        assert_eq!(a.log.len(), 3);
        assert!(
            matches!(&a.log[1], LogItem::Tool { done: Some((_, false, d)), .. } if d == "3 matches")
        );
        // Events for unknown keys are ignored.
        r.text("other", "x");
        assert_eq!(r.snapshot().len(), 1);
    }

    #[test]
    fn finished_rows_leave_or_linger() {
        let r = Registry::default();
        r.start("a", "agent-a", "general-purpose", "ok", "p", false);
        r.start("b", "agent-b", "general-purpose", "bad", "p", true);
        r.finish("a", true);
        r.finish("b", false);
        let now = Instant::now();
        let rows = r.rows(now);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, RowStatus::Failed);
        assert!(r.rows(now + LINGER).is_empty());
        r.dismiss("b");
        assert!(r.rows(now).is_empty());
        // Still viewable after it leaves the panel.
        assert!(r.get_if_newer("a", None).is_some());
        // Resuming under the same key brings it back.
        r.start("b", "agent-b", "general-purpose", "bad", "more", true);
        assert_eq!(r.rows(Instant::now()).len(), 1);
    }

    #[test]
    fn log_is_bounded() {
        let r = Registry::default();
        r.start("a", "agent-a", "x", "d", "p", false);
        for i in 0..MAX_LOG + 50 {
            r.tool_start("a", &format!("t{i}"), "Read");
        }
        assert_eq!(r.snapshot()[0].log.len(), MAX_LOG);
    }
}
