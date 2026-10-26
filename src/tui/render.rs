//! Rendering a transcript block, a subagent transcript, and the footer: the
//! text the model produced, laid out for the terminal.

use super::stream::*;
use super::*;

/// A subagent's transcript as the main one would show it: its task, its
/// text and its tool calls, with a header naming it.
pub(super) fn agent_transcript(
    a: &crate::agent::live::LiveAgent,
    width: usize,
    expand_all: bool,
) -> Vec<Line<'static>> {
    use crate::agent::live::LogItem;
    let state = match a.ended {
        None => "running".to_string(),
        Some((_, true)) => "done".to_string(),
        Some((_, false)) => "failed".to_string(),
    };
    let mut head = format!(
        "{} · {} · {} · {}",
        a.kind,
        a.id,
        state,
        crate::util::duration(a.elapsed().as_secs())
    );
    if a.tokens > 0 {
        head.push_str(&format!(" · ↓ {} tokens", crate::util::short_num(a.tokens)));
    }
    let mut out = vec![
        Line::from(Span::styled(
            format!("⧉ {head}"),
            Style::default()
                .fg(pal().accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            if a.background {
                "  type to message this agent · esc or the main row returns to the main conversation"
            } else {
                "  esc or the main row returns to the main conversation"
            },
            Style::default().fg(pal().dim),
        )),
        Line::raw(""),
    ];
    for item in &a.log {
        let kind = match item {
            LogItem::Prompt(p) => Kind::User(p.clone()),
            LogItem::Text(t) if t.trim().is_empty() => continue,
            LogItem::Text(t) => Kind::Assistant(t.trim().to_string()),
            LogItem::Tool {
                id,
                label,
                started,
                done,
            } => Kind::Tool(ToolBlock {
                id: id.clone(),
                label: label.clone(),
                display: done.as_ref().map(|d| d.2.clone()),
                content: None,
                is_error: done.as_ref().is_some_and(|d| d.1),
                children: vec![],
                started: *started,
                elapsed: done.as_ref().map(|d| d.0),
                progress: None,
            }),
        };
        out.extend(render_block(&Block::new(kind), width, expand_all, false));
        out.push(Line::raw(""));
    }
    out
}

