//! Line-oriented frontend: used for `-p` print mode, non-TTY output, and
//! `--plain` interactive mode.

use crate::agent::{Event, Frontend, PermissionReply, PermissionRequest};
use crate::ui::{self, MarkdownStream, Spinner};
use async_trait::async_trait;
use crossterm::style::{Color, Stylize};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Human-readable streaming transcript.
    Live,
    /// Only the final answer (print mode).
    Text,
    /// JSON lines, one per event.
    StreamJson,
    /// Nothing until the end (caller prints a JSON result).
    Json,
}

pub struct PlainFrontend {
    pub format: OutputFormat,
    pub show_thinking: bool,
    /// Quiet mode (`--plain`): only the spinner (naming the running tool),
    /// prompts, notices and errors while working; the REPL prints the final
    /// summary with [`PlainFrontend::print_final`].
    quiet: AtomicBool,
    state: Mutex<State>,
}

struct State {
    md: MarkdownStream,
    spinner: Option<Spinner>,
    in_reasoning: bool,
    text_open: bool,
    /// Top-level tool calls this turn (quiet mode's spinner shows the count).
    tools: usize,
}

impl PlainFrontend {
    pub fn new(format: OutputFormat, show_thinking: bool) -> Self {
        PlainFrontend {
            format,
            show_thinking,
            quiet: AtomicBool::new(false),
            state: Mutex::new(State {
                md: MarkdownStream::new(),
                spinner: None,
                in_reasoning: false,
                text_open: false,
                tools: 0,
            }),
        }
    }

    /// Switch quiet mode for the next turn (and restart its tool count).
    pub fn set_quiet(&self, on: bool) {
        self.quiet.store(on, Ordering::Relaxed);
        self.state.lock().unwrap().tools = 0;
    }

    /// Quiet mode: print the turn's final summary, which the live view held back.
    pub fn print_final(&self, text: &str) {
        let mut st = self.state.lock().unwrap();
        Self::stop_spinner(&mut st);
        Self::close_text(&mut st);
        let text = crate::util::strip_terminal_escapes(text.trim());
        if text.is_empty() {
            return;
        }
        ui::out(&format!("{} ", "❖".with(Color::White)));
        let s = st.md.push(&text);
        ui::out(&s);
        let rest = st.md.finish();
        ui::out(&format!("{rest}\n"));
    }

    fn stop_spinner(st: &mut State) {
        if let Some(mut s) = st.spinner.take() {
            s.stop();
        }
    }

    fn close_text(st: &mut State) {
        if st.in_reasoning {
            ui::out("\x1b[0m\n");
            st.in_reasoning = false;
        }
        if st.text_open {
            let rest = st.md.finish();
            ui::out(&format!("{rest}\n"));
            st.text_open = false;
        }
    }

    /// Ask a question; `free_text` adds a "Type something else…" choice.
    async fn ask(&self, question: &str, options: &[String], free_text: bool) -> Option<String> {
        {
            let mut st = self.state.lock().unwrap();
            Self::stop_spinner(&mut st);
            Self::close_text(&mut st);
        }
        let mut opts = options.to_vec();
        if free_text {
            opts.push("Type something else…".into());
        }
        let q = question.to_string();
        tokio::task::spawn_blocking(move || {
            let c = ui::choose("Question", &q, &opts)?;
            if free_text && c == opts.len() - 1 {
                ui::read_line("  › ").filter(|s| !s.trim().is_empty())
            } else {
                Some(opts[c].clone())
            }
        })
        .await
        .ok()
        .flatten()
    }
}

/// What `-p` text and json output put on stderr: errors and warnings, as
/// `(is_error, message)`.
fn print_mode_diagnostic(ev: &Event) -> Option<(bool, &str)> {
    match ev {
        Event::Error(e) => Some((true, e)),
        Event::Warn(w) => Some((false, w)),
        _ => None,
    }
}

