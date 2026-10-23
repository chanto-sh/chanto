//! Streaming markdown into the transcript: what is settled, what is still live,
//! and appending text a chunk at a time without re-rendering the whole block.

use super::render::*;
use super::*;

/// The quiet view's mask over the transcript (see [`quiet_mask`]).
#[derive(Debug, Default)]
pub(super) struct QuietMask {
    /// Blocks left out of the view.
    pub(super) hidden: Vec<bool>,
    /// The running turn's first block (the end when no turn runs): hiding from
    /// here on is provisional, since its last reply may be the final one.
    pub(super) hold: usize,
}

impl QuietMask {
    pub(super) fn hides(&self, i: usize) -> bool {
        self.hidden.get(i).copied().unwrap_or(false)
    }
}

/// Quiet view: in each turn (the blocks after a prompt), the assistant's
/// intermediate text, thinking and tool blocks are hidden, and only the turn's
/// last reply shows, once the turn has ended (`running`: the last turn is still
/// going, so its reply isn't known to be the final one yet). Prompts, notes
/// (warnings, errors, spend-guard notices, verification results) and command
/// output always show. Nothing is removed: the blocks and the session keep it all.
pub(super) fn quiet_mask(blocks: &[Block], running: bool) -> QuietMask {
    let mut hidden = vec![false; blocks.len()];
    let last_prompt = blocks.iter().rposition(|b| matches!(b.kind, Kind::User(_)));
    let hold = if running {
        last_prompt.map_or(0, |u| u + 1)
    } else {
        blocks.len()
    };
    let mut start = 0;
    for end in 0..=blocks.len() {
        if end < blocks.len() && !matches!(blocks[end].kind, Kind::User(_)) {
            continue;
        }
        let reply = if running && end == blocks.len() {
            None
        } else {
            blocks[start..end]
                .iter()
                .rposition(|b| matches!(&b.kind, Kind::Assistant(t) if !t.trim().is_empty()))
                .map(|k| start + k)
        };
        for (i, b) in blocks.iter().enumerate().take(end).skip(start) {
            if matches!(
                b.kind,
                Kind::Assistant(_) | Kind::Thinking { .. } | Kind::Tool(_)
            ) && Some(i) != reply
            {
                hidden[i] = true;
            }
        }
        start = end + 1;
    }
    QuietMask { hidden, hold }
}

/// Inline mode: the lines that are final and can go to scrollback, in order,
/// advancing `iv`. Stops at the first block still changing (a running tool,
/// open thinking, or the answer being streamed, whose finished Markdown pieces
/// do go out). Pieces render as the closed answer will, so nothing printed
/// ever needs redrawing.
///
/// The quiet view (`quiet`) skips the blocks it hides, and holds back from the
/// running turn's first hidden block: its reply may turn out to be the final
/// one, which prints when the turn ends. ctrl+o then prints the last turn's
/// hidden steps.
pub(super) fn settle(
    blocks: &[Block],
    open_text: Option<usize>,
    open_think: Option<usize>,
    iv: &mut InlineView,
    width: usize,
    quiet: Option<&QuietMask>,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let w = width.max(10);
    let printed = iv.printed.min(blocks.len());
    if iv.reprint
        && let Some(q) = quiet
    {
        let from = blocks[..printed]
            .iter()
            .rposition(|b| matches!(b.kind, Kind::User(_)))
            .map_or(0, |u| u + 1);
        let steps: Vec<usize> = (from..printed).filter(|&i| q.hides(i)).collect();
        if !steps.is_empty() {
            iv.reprint = false;
            for i in steps {
                out.extend(render_block(&blocks[i], width, false, false));
                out.push(Line::raw(""));
            }
        }
    }
    if std::mem::take(&mut iv.reprint)
        && let Some(b) = blocks[..printed].iter().rev().find(|b| b.expandable())
    {
        out.extend(render_block(b, width, true, false));
        out.push(Line::raw(""));
    }
    while iv.printed < blocks.len() {
        let i = iv.printed;
        let b = &blocks[i];
        let streaming = open_text == Some(i);
        let before = out.len();
        if let Some(q) = quiet.filter(|q| q.hides(i)) {
            if i >= q.hold {
                break;
            }
            iv.printed += 1;
            iv.printed_src = 0;
            continue;
        }
        if let Kind::Assistant(full) = &b.kind {
            let t = &full[..crate::llm::tool_call_start(full)];
            let mut done = iv.printed_src.min(t.len());
            let started = done > 0;
            while let Some(end) = next_md_boundary(t, done) {
                push_md_at(&mut out, &t[done..end], w, done == 0);
                done = end;
            }
            if streaming {
                iv.printed_src = done;
                break;
            }
            push_md_tail_at(&mut out, &t[done..], w, done == 0);
            if started || out.len() > before {
                out.push(Line::raw(""));
            }
        } else if streaming || b.is_running_tool() || open_think == Some(i) {
            break;
        } else {
            out.extend(render_block(b, width, false, false));
            if out.len() > before {
                out.push(Line::raw(""));
            }
        }
        iv.printed += 1;
        iv.printed_src = 0;
    }
    out
}

