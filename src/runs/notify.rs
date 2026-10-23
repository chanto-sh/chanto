//! Notifications when a background run finishes, fails or needs input:
//! ntfy (a plain HTTP POST, works with the ntfy phone app), a JSON webhook,
//! and desktop notifications for the local daemon.
//!
//! Bodies carry the run id, its state, the verify verdict and a link to the
//! web UI, never the task, the code, output or error text: notification
//! services and lock screens are not places for either.

use super::{Run, RunState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

/// `[notify]` in config.toml.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NotifyConfig {
    /// ntfy topic URL, e.g. "https://ntfy.sh/my-private-topic".
    pub ntfy: Option<String>,
    /// URL that receives a JSON POST per notification.
    pub webhook: Option<String>,
    /// Desktop notifications (default: on for the local daemon).
    pub desktop: Option<bool>,
    /// Base URL for links to the web UI (default: the server's own URL).
    pub link: Option<String>,
}

impl NotifyConfig {
    pub fn merge(&mut self, o: NotifyConfig) {
        if o.ntfy.is_some() {
            self.ntfy = o.ntfy;
        }
        if o.webhook.is_some() {
            self.webhook = o.webhook;
        }
        if o.desktop.is_some() {
            self.desktop = o.desktop;
        }
        if o.link.is_some() {
            self.link = o.link;
        }
    }
}

/// What a notification says.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub title: String,
    pub message: String,
    pub state: RunState,
    pub run_id: String,
    pub link: Option<String>,
}

impl Note {
    pub fn for_run(run: &Run, link_base: Option<&str>) -> Option<Note> {
        let what = match run.state {
            RunState::Done => "done",
            RunState::Failed => "failed",
            RunState::NeedsInput => "needs input",
            _ => return None,
        };
        let mut message = format!(
            "Background run {} {}.",
            run.id,
            if run.state == RunState::NeedsInput {
                "is waiting for an answer"
            } else {
                what
            }
        );
        if let (RunState::Done | RunState::Failed, Some(v)) = (run.state, &run.verify)
            && v.status != "off"
        {
            message.push_str(&format!(" Verification: {}.", v.status));
        }
        Some(Note {
            title: {
                // Goal and team runs say so; ids only, never the task.
                let kind = if run.kind.as_deref() == Some("goal") {
                    "goal run"
                } else {
                    "run"
                };
                let team = run
                    .team
                    .as_ref()
                    .map(|t| format!("team {t} "))
                    .unwrap_or_default();
                match &run.job {
                    Some(job) => format!("chanto: job {job} ({team}{kind} {}) {what}", run.id),
                    None => format!("chanto: {team}{kind} {} {what}", run.id),
                }
            },
            message,
            state: run.state,
            run_id: run.id.clone(),
            link: link_base.map(|b| format!("{}/#run={}", b.trim_end_matches('/'), run.id)),
        })
    }

    /// The webhook's JSON body.
    pub fn json(&self) -> Value {
        json!({"event": format!("run.{}", self.state.label()), "run_id": self.run_id, "state": self.state,
            "title": self.title, "message": self.message, "url": self.link})
    }
}

pub struct Notifier {
    cfg: NotifyConfig,
    desktop: bool,
    link_base: Option<String>,
    http: reqwest::Client,
}

impl Notifier {
    /// `desktop_default`: whether desktop notifications are on unless configured.
    pub fn new(cfg: NotifyConfig, desktop_default: bool, server_url: Option<String>) -> Notifier {
        let desktop = cfg.desktop.unwrap_or(desktop_default);
        let link_base = cfg.link.clone().or(server_url);
        Notifier {
            cfg,
            desktop,
            link_base,
            http: crate::http::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
        }
    }

    pub fn note(&self, run: &Run) -> Option<Note> {
        Note::for_run(run, self.link_base.as_deref())
    }

    /// Send everywhere configured; failures are logged, never fatal.
    pub async fn send(&self, note: &Note) {
        if let Some(url) = &self.cfg.ntfy {
            let tag = match note.state {
                RunState::Done => "white_check_mark",
                RunState::Failed => "x",
                _ => "question",
            };
            let mut req = self
                .http
                .post(url)
                .header("Title", &note.title)
                .header("Tags", tag)
                .body(note.message.clone());
            if note.state == RunState::NeedsInput {
                req = req.header("Priority", "high");
            }
            if let Some(l) = &note.link {
                req = req.header("Click", l);
            }
            if let Err(e) = req.send().await.and_then(|r| r.error_for_status()) {
                eprintln!("ntfy notification failed: {e}");
            }
        }
        if let Some(url) = &self.cfg.webhook
            && let Err(e) = self
                .http
                .post(url)
                .json(&note.json())
                .send()
                .await
                .and_then(|r| r.error_for_status())
        {
            eprintln!("webhook notification failed: {e}");
        }
        if self.desktop {
            desktop(&note.title, &note.message, note.link.as_deref());
        }
    }
}

/// Post a desktop notification through the OS. Shares the TUI's command so a
/// click focuses the terminal (macOS: `terminal-notifier` when installed;
/// Linux: `notify-send`) and opens a run's link.
fn desktop(title: &str, message: &str, link: Option<&str>) {
    if let Some(mut c) = crate::tui::notify::system_command(title, message, link) {
        let _ = c
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}
