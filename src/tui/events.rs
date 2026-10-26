//! Handling what arrives while the UI runs: agent events (streamed text, tool
//! calls, prompts) and messages from the input side (keys, questions, timers).

use super::*;

impl App {
    pub(super) fn close_stream(&mut self) {
        self.open_text = None;
        self.close_think();
    }

    /// Finish the open reasoning block, freezing how long it ran. Every path that
    /// stops the thinking stream must go through here, otherwise the block is
    /// stuck reading "Thinking…" after the model has moved on.
    pub(super) fn close_think(&mut self) {
        if let Some(i) = self.open_think.take() {
            if let Kind::Thinking {
                started, elapsed, ..
            } = &mut self.blocks[i].kind
            {
                *elapsed = Some(started.elapsed());
            }
            self.blocks[i].touch();
        }
    }

    /// ctrl+o and `/transcript`: expand every block, which in the quiet view
    /// also shows the steps it hides. Inline: printed blocks can't open in
    /// place, so the last one prints again, expanded (quiet: the last turn's
    /// hidden steps).
    pub(super) fn toggle_transcript(&mut self) {
        if let Some(iv) = self.inline.as_mut() {
            iv.reprint = true;
            return;
        }
        self.expand_all = !self.expand_all;
        if self.quiet {
            let msg = if self.expand_all {
                "Showing every step (ctrl+o hides them again)"
            } else {
                "Quiet view: steps hidden (ctrl+o shows them)"
            };
            self.hint = Some((msg.into(), Instant::now()));
        }
    }

    /// The quiet view's mask for this frame, or `None` when everything shows
    /// (quiet off, or ctrl+o expanded it; inline mode reprints instead).
    pub(super) fn quiet_mask(&self) -> Option<QuietMask> {
        (self.quiet && (self.inline.is_some() || !self.expand_all))
            .then(|| quiet_mask(&self.blocks, self.busy))
    }

    pub(super) fn find_tool(&mut self, id: &str) -> Option<&mut ToolBlock> {
        self.blocks
            .iter_mut()
            .rev()
            .find_map(|b| match &mut b.kind {
                Kind::Tool(t) if t.id == id => {
                    b.version += 1;
                    Some(t)
                }
                _ => None,
            })
    }