pub(super) fn render_block(
    b: &Block,
    width: usize,
    expand_all: bool,
    streaming: bool,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let w = width.max(10);
    let expanded = b.expanded || expand_all;
    let push_wrapped = |out: &mut Vec<Line<'static>>, l: Line<'static>, indent: usize| {
        out.extend(markdown::wrap(&l, w, indent))
    };
    match &b.kind {
        Kind::Welcome { model, cwd } => {
            let st = Style::default().fg(pal().accent);
            let inner = w.min(70).saturating_sub(2);
            out.push(Line::styled(format!("╭{}╮", "─".repeat(inner)), st));
            let row = |s: Vec<Span<'static>>| {
                let used: usize = s.iter().map(|x| x.width()).sum();
                let mut v = vec![Span::styled("│ ", st)];
                v.extend(s);
                v.push(Span::raw(" ".repeat(inner.saturating_sub(used + 2))));
                v.push(Span::styled(" │", st));
                Line::from(v)
            };
            // A simple mark, not block art: block glyphs render inconsistently
            // across terminals and fonts, and usually read as noise.
            let bold = Style::default().add_modifier(Modifier::BOLD);
            let dim = Style::default().fg(pal().dim);
            out.push(row(vec![]));
            out.push(row(vec![
                Span::raw(" "),
                Span::styled("❖ ", st),
                Span::styled("chanto", bold),
                Span::styled(format!(" v{}", env!("CARGO_PKG_VERSION")), dim),
            ]));
            let model = if model.is_empty() || model == "none" {
                "none yet · /model picks one".to_string()
            } else {
                model.clone()
            };
            out.push(row(vec![Span::styled(
                format!(
                    "   model  {}",
                    crate::util::one_line(&model, inner.saturating_sub(14))
                ),
                dim,
            )]));
            // The end of a long path is the part that says which project this is.
            out.push(row(vec![Span::styled(
                format!(
                    "   cwd    {}",
                    crate::util::tail_chars(cwd, inner.saturating_sub(14))
                ),
                dim,
            )]));
            out.push(row(vec![Span::styled("   /help for commands", dim)]));
            out.push(row(vec![]));
            out.push(Line::styled(format!("╰{}╯", "─".repeat(inner)), st));
            out.push(Line::raw(""));
            // As many tips as fit on one line, rather than one cut off mid-word.
            let tips = fit_parts(
                &[
                    "shift+tab cycles modes (auto mode runs autonomously)",
                    "@ attaches files",
                    "! runs shell",
                    "ctrl+o expands output",
                ],
                width.saturating_sub(7),
            );
            out.push(Line::styled(
                format!(" Try: {tips}"),
                Style::default().fg(pal().dim),
            ));
        }
        Kind::User(t) => {
            let st = Style::default().add_modifier(Modifier::BOLD);
            for (i, l) in t.lines().enumerate() {
                let p = if i == 0 { "> " } else { "  " };
                push_wrapped(
                    &mut out,
                    Line::from(vec![
                        Span::styled(p, Style::default().fg(pal().dim)),
                        Span::styled(l.to_string(), st),
                    ]),
                    2,
                );
            }
        }
        Kind::Assistant(t) => {
            let t = &t[..crate::llm::tool_call_start(t)];
            if streaming {
                // The same rendering `stream_into` builds up piece by piece.
                let mut done = 0;
                while let Some(end) = next_md_boundary(t, done) {
                    push_md(&mut out, &t[done..end], w);
                    done = end;
                }
                push_md_tail(&mut out, &t[done..], w);
            } else {
                let md = markdown::render_markdown(t);
                for (i, l) in md.into_iter().enumerate() {
                    let mut spans = vec![if i == 0 {
                        Span::raw("❖ ")
                    } else {
                        Span::raw("  ")
                    }];
                    spans.extend(l.line.spans);
                    push_wrapped(&mut out, Line::from(spans), l.indent + 2);
                }
            }
        }
        Kind::Thinking { text, elapsed, .. } => {
            let st = Style::default()
                .fg(pal().dim)
                .add_modifier(Modifier::ITALIC);
            // Keep reasoning out of the transcript by default:
            // show that it happened (and for how long), not the text streaming
            // past. ctrl+o or a click expands it.
            let label = thinking_label(*elapsed);
            out.push(Line::styled(label, st));
            if expanded {
                for l in text.trim().lines() {
                    push_wrapped(
                        &mut out,
                        Line::from(vec![Span::raw("  "), Span::styled(l.to_string(), st)]),
                        2,
                    );
                }
            }
        }
        Kind::Tool(t) => {
            let (dot, color) = match (t.elapsed, t.is_error) {
                (None, _) => {
                    let on = (view_time::elapsed(t.started).as_millis() / 500).is_multiple_of(2);
                    (if on { "▸" } else { " " }, pal().dim)
                }
                (Some(_), false) => ("▸", pal().success),
                (Some(_), true) => ("▸", pal().error),
            };
            let (name, rest) = match t.label.find('(') {
                Some(i) => (t.label[..i].to_string(), t.label[i..].to_string()),
                None => (t.label.clone(), String::new()),
            };
            let mut header = vec![
                Span::styled(format!("{dot} "), Style::default().fg(color)),
                Span::styled(name, Style::default().add_modifier(Modifier::BOLD)),
            ];
            let rest = if expanded {
                rest
            } else {
                crate::util::one_line(&rest, w.saturating_sub(20))
            };
            // The command or path stands out in the inline-code colour rather
            // than blending into the dim chrome.
            header.push(Span::styled(rest, Style::default().fg(pal().code)));
            let secs = t
                .elapsed
                .unwrap_or_else(|| view_time::elapsed(t.started))
                .as_secs();
            if secs >= 3 {
                header.push(Span::styled(
                    format!(" · {}", crate::util::duration(secs)),
                    Style::default().fg(pal().dim),
                ));
            }
            push_wrapped(&mut out, Line::from(header), 2);
            // Subagent activity.
            if !t.children.is_empty() {
                let shown: Vec<&(String, String, Option<bool>)> = if expanded || t.elapsed.is_none()
                {
                    let skip = if expanded {
                        0
                    } else {
                        t.children.len().saturating_sub(3)
                    };
                    t.children.iter().skip(skip).collect()
                } else {
                    vec![]
                };
                if !expanded && t.children.len() > shown.len() && t.elapsed.is_none() {
                    out.push(Line::styled(
                        format!(
                            "     … {}",
                            crate::util::plural(t.children.len() - shown.len(), "earlier tool use")
                        ),
                        Style::default().fg(pal().dim),
                    ));
                }
                for (_, label, st) in shown {
                    let c = match st {
                        None => pal().dim,
                        Some(false) => pal().success,
                        Some(true) => pal().error,
                    };
                    push_wrapped(
                        &mut out,
                        Line::from(vec![
                            Span::styled("  ↳  ", Style::default().fg(pal().dim)),
                            Span::styled("· ", Style::default().fg(c)),
                            Span::styled(label.clone(), Style::default().fg(pal().dim)),
                        ]),
                        5,
                    );
                }
            }
            match &t.display {
                None => {
                    if let Some(p) = &t.progress {
                        for (i, l) in p.lines().enumerate() {
                            let pre = if i == 0 { "  ↳  " } else { "     " };
                            out.push(Line::from(vec![
                                Span::styled(pre, Style::default().fg(pal().dim)),
                                Span::styled(
                                    crate::util::one_line(l, w.saturating_sub(6)),
                                    Style::default().fg(pal().dim),
                                ),
                            ]));
                        }
                    } else if t.children.is_empty() {
                        out.push(Line::styled(
                            "  ↳  Running…",
                            Style::default().fg(pal().dim),
                        ));
                    }
                }
                Some(d) => {
                    // Files a Bash command changed: a summary line, with the
                    // diffs when expanded.
                    let (d, changes) = match d.split_once(crate::tools::bash::CHANGES_MARK) {
                        Some((main, changes)) => (main.trim_end().to_string(), Some(changes)),
                        None => (d.clone(), None),
                    };
                    let d = &d;
                    let content = t.content.as_deref().unwrap_or("");
                    let summary_only = !matches!(
                        t.label.split('(').next().unwrap_or(""),
                        "Edit" | "MultiEdit" | "Write" | "TodoWrite"
                    );
                    let has_more =
                        summary_only && !content.is_empty() && content.trim() != d.trim();
                    // A display that is just the first lines of the output (Bash)
                    // gives way to the whole output when expanded, rather than
                    // showing the same lines twice.
                    let preview = expanded && has_more && display_is_preview(d, content);
                    let shown = if preview { content } else { d.as_str() };
                    let lines: Vec<Line<'static>> = {
                        let mut v = markdown::ansi_lines(shown.trim_end());
                        while v.last().map(|l| l.width() == 0).unwrap_or(false) {
                            v.pop();
                        }
                        v
                    };
                    let limit = if preview {
                        2000
                    } else if expanded {
                        usize::MAX
                    } else {
                        6
                    };
                    // One style for every row: the tool's own colors (diffs) still show.
                    let style = if t.is_error {
                        Style::default().fg(pal().error)
                    } else {
                        Style::default().fg(pal().dim)
                    };
                    for (i, l) in lines.iter().enumerate().take(limit) {
                        let mut spans = vec![Span::styled(
                            if i == 0 { "  ↳  " } else { "     " },
                            Style::default().fg(pal().dim),
                        )];
                        spans.extend(
                            l.spans
                                .iter()
                                .map(|s| Span::styled(s.content.to_string(), style.patch(s.style))),
                        );
                        let from = out.len();
                        push_wrapped(&mut out, Line::from(spans), 5);
                        pad_highlighted(&mut out[from..], w);
                    }
                    let hidden = lines.len().saturating_sub(limit);
                    if hidden > 0 && !preview {
                        out.push(Line::styled(
                            format!(
                                "     … +{} (ctrl+o to expand)",
                                crate::util::plural(hidden, "line")
                            ),
                            Style::default().fg(pal().dim),
                        ));
                    } else if !expanded && has_more && changes.is_none() {
                        // Inline hint on the last summary line.
                        if let Some(last) = out.last_mut() {
                            last.spans.push(Span::styled(
                                " (ctrl+o to expand)",
                                Style::default().fg(faint()),
                            ));
                        }
                    }
                    if expanded && has_more && !preview {
                        out.push(Line::styled(
                            "     ─── output ───",
                            Style::default().fg(pal().dim),
                        ));
                        for l in content.lines().take(2000) {
                            push_wrapped(
                                &mut out,
                                Line::from(vec![
                                    Span::raw("     "),
                                    Span::styled(
                                        l.replace('\t', "    "),
                                        Style::default().fg(pal().dim),
                                    ),
                                ]),
                                5,
                            );
                        }
                    }
                    if let Some(changes) = changes {
                        let lines = markdown::ansi_lines(changes.trim_end());
                        let style = Style::default().fg(pal().dim);
                        for (i, l) in
                            lines
                                .iter()
                                .enumerate()
                                .take(if expanded { usize::MAX } else { 1 })
                        {
                            let mut spans = vec![Span::styled(
                                if i == 0 && d.trim().is_empty() {
                                    "  ↳  "
                                } else {
                                    "     "
                                },
                                style,
                            )];
                            spans.extend(l.spans.iter().map(|s| {
                                Span::styled(s.content.to_string(), style.patch(s.style))
                            }));
                            if i == 0 && !expanded && lines.len() > 1 {
                                spans.push(Span::styled(
                                    " (ctrl+o to expand)",
                                    Style::default().fg(faint()),
                                ));
                            }
                            let from = out.len();
                            push_wrapped(&mut out, Line::from(spans), 5);
                            pad_highlighted(&mut out[from..], w);
                        }
                    }
                }
            }
        }
        Kind::Note(s, c) => {
            for l in s.lines() {
                push_wrapped(
                    &mut out,
                    Line::styled(l.to_string(), Style::default().fg(*c)),
                    2,
                );
            }
        }
        Kind::Markdown(s) => {
            for l in markdown::render_markdown(s) {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(l.line.spans);
                push_wrapped(&mut out, Line::from(spans), l.indent + 2);
            }
        }
    }
    out
}

