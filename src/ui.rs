//! Terminal rendering: streaming markdown, spinner, tool call display and
//! interactive prompts.

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::{Attribute, Color, Stylize};
use crossterm::terminal;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub static QUIET: AtomicBool = AtomicBool::new(false);
static OUT: Mutex<()> = Mutex::new(());

pub fn is_tty() -> bool {
    std::io::stdout().is_terminal()
}

pub fn width() -> usize {
    terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(100)
        .max(20)
}

/// Word-wrap plain text to `width` columns, for questions printed before
/// the TUI starts (the terminal would break words in half). Existing line
/// breaks stay; a wrapped line keeps the indentation of a `  - ` item.
pub fn word_wrap(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let width = width.max(20);
    let mut out = Vec::new();
    for line in text.split('\n') {
        let indent = line.len() - line.trim_start().len();
        let hang = indent
            + if line.trim_start().starts_with("- ") {
                2
            } else {
                0
            };
        let mut cur = line[..indent].to_string();
        let mut empty = true;
        for word in line.split_whitespace() {
            if !empty && cur.width() + 1 + word.width() > width {
                out.push(std::mem::take(&mut cur));
                cur = " ".repeat(hang);
                empty = true;
            }
            if !empty {
                cur.push(' ');
            }
            cur.push_str(word);
            empty = false;
        }
        out.push(cur);
    }
    out.join("\n")
}

