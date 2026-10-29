//! Mouse handling and selection: drag to copy, double-click for a word, and
//! painting the selection over the finished frame.

use super::*;

impl App {
    pub(super) fn on_mouse(&mut self, m: crossterm::event::MouseEvent) {
        match m.kind {
            MouseEventKind::ScrollUp => {
                self.scroll += 3;
                self.selection = None;
            }
            MouseEventKind::ScrollDown => {
                self.scroll = self.scroll.saturating_sub(3);
                self.selection = None;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // The "↓ jump to latest" pill: back to the bottom, nothing selected.
                let j = self.jump_area;
                if self.scroll > 0
                    && j.width > 0
                    && m.row == j.y
                    && m.column >= j.x
                    && m.column < j.x + j.width
                {
                    self.scroll = 0;
                    self.selection = None;
                    return;
                }
                // Rapid presses in the same cell build a double/triple click; a
                // fourth press starts a fresh run. The first press of a run may
                // have toggled a block, so take that back before selecting.
                let now = Instant::now();
                let repeat = matches!(self.last_click, Some((t, r, c))
                    if now.duration_since(t) < Duration::from_millis(400) && (r, c) == (m.row, m.column));
                self.click_count = if repeat {
                    (self.click_count % 3) + 1
                } else {
                    1
                };
                self.last_click = Some((now, m.row, m.column));
                if self.click_count > 1 {
                    if let Some(bi) = self.click_toggle.take()
                        && let Some(b) = self.blocks.get_mut(bi)
                    {
                        b.expanded = !b.expanded;
                        b.touch();
                    }
                } else {
                    self.click_toggle = None;
                }
                // A click on an agents-panel row opens it.
                let w = self.work_area;
                if w.height > 0 && m.row >= w.y && m.row < w.y + w.height {
                    let i = self.work_start + (m.row - w.y) as usize;
                    let items = self.background_work();
                    let (_, end, _) =
                        agents_panel::window(items.len(), self.work_sel, w.height as usize);
                    if i < end.min(items.len()) {
                        self.open_work_item(&items[i]);
                        self.work_sel = None;
                    }
                    return;
                }
                // The whole frame is selectable — the transcript, the prompt, the
                // panels under it, the footer and dialog text (including
                // questions) — except while the help overlay owns the screen. A
                // plain click opens a link or toggles a transcript block (on
                // Up); a second or third click picks the word or the whole row
                // instead, copying it on release unless `copy_on_select` is off.
                if !self.help {
                    self.selection = Some(Selection {
                        anchor: (m.row, m.column),
                        head: (m.row, m.column),
                    });
                    self.selection_moved = false;
                    let pick = match self.click_count {
                        2 => word_at(&self.row_text, m.row, m.column),
                        3 => line_at(&self.row_text, m.row),
                        _ => None,
                    };
                    if let Some(sel) = pick {
                        self.selection = Some(sel);
                        self.selection_moved = true;
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(s) = &mut self.selection {
                    if (m.row, m.column) != s.anchor {
                        self.selection_moved = true;
                    }
                    s.head = (m.row, m.column);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(mut sel) = self.selection else {
                    return;
                };
                // The release can land on a different cell than the last drag
                // event, so take the final position from the mouse-up itself.
                // A double or triple click picked a whole word or row on the
                // press; releasing over the same cell must not shrink it back
                // to the click point.
                if !release_keeps_pick(self.click_count, self.last_click, m.row, m.column) {
                    sel.head = (m.row, m.column);
                }
                if (m.row, m.column) != sel.anchor {
                    self.selection_moved = true;
                }
                let moved = self.selection_moved;
                self.selection = Some(sel);
                // A press without movement is a click: keep the old behaviour of
                // toggling a block open. A drag that happens to end back on the
                // anchor cell is still a selection.
                if !moved {
                    self.selection = None;
                    if let Some(url) = self.web_link_at(m.row, m.column) {
                        self.open_link = Some(url.to_string());
                        self.click_toggle = None;
                        return;
                    }
                    let a = self.transcript_area;
                    if m.row >= a.y && m.row < a.y + a.height {
                        let r = (m.row - a.y) as usize;
                        if let Some(&bi) = self.row_map.get(r)
                            && bi < self.blocks.len()
                            && self.blocks[bi].expandable()
                        {
                            let b = &mut self.blocks[bi];
                            b.expanded = !b.expanded;
                            b.touch();
                            self.click_toggle = Some(bi);
                        }
                    }
                    return;
                }
                self.click_toggle = None;
                if self.env.config.copy_on_select == Some(false) {
                    let text = selection_text(sel, &self.row_text);
                    if text.trim().is_empty() {
                        self.selection = None;
                        return;
                    }
                    self.hint = Some((
                        format!(
                            "Selected {} line(s); ctrl+shift+c copies",
                            text.lines().count()
                        ),
                        Instant::now(),
                    ));
                    return;
                }
                self.copy_selection();
            }
            _ => {}
        }
    }

    /// Copy the current mouse selection to the clipboard and say so. Leaves the
    /// highlight in place (the next keystroke clears it).
    pub(super) fn copy_selection(&mut self) {
        let Some(sel) = self.selection else {
            return;
        };
        let text = selection_text(sel, &self.row_text);
        if text.trim().is_empty() {
            self.selection = None;
            return;
        }
        self.hint = Some(match crate::commands::copy_to_clipboard(&text) {
            Ok(()) => (
                format!("Copied {} line(s) to the clipboard", text.lines().count()),
                Instant::now(),
            ),
            Err(e) => (
                format!("Copy failed ({e}); selection left highlighted"),
                Instant::now(),
            ),
        });
    }

    /// Record the plain text a widget drew on screen `row`, starting at column
    /// `x`. A later segment on the same row is appended after a gap of spaces,
    /// so the row reads across its full width (the status line's two halves, or
    /// the footer's hint and its right-aligned badges).
    pub(super) fn record_row(&mut self, row: u16, x: u16, text: &str) {
        let Some(slot) = self.row_text.get_mut(row as usize) else {
            return;
        };
        match slot {
            Some((sx, s)) => {
                let end = *sx as usize + unicode_width::UnicodeWidthStr::width(s.as_str());
                if (x as usize) > end {
                    s.push_str(&" ".repeat(x as usize - end));
                }
                s.push_str(text);
            }
            None => *slot = Some((x, text.to_string())),
        }
    }

    /// Paint the current selection over the finished frame. Reading the buffer
    /// back means every widget highlights without each having to know about the
    /// selection, and the plain text recorded while drawing keeps the copy exact.
    pub(super) fn paint_selection(&self, f: &mut Frame, sel: Selection) {
        let style = selection_style();
        for rect in selection_rects(sel, &self.row_text) {
            f.buffer_mut().set_style(rect, style);
        }
    }
}

/// Style for selected text. The foreground is overridden too: keeping a span's
/// own color under a highlight made dim greys and reds nearly unreadable.
pub(super) fn selection_style() -> Style {
    if theme::no_color() {
        return Style::default().add_modifier(Modifier::REVERSED);
    }
    Style::default().fg(highlight_fg()).bg(highlight_bg())
}

/// The display-column range covered by `sel` on rendered row `row`. `end` is
/// exclusive and inclusive of the cell under the head, matching how a terminal's
/// own selection behaves. `usize::MAX` means "to the end of the row".
pub(super) fn selected_cols(sel: Selection, x: u16, row: u16) -> Option<(usize, usize)> {
    let ((r1, c1), (r2, c2)) = sel.ordered();
    if row < r1 || row > r2 {
        return None;
    }
    let x = x as usize;
    let start = if row == r1 {
        (c1 as usize).saturating_sub(x)
    } else {
        0
    };
    let end = if row == r2 {
        (c2 as usize).saturating_sub(x) + 1
    } else {
        usize::MAX
    };
    (start < end).then_some((start, end))
}

/// Byte range of the glyph occupying display column `col`. Columns are terminal
/// cells, not bytes: a wide CJK glyph covers two cells and a multi-byte glyph
/// more bytes than cells. A column landing inside a glyph returns that whole
/// glyph, so a wide character is never sliced in half. Past the end of the row
/// both offsets are `s.len()`.
pub(super) fn glyph_at_col(s: &str, col: usize) -> (usize, usize) {
    let mut cells = 0usize;
    for (i, ch) in s.char_indices() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if col < cells + w {
            return (i, i + ch.len_utf8());
        }
        cells += w;
    }
    (s.len(), s.len())
}

/// The byte range of `s` covered by the display-column range `start..end`.
/// `end == usize::MAX` selects to the end of the row, which is what the rows in
/// the middle of a multi-row drag want.
pub(super) fn cols_to_bytes(s: &str, start: usize, end: usize) -> (usize, usize) {
    let a = glyph_at_col(s, start).0;
    let b = if end == usize::MAX {
        s.len()
    } else {
        glyph_at_col(s, end.saturating_sub(1)).1
    };
    (a, b.max(a).min(s.len()))
}

/// The screen rectangles the selection covers, one per row it reaches, from the
/// rows the last frame drew. Each row's text starts at its recorded column; the
/// highlight spans the visible cells between the anchor and the head.
pub(super) fn selection_rects(sel: Selection, rows: &[Option<(u16, String)>]) -> Vec<Rect> {
    let mut out = Vec::new();
    for (row, slot) in rows.iter().enumerate() {
        let Some((x, line)) = slot else { continue };
        let Some((cs, ce)) = selected_cols(sel, *x, row as u16) else {
            continue;
        };
        let cols = unicode_width::UnicodeWidthStr::width(line.as_str());
        let start = *x as usize + cs;
        let end = if ce == usize::MAX {
            *x as usize + cols
        } else {
            (*x as usize + ce).min(*x as usize + cols)
        };
        if start < end {
            out.push(Rect {
                x: start as u16,
                y: row as u16,
                width: (end - start) as u16,
                height: 1,
            });
        }
    }
    out
}

/// Plain text of the selected range, for the clipboard. Every screen row the
/// last frame drew carries the column its text starts at; the highlight itself
/// is painted straight onto the buffer (see `App::paint_selection`), so this is
/// the only place the copy has to reason about columns.
pub(super) fn selection_text(sel: Selection, rows: &[Option<(u16, String)>]) -> String {
    let mut out = String::new();
    for (row, slot) in rows.iter().enumerate() {
        let Some((x, line)) = slot else { continue };
        let Some((cs, ce)) = selected_cols(sel, *x, row as u16) else {
            continue;
        };
        let (a, b) = cols_to_bytes(line, cs, ce);
        // Every row inside the range contributes a separator, so blank lines
        // between blocks survive a copy instead of being silently dropped.
        if !out.is_empty() {
            out.push('\n');
        }
        if a < b {
            out.push_str(line[a..b].trim_end());
        }
    }
    out
}

/// Whether `ch` is part of a word for a double-click selection: letters, digits
/// and `_`. Punctuation and whitespace are boundaries.
pub(super) fn word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// The run of characters under cell (`row`, `col`) of the drawn rows, as a
/// selection: a word (letters/digits/`_`), or the punctuation run when the click
/// lands on punctuation. `None` when the row is blank at that cell or the click
/// lands on whitespace: a run of spaces copies as nothing once trailing padding
/// is trimmed, so a blank click keeps the plain cell selection instead.
pub(super) fn word_at(rows: &[Option<(u16, String)>], row: u16, col: u16) -> Option<Selection> {
    let (x, line) = rows.get(row as usize)?.as_ref()?;
    let x = *x as usize;
    let (a, b) = glyph_at_col(line, (col as usize).saturating_sub(x));
    let clicked = line[a..].chars().next()?;
    if clicked.is_whitespace() {
        return None;
    }
    let same = |c: char| {
        if word_char(clicked) {
            word_char(c)
        } else {
            !c.is_whitespace() && !word_char(c)
        }
    };
    let lo = line[..a]
        .char_indices()
        .rev()
        .take_while(|(_, c)| same(*c))
        .last()
        .map(|(i, _)| i)
        .unwrap_or(a);
    let hi = line[b..]
        .char_indices()
        .take_while(|(_, c)| same(*c))
        .last()
        .map(|(i, c)| b + i + c.len_utf8())
        .unwrap_or(b);
    let c1 = x + unicode_width::UnicodeWidthStr::width(&line[..lo]);
    let c2 = x + unicode_width::UnicodeWidthStr::width(&line[..hi]).saturating_sub(1);
    Some(Selection {
        anchor: (row, c1 as u16),
        head: (row, c2 as u16),
    })
}

/// The whole drawn row at `row` as a selection, for a triple-click. `None` when
/// nothing was recorded there or the row is empty.
pub(super) fn line_at(rows: &[Option<(u16, String)>], row: u16) -> Option<Selection> {
    let (x, line) = rows.get(row as usize)?.as_ref()?;
    let w = unicode_width::UnicodeWidthStr::width(line.as_str());
    if w == 0 {
        return None;
    }
    Some(Selection {
        anchor: (row, *x),
        head: (row, *x + w as u16 - 1),
    })
}

/// Whether the mouse-up ends a double or triple click that picked a word or
/// row on the press. Releasing over the same cell keeps that pick; the release
/// only moves the head for a plain click or an actual drag.
pub(super) fn release_keeps_pick(
    click_count: u8,
    last_click: Option<(Instant, u16, u16)>,
    row: u16,
    col: u16,
) -> bool {
    click_count > 1 && matches!(last_click, Some((_, r, c)) if (r, c) == (row, col))
}

/// The first and last columns carrying a non-space glyph on a rendered line.
pub(super) fn content_bounds(line: &Line<'_>) -> (usize, usize) {
    let mut col = 0usize;
    let mut first = None;
    let mut last = 0usize;
    for span in &line.spans {
        for ch in span.content.chars() {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if ch != ' ' {
                first.get_or_insert(col);
                last = col + w;
            }
            col += w;
        }
    }
    (first.unwrap_or(0), last)
}