/// Whether a tool's display is a preview of its output (its first lines,
/// perhaps followed by a "… +N lines" note) rather than a summary of it.
pub(super) fn display_is_preview(display: &str, content: &str) -> bool {
    let first = display.lines().next().unwrap_or("");
    !first.trim().is_empty() && content.lines().next() == Some(first)
}

/// Rows with a background (diff lines) run the full `width`, whether or not
/// they wrapped.
pub(super) fn pad_highlighted(lines: &mut [Line<'static>], width: usize) {
    for line in lines {
        if let Some(bg) = line
            .spans
            .last()
            .and_then(|s| s.style.bg)
            .filter(|bg| *bg != Color::Reset)
        {
            let pad = width.saturating_sub(line.width());
            if pad > 0 {
                line.spans
                    .push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
            }
        }
    }
}

/// `s` cut to `width` columns at a word boundary, ending in "…" when
/// anything was cut. A single word wider than `width` is cut mid-word.
pub(super) fn fit_words(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    if s.width() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for (i, word) in s.split(' ').enumerate() {
        let next = if i == 0 {
            word.to_string()
        } else {
            format!("{out} {word}")
        };
        if next.width() + 1 > width {
            break;
        }
        out = next;
    }
    let out = out
        .trim_end_matches([' ', '·', ',', ':', ';', '('])
        .to_string();
    if out.is_empty() {
        let mut cut = String::new();
        for c in s.chars() {
            if cut.width() + unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) + 1 > width {
                break;
            }
            cut.push(c);
        }
        return format!("{cut}…");
    }
    format!("{out}…")
}

