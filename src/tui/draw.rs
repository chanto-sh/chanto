//! Drawing the TUI: the transcript, todos, status and footer, the agents panel,
//! dialogs, and the popup.

use super::render::*;
use super::stream::*;
use super::*;

/// How many todos the panel lists before it counts the rest. `heights` and
/// `draw_todos` both read it, so the panel is always as tall as what it draws.
const TODOS_SHOWN: usize = 8;

impl App {
    // ------------------------------------------------------------- render

    /// Make sure block `i` has rendered lines for `width` and return how many.
    /// Running tools aren't cached (they change every frame): their lines go in `live`.
    pub(super) fn block_len(
        &mut self,
        i: usize,
        width: u16,
        live: &mut std::collections::HashMap<usize, Vec<Line<'static>>>,
    ) -> usize {
        let expand_all = self.expand_all;
        // The answer being streamed changes on every token. Re-parsing all of its
        // Markdown (and re-highlighting its code blocks) per frame is quadratic
        // over a long reply, so finished blocks are rendered once and only the
        // block still arriving is redone (see `stream_into`); the full render is
        // cached once it closes.
        let streaming = self.open_text == Some(i);
        let b = &mut self.blocks[i];
        if streaming && matches!(b.kind, Kind::Assistant(_)) {
            let Block { kind, cache, .. } = b;
            return stream_into(kind, cache, &mut self.stream_state, i, width);
        }
        if b.is_running_tool() || streaming {
            let lines = render_block(b, width as usize, expand_all, streaming);
            let n = lines.len();
            live.insert(i, lines);
            return n;
        }
        let fresh = matches!(&b.cache, Some((w, v, e, t, _)) if *w == width && *v == b.version && *e == expand_all && *t == theme::epoch());
        if !fresh {
            let lines = render_block(b, width as usize, expand_all, false);
            b.cache = Some((width, b.version, expand_all, theme::epoch(), lines));
        }
        b.cache.as_ref().map(|c| c.4.len()).unwrap_or(0)
    }

    /// Heights of everything below the transcript, for a terminal `w` columns
    /// wide; dialogs take at most 70% of `screen_h`.
    pub(super) fn heights(&self, w: u16, screen_h: u16) -> Heights {
        // Borders, one column of padding each side, and the "> " marker.
        let input_w = w.saturating_sub(6) as usize;
        let (rows, _) = self.input.layout(input_w);
        // Borders plus a blank row of padding above and below the text.
        let input_h = (rows.len().min(10) as u16) + 2;
        let todo = if self.show_todos
            && !self.todos.is_empty()
            && !self.todos.iter().all(|t| t.status == "completed")
        {
            // One row for the header, one blank row between the list and the
            // status line (so a running turn isn't read as a todo), and one for
            // the "more" line when not every todo fits.
            (self.todos.len().min(TODOS_SHOWN) as u16)
                + 2
                + u16::from(self.todos.len() > TODOS_SHOWN)
        } else {
            0
        };
        let panel = self
            .statuses
            .iter()
            .flat_map(|(_, l)| l.iter().skip(1))
            .take(8)
            .count() as u16;
        let status = if self.busy || !self.queue.is_empty() {
            1 + self.queue.len().min(3) as u16
        } else {
            0
        };
        let dialog = match &self.dialog {
            Some(d) => {
                // The dialog body wraps to the block's inner width (the full
                // width less a border each side), the same width the draw uses.
                let width = (w as usize).saturating_sub(2);
                // Borders, the options, and a spacer line (plus "Allow this action?" for permissions).
                let (body, reason, extra): (String, Option<&str>, usize) = match d {
                    Dialog::Permission { req, .. } => (
                        permission_title_body(&req.label, &req.tool, &req.detail).1,
                        req.reason.as_deref().filter(|r| !r.is_empty()),
                        3 + 4 + proceed_rows(true, req.proposed.is_some(), width).len(),
                    ),
                    Dialog::Question {
                        q,
                        options,
                        sel,
                        typing,
                        ..
                    } => (
                        q.clone(),
                        None,
                        option_rows(options, *sel, width).len() + 4 + usize::from(typing.is_some()),
                    ),
                    Dialog::Btw { .. } => (String::new(), None, 3),
                    // Borders, plus a row for the scroll hint when a list is
                    // long enough to need one.
                    Dialog::Mcp { .. } => (String::new(), None, 3),
                };
                // Count the rows the body actually wraps to, so the box is tall
                // enough to show them rather than scrolling them behind a hint.
                let body_rows = match d {
                    Dialog::Btw { index, .. } => btw_overlay_lines(
                        &self.env.btw.lock().unwrap(),
                        self.btw_ask.as_ref(),
                        *index,
                        width,
                    )
                    .len(),
                    Dialog::Mcp {
                        sel, menu, note, ..
                    } => mcp_overlay_lines(&self.env, *sel, *menu, note.as_deref(), width).len(),
                    _ => dialog_body_lines(d, &body, reason, width).len(),
                };
                let want = (body_rows + extra) as u16;
                want.min(screen_h * 9 / 10).max(8)
            }
            None => 0,
        };
        let max = if screen_h < 24 {
            3
        } else {
            agents_panel::MAX_ROWS
        };
        let work = if dialog > 0 {
            0
        } else {
            agents_panel::height(self.background_work().len(), max) as u16
        };
        Heights {
            panel,
            todo,
            status,
            bottom: if dialog > 0 { dialog } else { input_h },
            work,
        }
    }

    /// A permission prompt or a question is waiting on the user.
    pub(super) fn waiting_on_user(&self) -> bool {
        matches!(
            self.dialog,
            Some(Dialog::Permission { .. } | Dialog::Question { tx: Some(_), .. })
        )
    }

    /// Pause the busy timer while the user decides: note when a wait starts,
    /// and when it ends move `busy_since` on by its length.
    pub(super) fn sync_wait(&mut self) {
        match (self.waiting_on_user(), self.wait_started) {
            (true, None) => self.wait_started = Some(Instant::now()),
            (false, Some(t)) => {
                self.busy_since += t.elapsed();
                self.wait_started = None;
            }
            _ => {}
        }
    }

