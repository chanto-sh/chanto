//! Session-scoped scheduling: `/loop`
//! (on a fixed interval, or self-paced through the model's ScheduleWakeup
//! calls) and the CronCreate / CronList / CronDelete tools.
//!
//! Tasks only fire while the session is open and idle: a frontend that owns
//! an idle loop (the TUI, the plain REPL) marks the scheduler as driven and
//! asks [`Scheduler::take_due`] for the next prompt between turns. Nothing
//! catches up: a task that came due while a turn ran fires once afterwards.

use crate::runner::Cron;
use chrono::{DateTime, Duration as ChronoDuration, Local};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The scheduling tools (in `tools::schedule`).
pub const TOOLS: &[&str] = &["CronCreate", "CronList", "CronDelete", "ScheduleWakeup"];

/// At most this many scheduled tasks per session.
pub const MAX_TASKS: usize = 50;
/// Recurring tasks and loops end after a week (they fire one final time).
pub const EXPIRY: Duration = Duration::from_secs(7 * 86_400);
/// ScheduleWakeup delays are clamped to this range.
pub const WAKE_MIN: u64 = 60;
pub const WAKE_MAX: u64 = 3600;
/// A self-paced iteration that neither rescheduled nor stopped gets one
/// fallback wake-up this far out; if that one doesn't reschedule either, the
/// loop ends.
pub const WAKE_FALLBACK: u64 = 20 * 60;
/// `loop.md` content beyond this is cut.
const LOOP_MD_MAX: usize = 25_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `/loop <interval> [prompt]`.
    Loop,
    /// The pending wake-up of a self-paced `/loop`.
    Wakeup,
    /// CronCreate.
    Cron,
}

#[derive(Debug, Clone)]
pub enum Schedule {
    Every(Duration),
    Cron(Box<Cron>, String),
    /// Fires once at `next`.
    Once,
}

#[derive(Debug, Clone)]
pub struct Task {
    /// 8 characters, as CronDelete takes it.
    pub id: String,
    /// `None`: the loop's default prompt (`loop.md` or the built-in one),
    /// read again at every fire so edits apply to the next iteration.
    pub prompt: Option<String>,
    pub schedule: Schedule,
    pub source: Source,
    pub created: DateTime<Local>,
    pub next: DateTime<Local>,
    /// Why the model picked this wake-up time (ScheduleWakeup's `reason`).
    pub reason: Option<String>,
}

impl Task {
    pub fn describe_schedule(&self) -> String {
        match &self.schedule {
            Schedule::Every(d) => format!("every {}", crate::util::duration(d.as_secs())),
            Schedule::Cron(_, text) => format!("cron `{text}`"),
            Schedule::Once => "once".into(),
        }
    }
    fn prompt_text(&self) -> String {
        self.prompt
            .clone()
            .unwrap_or_else(|| "(default loop prompt)".into())
    }
}

/// A prompt to run now.
#[derive(Debug, Clone, PartialEq)]
pub struct Fire {
    pub id: String,
    pub source: Source,
    /// What to send to the model.
    pub prompt: String,
    /// One line for the transcript, e.g. "Scheduled task ab12cd34 fired".
    pub note: String,
    /// Send `prompt` to the model as it is: never run it as a slash command,
    /// `!shell` or `#note`. Set for everything the user didn't type: the
    /// model's CronCreate tasks and the default loop prompt (`loop.md` may
    /// come from the project).
    pub literal: bool,
}

#[derive(Debug, Default)]
struct SelfPaced {
    prompt: Option<String>,
    /// The current iteration called ScheduleWakeup.
    rescheduled: bool,
    /// The last wake-up was the fallback (the iteration before didn't reschedule).
    fallback: bool,
    started: Option<DateTime<Local>>,
}

#[derive(Default)]
pub struct Scheduler {
    tasks: Mutex<Vec<Task>>,
    self_paced: Mutex<Option<SelfPaced>>,
    /// A frontend fires due tasks between turns.
    driven: AtomicBool,
    /// Held while a `/loop` runs, so the machine stays awake between
    /// iterations (`keep_awake`).
    awake: Mutex<Option<crate::awake::Guard>>,
}