/// Inline mode: what is still changing (the rest of the answer being
/// streamed, running tools), drawn in the region above the input.
pub(super) fn live(
    blocks: &[Block],
    open_text: Option<usize>,
    iv: &InlineView,
    expand_all: bool,
    width: usize,
    quiet: Option<&QuietMask>,
) -> Vec<Line<'static>> {
    let w = width.max(10);
    let mut out: Vec<Line<'static>> = Vec::new();
    for (i, b) in blocks.iter().enumerate().skip(iv.printed) {
        if quiet.is_some_and(|q| q.hides(i)) {
            continue;
        }
        match &b.kind {
            Kind::Assistant(full) if i == iv.printed => {
                let t = &full[..crate::llm::tool_call_start(full)];
                push_md_tail_at(
                    &mut out,
                    &t[iv.printed_src.min(t.len())..],
                    w,
                    iv.printed_src == 0,
                );
            }
            _ => out.extend(render_block(b, width, expand_all, open_text == Some(i))),
        }
        out.push(Line::raw(""));
    }
    while out.last().is_some_and(|l| l.width() == 0) {
        out.pop();
    }
    out
}

/// Which rows of the measured blocks are on screen. `spans` are (block, rows)
/// in transcript order, ending at the bottom; `scroll` counts rows up from the
/// bottom and is clamped. Returns the visible (block, row-in-block) pairs and
/// the visible range's start and end within the measured rows.
pub(super) fn visible_rows(
    spans: &[(usize, usize)],
    h: usize,
    scroll: &mut usize,
) -> (Vec<(usize, usize)>, usize, usize) {
    let total: usize = spans.iter().map(|s| s.1).sum();
    *scroll = (*scroll).min(total.saturating_sub(h));
    let start = total.saturating_sub(h + *scroll);
    let end = (start + h).min(total);
    let mut rows = Vec::with_capacity(end - start);
    let mut at = 0;
    for &(i, n) in spans {
        if at + n > start && at < end {
            rows.extend((start.max(at) - at..(end - at).min(n)).map(|k| (i, k)));
        }
        at += n;
        if at >= end {
            break;
        }
    }
    (rows, start, end)
}

/// The reply being streamed, rendered as Markdown without redoing the whole
/// reply each frame. Finished blocks (a paragraph, list or closed code fence,
/// up to a safe boundary) are parsed once and kept; only the block still
/// arriving is re-rendered, and an unclosed code fence shows as plain code
/// lines (highlighting it every frame would be quadratic). The layout matches
/// the final render, so nothing jumps when the reply closes. The cache is marked
/// stale (version u64::MAX) so the block gets its full render once it closes.
type BlockCache = Option<(u16, u64, bool, u64, Vec<Line<'static>>)>;
pub(super) fn stream_into(
    kind: &Kind,
    cache: &mut BlockCache,
    state: &mut Option<(usize, u16, usize, usize, usize)>,
    i: usize,
    width: u16,
) -> usize {
    let Kind::Assistant(full) = kind else {
        return 0;
    };
    let t = &full[..crate::llm::tool_call_start(full)];
    let w = (width as usize).max(10);
    let valid = matches!(state, Some((b, bw, done, _, _)) if *b == i && *bw == width && *done <= t.len())
        && matches!(cache, Some((cw, v, ..)) if *v == u64::MAX && *cw == width);
    if !valid {
        *state = Some((i, width, 0, 0, 0));
        *cache = Some((width, u64::MAX, false, 0, Vec::new()));
    }
    let Some((_, _, done, _, partial)) = state.as_mut() else {
        return 0;
    };
    let Some((.., lines)) = cache.as_mut() else {
        return 0;
    };
    lines.truncate(lines.len() - *partial);
    while let Some(end) = next_md_boundary(t, *done) {
        push_md(lines, &t[*done..end], w);
        *done = end;
    }
    let before = lines.len();
    push_md_tail(lines, &t[*done..], w);
    *partial = lines.len() - before;
    lines.len()
}

/// A fence opener (up to three spaces in): its character, run length and indent.
pub(super) fn md_fence(line: &str) -> Option<(char, usize, usize)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    let c = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let n = rest.chars().take_while(|x| *x == c).count();
    (indent <= 3 && n >= 3 && !(c == '`' && rest[n..].contains('`'))).then_some((c, n, indent))
}

pub(super) fn md_fence_closes(line: &str, c: char, n: usize) -> bool {
    let l = line.trim_start_matches(' ');
    line.len() - l.len() <= 3
        && l.chars().take_while(|x| *x == c).count() >= n
        && l.trim_start_matches(c).trim().is_empty()
}