/// What quiet mode leaves out of the live transcript: the assistant's text
/// and reasoning (the final summary prints after the turn), and tool blocks.
/// Notices, warnings, errors, prompts and the spinner still show.
fn quiet_hides(ev: &Event) -> bool {
    matches!(
        ev,
        Event::AssistantStart
            | Event::Text(_)
            | Event::Reasoning(_)
            | Event::AssistantEnd
            | Event::ToolStart { .. }
            | Event::ToolEnd { .. }
    )
}

#[async_trait]
impl Frontend for PlainFrontend {
    fn emit(&self, ev: Event) {
        match self.format {
            OutputFormat::Json | OutputFormat::Text => {
                // Stdout carries only the answer; warnings go to stderr, so a
                // script still learns the run stopped at max_turns, hit the
                // output limit or switched to a fallback model.
                match print_mode_diagnostic(&ev) {
                    Some((true, e)) => ui::error(e),
                    Some((false, w)) => ui::warn(w),
                    None => {}
                }
                return;
            }
            OutputFormat::StreamJson => {
                let Some(v) = crate::agent::event_json(&ev) else {
                    return;
                };
                if matches!(
                    v["type"].as_str(),
                    Some(
                        "busy"
                            | "assistant_start"
                            | "assistant_end"
                            | "tool_progress"
                            | "todos"
                            | "mode"
                    )
                ) {
                    return;
                }
                println!("{v}");
                return;
            }
            OutputFormat::Live => {}
        }
        let mut st = self.state.lock().unwrap();
        if self.quiet.load(Ordering::Relaxed) && quiet_hides(&ev) {
            // The spinner says which tool runs, and how many have.
            if let Event::ToolStart {
                label,
                parent: None,
                ..
            } = &ev
            {
                st.tools += 1;
                Self::stop_spinner(&mut st);
                st.spinner = Some(Spinner::start(&format!(
                    "{} · {}",
                    crate::util::one_line(label, 60),
                    crate::util::plural(st.tools, "tool call")
                )));
            }
            return;
        }
        match ev {
            Event::Busy(Some(label)) => {
                Self::stop_spinner(&mut st);
                st.spinner = Some(Spinner::start(&label));
            }
            Event::Busy(None) => Self::stop_spinner(&mut st),
            Event::AssistantStart => {}
            Event::Background {
                id,
                description,
                status,
            } => {
                Self::stop_spinner(&mut st);
                Self::close_text(&mut st);
                ui::line(&ui::dim(&format!(
                    "⧉ background agent {id} ({description}): {status}"
                )));
            }
            Event::Reasoning(t) => {
                Self::stop_spinner(&mut st);
                if !self.show_thinking {
                    return;
                }
                if !st.in_reasoning {
                    ui::out(&format!(
                        "{}",
                        "❖ Thinking…\n".with(Color::DarkGrey).italic()
                    ));
                    st.in_reasoning = true;
                    ui::out("\x1b[90;3m");
                }
                ui::out(&crate::util::strip_terminal_escapes(&t));
            }
            Event::Text(t) => {
                Self::stop_spinner(&mut st);
                if st.in_reasoning {
                    ui::out("\x1b[0m\n\n");
                    st.in_reasoning = false;
                }
                if !st.text_open {
                    ui::out(&format!("{} ", "❖".with(Color::White)));
                    st.text_open = true;
                }
                // The model's own escapes go; the renderer adds its styling after.
                let s = st.md.push(&crate::util::strip_terminal_escapes(&t));
                ui::out(&s);
            }
            Event::AssistantEnd => {
                Self::stop_spinner(&mut st);
                Self::close_text(&mut st);
            }
            Event::ToolStart { label, parent, .. } => {
                Self::stop_spinner(&mut st);
                Self::close_text(&mut st);
                ui::tool_header(&label, if parent.is_some() { 1 } else { 0 });
            }
            Event::ToolEnd {
                display,
                is_error,
                parent,
                ..
            } => {
                Self::stop_spinner(&mut st);
                let lines: Vec<&str> = display.lines().collect();
                let shown = if lines.len() > 12 {
                    format!(
                        "{}\n… +{}",
                        lines[..12].join("\n"),
                        crate::util::plural(lines.len() - 12, "line")
                    )
                } else {
                    display.clone()
                };
                ui::tool_result(&shown, is_error, if parent.is_some() { 1 } else { 0 });
            }
            Event::Info(s) => {
                Self::close_text(&mut st);
                ui::line(&ui::wrapped(&s, Color::DarkGrey));
            }
            Event::Warn(s) => {
                Self::stop_spinner(&mut st);
                ui::line(&ui::wrapped(&s, Color::Yellow));
            }
            Event::Error(s) => {
                Self::stop_spinner(&mut st);
                ui::line(&ui::wrapped(&s, Color::Red));
            }
            Event::Todos(_) | Event::ToolProgress { .. } => {}
            Event::Usage { .. } => {}
            Event::ModeChanged(m) => ui::line(&ui::dim(&format!("mode: {}", m.label()))),
            Event::Compacted { before, after } => ui::line(&ui::dim(&format!(
                "❖ Conversation compacted ({before} → {after} tokens)"
            ))),
            Event::Limits(s) => ui::line(&ui::dim(&format!("❖ Account limits: {}", s.summary()))),
            Event::Status { key, lines } => {
                // The spend guard's footer marker repeats a notice already printed.
                if !lines.is_empty() && key != "spend" {
                    ui::line(&ui::wrapped(&lines.join("\n"), Color::DarkGrey));
                }
            }
        }
    }

