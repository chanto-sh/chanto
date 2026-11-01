//! Full-screen terminal UI (ratatui): streaming transcript with collapsible
//! tool/thinking blocks (click or ctrl+o), mouse scrolling, permission and
//! question dialogs, slash/@file completion, todo panel and status bar.

mod agents_panel;
mod attach;
mod dialog;
mod draw;
mod events;
mod input;
mod keys;
mod links;
mod mouse;
mod pickers;
mod render;
mod run;
mod stream;

pub mod keybindings;
pub mod markdown;
pub(crate) mod notify;
pub mod theme;

use attach::*;
use dialog::*;
use links::*;
use mouse::*;
use pickers::*;
use render::*;
use run::restore_terminal;
use stream::*;

pub use run::run;

use crate::agent::{Agent, Event, Frontend, PermissionReply, PermissionRequest, Queued, TurnEnd};
use crate::commands::{self, Outcome};
use crate::llm::Role;
use crate::permissions::Mode;
use crate::tools::misc::Todo;
use crate::{Cli, Setup, setup};
use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event as TermEvent, EventStream, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use input::Input;
use markdown::{cyan, faint, highlight_bg, highlight_fg, panel_bg, warn};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as WBlock, BorderType, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use theme::pal;
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------- messages

enum AppMsg {
    /// A suggested next prompt (`prompt_suggestions`).
    Suggestion(String),
    /// A `/btw` answer arrived: show it in the overlay.
    OpenBtw,
    /// A `/btw` ask failed: show the reason in the panel that asked it.
    BtwFailed(String),
    /// A `/mcp` panel action finished: show its result in the panel.
    McpNote(String),
    Agent(Event),
    Permission(PermissionRequest, oneshot::Sender<PermissionReply>),
    /// The question, its options, whether a free-text answer is allowed.
    Question(
        String,
        Vec<String>,
        bool,
        oneshot::Sender<Option<(usize, String)>>,
    ),
    Show(String),
    Completions(Vec<(String, String)>),
    Reset(Option<String>),
    TurnDone(Option<String>),
    Exit,
    /// SIGTERM or SIGHUP: interrupt, save and exit (with the code in `util::TERMINATED`).
    Terminate,
    /// Open a picker; choosing an item submits the corresponding command.
    Picker {
        title: String,
        labels: Vec<String>,
        values: Vec<String>,
    },
    Submit(String),
    StatusLine(String),
    /// The recorder started (or couldn't).
    VoiceStarted(Result<crate::voice::Recording, String>),
    /// A dictation's transcript (or why there is none).
    Voice(Result<String, String>),
    /// From the phone, while `/remote` is on.
    Remote(crate::remote::Input),
}

/// Voice input (ctrl+space or `/voice`): record, then transcribe into the prompt.
enum Voice {
    Off,
    Starting(crate::voice::Backend),
    Recording(Box<crate::voice::Recording>, crate::voice::Backend),
    Transcribing(Instant),
}

/// How framed boxes (input, dialogs) are drawn (`input_border` in the config).
///
/// A thin outline drawn with box-drawing glyphs (`╭─│`) can show a hairline of
/// the background where a corner meets an edge: some fonts (SF Mono among
/// them) draw the corner's stem a pixel short of the cell edge, so the
/// terminal's background shows through as a speck that comes and goes as the
/// screen repaints. `Block` draws the frame from block elements (`▁▏▔▕`),
/// which fill the part of the cell they cover, so it tiles cleanly on any
/// font. `Solid` paints the whole border cell.
/// Where a message typed while a turn runs goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Deliver {
    /// To the model at its next step (enter).
    #[default]
    Step,
    /// After the turn finishes (ctrl+x enter).
    Queue,
    /// Now, cutting the turn short (ctrl+x ctrl+s, ctrl+enter).
    Now,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum FrameStyle {
    #[default]
    Block,
    Line,
    Solid,
}

/// A framed box in the given style and colour.
/// The session's cost for the status line: the amount once there is one, with
/// `?` when some calls had no known price, and nothing while it's free (local
/// models, plan logins), which `/cost` still reports.
fn cost_label(total: f64, unknown: bool) -> Option<String> {
    match (total > 0.0, unknown) {
        (true, false) => Some(crate::util::usd(total)),
        (true, true) => Some(format!("{}?", crate::util::usd(total))),
        (false, true) => Some("$?".into()),
        (false, false) => None,
    }
}

/// The current branch's pull request in the status line: its number, then
/// what it's waiting on. Checked with `gh` once a minute in the background;
/// nothing shows without `gh`, a GitHub remote or an open PR.
fn watch_pr_status(cwd: std::path::PathBuf) -> std::sync::Arc<std::sync::Mutex<Option<String>>> {
    let status = std::sync::Arc::new(std::sync::Mutex::new(None));
    if !crate::shell::command_exists("gh") {
        return status;
    }
    let out = status.clone();
    std::thread::spawn(move || {
        loop {
            let view = std::process::Command::new("gh")
                .args([
                    "pr",
                    "view",
                    "--json",
                    "number,state,reviewDecision,statusCheckRollup",
                ])
                .current_dir(&cwd)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output();
            let text = view
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
                .and_then(|v| pr_label(&v));
            *out.lock().unwrap() = text;
            // Only while Chanto runs: the TUI holds the other end.
            if std::sync::Arc::strong_count(&out) == 1 {
                break;
            }
            std::thread::sleep(Duration::from_secs(60));
        }
    });
    status
}

/// `gh pr view --json number,state,reviewDecision,statusCheckRollup` → "PR #12 checks failing".
fn pr_label(v: &serde_json::Value) -> Option<String> {
    let n = v["number"].as_u64()?;
    if v["state"].as_str() != Some("OPEN") {
        return Some(format!(
            "PR #{n} {}",
            v["state"].as_str().unwrap_or("").to_lowercase()
        ));
    }
    let checks: Vec<&str> = v["statusCheckRollup"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            c["conclusion"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or(c["state"].as_str())
                .or(c["status"].as_str())
        })
        .collect();
    let failing = checks.iter().any(|c| {
        matches!(
            *c,
            "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED"
        )
    });
    let running = checks.iter().any(|c| {
        matches!(
            *c,
            "PENDING" | "IN_PROGRESS" | "QUEUED" | "EXPECTED" | "WAITING"
        )
    });
    let state = if failing {
        "checks failing"
    } else if running {
        "checks running"
    } else {
        match v["reviewDecision"].as_str() {
            Some("APPROVED") => "approved",
            Some("CHANGES_REQUESTED") => "changes requested",
            Some("REVIEW_REQUIRED") => "awaiting review",
            _ => "open",
        }
    };
    Some(format!("PR #{n} {state}"))
}

/// What the status line says about reasoning: the chosen effort, "thinking off",
/// or nothing when the provider's default applies.
/// Pasted text with `\n` line breaks. Terminal.app and many others send each
/// line break of a bracketed paste as `\r`, which would count as one line and
/// reach the model as carriage returns.
fn paste_tag_range(text: &str, index: usize) -> Option<std::ops::Range<usize>> {
    let start = text.find(&format!("[Pasted text #{} +", index + 1))?;
    let end = text[start..].find(" lines]")?;
    Some(start..start + end + 7)
}

/// `text` with its `[Pasted text #n +N lines]` tags replaced by the pastes. An
/// expanded paste (ctrl+o) left no tag behind; it's already text.
fn expand_pastes(text: &str, pastes: &[String]) -> String {
    let mut text = text.to_string();
    for (i, paste) in pastes.iter().enumerate() {
        if let Some(range) = paste_tag_range(&text, i) {
            text.replace_range(range, paste);
        }
    }
    text
}