/// Local reset time for the footer; do not imply that stale quota data refreshed.
pub(super) fn quota_reset(at: i64, now: i64) -> Option<String> {
    if at <= now {
        return Some("reset due".into());
    }
    let at = view_time::local_time(at)?;
    let now = view_time::local_time(now)?;
    let format = if at.date_naive() == now.date_naive() {
        "resets at %-I:%M %p"
    } else {
        "resets %b %-d at %-I:%M %p"
    };
    Some(at.format(format).to_string())
}

/// The footer's left text: the label, then the mode's shortcut hint only when
/// it fits whole. The hint is a teaching aid, not a status, so it never pays
/// for itself by cutting the label or pushing a right-side item (the context
/// meter, say) off the line.
pub(super) fn footer_left(label: &str, hint: Option<&str>, room: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    if label.width() > room {
        return fit_words(label, room);
    }
    match hint {
        Some(h) if label.width() + 3 + h.width() <= room => format!("{label} · {h}"),
        _ => label.to_string(),
    }
}

/// Blank columns between the footer's two sides: one reads as a collision.
pub(super) const FOOTER_GAP: usize = 2;

/// Which footer items fit `width` columns: whether the left side shows, and
/// the indices of the right-side items kept. Items are (width, priority); the
/// right ones are joined by " · " and end in a space, and a gap of at least
/// `FOOTER_GAP` columns always separates the two sides. The lowest priority goes first
/// (the later item on a tie); the right side keeps at least one item, and a
/// left side that still doesn't fit is cut with "…" by the caller.
pub(super) fn fit_footer(
    left: (usize, u8),
    right: &[(usize, u8)],
    width: usize,
) -> (bool, Vec<usize>) {
    let mut keep: Vec<usize> = (0..right.len()).collect();
    let mut show_left = left.0 > 0;
    let right_w = |keep: &[usize]| {
        keep.iter().map(|&i| right[i].0).sum::<usize>() + 3 * keep.len().saturating_sub(1) + 1
    };
    loop {
        let need = right_w(&keep) + if show_left { left.0 + FOOTER_GAP } else { 0 };
        if need <= width {
            break;
        }
        // The lowest-priority right item (the later one on a tie), unless
        // the left side matters less.
        let drop = keep
            .iter()
            .enumerate()
            .filter(|_| keep.len() > 1)
            .min_by_key(|&(_, &i)| (right[i].1, std::cmp::Reverse(i)))
            .map(|(k, &i)| (k, right[i].1));
        match drop {
            Some((_, p)) if show_left && left.1 < p => show_left = false,
            Some((k, _)) => {
                keep.remove(k);
            }
            None if show_left && left.1 <= 2 => show_left = false,
            None => break,
        }
    }
    // Truncation, not dropping, for an important left side: keep it while
    // a few columns are left for it.
    if show_left && width < right_w(&keep) + FOOTER_GAP + 4 {
        show_left = false;
    }
    (show_left, keep)
}