/// Set before the session is created by frontends that fire scheduled tasks
/// (the TUI and the plain REPL), so the scheduling tools are offered.
static FRONTEND_DRIVES: AtomicBool = AtomicBool::new(false);

pub fn set_frontend_drives() {
    FRONTEND_DRIVES.store(true, Ordering::Relaxed);
}

/// Whether the scheduling tools should be offered to the model.
pub fn available() -> bool {
    FRONTEND_DRIVES.load(Ordering::Relaxed)
        && std::env::var("CLAUDE_CODE_DISABLE_CRON").map_or(true, |v| v.is_empty() || v == "0")
}

fn new_id(tasks: &[Task]) -> String {
    loop {
        let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        if !tasks.iter().any(|t| t.id == id) {
            return id;
        }
    }
}

pub fn at(t: DateTime<Local>) -> String {
    if t.date_naive() == Local::now().date_naive() {
        t.format("%H:%M:%S").to_string()
    } else {
        t.format("%a %b %-d %H:%M").to_string()
    }
}

const SELF_PACED_NOTE: &str = "<system-reminder>This is an iteration of a self-paced /loop. When you finish this iteration, call ScheduleWakeup to \
pick when the next one runs: delay_seconds between 60 and 3600 (short while something is about to change, such as a build finishing; \
longer when nothing is pending) and a short reason. If the task is complete and no more iterations are needed, call ScheduleWakeup with \
stop: true instead. For events you can watch directly (a log, a CI status), the Monitor tool streams them to you without polling.</system-reminder>";

/// The built-in maintenance prompt for a bare `/loop`.
pub const MAINTENANCE_PROMPT: &str = "Loop iteration (maintenance). Work through these, in order, and stop at the first that has something to do:\n\
1. Continue any unfinished work from this conversation.\n\
2. Tend to the current branch's pull request, if there is one: address review comments, failed CI runs and merge conflicts.\n\
3. When nothing else is pending, run a cleanup pass such as a bug hunt or a simplification of recent changes.\n\
Don't start new initiatives outside that scope. Irreversible actions such as pushing or deleting only proceed when they continue \
something the conversation already authorized. If there is nothing to do, say so in one line.";

/// The default loop prompt: `loop.md` from the project (`.chanto/` or
/// `.claude/`), else from the user's Chanto or Claude home, else the
/// built-in maintenance prompt.
pub fn default_prompt(cwd: &std::path::Path) -> String {
    let candidates = [
        cwd.join(".chanto").join("loop.md"),
        cwd.join(".claude").join("loop.md"),
        crate::config::home_dir().join("loop.md"),
        crate::compat::claude_dir().join("loop.md"),
    ];
    for p in candidates {
        if let Ok(s) = std::fs::read_to_string(&p)
            && !s.trim().is_empty()
        {
            let mut s = s;
            if s.len() > LOOP_MD_MAX {
                let mut cut = LOOP_MD_MAX;
                while !s.is_char_boundary(cut) {
                    cut -= 1;
                }
                s.truncate(cut);
            }
            return s.trim().to_string();
        }
    }
    MAINTENANCE_PROMPT.into()
}

impl Scheduler {
    pub fn set_driven(&self) {
        self.driven.store(true, Ordering::Relaxed);
    }
    pub fn driven(&self) -> bool {
        self.driven.load(Ordering::Relaxed)
    }

    pub fn tasks(&self) -> Vec<Task> {
        let mut t = self.tasks.lock().unwrap().clone();
        t.sort_by_key(|t| t.next);
        t
    }

    /// Start a `/loop`, replacing any running one (cron tasks stay). With an
    /// interval the prompt runs on that schedule; without one the loop is
    /// self-paced. Either way the first iteration is due at once, so it runs
    /// as soon as the session is idle.
    pub fn start_loop(&self, every: Option<Duration>, prompt: Option<String>) {
        self.start_loop_inner(every, prompt);
        self.sync_awake();
    }

