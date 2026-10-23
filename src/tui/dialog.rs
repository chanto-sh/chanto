//! Dialog layout helpers: the body rows a dialog shows, the permission box's
//! title and countdown, and its proceed rows.

use super::*;

/// A dialog's body as screen rows, wrapped to `width`. A permission's reason
/// (from a hook or the auto-mode classifier) renders as markdown above the
/// diff or command, so its backticks and emphasis show instead of being
/// flattened into the detail. Shared by the draw and the height estimate so
/// the box is sized to the rows it actually shows.
pub(super) fn dialog_body_lines(
    d: &Dialog,
    body: &str,
    reason: Option<&str>,
    width: usize,
) -> Vec<Line<'static>> {
    let mut v: Vec<Line<'static>> = Vec::new();
    if let Some(r) = reason.filter(|r| !r.is_empty()) {
        v.extend(
            markdown::render_markdown(r)
                .into_iter()
                .flat_map(|l| markdown::wrap(&l.line, width, l.indent)),
        );
        v.push(Line::raw(""));
    }
    if matches!(d, Dialog::Question { .. }) {
        v.extend(
            markdown::render_markdown(body)
                .into_iter()
                .flat_map(|l| markdown::wrap(&l.line, width, l.indent)),
        );
    } else {
        let mut b: Vec<Line<'static>> = markdown::ansi_lines(body)
            .into_iter()
            .flat_map(|l| markdown::wrap(&l, width, 0))
            .collect();
        // Diff rows fill the width, as in the transcript.
        pad_highlighted(&mut b, width);
        v.extend(b);
    }
    v
}

/// How a dialog body of `total` lines fits `rows`: the rows for text, the
/// clamped scroll, and the "more lines" hint, which gets the last row of its
/// own when the body overflows rather than being drawn over the text.
pub(super) fn dialog_body_rows(total: u16, rows: u16, scroll: u16) -> (u16, u16, Option<String>) {
    if total <= rows || rows == 0 {
        return (rows, 0, None);
    }
    let text_h = rows - 1;
    let scroll = scroll.min(total - text_h);
    let below = total - text_h - scroll;
    let hint = if below > 0 {
        format!(
            "… {} (pgdn/pgup to scroll)",
            crate::util::plural(below as usize, "more line")
        )
    } else {
        "(pgup to scroll back)".to_string()
    };
    (text_h, scroll, Some(hint))
}

/// The rows above a dialog's choices: the permission question, with the
/// `e` hint after it when it fits and on a dim row of its own when it
/// doesn't; a blank spacer for other dialogs.
pub(super) fn proceed_rows(permission: bool, editable: bool, width: usize) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthStr;
    const ASK: &str = "Allow this action?";
    const HINT: &str = "e: edit the change in $EDITOR first";
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(pal().dim);
    if !permission {
        return vec![Line::raw("")];
    }
    if !editable {
        return markdown::wrap(&Line::styled(ASK, bold), width, 0);
    }
    if ASK.width() + 3 + HINT.width() <= width {
        return vec![Line::from(vec![
            Span::styled(ASK, bold),
            Span::styled(format!(" ({HINT})"), dim),
        ])];
    }
    let mut rows = markdown::wrap(&Line::styled(ASK, bold), width, 0);
    rows.extend(markdown::wrap(&Line::styled(HINT, dim), width, 0));
    rows
}

/// Append the free-text row to a dialog's option block and return the cell the
/// cursor belongs on. The row is whatever follows what is already there — the
/// question spacer, the numbered options and, for a question, its hint line —
/// so a permission without a hint keeps its cursor on the text it edits.
pub(super) fn push_typing_line(
    lines: &mut Vec<Line<'static>>,
    t: &Input,
    area: Rect,
) -> (u16, u16) {
    let row = area.y + lines.len() as u16;
    lines.push(Line::from(vec![
        Span::styled("   › ", Style::default().fg(pal().accent)),
        Span::raw(t.text.clone()),
    ]));
    let col = unicode_width::UnicodeWidthStr::width(&t.text[..t.cursor]) as u16;
    (area.x + 5 + col, row)
}

/// A permission dialog's title and body, saying each thing once: an edit's
/// path is in the title (`Edit(calc.py)`), so the body drops it; a command
/// is in the body (where long ones wrap), so the title is just `Bash`.
pub(super) fn permission_title_body(label: &str, tool: &str, detail: &str) -> (String, String) {
    let Some(inner) = label
        .strip_prefix(tool)
        .and_then(|r| r.strip_prefix('('))
        .and_then(|r| r.strip_suffix(')'))
    else {
        return (label.to_string(), detail.to_string());
    };
    let first = detail.lines().next().unwrap_or("").trim();
    if tool == "Bash"
        && !first.is_empty()
        && crate::util::one_line(detail, usize::MAX).starts_with(inner.trim_end_matches('…'))
    {
        return (tool.to_string(), detail.to_string());
    }
    if !first.is_empty() && first == inner.trim() {
        let rest = detail.split_once('\n').map(|(_, r)| r).unwrap_or("");
        return (label.to_string(), rest.to_string());
    }
    (label.to_string(), detail.to_string())
}

/// The permission dialog's title, with the auto-deny countdown appended when a
/// deadline is set (auto mode). `now` is passed in so it is testable.
pub(super) fn permission_title_with_countdown(
    title: &str,
    deadline: Option<Instant>,
    now: Instant,
) -> String {
    match deadline {
        // Round up, so it reads "1s" until the prompt actually expires.
        Some(d) => {
            let left = d.saturating_duration_since(now);
            let secs = left.as_secs() + u64::from(left.subsec_millis() > 0);
            format!(" {title} · auto-deny in {secs}s ")
        }
        None => format!(" {title} "),
    }
}