/// Word-wrap to the terminal width, then color every wrapped line.
pub fn wrapped(s: &str, color: Color) -> String {
    word_wrap(s, width())
        .split('\n')
        .map(|l| format!("{}", l.with(color)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Write raw text to stdout (serialized with the spinner).
pub fn out(s: &str) {
    let _g = OUT.lock().unwrap_or_else(|e| e.into_inner());
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(s.as_bytes());
    let _ = o.flush();
}

pub fn line(s: &str) {
    out(&format!("{s}\n"));
}

pub fn dim(s: &str) -> String {
    format!("{}", s.with(Color::DarkGrey))
}

pub fn bold(s: &str) -> String {
    format!("{}", s.bold())
}

fn diagnostic(s: &str, color: Color, tty: bool) -> String {
    if tty {
        format!("{}", s.with(color))
    } else {
        s.into()
    }
}

pub fn warn(s: &str) {
    eprintln!(
        "{}",
        diagnostic(s, Color::Yellow, std::io::stderr().is_terminal())
    );
}

pub fn error(s: &str) {
    eprintln!(
        "{}",
        diagnostic(s, Color::Red, std::io::stderr().is_terminal())
    );
}

pub fn accent(s: &str) -> String {
    format!(
        "{}",
        s.with(Color::Rgb {
            r: 215,
            g: 119,
            b: 87
        })
    )
}

/// A command or path, in the inline-code colour, so it stands out from the dim
/// chrome around it.
pub fn code(s: &str) -> String {
    format!(
        "{}",
        s.with(Color::Rgb {
            r: 210,
            g: 130,
            b: 70
        })
    )
}

// ------------------------------------------------------------------ spinner

pub struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

const FRAMES: &[&str] = &["◰", "◳", "◲", "◱"];

impl Spinner {
    pub fn start(label: &str) -> Spinner {
        let stop = Arc::new(AtomicBool::new(false));
        if !is_tty() || QUIET.load(Ordering::Relaxed) {
            return Spinner { stop, handle: None };
        }
        if crate::a11y::reduce_motion() {
            // No in-place redraw: print the label once and leave it in the flow,
            // so a screen reader reads it in order and nothing repaints under it.
            line(&format!(
                "{} {}",
                "❖".with(Color::Green),
                label.with(Color::DarkGrey)
            ));
            return Spinner { stop, handle: None };
        }
        let s2 = stop.clone();
        let label = label.to_string();
        let handle = std::thread::spawn(move || {
            let t0 = Instant::now();
            let mut i = 0;
            // Small delay so fast responses don't flicker.
            std::thread::sleep(Duration::from_millis(80));
            while !s2.load(Ordering::Relaxed) {
                {
                    let _g = OUT.lock().unwrap_or_else(|e| e.into_inner());
                    let mut o = std::io::stdout().lock();
                    let _ = write!(
                        o,
                        "\r\x1b[2K{} {} {}",
                        accent(FRAMES[i % FRAMES.len()]),
                        accent(&label),
                        dim(&format!(
                            "({} · ctrl+c to interrupt)",
                            crate::util::duration(t0.elapsed().as_secs())
                        ))
                    );
                    let _ = o.flush();
                }
                i += 1;
                std::thread::sleep(Duration::from_millis(80));
            }
            let _g = OUT.lock().unwrap_or_else(|e| e.into_inner());
            let mut o = std::io::stdout().lock();
            let _ = write!(o, "\r\x1b[2K");
            let _ = o.flush();
        });
        Spinner {
            stop,
            handle: Some(handle),
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop();
    }
}

// ------------------------------------------------------- streaming markdown

/// Wrap `text` in an OSC 8 hyperlink pointing at `url`. Terminals that support
/// the sequence (iTerm2, kitty, WezTerm, Ghostty, VS Code, Windows Terminal)
/// make the text cmd/ctrl-clickable; others ignore it and show `text` as-is.
pub fn hyperlink(url: &str, text: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// Make every bare `http(s)://…` URL in `s` clickable, leaving the text
/// otherwise untouched. Sentence punctuation that merely trails the URL (`.`,
/// `,`, an unbalanced `)`, …) stays outside the link — but a closing bracket
/// that the URL itself opened (`…/Rust_(programming_language)`) is kept.
pub fn linkify(s: &str) -> String {
    if !s.contains("http") {
        return s.to_string();
    }
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"https?://[^\s\x1b<>\[\]{}]+").unwrap());
    re.replace_all(s, |c: &regex::Captures| {
        let m = &c[0];
        let mut url = m.trim_end_matches(['.', ',', ';', ':', '!', '?', '\'', '"']);
        for (open, close) in [('(', ')'), ('[', ']'), ('{', '}')] {
            while url.ends_with(close) && url.matches(close).count() > url.matches(open).count() {
                url = &url[..url.len() - 1];
            }
        }
        format!("{}{}", hyperlink(url, url), &m[url.len()..])
    })
    .to_string()
}

/// Incremental markdown styler: styles headings, bullets, code fences,
/// inline code and bold as tokens stream in, without waiting for full lines.
#[derive(Default)]
pub struct MarkdownStream {
    at_line_start: bool,
    line_prefix: String, // buffered start-of-line chars until we know the line type
    in_fence: bool,
    in_code: bool,
    bold: bool,
    heading: bool,
    pending_star: bool,
    pending_tick: usize,
    link: LinkState,
    link_text: String, // buffered `[text]`
    link_url: String,  // buffered `(url)`
    pub wrote_any: bool,
    started: bool,
}

/// Where an in-progress `[text](url)` is up to, so a link split across stream
/// tokens can still be emitted as one hyperlink.
#[derive(Default, PartialEq)]
enum LinkState {
    #[default]
    None,
    Text,
    AfterText,
    Url,
}

impl MarkdownStream {
    pub fn new() -> Self {
        MarkdownStream {
            at_line_start: true,
            ..Default::default()
        }
    }

    pub fn push(&mut self, s: &str) -> String {
        let mut o = String::new();
        for c in s.chars() {
            self.push_char(c, &mut o);
        }
        if !o.is_empty() {
            self.wrote_any = true;
        }
        o
    }

    fn style_on(&self) -> String {
        let mut s = String::from("\x1b[0m");
        if self.in_fence {
            s.push_str("\x1b[38;5;151m");
        }
        if self.heading || self.bold {
            s.push_str(&format!("{}", Attribute::Bold));
        }
        if self.in_code {
            s.push_str("\x1b[38;5;180m");
        }
        s
    }

    fn push_char(&mut self, c: char, o: &mut String) {
        if !self.started {
            // Drop leading blank lines.
            if c == '\n' {
                return;
            }
            self.started = true;
        }
        if self.at_line_start {
            if c == '\n' {
                let p = std::mem::take(&mut self.line_prefix);
                o.push_str(&p);
                o.push('\n');
                return;
            }
            self.line_prefix.push(c);
            let p = self.line_prefix.clone();
            let t = p.trim_start();
            // Need more chars to decide?
            let need_more = (t.is_empty() && p.len() < 8)
                || (!t.is_empty() && t.len() < 3 && "```".starts_with(t))
                || (!t.is_empty() && t.len() < 7 && t.chars().all(|ch| ch == '#'))
                || t == "-"
                || t == "*";
            if need_more {
                return;
            }
            self.at_line_start = false;
            self.line_prefix.clear();
            if t.starts_with("```") {
                self.in_fence = !self.in_fence;
                o.push_str(&format!("{}", p.clone().with(Color::DarkGrey)));
                o.push_str(&self.style_on());
                return;
            }
            if self.in_fence {
                o.push_str(&self.style_on());
                o.push_str(&p);
                return;
            }
            if t.starts_with('#') && t.trim_start_matches('#').starts_with(' ') {
                self.heading = true;
                o.push_str(&self.style_on());
                return; // hide the hashes
            }
            if t == "- " || t == "* " {
                let indent = &p[..p.len() - t.len()];
                o.push_str(indent);
                o.push_str("• ");
                return;
            }
            // Not special: replay through inline handling.
            for ch in p.chars() {
                self.inline(ch, o);
            }
            return;
        }
        if c == '\n' {
            self.flush_pending(o);
            self.flush_link(o);
            if self.heading {
                self.heading = false;
            }
            self.bold = false;
            self.in_code = false;
            o.push_str("\x1b[0m");
            if self.in_fence {
                o.push_str(&self.style_on());
            }
            o.push('\n');
            self.at_line_start = true;
            return;
        }
        if self.in_fence {
            o.push(c);
            return;
        }
        self.inline(c, o);
    }

    fn flush_pending(&mut self, o: &mut String) {
        if self.pending_star {
            o.push('*');
            self.pending_star = false;
        }
        if self.pending_tick > 0 {
            for _ in 0..self.pending_tick {
                o.push('`');
            }
            self.pending_tick = 0;
        }
    }

    /// Emit any half-collected `[text](url)` literally: the stream ended (or a
    /// line did) before the construct was complete, so it was never a link.
    fn flush_link(&mut self, o: &mut String) {
        let text = std::mem::take(&mut self.link_text);
        match std::mem::take(&mut self.link) {
            LinkState::None => {}
            LinkState::Text => o.push_str(&format!("[{text}")),
            LinkState::AfterText => o.push_str(&format!("[{text}]")),
            LinkState::Url => {
                o.push_str(&format!("[{text}]({}", std::mem::take(&mut self.link_url)))
            }
        }
    }

    fn inline(&mut self, c: char, o: &mut String) {
        // Mid `[text](url)`: buffer until the whole construct is known, then
        // emit it as one OSC 8 hyperlink.
        match self.link {
            LinkState::Text => {
                if c == ']' {
                    self.link = LinkState::AfterText;
                } else {
                    self.link_text.push(c);
                }
                return;
            }
            LinkState::AfterText => {
                if c == '(' {
                    self.link = LinkState::Url;
                    return;
                }
                // `[text]` with no destination: it was literal after all.
                o.push_str(&format!("[{}]", std::mem::take(&mut self.link_text)));
                self.link = LinkState::None;
            }
            LinkState::Url => {
                if c == ')' {
                    let url = std::mem::take(&mut self.link_url);
                    let text = std::mem::take(&mut self.link_text);
                    self.link = LinkState::None;
                    let clean = !url.chars().any(|ch| ch.is_control() || ch.is_whitespace());
                    if clean && (url.starts_with("http://") || url.starts_with("https://")) {
                        o.push_str(&hyperlink(
                            &url,
                            &format!("\x1b[4;38;2;80;140;220m{text}\x1b[0m"),
                        ));
                        o.push_str(&self.style_on());
                    } else {
                        o.push_str(&format!("{text} ({url})"));
                    }
                } else {
                    self.link_url.push(c);
                }
                return;
            }
            LinkState::None => {}
        }
        if c == '`' {
            if self.pending_star {
                o.push('*');
                self.pending_star = false;
            }
            self.in_code = !self.in_code;
            o.push_str(&self.style_on());
            return;
        }
        if self.in_code {
            o.push(c);
            return;
        }
        if c == '[' {
            if self.pending_star {
                o.push('*');
                self.pending_star = false;
            }
            self.link = LinkState::Text;
            self.link_text.clear();
            return;
        }
        if c == '*' {
            if self.pending_star {
                self.pending_star = false;
                self.bold = !self.bold;
                o.push_str(&self.style_on());
            } else {
                self.pending_star = true;
            }
            return;
        }
        if self.pending_star {
            o.push('*');
            self.pending_star = false;
        }
        o.push(c);
    }

    pub fn finish(&mut self) -> String {
        let mut o = String::new();
        let p = std::mem::take(&mut self.line_prefix);
        o.push_str(&p);
        self.flush_pending(&mut o);
        self.flush_link(&mut o);
        o.push_str("\x1b[0m");
        *self = MarkdownStream::new();
        o
    }
}

/// Render a complete markdown string (non-streaming).
pub fn render_markdown(s: &str) -> String {
    let mut m = MarkdownStream::new();
    let mut o = m.push(s);
    o.push_str(&m.finish());
    o
}

// ---------------------------------------------------------- tool display

pub fn tool_header(label: &str, depth: usize) {
    if QUIET.load(Ordering::Relaxed) {
        return;
    }
    let indent = "  ".repeat(depth);
    let (name, rest) = match label.find('(') {
        Some(i) => (&label[..i], &label[i..]),
        None => (label, ""),
    };
    line(&format!(
        "{indent}{} {}{}",
        "▸".with(Color::Green),
        name.to_string().bold(),
        code(rest)
    ));
}

pub fn tool_result(summary: &str, is_error: bool, depth: usize) {
    if QUIET.load(Ordering::Relaxed) {
        return;
    }
    out(&tool_result_text(summary, is_error, depth, width()));
}

fn tool_result_text(summary: &str, is_error: bool, depth: usize, width: usize) -> String {
    let indent = "  ".repeat(depth);
    let prefix_len = indent.len() + 5;
    let mut s = String::new();
    for (i, line) in summary.lines().enumerate() {
        let segs = word_wrap(line.trim_end(), width.saturating_sub(prefix_len).max(20));
        for (j, seg) in segs.split('\n').enumerate() {
            let prefix = if i == 0 && j == 0 {
                format!("{indent}  ↳  ")
            } else {
                format!("{indent}     ")
            };
            let seg = seg.trim_end();
            // The command above is highlighted; its output recedes behind it, so
            // dim the whole body (not just the first line) and colour errors red.
            let text = if is_error {
                format!("{}", linkify(seg).with(Color::Red))
            } else {
                dim(&linkify(seg))
            };
            s.push_str(&format!("{prefix}{text}\n"));
        }
    }
    s
}

/// Colored unified diff for edits.
pub fn diff(old: &str, new: &str, max_lines: usize) -> String {
    use similar::{ChangeTag, TextDiff};
    // Diff only the changed region; whole-file diffs of big files are slow.
    let (offset, old, new) = crate::util::changed_region(old, new, 2);
    let d = TextDiff::from_lines(old, new);
    let mut out = String::new();
    let mut n = 0;
    for group in d.grouped_ops(2) {
        for op in group {
            for ch in d.iter_changes(&op) {
                if n >= max_lines {
                    out.push_str(&dim("…"));
                    out.push('\n');
                    return out;
                }
                let ln = ch
                    .new_index()
                    .or(ch.old_index())
                    .map(|i| i + 1 + offset)
                    .unwrap_or(0);
                let text = ch.value().trim_end_matches('\n');
                let l = match ch.tag() {
                    ChangeTag::Delete => format!(
                        "{}",
                        format!("{ln:>5} -{text}").on(Color::Rgb {
                            r: 80,
                            g: 20,
                            b: 20
                        })
                    ),
                    ChangeTag::Insert => format!(
                        "{}",
                        format!("{ln:>5} +{text}").on(Color::Rgb {
                            r: 20,
                            g: 60,
                            b: 20
                        })
                    ),
                    ChangeTag::Equal => dim(&format!("{ln:>5}  {text}")),
                };
                out.push_str(&l);
                out.push('\n');
                n += 1;
            }
        }
    }
    out
}

// --------------------------------------------------------------- prompts

fn read_key() -> Option<KeyEvent> {
    let _ = terminal::enable_raw_mode();
    let r = loop {
        match event::read() {
            Ok(Event::Key(k)) if k.kind == event::KeyEventKind::Press => break Some(k),
            Ok(_) => continue,
            Err(_) => break None,
        }
    };
    let _ = terminal::disable_raw_mode();
    r
}

/// Show a numbered menu and return the chosen index (None = escape/cancel).
pub fn choose(title: &str, body: &str, options: &[String]) -> Option<usize> {
    let _g = OUT.lock().unwrap_or_else(|e| e.into_inner());
    let w = width().min(100);
    let mut s = String::new();
    s.push_str(&format!(
        "\n{}\n",
        format!("╭─ {title} ").with(Color::Yellow).bold()
    ));
    for l in body.lines() {
        s.push_str(&format!(
            "{} {}\n",
            "│".with(Color::Yellow),
            crate::util::truncate(l, w)
        ));
    }
    s.push_str(&format!("{}\n", "╰─".with(Color::Yellow)));
    for (i, o) in options.iter().enumerate() {
        s.push_str(&format!("  {} {}\n", format!("{}.", i + 1).bold(), o));
    }
    s.push_str(&dim("  (number to choose, esc to cancel)"));
    s.push('\n');
    print!("{s}");
    let _ = std::io::stdout().flush();
    loop {
        let k = read_key()?;
        match k.code {
            KeyCode::Esc => return None,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return None,
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let i = c.to_digit(10).unwrap() as usize;
                if i >= 1 && i <= options.len() {
                    return Some(i - 1);
                }
            }
            KeyCode::Char('y') => return Some(0),
            KeyCode::Char('n') => return Some(options.len() - 1),
            KeyCode::Enter => return Some(0),
            _ => {}
        }
    }
}

/// Read a single line of text (cooked mode).
pub fn read_line(prompt: &str) -> Option<String> {
    let _g = OUT.lock().unwrap_or_else(|e| e.into_inner());
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    match std::io::stdin().read_line(&mut s) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(s.trim_end_matches(['\n', '\r']).to_string()),
    }
}

pub fn banner(model: &str, cwd: &str, mode: &str) {
    let title = format!(
        "› {} chanto",
        "✓".with(Color::Rgb {
            r: 16,
            g: 185,
            b: 129
        })
    );
    line(&format!(
        "\n{}  {}\n   {}  {}\n   {}\n",
        title.bold(),
        dim(&format!("v{}", env!("CARGO_PKG_VERSION"))),
        dim("model"),
        model,
        dim(&format!(
            "{cwd} · {mode} · /help for commands · shift+tab cycles modes"
        )),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_questions_wrap_at_word_boundaries() {
        let text = "Only trust folders whose contents you trust. Trust /a/very/long/path?\n  - .chanto/config.toml (hooks and permission rules)";
        let w = word_wrap(text, 29);
        assert!(
            w.lines()
                .all(|l| l.chars().count() <= 29 || !l.trim().contains(' ')),
            "{w}"
        );
        assert_eq!(
            w.split_whitespace().collect::<Vec<_>>(),
            text.split_whitespace().collect::<Vec<_>>(),
            "no word is split or lost"
        );
        assert!(
            w.contains("\n    (hooks"),
            "an item's continuation lines up under its text: {w}"
        );
        assert_eq!(word_wrap("short", 80), "short");
    }

    #[test]
    fn redirected_diagnostics_have_no_colors() {
        assert_eq!(diagnostic("error", Color::Red, false), "error");
        assert_eq!(diagnostic("warning", Color::Yellow, false), "warning");
        assert!(diagnostic("error", Color::Red, true).contains('\u{1b}'));
    }

    fn strip(s: &str) -> String {
        let sgr = regex::Regex::new("\x1b\\[[0-9;]*m").unwrap();
        let osc8 = regex::Regex::new("\x1b\\]8;;[^\x1b]*\x1b\\\\").unwrap();
        osc8.replace_all(&sgr.replace_all(s, ""), "").to_string()
    }

    #[test]
    fn long_tool_results_wrap_under_their_prefix() {
        let summary = "Blocked: a foreground `sleep 20` only uses up time. Run it in the background \
                       instead, and check on it while you get on with other work.";
        let s = strip(&tool_result_text(summary, false, 0, 60));
        assert!(s.lines().count() > 2, "the long line wraps: {s}");
        assert!(s.lines().all(|l| l.chars().count() <= 60), "{s}");
        assert!(
            s.lines().skip(1).all(|l| l.starts_with(&" ".repeat(5))),
            "continuation lines line up under the text: {s}"
        );
    }

    #[test]
    fn every_tool_output_row_recedes_behind_its_command() {
        // The command above is highlighted; its whole body carries one style —
        // not just the first line. Each row must have the same escape envelope
        // (crossterm drops the colors themselves under NO_COLOR, so compare the
        // shape, not the exact sequences).
        let body = "line one\nline two\nline three";
        for is_error in [false, true] {
            let out = tool_result_text(body, is_error, 0, 80);
            let rows: Vec<&str> = out.lines().collect();
            assert_eq!(rows.len(), 3, "{out:?}");
            let esc = |l: &str| l.matches('\x1b').count();
            let n = esc(rows[0]);
            assert!(n >= 2, "the first row is styled: {out:?}");
            assert!(
                rows.iter().all(|l| esc(l) == n),
                "every row is styled alike (is_error={is_error}): {out:?}"
            );
        }
    }

    #[test]
    fn markdown_links_become_hyperlinks() {
        // Split across stream tokens, the way a model's output arrives.
        let mut m = MarkdownStream::new();
        let mut o = String::new();
        for t in ["see [the ", "docs](htt", "ps://docs.rs/tok", "io) now"] {
            o.push_str(&m.push(t));
        }
        o.push_str(&m.finish());
        assert!(o.contains("\x1b]8;;https://docs.rs/tokio\x1b\\"), "{o:?}");
        assert!(!o.contains("](http"), "raw syntax should be gone: {o:?}");
        assert!(strip(&o).contains("see the docs now"), "{o:?}");
        // A relative destination isn't clickable; keep it readable.
        assert!(strip(&render_markdown("[main](src/main.rs)")).contains("main (src/main.rs)"));
    }

    #[test]
    fn brackets_that_are_not_links_stay_literal() {
        let o = render_markdown("a [bracket] and `[x](y)` and **[b]** and dangling [abc");
        assert!(!o.contains("\x1b]8;;"), "nothing should be linked: {o:?}");
        assert!(
            strip(&o).contains("a [bracket] and [x](y) and [b] and dangling [abc"),
            "{:?}",
            strip(&o)
        );
        assert!(
            strip(&render_markdown("- [ ] todo\n- [x] done\n")).contains("• [ ] todo\n• [x] done")
        );
    }

    #[test]
    fn bare_urls_are_linkified() {
        let s = linkify("see https://example.com/a. done");
        assert!(
            s.contains("\x1b]8;;https://example.com/a\x1b\\https://example.com/a\x1b]8;;\x1b\\."),
            "{s:?}"
        );
        assert_eq!(linkify("no url here"), "no url here");
        // An unbalanced trailing ) is sentence punctuation, not part of the URL.
        assert!(
            linkify("(see https://example.com/x)").contains("\x1b]8;;https://example.com/x\x1b\\")
        );
        // A ) the URL opened itself belongs to it.
        let wiki = linkify("https://en.wikipedia.org/wiki/Rust_(programming_language) baz");
        assert!(
            wiki.contains(
                "\x1b]8;;https://en.wikipedia.org/wiki/Rust_(programming_language)\x1b\\"
            ),
            "{wiki:?}"
        );
    }

    #[test]
    fn markdown_stream_handles_split_tokens() {
        let mut m = MarkdownStream::new();
        let mut o = String::new();
        for t in [
            "# Ti",
            "tle\nsome **bo",
            "ld** and `co",
            "de`\n- item\n```rust\nfn x() {}\n```\n",
        ] {
            o.push_str(&m.push(t));
        }
        o.push_str(&m.finish());
        let plain = strip(&o);
        assert!(plain.contains("Title\n"));
        assert!(plain.contains("some bold and code\n"));
        assert!(plain.contains("• item"));
        assert!(plain.contains("fn x() {}"));
    }
}

#[cfg(test)]
mod diff_tests {
    /// The diff of the changed region must render exactly like a diff of the
    /// whole file.
    #[test]
    fn region_diff_matches_full_diff() {
        fn full(old: &str, new: &str, max: usize) -> String {
            use similar::{ChangeTag, TextDiff};
            let d = TextDiff::from_lines(old, new);
            let mut v = Vec::new();
            for g in d.grouped_ops(2) {
                for op in g {
                    for ch in d.iter_changes(&op) {
                        let ln = ch
                            .new_index()
                            .or(ch.old_index())
                            .map(|i| i + 1)
                            .unwrap_or(0);
                        let t = match ch.tag() {
                            ChangeTag::Delete => '-',
                            ChangeTag::Insert => '+',
                            ChangeTag::Equal => ' ',
                        };
                        v.push(format!("{ln} {t}{}", ch.value().trim_end_matches('\n')));
                    }
                }
            }
            v.truncate(max);
            v.join("\n")
        }
        fn mine(old: &str, new: &str, max: usize) -> String {
            use similar::{ChangeTag, TextDiff};
            let (off, o, n) = crate::util::changed_region(old, new, 2);
            let d = TextDiff::from_lines(o, n);
            let mut v = Vec::new();
            for g in d.grouped_ops(2) {
                for op in g {
                    for ch in d.iter_changes(&op) {
                        let ln = ch
                            .new_index()
                            .or(ch.old_index())
                            .map(|i| i + 1 + off)
                            .unwrap_or(0);
                        let t = match ch.tag() {
                            ChangeTag::Delete => '-',
                            ChangeTag::Insert => '+',
                            ChangeTag::Equal => ' ',
                        };
                        v.push(format!("{ln} {t}{}", ch.value().trim_end_matches('\n')));
                    }
                }
            }
            v.truncate(max);
            v.join("\n")
        }
        let base: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let cases: Vec<String> = vec![
            base.replace("line 100\n", "line 100 changed\n"),
            base.replace("line 0\n", "first\n"),
            base.replace("line 199\n", "last\n"),
            base.replace("line 50\n", "line 50\ninserted a\ninserted b\n"),
            base.replace("line 70\nline 71\n", ""),
            base.trim_end_matches('\n').to_string(),
            format!("{base}tail without newline"),
            base.replace("line 10\n", "x\n")
                .replace("line 150\n", "y\n"),
            base.replace("line 1", "line one"),
            base.clone(),
            String::new(),
        ];
        for (i, new) in cases.iter().enumerate() {
            assert_eq!(mine(&base, new, 100), full(&base, new, 100), "case {i}");
            assert_eq!(
                mine(new, &base, 100),
                full(new, &base, 100),
                "reverse case {i}"
            );
        }
    }
}