fn normalize_paste(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

fn effort_label(think: bool, effort: Option<String>) -> Option<String> {
    if !think {
        return Some("thinking off".into());
    }
    effort.filter(|e| !e.is_empty())
}

/// The `/btw` overlay as screen rows: up to five earlier questions as dim
/// context, the selected question above its answer, and the keys on their own
/// dim line. While an answer is still on its way the question is shown with a
/// progress line in place of the answer. Built as rows rather than markdown so
/// the context and the keys are visibly dimmer than the answer they frame.
fn btw_overlay_lines(
    btw: &[(String, String)],
    ask: Option<&BtwAsk>,
    index: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(pal().dim);
    let mut out: Vec<Line<'static>> = Vec::new();
    // The question in flight is a virtual entry after the answered ones.
    let pending = if index >= btw.len() { ask } else { None };
    let (q, a): (&str, &str) = match (pending, btw.get(index)) {
        (Some(p), _) => (p.question.as_str(), ""),
        (None, Some((q, a))) => (q.as_str(), a.as_str()),
        (None, None) => {
            out.extend(markdown::wrap(
                &Line::styled(
                    "No side questions yet — ask one with /btw <question>.".to_string(),
                    dim,
                ),
                width,
                0,
            ));
            return out;
        }
    };
    // Earlier questions are context, numbered so they read as a thread rather
    // than as the opening of the answer.
    let hi = index.min(btw.len());
    let start = hi.saturating_sub(5);
    for (i, (eq, _)) in btw[start..hi].iter().enumerate() {
        let text = format!("{:>2} ↳ {}", start + i + 1, crate::util::one_line(eq, 100));
        out.extend(markdown::wrap(&Line::styled(text, dim), width, 2));
    }
    if index > 0 {
        out.push(Line::raw(""));
    }
    // The question you're reading, above its answer, marked with the same bar a
    // transcript quote uses so it doesn't read as another answer line.
    let bar = Style::default()
        .fg(pal().accent)
        .add_modifier(Modifier::BOLD);
    let question = Line::from(vec![
        Span::styled("│ ", bar),
        Span::styled(
            crate::util::one_line(q, 400),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]);
    out.extend(markdown::wrap(&question, width, 2));
    out.push(Line::raw(""));
    if let Some(p) = pending {
        // Still in flight (or failed): instead of an answer, the panel says
        // which. Only `esc` does anything until the answer lands.
        let style = if p.error.is_some() {
            Style::default().fg(pal().error)
        } else {
            dim
        };
        out.extend(markdown::wrap(
            &Line::styled(ask_progress(p), style),
            width,
            2,
        ));
        out.push(Line::raw(""));
        out.extend(markdown::wrap(
            &Line::styled("esc close".to_string(), dim),
            width,
            0,
        ));
        return out;
    }
    // The answer in full, rendered as markdown (fences, lists and all).
    out.extend(
        markdown::render_markdown(a)
            .into_iter()
            .flat_map(|l| markdown::wrap(&l.line, width, l.indent)),
    );
    out.push(Line::raw(""));
    // The keys, dim, and only those that do something: with a single question
    // there is nothing to navigate, so the arrows are left out.
    let total = btw.len();
    let keys = if total > 1 {
        format!(
            "{} of {} · ←/→ switch · f fork with tools · x clear · esc close",
            index + 1,
            total
        )
    } else {
        "f fork with tools · x clear · esc close".to_string()
    };
    out.extend(markdown::wrap(&Line::styled(keys, dim), width, 0));
    out
}

/// The line the `/btw` panel shows in place of an answer: the failure, or a
/// spinner while the ask is still in flight.
fn ask_progress(p: &BtwAsk) -> String {
    if let Some(e) = &p.error {
        return format!("Side question failed: {e}");
    }
    let frame = BTW_SPINNER[(p.started.elapsed().as_millis() / 90) as usize % BTW_SPINNER.len()];
    format!("{frame} Asking the side question…")
}

/// What the `/mcp` panel offers for one server, as (label, command) pairs.
/// Each runs through `handle_env`, so it works mid-turn like the command.
fn mcp_actions(row: &crate::commands::McpRow) -> Vec<(&'static str, String)> {
    use crate::commands::McpState;
    let n = &row.name;
    match row.state {
        McpState::Connected => {
            let mut v = vec![
                ("Reconnect", format!("/mcp reconnect {n}")),
                ("Disable for this project", format!("/mcp disable {n}")),
            ];
            if row.http {
                v.push(("Clear authentication", format!("/mcp logout {n}")));
            }
            v
        }
        McpState::NeedsLogin => vec![
            ("Authenticate", format!("/mcp login {n}")),
            ("Reconnect", format!("/mcp reconnect {n}")),
            ("Disable for this project", format!("/mcp disable {n}")),
        ],
        McpState::Failed => vec![
            ("Reconnect", format!("/mcp reconnect {n}")),
            ("Disable for this project", format!("/mcp disable {n}")),
        ],
        McpState::Off if row.can_enable => {
            vec![("Enable for this project", format!("/mcp enable {n}"))]
        }
        McpState::Off => vec![],
        McpState::Pending => vec![
            ("Approve for this project", format!("/mcp enable {n}")),
            ("Reject", format!("/mcp disable {n}")),
        ],
        McpState::Rejected => vec![],
        McpState::Connecting => vec![("Disable for this project", format!("/mcp disable {n}"))],
    }
}

fn plain(text: &str) -> String {
    // Row details come from config diagnostics, which mark code with backticks
    // for the markdown `/mcp` output; the panel styles spans itself.
    text.replace('`', "")
}

/// The `/mcp` panel: every server with its status, the selected one's actions
/// when the menu is open, and the result of the last action. Built as rows so
/// the status colour and the selection read at a glance.
fn mcp_overlay_lines(
    env: &crate::agent::Env,
    sel: usize,
    menu: Option<usize>,
    note: Option<&str>,
    width: usize,
) -> Vec<Line<'static>> {
    use crate::commands::McpState;
    let dim = Style::default().fg(pal().dim);
    let rows = crate::commands::mcp_rows(env);
    let mut out: Vec<Line<'static>> = Vec::new();
    if rows.is_empty() {
        out.extend(markdown::wrap(
            &Line::styled(
                "No MCP servers configured. Add one with `chanto mcp add`, under [mcp_servers.<name>] in ~/.chanto/config.toml, or in .mcp.json.".to_string(),
                dim,
            ),
            width,
            0,
        ));
        out.push(Line::raw(""));
        out.extend(markdown::wrap(
            &Line::styled("esc close".to_string(), dim),
            width,
            0,
        ));
        return out;
    }
    let sel = sel.min(rows.len() - 1);
    for (i, row) in rows.iter().enumerate() {
        let color = match row.state {
            McpState::Connected => pal().accent,
            McpState::NeedsLogin | McpState::Pending => warn(),
            McpState::Failed | McpState::Rejected => pal().error,
            McpState::Off | McpState::Connecting => pal().dim,
        };
        let mut spans = vec![
            Span::styled(
                if i == sel { " ▸ " } else { "   " },
                Style::default().fg(pal().accent),
            ),
            Span::styled(
                format!("{} ", row.state.glyph()),
                Style::default().fg(color),
            ),
            Span::styled(
                row.name.clone(),
                if i == sel {
                    Style::default()
                        .fg(pal().accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().add_modifier(Modifier::BOLD)
                },
            ),
            Span::styled(
                format!("  {}", row.state.label()),
                Style::default().fg(color),
            ),
        ];
        if !row.detail.is_empty() {
            spans.push(Span::styled(format!(" · {}", plain(&row.detail)), dim));
        }
        out.extend(markdown::wrap(&Line::from(spans), width, 2));
    }
    match menu {
        Some(m) => {
            let actions = mcp_actions(&rows[sel]);
            out.push(Line::raw(""));
            if actions.is_empty() {
                let why = match rows[sel].state {
                    McpState::Rejected => {
                        "Rejected by settings (disabledMcpjsonServers), so there is nothing to do here."
                            .to_string()
                    }
                    _ => format!("Nothing to do here: {}.", plain(&rows[sel].detail)),
                };
                out.extend(markdown::wrap(&Line::styled(why, dim), width, 2));
            } else {
                let m = m.min(actions.len() - 1);
                for (i, (label, _)) in actions.iter().enumerate() {
                    let chosen = i == m;
                    out.extend(markdown::wrap(
                        &Line::from(vec![
                            Span::styled(
                                if chosen { " ▸ " } else { "   " }.to_string(),
                                Style::default().fg(pal().accent),
                            ),
                            Span::styled(
                                label.to_string(),
                                if chosen {
                                    Style::default().fg(pal().accent)
                                } else {
                                    dim
                                },
                            ),
                        ]),
                        width,
                        2,
                    ));
                }
            }
        }
        None => {
            if let Some(row) = rows.get(sel) {
                let actions = mcp_actions(row);
                let hint = if actions.is_empty() {
                    match row.state {
                        McpState::Rejected => {
                            "rejected by settings (disabledMcpjsonServers)".to_string()
                        }
                        _ => plain(&row.detail),
                    }
                } else {
                    format!(
                        "can: {}",
                        actions
                            .iter()
                            .map(|(label, _)| label.to_ascii_lowercase())
                            .collect::<Vec<_>>()
                            .join(" · ")
                    )
                };
                if !hint.is_empty() {
                    out.push(Line::raw(""));
                    out.extend(markdown::wrap(&Line::styled(hint, dim), width, 2));
                }
            }
        }
    }
    if let Some(n) = note.filter(|n| !n.trim().is_empty()) {
        out.push(Line::raw(""));
        for line in markdown::render_markdown(n.trim()) {
            out.extend(markdown::wrap(&line.line, width, line.indent));
        }
    }
    out.push(Line::raw(""));
    let keys = if menu.is_some() {
        "↑/↓ choose · enter run · esc back"
    } else {
        "↑/↓ select · enter actions · esc close"
    };
    out.extend(markdown::wrap(
        &Line::styled(keys.to_string(), dim),
        width,
        0,
    ));
    out
}

/// `/mcp` or `/mcp list`: the panel, not the markdown listing `/mcp <server>`
/// subcommands print.
fn is_mcp_panel(text: &str) -> bool {
    text.trim()
        .strip_prefix("/mcp")
        .is_some_and(|a| a.is_empty() || a.trim() == "list")
}

/// A token typed at the start of the prompt is a slash command when it is `/`
/// followed by word characters only — so a path like `/usr/bin` (a second slash)
/// is left as the plain text it is.
fn is_command_token(tok: &str) -> bool {
    tok.starts_with('/')
        && tok[1..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// One prompt row's spans: a leading `/command` on the first row is the only
/// text that means "run this" rather than "say this", so it takes the accent
/// colour and a bold weight; `@file` mentions take the link colour, as they do
/// in the transcript. Everything else is plain.
fn input_row_spans(row: &str, command_row: bool) -> Vec<Span<'static>> {
    let accent = Style::default()
        .fg(pal().accent)
        .add_modifier(Modifier::BOLD);
    let link = Style::default().fg(pal().link);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let bytes = row.as_bytes();
    let mut i = 0;
    let mut first = true;
    while i < row.len() {
        let ws = i;
        while i < row.len() && bytes[i] == b' ' {
            i += 1;
        }
        plain.push_str(&row[ws..i]);
        if i >= row.len() {
            break;
        }
        let start = i;
        while i < row.len() && bytes[i] != b' ' {
            i += 1;
        }
        let tok = &row[start..i];
        let style = if first && command_row && is_command_token(tok) {
            Some(accent)
        } else if tok.starts_with('@') && tok.len() > 1 {
            Some(link)
        } else {
            None
        };
        match style {
            Some(st) => {
                if !plain.is_empty() {
                    spans.push(Span::raw(std::mem::take(&mut plain)));
                }
                spans.push(Span::styled(tok.to_string(), st));
            }
            None => plain.push_str(tok),
        }
        first = false;
    }
    if !plain.is_empty() {
        spans.push(Span::raw(plain));
    }
    spans
}

/// A dialog's choices as screen rows: up to 9 options around the selected one,
/// each wrapped to `width` so a long option shows in full rather than being
/// cut off at the edge.
fn option_rows(options: &[String], sel: usize, width: usize) -> Vec<Line<'static>> {
    let start = sel.saturating_sub(4).min(options.len().saturating_sub(9));
    let mut rows = Vec::new();
    for (i, o) in options.iter().enumerate().skip(start).take(9) {
        let selected = i == sel;
        let st = if selected {
            Style::default()
                .fg(pal().accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let label = format!("{}. {o}", i + 1);
        let indent = label.find(". ").map(|p| p + 2).unwrap_or(0);
        for (k, l) in markdown::wrap(
            &Line::styled(label, st),
            width.saturating_sub(3).max(10),
            indent,
        )
        .into_iter()
        .enumerate()
        {
            let mut spans = vec![Span::styled(
                if selected && k == 0 { " ❯ " } else { "   " },
                st,
            )];
            spans.extend(l.spans);
            rows.push(Line::from(spans));
        }
    }
    rows
}

fn framed(color: Color, frame: FrameStyle) -> WBlock<'static> {
    let style = Style::default().fg(color);
    match frame {
        FrameStyle::Block => WBlock::default()
            .borders(Borders::ALL)
            .border_set(border::ONE_EIGHTH_WIDE)
            .border_style(style),
        FrameStyle::Line => WBlock::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(style),
        FrameStyle::Solid => WBlock::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(style.bg(color)),
    }
}

/// The `input_border` setting; anything unrecognised (or unset) is the gap-free default.
fn frame_from(spec: Option<&str>) -> FrameStyle {
    match spec {
        Some("line") => FrameStyle::Line,
        Some("solid") => FrameStyle::Solid,
        _ => FrameStyle::Block,
    }
}

struct TuiFrontend {
    tx: mpsc::UnboundedSender<AppMsg>,
}

#[async_trait]
impl Frontend for TuiFrontend {
    fn emit(&self, ev: Event) {
        let _ = self.tx.send(AppMsg::Agent(ev));
    }
    async fn permission(&self, req: PermissionRequest) -> PermissionReply {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(AppMsg::Permission(req, tx)).is_err() {
            return PermissionReply::Deny(None);
        }
        rx.await.unwrap_or(PermissionReply::Deny(None))
    }
    async fn question(&self, q: &str, options: &[String]) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(AppMsg::Question(q.to_string(), options.to_vec(), true, tx))
            .ok()?;
        rx.await.ok().flatten().map(|(_, s)| s)
    }
    async fn choose(&self, q: &str, options: &[String]) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(AppMsg::Question(q.to_string(), options.to_vec(), false, tx))
            .ok()?;
        rx.await.ok().flatten().map(|(_, s)| s)
    }
}

// ------------------------------------------------------------------ blocks

struct ToolBlock {
    id: String,
    label: String,
    display: Option<String>,
    content: Option<String>,
    is_error: bool,
    children: Vec<(String, String, Option<bool>)>,
    started: Instant,
    elapsed: Option<Duration>,
    progress: Option<String>,
}

enum Kind {
    Welcome {
        model: String,
        cwd: String,
    },
    User(String),
    Assistant(String),
    Thinking {
        text: String,
        started: Instant,
        elapsed: Option<Duration>,
    },
    Tool(ToolBlock),
    Note(String, Color),
    Markdown(String),
}

struct Block {
    kind: Kind,
    expanded: bool,
    version: u64,
    /// (width, version, expand_all, theme epoch, lines)
    cache: Option<(u16, u64, bool, u64, Vec<Line<'static>>)>,
}

impl Block {
    fn new(kind: Kind) -> Block {
        Block {
            kind,
            expanded: false,
            version: 0,
            cache: None,
        }
    }
    fn touch(&mut self) {
        self.version += 1;
    }
    fn is_running_tool(&self) -> bool {
        matches!(&self.kind, Kind::Tool(t) if t.elapsed.is_none())
    }
    fn expandable(&self) -> bool {
        matches!(self.kind, Kind::Tool(_) | Kind::Thinking { .. })
    }
}

// --------------------------------------------------------------------- app

enum Dialog {
    Permission {
        req: PermissionRequest,
        sel: usize,
        tx: Option<oneshot::Sender<PermissionReply>>,
        feedback: Option<Input>,
        scroll: u16,
        /// When set, the agent auto-denies this prompt at this instant (auto
        /// mode); the dialog shows a countdown so the wait is visible.
        deadline: Option<Instant>,
    },
    Question {
        q: String,
        options: Vec<String>,
        sel: usize,
        tx: Option<oneshot::Sender<Option<(usize, String)>>>,
        typing: Option<Input>,
        scroll: u16,
        free_text: bool,
        search: Option<PickerSearch>,
        /// What the answer does when no caller is waiting on it.
        action: Option<QuestionAction>,
    },
    /// `/btw` answers: one at a time, earlier ones a key away.
    Btw { index: usize, scroll: u16 },
    /// `/mcp`: every server and its status, with a per-server action menu.
    Mcp {
        sel: usize,
        scroll: u16,
        /// The action menu for `sel` when one is open.
        menu: Option<usize>,
        /// Result of the last action, kept in the panel rather than scrollback.
        note: Option<String>,
    },
}

/// What answering a locally-opened question does. A question with a caller
/// waiting on it sends the answer instead.
#[derive(Clone, Copy, PartialEq)]
enum QuestionAction {
    /// The first option quits despite background work.
    Quit,
}

/// The quit confirmation shown while background work is running.
fn quit_warning(tasks: usize) -> String {
    if tasks == 1 {
        "**1 background task is still running.** Quit anyway? It stops when chanto exits.".into()
    } else {
        format!(
            "**{tasks} background tasks are still running.** Quit anyway? They stop when chanto exits."
        )
    }
}

/// A `/btw` side question in flight: the panel opens with the question and a
/// progress line, then shows the answer (or the failure) in place of it.
struct BtwAsk {
    question: String,
    /// Set when the ask failed, so the panel explains that instead of spinning.
    error: Option<String>,
    started: Instant,
}

/// The `/btw` panel's spinner, advanced by how long the ask has been running.
const BTW_SPINNER: &[&str] = &["◰", "◳", "◲", "◱"];

struct Popup {
    items: Vec<(String, String)>,
    sel: usize,
    kind: PopupKind,
}

#[derive(PartialEq)]
enum PopupKind {
    Slash,
    File,
}

struct App {
    blocks: Vec<Block>,
    input: Input,
    scroll: usize,
    /// When scrolled up: the top visible row (block, row in block) and how many
    /// rows ran from it to the bottom, so output arriving below doesn't move the view.
    scroll_anchor: Option<(usize, usize, usize)>,
    busy: bool,
    busy_label: Option<String>,
    busy_since: Instant,
    /// When a permission prompt or question began waiting on the user. The
    /// busy timer pauses meanwhile: `busy_since` moves on by the wait.
    wait_started: Option<Instant>,
    stream_chars: usize,
    open_text: Option<usize>,
    /// Incremental render of the streaming reply: (block, width, bytes of
    /// finished lines rendered, source lines rendered, wrapped rows of the partial line).
    stream_state: Option<(usize, u16, usize, usize, usize)>,
    open_think: Option<usize>,
    dialog: Option<Dialog>,
    popup: Option<Popup>,
    todos: Vec<Todo>,
    show_todos: bool,
    /// Live goal/team status (`Event::Status`), by key: the first line goes in
    /// the footer, the rest in a panel above the prompt.
    statuses: Vec<(String, Vec<String>)>,
    expand_all: bool,
    mode: Mode,
    model: String,
    ctx_used: u64,
    ctx_max: u64,
    total_cost_usd: f64,
    price_unknown: bool,
    tps: f64,
    queue: VecDeque<(String, Vec<std::path::PathBuf>)>,
    cancel: Option<CancellationToken>,
    ctrl_c_at: Option<Instant>,
    /// Timestamp of the last `esc`, for the double-esc rewind gesture. Kept apart
    /// from [`ctrl_c_at`]: sharing one timer made a ctrl+c followed by esc (or
    /// vice versa) trigger the wrong action.
    esc_at: Option<Instant>,
    quit: bool,
    row_map: Vec<usize>,
    transcript_area: Rect,
    agent: Arc<AsyncMutex<Agent>>,
    env: Arc<crate::agent::Env>,
    tx: mpsc::UnboundedSender<AppMsg>,
    completions: Vec<(String, String)>,
    files: Option<Vec<String>>,
    /// Gitignored files for `@` completion, listed the first time nothing else fills the list.
    ignored_files: Option<Vec<String>>,
    hint: Option<(String, Instant)>,
    /// The `/btw` question being asked, shown in its panel until the answer lands.
    btw_ask: Option<BtwAsk>,
    /// Account limits from the last provider response (ChatGPT plans).
    limits: Option<crate::llm::limits::Snapshot>,
    /// Highest usage-limit warning shown per window (75 or 90), so each shows once.
    limit_warned: std::collections::HashMap<String, u8>,
    spinner_i: usize,
    branch: Option<String>,
    /// The current branch's pull request and its review state, from `gh` (refreshed in the background).
    pr_status: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    mouse: bool,
    pastes: Vec<String>,
    /// The input a too-large refusal is about: enter on it again saves the
    /// text to a file and sends the path.
    oversize_offer: Option<String>,
    /// Images pasted from the clipboard, in the order their `[Image #N]` tags
    /// appear in the prompt. Expanded back to `@path` for the model on submit.
    images: Vec<std::path::PathBuf>,
    pending_dialogs: VecDeque<Dialog>,
    status_text: Option<String>,
    search: Option<Search>,
    /// Transcript line (counted from the bottom) to highlight for a search hit.
    highlight: Option<usize>,
    /// Active mouse drag selection, if any.
    selection: Option<Selection>,
    /// Whether the current selection has actually been dragged (as opposed to a
    /// plain click), so returning to the anchor cell still copies.
    selection_moved: bool,
    /// The last left-press's time and cell, and how many rapid presses landed
    /// there, so a double-click can select the word and a triple-click the row.
    last_click: Option<(Instant, u16, u16)>,
    click_count: u8,
    /// The transcript block a plain click expanded or collapsed, so the next
    /// press of a rapid run can undo it before selecting a word instead.
    click_toggle: Option<usize>,
    /// The `?` shortcuts overlay is open.
    help: bool,
    /// Plain text of every row drawn in the last frame, by absolute screen row,
    /// with the column the row's text starts at. Rebuilt each frame so a copy
    /// reads whole rows and the selection highlight can be painted over every
    /// widget at once.
    row_text: Vec<Option<(u16, String)>>,
    /// Visible web-link cells in the last frame, for terminals without OSC 8.
    screen_links: Vec<(u16, u16, u16, String)>,
    /// A deliberate link click, opened by the event loop after mouse handling.
    open_link: Option<String>,
    /// Set by ctrl+g: the main loop suspends the TUI and opens $EDITOR.
    open_editor: bool,
    /// `e` on an edit's permission prompt: edit the proposed content in $EDITOR.
    edit_proposal: bool,
    /// `/vim`: modal editing in the prompt. `vim_insert` is true in insert mode;
    /// when false, keys are vim motions/operators rather than text.
    vim: bool,
    vim_insert: bool,
    /// A pending operator for a two-key vim command (`d` waiting for `d`/`w`).
    vim_pending: Option<char>,
    /// ctrl+x was pressed: the next key completes a chord.
    chord_x: bool,
    /// How the message being submitted is delivered while a turn runs.
    deliver: Deliver,
    /// "Send now" interrupted the turn: the front of the queue runs next, ahead
    /// of step messages the turn didn't get to.
    send_now: bool,
    /// Messages sent mid-turn (Enter) as typed, in order, until the turn ends:
    /// the ones still in the inbox can be pulled back to edit (Up).
    steered: Vec<(String, Vec<std::path::PathBuf>)>,
    /// A suggested next prompt, shown greyed in the empty prompt; tab takes it.
    suggestion: Option<String>,
    /// The selected row of the agents panel under the prompt.
    work_sel: Option<usize>,
    /// The subagent whose transcript the transcript pane shows (its Task call
    /// id), instead of the main conversation.
    viewing: Option<String>,
    /// Where the agents panel was drawn, and its first visible item (for clicks).
    work_area: Rect,
    /// Where the "↓ jump to latest" pill was drawn while scrolled up (empty
    /// otherwise), so a click on it can jump to the bottom.
    jump_area: Rect,
    /// Rows of output that arrived below the view since it was scrolled up.
    scroll_new: usize,
    work_start: usize,
    /// The viewed agent's rendered transcript: (key, version, width, expand_all, lines).
    agent_view: Option<(String, u64, u16, bool, Vec<Line<'static>>)>,
    /// Custom keybindings (keybindings.json); `replaying` while running one.
    keymap: keybindings::Keymap,
    replaying: bool,
    /// Undo stack for vim `u` (snapshots of the prompt text).
    vim_undo: Vec<String>,
    /// Last title written to the terminal tab (OSC 0), so we only rewrite it
    /// when the state it shows actually changes.
    term_title: String,
    /// A turn has finished since the last one started: drives the tab's ✓ mark.
    turned: bool,
    /// OSC 7501 status reports are on (`terminal_status`), and the last one sent.
    term_status: Option<Option<notify::Status>>,
    /// The last turn ended with an error (an `error` status rather than `done`).
    turn_failed: bool,
    voice: Voice,
    /// Inline mode (`--inline`): what has gone to the terminal's scrollback.
    inline: Option<InlineView>,
    /// Terminal height, for sizing dialogs in inline mode's short region.
    screen_rows: u16,
    /// Whether the terminal window has focus; `None` until the terminal
    /// reports it (not all do).
    focused: Option<bool>,
    /// How notifications reach the desktop (`notifications` in the config).
    notify: notify::Channel,
    /// A prompt waiting for the user: since when, and what to say, until a
    /// notification has gone out for it.
    attention: Option<(Instant, String)>,
    /// The running turn was started by the loop, so esc ends the loop too.
    loop_turn: bool,
    /// The next turn's prompt is Chanto's, not the user's (a scheduled task,
    /// background reports): see [`crate::agent::Agent::typed_input`].
    agent_input: bool,
    /// When the last turn finished, until the user types again: drives the
    /// `idle_prompt` Notification hook and the away recap.
    idle_since: Option<Instant>,
    /// The `idle_prompt` notification went out for this idle stretch.
    idle_notified: bool,
    /// Idle compaction already ran (or was checked) in this idle spell.
    idle_compacted: bool,
    /// Editing an earlier prompt (`/rewind` → edit): its checkpoint and what
    /// to restore; submitting branches the session there and sends the text.
    editing_prompt: Option<(usize, &'static str)>,
    /// Turns since the last away recap (recaps never come twice in a row).
    turns_since_recap: usize,
    /// An away recap being generated, shown when it's ready.
    recap: Option<tokio::sync::oneshot::Receiver<String>>,
    /// Quiet view (`--quiet`, `/quiet`): a turn's intermediate text and tool
    /// blocks stay out of the transcript (ctrl+o or `/transcript` shows them);
    /// the status line says what's running.
    quiet: bool,
    /// Top-level tool calls in the running turn, and the one running now.
    turn_tools: usize,
    current_tool: Option<String>,
}

/// The `idle_prompt` Notification hook fires this long after a turn ends.
const IDLE_PROMPT_AFTER: Duration = Duration::from_secs(60);
/// The away recap is prepared this long after the last turn.
const AWAY_RECAP_AFTER: Duration = Duration::from_secs(180);

/// How long a prompt waits unanswered before notifying, when the terminal
/// doesn't say whether it has focus.
const ATTENTION_DELAY: Duration = Duration::from_secs(6);

/// Inline mode: finished blocks are printed once into the terminal's own
/// scrollback (so native scrolling, search and copy work); only what is still
/// changing is drawn, in a region at the bottom of the screen.
#[derive(Default)]
struct InlineView {
    /// Blocks already printed.
    printed: usize,
    /// Bytes of the streaming answer (block `printed`) already printed.
    printed_src: usize,
    /// Print the most recent tool or thinking block again, expanded (ctrl+o).
    reprint: bool,
}

/// Incremental search (ctrl+r: prompt history, ctrl+f: transcript).
struct Search {
    transcript: bool,
    query: String,
    /// History: index of the current match. Transcript: nth match from the bottom.
    pos: usize,
    /// Input text before the search started (restored on esc).
    orig: String,
    found: bool,
}

pub(crate) fn git_branch(cwd: &std::path::Path) -> Option<String> {
    crate::hostgit::read(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
}

const VERBS: &[&str] = &[
    "Working",
    "Reading",
    "Searching",
    "Building",
    "Checking",
    "Tracing",
    "Reasoning",
    "Testing",
];
const SPIN: &[&str] = &["◰", "◳", "◲", "◱"];

/// The icon a terminal/window title shows for a session state: a diamond while
/// busy, ✓ after a turn, `❯` at rest. The busy glyph is steady, not the pane's
/// rotating frames: the OS lays a window title out with the UI font, not the
/// terminal's cell grid, so frames whose glyphs have different advances nudge a
/// centred title. The pane can rotate because it is drawn on the fixed grid.
fn title_icon(busy: bool, turned: bool) -> &'static str {
    if busy {
        "❖"
    } else if turned {
        "✓"
    } else {
        "❯"
    }
}

/// The terminal title: state, project, product. It has the same shape as the
/// desktop window's title, so several Chanto tabs are told apart at a glance.
/// A project named `chanto` isn't repeated.
fn window_title(icon: &str, project: Option<&str>) -> String {
    match project
        .map(str::trim)
        .filter(|p| !p.is_empty() && *p != "chanto")
    {
        Some(project) => format!("{icon} {project} — chanto"),
        None => format!("{icon} chanto"),
    }
}

/// The busy status line's frame. With reduced motion it is one steady glyph, so
/// nothing repaints under a screen reader (and there is no flicker); otherwise
/// it advances about every 80ms.
fn busy_frame(reduce_motion: bool, ms: u128) -> &'static str {
    if reduce_motion {
        "❖"
    } else {
        SPIN[(ms / 80) as usize % SPIN.len()]
    }
}

/// A highlight that sweeps across the busy verb once every 1.4s, so a slow call
/// reads as moving even before anything has streamed. Plain named colors
/// (themes parsed without RGB) and reduced motion keep the flat accent.
fn shimmer_spans(text: &str, base: Color, reduce_motion: bool, ms: u128) -> Vec<Span<'static>> {
    let flat = || vec![Span::styled(text.to_string(), Style::default().fg(base))];
    let Color::Rgb(r, g, b) = base else {
        return flat();
    };
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() || reduce_motion {
        return flat();
    }
    // The band travels a little past both ends so the sweep wraps cleanly.
    let span = chars.len() as f64 + 8.0;
    let head = (ms % SHIMMER_PERIOD_MS) as f64 / SHIMMER_PERIOD_MS as f64 * span - 4.0;
    let width = 4.0;
    chars
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let t = (1.0 - (i as f64 - head).abs() / width).max(0.0);
            let t = t * t * 0.8;
            let mix = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * t).round() as u8;
            Span::styled(
                c.to_string(),
                Style::default().fg(Color::Rgb(mix(r, 255), mix(g, 255), mix(b, 255))),
            )
        })
        .collect()
}