/// As many of `parts` as fit `width` columns, joined with " · "; when not even
/// the first fits, it is cut at a word boundary.
pub(super) fn fit_parts(parts: &[&str], width: usize) -> String {
    let mut out = String::new();
    for p in parts {
        let next = if out.is_empty() {
            p.to_string()
        } else {
            format!("{out} · {p}")
        };
        if unicode_width::UnicodeWidthStr::width(next.as_str()) > width {
            break;
        }
        out = next;
    }
    if out.is_empty() {
        fit_words(parts.first().copied().unwrap_or(""), width)
    } else {
        out
    }
}

/// Rebuild transcript blocks from a restored conversation.
pub(super) fn blocks_from_messages(a: &Agent) -> Vec<Block> {
    let mut out = Vec::new();
    let msgs = &a.messages;
    for (i, m) in msgs.iter().enumerate() {
        match m.role {
            Role::User => {
                let t = m
                    .content
                    .split("<system-reminder>")
                    .next()
                    .unwrap_or("")
                    .trim();
                if t.starts_with("[image output") || t.is_empty() {
                    continue;
                }
                if t.starts_with("This session is being continued") {
                    out.push(Block::new(Kind::Note(
                        "❖ (conversation was compacted earlier)".into(),
                        pal().dim,
                    )));
                    continue;
                }
                out.push(Block::new(Kind::User(t.to_string())));
            }
            Role::Assistant => {
                if !m.content.trim().is_empty() {
                    out.push(Block::new(Kind::Assistant(m.content.trim().to_string())));
                }
                for c in &m.tool_calls {
                    let result = msgs[i + 1..]
                        .iter()
                        .find(|x| x.role == Role::Tool && x.tool_call_id.as_deref() == Some(&c.id));
                    let label = a
                        .env
                        .tools
                        .get(&c.name)
                        .map(|t| t.label(&c.arguments))
                        .unwrap_or_else(|| c.name.clone());
                    let content = result.map(|r| r.content.clone()).unwrap_or_default();
                    let first = content.lines().next().unwrap_or("").to_string();
                    out.push(Block::new(Kind::Tool(ToolBlock {
                        id: c.id.clone(),
                        label,
                        display: Some(crate::util::one_line(&first, 120)),
                        is_error: content.starts_with("Error"),
                        content: Some(content),
                        children: vec![],
                        started: Instant::now(),
                        elapsed: Some(Duration::ZERO),
                        progress: None,
                    })));
                }
            }
            _ => {}
        }
    }
    out
}
