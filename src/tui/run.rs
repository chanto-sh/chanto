//! Running the TUI: terminal setup and teardown, the event loop, and inline
//! mode's scrollback region.

use super::*;

/// Set once at startup when running inline (no alternate screen, no mouse capture).
static INLINE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(super) fn restore_terminal(kb_enh: bool) {
    let mut out = std::io::stdout();
    if kb_enh {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    if INLINE.load(std::sync::atomic::Ordering::Relaxed) {
        // Leaving an alternate screen we never entered would restore a stale cursor.
        let _ = execute!(
            out,
            DisableBracketedPaste,
            DisableFocusChange,
            crossterm::terminal::SetTitle(""),
            crossterm::cursor::Show
        );
        let _ = disable_raw_mode();
        return;
    }
    let _ = execute!(
        out,
        DisableMouseCapture,
        DisableBracketedPaste,
        DisableFocusChange,
        LeaveAlternateScreen,
        // Hand the tab back to the shell rather than leaving our ✓ behind.
        crossterm::terminal::SetTitle(""),
        crossterm::cursor::Show
    );
    let _ = disable_raw_mode();
}

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Inline mode's drawing surface: a region of `h` rows starting at row `top`,
/// with printed output above it. Lines are printed by drawing them where the
/// region was and scrolling the screen with line feeds, which (unlike scroll
/// escape sequences) every terminal keeps in its scrollback.
struct Region {
    top: u16,
    h: u16,
    w: u16,
    rows: u16,
    term: Term,
}

impl Region {
    fn new() -> Result<Region> {
        let (w, rows) = crossterm::terminal::size()?;
        // Start where the shell left the cursor; at the bottom if that can't be read.
        let top = crossterm::cursor::position()
            .map(|p| p.1)
            .unwrap_or(rows.saturating_sub(1))
            .min(rows.saturating_sub(1));
        let term = Self::fixed(Rect::new(0, top, w, 1))?;
        let mut r = Region {
            top,
            h: 1,
            w,
            rows,
            term,
        };
        r.reset()?;
        Ok(r)
    }

    fn fixed(area: Rect) -> Result<Term> {
        Ok(Terminal::with_options(
            CrosstermBackend::new(std::io::stdout()),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(area),
            },
        )?)
    }

    /// Clear from the region's top down and start a fresh terminal on it: a new
    /// terminal's empty back buffer would otherwise leave stale cells unwritten.
    fn reset(&mut self) -> Result<()> {
        self.clear_from(self.top)?;
        self.term = Self::fixed(Rect::new(0, self.top, self.w, self.h))?;
        Ok(())
    }

    /// Blank the rows from `y` to the bottom one line at a time. Erasing to the
    /// end of the screen from the top row counts as clearing the screen, which
    /// tmux (scroll-on-clear) and some terminals first copy into scrollback.
    fn clear_from(&self, y: u16) -> Result<()> {
        let mut out = std::io::stdout();
        for row in y..self.rows {
            crossterm::queue!(
                out,
                crossterm::cursor::MoveTo(0, row),
                crossterm::terminal::Clear(crossterm::terminal::ClearType::CurrentLine)
            )?;
        }
        use std::io::Write;
        out.flush()?;
        Ok(())
    }

    /// Scroll the whole screen up `n` rows; the top rows go to scrollback.
    fn scroll(&self, n: u16) -> Result<()> {
        if n > 0 {
            use std::io::Write;
            let mut out = std::io::stdout();
            crossterm::queue!(
                out,
                crossterm::cursor::MoveTo(0, self.rows.saturating_sub(1))
            )?;
            out.write_all("\n".repeat(n as usize).as_bytes())?;
            out.flush()?;
        }
        Ok(())
    }

    /// Print finished lines above the region.
    fn print(&mut self, lines: &[Line<'static>]) -> Result<()> {
        let mut rest = lines;
        while !rest.is_empty() {
            // Room below the region: draw there and move the region down.
            // At the bottom: scroll up, then draw in the rows that opened above it.
            let below = self.rows.saturating_sub(self.top + self.h);
            // Clamp in usize before narrowing: a huge print must still advance.
            let (y, n) = if below > 0 {
                let n = rest.len().min(below as usize) as u16;
                (self.top, n)
            } else {
                let n = rest
                    .len()
                    .min(self.rows.saturating_sub(self.h).max(1) as usize)
                    as u16;
                self.scroll(n)?;
                self.top = self.top.saturating_sub(n);
                (self.top, n)
            };
            self.clear_from(y)?;
            let mut t = Self::fixed(Rect::new(0, y, self.w, n))?;
            let chunk: Vec<Line<'static>> = rest[..n as usize].to_vec();
            let links = link_regions(&chunk, self.w);
            t.draw(|f| {
                let area = f.area();
                f.render_widget(Paragraph::new(chunk), area);
                paint_links(f.buffer_mut(), area, &links);
            })?;
            drop(t);
            self.top = y + n;
            rest = &rest[n as usize..];
        }
        self.reset()
    }

    /// Grow or shrink the region; growing past the bottom scrolls printed text up.
    fn set_height(&mut self, h: u16) -> Result<()> {
        let h = h.clamp(1, self.rows.saturating_sub(1).max(1));
        if h == self.h {
            return Ok(());
        }
        let over = (self.top + h).saturating_sub(self.rows);
        self.scroll(over)?;
        self.top -= over;
        self.h = h;
        self.reset()
    }

    /// After a resize the terminal has rewrapped what was on screen, the old
    /// region included, so nothing below the scrollback is where it was. Blank the
    /// screen and start at the top; the caller reprints the visible tail.
    fn resize(&mut self, w: u16, rows: u16) -> Result<()> {
        self.w = w;
        self.rows = rows;
        self.h = self.h.min(rows.saturating_sub(1).max(1));
        self.top = 0;
        self.reset()
    }

    /// Leave the cursor below the transcript for the shell.
    fn finish(&mut self) {
        let _ = self.clear_from(self.top);
        let _ = execute!(std::io::stdout(), crossterm::cursor::MoveTo(0, self.top));
    }
}

impl App {
    /// The state a session starts with. `run` adds what needs a subprocess or
    /// a background watcher (the branch, the pull request status).
    pub(super) fn new(
        env: Arc<crate::agent::Env>,
        agent: Arc<AsyncMutex<Agent>>,
        tx: mpsc::UnboundedSender<AppMsg>,
        completions: Vec<(String, String)>,
        keymap: keybindings::Keymap,
        history: Vec<String>,
        inline: bool,
    ) -> App {
        App {
            blocks: vec![],
            input: Input::with_history(history),
            selection: None,
            selection_moved: false,
            last_click: None,
            click_count: 0,
            click_toggle: None,
            row_text: Vec::new(),
            help: false,
            scroll: 0,
            scroll_anchor: None,
            busy: false,
            busy_label: None,
            busy_since: Instant::now(),
            wait_started: None,
            stream_chars: 0,
            open_text: None,
            stream_state: None,
            open_think: None,
            dialog: None,
            popup: None,
            todos: vec![],
            show_todos: true,
            statuses: vec![],
            expand_all: false,
            mode: env.mode(),
            model: env.model(),
            ctx_used: 0,
            ctx_max: env.context_max(),
            // Nonzero when resuming (`chanto -r`).
            total_cost_usd: env.accounting.total(),
            price_unknown: env.accounting.unknown(),
            tps: 0.0,
            queue: VecDeque::new(),
            cancel: None,
            ctrl_c_at: None,
            esc_at: None,
            quit: false,
            row_map: vec![],
            transcript_area: Rect::default(),
            agent,
            env: env.clone(),
            tx,
            completions,
            files: None,
            ignored_files: None,
            hint: None,
            btw_ask: None,
            limits: env.rate_limits.lock().unwrap().clone(),
            limit_warned: Default::default(),
            spinner_i: 0,
            branch: None,
            pr_status: Default::default(),
            mouse: !inline,
            pastes: vec![],
            images: vec![],
            pending_dialogs: VecDeque::new(),
            search: None,
            highlight: None,
            open_editor: false,
            edit_proposal: false,
            status_text: None,
            vim: false,
            vim_insert: true,
            vim_pending: None,
            chord_x: false,
            deliver: Deliver::Step,
            send_now: false,
            steered: Vec::new(),
            work_sel: None,
            viewing: None,
            work_area: Rect::default(),
            work_start: 0,
            agent_view: None,
            suggestion: None,
            keymap,
            replaying: false,
            vim_undo: vec![],
            term_title: String::new(),
            turned: false,
            term_status: notify::status_enabled(env.config.terminal_status.as_deref(), &|k| {
                std::env::var(k).ok().filter(|v| !v.is_empty())
            })
            .then_some(None),
            turn_failed: false,
            voice: Voice::Off,
            inline: inline.then(InlineView::default),
            screen_rows: 0,
            focused: None,
            notify: notify::channel(env.config.notifications.as_deref()),
            attention: None,
            idle_since: None,
            idle_notified: false,
            idle_compacted: false,
            editing_prompt: None,
            turns_since_recap: 0,
            recap: None,
            quiet: env.quiet(),
            turn_tools: 0,
            current_tool: None,
            loop_turn: false,
        }
    }

    /// Bracketed paste: into an open answer box, else into the prompt. A
    /// long paste collapses to a tag and a file path becomes an `@` mention.
    /// True when image files were attached (the loop skips straight to the
    /// next frame).
    pub(super) fn on_paste(&mut self, s: &str) -> bool {
        let s = normalize_paste(s);
        if let Some(
            Dialog::Permission {
                feedback: Some(fb), ..
            }
            | Dialog::Question {
                typing: Some(fb), ..
            },
        ) = &mut self.dialog
        {
            fb.insert(&s);
            return false;
        }
        let t = crate::util::unescape_path(s.trim().trim_matches('\'').trim_matches('"'));
        // Dragged or pasted image files (one or several) become
        // `[Image #N]` tags, like a clipboard image, instead of
        // a long path. A dragged screenshot may already be gone
        // by submit time, so each is copied somewhere durable.
        if let Some(images) = dropped_images(&s) {
            for path in images {
                // Attach the original if it can't be copied.
                self.images.push(persist_attachment(&path).unwrap_or(path));
                let n = self.images.len();
                self.input.insert(&format!("[Image #{n}] "));
            }
            self.update_popup();
            return true;
        }
        let is_path = !t.contains('\n') && std::path::Path::new(&t).is_file();
        let n_lines = s.lines().count();
        if is_path {
            self.input.insert(&format!("@{} ", t.replace(' ', "\\ ")));
        } else if (n_lines > 12 || s.len() > 1500) && self.env.config.collapse_pastes != Some(false)
        {
            self.pastes.push(s.clone());
            self.input.insert(&format!(
                "[Pasted text #{} +{} lines]",
                self.pastes.len(),
                n_lines
            ));
        } else {
            self.input.insert(&s);
        }
        self.update_popup();
        false
    }

    /// The 100ms tick: background notices, the recorder, attention and idle
    /// timers, and `/loop`. True when the screen needs a redraw.
    pub(super) fn on_tick(&mut self) -> bool {
        let mut dirty = false;
        if let Some(notice) = crate::update_check::take_notice() {
            self.note(notice, pal().dim);
            dirty = true;
        }
        // An in-flight `/btw` ask animates its panel's spinner.
        if self.busy
            || self.hint.is_some()
            || self.btw_ask.is_some()
            || self.blocks.iter().any(|b| b.is_running_tool())
            || !matches!(self.voice, Voice::Off)
        {
            dirty = true;
        }
        // Keep the auto-deny countdown ticking while a permission prompt waits.
        if matches!(
            &self.dialog,
            Some(Dialog::Permission {
                deadline: Some(_),
                ..
            })
        ) {
            dirty = true;
        }
        // Stop on silence after speech, or when the recorder quit on its own.
        let stop = match &mut self.voice {
            Voice::Recording(rec, _) => rec.should_stop(),
            _ => false,
        };
        if stop {
            self.toggle_voice();
        }
        self.check_attention();
        self.idle_tick();
        let before = self.busy;
        self.loop_tick();
        dirty | (self.busy != before)
    }

    /// A prompt answered elsewhere (in the IDE, or on the phone with
    /// /remote) closes its dialog here, and the next queued one opens. True
    /// when the open dialog was closed.
    pub(super) fn drop_answered_dialogs(&mut self) -> bool {
        let answered = |d: &Dialog| match d {
            Dialog::Permission { tx: Some(tx), .. } => tx.is_closed(),
            Dialog::Question { tx: Some(tx), .. } => tx.is_closed(),
            _ => false,
        };
        let mut closed = false;
        if self.dialog.as_ref().is_some_and(answered) {
            self.dialog = None;
            closed = true;
        }
        self.pending_dialogs.retain(|d| !answered(d));
        if self.dialog.is_none() {
            self.dialog = self.pending_dialogs.pop_front();
        }
        closed
    }

    /// What came back from $EDITOR: edited proposed content answers the
    /// permission prompt (unchanged = a plain yes); otherwise it's the prompt.
    pub(super) fn apply_edit(&mut self, proposal: Option<String>, edited: Result<String>) {
        match edited {
            Ok(t) if proposal.is_some() => {
                let reply = if proposal.as_deref() == Some(t.as_str()) {
                    PermissionReply::Allow
                } else {
                    PermissionReply::AllowEdited(t)
                };
                if let Some(Dialog::Permission { tx, .. }) = &mut self.dialog
                    && let Some(tx) = tx.take()
                {
                    let _ = tx.send(reply);
                }
                self.dialog = None;
            }
            Ok(t) => self.input.set(t.trim_end_matches('\n')),
            Err(e) => self.hint = Some((format!("Editor failed: {e}"), Instant::now())),
        }
    }
}

pub async fn run(cli: Cli) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<AppMsg>();
    {
        // A second signal while the first is still being handled exits at once.
        let tx = tx.clone();
        crate::util::on_termination(move |code| {
            if tx.send(AppMsg::Terminate).is_err() {
                std::process::exit(code);
            }
        });
    }
    let fe = crate::remote::mirror_tui(Arc::new(TuiFrontend { tx: tx.clone() }));
    // The TUI fires /loop iterations and scheduled tasks between turns.
    crate::cron::set_frontend_drives();
    let Setup {
        env,
        agent,
        warnings,
    } = setup(&cli, fe, true).await?;
    env.scheduler.set_driven();
    let theme_warning = theme::init(env.config.theme.as_deref());
    let keymap = keybindings::Keymap::load(keybindings::files(env.config.compat.claude));
    let keymap_warnings: Vec<String> = keymap
        .1
        .iter()
        .map(|p| format!("Keybindings: {p}"))
        .collect();

    let inline = cli.inline || env.config.tui.as_deref() == Some("inline");
    INLINE.store(inline, std::sync::atomic::Ordering::Relaxed);
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    if inline {
        execute!(stdout, EnableBracketedPaste)?;
    } else {
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
    }
    // Focus reports decide whether a notification is needed; terminals that
    // don't support them ignore the request.
    let _ = execute!(stdout, EnableFocusChange);
    let kb_enh = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if kb_enh {
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let _guard = TermGuard { kb_enh };
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal(kb_enh);
        prev_hook(info);
    }));
    // Full screen draws on a terminal over the alternate screen; inline draws
    // on a region at the bottom of the normal screen.
    let (mut terminal, mut region) = if inline {
        (None, Some(Region::new()?))
    } else {
        let mut t = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
        t.clear()?;
        (Some(t), None)
    };

    let completions = commands::completions(&agent);
    let resumed = agent.messages.len() > 1;
    let agent = Arc::new(AsyncMutex::new(agent));
    let remote_tx = tx.clone();
    crate::remote::attach_tui(agent.clone(), env.clone(), move |input| {
        remote_tx.send(AppMsg::Remote(input)).is_ok()
    });
    let mut app = App::new(
        env.clone(),
        agent.clone(),
        tx.clone(),
        completions,
        keymap.0,
        load_history(),
        inline,
    );
    app.branch = git_branch(&env.cwd());
    app.pr_status = watch_pr_status(env.cwd());
    app.refresh_status_line();
    app.on_msg(AppMsg::Reset(if resumed {
        Some("Resumed previous session.".into())
    } else {
        None
    }));
    let pending = crate::learn::lessons(&env.root)
        .iter()
        .filter(|l| l.status == "proposed")
        .count();
    if pending > 0 {
        app.note(
            format!("{pending} proposed lesson(s) from your past corrections: /learned to review"),
            pal().dim,
        );
    }
    for w in warnings
        .into_iter()
        .chain(theme_warning)
        .chain(keymap_warnings)
    {
        app.note(w, warn());
    }
    // Warm the model list in the background so the first `/model` opens at once.
    if crate::onboarding::cached_models(&env).is_none_or(|(_, age)| age > Duration::from_secs(3600))
    {
        let env = env.clone();
        tokio::spawn(async move { crate::onboarding::refresh_models(&env, true).await });
    }
    let peers = crate::presence::peers();
    if !peers.is_empty() {
        app.note(format!("{} other chanto session(s) working in this project; each is told about the others. /peers lists them.", peers.len()), pal().dim);
    }
    if let Some(msg) = crate::ide::auto_connect(&env).await {
        app.note(msg, pal().dim);
    }
    {
        let a = agent.lock().await;
        app.ctx_used = a.context_used();
    }
    // Keys typed before the TUI was up (after a startup y/N question, say)
    // land in the prompt rather than being lost; not sent, even after an Enter.
    let typed = typeahead_text(&markdown::take_typeahead());
    if !typed.is_empty() && cli.prompt.is_empty() {
        app.input.insert(&typed);
    }
    if !cli.prompt.is_empty() {
        app.input.set(&cli.prompt.join(" "));
        app.submit();
    } else if cli.resume.as_deref() == Some("") {
        app.open_picker("/resume");
    }

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut dirty = true;
    // Set when the screen may no longer be what we drew: a restored window, a
    // multiplexer redraw, a program writing to the tty. A diffed frame only
    // rewrites changed cells, so without this its glyphs survive in our rows.
    let mut redraw_all = false;
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    // Longest pause after which the terminal is assumed to have been repainted
    // by something else while we were idle.
    let idle_gap = Duration::from_secs(1);
    let mut last_input = Instant::now();
    loop {
        // 60 fps for typing; ~30 while a reply streams in, which reads the same
        // and halves the redraw work over a long answer.
        let frame = Duration::from_millis(if app.open_text.is_some() { 33 } else { 16 });
        if dirty && last_draw.elapsed() >= frame {
            app.sync_term_title();
            // Synchronized output: terminals that support it show each frame
            // whole, so a full-screen scroll can't tear; others ignore it.
            let _ = crossterm::queue!(
                std::io::stdout(),
                crossterm::terminal::BeginSynchronizedUpdate
            );
            if let Some(r) = region.as_mut() {
                if redraw_all {
                    // A fresh terminal makes the draw rewrite every cell; the
                    // reset has already blanked the rows it covers.
                    r.reset()?;
                }
                let settled = app.take_settled(r.w.saturating_sub(1) as usize);
                if !settled.is_empty() {
                    r.print(&settled)?;
                }
                app.screen_rows = r.rows;
                r.set_height(app.inline_height(r.w, r.rows))?;
                r.term.draw(|f| app.draw(f))?;
            } else if let Some(t) = terminal.as_mut() {
                if redraw_all {
                    t.clear()?;
                }
                t.draw(|f| app.draw(f))?;
            }
            redraw_all = false;
            let _ = execute!(
                std::io::stdout(),
                crossterm::terminal::EndSynchronizedUpdate
            );
            dirty = false;
            last_draw = Instant::now();
        }
        tokio::select! {
            ev = events.next() => {
                // Coming back after a pause: the window may have been restored
                // or the terminal repainted its own content, so rewrite every
                // cell on this first frame rather than leaving its glyphs in
                // rows whose diffed content did not change.
                if last_input.elapsed() >= idle_gap {
                    redraw_all = true;
                }
                last_input = Instant::now();
                match ev {
                    Some(Ok(TermEvent::Key(k))) => app.on_key(k),
                    Some(Ok(TermEvent::Mouse(m))) => app.on_mouse(m),
                    Some(Ok(TermEvent::FocusGained)) => {
                        app.focused = Some(true);
                        // The window may have been restored while unfocused,
                        // with the terminal repainting its own content.
                        redraw_all = true;
                    }
                    Some(Ok(TermEvent::FocusLost)) => {
                        app.focused = Some(false);
                        app.check_attention();
                    }
                    Some(Ok(TermEvent::Paste(s))) => {
                        if app.on_paste(&s) {
                            dirty = true;
                            continue;
                        }
                    }
                    Some(Ok(TermEvent::Resize(w, h))) => {
                        if let Some(r) = region.as_mut() {
                            r.resize(w, h)?;
                            let tail = app.reprint_after_resize(w.saturating_sub(1) as usize, r.rows.saturating_sub(r.h) as usize);
                            r.print(&tail)?;
                        }
                    }
                    Some(Err(_)) | None => break,
                }
                dirty = true;
            }
            Some(m) = rx.recv() => {
                app.on_msg(m);
                while let Ok(m) = rx.try_recv() {
                    app.on_msg(m);
                }
                dirty = true;
            }
            // Wake when the throttle window closes so a burst's final event is
            // drawn immediately rather than waiting for the next 100ms tick.
            _ = tokio::time::sleep(frame.saturating_sub(last_draw.elapsed())), if dirty => {}
            _ = tick.tick() => {
                dirty |= app.on_tick();
            }
        }
        dirty |= app.drop_answered_dialogs();
        if app.open_editor || app.edit_proposal {
            let proposal = match &app.dialog {
                Some(Dialog::Permission { req, .. }) if app.edit_proposal => req.proposed.clone(),
                _ => None,
            };
            app.edit_proposal = false;
            if proposal.is_none() && !app.open_editor {
                continue;
            }
            app.open_editor = false;
            // Stop reading terminal input while the editor owns it.
            drop(events);
            restore_terminal(kb_enh);
            let edited = edit_in_editor(proposal.as_deref().unwrap_or(&app.input.text));
            enable_raw_mode()?;
            if inline {
                execute!(std::io::stdout(), EnableBracketedPaste)?;
            } else {
                execute!(
                    std::io::stdout(),
                    EnterAlternateScreen,
                    EnableBracketedPaste
                )?;
            }
            let _ = execute!(std::io::stdout(), EnableFocusChange);
            if app.mouse {
                execute!(std::io::stdout(), EnableMouseCapture)?;
            }
            if kb_enh {
                let _ = execute!(
                    std::io::stdout(),
                    PushKeyboardEnhancementFlags(
                        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    )
                );
            }
            // A fresh Terminal forces a full redraw (Terminal::clear would query the
            // cursor position, which can time out right after the editor exits).
            if let Some(r) = region.as_mut() {
                // The editor ran on its own screen; ours is as we left it.
                r.reset()?;
            } else {
                let _ = execute!(
                    std::io::stdout(),
                    crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
                );
                terminal = Some(Terminal::new(CrosstermBackend::new(std::io::stdout()))?);
            }
            events = EventStream::new();
            app.apply_edit(proposal, edited);
            dirty = true;
        }
        if app.quit {
            break;
        }
    }
    if let Some(c) = &app.cancel {
        c.cancel();
    }
    crate::presence::stop();
    drop(terminal);
    if let Some(r) = region.as_mut() {
        // Whatever is still live (an interrupted answer, a running tool) goes to
        // the scrollback too, rather than vanishing with the region.
        let w = r.w.saturating_sub(1) as usize;
        let mut rest = app.take_settled(w);
        let live = app.live_lines(w);
        if !live.is_empty() {
            rest.extend(live);
            rest.push(Line::raw(""));
        }
        if !rest.is_empty() {
            let _ = r.print(&rest);
        }
        r.finish();
    }
    // Restore once, through the guard: leaving the alternate screen a second
    // time puts the cursor back where the TUI started, so the shell's prompt
    // would overwrite the resume hint below.
    drop(_guard);
    // Quitting mid-turn (a signal, say): the interrupted turn saves itself as
    // it ends; wait a little for that rather than skipping the save.
    let lock = match agent.try_lock() {
        Ok(a) => Some(a),
        Err(_) => {
            if let Some(c) = &app.cancel {
                c.cancel();
            }
            tokio::time::timeout(Duration::from_secs(5), agent.lock())
                .await
                .ok()
        }
    };
    if let Some(mut a) = lock {
        a.save_session();
        // After /clear the new session has nothing in it, and nothing was saved.
        let saved = a.messages.iter().any(|m| m.role == crate::llm::Role::User);
        if let Some(note) = crate::session_cmds::take_exit_note() {
            println!("{note}");
        } else if let Some(s) = a.session.as_ref().filter(|_| saved) {
            println!("Session saved. Resume with: chanto --resume {}", s.id);
        }
    }
    env.shutdown().await;
    if let Some(code) = crate::util::terminated() {
        std::process::exit(code);
    }
    Ok(())
}
