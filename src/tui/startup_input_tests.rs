#[test]
fn typeahead_becomes_prompt_text() {
    assert_eq!(
        super::typeahead_text(b"fix teh\x7f\x7fhe bug\x1b[D\r"),
        "fix the bug"
    );
    assert_eq!(super::typeahead_text(b""), "");
}

#[test]
fn mcp_panel_opens_for_list_only() {
    for t in ["/mcp", "/mcp ", "/mcp list", "  /mcp list  "] {
        assert!(super::is_mcp_panel(t), "{t}");
    }
    for t in ["/mcp login docs", "/mcpx", "/mcp disable all"] {
        assert!(!super::is_mcp_panel(t), "{t}");
    }
}

#[test]
fn dialog_more_lines_hint_has_its_own_row() {
    // Fits: no hint, every row is text.
    assert_eq!(super::dialog_body_rows(5, 10, 3), (10, 0, None));
    // Overflows: nine rows of text, the hint on the tenth, counting what's below.
    let (text_h, scroll, hint) = super::dialog_body_rows(30, 10, 0);
    assert_eq!((text_h, scroll), (9, 0));
    assert_eq!(
        hint.as_deref(),
        Some("… 21 more lines (pgdn/pgup to scroll)")
    );
    // Scrolled to the end: the last line is visible, not covered.
    let (text_h, scroll, hint) = super::dialog_body_rows(30, 10, 99);
    assert_eq!(scroll + text_h, 30);
    assert!(!hint.unwrap().contains("more lines"));
}