    fn start_loop_inner(&self, every: Option<Duration>, prompt: Option<String>) {
        self.stop_loop_inner();
        let now = Local::now();
        match every {
            Some(every) => {
                let mut tasks = self.tasks.lock().unwrap();
                let id = new_id(&tasks);
                tasks.push(Task {
                    id,
                    prompt,
                    schedule: Schedule::Every(every),
                    source: Source::Loop,
                    created: now,
                    next: now,
                    reason: None,
                });
            }
            None => {
                *self.self_paced.lock().unwrap() = Some(SelfPaced {
                    prompt,
                    started: Some(now),
                    rescheduled: true,
                    ..Default::default()
                });
                self.set_wakeup(now, None);
            }
        }
    }

    /// End the `/loop` (fixed or self-paced); tasks created with CronCreate
    /// stay. True if there was one.
    pub fn stop_loop(&self) -> bool {
        let r = self.stop_loop_inner();
        self.sync_awake();
        r
    }

    fn stop_loop_inner(&self) -> bool {
        let had_paced = self.self_paced.lock().unwrap().take().is_some();
        let mut tasks = self.tasks.lock().unwrap();
        let before = tasks.len();
        tasks.retain(|t| !matches!(t.source, Source::Loop | Source::Wakeup));
        had_paced || tasks.len() != before
    }

    /// Hold the machine awake exactly while a loop is active.
    fn sync_awake(&self) {
        let active = self.loop_active();
        let mut awake = self.awake.lock().unwrap();
        if active && awake.is_none() {
            *awake = Some(crate::awake::hold());
        } else if !active {
            *awake = None;
        }
    }

    pub fn loop_active(&self) -> bool {
        self.self_paced.lock().unwrap().is_some()
            || self
                .tasks
                .lock()
                .unwrap()
                .iter()
                .any(|t| t.source == Source::Loop)
    }

    /// One line on the running loop, if any.
    pub fn loop_status(&self) -> Option<String> {
        // Lock order everywhere: self_paced, then tasks.
        let paced = self.self_paced.lock().unwrap();
        let tasks = self.tasks.lock().unwrap();
        if let Some(t) = tasks.iter().find(|t| t.source == Source::Loop) {
            return Some(format!(
                "Looping {}: {} · next run {}",
                t.describe_schedule(),
                t.prompt_text(),
                at(t.next)
            ));
        }
        let p = paced.as_ref()?;
        let what = p
            .prompt
            .clone()
            .unwrap_or_else(|| "(default loop prompt)".into());
        Some(match tasks.iter().find(|t| t.source == Source::Wakeup) {
            Some(w) => format!(
                "Self-paced loop: {what} · next run {}{}",
                at(w.next),
                w.reason
                    .as_ref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default()
            ),
            None => format!("Self-paced loop: {what} · iteration running"),
        })
    }

    /// When the loop next runs (for the status line).
    pub fn loop_next(&self) -> Option<DateTime<Local>> {
        self.tasks
            .lock()
            .unwrap()
            .iter()
            .filter(|t| matches!(t.source, Source::Loop | Source::Wakeup))
            .map(|t| t.next)
            .min()
    }

    /// The earliest time anything is due.
    pub fn next_due(&self) -> Option<DateTime<Local>> {
        self.tasks.lock().unwrap().iter().map(|t| t.next).min()
    }

    /// A loop iteration's turn ended. For a self-paced loop that didn't call
    /// ScheduleWakeup: a fallback wake-up the first time, else the loop ends.
    /// Returns a note for the transcript.
    pub fn iteration_done(&self) -> Option<String> {
        let r = self.iteration_done_inner();
        self.sync_awake();
        r
    }

