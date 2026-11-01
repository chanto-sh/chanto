//! Drawing whole frames on a test terminal: the transcript, status line,
//! prompt, dialogs, overlays and footer.

use super::app_tests::*;
use super::*;

fn welcome(t: &mut TestApp) {
    t.on_msg(AppMsg::Reset(None));
}

#[tokio::test]
async fn an_idle_screen_shows_the_welcome_prompt_and_footer() {
    let mut t = test_app();
    welcome(&mut t);
    let rows = t.render(100, 30);
    let screen = rows.join("\n");
    assert!(screen.contains("chanto v"), "{screen}");
    assert!(screen.contains("Try \"fix the failing test\""), "{screen}");
    let footer = rows.last().unwrap();
    assert!(footer.contains("? keys"), "{footer}");
    assert!(
        footer.contains("mock") && footer.contains("0% ctx"),
        "{footer}"
    );
    // What was drawn is recorded for copying.
    assert!(
        t.row_text
            .iter()
            .flatten()
            .any(|(_, s)| s.contains("fix the failing test"))
    );
    // After a first exchange the placeholder invites a follow-up.
    t.push(Kind::User("hi".into()));
    assert!(t.screen(100, 30).contains("Ask a follow-up…"));
    t.busy = true;
    assert!(t.screen(100, 30).contains("Type a message…"));
}

#[tokio::test]
async fn the_busy_line_names_what_runs_and_what_waits() {
    let mut t = test_app();
    t.busy = true;
    t.busy_label = Some("Compacting".into());
    t.stream_chars = 8000;
    t.queue.push_back(("then this".into(), vec![]));
    t.queue.push_back(("and this".into(), vec![]));
    let s = t.screen(100, 30);
    assert!(s.contains("Compacting…"), "{s}");
    assert!(
        s.contains("↓ 2.0k tokens") && s.contains("esc to interrupt"),
        "{s}"
    );
    assert!(
        s.contains("⧗ queued: then this · ⏎ on an empty line runs it now"),
        "{s}"
    );
    assert!(s.contains("⧗ queued: and this"), "{s}");
    // While reasoning streams, the verb says so.
    t.on_agent(Event::Reasoning("hmm".into()));
    assert!(t.screen(100, 30).contains("Thinking…"));
    // A waiting prompt pauses the clock and says so.
    let (tx, _rx) = oneshot::channel();
    t.on_msg(AppMsg::Permission(permission(None), tx));
    assert!(t.screen(100, 30).contains("Waiting for you…"));
    assert!(t.wait_started.is_some());
    t.dialog = None;
    t.render(100, 30);
    assert!(t.wait_started.is_none(), "the wait ended");
}

#[tokio::test]
async fn the_quiet_view_names_the_running_tool() {
    let mut t = test_app();
    t.quiet = true;
    t.busy = true;
    t.on_agent(Event::ToolStart {
        id: "t1".into(),
        label: "Bash(cargo test)".into(),
        parent: None,
    });
    let s = t.screen(100, 30);
    assert!(
        s.contains("Bash(cargo test)…") && s.contains("1 tool call"),
        "{s}"
    );
    assert!(!s.contains("⚙"), "no tool block in the quiet view");
}

#[tokio::test]
async fn todos_and_status_panels() {
    let mut t = test_app();
    let todo = |c: &str, s: &str| Todo {
        content: c.into(),
        status: s.into(),
        active_form: format!("{c}ing"),
    };
    t.todos = vec![
        todo("plan", "completed"),
        todo("build", "in_progress"),
        todo("ship", "pending"),
    ];
    t.statuses = vec![(
        "team".into(),
        vec!["team: 2 working".into(), "alice: tests".into()],
    )];
    let s = t.screen(100, 30);
    assert!(s.contains("Todos (ctrl+t to hide)"), "{s}");
    assert!(
        s.contains("☒ plan") && s.contains("◐ building") && s.contains("☐ ship"),
        "{s}"
    );
    assert!(s.contains(" alice: tests"), "{s}");
    assert!(s.lines().last().unwrap().contains("team: 2 working"));
    // All done, or hidden: no panel.
    t.show_todos = false;
    assert!(!t.screen(100, 30).contains("Todos"));
    t.show_todos = true;
    for td in &mut t.todos {
        td.status = "completed".into();
    }
    assert!(!t.screen(100, 30).contains("Todos"));
}