    pub(super) fn draw(&mut self, f: &mut Frame) {
        self.sync_wait();
        let area = f.area();
        let w = area.width;
        // Plain text of the last frame, rebuilt from scratch: the selection reads
        // it back and paints over it, so it must never be a frame stale.
        self.row_text = vec![None; area.height as usize];
        let input_w = w.saturating_sub(6) as usize;
        let (rows, cursor) = self.input.layout(input_w);
        let hs = self.heights(
            w,
            if self.inline.is_some() {
                self.screen_rows
            } else {
                area.height
            },
        );
        let panel: Vec<String> = self
            .statuses
            .iter()
            .flat_map(|(_, l)| l.iter().skip(1).cloned())
            .take(8)
            .collect();
        let (panel_h, todo_h, status_h, bottom_h) = (hs.panel, hs.todo, hs.status, hs.bottom);
        let chunks = Layout::vertical([
            // Inline: the region is sized to fit, and the transcript part holds only what's live.
            Constraint::Min(if self.inline.is_some() { 0 } else { 3 }),
            Constraint::Length(panel_h),
            Constraint::Length(todo_h),
            Constraint::Length(status_h + 1),
            Constraint::Length(bottom_h),
            Constraint::Length(hs.work),
            Constraint::Length(1),
        ])
        .split(area);
        self.transcript_area = chunks[0];
        if self.inline.is_some() {
            self.draw_live(f, chunks[0]);
        } else if self.viewing.is_some() {
            self.draw_agent_view(f, chunks[0]);
        } else {
            self.draw_transcript(f, chunks[0]);
        }
        if panel_h > 0 {
            let lines: Vec<Line> = panel
                .iter()
                .map(|l| {
                    Line::from(Span::styled(
                        format!(" {l}"),
                        Style::default().fg(pal().dim),
                    ))
                })
                .collect();
            for (i, l) in lines.iter().enumerate() {
                let text: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
                self.record_row(chunks[1].y + i as u16, chunks[1].x, &text);
            }
            f.render_widget(Paragraph::new(lines), chunks[1]);
        }
        if todo_h > 0 {
            self.draw_todos(f, chunks[2]);
        }
        self.draw_status(f, chunks[3]);
        if self.dialog.is_some() {
            self.draw_dialog(f, chunks[4]);
        } else {
            self.draw_input(f, chunks[4], &rows, cursor);
            if self.popup.is_some() {
                self.draw_popup(f, chunks[4]);
            }
        }
        if hs.work > 0 {
            self.draw_work(f, chunks[5]);
        } else {
            self.work_area = Rect::default();
        }
        self.draw_footer(f, chunks[6]);
        if self.help {
            self.draw_help(f, area);
        }
        // The overlay owns the screen: nothing under it should appear selected.
        if let Some(sel) = self.selection.filter(|_| !self.help) {
            self.paint_selection(f, sel);
        }
    }

