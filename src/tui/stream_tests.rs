use super::*;

fn text(lines: &[Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

#[test]
fn work_panel_rows_never_wrap_past_the_panel_width() {
    use crate::agent::live::{Row, RowStatus};
    let rows = vec![
        agents_panel::Item::Main,
        agents_panel::Item::Agent(Row {
            key: "agent-1".into(),
            id: "a-very-long-background-agent-id".into(),
            kind: "general-purpose".into(),
            activity:
                "Reading src/very/deeply/nested/module/file.rs and every one of its neighbours"
                    .into(),
            secs: 3725,
            tokens: 123_456_789,
            status: RowStatus::Running,
            background: true,
        }),
        agents_panel::Item::Background {
            id: "b-very-long-background-shell-id".into(),
            label: "npm run build -- --watch --mode production".into(),
            secs: 3725,
        },
        agents_panel::Item::Shell {
            id: "bash_17".into(),
            label: "docker compose -f backend/docker-compose.yml up -d".into(),
            monitor: false,
        },
        agents_panel::Item::Shell {
            id: "monitor_23".into(),
            label: "CI run 39146861693 progress on the release branch".into(),
            monitor: true,
        },
    ];
    for w in [24usize, 40, 80, 200] {
        for it in &rows {
            let line = work_row(Some("agent-1"), it, w, true);
            assert!(
                line.width() <= w,
                "width {w}: {:?}",
                text(std::slice::from_ref(&line))
            );
        }
    }
}

#[test]
fn monitor_row_is_marked_apart_from_a_plain_shell() {
    let watch = agents_panel::Item::Shell {
        id: "monitor_23".into(),
        label: "CI run progress".into(),
        monitor: true,
    };
    let plain = agents_panel::Item::Shell {
        id: "bash_17".into(),
        label: "cargo build".into(),
        monitor: false,
    };
    let watch = text(&[work_row(None, &watch, 80, false)]);
    let plain = text(&[work_row(None, &plain, 80, false)]);
    let watch = watch[0].trim_start();
    let plain = plain[0].trim_start();
    assert!(watch.starts_with("◉ "), "{:?}", watch);
    assert!(watch.contains("· monitor_23"), "{:?}", watch);
    assert!(plain.starts_with("⚙ "), "{:?}", plain);
    assert!(plain.contains("· bash_17"), "{:?}", plain);
}

fn tool(done: bool) -> Block {
    Block::new(Kind::Tool(ToolBlock {
        id: "t".into(),
        label: "Bash(ls)".into(),
        display: None,
        content: Some("out".into()),
        is_error: false,
        children: vec![],
        started: Instant::now(),
        elapsed: done.then(|| Duration::from_millis(5)),
        progress: None,
    }))
}

#[test]
fn status_line_cost_is_short_and_hidden_when_free() {
    assert_eq!(cost_label(0.0, false), None, "local models and plan logins");
    assert_eq!(cost_label(0.0123, false).as_deref(), Some("$0.0123"));
    assert_eq!(cost_label(1.5, true).as_deref(), Some("$1.50?"));
    assert_eq!(cost_label(0.0, true).as_deref(), Some("$?"));
}

#[test]
fn pr_status_says_what_the_pull_request_waits_on() {
    use serde_json::json;
    let pr = |v: serde_json::Value| pr_label(&v);
    assert_eq!(pr(json!({"number": 12, "state": "OPEN", "reviewDecision": "APPROVED", "statusCheckRollup": [{"conclusion": "SUCCESS"}]})).as_deref(), Some("PR #12 approved"));
    assert_eq!(pr(json!({"number": 12, "state": "OPEN", "reviewDecision": "APPROVED", "statusCheckRollup": [{"conclusion": "FAILURE"}]})).as_deref(), Some("PR #12 checks failing"));
    assert_eq!(pr(json!({"number": 12, "state": "OPEN", "reviewDecision": "", "statusCheckRollup": [{"status": "IN_PROGRESS", "conclusion": ""}]})).as_deref(), Some("PR #12 checks running"));
    assert_eq!(pr(json!({"number": 12, "state": "OPEN", "reviewDecision": "CHANGES_REQUESTED", "statusCheckRollup": []})).as_deref(), Some("PR #12 changes requested"));
    assert_eq!(
        pr(json!({"number": 12, "state": "MERGED"})).as_deref(),
        Some("PR #12 merged")
    );
    assert_eq!(pr(json!({})), None);
}

#[test]
fn status_line_names_the_effort() {
    assert_eq!(
        effort_label(true, Some("high".into())).as_deref(),
        Some("high")
    );
    // Pastes count lines whatever the terminal sends as a line break.
    for raw in ["a\rb\rc", "a\r\nb\r\nc", "a\nb\nc", "a\nb\nc\n"] {
        assert_eq!(normalize_paste(raw).lines().count(), 3, "{raw:?}");
    }
    assert!(!normalize_paste("x\ry").contains('\r'));
    assert_eq!(
        effort_label(false, Some("high".into())).as_deref(),
        Some("thinking off")
    );
    assert_eq!(
        effort_label(true, None),
        None,
        "the provider's default isn't labelled"
    );
}

#[test]
fn btw_overlay_shows_the_selected_answer_with_earlier_questions_above() {
    let btw: Vec<(String, String)> = (1..=7)
        .map(|i| (format!("question {i}"), format!("answer {i}")))
        .collect();
    let s = text(&btw_overlay_lines(&btw, None, 6, 60)).join("\n");
    assert!(
        s.contains("question 7") && s.contains("answer 7") && s.contains("7 of 7"),
        "{s}"
    );
    assert!(
        s.contains("question 2") && !s.contains("question 1"),
        "five earlier questions: {s}"
    );
    assert!(!s.contains("answer 6"), "earlier answers stay hidden: {s}");
    assert!(
        text(&btw_overlay_lines(&btw, None, 0, 60))
            .join("\n")
            .contains("1 of 7")
    );
    let none = text(&btw_overlay_lines(&[], None, 0, 60)).join("\n");
    assert!(none.contains("No side questions"), "{none}");
    // A single question has nothing to navigate, so the arrow keys are left
    // out of the hint; the fork and clear keys stay.
    let one = vec![("q".to_string(), "a".to_string())];
    let hint = text(&btw_overlay_lines(&one, None, 0, 60)).join("\n");
    assert!(!hint.contains("←/→"), "no arrows with one question: {hint}");
    assert!(
        hint.contains("f fork") && hint.contains("esc close"),
        "{hint}"
    );
}

#[test]
fn btw_overlay_shows_the_question_while_the_answer_is_still_coming() {
    let btw = vec![("earlier".to_string(), "answer".to_string())];
    let asking = BtwAsk {
        question: "is this right?".into(),
        error: None,
        started: Instant::now(),
    };
    // The question in flight is one past the answered ones.
    let s = text(&btw_overlay_lines(&btw, Some(&asking), 1, 60)).join("\n");
    assert!(s.contains("is this right?"), "{s}");
    assert!(s.contains("Asking the side question…"), "{s}");
    assert!(
        s.contains("esc close") && !s.contains("f fork"),
        "nothing but esc works while it asks: {s}"
    );
    assert!(!s.contains("answer"), "no answer has landed yet: {s}");

    let failed = BtwAsk {
        question: "is this right?".into(),
        error: Some("You've hit your weekly limit".into()),
        started: Instant::now(),
    };
    let f = text(&btw_overlay_lines(&btw, Some(&failed), 1, 60)).join("\n");
    assert!(
        f.contains("Side question failed") && f.contains("weekly limit"),
        "the panel explains the failure: {f}"
    );
    assert!(
        !f.contains("Asking"),
        "a failure is not still spinning: {f}"
    );
}

#[test]
fn prompt_colours_a_leading_slash_command_but_not_a_path() {
    let accent = Style::default()
        .fg(pal().accent)
        .add_modifier(Modifier::BOLD);
    let spans = input_row_spans("/btw is this right", true);
    assert_eq!(spans[0].content.as_ref(), "/btw");
    assert_eq!(spans[0].style, accent);
    // The text is unchanged, so the cursor columns still line up.
    let shown: String = spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(shown, "/btw is this right");
    // A path has a second slash, so it is not a command and stays plain.
    let path = input_row_spans("/usr/bin/env", true);
    assert_eq!(
        path.len(),
        1,
        "one plain span: {:?}",
        text(&[Line::from(path)])
    );
    // Continuation rows never treat their first token as a command.
    assert_eq!(input_row_spans("/btw", false).len(), 1);
    // An @mention takes the link colour wherever it sits on the row.
    let m = input_row_spans("look at @src/main.rs", false);
    assert!(
        m.iter()
            .any(|s| s.content.as_ref() == "@src/main.rs" && s.style.fg == Some(pal().link)),
        "{:?}",
        text(&[Line::from(m)])
    );
}

#[test]
fn long_dialog_options_wrap_instead_of_being_cut_off() {
    let long = "Rename the playback queue module and update every import, then rerun the web tests and the type checker before reporting back".to_string();
    let options = vec!["Yes".to_string(), long.clone(), "No".to_string()];
    let rows = option_rows(&options, 1, 40);
    let shown: String = text(&rows).join(" ");
    for word in long.split_whitespace() {
        assert!(shown.contains(word), "{word} missing: {shown}");
    }
    assert!(rows.iter().all(|r| r.width() <= 40), "{:?}", text(&rows));
    assert!(rows.len() > 3, "the long option takes several rows");
    assert!(text(&rows)[1].starts_with(" ❯ 2."), "{:?}", text(&rows));
    // Continuation rows line up under the option's text, not its number.
    assert!(text(&rows)[2].starts_with("      "), "{:?}", text(&rows));
    // Only 9 options at a time, around the selection.
    let many: Vec<String> = (1..=20).map(|i| format!("option {i}")).collect();
    let rows = text(&option_rows(&many, 15, 40));
    assert_eq!(rows.len(), 9);
    assert!(rows.iter().any(|r| r.contains("❯ 16.")), "{rows:?}");
}

#[test]
fn highlighted_diff_lines_fill_the_width() {
    // Force a colored theme: NO_COLOR is a process-wide latch and another test
    // can have turned it off. The styled text is built by hand rather than with
    // `ui::diff`, because crossterm drops every SGR parameter (leaving a bare
    // reset) when NO_COLOR is set in the environment, which would leave nothing
    // highlighted to test.
    super::theme::enable_colors_for_test();
    let display = "\u{1b}[48;2;80;20;20m    2 -b = 2\u{1b}[0m\n\u{1b}[48;2;20;60;20m    2 +b = 20000\u{1b}[0m".to_string();
    let b = Block::new(Kind::Tool(ToolBlock {
        id: "e".into(),
        label: "Edit(a.py)".into(),
        display: Some(display),
        content: Some("ok".into()),
        is_error: false,
        children: vec![],
        started: Instant::now(),
        elapsed: Some(Duration::from_millis(5)),
        progress: None,
    }));
    let width = 60;
    let lines = render_block(&b, width, true, false);
    let highlighted: Vec<&Line> = lines
        .iter()
        .filter(|l| {
            l.spans
                .iter()
                .any(|s| s.style.bg.is_some_and(|c| c != Color::Reset))
        })
        .collect();
    assert!(highlighted.len() >= 2, "{:?}", text(&lines));
    for l in highlighted {
        assert_eq!(l.width(), width, "{:?}", text(std::slice::from_ref(l)));
    }
}

#[test]
fn a_blocked_call_styles_its_code_ticks() {
    let b = Block::new(Kind::Tool(ToolBlock {
        id: "x".into(),
        label: "Bash(sleep 20)".into(),
        display: Some(
            "Blocked: a foreground `sleep 20` only uses up time. To wait for a \
             background command, call BashOutput with `wait_secs`."
                .into(),
        ),
        content: Some(String::new()),
        is_error: true,
        children: vec![],
        started: Instant::now(),
        elapsed: Some(Duration::from_millis(5)),
        progress: None,
    }));
    let rows = text(&render_block(&b, 30, false, false));
    let joined = rows.join(" ");
    let flat = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains("foreground sleep 20 only"), "{rows:?}");
    assert!(!joined.contains('`'), "no ticks survive: {rows:?}");
}

#[test]
fn a_blocked_call_colours_only_its_label() {
    let b = Block::new(Kind::Tool(ToolBlock {
        id: "x".into(),
        label: "Bash(sleep 20)".into(),
        display: Some("Blocked: a foreground `sleep 20` only uses up time.".into()),
        content: Some(String::new()),
        is_error: true,
        children: vec![],
        started: Instant::now(),
        elapsed: Some(Duration::from_millis(5)),
        progress: None,
    }));
    let rows = render_block(&b, 60, false, false);
    let spans: Vec<&Span> = rows.iter().flat_map(|l| l.spans.iter()).collect();
    assert!(
        spans
            .iter()
            .any(|s| s.content == "Blocked:" && s.style.fg == Some(pal().error)),
        "{rows:?}"
    );
    // The advice after the label is prose, not another error.
    assert!(
        spans
            .iter()
            .any(|s| s.content.contains("only uses up time") && s.style.fg == Some(pal().dim)),
        "{rows:?}"
    );
    assert!(
        !spans
            .iter()
            .any(|s| s.content.contains("only uses up time") && s.style.fg == Some(pal().error)),
        "{rows:?}"
    );
}

#[test]
fn command_output_keeps_literal_backticks_when_collapsed_or_expanded() {
    let mut b = bash_block(8);
    let Kind::Tool(t) = &mut b.kind else {
        unreachable!()
    };
    t.display = Some("literal `ticks`\n… +7 lines".into());
    t.content = Some("literal `ticks`\nsecond `line`\nthird".into());
    t.is_error = true;
    for expanded in [false, true] {
        let joined = text(&render_block(&b, 80, expanded, false)).join("\n");
        assert!(joined.contains("literal `ticks`"), "{joined}");
        if expanded {
            assert!(joined.contains("second `line`"), "{joined}");
        }
    }
}

fn bash_block(n: usize) -> Block {
    let content: String = (1..=n).map(|i| format!("row {i}\n")).collect();
    let lines: Vec<&str> = content.lines().collect();
    let mut display = lines[..4.min(n)].join("\n");
    if n > 4 {
        display.push_str(&format!("\n… +{}", crate::util::plural(n - 4, "line")));
    }
    Block::new(Kind::Tool(ToolBlock {
        id: "b".into(),
        label: "Bash(seq)".into(),
        display: Some(display),
        content: Some(content),
        is_error: false,
        children: vec![],
        started: Instant::now(),
        elapsed: Some(Duration::from_millis(5)),
        progress: None,
    }))
}

#[test]
fn expanded_bash_output_shows_each_line_once() {
    let rows = text(&render_block(&bash_block(9), 80, true, false));
    for i in 1..=9 {
        assert_eq!(
            rows.iter()
                .filter(|r| r.trim_start_matches(['↳', ' ']).trim_end() == format!("row {i}"))
                .count(),
            1,
            "row {i}: {rows:?}"
        );
    }
    assert!(
        !rows
            .iter()
            .any(|r| r.contains("─── output ───") || r.contains("+5 lines")),
        "{rows:?}"
    );
    // Collapsed: the preview and its note.
    let rows = text(&render_block(&bash_block(9), 80, false, false));
    assert!(
        rows.iter().any(|r| r.contains("+5 lines")) && !rows.iter().any(|r| r.contains("row 6")),
        "{rows:?}"
    );
    // A summary display (Read) still gets the output below a rule.
    let mut b = bash_block(3);
    if let Kind::Tool(t) = &mut b.kind {
        t.label = "Read(a.txt)".into();
        t.display = Some("Read 3 lines".into());
    }
    let rows = text(&render_block(&b, 80, true, false));
    assert!(
        rows.iter().any(|r| r.contains("Read 3 lines"))
            && rows.iter().any(|r| r.contains("─── output ───")),
        "{rows:?}"
    );
}

#[test]
fn bash_file_changes_collapse_to_a_summary_line() {
    let diff = crate::ui::diff("old line\n", "new line\n", 20);
    let mut b = bash_block(2);
    if let Kind::Tool(t) = &mut b.kind {
        t.display = Some(format!(
            "{}\n{}Changed 1 file: f.txt\nf.txt\n{diff}",
            t.display.as_deref().unwrap(),
            crate::tools::bash::CHANGES_MARK
        ));
        t.content = Some(format!(
            "{}\n\n[This command changed 1 file: f.txt]",
            t.content.as_deref().unwrap()
        ));
    }
    let rows = text(&render_block(&b, 80, false, false));
    assert!(
        rows.iter()
            .any(|r| r.contains("Changed 1 file: f.txt") && r.contains("ctrl+o to expand")),
        "{rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r.contains("new line")),
        "the diff waits for ctrl+o: {rows:?}"
    );
    assert!(rows.iter().any(|r| r.contains("row 1")), "{rows:?}");
    let rows = text(&render_block(&b, 80, true, false));
    assert!(
        rows.iter().any(|r| r.contains("-old line"))
            && rows.iter().any(|r| r.contains("+new line")),
        "{rows:?}"
    );
    assert_eq!(
        rows.iter().filter(|r| r.contains("row 2")).count(),
        1,
        "{rows:?}"
    );
}

#[test]
fn collapsed_tool_output_is_one_style() {
    let lines = render_block(&bash_block(3), 80, false, false);
    let fgs: Vec<Option<Color>> = lines[1..]
        .iter()
        .map(|l| l.spans.last().unwrap().style.fg)
        .collect();
    assert!(fgs.windows(2).all(|w| w[0] == w[1]), "{fgs:?}");
}

#[test]
fn hidden_line_counts_agree_in_number() {
    // Seven summary lines, six shown.
    let mut b = bash_block(3);
    if let Kind::Tool(t) = &mut b.kind {
        t.label = "Grep(x)".into();
        t.display = Some(
            (1..=7)
                .map(|i| format!("match {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    let rows = text(&render_block(&b, 80, false, false));
    assert!(
        rows.iter().any(|r| r.contains("… +1 line (ctrl+o")),
        "{rows:?}"
    );
}

#[test]
fn the_footer_drops_low_priority_items_first() {
    // left "? keys" (prio 2); right: branch (3), model (8), ctx (7), awake (1), tok/s (1).
    let left = (16, 2);
    let right = [(8, 3), (4, 8), (6, 7), (5, 1), (9, 1)];
    let full: usize = 16 + FOOTER_GAP + right.iter().map(|r| r.0).sum::<usize>() + 3 * 4 + 1;
    assert_eq!(fit_footer(left, &right, full), (true, vec![0, 1, 2, 3, 4]));
    // One column short: tok/s goes first, then awake, then the hint.
    assert_eq!(fit_footer(left, &right, full - 1), (true, vec![0, 1, 2, 3]));
    assert_eq!(fit_footer(left, &right, full - 13), (true, vec![0, 1, 2]));
    assert_eq!(fit_footer(left, &right, full - 21), (false, vec![0, 1, 2]));
    // The model outlasts the branch and the context meter.
    assert_eq!(fit_footer(left, &right, 6), (false, vec![1]));
    // An important left side (a mode) stays, and the rest makes room.
    let (show, keep) = fit_footer((36, 9), &right, 60);
    assert!(show && keep == vec![1, 2], "{keep:?}");
    // Whatever is kept, the two sides never touch.
    for w in 1..80 {
        let (show, keep) = fit_footer(left, &right, w);
        let rw =
            keep.iter().map(|&i| right[i].0).sum::<usize>() + 3 * keep.len().saturating_sub(1) + 1;
        assert!(!show || 16 + FOOTER_GAP + rw <= w, "width {w}");
    }
}

#[test]
fn hints_fit_the_width_at_word_boundaries() {
    let ph = [
        "Try \"fix the failing test\"",
        "/ commands",
        "@ files",
        "! shell",
    ];
    assert_eq!(
        fit_parts(&ph, 80),
        "Try \"fix the failing test\" · / commands · @ files · ! shell"
    );
    assert_eq!(
        fit_parts(&ph, 46),
        "Try \"fix the failing test\" · / commands"
    );
    assert_eq!(fit_parts(&ph, 20), "Try \"fix the…");
    assert_eq!(
        fit_words("shift+tab cycles modes (auto mode runs autonomously)", 30),
        "shift+tab cycles modes (auto…"
    );
    assert_eq!(fit_words("abcdefghij", 5), "abcd…");
    for w in 0..60 {
        assert!(
            unicode_width::UnicodeWidthStr::width(fit_parts(&ph, w).as_str()) <= w,
            "{w}"
        );
    }
    // The welcome tips at 50 columns end on a whole word.
    let b = Block {
        kind: Kind::Welcome {
            model: "m".into(),
            cwd: "~".into(),
        },
        expanded: false,
        version: 0,
        cache: None,
    };
    let tips = text(&render_block(&b, 50, false, false))
        .into_iter()
        .find(|l| l.contains("Try:"))
        .unwrap();
    assert!(
        tips.chars().count() <= 50
            && (tips.ends_with('…')
                || tips.ends_with("files")
                || tips.ends_with("shell")
                || tips.ends_with("output")),
        "{tips}"
    );
}

#[test]
fn the_permission_question_wraps_at_narrow_widths() {
    let wide = text(&proceed_rows(true, true, 80));
    assert_eq!(
        wide,
        ["Allow this action? (e: edit the change in $EDITOR first)"]
    );
    let narrow = text(&proceed_rows(true, true, 50));
    assert_eq!(
        narrow,
        ["Allow this action?", "e: edit the change in $EDITOR first"]
    );
    let tiny = proceed_rows(true, true, 20);
    assert!(
        tiny.iter().all(|l| l.width() <= 20) && text(&tiny).join(" ").contains("$EDITOR"),
        "{:?}",
        text(&tiny)
    );
    assert_eq!(text(&proceed_rows(true, false, 80)), ["Allow this action?"]);
    assert_eq!(text(&proceed_rows(false, false, 80)), [""]);
}

#[test]
fn the_permission_dialog_says_each_thing_once() {
    let (t, b) = permission_title_body("Edit(calc.py)", "Edit", "calc.py\n  1 a\n  2 b");
    assert_eq!((t.as_str(), b.as_str()), ("Edit(calc.py)", "  1 a\n  2 b"));
    let (t, b) = permission_title_body(
        "Bash(python3 calc.py && ls)",
        "Bash",
        "python3 calc.py && ls\n\nRun it",
    );
    assert_eq!(
        (t.as_str(), b.as_str()),
        ("Bash", "python3 calc.py && ls\n\nRun it")
    );
    // A long command the label cut short.
    let cmd = "x".repeat(150);
    let label = format!("Bash({})", crate::util::one_line(&cmd, 100));
    assert_eq!(
        permission_title_body(&label, "Bash", &format!("{cmd}\n")).0,
        "Bash"
    );
    // Anything else stays as it was.
    let (t, b) = permission_title_body(
        "WebFetch(https://a.b)",
        "WebFetch",
        "{\n  \"url\": \"https://a.b\"\n}",
    );
    assert_eq!(
        (t.as_str(), b.as_str()),
        ("WebFetch(https://a.b)", "{\n  \"url\": \"https://a.b\"\n}")
    );
}

#[test]
fn a_permission_prompt_shows_the_auto_deny_countdown() {
    let now = Instant::now();
    assert_eq!(permission_title_with_countdown("Bash", None, now), " Bash ");
    assert_eq!(
        permission_title_with_countdown("Bash", Some(now + Duration::from_secs(75)), now),
        " Bash · auto-deny in 75s "
    );
    // Rounds up, so a fraction of a second left still reads as 1s.
    assert_eq!(
        permission_title_with_countdown("Bash", Some(now + Duration::from_millis(1500)), now),
        " Bash · auto-deny in 2s "
    );
    // A deadline that has passed reads 0s; the agent closes the prompt at once.
    assert_eq!(
        permission_title_with_countdown("Bash", Some(now), now),
        " Bash · auto-deny in 0s "
    );
}

#[test]
fn a_permission_reason_renders_as_markdown_above_the_body() {
    let req = PermissionRequest {
        tool: "Bash".into(),
        label: "Bash(rm -rf build)".into(),
        detail: "rm -rf build".into(),
        always_label: "Always allow Bash(rm:*)".into(),
        reason: Some("Blocked by auto mode: `rm -rf` deletes the build directory".into()),
        proposed: None,
        auto_deny: None,
    };
    let d = Dialog::Permission {
        req,
        sel: 0,
        tx: None,
        feedback: None,
        deadline: None,
        scroll: 0,
    };
    let reason = "Blocked by auto mode: `rm -rf` deletes the build directory";
    let lines = dialog_body_lines(&d, "rm -rf build", Some(reason), 60);
    let rows = text(&lines);
    // The reason keeps its wording, then a blank row, then the command.
    let at = rows
        .iter()
        .position(|l| l.contains("deletes the build directory"))
        .expect("reason row");
    assert!(
        rows[..at].iter().all(|l| !l.contains("rm -rf build")),
        "reason comes first: {rows:?}"
    );
    assert!(
        rows[at + 1].trim().is_empty(),
        "blank row between reason and body: {rows:?}"
    );
    assert!(
        rows[at + 2..].iter().any(|l| l.contains("rm -rf build")),
        "the command follows: {rows:?}"
    );
}

/// Inline mode prints a streamed answer piece by piece, and what reaches
/// scrollback is exactly the closed answer's render, printed once.
#[test]
fn inline_prints_a_streamed_answer_once_in_its_final_form() {
    let doc = "Intro paragraph that is long enough to wrap at this width.\n\n```rust\nfn a() {}\n\nfn b() {}\n```\nAfter.\n\n- x\n- y\n\nDone.";
    let width = 40;
    let mut blocks = vec![
        Block::new(Kind::User("hi".into())),
        Block::new(Kind::Assistant(String::new())),
    ];
    let mut iv = InlineView::default();
    let mut printed = Vec::new();
    for end in (0..doc.len()).step_by(3).chain([doc.len()]) {
        if let Kind::Assistant(s) = &mut blocks[1].kind {
            *s = doc[..end].to_string();
        }
        printed.extend(settle(&blocks, Some(1), None, &mut iv, width, None));
        // The live region holds only what hasn't been printed.
        let shown = text(&live(&blocks, Some(1), &iv, false, width, None));
        assert!(
            !shown.iter().any(|l| l.contains("Intro"))
                || !text(&printed).iter().any(|l| l.contains("Intro")),
            "at byte {end}"
        );
    }
    printed.extend(settle(&blocks, None, None, &mut iv, width, None));
    let mut want = render_block(&blocks[0], width, false, false);
    want.push(Line::raw(""));
    want.extend(render_block(&blocks[1], width, false, false));
    want.push(Line::raw(""));
    assert_eq!(text(&printed), text(&want));
    assert!(live(&blocks, None, &iv, false, width, None).is_empty());
}

#[test]
fn inline_waits_for_running_tools_and_reprints_on_request() {
    let mut blocks = vec![
        Block::new(Kind::User("hello".into())),
        tool(false),
        Block::new(Kind::User("later".into())),
    ];
    let mut iv = InlineView::default();
    let has = |lines: &[Line<'static>], s: &str| text(lines).iter().any(|l| l.contains(s));
    let first = settle(&blocks, None, None, &mut iv, 60, None);
    assert!(
        has(&first, "hello") && !has(&first, "later"),
        "stops at the running tool"
    );
    assert!(has(&live(&blocks, None, &iv, false, 60, None), "later"));
    blocks[1] = tool(true);
    let rest = settle(&blocks, None, None, &mut iv, 60, None);
    assert!(has(&rest, "Bash") && has(&rest, "later"));
    assert!(
        settle(&blocks, None, None, &mut iv, 60, None).is_empty(),
        "nothing prints twice"
    );
    iv.reprint = true;
    assert!(
        has(&settle(&blocks, None, None, &mut iv, 60, None), "Bash"),
        "ctrl+o prints the last tool again"
    );
}

fn assistant(t: &str) -> Block {
    Block::new(Kind::Assistant(t.into()))
}

/// A finished turn (narration, thinking, a tool, the summary, a warning),
/// then a running one.
fn quiet_blocks() -> Vec<Block> {
    vec![
        Block::new(Kind::User("fix it".into())),
        Block::new(Kind::Thinking {
            text: "hmm".into(),
            started: Instant::now(),
            elapsed: Some(Duration::from_secs(1)),
        }),
        assistant("Let me look at the code."),
        tool(true),
        assistant("Fixed the bug in a.rs; cargo test passes."),
        Block::new(Kind::Note("Spend guard: $1 reached".into(), warn())),
        Block::new(Kind::User("now docs".into())),
        assistant("Now I'll edit the docs."),
        tool(false),
    ]
}

#[test]
fn quiet_view_hides_steps_and_shows_the_final_summary() {
    let blocks = quiet_blocks();
    let hidden = |m: &QuietMask| {
        (0..blocks.len())
            .filter(|&i| m.hides(i))
            .collect::<Vec<_>>()
    };
    // While the second turn runs, all of its steps are hidden, even its
    // newest text, which may still turn out to be the summary.
    let running = quiet_mask(&blocks, true);
    assert_eq!(hidden(&running), [1, 2, 3, 7, 8]);
    assert_eq!(running.hold, 7);
    // Once it's over, its last reply is the summary and shows.
    let mut done = quiet_blocks();
    done[8] = tool(true);
    done.push(assistant("Updated README.md."));
    let finished = quiet_mask(&done, false);
    assert_eq!(
        (0..done.len())
            .filter(|&i| finished.hides(i))
            .collect::<Vec<_>>(),
        [1, 2, 3, 7, 8]
    );
    assert_eq!(finished.hold, done.len());
    // Prompts, the summary and notices always show.
    for i in [0, 4, 5, 6, 9] {
        assert!(!finished.hides(i), "block {i}");
    }
    // Nothing is removed: the blocks are all still there for ctrl+o.
    assert_eq!(done.len(), 10);
}

#[test]
fn quiet_inline_prints_the_summary_when_the_turn_ends() {
    let has = |lines: &[Line<'static>], s: &str| text(lines).iter().any(|l| l.contains(s));
    let mut blocks = quiet_blocks();
    blocks.truncate(6);
    blocks.push(Block::new(Kind::User("now docs".into())));
    blocks.push(assistant("Now I'll edit the docs."));
    let mut iv = InlineView::default();
    let q = quiet_mask(&blocks, true);
    let first = settle(&blocks, None, None, &mut iv, 60, Some(&q));
    assert!(
        has(&first, "fix it")
            && has(&first, "Fixed the bug")
            && has(&first, "Spend guard")
            && has(&first, "now docs")
    );
    assert!(
        !has(&first, "Let me look") && !has(&first, "Bash"),
        "the first turn's steps stay out"
    );
    assert!(
        !has(&first, "edit the docs"),
        "held back while the turn runs"
    );
    assert!(
        live(&blocks, None, &iv, false, 60, Some(&q)).is_empty(),
        "only the status line while working"
    );
    // The turn ends with that text as its reply: it prints now.
    let q = quiet_mask(&blocks, false);
    let rest = settle(&blocks, None, None, &mut iv, 60, Some(&q));
    assert!(has(&rest, "edit the docs"));
    // ctrl+o prints the last turn's hidden steps (none in this turn: the
    // last tool block instead, as without quiet).
    iv.reprint = true;
    let again = settle(&blocks, None, None, &mut iv, 60, Some(&q));
    assert!(has(&again, "Bash"));
}

#[tokio::test]
async fn quiet_view_of_a_real_turn_keeps_the_session_intact() {
    use crate::agent::tests::{make_env_with, mock_server, tc, tmpdir};
    use serde_json::json;
    let dir = tmpdir("quiet-tui");
    std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
    let (base, _) = mock_server(vec![
        json!({"content": "Let me read the file.", "tool_calls": tc("Read", json!({"file_path": "a.txt"}))}),
        json!({"content": "Read a.txt; nothing changed."}),
        json!({"content": "I read a.txt with the Read tool."}),
    ])
    .await;
    let env = make_env_with(
        &base,
        &dir,
        crate::permissions::Mode::AcceptEdits,
        32768,
        |c| c.optimize.quiet = Some(true),
    );
    let mut agent = Agent::new(env);
    agent
        .run_turn("read a.txt", vec![], CancellationToken::new())
        .await
        .unwrap();
    let blocks = blocks_from_messages(&agent);
    let q = quiet_mask(&blocks, false);
    let shown: Vec<String> = blocks
        .iter()
        .enumerate()
        .filter(|(i, _)| !q.hides(*i))
        .filter_map(|(_, b)| match &b.kind {
            Kind::User(t) | Kind::Assistant(t) => Some(t.clone()),
            Kind::Tool(t) => Some(t.label.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(shown, ["read a.txt", "Read a.txt; nothing changed."]);
    // ctrl+o's view (no mask) and the session still have every step.
    assert!(
        blocks
            .iter()
            .any(|b| matches!(&b.kind, Kind::Assistant(t) if t == "Let me read the file."))
    );
    assert!(blocks.iter().any(|b| matches!(b.kind, Kind::Tool(_))));
    assert!(crate::commands::last_turn_steps(&agent).contains("Let me read the file."));
    // "What did you do?" is answered from the full history.
    agent
        .run_turn("what did you do?", vec![], CancellationToken::new())
        .await
        .unwrap();
    assert!(
        agent
            .messages
            .iter()
            .any(|m| *m.content == "Let me read the file." && m.tool_calls.len() == 1)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Rendering the streaming reply piece by piece must give exactly the lines
/// a full re-render gives, whatever the chunking and width changes.
#[test]
fn incremental_stream_render_matches_full_render() {
    let doc: String = (0..60)
        .map(|i| format!("## Part {i}\n\nSome words that will wrap at narrow widths, with `code` and **bold** {i}.\n\n- item\n"))
        .collect();
    for (chunk, widths) in [
        (1usize, vec![40u16]),
        (7, vec![30, 30, 80]),
        (23, vec![100]),
        (500, vec![25, 60]),
    ] {
        let mut b = Block::new(Kind::Assistant(String::new()));
        let mut state = None;
        let mut sent = 0;
        let mut step = 0;
        while sent < doc.len() {
            let mut end = (sent + chunk).min(doc.len());
            while !doc.is_char_boundary(end) {
                end += 1;
            }
            if let Kind::Assistant(s) = &mut b.kind {
                s.push_str(&doc[sent..end]);
            }
            sent = end;
            let width = widths[step % widths.len()];
            step += 1;
            let Block { kind, cache, .. } = &mut b;
            let n = stream_into(kind, cache, &mut state, 0, width);
            let full = render_block(&b, width as usize, false, true);
            let got = &b.cache.as_ref().unwrap().4;
            assert_eq!(n, full.len());
            assert_eq!(
                text(got),
                text(&full),
                "chunk {chunk} width {width} at byte {sent}"
            );
        }
    }
}
