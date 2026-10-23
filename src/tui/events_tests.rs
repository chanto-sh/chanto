//! Agent events and app messages: what each does to the transcript and state.

use super::app_tests::*;
use super::*;

fn assistant_text(t: &TestApp) -> Vec<String> {
    t.blocks
        .iter()
        .filter_map(|b| match &b.kind {
            Kind::Assistant(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn streamed_text_and_reasoning_build_blocks() {
    let mut t = test_app();
    t.on_agent(Event::AssistantStart);
    t.on_agent(Event::Reasoning("  let me".into()));
    t.on_agent(Event::Reasoning(" think".into()));
    let i = t.open_think.unwrap();
    assert!(
        matches!(&t.blocks[i].kind, Kind::Thinking { text, elapsed: None, .. } if text == "let me think")
    );
    // The answer starting closes the reasoning block.
    t.on_agent(Event::Text("\n  Hello".into()));
    t.on_agent(Event::Text(", world".into()));
    assert!(t.open_think.is_none());
    assert!(matches!(
        &t.blocks[i].kind,
        Kind::Thinking {
            elapsed: Some(_),
            ..
        }
    ));
    assert_eq!(assistant_text(&t), ["Hello, world"]);
    assert_eq!(t.busy_label.as_deref(), Some("Responding"));
    assert!(t.stream_chars > 0);
    t.on_agent(Event::AssistantEnd);
    assert!(t.open_text.is_none());
    // Whitespace alone opens nothing.
    t.on_agent(Event::Text("   ".into()));
    assert!(t.open_text.is_none());
    assert_eq!(assistant_text(&t).len(), 1);
}

#[tokio::test]
async fn reasoning_can_be_hidden() {
    let mut t = test_app_with(|c| c.show_thinking = Some(false), false);
    t.on_agent(Event::Reasoning("secret".into()));
    assert!(t.blocks.is_empty() && t.open_think.is_none());
}

#[tokio::test]
async fn tool_calls_and_their_children() {
    let mut t = test_app();
    t.on_agent(Event::ToolStart {
        id: "t1".into(),
        label: "Task(explore)".into(),
        parent: None,
    });
    assert_eq!(t.turn_tools, 1);
    assert_eq!(t.current_tool.as_deref(), Some("Task(explore)"));
    assert!(t.blocks.last().unwrap().is_running_tool());
    t.on_agent(Event::ToolStart {
        id: "c1".into(),
        label: "Read(a.rs)".into(),
        parent: Some("t1".into()),
    });
    t.on_agent(Event::ToolEnd {
        id: "c1".into(),
        display: String::new(),
        content: String::new(),
        is_error: true,
        images: vec![],
        parent: Some("t1".into()),
    });
    t.on_agent(Event::ToolProgress {
        id: "t1".into(),
        text: "3 files".into(),
    });
    // Events for unknown tools are ignored.
    t.on_agent(Event::ToolProgress {
        id: "nope".into(),
        text: "x".into(),
    });
    t.on_agent(Event::ToolEnd {
        id: "t1".into(),
        display: "found it".into(),
        content: "found it in a.rs".into(),
        is_error: false,
        images: vec![],
        parent: None,
    });
    assert!(t.current_tool.is_none());
    let Kind::Tool(tb) = &t.blocks.last().unwrap().kind else {
        panic!()
    };
    assert_eq!(
        tb.children,
        [("c1".into(), "Read(a.rs)".into(), Some(true))]
    );
    assert_eq!(tb.progress.as_deref(), Some("3 files"));
    assert_eq!(tb.display.as_deref(), Some("found it"));
    assert!(tb.elapsed.is_some() && !tb.is_error);
    assert!(t.blocks.last().unwrap().version > 0, "each change bumps it");
}

#[tokio::test]
async fn status_events_update_the_footer_state() {
    let mut t = test_app();
    t.on_agent(Event::Busy(Some("Compacting…".into())));
    assert_eq!(t.busy_label.as_deref(), Some("Compacting"));
    t.on_agent(Event::Busy(None));
    assert_eq!(t.busy_label.as_deref(), Some("Compacting"));
    t.on_agent(Event::Info("info".into()));
    t.on_agent(Event::Warn("warn".into()));
    t.on_agent(Event::Error("err".into()));
    assert_eq!(t.notes(), ["info", "warn", "err"]);
    t.show_todos = false;
    t.on_agent(Event::Todos(vec![Todo {
        content: "a".into(),
        status: "pending".into(),
        active_form: String::new(),
    }]));
    assert!(t.show_todos && t.todos.len() == 1);
    t.on_agent(Event::ModeChanged(Mode::Plan));
    assert_eq!(t.mode, Mode::Plan);
    t.on_agent(Event::Compacted {
        before: 9000,
        after: 1200,
    });
    assert!(t.notes().last().unwrap().contains("9000 → 1200"));
    t.on_agent(Event::Status {
        key: "goal".into(),
        lines: vec!["goal: 1/3".into(), "step two".into()],
    });
    t.on_agent(Event::Status {
        key: "goal".into(),
        lines: vec!["goal: 2/3".into()],
    });
    assert_eq!(t.statuses, [("goal".into(), vec!["goal: 2/3".to_string()])]);
    t.on_agent(Event::Status {
        key: "goal".into(),
        lines: vec![],
    });
    assert!(t.statuses.is_empty());
    t.on_agent(Event::Background {
        id: "bg-1".into(),
        description: "lint".into(),
        status: "running".into(),
    });
    t.on_agent(Event::Background {
        id: "bg-1".into(),
        description: "lint".into(),
        status: "failed".into(),
    });
    let notes = t.notes();
    assert!(
        notes
            .iter()
            .any(|n| n == "⧉ Background agent bg-1 (lint): running")
    );
    assert!(
        notes
            .iter()
            .any(|n| n == "✗ Background agent bg-1 (lint): failed")
    );
}

#[tokio::test]
async fn usage_keeps_the_last_known_speed() {
    let mut t = test_app();
    let usage = |tps: f64| Event::Usage {
        context_used: 1000,
        context_max: 4000,
        total_cost_usd: 0.5,
        price_unknown: true,
        tps,
        total_in: 0,
        total_out: 0,
    };
    t.on_agent(usage(42.0));
    t.on_agent(usage(0.0));
    assert_eq!((t.ctx_used, t.ctx_max), (1000, 4000));
    assert_eq!(t.tps, 42.0);
    assert!(t.price_unknown && t.total_cost_usd == 0.5);
}

#[tokio::test]
async fn dialogs_open_from_messages_and_queue() {
    let mut t = test_app();
    let (tx, _r1) = oneshot::channel();
    t.on_msg(AppMsg::Permission(permission(None), tx));
    assert!(matches!(t.dialog, Some(Dialog::Permission { .. })));
    assert!(
        t.attention
            .as_ref()
            .unwrap()
            .1
            .contains("Needs your permission")
    );
    let (tx, _r2) = oneshot::channel();
    t.on_msg(AppMsg::Question("Pick?".into(), vec!["a".into()], true, tx));
    assert_eq!(t.pending_dialogs.len(), 1);
    let Some(Dialog::Question {
        options, typing, ..
    }) = t.pending_dialogs.front()
    else {
        panic!()
    };
    assert_eq!(options, &["a", "Type something else…"]);
    assert!(typing.is_none());
}

#[tokio::test]
async fn show_reset_and_completions() {
    let mut t = test_app();
    t.on_msg(AppMsg::Show("**done**".into()));
    assert!(matches!(&t.blocks[0].kind, Kind::Markdown(s) if s == "**done**"));
    t.on_msg(AppMsg::Completions(vec![("/x".into(), "x".into())]));
    assert_eq!(t.completions.len(), 1);
    t.pastes.push("p".into());
    t.scroll = 3;
    t.open_think = Some(0);
    t.on_msg(AppMsg::Reset(Some("Cleared.".into())));
    assert!(matches!(&t.blocks[0].kind, Kind::Welcome { model, .. } if model == "mock"));
    assert_eq!(t.notes(), ["Cleared."]);
    assert!(t.pastes.is_empty() && t.scroll == 0 && t.open_think.is_none());
    // Inline mode starts its printed view over.
    let mut t = test_app_with(|_| {}, true);
    t.inline.as_mut().unwrap().printed = 5;
    t.on_msg(AppMsg::Reset(None));
    assert_eq!(t.inline.as_ref().unwrap().printed, 0);
}

#[tokio::test]
async fn turn_done_settles_and_runs_what_was_queued() {
    let mut t = test_app();
    t.busy = true;
    t.loop_turn = true;
    // A question nobody waits for any more goes; the btw panel stays.
    let (tx, rx) = oneshot::channel();
    drop(rx);
    t.dialog = Some(Dialog::Question {
        q: "stale".into(),
        options: vec![],
        sel: 0,
        tx: Some(tx),
        typing: None,
        scroll: 0,
        free_text: false,
        search: None,
        action: None,
    });
    t.pending_dialogs.push_back(Dialog::Btw {
        index: 0,
        scroll: 0,
    });
    t.on_msg(AppMsg::TurnDone(Some("Error: boom".into())));
    assert!(!t.busy && !t.loop_turn && t.turned && t.turn_failed);
    assert!(t.idle_since.is_some());
    assert!(t.dialog.is_none() && t.pending_dialogs.len() == 1);
    assert_eq!(t.notes(), ["Error: boom"]);
    // The next queued message starts.
    t.busy = true;
    t.queue.push_back(("next one".into(), vec![]));
    t.on_msg(AppMsg::TurnDone(None));
    assert!(t.busy && !t.turn_failed);
    assert!(matches!(&t.blocks.last().unwrap().kind, Kind::User(s) if s == "next one"));
}

#[tokio::test]
async fn turn_done_delivers_leftover_inbox_messages_first() {
    let mut t = test_app();
    t.busy = true;
    t.env
        .inbox
        .lock()
        .unwrap()
        .push(Queued::new("one more thing"));
    t.queue.push_back(("queued".into(), vec![]));
    t.on_msg(AppMsg::TurnDone(None));
    assert!(t.busy, "a turn started for the inbox");
    assert_eq!(t.queue.len(), 1, "the queue waits its turn");
    assert!(t.env.inbox.lock().unwrap().is_empty());
}

#[tokio::test]
async fn send_now_runs_the_front_of_the_queue() {
    let mut t = test_app();
    t.busy = true;
    t.send_now = true;
    t.queue.push_back(("urgent".into(), vec![]));
    t.steered.push(("x".into(), vec![]));
    t.on_msg(AppMsg::TurnDone(None));
    assert!(t.busy && t.queue.is_empty() && t.steered.is_empty());
    assert!(matches!(&t.blocks.last().unwrap().kind, Kind::User(s) if s == "urgent"));
}

#[tokio::test]
async fn exit_and_terminate_quit() {
    let mut t = test_app();
    t.on_msg(AppMsg::Exit);
    assert!(t.quit);
    let mut t = test_app();
    let c = CancellationToken::new();
    t.cancel = Some(c.clone());
    t.on_msg(AppMsg::Terminate);
    assert!(t.quit && c.is_cancelled());
}

#[tokio::test]
async fn a_picker_message_opens_on_the_current_choice_and_submits_the_value() {
    let mut t = test_app();
    t.on_msg(AppMsg::Picker {
        title: "Theme".into(),
        labels: vec![],
        values: vec![],
    });
    assert_eq!(t.notes(), ["Theme: nothing to choose from"]);
    t.on_msg(AppMsg::Picker {
        title: "Theme".into(),
        labels: vec!["dark".into(), "light (current)".into()],
        values: vec!["/theme dark".into(), "/theme light".into()],
    });
    let Some(Dialog::Question { sel, search, .. }) = &t.dialog else {
        panic!()
    };
    assert_eq!(*sel, 1);
    assert!(search.is_none());
    t.on_key(key(KeyCode::Char('1')));
    assert!(matches!(t.rx.recv().await, Some(AppMsg::Submit(s)) if s == "/theme dark"));
}

#[tokio::test]
async fn the_model_picker_searches_and_can_pick_for_the_session() {
    let mut t = test_app();
    let open = |t: &mut TestApp| {
        t.on_msg(AppMsg::Picker {
            title: "Switch model · type to search".into(),
            labels: vec!["gpt-5".into(), "qwen3 (current)".into()],
            values: vec!["/model gpt-5".into(), "/model qwen3".into()],
        })
    };
    open(&mut t);
    assert!(matches!(
        &t.dialog,
        Some(Dialog::Question {
            search: Some(_),
            ..
        })
    ));
    t.on_key(ctrl('s'));
    assert!(matches!(t.rx.recv().await, Some(AppMsg::Submit(s)) if s == "/model --session qwen3"));
    open(&mut t);
    type_str(&mut t, "my/own-model");
    t.on_key(key(KeyCode::Enter));
    assert!(matches!(t.rx.recv().await, Some(AppMsg::Submit(s)) if s == "/model my/own-model"));
}

#[tokio::test]
async fn btw_answers_and_failures() {
    let mut t = test_app();
    t.on_msg(AppMsg::OpenBtw);
    assert!(t.dialog.is_none(), "nothing to show");
    t.on_msg(AppMsg::BtwFailed("Side question failed: offline".into()));
    assert_eq!(t.notes(), ["Side question failed: offline"]);
    // An ask in flight with its panel open shows the failure there.
    t.btw_ask = Some(BtwAsk {
        question: "q".into(),
        error: None,
        started: Instant::now(),
    });
    t.show_btw(0);
    t.on_msg(AppMsg::BtwFailed("timeout".into()));
    assert_eq!(
        t.btw_ask.as_ref().unwrap().error.as_deref(),
        Some("timeout")
    );
    // Closed meanwhile: into the transcript instead.
    t.dialog = None;
    t.on_msg(AppMsg::BtwFailed("again".into()));
    assert!(t.btw_ask.is_none());
    assert_eq!(t.notes().last().unwrap(), "Side question failed: again");
    // An answer lands: the panel shows it.
    t.env.btw.lock().unwrap().push(("q".into(), "a".into()));
    t.on_msg(AppMsg::OpenBtw);
    assert!(matches!(t.dialog, Some(Dialog::Btw { index: 0, .. })));
}

#[tokio::test]
async fn mcp_note_without_a_panel_goes_to_the_transcript() {
    let mut t = test_app();
    t.on_msg(AppMsg::McpNote("Reconnected docs.".into()));
    assert_eq!(t.notes(), ["Reconnected docs."]);
}

#[tokio::test]
async fn status_line_and_voice_messages() {
    let mut t = test_app();
    t.on_msg(AppMsg::StatusLine("main ✓".into()));
    assert_eq!(t.status_text.as_deref(), Some("main ✓"));
    t.on_msg(AppMsg::StatusLine("  ".into()));
    assert!(t.status_text.is_none());
    t.on_msg(AppMsg::Voice(Ok("  ".into())));
    assert_eq!(t.hint_text(), "No speech recognized");
    t.input.set("say");
    t.on_msg(AppMsg::Voice(Ok(" hello ".into())));
    assert_eq!(t.input.text, "say hello");
    t.on_msg(AppMsg::Voice(Err("no mic".into())));
    t.on_msg(AppMsg::VoiceStarted(Err("denied".into())));
    assert_eq!(t.notes(), ["Voice input: no mic", "Voice input: denied"]);
    assert!(matches!(t.voice, Voice::Off));
}

#[tokio::test]
async fn remote_input_is_handled_like_typing() {
    let mut t = test_app();
    t.busy = true;
    t.on_msg(AppMsg::Remote(crate::remote::Input::Prompt(
        "from the phone".into(),
    )));
    assert_eq!(t.env.inbox.lock().unwrap().len(), 1);
    assert!(matches!(&t.blocks.last().unwrap().kind, Kind::User(s) if s == "from the phone"));
    t.on_msg(AppMsg::Remote(crate::remote::Input::Prompt(
        "/clear".into(),
    )));
    assert_eq!(t.queue[0].0, "/clear");
    let c = CancellationToken::new();
    t.cancel = Some(c.clone());
    t.on_msg(AppMsg::Remote(crate::remote::Input::Interrupt));
    assert!(c.is_cancelled() && t.queue.is_empty());
    // Idle: it starts a turn.
    let mut t = test_app();
    t.on_msg(AppMsg::Remote(crate::remote::Input::Prompt("go".into())));
    assert!(t.busy);
}

#[tokio::test]
async fn submit_messages_mid_turn() {
    let mut t = test_app();
    t.busy = true;
    t.on_msg(AppMsg::Submit("/mcp".into()));
    assert!(matches!(t.dialog, Some(Dialog::Mcp { .. })));
    t.on_msg(AppMsg::Submit("/think on".into()));
    assert!(t.queue.is_empty(), "settings run at once");
    t.on_msg(AppMsg::Submit("/compact".into()));
    assert_eq!(t.queue[0].0, "/compact");
}

#[tokio::test]
async fn the_transcript_toggle_explains_the_quiet_view() {
    let mut t = test_app();
    t.quiet = true;
    t.toggle_transcript();
    assert!(t.expand_all && t.hint_text().starts_with("Showing every step"));
    assert!(t.quiet_mask().is_none(), "expanded: everything shows");
    t.toggle_transcript();
    assert!(t.hint_text().starts_with("Quiet view"));
    assert!(t.quiet_mask().is_some());
}