    pub(super) fn on_agent(&mut self, ev: Event) {
        match ev {
            Event::AssistantStart => self.close_stream(),
            Event::Text(t) => {
                self.stream_chars += t.len();
                self.busy_label = Some("Responding".into());
                match self.open_text {
                    Some(i) => {
                        if let Kind::Assistant(s) = &mut self.blocks[i].kind {
                            s.push_str(&t);
                        }
                        self.blocks[i].touch();
                    }
                    None => {
                        // The model moved from reasoning into its answer: close
                        // the reasoning block so it stops claiming to be thinking.
                        self.close_think();
                        let t = t.trim_start().to_string();
                        if !t.is_empty() {
                            let i = self.push(Kind::Assistant(t));
                            self.open_text = Some(i);
                        }
                    }
                }
            }
            Event::Reasoning(t) => {
                // Collapsed by default. `show_thinking = false`
                // hides the reasoning block entirely.
                if self.env.config.show_thinking == Some(false) {
                    return;
                }
                self.stream_chars += t.len();
                match self.open_think {
                    Some(i) => {
                        if let Kind::Thinking { text, .. } = &mut self.blocks[i].kind {
                            text.push_str(&t);
                        }
                        self.blocks[i].touch();
                    }
                    None => {
                        let i = self.push(Kind::Thinking {
                            text: t.trim_start().to_string(),
                            started: Instant::now(),
                            elapsed: None,
                        });
                        self.open_think = Some(i);
                    }
                }
            }
            Event::AssistantEnd => self.close_stream(),
            Event::ToolStart { id, label, parent } => {
                self.close_stream();
                match parent {
                    Some(p) => {
                        if let Some(t) = self.find_tool(&p) {
                            t.children.push((id, label, None));
                        }
                    }
                    None => {
                        self.turn_tools += 1;
                        self.current_tool = Some(label.clone());
                        self.push(Kind::Tool(ToolBlock {
                            id,
                            label,
                            display: None,
                            content: None,
                            is_error: false,
                            children: vec![],
                            started: Instant::now(),
                            elapsed: None,
                            progress: None,
                        }));
                    }
                }
            }
            Event::ToolEnd {
                id,
                display,
                content,
                is_error,
                parent,
                ..
            } => match parent {
                Some(p) => {
                    if let Some(t) = self.find_tool(&p)
                        && let Some(c) = t.children.iter_mut().find(|c| c.0 == id)
                    {
                        c.2 = Some(is_error);
                    }
                }
                None => {
                    self.current_tool = None;
                    if let Some(t) = self.find_tool(&id) {
                        t.display = Some(display);
                        t.content = Some(content);
                        t.is_error = is_error;
                        t.elapsed = Some(t.started.elapsed());
                    }
                }
            },
            Event::ToolProgress { id, text } => {
                if let Some(t) = self.find_tool(&id) {
                    t.progress = Some(text);
                }
            }
            Event::Busy(l) => {
                if let Some(l) = l {
                    self.busy_label = Some(l.trim_end_matches('…').to_string());
                }
            }
            Event::Info(s) => self.note(s, pal().dim),
            Event::Warn(s) => self.note(s, warn()),
            Event::Error(s) => self.note(s, pal().error),
            Event::Todos(t) => {
                self.todos = t;
                self.show_todos = true;
            }
            Event::Usage {
                context_used,
                context_max,
                total_cost_usd,
                price_unknown,
                tps,
                ..
            } => {
                self.total_cost_usd = total_cost_usd;
                self.price_unknown = price_unknown;
                self.ctx_used = context_used;
                self.ctx_max = context_max;
                if tps > 0.0 {
                    self.tps = tps;
                }
            }
            Event::ModeChanged(m) => self.mode = m,
            Event::Background {
                id,
                description,
                status,
            } => {
                let icon = match status.as_str() {
                    "running" => "⧉",
                    "completed" => "✓",
                    _ => "✗",
                };
                self.note(
                    format!("{icon} Background agent {id} ({description}): {status}"),
                    pal().dim,
                );
                if status != "running" && !self.busy {
                    self.deliver_notifications();
                }
            }
            Event::Compacted { before, after } => self.note(
                format!(
                    "❖ Conversation compacted ({before} → {after} tokens). ctrl+o to see history."
                ),
                pal().dim,
            ),
            // Keep the footer current and warn once at each low-quota threshold.
            Event::Limits(s) => {
                // Say so once as a window passes 75% and 90%;
                // the status line alone is easy to miss.
                let now = chrono::Utc::now().timestamp();
                for w in [&s.primary, &s.secondary].into_iter().flatten() {
                    let warned = self.limit_warned.entry(w.label()).or_insert(0);
                    let new_window = self.limits.as_ref().into_iter()
                        .flat_map(|old| [&old.primary, &old.secondary]).flatten()
                        .find(|old| old.label() == w.label())
                        .is_some_and(|old| matches!((old.resets_at, w.resets_at), (Some(a), Some(b)) if a <= now && b > now));
                    if w.used_percent < 75.0 || new_window {
                        *warned = 0;
                    }
                    if let Some((level, msg)) = crate::llm::limits::warning(w, *warned, now) {
                        *warned = level;
                        let color = if level >= 90 { pal().error } else { warn() };
                        self.note(format!("⚠ {msg}"), color);
                    }
                }
                self.limits = Some(s);
            }
            Event::Status { key, lines } => {
                self.statuses.retain(|(k, _)| *k != key);
                if !lines.is_empty() {
                    self.statuses.push((key, lines));
                }
            }
        }
    }