    fn iteration_done_inner(&self) -> Option<String> {
        let mut paced = self.self_paced.lock().unwrap();
        let p = paced.as_mut()?;
        if p.rescheduled {
            p.rescheduled = false;
            p.fallback = false;
            let tasks = self.tasks.lock().unwrap();
            let w = tasks.iter().find(|t| t.source == Source::Wakeup)?;
            return Some(format!(
                "↻ Next iteration at {} (in {}){}",
                at(w.next),
                crate::util::duration((w.next - Local::now()).num_seconds().max(0) as u64),
                w.reason
                    .as_ref()
                    .map(|r| format!(": {r}"))
                    .unwrap_or_default()
            ));
        }
        if p.fallback {
            *paced = None;
            self.tasks
                .lock()
                .unwrap()
                .retain(|t| t.source != Source::Wakeup);
            return Some("Loop ended: the last iterations didn't schedule another.".into());
        }
        p.fallback = true;
        let next = Local::now() + ChronoDuration::seconds(WAKE_FALLBACK as i64);
        self.set_wakeup(
            next,
            Some("fallback: the iteration didn't reschedule".into()),
        );
        Some(format!(
            "↻ The iteration didn't schedule the next one; checking again at {} (the loop ends if that one doesn't either).",
            at(next)
        ))
    }

    fn set_wakeup(&self, next: DateTime<Local>, reason: Option<String>) {
        let mut tasks = self.tasks.lock().unwrap();
        tasks.retain(|t| t.source != Source::Wakeup);
        let id = new_id(&tasks);
        tasks.push(Task {
            id,
            prompt: None,
            schedule: Schedule::Once,
            source: Source::Wakeup,
            created: Local::now(),
            next,
            reason,
        });
    }

    /// ScheduleWakeup: when the next self-paced iteration runs, or stop.
    pub fn schedule_wakeup(
        &self,
        delay_secs: Option<u64>,
        reason: Option<String>,
        stop: bool,
    ) -> Result<String, String> {
        let r = self.schedule_wakeup_inner(delay_secs, reason, stop);
        self.sync_awake();
        r
    }

    fn schedule_wakeup_inner(
        &self,
        delay_secs: Option<u64>,
        reason: Option<String>,
        stop: bool,
    ) -> Result<String, String> {
        let mut paced = self.self_paced.lock().unwrap();
        let Some(p) = paced.as_mut() else {
            return Err("No self-paced /loop is running: ScheduleWakeup only schedules the next iteration of one. Use CronCreate for a reminder or a recurring task.".into());
        };
        if stop {
            *paced = None;
            self.tasks
                .lock()
                .unwrap()
                .retain(|t| t.source != Source::Wakeup);
            return Ok("Loop stopped: no more iterations are scheduled.".into());
        }
        let Some(delay) = delay_secs else {
            return Err("Pass delay_seconds (60 to 3600), or stop: true to end the loop.".into());
        };
        let delay = delay.clamp(WAKE_MIN, WAKE_MAX);
        if let Some(started) = p.started
            && Local::now() + ChronoDuration::seconds(delay as i64)
                > started + ChronoDuration::from_std(EXPIRY).unwrap()
        {
            *paced = None;
            self.tasks
                .lock()
                .unwrap()
                .retain(|t| t.source != Source::Wakeup);
            return Ok("Loop ended: loops stop after 7 days.".into());
        }
        p.rescheduled = true;
        drop(paced);
        let next = Local::now() + ChronoDuration::seconds(delay as i64);
        self.set_wakeup(next, reason.clone().filter(|r| !r.trim().is_empty()));
        Ok(format!(
            "Next iteration at {} (in {}).",
            at(next),
            crate::util::duration(delay)
        ))
    }

