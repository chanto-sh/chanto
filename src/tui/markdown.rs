//! Markdown → styled lines, ANSI → styled lines, and width-aware word wrapping.

use super::theme::pal;
use pulldown_cmark::{CodeBlockKind, Event as Md, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

static LIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set by the theme (see `theme::switch`).
pub fn set_light(on: bool) {
    LIGHT.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn is_light() -> bool {
    LIGHT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Shorthands for theme colors used all over the TUI.
pub fn warn() -> Color {
    pal().warning
}
pub fn cyan() -> Color {
    pal().info
}
/// Auto mode: amber, so autonomous operation stands out from accept-edits (purple).
pub fn amber() -> Color {
    pal().auto_mode
}
pub fn panel_bg() -> Color {
    pal().panel_bg
}
/// Background for a text selection (drag-select or a search hit). Pairs with
/// [`highlight_fg`] so the text stays legible no matter which foreground the
/// underlying span carried.
pub fn highlight_bg() -> Color {
    pal().selection_bg
}

/// Foreground used together with [`highlight_bg`]. Selection overrides the
/// span's own color, which is what keeps dim greys and reds readable while
/// selected.
pub fn highlight_fg() -> Color {
    pal().selection_fg
}
pub fn faint() -> Color {
    pal().faint
}

/// Whether the terminal has a light background. Asked once: the terminal's
/// own answer first (OSC 11), then `COLORFGBG`, otherwise dark. The system
/// appearance is not consulted: a dark terminal on a Mac in light mode is
/// common, and a light palette there turns code into unreadable pastels.
pub fn detect_light() -> bool {
    static LIGHT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *LIGHT.get_or_init(|| {
        terminal_background()
            .map(|rgb| luminance(rgb) > 0.5)
            .unwrap_or_else(detect_light_from_env)
    })
}

/// Relative luminance of an sRGB colour (components 0..1).
fn luminance((r, g, b): (f64, f64, f64)) -> f64 {
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// The colour in an OSC 11 reply (`\x1b]11;rgb:RRRR/GGGG/BBBB\x07`; 1-4 hex
/// digits per channel), as 0..1 components.
pub(crate) fn parse_osc11(reply: &str) -> Option<(f64, f64, f64)> {
    let rgb = &reply[reply.find("rgb:")? + 4..];
    let mut parts = rgb.split('/').map(|p| {
        p.chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect::<String>()
    });
    let mut channel = || {
        let h = parts.next()?;
        (!h.is_empty() && h.len() <= 4)
            .then(|| {
                u32::from_str_radix(&h, 16)
                    .ok()
                    .map(|v| v as f64 / ((1u32 << (4 * h.len())) - 1) as f64)
            })
            .flatten()
    };
    Some((channel()?, channel()?, channel()?))
}

/// Keys typed before the TUI took over (say, right after answering a y/N
/// question at startup) that the background-colour query read along with
/// its reply. The TUI puts them in the prompt.
static TYPEAHEAD: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());

/// Take the keys the background query read on the TUI's behalf.
pub fn take_typeahead() -> Vec<u8> {
    std::mem::take(&mut *TYPEAHEAD.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Split what was read after the OSC 11 query into the terminal's reply
/// (when complete) and everything else, which is the user typing.
pub(crate) fn split_osc11(buf: &[u8]) -> (Option<String>, Vec<u8>) {
    let Some(start) = buf.windows(5).position(|w| w == b"\x1b]11;") else {
        return (None, buf.to_vec());
    };
    let after = &buf[start..];
    let end = after
        .iter()
        .position(|&b| b == 0x07)
        .map(|i| i + 1)
        .or_else(|| after.windows(2).position(|w| w == b"\x1b\\").map(|i| i + 2));
    match end {
        Some(end) => {
            let mut rest = buf[..start].to_vec();
            rest.extend_from_slice(&after[end..]);
            (
                Some(String::from_utf8_lossy(&after[..end]).into_owned()),
                rest,
            )
        }
        None => (None, buf[..start].to_vec()),
    }
}

/// Ask the terminal for its background colour. Only before the TUI reads
/// keys: a late reply would land in the input as text.
#[cfg(unix)]
fn terminal_background() -> Option<(f64, f64, f64)> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return None;
    }
    let was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
    if !was_raw {
        crossterm::terminal::enable_raw_mode().ok()?;
    }
    let reply = (|| {
        let mut out = std::io::stdout();
        out.write_all(b"\x1b]11;?\x07").ok()?;
        out.flush().ok()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        let mut buf = Vec::new();
        while split_osc11(&buf).0.is_none() {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            let mut pfd = libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut pfd, 1, left.as_millis() as i32) } <= 0 {
                break;
            }
            let mut b = [0u8; 64];
            let n = unsafe { libc::read(0, b.as_mut_ptr().cast(), b.len()) };
            if n <= 0 {
                break;
            }
            buf.extend_from_slice(&b[..n as usize]);
        }
        // Keys typed meanwhile aren't the reply: keep them for the prompt.
        let (reply, typed) = split_osc11(&buf);
        TYPEAHEAD
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(&typed);
        reply
    })();
    if !was_raw {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    parse_osc11(&reply?)
}

#[cfg(not(unix))]
fn terminal_background() -> Option<(f64, f64, f64)> {
    None
}

fn detect_light_from_env() -> bool {
    light_from_colorfgbg(std::env::var("COLORFGBG").ok().as_deref())
}

/// Whether `COLORFGBG` (`fg;bg` or `fg;default;bg`) names a light background.
/// Unset or unparsable means dark: tmux and ssh never answer OSC 11, and the
/// OS appearance says nothing about the terminal's own background.
fn light_from_colorfgbg(v: Option<&str>) -> bool {
    match v
        .and_then(|v| v.rsplit(';').next())
        .and_then(|b| b.trim().parse::<u8>().ok())
    {
        Some(bg) => bg == 7 || (9..=15).contains(&bg),
        None => false,
    }
}

/// A table as text lines: the header, a rule, then the rows, with every
/// column padded to its widest cell so the `│` separators line up.
fn table_lines(rows: &[Vec<String>]) -> Vec<String> {
    let width = |s: &str| s.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>();
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|c| {
            rows.iter()
                .filter_map(|r| r.get(c))
                .map(|s| width(s))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |r: &Vec<String>| {
        let cells: Vec<String> = (0..cols)
            .map(|c| {
                let s = r.get(c).map(String::as_str).unwrap_or("");
                format!("{s}{}", " ".repeat(widths[c] - width(s)))
            })
            .collect();
        cells.join(" │ ").trim_end().to_string()
    };
    let mut out = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        out.push(line(r));
        if i == 0 {
            out.push(
                widths
                    .iter()
                    .map(|w| "─".repeat(*w))
                    .collect::<Vec<_>>()
                    .join("─┼─"),
            );
        }
    }
    out
}

/// A logical line plus the indent (in columns) to use for wrapped continuations.
pub struct Logical {
    pub line: Line<'static>,
    pub indent: usize,
}

/// Prose with bare `http(s)://…` URLs styled like Markdown links. Models paste
/// URLs as plain text at least as often as they write `[text](url)`, and those
/// deserve to read as links too. Sentence punctuation that trails the URL
/// stays outside the link, as does a `)` the URL itself did not open.
pub(super) fn link_spans(text: &str, style: Style) -> Vec<Span<'static>> {
    let link = style.fg(pal().link).add_modifier(Modifier::UNDERLINED);
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut at = 0;
    while at < text.len() {
        let Some(rel) = text[at..].find("http") else {
            break;
        };
        let start = at + rel;
        if !(text[start..].starts_with("http://") || text[start..].starts_with("https://")) {
            // `http` inside a word, not a scheme: keep looking after it.
            out.push(Span::styled(text[at..start + 4].to_string(), style));
            at = start + 4;
            continue;
        }
        if start > at {
            out.push(Span::styled(text[at..start].to_string(), style));
        }
        let mut end = text[start..]
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`'))
            .map(|n| start + n)
            .unwrap_or(text.len());
        while end > start {
            let c = text[..end].chars().next_back().unwrap_or(' ');
            match c {
                '.' | ',' | ';' | ':' | '!' | '?' => end -= c.len_utf8(),
                ')' if text[start..end].matches(')').count()
                    > text[start..end].matches('(').count() =>
                {
                    end -= 1;
                }
                _ => break,
            }
        }
        out.push(Span::styled(text[start..end].to_string(), link));
        at = end;
    }
    if at < text.len() {
        out.push(Span::styled(text[at..].to_string(), style));
    }
    out
}

/// A run of link-styled text on a rendered line, by screen column.
pub struct LinkRun {
    pub start: usize,
    pub end: usize,
    pub text: String,
    /// The destination when the run is a Markdown link's label (`[docs](url)`)
    /// rather than the URL itself.
    pub dest: Option<String>,
}

fn is_link_style(style: Style) -> bool {
    style.add_modifier.contains(Modifier::UNDERLINED)
        && (super::theme::no_color() || style.fg == Some(pal().link))
}

/// The `(url)` in a span, as `(offset of the url, url)`.
fn paren_url(t: &str) -> Option<(usize, String)> {
    let lead = t.len() - t.trim_start().len();
    let inner = t.trim().strip_prefix('(')?.strip_suffix(')')?;
    (inner.starts_with("http://") || inner.starts_with("https://"))
        .then(|| (lead + 1, inner.to_string()))
}

/// Link-styled runs on a rendered line. A wrapped URL arrives as one run per
/// line; the caller stitches those back together (see `link_regions` in the
/// TUI). The dim ` (url)` shown after a Markdown link's label is reported as a
/// run of its own, so the URL on screen is clickable as well as the label.
pub fn link_runs(line: &Line<'static>) -> Vec<LinkRun> {
    let mut runs: Vec<LinkRun> = Vec::new();
    let mut spans: Vec<(usize, usize, &str)> = Vec::new();
    let mut col = 0usize;
    for span in &line.spans {
        let w = unicode_width::UnicodeWidthStr::width(span.content.as_ref());
        spans.push((col, col + w, span.content.as_ref()));
        if is_link_style(span.style) {
            if let Some(last) = runs.last_mut()
                && last.end == col
            {
                last.text.push_str(&span.content);
                last.end = col + w;
            } else {
                runs.push(LinkRun {
                    start: col,
                    end: col + w,
                    text: span.content.to_string(),
                    dest: None,
                });
            }
        }
        col += w;
    }
    for run in &mut runs {
        if run.text.starts_with("http://") || run.text.starts_with("https://") {
            continue;
        }
        run.dest = spans
            .iter()
            .filter(|(s, _, _)| *s >= run.end)
            .find_map(|(_, _, t)| paren_url(t))
            .map(|(_, url)| url);
    }
    for (s, _, t) in &spans {
        if let Some((off, url)) = paren_url(t) {
            let start = s + off;
            runs.push(LinkRun {
                start,
                end: start + url.len(),
                text: url.clone(),
                dest: Some(url),
            });
        }
    }
    runs.sort_by_key(|r| r.start);
    runs
}

pub fn render_markdown(src: &str) -> Vec<Logical> {
    let mut out: Vec<Logical> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut prefix_stack: Vec<String> = Vec::new(); // quote/list indentation
    let mut styles: Vec<Style> = vec![Style::default()];
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut in_code: Option<String> = None;
    let mut pending_item_marker: Option<String> = None;
    let mut link_dest: Option<String> = None;
    let mut table_row: Vec<String> = Vec::new();
    let mut table_cell = String::new();
    // Rows of the table being read (the first is the header), laid out in
    // aligned columns when the table ends.
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut in_table = false;

    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    let prefix = |stack: &Vec<String>| stack.concat();

    macro_rules! flush {
        () => {{
            if !cur.is_empty() || pending_item_marker.is_some() {
                let p = prefix(&prefix_stack);
                let mut spans = vec![Span::raw(p.clone())];
                let mut indent = p.chars().count();
                if let Some(m) = pending_item_marker.take() {
                    indent += m.chars().count();
                    spans.push(Span::styled(m, Style::default().fg(pal().dim)));
                }
                spans.extend(std::mem::take(&mut cur));
                out.push(Logical {
                    line: Line::from(spans),
                    indent,
                });
            }
        }};
    }
    macro_rules! blank {
        () => {{
            if out
                .last()
                .map(|l| !l.line.spans.iter().all(|s| s.content.trim().is_empty()))
                .unwrap_or(false)
            {
                out.push(Logical {
                    line: Line::raw(""),
                    indent: 0,
                });
            }
        }};
    }

    for ev in Parser::new_ext(src, opts) {
        let style = *styles.last().unwrap();
        match ev {
            Md::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    flush!();
                    blank!();
                    // H1 and H2 in the accent colour, so a bold H3 still reads
                    // as a heading rather than as emphasis.
                    let s = match level {
                        HeadingLevel::H1 => Style::default()
                            .fg(pal().accent)
                            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                        HeadingLevel::H2 => Style::default()
                            .fg(pal().accent)
                            .add_modifier(Modifier::BOLD),
                        _ => Style::default().add_modifier(Modifier::BOLD),
                    };
                    styles.push(s);
                }
                Tag::Paragraph => {
                    if lists.is_empty() {
                        flush!();
                        blank!();
                    }
                }
                Tag::BlockQuote(_) => {
                    flush!();
                    prefix_stack.push("│ ".into());
                    styles.push(style.add_modifier(Modifier::ITALIC).fg(pal().dim));
                }
                Tag::CodeBlock(kind) => {
                    flush!();
                    blank!();
                    let lang = match kind {
                        CodeBlockKind::Fenced(l) => l.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    in_code = Some(lang);
                }
                Tag::List(start) => {
                    flush!();
                    if lists.is_empty() {
                        blank!();
                    }
                    lists.push(start);
                }
                Tag::Item => {
                    flush!();
                    let depth = lists.len().saturating_sub(1);
                    let marker = match lists.last_mut() {
                        Some(Some(n)) => {
                            let m = format!("{n}. ");
                            *n += 1;
                            m
                        }
                        _ => {
                            if depth.is_multiple_of(2) {
                                "• ".to_string()
                            } else {
                                "◦ ".to_string()
                            }
                        }
                    };
                    pending_item_marker = Some(format!("{}{}", "  ".repeat(depth), marker));
                }
                Tag::Emphasis => styles.push(style.add_modifier(Modifier::ITALIC)),
                Tag::Strong => styles.push(style.add_modifier(Modifier::BOLD)),
                Tag::Strikethrough => styles.push(style.add_modifier(Modifier::CROSSED_OUT)),
                Tag::Link { dest_url, .. } => {
                    link_dest = Some(dest_url.to_string());
                    styles.push(style.fg(pal().link).add_modifier(Modifier::UNDERLINED));
                }
                Tag::Table(_) => {
                    flush!();
                    blank!();
                    in_table = true;
                }
                Tag::TableHead | Tag::TableRow => table_row.clear(),
                Tag::TableCell => table_cell.clear(),
                _ => {}
            },
            Md::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    styles.pop();
                    flush!();
                }
                TagEnd::Paragraph => flush!(),
                TagEnd::BlockQuote(_) => {
                    flush!();
                    prefix_stack.pop();
                    styles.pop();
                }
                TagEnd::CodeBlock => {
                    in_code = None;
                }
                TagEnd::List(_) => {
                    flush!();
                    lists.pop();
                }
                TagEnd::Item => flush!(),
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    styles.pop();
                }
                TagEnd::Link => {
                    styles.pop();
                    if let Some(d) = link_dest.take() {
                        let shown: String = cur.iter().map(|s| s.content.as_ref()).collect();
                        if !d.starts_with('#') && !shown.contains(&d) {
                            cur.push(Span::styled(
                                format!(" ({d})"),
                                Style::default().fg(pal().dim),
                            ));
                        }
                    }
                }
                TagEnd::TableCell => table_row.push(std::mem::take(&mut table_cell)),
                TagEnd::TableHead | TagEnd::TableRow => {
                    table_rows.push(std::mem::take(&mut table_row))
                }
                TagEnd::Table => {
                    in_table = false;
                    for (i, line) in table_lines(&std::mem::take(&mut table_rows))
                        .into_iter()
                        .enumerate()
                    {
                        let style = match i {
                            0 => Style::default().add_modifier(Modifier::BOLD),
                            1 => Style::default().fg(pal().dim),
                            _ => Style::default(),
                        };
                        out.push(Logical {
                            line: Line::styled(line, style),
                            indent: 0,
                        });
                    }
                }
                _ => {}
            },
            Md::Text(t) => {
                if in_table {
                    table_cell.push_str(&t);
                } else if let Some(lang) = &in_code {
                    let p = prefix(&prefix_stack);
                    let highlighted = highlight_block(t.trim_end_matches('\n'), lang);
                    for spans in highlighted {
                        let mut v = vec![Span::raw(format!("{p}  "))];
                        v.extend(spans);
                        out.push(Logical {
                            line: Line::from(v),
                            indent: p.chars().count() + 2,
                        });
                    }
                } else if link_dest.is_some() {
                    // Already styled by the link; don't restyle its text.
                    cur.push(Span::styled(t.to_string(), style));
                } else {
                    cur.extend(link_spans(&t, style));
                }
            }
            Md::Code(t) => {
                if in_table {
                    table_cell.push_str(&t);
                } else if super::theme::no_color() {
                    // No colour to set it apart: keep the backticks.
                    cur.push(Span::styled(format!("`{t}`"), style));
                } else {
                    cur.push(Span::styled(t.to_string(), style.fg(pal().code)));
                }
            }
            // Keep the line breaks the model wrote: line-per-item
            // output without list markers would otherwise run into one paragraph.
            Md::SoftBreak => flush!(),
            Md::HardBreak => flush!(),
            Md::Rule => {
                flush!();
                out.push(Logical {
                    line: Line::styled("─".repeat(40), Style::default().fg(pal().dim)),
                    indent: 0,
                });
            }
            Md::TaskListMarker(done) => {
                cur.push(Span::raw(if done { "☒ " } else { "☐ " }));
            }
            Md::Html(h) | Md::InlineHtml(h) => {
                cur.push(Span::styled(h.to_string(), style.fg(pal().dim)))
            }
            _ => {}
        }
    }
    flush!();
    // Trim leading/trailing blank lines.
    while out.first().map(|l| l.line.width() == 0).unwrap_or(false) {
        out.remove(0);
    }
    while out.last().map(|l| l.line.width() == 0).unwrap_or(false) {
        out.pop();
    }
    out
}

struct Highlighter {
    syntaxes: syntect::parsing::SyntaxSet,
    dark: syntect::highlighting::Theme,
    light: syntect::highlighting::Theme,
}

impl Highlighter {
    fn theme(&self) -> &syntect::highlighting::Theme {
        if is_light() { &self.light } else { &self.dark }
    }
}

fn highlighter() -> &'static Highlighter {
    static H: std::sync::OnceLock<Highlighter> = std::sync::OnceLock::new();
    H.get_or_init(|| {
        let syntaxes = syntect::parsing::SyntaxSet::load_defaults_newlines();
        let mut themes = syntect::highlighting::ThemeSet::load_defaults();
        let dark = themes
            .themes
            .remove("base16-ocean.dark")
            .unwrap_or_default();
        let light = themes.themes.remove("InspiredGitHub").unwrap_or_default();
        Highlighter {
            syntaxes,
            dark,
            light,
        }
    })
}

/// Syntax-highlight a code block with syntect; falls back to the light
/// highlighter for unknown languages or very large blocks.
fn highlight_block(code: &str, lang: &str) -> Vec<Vec<Span<'static>>> {
    use syntect::easy::HighlightLines;
    if super::theme::no_color() {
        return code
            .split('\n')
            .map(|l| vec![Span::raw(l.replace('\t', "    "))])
            .collect();
    }
    let lang_l = lang
        .split(|c: char| c == ',' || c.is_whitespace())
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if code.len() > 60_000 || matches!(lang_l.as_str(), "diff" | "patch" | "" | "text" | "txt") {
        return code
            .split('\n')
            .map(|l| highlight_code_line(l, &lang_l))
            .collect();
    }
    let h = highlighter();
    let alias = match lang_l.as_str() {
        "ts" | "typescript" | "tsx" => "js",
        "py" => "python",
        "sh" | "bash" | "zsh" | "shell" | "console" => "sh",
        "rs" => "rust",
        "yml" => "yaml",
        "jsonc" => "json",
        "golang" => "go",
        x => x,
    };
    let Some(syntax) = h.syntaxes.find_syntax_by_token(alias) else {
        return code
            .split('\n')
            .map(|l| highlight_code_line(l, &lang_l))
            .collect();
    };
    let mut hl = HighlightLines::new(syntax, h.theme());
    // The theme's plain text colour becomes the terminal's own foreground, so
    // untokenised text stays readable whatever the background really is.
    let plain = h.theme().settings.foreground;
    let mut out = Vec::new();
    for line in syntect::util::LinesWithEndings::from(code) {
        let ranges = match hl.highlight_line(line, &h.syntaxes) {
            Ok(r) => r,
            Err(_) => {
                out.push(vec![Span::styled(
                    line.trim_end_matches('\n').to_string(),
                    Style::default().fg(pal().code_block),
                )]);
                continue;
            }
        };
        let spans = ranges
            .into_iter()
            .filter(|(_, t)| !t.is_empty())
            .map(|(st, t)| {
                let mut s = Style::default().fg(syntect_fg(st.foreground, plain));
                if st
                    .font_style
                    .contains(syntect::highlighting::FontStyle::BOLD)
                {
                    s = s.add_modifier(Modifier::BOLD);
                }
                if st
                    .font_style
                    .contains(syntect::highlighting::FontStyle::ITALIC)
                {
                    s = s.add_modifier(Modifier::ITALIC);
                }
                Span::styled(t.trim_end_matches('\n').replace('\t', "    "), s)
            })
            .collect();
        out.push(spans);
    }
    out
}

/// A syntect token colour as a terminal colour; the theme's default
/// foreground maps to `Color::Reset`.
fn syntect_fg(
    c: syntect::highlighting::Color,
    plain: Option<syntect::highlighting::Color>,
) -> Color {
    if plain == Some(c) {
        Color::Reset
    } else {
        Color::Rgb(c.r, c.g, c.b)
    }
}

/// Very light keyword/string/comment highlighting (no grammar needed).
fn highlight_code_line(line: &str, lang: &str) -> Vec<Span<'static>> {
    let base = Style::default().fg(pal().code_block);
    if matches!(lang, "diff" | "patch") {
        let c = if line.starts_with('+') {
            pal().diff_add
        } else if line.starts_with('-') {
            pal().diff_remove
        } else if line.starts_with("@@") {
            cyan()
        } else {
            pal().code_block
        };
        return vec![Span::styled(line.to_string(), Style::default().fg(c))];
    }
    let comment = match lang {
        "py" | "python" | "sh" | "bash" | "zsh" | "shell" | "toml" | "yaml" | "yml" | "rb"
        | "ruby" => "#",
        "sql" | "lua" => "--",
        _ => "//",
    };
    let trimmed = line.trim_start();
    if trimmed.starts_with(comment) {
        return vec![Span::styled(
            line.to_string(),
            Style::default()
                .fg(pal().dim)
                .add_modifier(Modifier::ITALIC),
        )];
    }
    const KW: &[&str] = &[
        "fn",
        "let",
        "mut",
        "pub",
        "use",
        "mod",
        "struct",
        "enum",
        "impl",
        "trait",
        "match",
        "if",
        "else",
        "for",
        "while",
        "loop",
        "return",
        "async",
        "await",
        "const",
        "static",
        "def",
        "class",
        "import",
        "from",
        "as",
        "function",
        "var",
        "export",
        "interface",
        "type",
        "new",
        "try",
        "catch",
        "throw",
        "in",
        "of",
        "self",
        "Self",
        "true",
        "false",
        "None",
        "null",
        "nil",
        "func",
        "package",
        "go",
        "with",
        "yield",
        "lambda",
        "where",
        "public",
        "private",
        "void",
        "int",
    ];
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut chars = line.chars().peekable();
    let push_word = |w: &mut String, spans: &mut Vec<Span<'static>>| {
        if w.is_empty() {
            return;
        }
        let s = if KW.contains(&w.as_str()) {
            Style::default().fg(Color::Rgb(200, 140, 220))
        } else if w
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
        {
            Style::default().fg(Color::Rgb(240, 160, 110))
        } else {
            base
        };
        spans.push(Span::styled(std::mem::take(w), s));
    };
    while let Some(c) = chars.next() {
        if c == '"' || c == '\'' || c == '`' {
            push_word(&mut buf, &mut spans);
            let mut s = String::from(c);
            let mut esc = false;
            for d in chars.by_ref() {
                s.push(d);
                if esc {
                    esc = false;
                } else if d == '\\' {
                    esc = true;
                } else if d == c {
                    break;
                }
            }
            spans.push(Span::styled(
                s,
                Style::default().fg(Color::Rgb(150, 200, 120)),
            ));
        } else if c.is_alphanumeric() || c == '_' {
            buf.push(c);
        } else {
            push_word(&mut buf, &mut spans);
            spans.push(Span::styled(c.to_string(), base));
        }
    }
    push_word(&mut buf, &mut spans);
    spans
}