const SHIMMER_PERIOD_MS: u128 = 1400;

/// The one-line summary shown for a reasoning block.
fn thinking_label(elapsed: Option<std::time::Duration>) -> String {
    match elapsed {
        Some(d) => format!(
            "❖ thinking for {}s",
            (d.as_secs_f64().round() as u64).max(1)
        ),
        None => "❖ thinking…".to_string(),
    }
}

/// Progressive wording for a long reasoning stream, so a slow model reads as
/// still working instead of frozen.
fn thinking_verb(secs: u64) -> &'static str {
    if secs >= 45 {
        "deep in thought"
    } else if secs >= 30 {
        "thinking some more"
    } else if secs >= 20 {
        "thinking more"
    } else if secs >= 10 {
        "still thinking"
    } else {
        "thinking"
    }
}

fn history_path() -> std::path::PathBuf {
    crate::config::home_dir().join("history.jsonl")
}

fn load_history() -> Vec<String> {
    std::fs::read_to_string(history_path())
        .map(|s| {
            s.lines()
                .filter_map(|l| serde_json::from_str::<String>(l).ok())
                .collect::<Vec<_>>()
        })
        .map(|mut v| {
            let n = v.len();
            if n > 500 {
                v.drain(..n - 500);
            }
            v
        })
        .unwrap_or_default()
}