    /// CronCreate. The model writes these prompts, so one that would read as
    /// typed input (`!shell`, `/command`, `#note`) is refused; the frontends
    /// also send cron fires to the model literally ([`Fire::literal`]).
    pub fn create(&self, cron: &str, prompt: &str, recurring: bool) -> Result<Task, String> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err("`prompt` is empty.".into());
        }
        if prompt.starts_with(['!', '/', '#']) {
            return Err("A scheduled prompt is sent to the model as plain text, so it can't start with `!`, `/` or `#` (those run shell commands, slash commands and memory notes only when the user types them). Describe the task in words instead, e.g. \"run the test suite and report failures\".".into());
        }
        let parsed = Cron::parse(cron).map_err(|e| format!("{e:#}"))?;
        let now = Local::now();
        let next = parsed
            .next_after(now)
            .ok_or_else(|| format!("`{cron}` never matches a date."))?;
        let mut tasks = self.tasks.lock().unwrap();
        if tasks.iter().filter(|t| t.source == Source::Cron).count() >= MAX_TASKS {
            return Err(format!(
                "A session holds at most {MAX_TASKS} scheduled tasks; delete one first."
            ));
        }
        let task = Task {
            id: new_id(&tasks),
            prompt: Some(prompt.to_string()),
            schedule: if recurring {
                Schedule::Cron(
                    Box::new(parsed),
                    cron.split_whitespace().collect::<Vec<_>>().join(" "),
                )
            } else {
                Schedule::Once
            },
            source: Source::Cron,
            created: now,
            next,
            reason: None,
        };
        tasks.push(task.clone());
        Ok(task)
    }

    /// CronDelete (also ends a loop when given its id).
    pub fn delete(&self, id: &str) -> Option<Task> {
        let r = self.delete_inner(id);
        self.sync_awake();
        r
    }

    fn delete_inner(&self, id: &str) -> Option<Task> {
        let mut paced = self.self_paced.lock().unwrap();
        let mut tasks = self.tasks.lock().unwrap();
        let i = tasks
            .iter()
            .position(|t| t.id == id || (id.len() >= 4 && t.id.starts_with(id)))?;
        let t = tasks.remove(i);
        if t.source == Source::Wakeup {
            *paced = None;
        }
        Some(t)
    }

    /// The next task that is due, advanced to its following run (or
    /// removed). Only call between turns: one fire per call.
    pub fn take_due(&self, now: DateTime<Local>, cwd: &std::path::Path) -> Option<Fire> {
        let r = self.take_due_inner(now, cwd);
        self.sync_awake();
        r
    }

    fn take_due_inner(&self, now: DateTime<Local>, cwd: &std::path::Path) -> Option<Fire> {
        let mut tasks = self.tasks.lock().unwrap();
        let i = tasks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.next <= now)
            .min_by_key(|(_, t)| t.next)
            .map(|(i, _)| i)?;
        let expiry = ChronoDuration::from_std(EXPIRY).unwrap();
        let t = tasks[i].clone();
        let expired = now >= t.created + expiry;
        let next = match &t.schedule {
            Schedule::Every(d) => {
                let step = ChronoDuration::from_std(*d).unwrap_or(ChronoDuration::minutes(10));
                let mut n = t.next;
                while n <= now {
                    n += step;
                }
                Some(n)
            }
            Schedule::Cron(c, _) => c.next_after(now),
            Schedule::Once => None,
        };
        match next {
            Some(n) if !expired => tasks[i].next = n,
            _ => {
                tasks.remove(i);
            }
        }
        drop(tasks);
        let ending = if expired && !matches!(t.schedule, Schedule::Once) {
            " (final run: scheduled tasks end after 7 days)"
        } else {
            ""
        };
        Some(match t.source {
            Source::Loop => Fire {
                id: t.id.clone(),
                source: Source::Loop,
                literal: t.prompt.is_none(),
                prompt: t.prompt.clone().unwrap_or_else(|| default_prompt(cwd)),
                note: format!("↻ Loop iteration{ending}"),
            },
            Source::Wakeup => {
                let prompt = self.self_paced.lock().unwrap().as_mut().map(|p| {
                    p.rescheduled = false;
                    p.prompt.clone()
                });
                // The loop was stopped meanwhile: nothing to run.
                let prompt = prompt?;
                Fire {
                    id: t.id.clone(),
                    source: Source::Wakeup,
                    literal: prompt.is_none(),
                    prompt: self_paced_prompt(prompt, cwd),
                    note: "↻ Loop iteration".into(),
                }
            }
            Source::Cron => Fire {
                id: t.id.clone(),
                source: Source::Cron,
                literal: true,
                prompt: format!(
                    "{}\n\n<system-reminder>This prompt was scheduled with CronCreate (task {}, {}){}.</system-reminder>",
                    t.prompt_text(),
                    t.id,
                    t.describe_schedule(),
                    ending
                ),
                note: format!(
                    "⏰ Scheduled task {} ({}){ending}",
                    t.id,
                    t.describe_schedule()
                ),
            },
        })
    }

    /// The `session_crons` field of Stop hook input.
    pub fn hook_json(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.tasks()
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "id": t.id,
                        "schedule": t.describe_schedule(),
                        "kind": match t.source { Source::Loop => "loop", Source::Wakeup => "wakeup", Source::Cron => "cron" },
                        "recurring": !matches!(t.schedule, Schedule::Once),
                        "next_run": t.next.to_rfc3339(),
                        "prompt": t.prompt_text(),
                    })
                })
                .collect(),
        )
    }
}