#[tokio::test]
async fn the_todo_list_keeps_a_blank_row_above_the_status_line() {
    let mut t = test_app();
    t.busy = true;
    t.todos = vec![Todo {
        content: "ship".into(),
        status: "in_progress".into(),
        active_form: "shipping".into(),
    }];
    let rows = t.render(100, 30);
    let last = rows
        .iter()
        .position(|r| r.contains("shipping"))
        .expect("the todo row");
    assert!(
        rows[last + 1].trim().is_empty(),
        "a running turn's status is not part of the list: {:?}",
        &rows[last..last + 3]
    );
    assert!(
        !rows[last + 2].trim().is_empty(),
        "the status line follows the gap: {:?}",
        &rows[last..last + 3]
    );
}

#[tokio::test]
async fn a_long_todo_is_cut_to_the_panel_with_an_ellipsis() {
    let mut t = test_app();
    t.todos = vec![Todo {
        content: "fix the Windows signature check in the installer so a BOM cannot break it".into(),
        status: "pending".into(),
        active_form: String::new(),
    }];
    let rows = t.render(40, 30);
    let row = rows
        .iter()
        .find(|r| r.contains("fix the"))
        .expect("the todo row");
    assert!(row.ends_with('…'), "cut at a word, not clipped: {row:?}");
    assert!(row.chars().count() <= 40, "fits the panel: {row:?}");
}

#[tokio::test]
async fn the_todo_list_says_what_it_hides() {
    let mut t = test_app();
    t.todos = (1..=10)
        .map(|n| Todo {
            content: format!("task {n}"),
            status: "pending".into(),
            active_form: String::new(),
        })
        .collect();
    let rows = t.render(100, 30);
    let s = rows.join("\n");
    assert!(s.contains("task 8"), "{s}");
    assert!(!s.contains("task 9"), "only eight fit: {s}");
    let more = rows
        .iter()
        .position(|r| r.contains("2 more"))
        .unwrap_or_else(|| panic!("the count is drawn: {s}"));
    assert!(
        rows[more + 1].trim().is_empty(),
        "and the panel still leaves room under it: {:?}",
        &rows[more..more + 2]
    );
}

#[tokio::test]
async fn a_permission_dialog_replaces_the_prompt() {
    let mut t = test_app();
    let (tx, _rx) = oneshot::channel();
    let mut req = permission(None);
    req.reason = Some("a hook asked".into());
    t.on_msg(AppMsg::Permission(req, tx));
    let s = t.screen(100, 30);
    assert!(s.contains("rm -rf build"), "{s}");
    assert!(s.contains("a hook asked"), "{s}");
    assert!(s.contains("1. Yes"), "{s}");
    assert!(s.contains("2. Yes, and don't ask again for rm"), "{s}");
    assert!(s.contains("3. No · say what to do instead (esc)"), "{s}");
    assert!(!s.contains("Try \""), "the prompt is hidden");
    // Saying no opens the feedback line.
    t.on_key(key(KeyCode::Char('n')));
    type_str(&mut t, "use trash");
    assert!(t.screen(100, 30).contains("use trash"));
    // A network prompt says "Allow once".
    let mut t = test_app();
    let (tx, _rx) = oneshot::channel();
    let mut req = permission(None);
    req.tool = "SandboxNetwork".into();
    t.on_msg(AppMsg::Permission(req, tx));
    assert!(t.screen(100, 30).contains("1. Allow once"));
}

#[tokio::test]
async fn question_dialogs_show_their_keys() {
    let mut t = test_app();
    let (tx, _rx) = oneshot::channel();
    t.on_msg(AppMsg::Question(
        "Which database?".into(),
        vec!["Postgres".into(), "SQLite".into()],
        true,
        tx,
    ));
    let s = t.screen(100, 30);
    assert!(
        s.contains(" Question ") && s.contains("Which database?"),
        "{s}"
    );
    assert!(
        s.contains("1. Postgres") && s.contains("3. Type something else…"),
        "{s}"
    );
    assert!(s.contains("or type an answer"), "{s}");
    type_str(&mut t, "mysql");
    let s = t.screen(100, 30);
    assert!(
        s.contains("type an answer · enter to send") && s.contains("mysql"),
        "{s}"
    );
    // A plain choice and the quit confirmation title themselves.
    let mut t = test_app();
    let (tx, _rx) = oneshot::channel();
    t.on_msg(AppMsg::Question("Pick".into(), vec!["a".into()], false, tx));
    let s = t.screen(100, 30);
    assert!(s.contains(" Select ") && s.contains("1-9 to choose"), "{s}");
    let mut t = test_app();
    t.open_dialog(Dialog::Question {
        q: quit_warning(1),
        options: vec!["Quit anyway".into()],
        sel: 0,
        tx: None,
        typing: None,
        scroll: 0,
        free_text: false,
        search: None,
        action: Some(QuestionAction::Quit),
    });
    assert!(t.screen(100, 30).contains(" Quit "));
}