/// Parse text containing ANSI SGR sequences into styled lines.
pub fn ansi_lines(s: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut style = Style::default();
    for raw in s.split('\n') {
        let mut spans = Vec::new();
        let mut buf = String::new();
        let mut it = raw.chars().peekable();
        while let Some(c) = it.next() {
            if c == '\x1b' && it.peek() == Some(&'[') {
                it.next();
                let mut code = String::new();
                while let Some(&d) = it.peek() {
                    it.next();
                    if d.is_ascii_alphabetic() {
                        if d == 'm' {
                            if !buf.is_empty() {
                                spans.push(Span::styled(std::mem::take(&mut buf), style));
                            }
                            style = apply_sgr(style, &code);
                        }
                        break;
                    }
                    code.push(d);
                }
            } else if c == '\t' {
                buf.push_str("    ");
            } else if c != '\r' {
                buf.push(c);
            }
        }
        if !buf.is_empty() {
            spans.push(Span::styled(buf, style));
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn apply_sgr(mut st: Style, code: &str) -> Style {
    let parts: Vec<u16> = code.split(';').map(|p| p.parse().unwrap_or(0)).collect();
    let mut i = 0;
    while i < parts.len() {
        match parts[i] {
            0 => st = Style::default(),
            1 => st = st.add_modifier(Modifier::BOLD),
            2 => st = st.add_modifier(Modifier::DIM),
            3 => st = st.add_modifier(Modifier::ITALIC),
            4 => st = st.add_modifier(Modifier::UNDERLINED),
            22 => st = st.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => st = st.remove_modifier(Modifier::ITALIC),
            30..=37 => st = st.fg(Color::Indexed((parts[i] - 30) as u8)),
            90..=97 => st = st.fg(Color::Indexed((parts[i] - 90 + 8) as u8)),
            40..=47 => st = st.bg(Color::Indexed((parts[i] - 40) as u8)),
            39 => st.fg = None,
            49 => st.bg = None,
            38 | 48 => {
                let fg = parts[i] == 38;
                let color = match parts.get(i + 1) {
                    Some(5) => {
                        let c = Color::Indexed(*parts.get(i + 2).unwrap_or(&0) as u8);
                        i += 2;
                        Some(c)
                    }
                    Some(2) => {
                        let c = Color::Rgb(
                            *parts.get(i + 2).unwrap_or(&0) as u8,
                            *parts.get(i + 3).unwrap_or(&0) as u8,
                            *parts.get(i + 4).unwrap_or(&0) as u8,
                        );
                        i += 4;
                        Some(c)
                    }
                    _ => None,
                };
                if let Some(c) = color {
                    st = if fg { st.fg(c) } else { st.bg(c) };
                }
            }
            _ => {}
        }
        i += 1;
    }
    if let Some(bg) = st.bg {
        st.bg = Some(super::theme::ansi_bg(bg));
    }
    // NO_COLOR: keep bold and the like, drop the tools' colors.
    if super::theme::no_color() {
        st.fg = None;
        st.bg = None;
    }
    st
}

/// Whether a whitespace-delimited word starts an `http(s)://` URL.
fn is_url(word: &[(char, Style)]) -> bool {
    let head: String = word.iter().take(8).map(|(c, _)| *c).collect();
    head.starts_with("http://") || head.starts_with("https://")
}

/// Where a long URL may be cut: before a new query parameter, fragment or
/// scope (`&`, `#`, `?`), and after a path's `/`.
fn url_breaks(word: &[(char, Style)]) -> Vec<usize> {
    word.iter()
        .enumerate()
        .filter_map(|(k, (c, _))| match c {
            '?' | '#' | '&' => Some(k),
            '/' => Some(k + 1),
            _ => None,
        })
        .collect()
}

/// Greedy word-wrap a styled line to `width` columns; continuation lines are
/// indented by `indent` spaces.
pub fn wrap(line: &Line<'static>, width: usize, indent: usize) -> Vec<Line<'static>> {
    let width = width.max(8);
    let indent = if indent * 2 > width { 0 } else { indent };
    if line.width() <= width {
        return vec![line.clone()];
    }
    // Flatten into (char, style).
    let cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|s| {
            let st = line.style.patch(s.style);
            s.content.chars().map(move |c| (c, st))
        })
        .collect();
    let mut out: Vec<Vec<(char, Style)>> = Vec::new();
    let mut cur: Vec<(char, Style)> = Vec::new();
    let mut cur_w = 0;
    let mut i = 0;
    while i < cells.len() {
        // Next word = run of non-spaces + trailing spaces.
        let start = i;
        while i < cells.len() && cells[i].0 != ' ' {
            i += 1;
        }
        while i < cells.len() && cells[i].0 == ' ' {
            i += 1;
        }
        let word = &cells[start..i];
        let ww: usize = word.iter().map(|(c, _)| c.width().unwrap_or(0)).sum();
        let ww_trim: usize = word
            .iter()
            .rev()
            .skip_while(|(c, _)| *c == ' ')
            .map(|(c, _)| c.width().unwrap_or(0))
            .sum();
        let avail = width - if out.is_empty() { 0 } else { indent };
        if cur_w + ww_trim > avail && cur_w > 0 {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        let avail = width - if out.is_empty() { 0 } else { indent };
        if ww_trim > avail {
            // Long word: split it. A URL is cut at its own separators, so a
            // query string is broken between parameters (the line then starts
            // with `&key=…`) and never inside a `%XX` escape, which would
            // leave two halves that mean nothing on their own.
            let breaks = if is_url(word) {
                url_breaks(word)
            } else {
                Vec::new()
            };
            let mut at = 0;
            while at < word.len() {
                let avail = width - if out.is_empty() { 0 } else { indent };
                let room = avail.saturating_sub(cur_w);
                let mut fit = 0;
                let mut fit_w = 0;
                while at + fit < word.len() {
                    let cw = word[at + fit].0.width().unwrap_or(0);
                    if fit_w + cw > room {
                        break;
                    }
                    fit_w += cw;
                    fit += 1;
                }
                if at + fit == word.len() {
                    cur.extend_from_slice(&word[at..]);
                    cur_w += fit_w;
                    break;
                }
                if fit == 0 {
                    if cur_w == 0 {
                        // A single glyph wider than the line: take it anyway
                        // rather than looping without progress.
                        fit = 1;
                    } else {
                        out.push(std::mem::take(&mut cur));
                        cur_w = 0;
                        continue;
                    }
                }
                let mut cut = at + fit;
                if let Some(&b) = breaks.iter().rfind(|&&b| b > at && b <= cut) {
                    cut = b;
                }
                while cut > at
                    && cut < word.len()
                    && (word[cut - 1].0 == '%'
                        || (cut >= at + 2
                            && word[cut - 2].0 == '%'
                            && word[cut - 1].0.is_ascii_hexdigit()))
                {
                    cut -= 1;
                }
                if cut == at {
                    cut = at + fit;
                }
                cur.extend_from_slice(&word[at..cut]);
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
                at = cut;
            }
        } else {
            cur.extend_from_slice(word);
            cur_w += ww;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out.into_iter()
        .enumerate()
        .map(|(n, cells)| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            if n > 0 && indent > 0 {
                spans.push(Span::raw(" ".repeat(indent)));
            }
            let mut buf = String::new();
            let mut st: Option<Style> = None;
            for (c, s) in cells {
                if st != Some(s) {
                    if let Some(prev) = st {
                        spans.push(Span::styled(std::mem::take(&mut buf), prev));
                    }
                    st = Some(s);
                }
                buf.push(c);
            }
            if let Some(s) = st {
                let t = buf.trim_end().to_string();
                if !t.is_empty() {
                    spans.push(Span::styled(t, s));
                }
            }
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn wraps_words() {
        let l = Line::raw("the quick brown fox jumps over the lazy dog");
        let w = wrap(&l, 15, 2);
        assert!(w.len() >= 3);
        assert!(w.iter().all(|x| x.width() <= 15));
        assert!(text(&w[1]).starts_with("  "));
    }

    #[test]
    fn renders_lists_and_code() {
        let md = render_markdown("# Title\n\n- one\n- two `x`\n\n```rust\nfn a() {}\n```\n");
        let all: Vec<String> = md.iter().map(|l| text(&l.line)).collect();
        assert!(all.iter().any(|l| l.contains("Title")));
        assert!(all.iter().any(|l| l.contains("• one")));
        assert!(all.iter().any(|l| l.contains("fn a() {}")));
    }

    #[test]
    fn reads_the_terminal_background_reply() {
        let dark = parse_osc11("\x1b]11;rgb:1e1e/1e1e/1e1e\x07").unwrap();
        assert!(luminance(dark) < 0.5);
        let light = parse_osc11("\x1b]11;rgb:ffff/ffff/ffff\x1b\\").unwrap();
        assert!(luminance(light) > 0.5);
        assert!(
            luminance(parse_osc11("\x1b]11;rgb:f/f/f\x07").unwrap()) > 0.9,
            "one hex digit per channel"
        );
        assert!(
            parse_osc11("no reply").is_none() && parse_osc11("\x1b]11;rgb:zz/00/00\x07").is_none()
        );
    }

    #[test]
    fn without_an_answer_the_background_is_dark() {
        // No OSC 11 reply and no COLORFGBG (tmux, ssh): dark, whatever the OS says.
        assert!(!light_from_colorfgbg(None));
        assert!(!light_from_colorfgbg(Some("")));
        assert!(!light_from_colorfgbg(Some("garbage")));
        assert!(!light_from_colorfgbg(Some("15;0")));
        assert!(!light_from_colorfgbg(Some("7;default;8")));
        assert!(light_from_colorfgbg(Some("0;15")));
        assert!(light_from_colorfgbg(Some("0;default;7")));
        assert!(light_from_colorfgbg(Some("0;11")));
    }

    #[test]
    fn plain_code_uses_the_terminal_foreground() {
        let plain = syntect::highlighting::Color {
            r: 192,
            g: 197,
            b: 206,
            a: 255,
        };
        assert_eq!(syntect_fg(plain, Some(plain)), Color::Reset);
        let kw = syntect::highlighting::Color {
            r: 180,
            g: 142,
            b: 173,
            a: 255,
        };
        assert_eq!(syntect_fg(kw, Some(plain)), Color::Rgb(180, 142, 173));
        // The real theme: untokenised text comes out in the terminal's colour.
        let lines = highlight_block("let x = 1;", "rust");
        assert!(
            lines[0].iter().any(|s| s.style.fg == Some(Color::Reset)),
            "{lines:?}"
        );
    }

    #[test]
    fn headings_have_a_hierarchy() {
        let md = render_markdown("# One\n\n## Two\n\n### Three\n");
        let style = |i: usize| {
            md[i]
                .line
                .spans
                .iter()
                .find(|s| !s.content.trim().is_empty())
                .unwrap()
                .style
        };
        let accent = pal().accent;
        assert_eq!(style(0).fg, Some(accent));
        assert_eq!(style(2).fg, Some(accent));
        assert_ne!(style(4).fg, Some(accent));
        assert!(!style(4).add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn keys_typed_around_the_background_reply_are_kept() {
        let (reply, typed) = split_osc11(b"fix the\x1b]11;rgb:0/0/0\x07 bug");
        assert_eq!(reply.as_deref(), Some("\x1b]11;rgb:0/0/0\x07"));
        assert_eq!(typed, b"fix the bug");
        let (reply, typed) = split_osc11(b"ab\x1b]11;rgb:f/f/f\x1b\\c");
        assert!(reply.is_some());
        assert_eq!(typed, b"abc");
        // No reply (yet): it's all typing, except a partial reply.
        assert_eq!(split_osc11(b"hello"), (None, b"hello".to_vec()));
        assert_eq!(split_osc11(b"hi\x1b]11;rgb:0"), (None, b"hi".to_vec()));
    }

    #[test]
    fn keeps_single_line_breaks() {
        let md = render_markdown("alpha: 1\nbeta: 2\ngamma: 3");
        let all: Vec<String> = md.iter().map(|l| text(&l.line)).collect();
        assert_eq!(all, ["alpha: 1", "beta: 2", "gamma: 3"]);
    }

    #[test]
    fn aligns_table_columns() {
        let md = render_markdown(
            "| | sessions | cost |\n|---|---|---|\n| This project, last 7 days | 3 | $0.00 |\n| All | 12 | $1.50 |\n",
        );
        let all: Vec<String> = md
            .iter()
            .map(|l| text(&l.line))
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(all[0], "                          │ sessions │ cost");
        assert_eq!(all[1], "──────────────────────────┼──────────┼──────");
        assert_eq!(all[2], "This project, last 7 days │ 3        │ $0.00");
        assert_eq!(all[3], "All                       │ 12       │ $1.50");
    }

    #[test]
    fn parses_ansi() {
        let l = ansi_lines("\x1b[1mbold\x1b[0m plain\nnext");
        assert_eq!(l.len(), 2);
        assert!(l[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    fn is_link(s: &Span) -> bool {
        s.style.fg == Some(pal().link) && s.style.add_modifier.contains(Modifier::UNDERLINED)
    }

    #[test]
    fn bare_urls_read_as_links() {
        let md = render_markdown("open https://example.com/a?b=1, then continue");
        let spans = &md[0].line.spans;
        let url = spans
            .iter()
            .find(|s| s.content.contains("example.com"))
            .unwrap();
        assert!(is_link(url), "{url:?}");
        // Sentence punctuation trailing the URL is not part of the link.
        assert_eq!(url.content.as_ref(), "https://example.com/a?b=1");
        assert!(
            spans
                .iter()
                .any(|s| s.content.starts_with(',') && !is_link(s))
        );
    }

    #[test]
    fn a_paren_the_url_did_not_open_stays_outside_it() {
        let md = render_markdown("(see https://example.com/x)");
        let url = md[0]
            .line
            .spans
            .iter()
            .find(|s| s.content.starts_with("https://"))
            .unwrap();
        assert!(is_link(url));
        assert_eq!(url.content.as_ref(), "https://example.com/x");
    }

    #[test]
    fn a_plain_http_word_is_not_a_link() {
        let md = render_markdown("the http protocol, not a scheme");
        assert!(md[0].line.spans.iter().all(|s| !is_link(s)));
    }

    #[test]
    fn markdown_link_text_is_not_styled_twice() {
        // A bare URL written as Markdown link text must keep the Markdown link's
        // styling rather than gain a second, nested link span.
        let md = render_markdown("[https://example.com/x](https://example.com/x)");
        let spans = &md[0].line.spans;
        assert_eq!(spans.iter().filter(|s| is_link(s)).count(), 1, "{spans:?}");
        assert_eq!(text(&md[0].line), "https://example.com/x");
    }

    #[test]
    fn long_urls_break_between_parameters() {
        let url = "https://dash.cloudflare.com/oauth2/auth?response_type=code&client_id=abcdef123&redirect_uri=http%3A%2F%2Flocalhost%3A8762%2Foauth%2Fcallback&scope=k2.read%20k2.write";
        let w = wrap(&Line::raw(url), 40, 0);
        assert!(w.len() > 3);
        assert!(
            w.iter().all(|l| l.width() <= 40),
            "{:?}",
            w.iter().map(text).collect::<Vec<_>>()
        );
        assert_eq!(w.iter().map(text).collect::<String>(), url);
        // Cuts land before a `&`, so continuation lines start with the key.
        assert!(
            w.iter().skip(1).any(|l| text(l).starts_with('&')),
            "{:?}",
            w.iter().map(text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_wrap_never_splits_a_percent_escape() {
        let url = "https://example.com/cb?redirect_uri=http%3A%2F%2Flocalhost%3A8762%2Foauth%2Fcallback&scope=read";
        for width in 12..60 {
            let w = wrap(&Line::raw(url), width, 0);
            let mut at = 0;
            for l in &w {
                at += l.width();
                let head = &url[..at];
                if let Some(i) = head.rfind('%') {
                    let after = &head[i + 1..];
                    assert!(
                        !(after.len() < 2 && after.chars().all(|c| c.is_ascii_hexdigit())),
                        "width {width} splits %{after:?} at {at}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_short_url_is_left_whole() {
        let l = Line::raw("see https://example.com/x ok");
        let w = wrap(&l, 60, 0);
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn link_runs_find_a_bare_url() {
        let md = render_markdown("open https://example.com/a?b=1, then");
        let runs = link_runs(&md[0].line);
        assert_eq!(runs.len(), 1, "{:?}", runs.len());
        assert_eq!(runs[0].text, "https://example.com/a?b=1");
        // "open " is five columns wide.
        assert_eq!((runs[0].start, runs[0].end), (5, 30));
        assert!(runs[0].dest.is_none());
    }

    #[test]
    fn link_runs_give_a_label_its_destination() {
        let md = render_markdown("see [docs](https://example.com/x) now");
        let runs = link_runs(&md[0].line);
        let label = runs.iter().find(|r| r.text == "docs").unwrap();
        assert_eq!(label.dest.as_deref(), Some("https://example.com/x"));
        // The dim URL shown after the label is clickable on its own.
        let shown: Vec<_> = runs
            .iter()
            .filter(|r| r.text == "https://example.com/x")
            .collect();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].dest.as_deref(), Some("https://example.com/x"));
    }

    #[test]
    fn a_wrapped_url_arrives_as_link_fragments() {
        let url = "https://example.com/a?b=1&c=2&d=3&e=4";
        let md = render_markdown(url);
        let wrapped = wrap(&md[0].line, 20, 0);
        assert!(wrapped.len() > 1);
        let text: String = wrapped
            .iter()
            .map(|l| link_runs(l).into_iter().map(|r| r.text).collect::<String>())
            .collect();
        assert_eq!(text, url);
    }
}