    pub(super) fn on_msg(&mut self, m: AppMsg) {
        match m {
            AppMsg::Agent(e) => self.on_agent(e),
            AppMsg::Permission(req, tx) => {
                let what = format!("Needs your permission: {}", req.label);
                let deadline = req.auto_deny.map(|d| Instant::now() + d);
                self.open_dialog(Dialog::Permission {
                    req,
                    sel: 0,
                    tx: Some(tx),
                    feedback: None,
                    scroll: 0,
                    deadline,
                });
                self.want_attention(what);
            }
            AppMsg::Question(q, mut options, free_text, tx) => {
                let what = format!("Has a question: {q}");
                // Nothing to pick from: the answer box is open from the start.
                let typing = (free_text && options.is_empty()).then(Input::default);
                if free_text {
                    options.push("Type something else…".into());
                }
                self.open_dialog(Dialog::Question {
                    q,
                    options,
                    sel: 0,
                    tx: Some(tx),
                    typing,
                    scroll: 0,
                    free_text,
                    action: None,
                    search: None,
                });
                self.want_attention(what);
            }
            AppMsg::Completions(items) => {
                self.completions = items;
                self.update_popup();
            }
            AppMsg::Show(md) => {
                crate::remote::show(&md);
                self.close_stream();
                self.push(Kind::Markdown(md));
            }
            AppMsg::Reset(msg) => {
                crate::remote::reset();
                // Drop stream pointers along with the blocks: a dangling
                // `open_think`/`open_text` index would panic on the next
                // render or stream event.
                self.blocks.clear();
                // Inline: scrollback can't be rewritten, so the new state (a fresh
                // welcome, or the conversation as it now stands) prints below it.
                if let Some(iv) = self.inline.as_mut() {
                    *iv = InlineView::default();
                }
                self.open_think = None;
                self.open_text = None;
                self.todos.clear();
                // Pending paste/image tags refer to state (and temp files) tied to
                // the conversation that just went away.
                self.pastes.clear();
                self.images.clear();
                self.scroll = 0;
                let (model, cwd) = (self.env.model(), crate::util::tilde(&self.env.cwd()));
                self.push(Kind::Welcome { model, cwd });
                if let Ok(a) = self.agent.try_lock() {
                    let blocks = blocks_from_messages(&a);
                    self.blocks.extend(blocks);
                }
                if let Some(m) = msg {
                    self.note(m, pal().dim);
                }
            }
            AppMsg::TurnDone(err) => {
                crate::remote::turn_done(err.as_deref());
                self.sync_wait();
                let took = self.busy_since.elapsed().as_secs();
                // Only when nobody is (known to be) watching.
                if self.busy
                    && took >= self.env.config.notify_after_secs.unwrap_or(30)
                    && self.env.config.notify_after_secs != Some(0)
                    && self.focused != Some(true)
                {
                    let what = if err.is_some() {
                        "Stopped with an error"
                    } else {
                        "Done"
                    };
                    notify::send(
                        self.notify,
                        "chanto",
                        &format!("{what} after {}", crate::util::duration(took)),
                    );
                    notify::play_sound(self.env.config.notify_sound.as_deref());
                }
                self.busy = false;
                if std::mem::take(&mut self.loop_turn)
                    && let Some(n) = self.env.scheduler.iteration_done()
                {
                    self.note(n, pal().dim);
                }
                self.idle_since = Some(Instant::now());
                self.idle_notified = false;
                self.idle_compacted = false;
                self.turns_since_recap += 1;
                self.turned = true;
                self.refresh_status_line();
                self.cancel = None;
                self.close_stream();
                // Drop only prompts nobody is waiting for any more (the finished
                // turn's); background agents may still be waiting on theirs.
                let live = |d: &Dialog| match d {
                    Dialog::Permission { tx, .. } => tx.as_ref().is_some_and(|t| !t.is_closed()),
                    Dialog::Question { tx, .. } => tx.as_ref().is_some_and(|t| !t.is_closed()),
                    Dialog::Btw { .. } => true,
                    Dialog::Mcp { .. } => true,
                };
                if self.dialog.as_ref().is_some_and(|d| !live(d)) {
                    self.dialog = None;
                }
                self.pending_dialogs.retain(|d| live(d));
                let failed = err.is_some();
                self.turn_failed = failed;
                if let Some(e) = err {
                    self.note(e, pal().error);
                }
                self.model = self.env.model();
                self.branch = git_branch(&self.env.cwd());
                let send_now = std::mem::take(&mut self.send_now);
                self.steered.clear();
                if send_now && !self.queue.is_empty() {
                    // Undelivered step messages stay in the inbox and reach the
                    // model at the next turn's first tool boundary (or its end).
                    let (next, images) = self.queue.pop_front().unwrap();
                    self.images = images;
                    self.start(next);
                    return;
                }
                let mut leftover: Vec<String> = self
                    .env
                    .inbox
                    .lock()
                    .unwrap()
                    .drain(..)
                    .map(|q| q.text)
                    .collect();
                // The user typed these; the notifications joined to them aren't.
                for t in &leftover {
                    self.env.record_user_request(t);
                }
                leftover.extend(self.env.notifications.lock().unwrap().drain(..));
                if !leftover.is_empty() {
                    // The turn ended before these were delivered: send them now.
                    self.start_hidden(leftover.join("\n\n"), true);
                } else if let Some((next, images)) = self.queue.pop_front() {
                    self.images = images;
                    self.start(next);
                } else if self.env.config.prompt_suggestions == Some(true) && !failed {
                    self.suggest_next_prompt();
                }
            }
            AppMsg::Exit => self.request_quit(),
            AppMsg::Terminate => {
                self.interrupt();
                self.quit = true;
            }
            AppMsg::Picker {
                title,
                labels,
                values,
            } => {
                if labels.is_empty() {
                    self.note(format!("{title}: nothing to choose from"), pal().dim);
                    return;
                }
                let (tx, rx) = oneshot::channel::<Option<(usize, String)>>();
                let app_tx = self.tx.clone();
                tokio::spawn(async move {
                    if let Ok(Some((i, action))) = rx.await {
                        // A model name typed in the search that isn't in the list.
                        if i == usize::MAX && !action.trim().is_empty() {
                            let _ =
                                app_tx.send(AppMsg::Submit(format!("/model {}", action.trim())));
                            return;
                        }
                        if let Some(v) = values.get(i) {
                            let value = if action == "session" {
                                v.replacen("/model ", "/model --session ", 1)
                            } else {
                                v.clone()
                            };
                            let _ = app_tx.send(AppMsg::Submit(value));
                        }
                    }
                });
                let search = title
                    .starts_with("Switch model")
                    .then(|| PickerSearch::new(&labels, labels.clone(), false));
                // Open on the current choice, as a settings menu does.
                let sel = labels
                    .iter()
                    .position(|l| l.ends_with("(current)"))
                    .unwrap_or(0);
                self.open_dialog(Dialog::Question {
                    q: format!("**{title}**"),
                    options: labels,
                    sel,
                    tx: Some(tx),
                    typing: None,
                    scroll: 0,
                    free_text: false,
                    search,
                    action: None,
                });
            }
            AppMsg::Suggestion(s) => {
                if self.input.is_empty() && !self.busy {
                    self.suggestion = Some(s);
                }
            }
            AppMsg::OpenBtw => {
                // The answer landed, so the panel's progress state is done.
                self.btw_ask = None;
                let n = self.env.btw.lock().unwrap().len();
                if n > 0 {
                    self.show_btw(n - 1);
                }
            }
            AppMsg::BtwFailed(s) => {
                // The command's message already names the failure; don't repeat it.
                let s = s
                    .trim()
                    .strip_prefix("Side question failed: ")
                    .unwrap_or(s.trim())
                    .to_string();
                let Some(ask) = &mut self.btw_ask else {
                    self.note(format!("Side question failed: {s}"), pal().error);
                    return;
                };
                ask.error = Some(s);
                // The panel usually still shows the ask; if it was closed while
                // waiting, say so in the transcript rather than popping it open.
                if !matches!(self.dialog, Some(Dialog::Btw { .. })) {
                    let msg = format!(
                        "Side question failed: {}",
                        ask.error.as_deref().unwrap_or("")
                    );
                    self.btw_ask = None;
                    self.note(msg, pal().error);
                }
            }
            AppMsg::McpNote(s) => {
                if let Some(Dialog::Mcp { menu, note, .. }) = &mut self.dialog {
                    *menu = None;
                    *note = Some(s);
                } else {
                    // The panel was closed while the action ran: keep the result.
                    self.note(s, pal().dim);
                }
            }
            AppMsg::StatusLine(s) => {
                self.status_text = if s.trim().is_empty() { None } else { Some(s) }
            }
            AppMsg::VoiceStarted(r) => match (std::mem::replace(&mut self.voice, Voice::Off), r) {
                (Voice::Starting(backend), Ok(rec)) => {
                    self.voice = Voice::Recording(Box::new(rec), backend)
                }
                // Cancelled while starting: dropping the recording discards it.
                (other, Ok(_)) => self.voice = other,
                (_, Err(e)) => self.note(format!("Voice input: {e}"), pal().error),
            },
            AppMsg::Voice(r) => {
                self.voice = Voice::Off;
                match r {
                    Ok(text) if text.trim().is_empty() => {
                        self.hint = Some(("No speech recognized".into(), Instant::now()))
                    }
                    Ok(text) => {
                        // Into the prompt for review; never sent on its own.
                        if !self.input.text[..self.input.cursor]
                            .chars()
                            .last()
                            .is_none_or(char::is_whitespace)
                        {
                            self.input.insert(" ");
                        }
                        self.input.insert(text.trim());
                        self.update_popup();
                    }
                    Err(e) => self.note(format!("Voice input: {e}"), pal().error),
                }
            }
            AppMsg::Remote(crate::remote::Input::Interrupt) => self.interrupt(),
            AppMsg::Remote(crate::remote::Input::Prompt(text)) => {
                // Commands, `!shell` and `#memory` behave as typed: queued while busy.
                if text.starts_with(['/', '!', '#']) {
                    self.on_msg(AppMsg::Submit(text));
                } else if self.busy {
                    // Like enter while the model works: delivered at its next step.
                    self.close_stream();
                    self.push(Kind::User(text.clone()));
                    self.env.inbox.lock().unwrap().push(Queued::new(text));
                } else {
                    self.start(text);
                }
            }
            AppMsg::Submit(cmd) => {
                if self.busy {
                    if is_mcp_panel(&cmd) {
                        // The panel is only a view; open it mid-turn too.
                        self.open_mcp();
                    } else if crate::commands::is_immediate(&cmd) {
                        // Settings commands only need the environment, so run them now
                        // rather than hiding them behind the running turn.
                        self.run_env_command(cmd);
                    } else {
                        let images = std::mem::take(&mut self.images);
                        self.queue.push_back((cmd, images));
                    }
                } else {
                    self.start(cmd);
                }
            }
        }
    }
}