#[tokio::test]
async fn a_long_dialog_body_scrolls_with_a_hint() {
    let mut t = test_app();
    let (tx, _rx) = oneshot::channel();
    let mut req = permission(None);
    req.detail = (0..80)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    t.on_msg(AppMsg::Permission(req, tx));
    let s = t.screen(80, 24);
    assert!(s.contains("more lines (pgdn/pgup to scroll)"), "{s}");
    let hs = t.heights(80, 24);
    assert!(hs.bottom <= 24 * 9 / 10 && hs.work == 0);
}

#[tokio::test]
async fn btw_and_mcp_panels_draw() {
    let mut t = test_app();
    t.env
        .btw
        .lock()
        .unwrap()
        .push(("what changed?".into(), "Three files.".into()));
    t.show_btw(0);
    let s = t.screen(100, 30);
    assert!(
        s.contains(" /btw ") && s.contains("what changed?") && s.contains("Three files."),
        "{s}"
    );
    let mut t = test_app();
    t.open_mcp();
    let s = t.screen(100, 30);
    assert!(
        s.contains(" /mcp ") && s.contains("No MCP servers configured"),
        "{s}"
    );
}

#[tokio::test]
async fn the_help_overlay_lists_shortcuts() {
    let mut t = test_app();
    t.help = true;
    let s = t.screen(100, 60);
    assert!(
        s.contains(" Shortcuts ") && s.contains("press any key to close"),
        "{s}"
    );
    // Too short for all of them: says how many are hidden.
    let s = t.screen(100, 12);
    assert!(s.contains("more · resize to see all"), "{s}");
}

#[tokio::test]
async fn the_popup_draws_over_the_transcript() {
    let mut t = test_app();
    t.completions = (0..14)
        .map(|i| (format!("/cmd{i:02}"), format!("does thing {i}")))
        .collect();
    type_str(&mut t, "/cmd");
    for _ in 0..11 {
        t.on_key(key(KeyCode::Down));
    }
    let s = t.screen(100, 30);
    assert!(s.contains("12/14"), "{s}");
    assert!(s.contains("/cmd11") && s.contains("does thing 11"), "{s}");
    assert!(!s.contains("/cmd00"), "scrolled past the first");
}