fn append_history(s: &str) {
    use std::io::Write;
    let p = history_path();
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    if let Ok(mut f) = crate::util::append_private(&p) {
        let _ = writeln!(f, "{}", serde_json::to_string(s).unwrap_or_default());
    }
}

/// Rows of the parts below the transcript (see `App::heights`).
struct Heights {
    panel: u16,
    todo: u16,
    status: u16,
    bottom: u16,
    /// The agents panel (subagents and shells), under the prompt.
    work: u16,
}

impl Heights {
    /// With the status row's spacer and the footer.
    fn total(&self) -> u16 {
        self.panel + self.todo + self.status + 1 + self.bottom + self.work + 1
    }
}

/// One row of the work panel, at most `width` cells wide, so a long label or id
/// is truncated rather than wrapping onto the row below.
fn work_row(
    viewing: Option<&str>,
    item: &agents_panel::Item,
    width: usize,
    selected: bool,
) -> Line<'static> {
    let dim = Style::default().fg(pal().dim);
    let st = if selected {
        Style::default()
            .fg(pal().accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let cursor = Span::styled(if selected { "❯" } else { " " }, st);
    let line = match item {
        agents_panel::Item::Main => {
            let viewed = viewing.is_none();
            Line::from(vec![
                cursor,
                Span::styled(if viewed { "● " } else { "○ " }, st),
                Span::styled(
                    "main",
                    if viewed {
                        st.add_modifier(Modifier::BOLD)
                    } else {
                        st
                    },
                ),
            ])
        }
        agents_panel::Item::Agent(r) => {
            let viewed = viewing == Some(r.key.as_str());
            let (left, right) = agents_panel::agent_row(
                &r.kind,
                &r.activity,
                r.secs,
                r.tokens,
                &r.status,
                width.saturating_sub(3),
            );
            let (kind, rest) = left.split_at(left.find("  ").unwrap_or(left.len()));
            let mark = if viewed { "● " } else { "○ " };
            let failed = r.status == crate::agent::live::RowStatus::Failed;
            Line::from(vec![
                cursor,
                Span::styled(
                    mark,
                    if failed {
                        Style::default().fg(pal().error)
                    } else {
                        st
                    },
                ),
                Span::styled(
                    kind.to_string(),
                    if viewed {
                        st.add_modifier(Modifier::BOLD)
                    } else {
                        st
                    },
                ),
                Span::styled(rest.to_string(), if selected { st } else { dim }),
                Span::styled(format!("  {right}"), dim),
            ])
        }
        agents_panel::Item::Background { id, label, secs } => {
            // The id rides with the timing on the right, so both have to be
            // reserved before the label is truncated: otherwise the row runs
            // past the width and wraps over the next one.
            let right = crate::util::one_line(
                &format!("{} · {id}", crate::util::duration(*secs)),
                width.saturating_sub(6).max(1),
            );
            let room = width.saturating_sub(5 + right.chars().count());
            let label = if room == 0 {
                String::new()
            } else {
                crate::util::one_line(label, room)
            };
            let pad = width.saturating_sub(5 + label.chars().count() + right.chars().count());
            Line::from(vec![
                cursor,
                Span::styled("○ ", st),
                Span::styled(format!("{label}{}", " ".repeat(pad)), st),
                Span::styled(format!("  {right}"), dim),
            ])
        }
        agents_panel::Item::Shell { id, label, monitor } => {
            // A watch reads as a different kind of background work from a
            // one-shot command, and matches the ◉ used in its notifications.
            // The id stays verbatim: it's what BashOutput/KillShell take.
            let marker = if *monitor { "◉ " } else { "⚙ " };
            // `· id` is 3 columns plus the id; truncate both so the row fits.
            let id = crate::util::one_line(id, width.saturating_sub(8).max(1));
            let room = width.saturating_sub(id.chars().count() + 6);
            let label = if room == 0 {
                String::new()
            } else {
                crate::util::one_line(label, room)
            };
            Line::from(vec![
                cursor,
                Span::styled(marker, st),
                Span::styled(label, st),
                Span::styled(format!(" · {id}"), dim),
            ])
        }
    };
    clamp_row(line, width)
}

/// Cut a panel row to `width` cells (an ellipsis stands in for the last one), so
/// a long label or id can never wrap the row onto the one below it.
fn clamp_row(line: Line<'static>, width: usize) -> Line<'static> {
    if line.width() <= width {
        return line;
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in line.spans {
        let room = width.saturating_sub(used);
        if room == 0 {
            break;
        }
        let n = span.content.chars().count();
        if n <= room {
            used += n;
            spans.push(span);
        } else {
            let mut s: String = span.content.chars().take(room.saturating_sub(1)).collect();
            s.push('…');
            spans.push(Span::styled(s, span.style));
            break;
        }
    }
    Line::from(spans)
}

impl App {
    /// Run the user's status_line command in the background.
    fn refresh_status_line(&self) {
        let Some(cmd) = self.env.config.status_line.clone() else {
            return;
        };
        let tx = self.tx.clone();
        let payload = serde_json::json!({
            "cwd": self.env.cwd(), "model": self.env.model(), "mode": self.env.mode().label(),
            "context_used": self.ctx_used, "context_max": self.ctx_max,
        })
        .to_string();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let mut c = crate::shell::command(&cmd);
            c.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null());
            let Ok(mut child) = c.spawn() else { return };
            if let Some(mut si) = child.stdin.take() {
                let _ = si.write_all(payload.as_bytes()).await;
            }
            if let Ok(Ok(o)) =
                tokio::time::timeout(Duration::from_secs(5), child.wait_with_output()).await
            {
                let line = String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                let _ = tx.send(AppMsg::StatusLine(line));
            }
        });
    }

    /// Show a dialog now, or queue it behind the one currently open.
    fn open_dialog(&mut self, d: Dialog) {
        if self.dialog.is_none() {
            self.dialog = Some(d);
        } else {
            self.pending_dialogs.push_back(d);
        }
    }

    /// Background work that quitting would stop: running background shells
    /// (monitors included) and background subagents.
    fn background_tasks(&self) -> usize {
        let shells = self
            .env
            .shells
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.exit.lock().unwrap().is_none())
            .count();
        shells + crate::agent::background_running(&self.env)
    }

    /// Quit, asking first when background work is still running.
    fn request_quit(&mut self) {
        let tasks = self.background_tasks();
        if tasks == 0 {
            self.quit = true;
            return;
        }
        self.open_dialog(Dialog::Question {
            q: quit_warning(tasks),
            options: vec!["Quit anyway".into(), "Keep working".into()],
            sel: 0,
            tx: None,
            typing: None,
            scroll: 0,
            free_text: false,
            search: None,
            action: Some(QuestionAction::Quit),
        });
    }

    /// Show the `/btw` panel at `index`, retargeting one that is already open
    /// rather than stacking a second copy behind it.
    fn show_btw(&mut self, index: usize) {
        if let Some(Dialog::Btw { index: i, scroll }) = &mut self.dialog {
            *i = index;
            *scroll = 0;
        } else {
            self.open_dialog(Dialog::Btw { index, scroll: 0 });
        }
    }

    /// Show the `/mcp` panel: every server, selected on the first one that
    /// needs attention.
    fn open_mcp(&mut self) {
        if let Some(Dialog::Mcp { scroll, menu, .. }) = &mut self.dialog {
            *scroll = 0;
            *menu = None;
            return;
        }
        let rows = crate::commands::mcp_rows(&self.env);
        // Land on a server that needs a decision, not one turned off on purpose.
        let needs_you = |r: &crate::commands::McpRow| {
            matches!(
                r.state,
                crate::commands::McpState::NeedsLogin
                    | crate::commands::McpState::Failed
                    | crate::commands::McpState::Pending
                    | crate::commands::McpState::Rejected
            )
        };
        let sel = rows.iter().position(needs_you).unwrap_or(0);
        self.open_dialog(Dialog::Mcp {
            sel,
            scroll: 0,
            menu: None,
            note: None,
        });
    }

    fn push(&mut self, k: Kind) -> usize {
        if let Kind::User(t) = &k {
            crate::remote::user(t);
        }
        self.blocks.push(Block::new(k));
        self.blocks.len() - 1
    }

    fn note(&mut self, s: impl Into<String>, c: Color) {
        self.close_stream();
        self.push(Kind::Note(s.into(), c));
    }

    /// A permission prompt or question now waits for the user.
    fn want_attention(&mut self, what: String) {
        if self.attention.is_none() {
            self.attention = Some((Instant::now(), what));
        }
        self.check_attention();
    }

    /// Notify about a waiting prompt once it's due when it
    /// needs you: at once if the window is in the background, after a few
    /// seconds unanswered if the terminal doesn't report focus, and never while
    /// it has focus (switching away later still notifies).
    fn check_attention(&mut self) {
        if self.dialog.is_none() && self.pending_dialogs.is_empty() {
            self.attention = None;
            return;
        }
        let due = match (&self.attention, self.focused) {
            (None, _) | (_, Some(true)) => false,
            (Some(_), Some(false)) => true,
            (Some((since, _)), None) => since.elapsed() >= ATTENTION_DELAY,
        };
        if due && let Some((_, what)) = self.attention.take() {
            notify::send(self.notify, "chanto", &what);
        }
    }

    /// While waiting for the user after a turn: the `idle_prompt`
    /// Notification hook after 60s (only when you seem to
    /// be away, nothing runs in the background, and you haven't typed), and
    /// the away recap after 3 minutes with the terminal unfocused.
    fn idle_tick(&mut self) {
        if let Some(rx) = self.recap.as_mut() {
            match rx.try_recv() {
                Ok(r) => {
                    self.recap = None;
                    self.note(format!("※ recap: {r}"), pal().dim);
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => self.recap = None,
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            }
        }
        let Some(since) = self.idle_since else { return };
        if self.busy {
            return;
        }
        let quiet = self.dialog.is_none()
            && crate::agent::background_running(&self.env) == 0
            && crate::tools::monitor::active(&self.env) == 0;
        // Idle compaction: summarize a long conversation in the background so
        // the next prompt doesn't wait for it (or for a cold prompt cache).
        if !self.idle_compacted
            && quiet
            && self.env.config.idle_compaction == Some(true)
            && let Ok(a) = self.agent.try_lock()
            && a.idle_compact_due(since.elapsed())
        {
            self.idle_compacted = true;
            drop(a);
            let (agent, tx) = (self.agent.clone(), self.tx.clone());
            tokio::spawn(async move {
                let mut a = agent.lock().await;
                let done = a.compact(None, "auto", &CancellationToken::new()).await;
                a.save_session();
                if let Err(e) = done {
                    let _ = tx.send(AppMsg::Show(format!("Idle compaction failed: {e:#}")));
                }
            });
        }
        if !self.idle_notified
            && since.elapsed() >= IDLE_PROMPT_AFTER
            && self.focused != Some(true)
            && quiet
        {
            self.idle_notified = true;
            let env = self.env.clone();
            tokio::spawn(async move {
                let msg = "Chanto is waiting for your input";
                crate::hooks::notify(
                    &env,
                    "Notification",
                    Some("idle_prompt"),
                    serde_json::json!({"message": msg, "notification_type": "idle_prompt"}),
                )
                .await;
            });
        }
        if self.recap.is_none()
            && self.turns_since_recap > 0
            && self.focused == Some(false)
            && since.elapsed() >= AWAY_RECAP_AFTER
            && crate::session_cmds::away_summary_enabled(&self.env)
        {
            let Ok(a) = self.agent.try_lock() else { return };
            if crate::session_cmds::user_turns(&a.messages) < 3 {
                return;
            }
            let messages = a.messages.clone();
            drop(a);
            self.turns_since_recap = 0;
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.recap = Some(rx);
            let env = self.env.clone();
            tokio::spawn(async move {
                if let Ok(r) = crate::session_cmds::recap(&env, &messages).await {
                    let _ = tx.send(r);
                }
            });
        }
    }

    /// `/loop …`, handled here because the TUI owns the clock.
    fn loop_command(&mut self, arg: &str) {
        match commands::parse_loop(arg) {
            Err(e) => self.note(e, warn()),
            Ok(commands::LoopCmd::Stop) => {
                if !self.env.scheduler.loop_active() {
                    self.note("No loop is running.", pal().dim);
                }
                self.stop_loop();
            }
            Ok(commands::LoopCmd::Status) => match self.env.scheduler.loop_status() {
                Some(s) => self.note(format!("{s} · `/loop stop` or esc ends it"), pal().dim),
                None => self.note(
                    format!("No loop is running. {}", commands::LOOP_USAGE),
                    pal().dim,
                ),
            },
            Ok(commands::LoopCmd::Start { every, prompt }) => {
                // Spend guard: a loop that could cost more than the confirm
                // threshold in a day starts only once you say so.
                if let Some((note, true)) = crate::spend::loop_note(&self.env, every) {
                    let env = self.env.clone();
                    tokio::spawn(async move {
                        let options = ["Start the loop".to_string(), "Cancel".to_string()];
                        let q = format!("{note} That's over your spend confirm threshold.");
                        let start = env.frontend.choose(&q, &options).await.as_deref()
                            == Some("Start the loop");
                        env.emit(crate::agent::Event::Info(if start {
                            commands::start_loop(&env, every, prompt)
                        } else {
                            "Loop not started.".into()
                        }));
                    });
                    return;
                }
                let mut note = commands::start_loop(&self.env, every, prompt);
                if self.busy {
                    note.push_str(" · first run when the current turn ends");
                }
                self.note(note, pal().dim);
                self.loop_tick();
            }
        }
    }

    fn stop_loop(&mut self) {
        self.loop_turn = false;
        if self.env.scheduler.stop_loop() {
            self.note("Loop stopped.", pal().dim);
        }
    }

    /// Between turns: hand background reports (Monitor lines, finished
    /// agents) to the model, else run a `/loop` iteration or a scheduled task
    /// that's due. A task that came due during a turn (or an open dialog)
    /// runs once afterwards; missed runs aren't caught up.
    fn loop_tick(&mut self) {
        if self.busy || self.dialog.is_some() || !self.pending_dialogs.is_empty() {
            return;
        }
        let events: Vec<String> = self
            .env
            .notifications
            .lock()
            .unwrap()
            .iter()
            .filter_map(|n| {
                n.strip_prefix("<system-reminder>Monitor ").map(|r| {
                    r.split("</system-reminder>")
                        .next()
                        .unwrap_or("")
                        .trim_end_matches(':')
                        .to_string()
                })
            })
            .collect();
        if !self.env.notifications.lock().unwrap().is_empty() {
            for e in events {
                self.note(format!("◉ Monitor {e}"), pal().dim);
            }
            self.deliver_notifications();
            return;
        }
        let Some(fire) = self
            .env
            .scheduler
            .take_due(chrono::Local::now(), &self.env.cwd())
        else {
            return;
        };
        let shown = fire
            .prompt
            .split("\n\n<system-reminder>")
            .next()
            .unwrap_or("")
            .to_string();
        let label = if fire.note.is_empty() {
            "↻ Loop iteration".to_string()
        } else {
            fire.note.clone()
        };
        self.note(
            format!("{label}: {}", crate::util::one_line(&shown, 100)),
            pal().dim,
        );
        self.loop_turn = fire.source != crate::cron::Source::Cron;
        self.start_hidden(fire.prompt, fire.literal);
    }

    // ------------------------------------------------------------ actions

    /// Run an environment-only command (`/model`, `/think`, ...) without waiting
    /// for the running turn to release the agent.
    fn run_env_command(&mut self, line: String) {
        let env = self.env.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match crate::commands::handle_env(&env, &line).await {
                Some(crate::commands::Outcome::Show(s)) => {
                    let _ = tx.send(AppMsg::Show(s));
                }
                Some(crate::commands::Outcome::Exit) => {
                    let _ = tx.send(AppMsg::Exit);
                }
                _ => {}
            }
        });
    }

    /// Run a `/mcp` panel action in the background and leave the panel open;
    /// the result lands in its note line.
    fn run_mcp_action(&mut self, line: String) {
        let (env, tx) = (self.env.clone(), self.tx.clone());
        tokio::spawn(async move {
            let out = crate::commands::handle_env(&env, &line).await;
            let text = match out {
                Some(crate::commands::Outcome::Show(s)) => s,
                _ => format!("Ran `{line}`."),
            };
            let _ = tx.send(AppMsg::McpNote(text));
        });
    }

    /// ctrl+space / `/voice`: start recording, or stop and transcribe.
    fn toggle_voice(&mut self) {
        match std::mem::replace(&mut self.voice, Voice::Off) {
            Voice::Off => {
                let backend = match crate::voice::backend(&self.env.config) {
                    Ok(b) => b,
                    Err(e) => {
                        self.note(format!("Voice input: {e}"), warn());
                        return;
                    }
                };
                self.voice = Voice::Starting(backend);
                let (cfg, tx) = (self.env.config.voice.clone(), self.tx.clone());
                tokio::spawn(async move {
                    let r = crate::voice::Recording::start(&cfg)
                        .await
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(AppMsg::VoiceStarted(r));
                });
            }
            Voice::Recording(rec, backend) => {
                self.voice = Voice::Transcribing(Instant::now());
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let r = async {
                        let audio = rec.stop().await?;
                        crate::voice::transcribe(&backend, audio.path()).await
                        // `audio` (the temp file) is deleted here.
                    }
                    .await;
                    let _ = tx.send(AppMsg::Voice(r.map_err(|e| format!("{e:#}"))));
                });
            }
            other => self.voice = other,
        }
    }

    /// ctrl+o with the cursor on (or just after) a `[Pasted text #n +N lines]`
    /// tag: put the pasted text there instead, to edit it. False if there's no tag.
    fn expand_paste_at_cursor(&mut self) -> bool {
        let Some((start, end, n)) = paste_tag_at(&self.input.text, self.input.cursor) else {
            return false;
        };
        let Some(p) = self.pastes.get(n - 1).cloned() else {
            return false;
        };
        self.input.text.replace_range(start..end, &p);
        self.input.cursor = start + p.len();
        true
    }

    /// The tokens and context window if `text` can't fit this model, when
    /// that can be told now (a running turn holds the agent; it checks again).
    fn oversized(&self, text: &str) -> Option<(u64, u64)> {
        self.agent.try_lock().ok()?.oversized_tokens(text)
    }

    /// Enter on a message that can't fit: keep it in the box and say what can
    /// be done, rather than clearing a paste that's refused a moment later.
    /// Enter again on the same text saves it to a file and sends the path. True
    /// when the message stays in the box.
    fn hold_oversized(&mut self) -> bool {
        let raw = self.input.text.clone();
        let trimmed = raw.trim_start();
        let full = expand_pastes(&raw, &self.pastes);
        let too_big = if trimmed.is_empty() || trimmed.starts_with(['/', '!']) {
            None
        } else {
            self.oversized(&full)
        };
        let Some((tokens, max)) = too_big else {
            self.oversize_offer = None;
            return false;
        };
        if self.oversize_offer.take().as_deref() != Some(raw.as_str()) {
            self.note(
                format!(
                    "Too large to send: ~{tokens} tokens, and this model's context is {max}. It's still in the box. \
                     Press enter again to save it to a file for the model to read from, or edit it, or switch with /model."
                ),
                pal().warning,
            );
            self.oversize_offer = Some(raw);
            return true;
        }
        match self.message_with_saved_pastes(&raw, &full) {
            Ok(text) => {
                self.input.set(&text);
                self.pastes.clear();
                false
            }
            Err(e) => {
                self.note(format!("Couldn't save it to a file: {e}"), pal().error);
                true
            }
        }
    }

    /// `raw` with each paste saved to a file and replaced by its path, keeping
    /// what was typed around them; the whole message in a file if that's still
    /// too large, or if it was typed rather than pasted.
    fn message_with_saved_pastes(&self, raw: &str, full: &str) -> std::io::Result<String> {
        let mut text = raw.to_string();
        for (i, paste) in self.pastes.iter().enumerate() {
            if let Some(range) = paste_tag_range(&text, i)
                && self.oversized(paste).is_some()
            {
                let path = attach::save_oversized_text(paste)?;
                text.replace_range(
                    range,
                    &format!(
                        "[Pasted text saved to {}; read what you need from it]",
                        path.display()
                    ),
                );
            }
        }
        if self
            .oversized(&expand_pastes(&text, &self.pastes))
            .is_none()
            && text != raw
        {
            return Ok(expand_pastes(&text, &self.pastes));
        }
        let path = attach::save_oversized_text(full)?;
        Ok(format!(
            "My message was too large to send, so it's saved to {}. Read what you need from it.",
            path.display()
        ))
    }

    fn submit(&mut self) {
        if let Some(p) = &self.popup
            && p.kind == PopupKind::Slash
        {
            let (cmd, _) = p.items[p.sel].clone();
            let (_, word) = self.input.current_word();
            if word != cmd {
                self.input.set(&cmd);
            }
        }
        self.popup = None;
        self.suggestion = None;
        if self.hold_oversized() {
            return;
        }
        let deliver = std::mem::take(&mut self.deliver);
        let mut text = self.input.take();
        if text.trim().is_empty() {
            return;
        }
        // :tada: → 🎉 in what was typed (not in pasted text, commands or `!shell`).
        if !text.trim_start().starts_with(['/', '!']) {
            text = crate::util::emoji_shortcodes(&text);
        }
        text = expand_pastes(&text, &self.pastes);
        self.pastes.clear();
        // An edited earlier prompt: branch there (the transcript reloads), then send it.
        if let Some((idx, mode)) = self.editing_prompt.take()
            && !text.trim_start().starts_with(['/', '!'])
        {
            append_history(&text);
            let _ = self
                .tx
                .send(AppMsg::Submit(format!("/edit-prompt {idx} {mode}")));
            let _ = self.tx.send(AppMsg::Submit(text));
            return;
        }
        // Viewing a subagent: what's typed goes to it, not the main conversation
        // (commands and `!shell` still run here).
        if let Some(key) = self
            .viewing
            .clone()
            .filter(|_| !text.trim_start().starts_with(['/', '!']))
            && self.send_to_agent(&key, &text)
        {
            return;
        }
        if text.trim() == "/voice" {
            self.toggle_voice();
            return;
        }
        append_history(&text);
        self.scroll = 0;
        if let Some(arg) = text
            .trim()
            .strip_prefix("/loop")
            .filter(|a| a.is_empty() || a.starts_with(char::is_whitespace))
        {
            self.loop_command(arg);
            return;
        }
        // `&task` starts a background run, like `/bg task`.
        if let Some(task) = text
            .trim_start()
            .strip_prefix('&')
            .filter(|t| !t.trim().is_empty())
        {
            text = format!("/bg {}", task.trim());
        }
        {
            let t = text.trim();
            // `/btw` answers in its overlay, idle or mid-turn, without waiting for the agent.
            if t == "/btw" || t.starts_with("/btw ") {
                let t = t.to_string();
                self.images.clear();
                let arg = t.trim_start_matches("/btw").trim();
                if arg.is_empty() && !self.env.btw.lock().unwrap().is_empty() {
                    let n = self.env.btw.lock().unwrap().len();
                    self.show_btw(n - 1);
                } else if arg.is_empty() || arg == "fork" || arg.starts_with("fork ") {
                    self.run_env_command(t);
                } else {
                    if !self.busy {
                        // Idle, the agent's conversation is complete: let /btw see all of it.
                        if let Ok(a) = self.agent.try_lock() {
                            a.publish_conversation();
                        }
                    }
                    // Open the panel with the question straight away, so the
                    // ask has a place to land instead of a line of scrollback.
                    let question = arg.to_string();
                    self.btw_ask = Some(BtwAsk {
                        question,
                        error: None,
                        started: Instant::now(),
                    });
                    let n = self.env.btw.lock().unwrap().len();
                    self.show_btw(n);
                    let (env, tx) = (self.env.clone(), self.tx.clone());
                    tokio::spawn(async move {
                        let before = env.btw.lock().unwrap().len();
                        let out = crate::commands::handle_env(&env, &t).await;
                        if env.btw.lock().unwrap().len() > before {
                            let _ = tx.send(AppMsg::OpenBtw);
                        } else if let Some(crate::commands::Outcome::Show(s)) = out {
                            let _ = tx.send(AppMsg::BtwFailed(s));
                        }
                    });
                }
                return;
            }
            // `/mcp` opens the server panel; subcommands still run immediately.
            if is_mcp_panel(t) {
                self.open_mcp();
                return;
            }
            // `/transcript` shows the steps the quiet view hides, as ctrl+o does.
            if t == "/transcript" {
                self.toggle_transcript();
                return;
            }
            // `/quiet`: the view switches here; commands.rs switches the model side.
            if t == "/quiet" || t.starts_with("/quiet ") {
                let on = match t.trim_start_matches("/quiet").trim() {
                    "on" => Some(true),
                    "off" => Some(false),
                    "" => Some(!self.quiet),
                    _ => None,
                };
                if let Some(on) = on {
                    self.quiet = on;
                    // Spell out the state, so the model side can't end up the other way round.
                    text = format!("/quiet {}", if on { "on" } else { "off" });
                }
            }
            let t = text.trim();
            // `/vim` is prompt-editor state, so it never reaches commands.rs.
            if t == "/vim" || t.starts_with("/vim ") {
                let on = match t.trim_start_matches("/vim").trim() {
                    "on" => true,
                    "off" => false,
                    _ => !self.vim,
                };
                self.vim = on;
                self.vim_insert = true;
                self.vim_pending = None;
                self.vim_undo.clear();
                self.note(
                    if on {
                        "Vim mode on — Esc for normal mode, i to insert."
                    } else {
                        "Vim mode off."
                    },
                    pal().accent,
                );
                return;
            }
            // Interactive settings editors, idle or mid-turn: a bare command
            // opens a chooser; the chosen row runs the equivalent command.
            if matches!(t, "/config" | "/permissions" | "/hooks" | "/agents") {
                let t = t.to_string();
                self.images.clear();
                if self.open_settings_picker(&t) {
                    return;
                }
            }
        }
        if self.busy {
            let t = text.trim().to_string();
            // Bare `/model` opens the picker; the generic env-command path below
            // would only print the current model, which is no use mid-turn — and
            // swapping the model while a turn is stuck is exactly when you want it.
            if t == "/model" {
                self.images.clear();
                self.open_picker(&t);
                return;
            }
            // Settings commands run now; anything that rewrites the conversation
            // waits for the turn to finish, since it needs the agent.
            if t.starts_with('/') && crate::commands::is_immediate(&t) {
                self.images.clear();
                self.run_env_command(t);
            } else if deliver == Deliver::Now {
                let images = std::mem::take(&mut self.images);
                self.queue.push_front((t, images));
                self.send_queued_now();
            } else if deliver == Deliver::Queue
                || t.starts_with('/')
                || t.starts_with('!')
                || t.starts_with('#')
            {
                let images = std::mem::take(&mut self.images);
                self.queue.push_back((t, images));
            } else {
                // Delivered to the model at the next tool boundary. Inbox messages
                // aren't tag-expanded, so attach the image paths here.
                self.close_stream();
                self.push(Kind::User(t.clone()));
                let with_paths = expand_image_tags(&t, &self.images);
                self.steered
                    .push((t.clone(), std::mem::take(&mut self.images)));
                self.env.inbox.lock().unwrap().push(Queued::new(with_paths));
            }
            return;
        }
        self.start(text);
    }

    /// Ask the model, on the side, what the user is likely to type next.
    fn suggest_next_prompt(&mut self) {
        if let Ok(a) = self.agent.try_lock() {
            a.publish_conversation();
        }
        let (env, tx) = (self.env.clone(), self.tx.clone());
        tokio::spawn(async move {
            let conversation = env.conversation.read().unwrap().clone();
            if conversation.len() < 2 {
                return;
            }
            let ask = "Suggest the message the user will most likely send you next, as they would type it: one short line, \
                       under 12 words, no quotes or preamble. If nothing obvious follows, reply with just: none";
            if let Ok(s) =
                crate::agent::Agent::side_question_on(&env, &env.model(), &conversation, ask).await
            {
                let s = s.trim().trim_matches('"').trim();
                if !s.is_empty()
                    && !s.eq_ignore_ascii_case("none")
                    && s.len() < 200
                    && !s.contains('\n')
                {
                    let _ = tx.send(AppMsg::Suggestion(s.to_string()));
                }
            }
        });
    }

    /// Run a keybinding's action.
    fn run_key_effect(&mut self, effect: keybindings::Effect) {
        use keybindings::Effect;
        match effect {
            Effect::Keys(events) => {
                self.replaying = true;
                for e in events {
                    self.on_key(e);
                }
                self.replaying = false;
            }
            Effect::ModelPicker => {
                self.open_picker("/model");
            }
            Effect::KillAgents => {
                let mut n = 0;
                for b in self
                    .env
                    .bg_agents
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|b| b.status == "running")
                {
                    b.cancel.cancel();
                    n += 1;
                }
                self.hint = Some((format!("Stopped {n} background agent(s)"), Instant::now()));
            }
            Effect::ToggleThinking => {
                let on = !self.env.think();
                self.env.set_think(on);
                self.hint = Some((
                    if on { "Thinking on" } else { "Thinking off" }.into(),
                    Instant::now(),
                ));
            }
            Effect::CopySelection => self.copy_selection(),
            Effect::Exit => {
                let _ = self.tx.send(AppMsg::Exit);
            }
            Effect::Nothing => {}
        }
    }

    /// Move messages that haven't reached the model yet back into the prompt,
    /// joined in the order they were sent. Returns whether there were any.
    fn pull_back_waiting(&mut self) -> bool {
        let unread = std::mem::take(&mut *self.env.inbox.lock().unwrap()).len();
        let from_inbox: Vec<(String, Vec<std::path::PathBuf>)> = self
            .steered
            .split_off(self.steered.len().saturating_sub(unread));
        // Their transcript entries go too: they'll show again when re-sent.
        for (text, _) in from_inbox.iter().rev() {
            if let Some(i) = self
                .blocks
                .iter()
                .rposition(|b| matches!(&b.kind, Kind::User(t) if t == text))
            {
                self.blocks.remove(i);
            }
        }
        let queued: Vec<(String, Vec<std::path::PathBuf>)> = self.queue.drain(..).collect();
        let all: Vec<(String, Vec<std::path::PathBuf>)> =
            from_inbox.into_iter().chain(queued).collect();
        if all.is_empty() {
            return false;
        }
        let text = all
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for (_, imgs) in all {
            self.images.extend(imgs);
        }
        self.input.set(&text);
        self.popup = None;
        true
    }

    /// Start a turn to hand finished background agents' reports to the model.
    fn deliver_notifications(&mut self) {
        let pending: Vec<String> = self.env.notifications.lock().unwrap().drain(..).collect();
        if !pending.is_empty() {
            self.start_hidden(pending.join("\n\n"), true);
        }
    }

    /// Start a turn whose user block is already shown. `literal`: the text
    /// goes to the model as it is, never run as a `/command`, `!shell` or
    /// `#note` (for text the user didn't type: reports, the model's
    /// scheduled prompts).
    fn start_hidden(&mut self, text: String, literal: bool) {
        let n = self.blocks.len();
        self.agent_input = true;
        crate::remote::quietly(|| self.start_as(text, literal));
        self.agent_input = false;
        if let Some(Kind::User(_)) = self.blocks.get(n).map(|b| &b.kind) {
            self.blocks.remove(n);
        }
    }

    fn start(&mut self, text: String) {
        self.start_as(text, false)
    }

    fn start_as(&mut self, text: String, literal: bool) {
        let trimmed = text.trim().to_string();
        if !literal && self.open_picker(&trimmed) {
            return;
        }
        // `[Image #N]` tags are expanded to `@path` for the model once the prompt
        // is final, but the transcript keeps the readable tag.
        let image_paths = if literal {
            Vec::new()
        } else {
            std::mem::take(&mut self.images)
        };
        let mut agent_input = self.agent_input || literal;
        let tx = self.tx.clone();
        let agent = self.agent.clone();
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        self.busy = true;
        self.busy_since = Instant::now();
        self.wait_started = self.waiting_on_user().then(Instant::now);
        self.busy_label = None;
        self.stream_chars = 0;
        self.turn_tools = 0;
        self.current_tool = None;
        self.turned = false;
        self.spinner_i = (self.spinner_i + 1) % VERBS.len();
        if literal || trimmed.starts_with('/') {
            self.push(Kind::User(trimmed.clone()));
        } else if let Some(cmd) = trimmed.strip_prefix('!') {
            self.push(Kind::User(trimmed.clone()));
            let cmd = cmd.to_string();
            if self.env.config.learning.capture() {
                crate::learn::record(&self.env.root, "shell", cmd.trim(), "");
            }
            tokio::spawn(async move {
                let env = agent.lock().await.env.clone();
                // Your own commands get your shell setup too.
                crate::shell::snapshot::ensure(&env).await;
                let script = format!("{}{cmd}", crate::shell::snapshot::prelude(&env));
                let out = crate::shell::command(&script)
                    .current_dir(env.cwd())
                    .output()
                    .await;
                let text = match out {
                    Ok(o) => format!(
                        "{}{}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    ),
                    Err(e) => e.to_string(),
                };
                let text = crate::tools::bash::strip_ansi(&text);
                let _ = tx.send(AppMsg::Show(format!(
                    "```\n{}\n```",
                    crate::util::truncate_middle(text.trim_end(), 20_000)
                )));
                agent
                    .lock()
                    .await
                    .messages
                    .push(crate::llm::Message::user(format!(
                        "<bash-input>{cmd}</bash-input>\n<bash-output>{}</bash-output>",
                        crate::util::truncate(&text, 20_000)
                    )));
                let _ = tx.send(AppMsg::TurnDone(None));
            });
            return;
        } else if let Some(note) = trimmed.strip_prefix('#') {
            self.push(Kind::User(trimmed.clone()));
            let note = note.to_string();
            tokio::spawn(async move {
                let a = agent.lock().await;
                if let Outcome::Show(s) = commands::add_memory(&a, &note) {
                    let _ = tx.send(AppMsg::Show(s));
                }
                let _ = tx.send(AppMsg::TurnDone(None));
            });
            return;
        } else {
            self.push(Kind::User(trimmed.clone()));
        }
        tokio::spawn(async move {
            let mut a = agent.lock().await;
            let mut prompt = trimmed.clone();
            if !literal && trimmed.starts_with('/') {
                let outcome = commands::handle(&mut a, &trimmed).await;
                let _ = tx.send(AppMsg::Completions(commands::completions(&a)));
                match outcome {
                    Outcome::Show(s) => {
                        let _ = tx.send(AppMsg::Show(s));
                        let _ = tx.send(AppMsg::TurnDone(None));
                        return;
                    }
                    Outcome::Exit => {
                        let _ = tx.send(AppMsg::Exit);
                        return;
                    }
                    Outcome::Reset(m) => {
                        drop(a);
                        let _ = tx.send(AppMsg::Reset(m));
                        let _ = tx.send(AppMsg::TurnDone(None));
                        return;
                    }
                    Outcome::Nothing => {
                        let _ = tx.send(AppMsg::TurnDone(None));
                        return;
                    }
                    Outcome::Prompt(p) => {
                        // The user typed the command; the prompt it expands to
                        // (a skill, `loop.md`, a built-in) is not their words.
                        if !agent_input {
                            a.env.record_user_request(&trimmed);
                        }
                        agent_input = true;
                        prompt = p;
                    }
                    Outcome::Run(job) => {
                        // A goal, team or outcome: the user typed the
                        // command; the prompts the job writes are its own.
                        if !agent_input {
                            a.env.record_user_request(&trimmed);
                        }
                        let label = job.label();
                        let msg = match job.run(&mut a, cancel).await {
                            Ok(r) => AppMsg::Show(r),
                            Err(e) => AppMsg::Agent(Event::Error(format!("{label} failed: {e:#}"))),
                        };
                        a.save_session();
                        let _ = tx.send(msg);
                        let _ = tx.send(AppMsg::TurnDone(None));
                        return;
                    }
                }
            }
            let (text, images) = if literal {
                (prompt, Vec::new())
            } else {
                let prompt = expand_image_tags(&prompt, &image_paths);
                commands::expand_mentions(&a, &prompt).await
            };
            a.typed_input = !agent_input;
            let res = a.run_turn(&text, images, cancel).await;
            a.save_session();
            let err = match res {
                Ok(TurnEnd::Done(_)) => None,
                Ok(TurnEnd::Interrupted) => {
                    let _ = tx.send(AppMsg::Agent(Event::Warn(
                        "↳  Interrupted · what next?".into(),
                    )));
                    None
                }
                Err(e) => Some(format!("Error: {e:#}")),
            };
            let _ = tx.send(AppMsg::TurnDone(err));
        });
    }

    fn interrupt(&mut self) {
        if let Some(c) = &self.cancel {
            c.cancel();
        }
        self.queue.clear();
    }

    /// Submit the prompt with `how` deciding what happens while a turn runs;
    /// when idle it's an ordinary submit.
    fn submit_as(&mut self, how: Deliver) {
        self.deliver = how;
        self.submit();
        self.deliver = Deliver::Step;
    }

    /// Send the oldest queued message now, cutting the running turn short,
    /// instead of letting it wait for the turn to finish. Everything else
    /// queued goes with the interruption, exactly as with esc.
    fn send_queued_now(&mut self) {
        let Some(next) = self.queue.pop_front() else {
            return;
        };
        if let Some(c) = &self.cancel {
            c.cancel();
        }
        // The rest of the queue keeps its place behind it.
        self.queue.push_front(next);
        self.send_now = true;
    }

    fn cycle_mode(&mut self) {
        let m = self.env.mode().next();
        self.env.set_mode(m);
        self.mode = m;
    }

    /// Keep the terminal tab in step with the session, so a glance at the tab
    /// strip says whether Chanto is still working and which project the tab
    /// belongs to: a diamond while a turn runs, ✓ once it finishes, `❯` at
    /// rest, then the project directory's name. Written as OSC 0, which most
    /// terminals show in the tab/window title; terminals that ignore it are
    /// unaffected.
    fn sync_term_title(&mut self) {
        let icon = title_icon(self.busy, self.turned);
        let project = self
            .env
            .cwd()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        let title = window_title(icon, project.as_deref());
        if title != self.term_title {
            let _ = execute!(
                std::io::stdout(),
                crossterm::terminal::SetTitle(title.clone())
            );
            self.term_title = title;
        }
        self.sync_term_status();
    }

    /// The same state as an OSC 7501 report, for terminals that show it
    /// (`terminal_status`): working, waiting on you (a permission prompt or
    /// question), then done or error. Sent only when it changes.
    fn sync_term_status(&mut self) {
        let Some(last) = self.term_status else { return };
        let (status, msg) = match &self.dialog {
            Some(Dialog::Permission { req, .. }) => (
                Some(notify::Status::Blocked("permission")),
                format!("Allow {}?", req.tool),
            ),
            // A picker opened at rest isn't the session waiting on you.
            Some(Dialog::Question { q, tx: Some(_), .. }) if self.busy => (
                Some(notify::Status::Blocked("question")),
                q.lines().next().unwrap_or("").replace('*', ""),
            ),
            _ if self.busy => (Some(notify::Status::Working), String::new()),
            _ if self.turned && self.turn_failed => (Some(notify::Status::Error), String::new()),
            _ if self.turned => (Some(notify::Status::Done), String::new()),
            _ => (None, String::new()),
        };
        if status == last {
            return;
        }
        self.term_status = Some(status);
        if let Some(s) = status {
            notify::send_status(s, &msg);
        }
    }

    fn update_popup(&mut self) {
        let text = &self.input.text;
        // A recalled `/command` shouldn't open the popup: it would take over
        // up/down and stop history browsing on that entry.
        if self.input.recalled() {
            self.popup = None;
            return;
        }
        if text.starts_with('/') && !text.contains(char::is_whitespace) {
            let q = text.to_lowercase();
            let mut items: Vec<(String, String)> = self
                .completions
                .iter()
                .filter(|(c, _)| c.starts_with(&q))
                .cloned()
                .collect();
            if items.is_empty() {
                items = self
                    .completions
                    .iter()
                    .filter(|(c, _)| c.contains(&q[1..]))
                    .cloned()
                    .collect();
            }
            self.popup = if items.is_empty() {
                None
            } else {
                Some(Popup {
                    items,
                    sel: 0,
                    kind: PopupKind::Slash,
                })
            };
            return;
        }
        let (_, word) = self.input.current_word();
        if let Some(q) = word.strip_prefix('@') {
            let q = q.to_lowercase();
            if self.files.is_none() {
                self.files = Some(list_files(&self.env.cwd()));
            }
            let files = self.files.as_ref().unwrap();
            let rank = |files: &[String]| {
                let mut scored: Vec<(i64, &String)> = files
                    .iter()
                    .filter_map(|f| fuzzy_score(&f.to_lowercase(), &q).map(|s| (s, f)))
                    .collect();
                scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())));
                scored
                    .into_iter()
                    .map(|(_, f)| f.clone())
                    .collect::<Vec<String>>()
            };
            let mut items: Vec<(String, String)> = rank(files)
                .into_iter()
                .take(8)
                .map(|f| (format!("@{f}"), String::new()))
                .collect();
            // Room left: gitignored files that match, ranked after the rest.
            if items.len() < 8 && !q.is_empty() {
                if self.ignored_files.is_none() {
                    self.ignored_files = Some(list_ignored_files(&self.env.cwd(), files));
                }
                let more = rank(self.ignored_files.as_deref().unwrap_or_default());
                items.extend(
                    more.into_iter()
                        .take(8 - items.len())
                        .map(|f| (format!("@{f}"), "gitignored".to_string())),
                );
            }
            self.popup = if items.is_empty() {
                None
            } else {
                Some(Popup {
                    items,
                    sel: 0,
                    kind: PopupKind::File,
                })
            };
            return;
        }
        self.popup = None;
    }

    fn accept_popup(&mut self) {
        if let Some(p) = self.popup.take() {
            let (v, _) = &p.items[p.sel];
            if p.kind == PopupKind::Slash {
                self.input.set(&format!("{v} "));
            } else {
                self.input.replace_current_word(&format!("{v} "));
            }
        }
    }
}
// -------------------------------------------------------------------- run