fn self_paced_prompt(prompt: Option<String>, cwd: &std::path::Path) -> String {
    format!(
        "{}\n\n{SELF_PACED_NOTE}",
        prompt.unwrap_or_else(|| default_prompt(cwd))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> std::path::PathBuf {
        std::env::temp_dir()
    }

    #[test]
    fn fixed_loop_fires_on_its_interval_and_stops() {
        let s = Scheduler::default();
        s.start_loop(Some(Duration::from_secs(300)), Some("check CI".into()));
        assert!(s.loop_active());
        assert!(
            s.awake.lock().unwrap().is_some(),
            "a loop keeps the machine awake"
        );
        let now = Local::now();
        // The first run is due at once.
        assert_eq!(s.take_due(now, &dir()).unwrap().prompt, "check CI");
        assert!(s.take_due(now, &dir()).is_none(), "not due yet");
        let fire = s
            .take_due(now + ChronoDuration::seconds(301), &dir())
            .unwrap();
        assert_eq!(
            (fire.source, fire.prompt.as_str()),
            (Source::Loop, "check CI")
        );
        // Advanced past now: no catch-up for missed runs.
        assert!(
            s.take_due(now + ChronoDuration::seconds(302), &dir())
                .is_none()
        );
        assert!(s.loop_status().unwrap().contains("every 5m"));
        assert!(s.stop_loop());
        assert!(!s.loop_active() && s.next_due().is_none());
        assert!(s.awake.lock().unwrap().is_none());
    }

    #[test]
    fn self_paced_loop_reschedules_falls_back_then_ends() {
        let s = Scheduler::default();
        s.start_loop(None, Some("watch the deploy".into()));
        let first = s.take_due(Local::now(), &dir()).unwrap();
        assert!(
            first.prompt.starts_with("watch the deploy") && first.prompt.contains("ScheduleWakeup")
        );
        // Clamped to the 60s minimum.
        let out = s
            .schedule_wakeup(Some(5), Some("build finishing".into()), false)
            .unwrap();
        assert!(out.contains("in 1m"), "{out}");
        assert!(s.iteration_done().unwrap().contains("build finishing"));
        let fire = s
            .take_due(Local::now() + ChronoDuration::seconds(61), &dir())
            .unwrap();
        assert_eq!(fire.source, Source::Wakeup);
        // This iteration doesn't reschedule: one fallback wake-up, ~20 minutes out.
        assert!(s.iteration_done().unwrap().contains("didn't schedule"));
        let next = s.next_due().unwrap();
        assert!((next - Local::now()).num_seconds() > 19 * 60);
        s.take_due(next, &dir()).unwrap();
        // Still no reschedule: the loop ends.
        assert!(s.iteration_done().unwrap().contains("Loop ended"));
        assert!(!s.loop_active());
        assert!(
            s.schedule_wakeup(Some(60), None, false).is_err(),
            "no loop to schedule"
        );
    }

    #[test]
    fn stop_true_ends_a_self_paced_loop() {
        let s = Scheduler::default();
        s.start_loop(None, None);
        assert!(
            s.take_due(Local::now(), &dir())
                .unwrap()
                .prompt
                .contains("ScheduleWakeup")
        );
        s.schedule_wakeup(Some(600), None, false).unwrap();
        assert!(
            s.schedule_wakeup(None, None, true)
                .unwrap()
                .contains("stopped")
        );
        assert!(!s.loop_active() && s.next_due().is_none());
    }

    #[test]
    fn cron_tasks_recur_or_fire_once() {
        let s = Scheduler::default();
        assert!(s.create("61 * * * *", "x", true).is_err());
        assert!(s.create("* * * * *", " ", true).is_err());
        let every = s.create("*/5 * * * *", "poll the PR", true).unwrap();
        let once = s.create("* * * * *", "remind me", false).unwrap();
        assert_eq!(every.id.len(), 8);
        let later = Local::now() + ChronoDuration::minutes(6);
        let a = s.take_due(later, &dir()).unwrap();
        let b = s.take_due(later, &dir()).unwrap();
        assert!(s.take_due(later, &dir()).is_none());
        let fired: Vec<&str> = vec![a.id.as_str(), b.id.as_str()];
        assert!(fired.contains(&every.id.as_str()) && fired.contains(&once.id.as_str()));
        assert!(a.prompt.contains("CronCreate"));
        // The one-shot deleted itself; the recurring one remains.
        assert_eq!(
            s.tasks().iter().map(|t| t.id.clone()).collect::<Vec<_>>(),
            vec![every.id.clone()]
        );
        assert!(s.delete(&every.id).is_some() && s.tasks().is_empty());
        // Loops are separate from cron tasks.
        s.create("0 9 * * 1-5", "standup notes", true).unwrap();
        s.start_loop(Some(Duration::from_secs(60)), Some("x".into()));
        s.stop_loop();
        assert_eq!(s.tasks().len(), 1);
    }

    #[test]
    fn model_scheduled_prompts_never_read_as_typed_input() {
        let s = Scheduler::default();
        for p in ["!touch /tmp/x", " /review", "#remember this", "\n!id"] {
            assert!(s.create("* * * * *", p, true).is_err(), "{p:?}");
        }
        s.create("* * * * *", "run the tests", false).unwrap();
        let later = Local::now() + ChronoDuration::minutes(2);
        assert!(s.take_due(later, &dir()).unwrap().literal);
        // The user's own `/loop 5m /cmd` may run a command; the default
        // prompt (which `loop.md` in the project can supply) may not.
        s.start_loop(Some(Duration::from_secs(300)), Some("/babysit".into()));
        assert!(!s.take_due(Local::now(), &dir()).unwrap().literal);
        s.start_loop(Some(Duration::from_secs(300)), None);
        assert!(s.take_due(Local::now(), &dir()).unwrap().literal);
        s.start_loop(None, None);
        assert!(s.take_due(Local::now(), &dir()).unwrap().literal);
        s.stop_loop();
    }

    #[test]
    fn recurring_tasks_fire_a_final_time_after_a_week() {
        let s = Scheduler::default();
        let t = s.create("* * * * *", "x", true).unwrap();
        let late = t.created + ChronoDuration::days(8);
        let f = s.take_due(late, &dir()).unwrap();
        assert!(f.note.contains("final run"));
        assert!(s.tasks().is_empty());
    }

    #[test]
    fn default_prompt_prefers_loop_md() {
        let d =
            std::env::temp_dir().join(format!("chanto-loopmd-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(d.join(".claude")).unwrap();
        std::fs::write(d.join(".claude/loop.md"), "Keep the release PR green.\n").unwrap();
        assert_eq!(default_prompt(&d), "Keep the release PR green.");
        let _ = std::fs::remove_dir_all(&d);
    }
}
