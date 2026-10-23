//! What the line frontend does with each event: which output modes print it,
//! and how the open text block, reasoning block and quiet-mode tool count follow
//! the stream.

use super::*;

fn text(t: &str) -> Event {
    Event::Text(t.into())
}

fn tool_start(label: &str, parent: Option<&str>) -> Event {
    Event::ToolStart {
        id: "t".into(),
        label: label.into(),
        parent: parent.map(String::from),
    }
}

fn tool_end(display: &str, parent: Option<&str>) -> Event {
    Event::ToolEnd {
        id: "t".into(),
        display: display.into(),
        content: display.into(),
        is_error: false,
        images: vec![],
        parent: parent.map(String::from),
    }
}

fn open(fe: &PlainFrontend) -> (bool, bool, usize) {
    let st = fe.state.lock().unwrap();
    (st.text_open, st.in_reasoning, st.tools)
}

#[test]
fn live_output_tracks_reasoning_and_text_blocks() {
    let fe = PlainFrontend::new(OutputFormat::Live, true);
    fe.emit(Event::AssistantStart);
    fe.emit(Event::Reasoning("thinking it over".into()));
    assert_eq!(open(&fe), (false, true, 0), "a reasoning block opens");
    fe.emit(text("The **answer**"));
    assert_eq!(open(&fe), (true, false, 0), "text closes the reasoning");
    fe.emit(text(" continues"));
    fe.emit(Event::AssistantEnd);
    assert_eq!(
        open(&fe),
        (false, false, 0),
        "the turn's end closes the text"
    );
    // A tool call closes an open text block too.
    fe.emit(text("Let me look"));
    fe.emit(tool_start("Read(a.txt)", None));
    assert_eq!(open(&fe), (false, false, 0));
    let long: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    fe.emit(tool_end(&long, Some("parent")));
    fe.emit(tool_end("short", None));
    // An open reasoning block is closed when a notice interrupts it.
    fe.emit(Event::Reasoning("more".into()));
    fe.emit(Event::Info("✓ Verified".into()));
    assert_eq!(open(&fe), (false, false, 0));
}

#[test]
fn hidden_thinking_never_opens_a_block() {
    let fe = PlainFrontend::new(OutputFormat::Live, false);
    fe.emit(Event::Reasoning("private".into()));
    assert_eq!(open(&fe), (false, false, 0));
}

#[test]
fn notices_and_status_events_print_without_opening_blocks() {
    let fe = PlainFrontend::new(OutputFormat::Live, false);
    for ev in [
        Event::Busy(Some("Working".into())),
        Event::Busy(None),
        Event::Background {
            id: "a1".into(),
            description: "survey".into(),
            status: "running".into(),
        },
        Event::Warn("careful".into()),
        Event::Error("boom".into()),
        Event::Todos(vec![]),
        Event::ToolProgress {
            id: "t".into(),
            text: "…".into(),
        },
        Event::Usage {
            context_used: 1,
            context_max: 2,
            total_in: 3,
            total_out: 4,
            total_cost_usd: 0.0,
            price_unknown: false,
            tps: 0.0,
        },
        Event::ModeChanged(crate::permissions::Mode::Plan),
        Event::Compacted {
            before: 1000,
            after: 100,
        },
        Event::Status {
            key: "goal".into(),
            lines: vec!["iteration 2".into()],
        },
        Event::Status {
            key: "spend".into(),
            lines: vec!["$1".into()],
        },
        Event::Status {
            key: "goal".into(),
            lines: vec![],
        },
    ] {
        fe.emit(ev);
        assert_eq!(open(&fe), (false, false, 0));
    }
    assert!(fe.state.lock().unwrap().spinner.is_none());
}

#[test]
fn quiet_mode_counts_top_level_tools_and_hides_the_steps() {
    let fe = PlainFrontend::new(OutputFormat::Live, true);
    fe.set_quiet(true);
    fe.emit(Event::AssistantStart);
    fe.emit(text("Let me look"));
    fe.emit(Event::Reasoning("hmm".into()));
    assert_eq!(open(&fe), (false, false, 0), "steps stay out of the output");
    fe.emit(tool_start("Bash(ls)", None));
    fe.emit(tool_end("a.txt", None));
    fe.emit(tool_start("Read(a.txt)", None));
    // A subagent's calls aren't the turn's own.
    fe.emit(tool_start("Grep(x)", Some("sub")));
    assert_eq!(open(&fe).2, 2);
    fe.print_final("  Done: **2** files  ");
    assert!(fe.state.lock().unwrap().spinner.is_none());
    fe.print_final("   ");
    // The next turn starts counting again.
    fe.set_quiet(false);
    assert_eq!(open(&fe).2, 0);
    fe.emit(text("visible"));
    assert_eq!(open(&fe), (true, false, 0));
    fe.print_final("final");
    assert_eq!(open(&fe), (false, false, 0), "the summary closes the text");
}

#[test]
fn print_and_stream_formats_leave_the_live_state_alone() {
    for format in [
        OutputFormat::Text,
        OutputFormat::Json,
        OutputFormat::StreamJson,
    ] {
        let fe = PlainFrontend::new(format, true);
        for ev in [
            text("answer"),
            Event::Reasoning("why".into()),
            Event::Warn("w".into()),
            Event::Error("e".into()),
            Event::Busy(Some("x".into())),
            Event::AssistantStart,
            tool_start("Bash(ls)", None),
        ] {
            fe.emit(ev);
        }
        assert_eq!(open(&fe), (false, false, 0));
    }
}
