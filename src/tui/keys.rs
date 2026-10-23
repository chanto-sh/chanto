//! Keyboard handling: the main key handler, search and vim modes, and the keys
//! a dialog, popup or question takes while it is open.

use super::*;

impl App {
    // --------------------------------------------------------------- keys

    pub(super) fn on_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        // Typing means you're here: no idle notification or away recap.
        self.idle_since = None;
        // keybindings.json: a remapped key runs its action by replaying the
        // key Chanto already uses for it.
        if !self.replaying {
            if let Some(problems) = self.keymap.refresh() {
                let msg = if problems.is_empty() {
                    "Keybindings reloaded.".to_string()
                } else {
                    format!(
                        "Keybindings reloaded, with problems:\n- {}",
                        problems.join("\n- ")
                    )
                };
                self.note(msg, pal().dim);
            }
            if !self.keymap.is_empty() {
                use keybindings::{Context, Lookup};
                let active: &[Context] = if self.dialog.is_some() {
                    &[Context::Confirmation, Context::Select, Context::Global]
                } else if self.search.is_some() {
                    &[Context::HistorySearch, Context::Global]
                } else if self.popup.is_some() {
                    &[Context::Autocomplete, Context::Chat, Context::Global]
                } else {
                    &[Context::Chat, Context::Global]
                };
                match self.keymap.lookup(active, keybindings::Key::of(&k)) {
                    Lookup::Default => {}
                    Lookup::Pending | Lookup::Swallow => return,
                    Lookup::Run(effect) => return self.run_key_effect(effect),
                }
            }
        }
        // Copy a kept selection before the keystroke clears it. This is the
        // fallback when copy_on_select is off (and the default binding for
        // Claude Code's `selection:copy`).
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        if ctrl && shift && matches!(k.code, KeyCode::Char('c')) && self.selection.is_some() {
            self.copy_selection();
            return;
        }
        // Any keystroke ends a text selection: leaving the highlight painted over
        // content that then scrolls or reflows is just stale chrome.
        self.selection = None;
        self.selection_moved = false;
        if self.dialog.is_some() {
            self.on_dialog_key(k);
            return;
        }
        if self.search.is_some() {
            self.on_search_key(k);
            return;
        }
        if self.work_sel.is_some() {
            self.on_work_key(k);
            return;
        }
        // The shortcuts overlay is dismissed by any key, except a bare modifier
        // press (tapping shift/ctrl on its own shouldn't close it).
        if self.help {
            if !matches!(k.code, KeyCode::Modifier(_)) {
                self.help = false;
            }
            return;
        }
        // ctrl+x chords: ctrl+x ctrl+s sends now, ctrl+x enter
        // queues until the turn ends, ctrl+x ctrl+e opens the editor. Any other
        // key cancels the chord and acts as usual.
        if std::mem::take(&mut self.chord_x) {
            // The chord's hint goes away however the chord ends.
            self.hint = None;
            match k.code {
                KeyCode::Char('s') if ctrl => return self.submit_as(Deliver::Now),
                KeyCode::Enter => return self.submit_as(Deliver::Queue),
                KeyCode::Char('e') if ctrl => {
                    self.open_editor = true;
                    return;
                }
                _ => {}
            }
        } else if matches!(k.code, KeyCode::Char('x') if ctrl) {
            self.chord_x = true;
            self.hint = Some((
                "ctrl+x … ctrl+s send now · enter queue for after this turn · ctrl+e editor".into(),
                Instant::now(),
            ));
            return;
        }
        // Vim modal editing (/vim). Ctrl chords still fall through so ctrl+c,
        // ctrl+r etc. keep working; in normal mode every other key is a command.
        if self.vim && !ctrl && !alt {
            if self.vim_insert {
                if k.code == KeyCode::Esc {
                    self.vim_insert = false;
                    self.vim_pending = None;
                    return;
                }
            } else {
                self.vim_normal_key(k);
                return;
            }
        }
        // Voice input: the same key starts and stops; esc discards a recording.
        if matches!(k.code, KeyCode::Char(' ') if ctrl) || k.code == KeyCode::Null {
            self.toggle_voice();
            return;
        }
        if k.code == KeyCode::Esc && matches!(self.voice, Voice::Recording(..) | Voice::Starting(_))
        {
            self.voice = Voice::Off;
            self.hint = Some(("Recording discarded".into(), Instant::now()));
            return;
        }
        // Global shortcuts.
        match k.code {
            KeyCode::Char('c') if ctrl => {
                if !self.input.is_empty() {
                    self.input.set("");
                    self.popup = None;
                } else if self.busy {
                    self.interrupt();
                } else if self
                    .ctrl_c_at
                    .map(|t| t.elapsed() < Duration::from_secs(2))
                    .unwrap_or(false)
                {
                    self.request_quit();
                } else {
                    self.ctrl_c_at = Some(Instant::now());
                    self.hint = Some(("Press ctrl+c again to exit".into(), Instant::now()));
                }
                return;
            }
            KeyCode::Char('d') if ctrl => {
                if self.input.is_empty() {
                    self.request_quit();
                } else {
                    self.input.delete();
                }
                return;
            }
            KeyCode::Char('o') if ctrl && self.expand_paste_at_cursor() => {}
            KeyCode::Char('o') if ctrl => {
                self.toggle_transcript();
                return;
            }
            KeyCode::Char('r') if ctrl => {
                self.search = Some(Search {
                    transcript: false,
                    query: String::new(),
                    pos: usize::MAX,
                    orig: self.input.text.clone(),
                    found: true,
                });
                self.popup = None;
                return;
            }
            KeyCode::Char('f') if ctrl && self.inline.is_some() => {
                self.hint = Some(("The transcript is in your terminal's scrollback: search it with the terminal's own find.".into(), Instant::now()));
                return;
            }
            KeyCode::Char('f') if ctrl => {
                self.search = Some(Search {
                    transcript: true,
                    query: String::new(),
                    pos: 0,
                    orig: String::new(),
                    found: true,
                });
                self.popup = None;
                return;
            }
            KeyCode::Char('g') if ctrl => {
                self.open_editor = true;
                return;
            }
            KeyCode::Char('t') if ctrl => {
                self.show_todos = !self.show_todos;
                return;
            }
            KeyCode::F(2) | KeyCode::Char('s')
                if (k.code == KeyCode::F(2) || ctrl) && self.inline.is_some() =>
            {
                self.hint = Some((
                    "Inline mode leaves the mouse to your terminal: select and scroll as usual."
                        .into(),
                    Instant::now(),
                ));
                return;
            }
            KeyCode::F(2) | KeyCode::Char('s') if k.code == KeyCode::F(2) || ctrl => {
                // Toggle mouse capture so the terminal's native text selection works.
                self.mouse = !self.mouse;
                let mut out = std::io::stdout();
                let _ = if self.mouse {
                    execute!(out, EnableMouseCapture)
                } else {
                    execute!(out, DisableMouseCapture)
                };
                self.hint = Some((
                    if self.mouse {
                        "Mouse mode: click to expand, wheel to scroll".into()
                    } else {
                        "Selection mode: select text with the mouse (ctrl+s to return)".into()
                    },
                    Instant::now(),
                ));
                return;
            }
            KeyCode::Char('l') if ctrl => return,
            // `?` on an empty prompt opens the shortcuts the footer advertises.
            KeyCode::Char('?') if self.input.is_empty() && self.popup.is_none() => {
                self.help = true;
                return;
            }
            KeyCode::Char('v') if ctrl => {
                match clipboard_image() {
                    Some(p) => {
                        self.images.push(p);
                        let n = self.images.len();
                        self.input.insert(&format!("[Image #{n}] "));
                        self.hint =
                            Some((format!("Image #{n} pasted from clipboard"), Instant::now()));
                    }
                    None => {
                        self.hint = Some((
                            "No image in clipboard (use your terminal's paste for text)".into(),
                            Instant::now(),
                        ))
                    }
                }
                return;
            }
            KeyCode::BackTab => {
                self.cycle_mode();
                return;
            }
            KeyCode::Tab if shift => {
                self.cycle_mode();
                return;
            }
            KeyCode::Esc => {
                if self.popup.is_some() {
                    self.popup = None;
                } else if self.editing_prompt.take().is_some() {
                    self.input.take();
                    self.hint = Some(("Edit cancelled".into(), Instant::now()));
                } else if self.scroll > 0 {
                    // Checked before `busy`: the footer says "esc to jump to
                    // bottom", so being scrolled up must win over interrupting.
                    // Interrupting is destructive and still available once the
                    // view is back at the bottom (or via ctrl+c).
                    self.scroll = 0;
                } else if self.viewing.is_some() {
                    // Viewing a subagent: back to the main conversation first.
                    self.view_agent(None);
                } else if self.busy {
                    if self.loop_turn {
                        self.stop_loop();
                    }
                    self.interrupt();
                } else if self.input.is_empty() && self.env.scheduler.loop_active() {
                    self.stop_loop();
                } else if self.input.is_empty() {
                    // Double-esc opens the rewind picker.
                    if self
                        .esc_at
                        .map(|t| t.elapsed() < Duration::from_millis(600))
                        .unwrap_or(false)
                    {
                        self.esc_at = None;
                        self.open_picker("/rewind");
                    } else {
                        self.esc_at = Some(Instant::now());
                    }
                }
                return;
            }
            KeyCode::PageUp => {
                self.scroll += (self.transcript_area.height as usize)
                    .saturating_sub(2)
                    .max(1);
                return;
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(
                    (self.transcript_area.height as usize)
                        .saturating_sub(2)
                        .max(1),
                );
                return;
            }
            _ => {}
        }
        // Up on an empty prompt pulls waiting messages back to edit:
        // ones queued for after the turn, and ones sent mid-turn
        // that the model hasn't received yet.
        if k.code == KeyCode::Up && self.input.text.is_empty() && self.pull_back_waiting() {
            return;
        }
        // Popup navigation.
        if let Some(p) = &mut self.popup {
            match k.code {
                KeyCode::Up => {
                    p.sel = p.sel.checked_sub(1).unwrap_or(p.items.len() - 1);
                    return;
                }
                KeyCode::Down => {
                    p.sel = (p.sel + 1) % p.items.len();
                    return;
                }
                KeyCode::Tab => {
                    self.accept_popup();
                    return;
                }
                KeyCode::Enter if p.kind == PopupKind::File => {
                    self.accept_popup();
                    return;
                }
                _ => {}
            }
        }
        // Editing.
        match k.code {
            KeyCode::Enter if ctrl && !shift && !alt => self.submit_as(Deliver::Now),
            KeyCode::Enter if shift || alt => self.input.insert_char('\n'),
            KeyCode::Char('j') if ctrl => self.input.insert_char('\n'),
            KeyCode::Enter => {
                // An empty box while a turn is running means "run what's queued
                // now", not "send nothing".
                if self.busy && self.input.is_empty() && !self.queue.is_empty() {
                    self.send_queued_now();
                } else if self.input.text.ends_with('\\')
                    && self.input.cursor == self.input.text.len()
                {
                    self.input.backspace();
                    self.input.insert_char('\n');
                } else {
                    self.submit();
                }
            }
            KeyCode::Char('a') if ctrl => self.input.home(),
            KeyCode::Char('e') if ctrl => self.input.end(),
            KeyCode::Char('u') if ctrl => self.input.kill_to_start(),
            KeyCode::Char('k') if ctrl => self.input.kill_to_end(),
            KeyCode::Char('w') if ctrl => self.input.delete_word_back(),
            KeyCode::Backspace if alt || ctrl => self.input.delete_word_back(),
            KeyCode::Char('b') if alt => self.input.word_left(),
            KeyCode::Char('f') if alt => self.input.word_right(),
            KeyCode::Left if alt || ctrl => self.input.word_left(),
            KeyCode::Right if alt || ctrl => self.input.word_right(),
            KeyCode::Tab if self.input.is_empty() && self.suggestion.is_some() => {
                let s = self.suggestion.take().unwrap_or_default();
                self.input.set(&s);
            }
            KeyCode::Char(c) if !ctrl => self.input.insert_char(c),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Up => {
                if !self.input.up() {
                    self.scroll += 1;
                }
            }
            KeyCode::Down => {
                // At the bottom of an empty prompt, ↓ moves into the background panel.
                if self.input.is_empty() && self.scroll == 0 && !self.background_work().is_empty() {
                    self.work_sel = Some(0);
                    self.hint = Some((
                        "↑↓ select · enter view · x stop · esc back".into(),
                        Instant::now(),
                    ));
                } else if !self.input.down() {
                    self.scroll = self.scroll.saturating_sub(1);
                }
            }
            _ => return,
        }
        self.update_popup();
    }

    pub(super) fn on_search_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Some(s) = &mut self.search else { return };
        match k.code {
            KeyCode::Esc => {
                if !s.transcript {
                    let orig = s.orig.clone();
                    self.input.set(&orig);
                }
                self.search = None;
                self.highlight = None;
                return;
            }
            KeyCode::Char('c') if ctrl => {
                self.search = None;
                self.highlight = None;
                return;
            }
            KeyCode::Enter => {
                // Keep the match: history → in the input box; transcript → scrolled there.
                self.search = None;
                return;
            }
            // Next (older) match.
            KeyCode::Char('r') | KeyCode::Char('f') | KeyCode::Up
                if ctrl || k.code == KeyCode::Up =>
            {
                if s.transcript {
                    s.pos += 1;
                } else {
                    s.pos = s.pos.saturating_sub(1);
                }
            }
            KeyCode::Down if s.transcript => s.pos = s.pos.saturating_sub(1),
            KeyCode::Backspace => {
                s.query.pop();
                s.pos = if s.transcript { 0 } else { usize::MAX };
            }
            KeyCode::Char(c) if !ctrl => {
                s.query.push(c);
                s.pos = if s.transcript { 0 } else { usize::MAX };
            }
            _ => return,
        }
        self.run_search();
    }

    pub(super) fn run_search(&mut self) {
        let Some(s) = &mut self.search else { return };
        let q = s.query.to_lowercase();
        if q.is_empty() {
            s.found = true;
            self.highlight = None;
            return;
        }
        if !s.transcript {
            let hist = self.input.history();
            if hist.is_empty() {
                s.found = false;
                return;
            }
            // Matches, oldest first; pos indexes into history (usize::MAX = newest).
            let start = s.pos.min(hist.len().saturating_sub(1));
            let hit = (0..=start)
                .rev()
                .find(|&i| hist[i].to_lowercase().contains(&q));
            match hit {
                Some(i) => {
                    s.pos = i;
                    s.found = true;
                    let t = hist[i].clone();
                    self.input.set(&t);
                }
                None => s.found = false,
            }
            return;
        }
        // Transcript: walk every block's rendered lines and find the nth matching
        // line from the bottom. Borrowing the cached lines (rather than cloning
        // them, as `block_lines` does) keeps each keystroke cheap on a long
        // session.
        // Must match `draw_transcript`'s width exactly (including no floor), or
        // the wrapped line counts — and so the hit rows and scroll target — drift.
        let width = self.transcript_area.width.saturating_sub(1);
        let mut live = std::collections::HashMap::new();
        let mut total = 0usize;
        let mut hits: Vec<usize> = Vec::new();
        let quiet = self.quiet_mask();
        for i in 0..self.blocks.len() {
            if quiet.as_ref().is_some_and(|q| q.hides(i)) {
                continue;
            }
            let n = self.block_len(i, width, &mut live);
            let lines: &[Line<'static>] = match live.get(&i) {
                Some(v) => v.as_slice(),
                None => self.blocks[i]
                    .cache
                    .as_ref()
                    .map(|c| c.4.as_slice())
                    .unwrap_or(&[]),
            };
            for (k, l) in lines.iter().enumerate() {
                if l.spans
                    .iter()
                    .any(|sp| sp.content.to_lowercase().contains(&q))
                {
                    hits.push(total + k);
                }
            }
            total += n + 1;
        }
        // `hits` is in top-down order; search bottom-up.
        hits.reverse();
        let Some(s) = &mut self.search else { return };
        if hits.is_empty() {
            s.found = false;
            self.highlight = None;
            return;
        }
        s.found = true;
        s.pos = s.pos.min(hits.len() - 1);
        let from_bottom = total - 1 - hits[s.pos];
        self.highlight = Some(from_bottom);
        let h = self.transcript_area.height as usize;
        self.scroll = from_bottom.saturating_sub(h / 2);
    }

    /// Handle a key in vim normal mode. Deliberately a small, useful subset —
    /// motions, the common operators, and `u` to undo — rather than a full vim.
    pub(super) fn vim_normal_key(&mut self, k: KeyEvent) {
        let KeyCode::Char(c) = k.code else {
            match k.code {
                KeyCode::Left => self.input.left(),
                KeyCode::Right => self.input.right(),
                KeyCode::Up => {
                    self.input.up();
                }
                KeyCode::Down => {
                    self.input.down();
                }
                KeyCode::Home => self.input.home(),
                KeyCode::End => self.input.end(),
                KeyCode::Backspace => self.input.left(),
                KeyCode::Enter => self.submit(),
                _ => {}
            }
            return;
        };
        // A pending `d` completes dd (clear) or dw (delete one word back).
        if let Some(p) = self.vim_pending.take()
            && p == 'd'
        {
            match c {
                'd' => {
                    self.vim_snapshot();
                    self.input.set("");
                }
                'w' => {
                    self.vim_snapshot();
                    self.input.delete_word_back();
                }
                '$' => {
                    self.vim_snapshot();
                    self.input.kill_to_end();
                }
                _ => {}
            }
            return;
        }
        match c {
            'h' => self.input.left(),
            'l' => self.input.right(),
            'j' => {
                self.input.down();
            }
            'k' => {
                self.input.up();
            }
            '0' => self.input.home(),
            '$' => self.input.end(),
            'w' => self.input.word_right(),
            'b' => self.input.word_left(),
            'i' => {
                self.vim_snapshot();
                self.vim_insert = true;
            }
            'I' => {
                self.vim_snapshot();
                self.input.home();
                self.vim_insert = true;
            }
            'a' => {
                self.vim_snapshot();
                self.input.right();
                self.vim_insert = true;
            }
            'A' => {
                self.vim_snapshot();
                self.input.end();
                self.vim_insert = true;
            }
            'x' => {
                self.vim_snapshot();
                self.input.delete();
            }
            'D' => {
                self.vim_snapshot();
                self.input.kill_to_end();
            }
            'u' => {
                if let Some(prev) = self.vim_undo.pop() {
                    self.input.set(&prev);
                }
            }
            'd' => self.vim_pending = Some('d'),
            '\n' | '\r' => self.submit(),
            _ => {}
        }
    }

    /// Remember the prompt before a vim edit so `u` can restore it.
    pub(super) fn vim_snapshot(&mut self) {
        if self
            .vim_undo
            .last()
            .map(|s| *s == self.input.text)
            .unwrap_or(false)
        {
            return;
        }
        self.vim_undo.push(self.input.text.clone());
        if self.vim_undo.len() > 100 {
            self.vim_undo.remove(0);
        }
    }

    pub(super) fn on_dialog_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Some(d) = &mut self.dialog else { return };
        // An `/mcp` action runs after the dialog borrow ends (it may open a
        // browser), so key handling only records it here.
        let mut run: Option<String> = None;
        match d {
            Dialog::Btw { index, scroll } => {
                let answered = self.env.btw.lock().unwrap().len();
                // A question still in flight sits after the answered ones.
                let count = answered + usize::from(self.btw_ask.is_some());
                match k.code {
                    KeyCode::Left | KeyCode::Char('[') | KeyCode::BackTab => {
                        *index = index.saturating_sub(1);
                        *scroll = 0;
                    }
                    KeyCode::Right | KeyCode::Char(']') | KeyCode::Tab => {
                        *index = (*index + 1).min(count.saturating_sub(1));
                        *scroll = 0;
                    }
                    KeyCode::PageDown | KeyCode::Down => {
                        *scroll += if k.code == KeyCode::Down { 1 } else { 5 }
                    }
                    KeyCode::PageUp | KeyCode::Up => {
                        *scroll = scroll.saturating_sub(if k.code == KeyCode::Up { 1 } else { 5 })
                    }
                    // Forking or clearing needs an answer, so both wait for one.
                    KeyCode::Char('f') if *index < answered => {
                        let i = *index;
                        self.dialog = None;
                        self.run_env_command(format!("/btw fork {}", i + 1));
                    }
                    KeyCode::Char('x') if *index < answered => {
                        self.env.btw.lock().unwrap().clear();
                        self.dialog = None;
                        self.hint = Some(("Side questions cleared".into(), Instant::now()));
                    }
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('q') => {
                        self.dialog = None
                    }
                    _ => {}
                }
            }
            Dialog::Mcp {
                sel,
                scroll,
                menu,
                note,
            } => {
                let rows = crate::commands::mcp_rows(&self.env);
                let count = rows.len();
                *sel = (*sel).min(count.saturating_sub(1));
                match menu {
                    Some(m) => {
                        let actions = rows.get(*sel).map(mcp_actions).unwrap_or_default();
                        match k.code {
                            KeyCode::Up | KeyCode::Char('k') => *m = m.saturating_sub(1),
                            KeyCode::Down | KeyCode::Char('j') => {
                                *m = (*m + 1).min(actions.len().saturating_sub(1))
                            }
                            KeyCode::Esc => *menu = None,
                            KeyCode::Enter => {
                                if let Some((label, cmd)) = actions.get(*m) {
                                    *note = Some(format!("{label}…"));
                                    run = Some(cmd.clone());
                                }
                            }
                            _ => {}
                        }
                    }
                    None => match k.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            *sel = sel.saturating_sub(1);
                            *scroll = (*scroll).min(*sel as u16);
                            *note = None;
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            *sel = (*sel + 1).min(count.saturating_sub(1));
                            if (*sel as u16) >= *scroll + 8 {
                                *scroll = *sel as u16 - 7;
                            }
                            *note = None;
                        }
                        KeyCode::PageDown => *scroll += 5,
                        KeyCode::PageUp => *scroll = scroll.saturating_sub(5),
                        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                            if rows.get(*sel).is_some_and(|r| !mcp_actions(r).is_empty()) {
                                *menu = Some(0);
                            }
                        }
                        KeyCode::Esc | KeyCode::Char('q') => self.dialog = None,
                        _ => {}
                    },
                }
            }
            Dialog::Permission {
                req,
                sel,
                tx,
                feedback,
                scroll,
                ..
            } => {
                let editable = req.proposed.is_some();
                if let Some(fb) = feedback {
                    match k.code {
                        KeyCode::Enter => {
                            let t = fb.take();
                            let reply = if t.trim().is_empty() {
                                PermissionReply::Deny(None)
                            } else {
                                PermissionReply::Deny(Some(t))
                            };
                            if let Some(tx) = tx.take() {
                                let _ = tx.send(reply);
                            }
                            self.dialog = None;
                        }
                        KeyCode::Esc => *feedback = None,
                        KeyCode::Backspace => fb.backspace(),
                        KeyCode::Left => fb.left(),
                        KeyCode::Right => fb.right(),
                        KeyCode::Char(c) if !ctrl => fb.insert_char(c),
                        _ => {}
                    }
                    return;
                }
                let choose = |i: usize| match i {
                    0 => Some(PermissionReply::Allow),
                    1 => Some(PermissionReply::AllowAlways),
                    _ => None,
                };
                let pick = match k.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        *sel = sel.saturating_sub(1);
                        None
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        *sel = (*sel + 1).min(2);
                        None
                    }
                    KeyCode::PageDown => {
                        *scroll += 5;
                        None
                    }
                    KeyCode::PageUp => {
                        *scroll = scroll.saturating_sub(5);
                        None
                    }
                    KeyCode::Char('1') | KeyCode::Char('y') => Some(0),
                    KeyCode::Char('2') | KeyCode::Char('a') => Some(1),
                    KeyCode::Char('3') | KeyCode::Char('n') => Some(2),
                    // Open the proposed file content in $VISUAL/$EDITOR; what's saved gets written.
                    KeyCode::Char('e') if !ctrl => {
                        self.edit_proposal = editable;
                        None
                    }
                    KeyCode::Enter => Some(*sel),
                    KeyCode::Esc => Some(99),
                    KeyCode::Char('c') if ctrl => Some(99),
                    KeyCode::BackTab => {
                        // Switching to accept-edits/auto from a prompt approves this one too.
                        Some(100)
                    }
                    _ => None,
                };
                if let Some(i) = pick {
                    if i == 2 {
                        *feedback = Some(Input::default());
                        return;
                    }
                    let reply = if i == 100 {
                        let m = self.env.mode().next();
                        self.env.set_mode(m);
                        self.mode = m;
                        if !matches!(m, Mode::AcceptEdits | Mode::Auto | Mode::Bypass) {
                            // Switching to a stricter mode doesn't answer the prompt.
                            return;
                        }
                        PermissionReply::Allow
                    } else {
                        choose(i).unwrap_or(PermissionReply::Deny(None))
                    };
                    let interrupt = reply == PermissionReply::Deny(None);
                    if let Some(Dialog::Permission { tx, .. }) = &mut self.dialog
                        && let Some(tx) = tx.take()
                    {
                        let _ = tx.send(reply);
                    }
                    self.dialog = None;
                    if interrupt {
                        self.queue.clear();
                    }
                }
            }
            Dialog::Question {
                q,
                options,
                sel,
                tx,
                typing,
                scroll,
                free_text,
                search,
                action,
                ..
            } => {
                if let Some(s) = search {
                    // `/resume`'s row actions: the chosen row with what to do to it.
                    let action = match k.code {
                        KeyCode::Char('d') if ctrl && s.sessions => Some("delete"),
                        KeyCode::Char('a') if ctrl && s.sessions => Some("archive"),
                        KeyCode::Tab if s.sessions => Some("toggle-archived"),
                        KeyCode::Char('s') if ctrl && !s.sessions => Some("session"),
                        _ => None,
                    };
                    if let Some(action) = action {
                        let row = if action == "toggle-archived" {
                            Some(usize::MAX)
                        } else {
                            s.indices.get(*sel).copied()
                        };
                        if let (Some(i), Some(tx)) = (row, tx.take()) {
                            let _ = tx.send(Some((i, action.into())));
                        }
                        self.dialog = None;
                        return;
                    }
                    match k.code {
                        KeyCode::Enter => {
                            if let Some(&i) = s.indices.get(*sel) {
                                if let Some(tx) = tx.take() {
                                    let _ = tx.send(Some((i, String::new())));
                                }
                                self.dialog = None;
                            } else if !s.sessions && !s.query.trim().is_empty() {
                                if let Some(tx) = tx.take() {
                                    let _ = tx.send(Some((usize::MAX, s.query.clone())));
                                }
                                self.dialog = None;
                            }
                            return;
                        }
                        KeyCode::Char(c) if !ctrl => s.query.push(c),
                        KeyCode::Backspace => {
                            s.query.pop();
                        }
                        _ => {}
                    }
                    if matches!(k.code, KeyCode::Char(_) | KeyCode::Backspace) && !ctrl {
                        s.refilter();
                        *options = s.indices.iter().map(|i| s.all[*i].clone()).collect();
                        *sel = 0;
                        *q = s.heading();
                        return;
                    }
                }
                if let Some(t) = typing {
                    match k.code {
                        KeyCode::Enter => {
                            let s = t.take();
                            if let Some(tx) = tx.take() {
                                let _ = tx.send(if s.trim().is_empty() {
                                    None
                                } else {
                                    Some((usize::MAX, s))
                                });
                            }
                            self.dialog = None;
                        }
                        KeyCode::Esc => *typing = None,
                        KeyCode::Backspace => t.backspace(),
                        KeyCode::Left => t.left(),
                        KeyCode::Right => t.right(),
                        KeyCode::Char(c) if !ctrl => t.insert_char(c),
                        _ => {}
                    }
                    return;
                }
                let n = options.len();
                // Typing a letter starts a free-text answer (digits pick options).
                if let KeyCode::Char(c) = k.code
                    && *free_text
                    && !ctrl
                    && !c.is_ascii_digit()
                {
                    let mut t = Input::default();
                    t.insert_char(c);
                    *typing = Some(t);
                    return;
                }
                let pick = match k.code {
                    KeyCode::Up => {
                        *sel = sel.saturating_sub(1);
                        None
                    }
                    KeyCode::Down => {
                        *sel = (*sel + 1).min(n.saturating_sub(1));
                        None
                    }
                    KeyCode::PageDown => {
                        *scroll += 5;
                        None
                    }
                    KeyCode::PageUp => {
                        *scroll = scroll.saturating_sub(5);
                        None
                    }
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        let i = c.to_digit(10).unwrap() as usize;
                        if i >= 1 && i <= n { Some(i - 1) } else { None }
                    }
                    KeyCode::Enter => Some(*sel),
                    KeyCode::Esc => Some(usize::MAX),
                    _ => None,
                };
                if let Some(i) = pick {
                    if i == n.saturating_sub(1) && *free_text {
                        *typing = Some(Input::default());
                        return;
                    }
                    let ans = options.get(i).cloned().map(|o| (i, o));
                    if let Some(tx) = tx.take() {
                        let _ = tx.send(ans);
                    }
                    // A local confirmation: its first option is the one that acts.
                    if *action == Some(QuestionAction::Quit) && i == 0 {
                        self.quit = true;
                    }
                    self.dialog = None;
                }
            }
        }
        if let Some(cmd) = run {
            self.run_mcp_action(cmd);
        }
    }
}