/// The first point after `from` where the streamed Markdown can be split and the
/// part before it rendered for good: the start of a complete top-level line
/// that follows a blank line or a closing unindented fence, outside any fence.
/// Lines that could continue the previous block (indented, list items, quotes,
/// rules) are not split points, so the pieces render as the whole would.
pub(super) fn next_md_boundary(t: &str, from: usize) -> Option<usize> {
    let mut fence: Option<(char, usize, usize)> = None;
    let (mut prev_blank, mut prev_close) = (false, false);
    let mut pos = from;
    while let Some(rel) = t[pos..].find('\n') {
        let line = &t[pos..pos + rel];
        // Reference links and footnotes resolve against definitions anywhere in
        // the reply, so pieces can't be rendered apart once one appears: the
        // rest stays a single piece (slower, but rare).
        if fence.is_none()
            && (line.contains("][")
                || line.contains("[^")
                || (line.trim_start().starts_with('[') && line.contains("]:")))
        {
            return None;
        }
        if fence.is_none() && pos > from && !line.trim().is_empty() {
            let continues = line.starts_with([' ', '\t', '-', '*', '+', '>', '|', '=', '_', '<'])
                || {
                    let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
                    digits > 0 && line[digits..].starts_with(['.', ')'])
                };
            if prev_close || (prev_blank && !continues) {
                return Some(pos);
            }
        }
        prev_close = false;
        match fence {
            Some((c, n, indent)) => {
                if md_fence_closes(line, c, n) {
                    fence = None;
                    prev_close = indent == 0;
                }
            }
            None => fence = md_fence(line),
        }
        prev_blank = fence.is_none() && line.trim().is_empty();
        pos += rel + 1;
    }
    None
}

/// Render a finished piece of the reply after what is already there, spaced as
/// the full render spaces blocks.
pub(super) fn push_md(lines: &mut Vec<Line<'static>>, src: &str, w: usize) {
    let first = lines.is_empty();
    push_md_at(lines, src, w, first);
}

/// `push_md` for a piece that starts the reply (`first`: bullet, no spacer) or not.
pub(super) fn push_md_at(lines: &mut Vec<Line<'static>>, src: &str, w: usize, mut first: bool) {
    let md = markdown::render_markdown(src);
    if md.is_empty() {
        return;
    }
    if !first {
        lines.extend(markdown::wrap(&Line::from(vec![Span::raw("  ")]), w, 2));
    }
    for l in md {
        let mut spans = vec![Span::raw(if first { "❖ " } else { "  " })];
        first = false;
        spans.extend(l.line.spans);
        lines.extend(markdown::wrap(&Line::from(spans), w, l.indent + 2));
    }
}

/// The block still arriving. An unclosed fence is shown as plain code lines,
/// indented as the highlighted render will be.
pub(super) fn push_md_tail(lines: &mut Vec<Line<'static>>, tail: &str, w: usize) {
    let first = lines.is_empty();
    push_md_tail_at(lines, tail, w, first);
}

pub(super) fn push_md_tail_at(
    lines: &mut Vec<Line<'static>>,
    tail: &str,
    w: usize,
    mut first: bool,
) {
    let mut fence: Option<(char, usize, usize)> = None;
    let mut open_at = (0, 0); // (opener line start, code start)
    let mut pos = 0;
    while pos < tail.len() {
        let end = tail[pos..].find('\n').map(|r| pos + r);
        let line = &tail[pos..end.unwrap_or(tail.len())];
        match fence {
            Some((c, n, _)) if end.is_some() && md_fence_closes(line, c, n) => fence = None,
            Some(_) => {}
            None => {
                if let Some(f) = md_fence(line) {
                    fence = Some(f);
                    open_at = (pos, end.map_or(tail.len(), |e| e + 1));
                }
            }
        }
        pos = end.map_or(tail.len(), |e| e + 1);
    }
    let Some((_, _, indent)) = fence else {
        push_md_at(lines, tail, w, first);
        return;
    };
    let n = lines.len();
    push_md_at(lines, &tail[..open_at.0], w, first);
    first &= lines.len() == n;
    let code = tail[open_at.1..].trim_end_matches('\n');
    if code.is_empty() {
        return;
    }
    if !first {
        lines.extend(markdown::wrap(&Line::from(vec![Span::raw("  ")]), w, 2));
    }
    for l in code.split('\n') {
        // Inside a fence indented n spaces, up to n leading spaces are not content.
        let lead = l.len() - l.trim_start_matches(' ').len();
        let l = &l[lead.min(indent)..];
        let bullet = if first { "❖ " } else { "  " };
        first = false;
        lines.extend(markdown::wrap(
            &Line::from(vec![
                Span::raw(bullet),
                Span::raw("  "),
                Span::raw(l.to_string()),
            ]),
            w,
            4,
        ));
    }
}