    async fn permission(&self, req: PermissionRequest) -> PermissionReply {
        {
            let mut st = self.state.lock().unwrap();
            Self::stop_spinner(&mut st);
            Self::close_text(&mut st);
        }
        let opts = vec![
            if req.tool == "SandboxNetwork" {
                "Allow once".to_string()
            } else {
                "Yes".to_string()
            },
            req.always_label.clone(),
            "No, and tell the model what to do differently".to_string(),
        ];
        let title = format!("{} wants to run", req.tool);
        let detail = match req.reason.as_deref().filter(|r| !r.is_empty()) {
            Some(r) => format!("{r}\n\n{}", req.detail),
            None => req.detail.clone(),
        };

        tokio::task::spawn_blocking(move || {
            let c = ui::choose(&title, &detail, &opts);
            match c {
                Some(0) => PermissionReply::Allow,
                Some(1) => PermissionReply::AllowAlways,
                Some(_) => {
                    let fb = ui::read_line(
                        "  What should the model do instead? (enter to just stop) › ",
                    )
                    .unwrap_or_default();
                    if fb.trim().is_empty() {
                        PermissionReply::Deny(None)
                    } else {
                        PermissionReply::Deny(Some(fb))
                    }
                }
                None => PermissionReply::Deny(None),
            }
        })
        .await
        .unwrap_or(PermissionReply::Deny(None))
    }

    async fn question(&self, question: &str, options: &[String]) -> Option<String> {
        self.ask(question, options, true).await
    }

    async fn choose(&self, question: &str, options: &[String]) -> Option<String> {
        self.ask(question, options, false).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_mode_reports_warnings_on_stderr() {
        // `-p` used to drop warnings, so "Stopped after 1 model calls
        // (max_turns)" ended with empty output, exit 0 and nothing said.
        let w = Event::Warn("Stopped after 1 model calls (max_turns).".into());
        assert_eq!(
            print_mode_diagnostic(&w),
            Some((false, "Stopped after 1 model calls (max_turns)."))
        );
        assert_eq!(
            print_mode_diagnostic(&Event::Error("boom".into())),
            Some((true, "boom"))
        );
        assert_eq!(print_mode_diagnostic(&Event::Text("answer".into())), None);
    }

    #[test]
    fn quiet_hides_steps_but_not_notices() {
        assert!(quiet_hides(&Event::Text("Let me look".into())));
        assert!(quiet_hides(&Event::Reasoning("hmm".into())));
        assert!(quiet_hides(&Event::ToolStart {
            id: "1".into(),
            label: "Bash(ls)".into(),
            parent: None
        }));
        for ev in [
            Event::Warn("spend".into()),
            Event::Error("boom".into()),
            Event::Info("✓ Verified: cargo test".into()),
            Event::Busy(Some("Working".into())),
        ] {
            assert!(!quiet_hides(&ev));
        }
    }
}