    /// Inline mode: the lines that are final and can go to scrollback (see `settle`).
    pub(super) fn take_settled(&mut self, width: usize) -> Vec<Line<'static>> {
        let quiet = self.quiet_mask();
        match self.inline.as_mut() {
            Some(iv) => settle(
                &self.blocks,
                self.open_text,
                self.open_think,
                iv,
                width,
                quiet.as_ref(),
            ),
            None => vec![],
        }
    }

    /// Inline mode, after a resize: everything printed so far rendered at the new
    /// width, keeping the last `max` lines to redraw the visible part of the screen.
    /// Lines already in the scrollback stay as they were.
    pub(super) fn reprint_after_resize(&mut self, width: usize, max: usize) -> Vec<Line<'static>> {
        let quiet = self.quiet_mask();
        let Some(iv) = self.inline.as_mut() else {
            return vec![];
        };
        let mut fresh = InlineView::default();
        let mut lines = settle(
            &self.blocks,
            self.open_text,
            self.open_think,
            &mut fresh,
            width,
            quiet.as_ref(),
        );
        // Anything that settled since the last frame is in `lines` too.
        *iv = fresh;
        lines.drain(..lines.len().saturating_sub(max));
        lines
    }

    /// Inline mode: what is still changing (see `live`).
    pub(super) fn live_lines(&self, width: usize) -> Vec<Line<'static>> {
        if self.inline.is_some()
            && let Some(lines) = self.agent_view_lines(width as u16)
        {
            return lines;
        }
        match &self.inline {
            Some(iv) => live(
                &self.blocks,
                self.open_text,
                iv,
                self.expand_all,
                width,
                self.quiet_mask().as_ref(),
            ),
            None => vec![],
        }
    }

    /// Inline mode: the region's height this frame. The live part is capped so
    /// recent output stays on screen above it; a popup or help needs its room.
    pub(super) fn inline_height(&self, w: u16, rows: u16) -> u16 {
        let fixed = self.heights(w, rows).total();
        let live = self
            .live_lines(w.saturating_sub(1) as usize)
            .len()
            .min(u16::MAX as usize) as u16;
        let popup = self
            .popup
            .as_ref()
            .map_or(0, |p| p.items.len().min(10) as u16);
        let help = if self.help {
            crate::commands::SHORTCUTS.len() as u16 + 3
        } else {
            0
        };
        let max_live = (rows * 2 / 3).saturating_sub(fixed);
        (fixed + live.min(max_live).max(popup)).max(help)
    }

    /// Inline mode's live part: the newest lines that fit.
    pub(super) fn draw_live(&mut self, f: &mut Frame, area: Rect) {
        let lines = self.live_lines(area.width.saturating_sub(1) as usize);
        let skip = lines.len().saturating_sub(area.height as usize);
        let shown: Vec<Line> = lines.into_iter().skip(skip).collect();
        for (i, l) in shown.iter().enumerate() {
            let text: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
            self.record_row(area.y + i as u16, area.x, &text);
        }
        let links = link_regions(&shown, area.width);
        f.render_widget(Paragraph::new(shown), area);
        paint_links(f.buffer_mut(), area, &links);
    }

    pub(super) fn draw_transcript(&mut self, f: &mut Frame, area: Rect) {
        let width = area.width.saturating_sub(1);
        let h = area.height as usize;
        // Measure blocks from the bottom up until the viewport (plus scroll)
        // is covered, then copy only the visible lines: scrolling far back
        // stays linear and nothing off-screen is cloned.
        let mut live = std::collections::HashMap::new();
        // The quiet view leaves a turn's steps out (no rows, no blank line).
        let quiet = self.quiet_mask();
        let hides = |i: usize| quiet.as_ref().is_some_and(|q| q.hides(i));
        // Scrolled up: keep the same top row on screen by growing the offset
        // by the rows added below it since the last frame.
        if let Some((ai, ak, below)) = self.scroll_anchor.take()
            && self.scroll > 0
            && ai < self.blocks.len()
        {
            let now = (ai..self.blocks.len())
                .filter(|&j| !hides(j))
                .map(|j| self.block_len(j, width, &mut live) + 1)
                .sum::<usize>()
                .saturating_sub(ak);
            self.scroll += now.saturating_sub(below);
        }
        let need = h + self.scroll;
        let mut spans: Vec<(usize, usize)> = Vec::new(); // (block, lines incl. the blank after it)
        let mut total = 0;
        for i in (0..self.blocks.len()).rev() {
            if hides(i) {
                continue;
            }
            let n = self.block_len(i, width, &mut live) + 1;
            spans.push((i, n));
            total += n;
            if total >= need + h {
                break;
            }
        }
        spans.reverse();
        let (rows, start, end) = visible_rows(&spans, h, &mut self.scroll);
        let measured: usize = spans.iter().map(|s| s.1).sum();
        self.scroll_anchor = if self.scroll > 0 {
            rows.first().map(|&(i, k)| (i, k, measured - start))
        } else {
            None
        };
        let mut visible: Vec<(usize, Line<'static>)> = Vec::with_capacity(rows.len());
        for (i, k) in rows {
            let lines: &[Line<'static>] = match live.get(&i) {
                Some(l) => l,
                None => self.blocks[i]
                    .cache
                    .as_ref()
                    .map(|c| c.4.as_slice())
                    .unwrap_or(&[]),
            };
            // The last row of each span is the blank line between blocks.
            visible.push((i, lines.get(k).cloned().unwrap_or_else(|| Line::raw(""))));
        }
        let total = spans.iter().map(|s| s.1).sum::<usize>();
        if let Some(hl) = self.highlight {
            let idx = total
                .checked_sub(1 + hl)
                .filter(|i| (start..end).contains(i));
            if let Some(i) = idx {
                let l = &mut visible[i - start].1;
                *l = l.clone().patch_style(selection_style());
            }
        }
        self.row_map = visible.iter().map(|(i, _)| *i).collect();
        let widget = Rect {
            x: area.x + 1,
            width,
            ..area
        };
        // Snapshot the plain text of every visible row *before* any selection
        // styling. The copy must see whole rows: reading it back from the painted
        // lines would hand back only the highlighted slice. Captured on every
        // draw, not just while selecting: a quick drag can begin and end between
        // two frames (draws are throttled to 16ms), so by the time the mouse is
        // released there may have been no selection-time draw at all.
        for (i, (_, l)) in visible.iter().enumerate() {
            let text: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
            self.record_row(area.y + i as u16, widget.x, &text);
        }
        let lines: Vec<Line> = visible.into_iter().map(|(_, l)| l).collect();
        let links = link_regions(&lines, widget.width);
        f.render_widget(Paragraph::new(lines), widget);
        // The help overlay is drawn over these rows later; decorating them
        // would leave its cells carrying the transcript's link attributes.
        if !self.help {
            paint_links(f.buffer_mut(), widget, &links);
        }
        if self.scroll > 0 {
            let s = format!(
                " ↑ scrolled {} · esc to jump to bottom ",
                crate::util::plural(self.scroll, "line")
            );
            let r = Rect {
                x: area.x + area.width.saturating_sub(s.len() as u16 + 1),
                y: area.y,
                width: s.len() as u16,
                height: 1,
            };
            let st = if theme::no_color() {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default().fg(Color::Black).bg(pal().dim)
            };
            f.render_widget(Paragraph::new(Span::styled(s, st)), r);
        }
    }

    pub(super) fn draw_todos(&mut self, f: &mut Frame, area: Rect) {
        let mut lines = vec![Line::from(Span::styled(
            "Todos (ctrl+t to hide)",
            Style::default().fg(pal().dim),
        ))];
        for t in self.todos.iter().take(TODOS_SHOWN) {
            let (mark, st) = match t.status.as_str() {
                "completed" => (
                    "☒",
                    Style::default()
                        .fg(pal().dim)
                        .add_modifier(Modifier::CROSSED_OUT),
                ),
                "in_progress" => (
                    "◐",
                    Style::default()
                        .fg(pal().accent)
                        .add_modifier(Modifier::BOLD),
                ),
                _ => ("☐", Style::default()),
            };
            let text = if t.status == "in_progress" && !t.active_form.is_empty() {
                &t.active_form
            } else {
                &t.content
            };
            // "   ", the mark and a space; cut the rest at a word so a long
            // title ends in "…" rather than being clipped by the frame.
            let text = fit_words(text, (area.width as usize).saturating_sub(5));
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(format!("{mark} {text}"), st),
            ]));
        }
        let hidden = self.todos.len().saturating_sub(TODOS_SHOWN);
        if hidden > 0 {
            lines.push(Line::from(Span::styled(
                format!("   … {hidden} more"),
                Style::default().fg(pal().dim),
            )));
        }
        for (i, l) in lines.iter().enumerate() {
            let text: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
            self.record_row(area.y + i as u16, area.x, &text);
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    /// The agents panel's items: `main` and subagents (foreground and
    /// background), then background shells.
    pub(super) fn background_work(&self) -> Vec<agents_panel::Item> {
        let rows = self.env.live_agents.rows(Instant::now());
        // Background agents started outside a Task call (no live transcript).
        let background: Vec<(String, String, u64)> = self
            .env
            .bg_agents
            .lock()
            .unwrap()
            .iter()
            .filter(|b| b.status == "running" && !rows.iter().any(|r| r.id == b.id))
            .map(|b| {
                (
                    b.id.clone(),
                    b.description.clone(),
                    b.started.elapsed().as_secs(),
                )
            })
            .collect();
        let mut shells: Vec<(String, String, bool)> = self
            .env
            .shells
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.exit.lock().unwrap().is_none())
            .map(|s| {
                // A monitor's description says what it watches; its command is a
                // long watch loop that would crowd the row out.
                let label = s.monitor.clone().unwrap_or_else(|| s.command.clone());
                (s.id.clone(), label, s.monitor.is_some())
            })
            .collect();
        shells.sort();
        agents_panel::items(rows, background, shells)
    }

    /// The panel under the prompt: ● marks the transcript on
    /// screen, ❯ the selected row.
    pub(super) fn draw_work(&mut self, f: &mut Frame, area: Rect) {
        let items = self.background_work();
        let (start, end, more) =
            agents_panel::window(items.len(), self.work_sel, area.height as usize);
        self.work_area = area;
        self.work_start = start;
        let width = area.width as usize;
        let dim = Style::default().fg(pal().dim);
        let mut lines: Vec<Line> = Vec::new();
        for (i, item) in items.iter().enumerate().take(end).skip(start) {
            let selected = self.work_sel == Some(i);
            lines.push(work_row(self.viewing.as_deref(), item, width, selected));
        }
        if more > 0 {
            lines.push(Line::from(Span::styled(format!("  ↓ {more} more"), dim)));
        }
        for (i, l) in lines.iter().enumerate() {
            let text: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
            self.record_row(area.y + i as u16, area.x, &text);
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    /// A prompt typed while viewing subagent `key`: a background agent gets
    /// it (queued, or resumed if it finished); a foreground one can't take
    /// input, so the text stays in the prompt with a hint. False when the
    /// agent is gone, and the prompt goes to the main conversation.
    pub(super) fn send_to_agent(&mut self, key: &str, text: &str) -> bool {
        let Some(a) = self.env.live_agents.get_if_newer(key, None) else {
            return false;
        };
        if !a.background {
            self.input.set(text);
            self.hint = Some((
                format!(
                    "{} ({}) runs in the foreground and can't take messages; esc returns to main",
                    a.kind, a.id
                ),
                Instant::now(),
            ));
            return true;
        }
        append_history(text);
        let msg = match crate::agent::message_background(&self.env, &a.id, text) {
            Ok(crate::agent::Sent::Queued) => {
                format!("Sent to {}: it reads it after its current step", a.id)
            }
            Ok(crate::agent::Sent::Resumed) => format!("Resumed {} with your message", a.id),
            Err(e) => {
                self.input.set(text);
                e
            }
        };
        self.hint = Some((msg, Instant::now()));
        true
    }

    /// Who a prompt typed now goes to, when it isn't the main conversation:
    /// the viewed subagent's kind, id and whether it can take messages.
    pub(super) fn prompt_target(&self) -> Option<(String, String, bool)> {
        let a = self
            .env
            .live_agents
            .get_if_newer(self.viewing.as_deref()?, None)?;
        Some((a.kind, a.id, a.background))
    }

    /// Show `key`'s transcript in the transcript pane, or the main one (None).
    pub(super) fn view_agent(&mut self, key: Option<String>) {
        if self.viewing != key {
            self.scroll = 0;
            self.scroll_anchor = None;
            self.selection = None;
        }
        self.viewing = key;
    }

    /// Select panel row `i` with Enter or a click.
    pub(super) fn open_work_item(&mut self, item: &agents_panel::Item) {
        match item {
            agents_panel::Item::Main => self.view_agent(None),
            agents_panel::Item::Agent(r) => {
                self.view_agent(Some(r.key.clone()));
                self.hint = Some((
                    format!("Viewing {} ({}) · esc to return to main", r.kind, r.id),
                    Instant::now(),
                ));
            }
            agents_panel::Item::Background { id, .. } => {
                let id = id.clone();
                let handle = self
                    .env
                    .bg_agents
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|b| b.id == id)
                    .map(|b| b.agent.clone());
                let latest = handle.and_then(|h| {
                    let a = h.try_lock().ok()?;
                    a.messages
                        .iter()
                        .rev()
                        .find(|m| {
                            m.role == crate::llm::Role::Assistant && !m.content.trim().is_empty()
                        })
                        .map(|m| m.content.clone())
                });
                let text = match latest {
                    Some(t) => format!("**{id}**, still running. Its latest message:\n\n{t}"),
                    None => format!("**{id}** is running and hasn't said anything yet."),
                };
                self.push(Kind::Markdown(text));
            }
            agents_panel::Item::Shell { .. } => {}
        }
    }

    /// Keys while a row of the agents panel is selected.
    pub(super) fn on_work_key(&mut self, k: KeyEvent) {
        let items = self.background_work();
        let Some(sel) = self.work_sel.filter(|s| *s < items.len()) else {
            self.work_sel = None;
            return;
        };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Up => self.work_sel = agents_panel::step(sel, items.len(), false),
            KeyCode::Char('p') if ctrl => {
                self.work_sel = agents_panel::step(sel, items.len(), false)
            }
            KeyCode::Down => self.work_sel = agents_panel::step(sel, items.len(), true),
            KeyCode::Char('n') if ctrl => {
                self.work_sel = agents_panel::step(sel, items.len(), true)
            }
            KeyCode::Esc => self.work_sel = None,
            KeyCode::Enter => {
                self.open_work_item(&items[sel]);
                self.work_sel = None;
            }
            KeyCode::Char('x') => {
                let msg = match &items[sel] {
                    agents_panel::Item::Main => return,
                    agents_panel::Item::Agent(r)
                        if r.status != crate::agent::live::RowStatus::Running =>
                    {
                        self.env.live_agents.dismiss(&r.key);
                        format!("Dismissed {}", r.id)
                    }
                    agents_panel::Item::Agent(r) if !r.background => {
                        "A foreground agent stops with the turn: esc to interrupt".to_string()
                    }
                    agents_panel::Item::Agent(r) => {
                        crate::agent::stop_background(&self.env, &r.id);
                        format!("Stopped {}", r.id)
                    }
                    agents_panel::Item::Background { id, .. } => {
                        crate::agent::stop_background(&self.env, id);
                        format!("Stopped {id}")
                    }
                    agents_panel::Item::Shell { id, .. } => {
                        if let Some(s) = self.env.shells.lock().unwrap().remove(id)
                            && let Some(pid) = s.pid
                        {
                            crate::tools::bash::kill_group(pid);
                        }
                        format!("Stopped {id}")
                    }
                };
                self.hint = Some((msg, Instant::now()));
                self.work_sel = None;
            }
            _ => {}
        }
    }

    /// The viewed subagent's transcript, built from what the registry kept.
    pub(super) fn agent_view_lines(&self, width: u16) -> Option<Vec<Line<'static>>> {
        let key = self.viewing.as_ref()?;
        let a = self.env.live_agents.get_if_newer(key, None)?;
        Some(agent_transcript(&a, width as usize, self.expand_all))
    }

    /// Fullscreen: the viewed subagent's transcript, newest at the bottom.
    pub(super) fn draw_agent_view(&mut self, f: &mut Frame, area: Rect) {
        let width = area.width.saturating_sub(1);
        let key = self.viewing.clone().unwrap_or_default();
        let seen = self
            .agent_view
            .as_ref()
            .filter(|c| c.0 == key && c.2 == width && c.3 == self.expand_all)
            .map(|c| c.1);
        match self.env.live_agents.get_if_newer(&key, seen) {
            Some(a) => {
                let lines = agent_transcript(&a, width as usize, self.expand_all);
                self.agent_view = Some((key, a.version, width, self.expand_all, lines));
            }
            None if seen.is_none() => {
                // Forgotten (or never known): back to the main conversation.
                self.viewing = None;
                self.agent_view = None;
                return self.draw_transcript(f, area);
            }
            None => {}
        }
        let lines = self
            .agent_view
            .as_ref()
            .map(|c| c.4.as_slice())
            .unwrap_or(&[]);
        let h = area.height as usize;
        self.scroll = self.scroll.min(lines.len().saturating_sub(h));
        let end = lines.len() - self.scroll;
        let start = end.saturating_sub(h);
        let visible: Vec<Line> = lines[start..end].to_vec();
        self.row_map.clear();
        for (i, l) in visible.iter().enumerate() {
            let text: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
            self.record_row(area.y + i as u16, area.x + 1, &text);
        }
        f.render_widget(
            Paragraph::new(visible),
            Rect {
                x: area.x + 1,
                width,
                ..area
            },
        );
    }

    pub(super) fn draw_status(&mut self, f: &mut Frame, area: Rect) {
        let mut lines = Vec::new();
        if self.busy {
            let el = view_time::elapsed(self.busy_since);
            let frame = busy_frame(crate::a11y::reduce_motion(), el.as_millis());
            // While reasoning is streaming, use progressive wording so a slow
            // model reads as still working rather than frozen.
            let thinking = self.open_think.and_then(|i| match &self.blocks[i].kind {
                Kind::Thinking { started, .. } => Some(view_time::elapsed(*started).as_secs()),
                _ => None,
            });
            // A prompt waiting on the user isn't the model thinking, and the
            // time spent deciding isn't counted (see `sync_wait`).
            let waiting = self.waiting_on_user();
            let el = el.saturating_sub(
                self.wait_started
                    .map(view_time::elapsed)
                    .unwrap_or_default(),
            );
            let verb = match thinking {
                _ if waiting => "Waiting for you".to_string(),
                Some(s) => crate::util::capitalize(thinking_verb(s)),
                None => self
                    .busy_label
                    .clone()
                    .unwrap_or_else(|| VERBS[self.spinner_i].to_string()),
            };
            let frame = if waiting { "◇" } else { frame };
            let toks = self.stream_chars / 4;
            // The quiet view shows no tool blocks, so the status line names
            // the running tool and counts the calls so far.
            let verb = match &self.current_tool {
                Some(tool) if self.quiet && !waiting => crate::util::one_line(tool, 60),
                _ => verb,
            };
            let mut detail = format!(" ({}", crate::util::duration(el.as_secs()));
            if self.quiet && self.turn_tools > 0 {
                detail.push_str(&format!(
                    " · {}",
                    crate::util::plural(self.turn_tools, "tool call")
                ));
            }
            if toks > 0 {
                detail.push_str(&format!(
                    " · ↓ {} tokens",
                    crate::util::short_num(toks as u64)
                ));
            }
            detail.push_str(" · esc to interrupt)");
            lines.push(Line::from(vec![
                Span::styled(format!(" {frame} "), Style::default().fg(pal().accent)),
                Span::styled(format!("{verb}…"), Style::default().fg(pal().accent)),
                Span::styled(detail, Style::default().fg(pal().dim)),
            ]));
        }
        for (i, q) in self.queue.iter().take(3).enumerate() {
            let hint = if i == 0 {
                " · ⏎ on an empty line runs it now"
            } else {
                ""
            };
            lines.push(Line::from(Span::styled(
                format!(" ⧗ queued: {}{hint}", crate::util::one_line(&q.0, 80)),
                Style::default().fg(pal().dim),
            )));
        }
        for (i, l) in lines.iter().enumerate() {
            let text: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
            self.record_row(area.y + i as u16, area.x, &text);
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    pub(super) fn frame_style(&self) -> FrameStyle {
        frame_from(self.env.config.input_border.as_deref())
    }

    pub(super) fn draw_input(
        &mut self,
        f: &mut Frame,
        area: Rect,
        rows: &[String],
        cursor: (usize, usize),
    ) {
        // A leading `!` is the shell escape hatch, not a message to the model, so
        // the prompt recolors to say so before it's sent.
        let shell = self.input.text.trim_start().starts_with('!');
        let border = if shell {
            pal().code_block
        } else {
            match self.mode {
                Mode::Plan => cyan(),
                Mode::Auto => markdown::amber(),
                Mode::Bypass => pal().error,
                Mode::AcceptEdits => pal().accept_edits,
                _ => pal().border,
            }
        };
        // The default prompt is a line above and a line below, as in Claude
        // Code: a full-width rule sits mid-cell, so the text gets some air
        // without blank rows. `input_border = "line"` or `"solid"` keep a box.
        let block = match self.frame_style() {
            FrameStyle::Block => WBlock::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_style(Style::default().fg(border)),
            style => framed(border, style),
        }
        .padding(ratatui::widgets::Padding::new(1, 1, 0, 0));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let visible_rows = inner.height as usize;
        let first = cursor.0.saturating_sub(visible_rows.saturating_sub(1));
        let mut lines: Vec<Line> = Vec::new();
        if self.input.is_empty() {
            // While busy the model is mid-turn, so the `⧗ queued:` line (and the
            // status bar's "esc to interrupt") already explain what happens to a
            // message typed now; the placeholder only needs to invite typing.
            // One concrete example is easier to act on than a list of syntax.
            // Before the first turn it shows what a prompt looks like; after
            // that it invites a follow-up. `/`, `@` and `!` stay discoverable
            // in the welcome panel and the `?` shortcuts list.
            let ph = match self.prompt_target() {
                Some((kind, id, true)) => format!("Message {kind} ({id})…"),
                Some((kind, id, false)) => format!(
                    "{kind} ({id}) runs in the foreground and can't take messages · esc returns to main"
                ),
                None if self.busy => "Type a message…".to_string(),
                None if self
                    .blocks
                    .iter()
                    .any(|b| matches!(b.kind, Kind::User(_) | Kind::Assistant(_))) =>
                {
                    "Ask a follow-up…".to_string()
                }
                None => "Try \"fix the failing test\"".to_string(),
            };
            // Whole hints that fit, never one cut mid-word.
            let ph = fit_parts(
                &ph.split(" · ").collect::<Vec<_>>(),
                (inner.width as usize).saturating_sub(2),
            );
            match self.suggestion.as_deref().filter(|_| !self.busy) {
                Some(s) => lines.push(Line::from(vec![
                    Span::styled("> ", Style::default().fg(pal().dim)),
                    Span::styled(
                        crate::util::one_line(s, (area.width as usize).saturating_sub(20)),
                        Style::default()
                            .fg(pal().dim)
                            .add_modifier(Modifier::ITALIC),
                    ),
                    Span::styled("  tab to use", Style::default().fg(faint())),
                ])),
                None => lines.push(Line::from(vec![
                    Span::styled("> ", Style::default().fg(pal().dim)),
                    Span::styled(
                        ph,
                        Style::default()
                            .fg(pal().dim)
                            .add_modifier(Modifier::ITALIC),
                    ),
                ])),
            }
        } else {
            let marker = if shell {
                ("! ".to_string(), Style::default().fg(pal().code_block))
            } else {
                ("> ".to_string(), Style::default().fg(pal().dim))
            };
            for (i, r) in rows.iter().enumerate().skip(first).take(visible_rows) {
                let p = if i == 0 {
                    marker.clone()
                } else {
                    ("  ".to_string(), Style::default().fg(pal().dim))
                };
                // A slash command on the first row stands out; a `!shell` line is
                // all one colour already, so it isn't re-styled.
                let mut spans = vec![Span::styled(p.0, p.1)];
                if shell {
                    spans.push(Span::raw(r.clone()));
                } else {
                    spans.extend(input_row_spans(r, i == 0));
                }
                lines.push(Line::from(spans));
            }
        }
        for (i, l) in lines.iter().enumerate() {
            let text: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
            self.record_row(inner.y + i as u16, inner.x, &text);
        }
        f.render_widget(Paragraph::new(lines), inner);
        f.set_cursor_position((
            inner.x + 2 + cursor.1 as u16,
            inner.y + (cursor.0 - first) as u16,
        ));
    }

    pub(super) fn draw_popup(&self, f: &mut Frame, input_area: Rect) {
        let Some(p) = &self.popup else { return };
        let n = p.items.len().min(10);
        let h = n as u16;
        // Full width: a one-column inset let transcript glyphs show at the edges.
        let area = Rect {
            x: input_area.x,
            y: input_area.y.saturating_sub(h),
            width: input_area.width,
            height: h,
        };
        f.render_widget(Clear, area);
        let start = p.sel.saturating_sub(n - 1);
        // "3/42" when the list is longer than the popup, on the top row.
        let count = (p.items.len() > n).then(|| format!(" {}/{} ", p.sel + 1, p.items.len()));
        let count_w = count.as_ref().map_or(0, |c| c.chars().count());
        let width = area.width as usize;
        let lines: Vec<Line> = p
            .items
            .iter()
            .enumerate()
            .skip(start)
            .take(n)
            .map(|(i, (a, b))| {
                let selected = i == p.sel;
                let room = width.saturating_sub(
                    a.chars().count().max(22) + 2 + if i == start { count_w } else { 0 },
                );
                let desc = format!(" {}", crate::util::one_line(b, room));
                let name = format!(" {a:<22}");
                // The selected row is highlighted across the whole popup, so it
                // shows on any background, not just as a change of text colour.
                let (name_st, desc_st) = if selected {
                    let s = selection_style().add_modifier(Modifier::BOLD);
                    (s, s)
                } else {
                    (Style::default(), Style::default().fg(pal().dim))
                };
                let mut spans = vec![Span::styled(name, name_st), Span::styled(desc, desc_st)];
                let used: usize = spans.iter().map(|s| s.width()).sum();
                let pad = width.saturating_sub(used + if i == start { count_w } else { 0 });
                spans.push(Span::styled(
                    " ".repeat(pad),
                    if selected { name_st } else { Style::default() },
                ));
                if i == start
                    && let Some(c) = &count
                {
                    spans.push(Span::styled(c.clone(), Style::default().fg(pal().dim)));
                }
                Line::from(spans)
            })
            .collect();
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(panel_bg())),
            area,
        );
    }

    pub(super) fn draw_dialog(&mut self, f: &mut Frame, area: Rect) {
        let Some(d) = &self.dialog else { return };
        // Permissions and questions frame in the warning colour; `/btw` is not a
        // prompt, so it frames in the accent.
        let frame_color = if matches!(d, Dialog::Btw { .. } | Dialog::Mcp { .. }) {
            pal().accent
        } else {
            warn()
        };
        let (title, body, options, sel, typing, scroll): (
            String,
            String,
            Vec<String>,
            usize,
            Option<&Input>,
            u16,
        ) = match d {
            Dialog::Permission {
                req,
                sel,
                feedback,
                scroll,
                deadline,
                ..
            } => {
                let (title, detail) = permission_title_body(&req.label, &req.tool, &req.detail);
                let title = permission_title_with_countdown(&title, *deadline, Instant::now());
                (
                    title,
                    detail,
                    vec![
                        if req.tool == "SandboxNetwork" {
                            "Allow once".into()
                        } else {
                            "Yes".into()
                        },
                        req.always_label.clone(),
                        "No · say what to do instead (esc)".into(),
                    ],
                    *sel,
                    feedback.as_ref(),
                    *scroll,
                )
            }
            Dialog::Question {
                q,
                options,
                sel,
                typing,
                scroll,
                free_text,
                action,
                ..
            } => (
                if *free_text {
                    " Question "
                } else if *action == Some(QuestionAction::Quit) {
                    " Quit "
                } else {
                    " Select "
                }
                .into(),
                q.clone(),
                options.clone(),
                *sel,
                typing.as_ref(),
                *scroll,
            ),
            Dialog::Btw { scroll, .. } => {
                (" /btw ".into(), String::new(), vec![], 0, None, *scroll)
            }
            Dialog::Mcp { scroll, .. } => {
                (" /mcp ".into(), String::new(), vec![], 0, None, *scroll)
            }
        };
        // Like the input box: the default is a rule above and below (the title
        // sits on the top rule), so the modal doesn't fence itself off with
        // vertical bars that compete with the transcript. `input_border = "line"`
        // or `"solid"` keep a full box. The inset keeps the text off the screen
        // edge where the side bars used to be.
        let block = match self.frame_style() {
            FrameStyle::Block => WBlock::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_style(Style::default().fg(frame_color))
                .padding(ratatui::widgets::Padding::new(1, 1, 0, 0)),
            style => framed(frame_color, style),
        }
        .title(Span::styled(
            title,
            Style::default()
                .fg(frame_color)
                .add_modifier(Modifier::BOLD),
        ));
        let inner = block.inner(area);
        f.render_widget(Clear, area);
        f.render_widget(block, area);
        let permission = matches!(d, Dialog::Permission { .. });
        let editable = matches!(d, Dialog::Permission { req, .. } if req.proposed.is_some());
        let option_lines = option_rows(&options, sel, inner.width as usize);
        // A spacer between the body and the choices; permissions ask there.
        let question = proceed_rows(permission, editable, inner.width as usize);
        let opt_h = option_lines.len() as u16
            + question.len() as u16
            + u16::from(permission)
            + u16::from(typing.is_some())
            + u16::from(matches!(d, Dialog::Question { .. }));
        let body_h = inner.height.saturating_sub(opt_h);
        // Why we're asking (a hook, or the auto-mode classifier) shows above the
        // diff/command, so the reason is never hidden.
        let reason = match d {
            Dialog::Permission { req, .. } => req.reason.as_deref().filter(|r| !r.is_empty()),
            _ => None,
        };
        let body_lines = match d {
            Dialog::Btw { index, .. } => btw_overlay_lines(
                &self.env.btw.lock().unwrap(),
                self.btw_ask.as_ref(),
                *index,
                inner.width as usize,
            ),
            Dialog::Mcp {
                sel, menu, note, ..
            } => mcp_overlay_lines(
                &self.env,
                *sel,
                *menu,
                note.as_deref(),
                inner.width as usize,
            ),
            _ => dialog_body_lines(d, &body, reason, inner.width as usize),
        };
        let total = body_lines.len() as u16;
        let (text_h, scroll, hint) = dialog_body_rows(total, body_h, scroll);
        let plain =
            |line: &Line| -> String { line.spans.iter().map(|s| s.content.as_ref()).collect() };
        let body_text: Vec<String> = (0..text_h)
            .filter_map(|i| body_lines.get((scroll + i) as usize).map(plain))
            .collect();
        f.render_widget(
            Paragraph::new(body_lines).scroll((scroll, 0)),
            Rect {
                height: text_h,
                ..inner
            },
        );
        if let Some(hint) = hint.as_deref() {
            f.render_widget(
                Paragraph::new(Span::styled(hint, Style::default().fg(pal().dim))),
                Rect {
                    y: inner.y + text_h,
                    height: 1,
                    ..inner
                },
            );
        }
        let mut lines = question;
        lines.extend(option_lines);
        // The keys, so the numbered options and the free-text path are obvious.
        if let Dialog::Question { free_text, .. } = d {
            let hint = if typing.is_some() {
                "  type an answer · enter to send · esc to cancel"
            } else if *free_text {
                "  ↑/↓ and enter, or type an answer · 1-9 pick · esc cancel"
            } else {
                "  ↑/↓ or 1-9 to choose · enter to select · esc to cancel"
            };
            lines.push(Line::from(Span::styled(
                hint,
                Style::default().fg(pal().dim),
            )));
        }
        let oa = Rect {
            y: inner.y + body_h,
            height: opt_h.min(inner.height),
            ..inner
        };
        let cursor = typing.map(|t| push_typing_line(&mut lines, t, oa));
        let option_text: Vec<String> = lines.iter().map(&plain).collect();
        f.render_widget(Paragraph::new(lines), oa);
        if let Some(pos) = cursor {
            f.set_cursor_position(pos);
        }
        // Dialog text is selectable like the rest of the screen, so questions and
        // permission details can be copied.
        for (i, text) in body_text.iter().enumerate() {
            self.record_row(inner.y + i as u16, inner.x, text);
        }
        if let Some(hint) = hint.as_deref() {
            self.record_row(inner.y + text_h, inner.x, hint);
        }
        for (i, text) in option_text.iter().enumerate() {
            self.record_row(oa.y + i as u16, oa.x, text);
        }
    }

    /// The `?` shortcuts overlay: a centered panel built from the same list
    /// `/help` prints, so the two never disagree.
    pub(super) fn draw_help(&self, f: &mut Frame, area: Rect) {
        let key_w = crate::commands::SHORTCUTS
            .iter()
            .map(|(k, _)| unicode_width::UnicodeWidthStr::width(*k))
            .max()
            .unwrap_or(0);
        // The full width: a centered panel left strips of transcript showing
        // at its sides, which read as stray glyphs.
        let h = (crate::commands::SHORTCUTS.len() as u16 + 3).min(area.height);
        let rect = Rect {
            x: area.x,
            y: area.y + area.height.saturating_sub(h) / 2,
            width: area.width,
            height: h,
        };
        let block = framed(pal().accent, self.frame_style()).title(Span::styled(
            " Shortcuts ",
            Style::default()
                .fg(pal().accent)
                .add_modifier(Modifier::BOLD),
        ));
        let mut lines: Vec<Line> = crate::commands::SHORTCUTS
            .iter()
            .map(|(k, d)| {
                Line::from(vec![
                    Span::styled(format!(" {k:<key_w$}  "), Style::default().fg(pal().accent)),
                    Span::styled(*d, Style::default().fg(pal().dim)),
                ])
            })
            .collect();
        f.render_widget(Clear, rect);
        let inner = block.inner(rect);
        // On a short terminal the list may not fit; say how many rows are hidden
        // rather than silently dropping the rest.
        let rows = inner.height as usize;
        if lines.len() + 1 > rows && rows > 0 {
            let shown = rows.saturating_sub(1);
            let hidden = lines.len().saturating_sub(shown);
            lines.truncate(shown);
            lines.push(Line::from(Span::styled(
                format!(" …{hidden} more · resize to see all"),
                Style::default()
                    .fg(pal().dim)
                    .add_modifier(Modifier::ITALIC),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                " press any key to close",
                Style::default()
                    .fg(pal().dim)
                    .add_modifier(Modifier::ITALIC),
            )));
        }
        f.render_widget(block, rect);
        f.render_widget(Paragraph::new(lines), inner);
    }

    pub(super) fn draw_footer(&mut self, f: &mut Frame, area: Rect) {
        let (mode_txt, mode_color, mode_hint) = match self.mode {
            Mode::Default => ("? keys".to_string(), pal().dim, None),
            Mode::AcceptEdits => (
                "✎ accept edits".into(),
                pal().accept_edits,
                Some("shift+tab changes mode"),
            ),
            Mode::Plan => ("▹ plan mode".into(), cyan(), Some("shift+tab changes mode")),
            Mode::Auto => (
                "▸ auto mode".into(),
                markdown::amber(),
                Some("shift+tab changes mode"),
            ),
            Mode::Bypass => ("⊘ bypass permissions".into(), pal().error, None),
        };
        if let Some(s) = &self.search {
            let label = if s.transcript {
                "search transcript"
            } else {
                "reverse-i-search"
            };
            let st = if s.found {
                Style::default().fg(pal().accent)
            } else {
                Style::default().fg(pal().error)
            };
            let keys = if s.transcript {
                "ctrl+f/↑ older · ↓ newer · enter keep · esc close"
            } else {
                "ctrl+r older · enter accept · esc cancel"
            };
            let line = Line::from(vec![
                Span::styled(format!(" ({label}) "), st),
                Span::raw(format!("{}▏", s.query)),
                Span::styled(
                    format!("  {}{keys}", if s.found { "" } else { "no match · " }),
                    Style::default().fg(pal().dim),
                ),
            ]);
            let text: String = line.spans.iter().map(|sp| sp.content.as_ref()).collect();
            self.record_row(area.y, area.x, &text);
            f.render_widget(Paragraph::new(line), area);
            return;
        }
        let elapsed = |d: Duration| format!("{}:{:02}", d.as_secs() / 60, d.as_secs() % 60);
        // The left side and how much it matters: the shortcuts hint and the
        // status text go first on a narrow terminal, a mode or a warning last.
        let (left, left_hint, left_prio) = match (&self.voice, &self.hint, &self.status_text) {
            (Voice::Starting(_), _, _) => (
                Span::styled(
                    " ● starting the microphone…",
                    Style::default().fg(pal().error),
                ),
                None,
                9,
            ),
            (Voice::Recording(rec, _), _, _) => (
                Span::styled(
                    format!(
                        " ● recording {} · ctrl+space to stop · esc to discard",
                        elapsed(rec.elapsed())
                    ),
                    Style::default()
                        .fg(pal().error)
                        .add_modifier(Modifier::BOLD),
                ),
                None,
                9,
            ),
            (Voice::Transcribing(t), _, _) => (
                Span::styled(
                    format!(" ◌ transcribing… {}", elapsed(t.elapsed())),
                    Style::default().fg(pal().accent),
                ),
                None,
                9,
            ),
            (_, Some((h, t)), _) if t.elapsed() < Duration::from_secs(3) => (
                Span::styled(format!(" {h}"), Style::default().fg(warn())),
                None,
                9,
            ),
            (_, _, Some(st)) if self.mode == Mode::Default => (
                Span::styled(format!(" {st}"), Style::default().fg(pal().dim)),
                None,
                2,
            ),
            _ => (
                Span::styled(format!(" {mode_txt}"), Style::default().fg(mode_color)),
                mode_hint,
                if self.mode == Mode::Default { 2 } else { 9 },
            ),
        };
        let pct = if self.ctx_max > 0 {
            self.ctx_used as f64 * 100.0 / self.ctx_max as f64
        } else {
            0.0
        };
        let ctx_color = if pct > 80.0 {
            pal().error
        } else if pct > 60.0 {
            warn()
        } else {
            pal().dim
        };
        // (text, style, priority): the lowest priorities drop first when the
        // terminal is too narrow for everything.
        let dim = Style::default().fg(pal().dim);
        let mut right: Vec<(String, Style, u8)> = vec![];
        // Always visible while the model may drive the desktop.
        if let Some(ind) = crate::tools::computer::indicator() {
            right.push((
                ind.to_string(),
                Style::default()
                    .fg(pal().error)
                    .add_modifier(Modifier::BOLD),
                10,
            ));
        }
        for (_, lines) in &self.statuses {
            right.push((lines[0].clone(), Style::default().fg(pal().accent), 5));
        }
        // MCP servers at a glance: connected out of configured, amber when one
        // failed or needs sign-in. `/mcp` opens the full list.
        let (mcp_ok, mcp_total, mcp_attention) = crate::commands::mcp_summary(&self.env);
        if mcp_total > 0 {
            right.push((
                format!("mcp {mcp_ok}/{mcp_total}"),
                Style::default().fg(if mcp_attention { warn() } else { pal().dim }),
                5,
            ));
        }
        if let Some(next) = self.env.scheduler.loop_next() {
            right.push((
                format!("↻ next {}", next.format("%H:%M")),
                Style::default().fg(pal().accent),
                5,
            ));
        }
        if let Some(b) = &self.branch {
            right.push((format!("⎇ {b}"), dim, 3));
        }
        if let Some(pr) = self.pr_status.lock().unwrap().clone() {
            let color = if pr.contains("failing") || pr.contains("changes requested") {
                warn()
            } else {
                pal().dim
            };
            right.push((pr, Style::default().fg(color), 4));
        }
        // Read live: /model (idle or mid-turn), the picker and resume all change it.
        right.push((self.env.model(), dim, 8));
        // Reasoning effort, when it isn't the provider's default (/effort, --effort).
        if let Some(effort) = effort_label(self.env.think(), self.env.effort()) {
            right.push((effort, dim, 4));
        }
        // Account limits (ChatGPT plans). Color warns as the window fills so the
        // status line matches what the provider will actually enforce.
        if let Some(l) = &self.limits {
            let (warn_at, crit_at) = (75.0, 90.0);
            if let Some(w) = l
                .primary
                .iter()
                .chain(l.secondary.iter())
                .max_by(|a, b| a.used_percent.total_cmp(&b.used_percent))
            {
                let color = if w.used_percent >= crit_at {
                    pal().error
                } else if w.used_percent >= warn_at {
                    warn()
                } else {
                    pal().dim
                };
                let warning = w.used_percent >= warn_at;
                let marker = if warning { "⚠ " } else { "" };
                let label = w.label();
                let limit = if area.width >= 80 && label != "limit" {
                    format!("{label} limit:")
                } else {
                    label
                };
                right.push((
                    format!("{marker}{limit} {:.0}% left", w.left_percent()),
                    Style::default().fg(color),
                    if warning { 9 } else { 6 },
                ));
                if let Some(reset) = w
                    .resets_at
                    .and_then(|at| quota_reset(at, view_time::timestamp()))
                {
                    right.push((reset, Style::default().fg(color), 5));
                }
                right.push(("/usage".into(), dim, 3));
            }
        }
        right.push((
            format!("{:.0}% ctx", pct),
            Style::default().fg(ctx_color),
            7,
        ));
        // Why the Mac isn't sleeping: Chanto is holding it awake (`keep_awake`).
        if crate::awake::active() {
            right.push(("awake".into(), dim, 1));
        }
        if let Some(cost) = cost_label(self.total_cost_usd, self.price_unknown) {
            right.push((cost, dim, 4));
        }
        if self.tps > 0.0 {
            right.push((format!("{:.0} tok/s", self.tps), dim, 1));
        }
        let widths: Vec<(usize, u8)> = right
            .iter()
            .map(|(t, _, p)| (unicode_width::UnicodeWidthStr::width(t.as_str()), *p))
            .collect();
        let (show_left, keep) = fit_footer((left.width(), left_prio), &widths, area.width as usize);
        let mut spans: Vec<Span> = Vec::new();
        for (_, (t, st, _)) in right
            .into_iter()
            .enumerate()
            .filter(|(i, _)| keep.contains(i))
        {
            if !spans.is_empty() {
                spans.push(Span::styled(" · ", dim));
            }
            spans.push(Span::styled(t, st));
        }
        spans.push(Span::raw(" "));
        let rw: usize = spans.iter().map(|s| s.width()).sum();
        let right_w = (rw as u16).min(area.width);
        let mut left_text = String::new();
        if show_left {
            // Always a gap before the right side; a left side that still
            // doesn't fit ends in "…" rather than running into it.
            let room = area.width.saturating_sub(right_w + FOOTER_GAP as u16) as usize;
            let text = footer_left(&left.content, left_hint, room);
            left_text = text.clone();
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(text, left.style))),
                Rect {
                    width: room as u16,
                    ..area
                },
            );
        }
        let ra = Rect {
            x: area.x + area.width - right_w,
            width: right_w,
            ..area
        };
        // The two halves are one row for a copy: the left one as it fits, then
        // padded across the gap to where the right one starts.
        let right_text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        self.record_row(area.y, area.x, &left_text);
        self.record_row(area.y, ra.x, &right_text);
        f.render_widget(Paragraph::new(Line::from(spans)), ra);
    }
}