#[tokio::test]
async fn the_prompt_shows_typed_text_and_the_shell_marker() {
    let mut t = test_app();
    t.input.set("/model fast");
    let s = t.screen(80, 20);
    assert!(s.contains("> /model fast"), "{s}");
    t.input.set("!ls -la");
    assert!(t.screen(80, 20).contains("! !ls -la"));
    // Many lines: the box grows to ten and follows the cursor.
    t.input.set(
        &(0..15)
            .map(|i| format!("row {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let s = t.screen(80, 30);
    assert!(s.contains("row 14") && !s.contains("row 0\n"), "{s}");
    // A suggestion shows greyed with how to take it.
    t.input.set("");
    t.suggestion = Some("run the tests".into());
    assert!(t.screen(80, 20).contains("run the tests  tab to use"));
}

#[tokio::test]
async fn the_frame_setting_changes_the_box() {
    for (spec, glyph) in [("line", "╭"), ("solid", "╭")] {
        let mut t = test_app_with(|c| c.input_border = Some(spec.into()), false);
        assert_eq!(t.frame_style(), frame_from(Some(spec)));
        assert!(t.screen(60, 15).contains(glyph), "{spec}");
    }
}

#[tokio::test]
async fn a_click_on_the_pill_jumps_to_the_latest() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut t = test_app();
    for i in 0..40 {
        t.push(Kind::Markdown(format!("paragraph {i}")));
    }
    t.scroll = 5;
    t.render(80, 20);
    let j = t.jump_area;
    assert!(j.width > 0);
    let click = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    // A click beside the pill doesn't jump.
    t.on_mouse(click(
        MouseEventKind::Down(MouseButton::Left),
        j.x.saturating_sub(2),
        j.y,
    ));
    t.on_mouse(click(
        MouseEventKind::Up(MouseButton::Left),
        j.x.saturating_sub(2),
        j.y,
    ));
    assert_eq!(t.scroll, 5);
    t.on_mouse(click(MouseEventKind::Down(MouseButton::Left), j.x + 1, j.y));
    t.on_mouse(click(MouseEventKind::Up(MouseButton::Left), j.x + 1, j.y));
    assert_eq!(t.scroll, 0);
    assert!(t.selection.is_none());
    assert!(t.screen(80, 20).contains("paragraph 39"));
    // At the bottom there is no pill to click.
    t.render(80, 20);
    assert_eq!(t.jump_area.width, 0);
}

#[tokio::test]
async fn scrolled_up_shows_how_far() {
    let mut t = test_app();
    for i in 0..40 {
        t.push(Kind::Markdown(format!("paragraph {i}")));
    }
    t.scroll = 5;
    let s = t.screen(80, 20);
    assert!(s.contains("↓ jump to latest · click or esc"), "{s}");
    assert!(!s.contains("paragraph 39"));
    assert!(t.scroll_anchor.is_some());
    // New output below keeps the same rows on screen, and the pill counts it.
    let before = t.render(80, 20)[2].clone();
    t.push(Kind::Markdown("new at the bottom".into()));
    assert_eq!(t.render(80, 20)[2], before);
    assert_eq!(t.scroll, 7);
    assert!(t.screen(80, 20).contains("↓ 2 new lines · click or esc"));
    // Too narrow for the full label: the short one keeps esc.
    assert!(t.screen(20, 20).contains("↓ latest · esc"));
}

#[tokio::test]
async fn a_search_hit_is_highlighted_and_the_footer_shows_the_query() {
    let mut t = test_app();
    for i in 0..10 {
        t.push(Kind::Markdown(format!("entry {i}")));
    }
    t.render(80, 20);
    t.on_key(ctrl('f'));
    type_str(&mut t, "entry 2");
    let rows = t.render(80, 20);
    assert!(
        rows.last()
            .unwrap()
            .contains("(search transcript) entry 2▏")
    );
    assert!(t.highlight.is_some());
    type_str(&mut t, "x");
    assert!(t.render(80, 20).last().unwrap().contains("no match · "));
    t.on_key(key(KeyCode::Esc));
    t.on_key(ctrl('r'));
    assert!(
        t.render(80, 20)
            .last()
            .unwrap()
            .contains("(reverse-i-search)")
    );
}

#[tokio::test]
async fn the_footer_shows_mode_hints_and_session_facts() {
    let mut t = test_app();
    for (mode, label) in [
        (Mode::AcceptEdits, "✎ accept edits"),
        (Mode::Plan, "▹ plan mode"),
        (Mode::Auto, "▸ auto mode"),
        (Mode::Bypass, "⊘ bypass permissions"),
    ] {
        t.mode = mode;
        let footer = t.render(120, 20).pop().unwrap();
        assert!(footer.contains(label), "{footer}");
    }
    t.mode = Mode::Default;
    t.status_text = Some("custom status".into());
    t.branch = Some("feature/x".into());
    *t.pr_status.lock().unwrap() = Some("PR #3 checks failing".into());
    t.total_cost_usd = 1.25;
    t.tps = 33.0;
    t.ctx_used = 30000;
    t.ctx_max = 32768;
    t.statuses = vec![("goal".into(), vec!["goal 1/2".into()])];
    let footer = t.render(200, 20).pop().unwrap();
    for part in [
        "custom status",
        "PR #3 checks failing",
        "$1.25",
        "33 tok/s",
        "92% ctx",
        "goal 1/2",
    ] {
        assert!(footer.contains(part), "{part}: {footer}");
    }
    assert!(!footer.contains("feature/x"), "{footer}");
    *t.pr_status.lock().unwrap() = None;
    assert!(t.render(200, 20).pop().unwrap().contains("⎇ feature/x"));
    t.env.set_model("deepseek/deepseek-flash");
    let footer = t.render(200, 20).pop().unwrap();
    assert!(footer.contains("deepseek-flash"), "{footer}");
    assert!(!footer.contains("deepseek/deepseek-flash"), "{footer}");
    t.env.set_model("mock");
    // A fresh hint wins over the status text.
    t.hint = Some(("Copied".into(), Instant::now()));
    assert!(t.render(200, 20).pop().unwrap().contains("Copied"));
    // Narrow: the least important parts drop, the model stays.
    t.hint = None;
    let footer = t.render(40, 20).pop().unwrap();
    assert!(
        footer.contains("mock") && !footer.contains("tok/s"),
        "{footer}"
    );
}

#[tokio::test]
async fn the_footer_shows_mcp_and_voice_state() {
    let mut t = test_app_with(
        |c| {
            c.mcp_servers.insert(
                "docs".into(),
                crate::config::McpServerConfig {
                    command: "docs".into(),
                    ..Default::default()
                },
            );
        },
        false,
    );
    assert!(t.render(120, 20).pop().unwrap().contains("mcp 0/1"));
    t.voice = Voice::Transcribing(Instant::now());
    assert!(
        t.render(120, 20)
            .pop()
            .unwrap()
            .contains("◌ transcribing… 0:00")
    );
}

#[tokio::test]
async fn the_agents_panel_lists_subagents_and_views_one() {
    let mut t = test_app();
    t.env.live_agents.start(
        "k1",
        "agent-1",
        "explore",
        "look around",
        "find the parser",
        true,
    );
    t.env.live_agents.text("k1", "Found it in src/parse.rs.");
    let s = t.screen(100, 30);
    assert!(s.contains("● main") && s.contains("explore"), "{s}");
    assert!(t.work_area.height > 0);
    t.view_agent(Some("k1".into()));
    let s = t.screen(100, 30);
    assert!(s.contains("Found it in src/parse.rs."), "{s}");
    assert!(s.contains("Message explore (agent-1)…"), "{s}");
    assert!(t.agent_view.is_some());
    // Cached: drawing again reuses it.
    t.render(100, 30);
    // Gone from the registry: back to main.
    t.view_agent(Some("unknown".into()));
    t.agent_view = None;
    t.render(100, 30);
    assert!(t.viewing.is_none());
}

#[tokio::test]
async fn background_agents_without_a_live_transcript_show_their_latest_message() {
    let mut t = test_app();
    t.open_work_item(&agents_panel::Item::Background {
        id: "bg-9".into(),
        label: "lint".into(),
        secs: 3,
    });
    assert!(matches!(&t.blocks.last().unwrap().kind,
        Kind::Markdown(s) if s == "**bg-9** is running and hasn't said anything yet."));
    t.open_work_item(&agents_panel::Item::Shell {
        id: "bash_1".into(),
        label: "sleep".into(),
        monitor: false,
    });
    assert_eq!(t.blocks.len(), 1, "a shell has nothing to open");
}

#[tokio::test]
async fn inline_mode_prints_settled_blocks_and_draws_the_live_tail() {
    let mut t = test_app_with(|_| {}, true);
    t.screen_rows = 40;
    t.push(Kind::User("first question".into()));
    t.push(Kind::Markdown("an answer".into()));
    t.on_agent(Event::Text("streaming now".into()));
    let settled = t.take_settled(79);
    let text: Vec<String> = settled
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert!(
        text.iter().any(|l| l.contains("first question")),
        "{text:?}"
    );
    assert!(t.take_settled(79).is_empty(), "printed once");
    let h = t.inline_height(80, 40);
    assert!(h >= t.heights(80, 40).total());
    let s = t.screen(80, h);
    assert!(s.contains("streaming now"), "{s}");
    // After a resize everything printed comes again, capped.
    let tail = t.reprint_after_resize(60, 2);
    assert!(tail.len() <= 2);
    t.help = true;
    assert!(t.inline_height(80, 40) >= crate::commands::SHORTCUTS.len() as u16 + 3);
    // Full screen: none of this applies.
    let mut full = test_app();
    assert!(full.take_settled(79).is_empty());
    assert!(full.reprint_after_resize(79, 5).is_empty());
    assert!(full.live_lines(79).is_empty());
}

#[tokio::test]
async fn heights_make_room_for_each_panel() {
    let mut t = test_app();
    let hs = t.heights(80, 30);
    assert_eq!(
        (hs.panel, hs.todo, hs.status, hs.bottom, hs.work),
        (0, 0, 0, 3, 0)
    );
    assert_eq!(hs.total(), 5);
    t.busy = true;
    t.queue.push_back(("q".into(), vec![]));
    t.input.set("a\nb\nc");
    let hs = t.heights(80, 30);
    assert_eq!((hs.status, hs.bottom), (2, 5));
}

#[tokio::test]
async fn hovering_highlights_what_a_click_acts_on_and_redraws_only_on_change() {
    let mut t = test_app();
    t.push(Kind::Markdown("plain text".into()));
    t.push(Kind::Thinking {
        text: "reasoning\nmore reasoning".into(),
        started: Instant::now(),
        elapsed: Some(Duration::from_secs(1)),
    });
    let mut term = Terminal::new(ratatui::backend::TestBackend::new(60, 20)).unwrap();
    term.draw(|f| t.app.draw(f)).unwrap();
    let a = t.transcript_area;
    let find = |t: &TestApp, want: fn(&Kind) -> bool| {
        t.row_map
            .iter()
            .position(|bi| t.blocks.get(*bi).is_some_and(|b| want(&b.kind)))
            .map(|r| a.y + r as u16)
    };
    let block_row =
        find(&t, |k| matches!(k, Kind::Thinking { .. })).expect("the block is on screen");
    let text_row = find(&t, |k| matches!(k, Kind::Markdown(_))).expect("the text is on screen");

    assert!(t.on_mouse_moved(block_row, 3), "onto the block");
    assert!(
        !t.on_mouse_moved(block_row, 9),
        "within it: nothing to draw"
    );
    term.draw(|f| t.app.draw(f)).unwrap();
    // The block's grey text lightens, and only while the mouse is on it.
    let greys = |term: &Terminal<ratatui::backend::TestBackend>| {
        let buf = term.backend().buffer();
        (a.x..a.x + a.width)
            .filter(|x| buf[(*x, block_row)].fg == pal().dim)
            .count()
    };
    assert_eq!(greys(&term), 0, "lightened on hover");

    assert!(t.on_mouse_moved(text_row, 3), "off it: drawn once more");
    assert!(t.hover.is_none(), "text isn't clickable");
    assert!(!t.on_mouse_moved(text_row, 9));
    term.draw(|f| t.app.draw(f)).unwrap();
    assert!(greys(&term) > 0, "back to grey");

    // Nothing lights up under a selection or the help overlay.
    t.selection = Some(Selection {
        anchor: (block_row, 1),
        head: (block_row, 2),
    });
    assert!(!t.on_mouse_moved(block_row, 3));
    t.selection = None;
    t.help = true;
    assert!(!t.on_mouse_moved(block_row, 3));
}

#[tokio::test]
async fn only_a_block_with_more_to_show_lights_up() {
    let tool = |display: &str, content: &str| {
        Kind::Tool(ToolBlock {
            id: "t".into(),
            label: "Write(b.txt)".into(),
            display: Some(display.into()),
            content: Some(content.into()),
            is_error: false,
            children: vec![],
            started: Instant::now(),
            elapsed: Some(Duration::from_secs(1)),
            progress: None,
        })
    };
    let mut t = test_app();
    t.push(tool("Wrote 1 line to b.txt", "Wrote 1 line to b.txt"));
    let long = (0..40)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    t.push(tool(&long, &long));
    let mut term = Terminal::new(ratatui::backend::TestBackend::new(60, 30)).unwrap();
    term.draw(|f| t.app.draw(f)).unwrap();
    let a = t.transcript_area;
    let row = |t: &TestApp, want: usize| {
        let at = t.blocks.len() - want;
        a.y + t.row_map.iter().position(|bi| *bi == at).unwrap() as u16
    };
    let (short, long_row) = (row(&t, 2), row(&t, 1));
    t.on_mouse_moved(short, 3);
    assert_eq!(t.hover, None, "expanding it would show nothing more");
    t.on_mouse_moved(long_row, 3);
    assert!(matches!(t.hover, Some(mouse::Hover::Block(_))));
    // Open, it can still be clicked closed.
    let last = t.blocks.len() - 1;
    t.blocks[last].expanded = true;
    t.blocks[last].touch();
    term.draw(|f| t.app.draw(f)).unwrap();
    let open_row = row(&t, 1);
    t.on_mouse_moved(open_row, 3);
    assert!(matches!(t.hover, Some(mouse::Hover::Block(_))));
}