struct TermGuard {
    kb_enh: bool,
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        restore_terminal(self.kb_enh);
    }
}

/// Edit text in $VISUAL / $EDITOR (vi on Unix, notepad on Windows).
fn edit_in_editor(text: &str) -> Result<String> {
    let path = std::env::temp_dir().join(format!(
        "chanto-prompt-{}.md",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    std::fs::write(&path, text)?;
    let fallback = || {
        if cfg!(windows) {
            "notepad".to_string()
        } else {
            "vi".to_string()
        }
    };
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| fallback());
    let mut parts = shell_words::split(&editor).unwrap_or_else(|_| vec![editor.clone()]);
    if parts.is_empty() {
        parts.push(fallback());
    }
    let status = std::process::Command::new(&parts[0])
        .args(&parts[1..])
        .arg(&path)
        .status();
    let out = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);
    let status = status?;
    if !status.success() {
        anyhow::bail!("{editor} exited with {status}");
    }
    Ok(out?)
}

/// A mouse drag selection, in terminal grid coordinates (row, column).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Selection {
    anchor: (u16, u16),
    head: (u16, u16),
}

impl Selection {
    /// Anchor and head in reading order, so a drag up/left works like down/right.
    fn ordered(self) -> ((u16, u16), (u16, u16)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// Raw keystrokes read before the TUI started, as prompt text: backspace
/// applied, escape sequences (arrow keys) and other control keys dropped,
/// and a trailing Enter left out.
fn typeahead_text(raw: &[u8]) -> String {
    let mut out = String::new();
    for c in crate::util::strip_terminal_escapes(
        &String::from_utf8_lossy(raw)
            .replace(['\x7f', '\x08'], "\u{E000}")
            .replace('\r', "\n"),
    )
    .chars()
    {
        match c {
            '\u{E000}' => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out.trim_end_matches('\n').to_string()
}

#[cfg(test)]
mod startup_input_tests;

/// Snap a byte index back to a UTF-8 boundary, for slicing a rendered line whose
/// span boundaries the caller only knows approximately.
#[cfg(test)]
fn floor_char(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
fn ceil_char(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// The `/mode` argument that selects a mode (the inverse of `Mode::parse`).
fn mode_arg(m: crate::permissions::Mode) -> &'static str {
    use crate::permissions::Mode::*;
    match m {
        Default => "default",
        AcceptEdits => "acceptEdits",
        Plan => "plan",
        Auto => "auto",
        Bypass => "bypass",
    }
}

#[cfg(test)]
mod thinking_tests;

#[cfg(test)]
mod image_tests;

#[cfg(test)]
mod selection_tests;

#[cfg(test)]
mod frame_tests;

#[cfg(test)]
mod quit_tests;

#[cfg(test)]
mod welcome_tests;

#[cfg(test)]
mod picker_tests;

#[cfg(test)]
mod stream_tests;

#[cfg(test)]
mod link_tests;

#[cfg(test)]
mod app_tests;

#[cfg(test)]
mod keys_tests;

#[cfg(test)]
mod events_tests;

#[cfg(test)]
mod draw_tests;

#[cfg(test)]
mod run_tests;

#[cfg(test)]
mod quota_tests;

mod view_time;
#[cfg(test)]
mod visual_tests;
