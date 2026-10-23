use super::*;

#[test]
fn ignored_files_for_at_completion() {
    let d = std::env::temp_dir().join(format!("chanto-ignored-{}", uuid::Uuid::new_v4().simple()));
    for p in [".git/objects", "build", "src"] {
        std::fs::create_dir_all(d.join(p)).unwrap();
    }
    for (p, t) in [
        (".gitignore", "build/\n*.log\n"),
        ("src/a.rs", ""),
        ("build/out.txt", ""),
        ("debug.log", ""),
        (".git/HEAD", "x"),
    ] {
        std::fs::write(d.join(p), t).unwrap();
    }
    let files = list_files(&d);
    assert!(
        files.contains(&"src/a.rs".replace('/', std::path::MAIN_SEPARATOR_STR))
            && !files.iter().any(|f| f.contains("out.txt"))
    );
    let mut ignored = list_ignored_files(&d, &files);
    ignored.sort();
    assert_eq!(
        ignored,
        vec![
            "build/out.txt".replace('/', std::path::MAIN_SEPARATOR_STR),
            "debug.log".to_string()
        ]
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn paste_tag_under_the_cursor() {
    let t = "see [Pasted text #2 +40 lines] and";
    let (s, e) = (4, 4 + "[Pasted text #2 +40 lines]".len());
    assert_eq!(paste_tag_at(t, s), Some((s, e, 2)));
    assert_eq!(
        paste_tag_at(t, e),
        Some((s, e, 2)),
        "just after the tag counts"
    );
    assert_eq!(paste_tag_at(t, 1), None);
    assert_eq!(paste_tag_at(t, t.len()), None);
}

#[test]
fn the_free_text_cursor_sits_on_the_typed_row() {
    let mut input = Input::default();
    input.set("adsd");
    // A permission: a spacer and three options, no hint row, so the input
    // follows the last option directly.
    let mut permission = vec![
        Line::raw(""),
        Line::raw("1. Yes"),
        Line::raw("2. Always allow"),
        Line::raw("3. No · say what to do instead (esc)"),
    ];
    assert_eq!(
        push_typing_line(&mut permission, &input, Rect::new(3, 10, 60, 20)),
        (3 + 5 + 4, 14)
    );
    // A question adds its key hint above the input; the cursor moves with it.
    let mut question = vec![
        Line::raw(""),
        Line::raw("1. Yes"),
        Line::raw("  type an answer · enter to send"),
    ];
    assert_eq!(
        push_typing_line(&mut question, &input, Rect::new(0, 10, 60, 20)),
        (9, 13)
    );
}

#[test]
fn a_running_thought_reads_as_thinking() {
    assert_eq!(thinking_label(None), "❖ thinking…");
}

#[test]
fn a_finished_thought_always_states_a_duration() {
    // Sub-second thoughts must still show a duration: a bare label was the
    // confusing thing this replaced.
    assert_eq!(
        thinking_label(Some(Duration::from_millis(0))),
        "❖ thinking for 1s"
    );
    assert_eq!(
        thinking_label(Some(Duration::from_millis(400))),
        "❖ thinking for 1s"
    );
    assert_eq!(
        thinking_label(Some(Duration::from_millis(1500))),
        "❖ thinking for 2s"
    );
    assert_eq!(
        thinking_label(Some(Duration::from_secs(12))),
        "❖ thinking for 12s"
    );
}

#[test]
fn long_running_thoughts_step_through_progressive_wording() {
    assert_eq!(thinking_verb(0), "thinking");
    assert_eq!(thinking_verb(9), "thinking");
    assert_eq!(thinking_verb(10), "still thinking");
    assert_eq!(thinking_verb(19), "still thinking");
    assert_eq!(thinking_verb(20), "thinking more");
    assert_eq!(thinking_verb(30), "thinking some more");
    assert_eq!(thinking_verb(45), "deep in thought");
    assert_eq!(thinking_verb(600), "deep in thought");
}

#[test]
fn the_spinner_cycles_single_cell_frames_in_place() {
    // Every frame is one cell wide and non-ambiguous (East-Asian Ambiguous
    // glyphs like `·` and `✽` render two cells in a CJK-width terminal, so
    // `width_cjk()` catches them), so the label after it never shifts, and
    // the `% SPIN.len()` index draw_status uses wraps back to the first.
    use unicode_width::UnicodeWidthStr;
    assert!(
        SPIN.iter()
            .all(|c| c.chars().count() == 1 && c.width_cjk() == 1),
        "one cell per frame"
    );
    assert_eq!(SPIN[SPIN.len() % SPIN.len()], SPIN[0]);
    assert!(SPIN.contains(&"◰"), "a square frame is present");
}

#[test]
fn the_title_icon_is_steady_while_busy() {
    // The busy title must not change frame to frame: the OS lays the window
    // title out with the UI font, so a rotating glyph nudges it. It stays a
    // single diamond; done and rest are distinct single-cell marks.
    assert_eq!(title_icon(true, false), "❖");
    assert_eq!(
        title_icon(true, true),
        "❖",
        "a running turn's icon doesn't depend on the last one"
    );
    assert_eq!(title_icon(false, true), "✓");
    assert_eq!(title_icon(false, false), "❯");
    for icon in [
        title_icon(true, false),
        title_icon(false, true),
        title_icon(false, false),
    ] {
        assert_eq!(icon.chars().count(), 1, "one cell: {icon:?}");
    }
    // Busy differs from both settled states, so a glance tells them apart.
    assert_ne!(title_icon(true, false), title_icon(false, true));
    assert_ne!(title_icon(true, false), title_icon(false, false));
}

#[test]
fn the_window_title_names_the_project_but_not_twice() {
    assert_eq!(window_title("❯", Some("api")), "❯ api — chanto");
    assert_eq!(window_title("✓", Some("chanto")), "✓ chanto");
    assert_eq!(window_title("❖", None), "❖ chanto");
    assert_eq!(window_title("❯", Some("  ")), "❯ chanto");
}

#[test]
fn the_mode_hint_yields_before_the_label_or_the_right_side() {
    use unicode_width::UnicodeWidthStr;
    let label = " ▸ auto mode";
    let hint = "shift+tab changes mode";
    // Wide enough: the hint teaches the shortcut.
    let wide = label.width() + 3 + hint.width();
    assert_eq!(
        footer_left(label, Some(hint), wide),
        format!("{label} · {hint}")
    );
    // One column short: the hint goes whole rather than being cut, and the
    // label survives for the context meter beside it.
    assert_eq!(footer_left(label, Some(hint), wide - 1), label);
    // Even the label doesn't fit: truncate it, as before.
    assert!(footer_left(label, Some(hint), 4).ends_with('…'));
    // No hint (default and bypass modes) is unchanged.
    assert_eq!(footer_left("? keys", None, 6), "? keys");
}

#[test]
fn reduced_motion_holds_a_single_busy_frame() {
    // With reduced motion the frame never changes, so nothing repaints under
    // a screen reader; otherwise it advances with the clock.
    for ms in [0, 120, 600, 10_000] {
        assert_eq!(busy_frame(true, ms), "❖");
    }
    assert_eq!(busy_frame(false, 0), SPIN[0]);
    assert_ne!(busy_frame(false, 0), busy_frame(false, 120));
    assert_eq!(busy_frame(false, 120), SPIN[1]);
}
