use super::*;

#[test]
fn startup_questions_wrap_at_word_boundaries() {
    let text = "Only trust folders whose contents you trust. Trust /a/very/long/path?\n  - .chanto/config.toml (hooks and permission rules)";
    let w = word_wrap(text, 29);
    assert!(
        w.lines()
            .all(|l| l.chars().count() <= 29 || !l.trim().contains(' ')),
        "{w}"
    );
    assert_eq!(
        w.split_whitespace().collect::<Vec<_>>(),
        text.split_whitespace().collect::<Vec<_>>(),
        "no word is split or lost"
    );
    assert!(
        w.contains("\n    (hooks"),
        "an item's continuation lines up under its text: {w}"
    );
    assert_eq!(word_wrap("short", 80), "short");
}

#[test]
fn redirected_diagnostics_have_no_colors() {
    assert_eq!(diagnostic("error", Color::Red, false), "error");
    assert_eq!(diagnostic("warning", Color::Yellow, false), "warning");
    assert!(diagnostic("error", Color::Red, true).contains('\u{1b}'));
}

fn strip(s: &str) -> String {
    let sgr = regex::Regex::new("\x1b\\[[0-9;]*m").unwrap();
    let osc8 = regex::Regex::new("\x1b\\]8;;[^\x1b]*\x1b\\\\").unwrap();
    osc8.replace_all(&sgr.replace_all(s, ""), "").to_string()
}

#[test]
fn a_summary_label_is_the_first_word_when_it_reads_as_one() {
    assert_eq!(
        error_label("Blocked: a foreground `sleep 20` only uses up time."),
        Some(8)
    );
    assert_eq!(error_label("Error: no such file"), Some(6));
    assert_eq!(error_label("the command exited 1"), None);
    assert_eq!(error_label("note: lower case is prose"), None);
    assert_eq!(error_label("A:)"), None);
    assert_eq!(error_label(""), None);
}

#[test]
fn long_tool_results_wrap_under_their_prefix() {
    let summary = "Blocked: a foreground `sleep 20` only uses up time. Run it in the background \
                       instead, and check on it while you get on with other work.";
    let s = strip(&tool_result_text(summary, false, 0, 60));
    assert!(s.lines().count() > 2, "the long line wraps: {s}");
    assert!(s.lines().all(|l| l.chars().count() <= 60), "{s}");
    assert!(
        s.lines().skip(1).all(|l| l.starts_with(&" ".repeat(5))),
        "continuation lines line up under the text: {s}"
    );
}

#[test]
fn every_tool_output_row_recedes_behind_its_command() {
    // The command above is highlighted; its whole body carries one style —
    // not just the first line. Each row must have the same escape envelope
    // (crossterm drops the colors themselves under NO_COLOR, so compare the
    // shape, not the exact sequences).
    let body = "line one\nline two\nline three";
    for is_error in [false, true] {
        let out = tool_result_text(body, is_error, 0, 80);
        let rows: Vec<&str> = out.lines().collect();
        assert_eq!(rows.len(), 3, "{out:?}");
        let esc = |l: &str| l.matches('\x1b').count();
        let n = esc(rows[0]);
        assert!(n >= 2, "the first row is styled: {out:?}");
        assert!(
            rows.iter().all(|l| esc(l) == n),
            "every row is styled alike (is_error={is_error}): {out:?}"
        );
    }
}

#[test]
fn markdown_links_become_hyperlinks() {
    // Split across stream tokens, the way a model's output arrives.
    let mut m = MarkdownStream::new();
    let mut o = String::new();
    for t in ["see [the ", "docs](htt", "ps://docs.rs/tok", "io) now"] {
        o.push_str(&m.push(t));
    }
    o.push_str(&m.finish());
    assert!(o.contains("\x1b]8;;https://docs.rs/tokio\x1b\\"), "{o:?}");
    assert!(!o.contains("](http"), "raw syntax should be gone: {o:?}");
    assert!(strip(&o).contains("see the docs now"), "{o:?}");
    // A relative destination isn't clickable; keep it readable.
    assert!(strip(&render_markdown("[main](src/main.rs)")).contains("main (src/main.rs)"));
}

#[test]
fn brackets_that_are_not_links_stay_literal() {
    let o = render_markdown("a [bracket] and `[x](y)` and **[b]** and dangling [abc");
    assert!(!o.contains("\x1b]8;;"), "nothing should be linked: {o:?}");
    assert!(
        strip(&o).contains("a [bracket] and [x](y) and [b] and dangling [abc"),
        "{:?}",
        strip(&o)
    );
    assert!(strip(&render_markdown("- [ ] todo\n- [x] done\n")).contains("• [ ] todo\n• [x] done"));
}

#[test]
fn bare_urls_are_linkified() {
    let s = linkify("see https://example.com/a. done");
    assert!(
        s.contains("\x1b]8;;https://example.com/a\x1b\\https://example.com/a\x1b]8;;\x1b\\."),
        "{s:?}"
    );
    assert_eq!(linkify("no url here"), "no url here");
    // An unbalanced trailing ) is sentence punctuation, not part of the URL.
    assert!(linkify("(see https://example.com/x)").contains("\x1b]8;;https://example.com/x\x1b\\"));
    // A ) the URL opened itself belongs to it.
    let wiki = linkify("https://en.wikipedia.org/wiki/Rust_(programming_language) baz");
    assert!(
        wiki.contains("\x1b]8;;https://en.wikipedia.org/wiki/Rust_(programming_language)\x1b\\"),
        "{wiki:?}"
    );
}

#[test]
fn markdown_stream_handles_split_tokens() {
    let mut m = MarkdownStream::new();
    let mut o = String::new();
    for t in [
        "# Ti",
        "tle\nsome **bo",
        "ld** and `co",
        "de`\n- item\n```rust\nfn x() {}\n```\n",
    ] {
        o.push_str(&m.push(t));
    }
    o.push_str(&m.finish());
    let plain = strip(&o);
    assert!(plain.contains("Title\n"));
    assert!(plain.contains("some bold and code\n"));
    assert!(plain.contains("• item"));
    assert!(plain.contains("fn x() {}"));
}
