use super::*;

fn text(l: &Line) -> String {
    l.spans.iter().map(|s| s.content.as_ref()).collect()
}

#[test]
fn wraps_words() {
    let l = Line::raw("the quick brown fox jumps over the lazy dog");
    let w = wrap(&l, 15, 2);
    assert!(w.len() >= 3);
    assert!(w.iter().all(|x| x.width() <= 15));
    assert!(text(&w[1]).starts_with("  "));
}

#[test]
fn plain_summaries_style_their_code_ticks() {
    let line = &code_ticks_lines("Blocked: a foreground `sleep 20` only uses up time.")[0];
    assert_eq!(
        text(line),
        "Blocked: a foreground sleep 20 only uses up time."
    );
    let code = line
        .spans
        .iter()
        .find(|s| s.content == "sleep 20")
        .expect("the code run is its own span");
    assert_eq!(code.style.fg, Some(pal().code));

    // A tick with no partner is text, not a code span.
    let line = &code_ticks_lines("a lone ` tick")[0];
    assert_eq!(text(line), "a lone ` tick");
    assert!(line.spans.iter().all(|s| s.style.fg != Some(pal().code)));

    // Each line is parsed on its own, so a tick cannot pair across lines.
    let lines = code_ticks_lines("a `b\nc` d");
    assert_eq!(text(&lines[0]), "a `b");
    assert_eq!(text(&lines[1]), "c` d");
}

#[test]
fn renders_lists_and_code() {
    let md = render_markdown("# Title\n\n- one\n- two `x`\n\n```rust\nfn a() {}\n```\n");
    let all: Vec<String> = md.iter().map(|l| text(&l.line)).collect();
    assert!(all.iter().any(|l| l.contains("Title")));
    assert!(all.iter().any(|l| l.contains("• one")));
    assert!(all.iter().any(|l| l.contains("fn a() {}")));
}

#[test]
fn reads_the_terminal_background_reply() {
    let dark = parse_osc11("\x1b]11;rgb:1e1e/1e1e/1e1e\x07").unwrap();
    assert!(luminance(dark) < 0.5);
    let light = parse_osc11("\x1b]11;rgb:ffff/ffff/ffff\x1b\\").unwrap();
    assert!(luminance(light) > 0.5);
    assert!(
        luminance(parse_osc11("\x1b]11;rgb:f/f/f\x07").unwrap()) > 0.9,
        "one hex digit per channel"
    );
    assert!(parse_osc11("no reply").is_none() && parse_osc11("\x1b]11;rgb:zz/00/00\x07").is_none());
}

#[test]
fn without_an_answer_the_background_is_dark() {
    // No OSC 11 reply and no COLORFGBG (tmux, ssh): dark, whatever the OS says.
    assert!(!light_from_colorfgbg(None));
    assert!(!light_from_colorfgbg(Some("")));
    assert!(!light_from_colorfgbg(Some("garbage")));
    assert!(!light_from_colorfgbg(Some("15;0")));
    assert!(!light_from_colorfgbg(Some("7;default;8")));
    assert!(light_from_colorfgbg(Some("0;15")));
    assert!(light_from_colorfgbg(Some("0;default;7")));
    assert!(light_from_colorfgbg(Some("0;11")));
}

#[test]
fn plain_code_uses_the_terminal_foreground() {
    let plain = syntect::highlighting::Color {
        r: 192,
        g: 197,
        b: 206,
        a: 255,
    };
    assert_eq!(syntect_fg(plain, Some(plain)), Color::Reset);
    let kw = syntect::highlighting::Color {
        r: 180,
        g: 142,
        b: 173,
        a: 255,
    };
    assert_eq!(syntect_fg(kw, Some(plain)), Color::Rgb(180, 142, 173));
    // The real theme: untokenised text comes out in the terminal's colour.
    let lines = highlight_block("let x = 1;", "rust");
    assert!(
        lines[0].iter().any(|s| s.style.fg == Some(Color::Reset)),
        "{lines:?}"
    );
}

#[test]
fn headings_have_a_hierarchy() {
    let md = render_markdown("# One\n\n## Two\n\n### Three\n");
    let style = |i: usize| {
        md[i]
            .line
            .spans
            .iter()
            .find(|s| !s.content.trim().is_empty())
            .unwrap()
            .style
    };
    let accent = pal().accent;
    assert_eq!(style(0).fg, Some(accent));
    assert_eq!(style(2).fg, Some(accent));
    assert_ne!(style(4).fg, Some(accent));
    assert!(!style(4).add_modifier.contains(Modifier::ITALIC));
}

#[test]
fn keys_typed_around_the_background_reply_are_kept() {
    let (reply, typed) = split_osc11(b"fix the\x1b]11;rgb:0/0/0\x07 bug");
    assert_eq!(reply.as_deref(), Some("\x1b]11;rgb:0/0/0\x07"));
    assert_eq!(typed, b"fix the bug");
    let (reply, typed) = split_osc11(b"ab\x1b]11;rgb:f/f/f\x1b\\c");
    assert!(reply.is_some());
    assert_eq!(typed, b"abc");
    // No reply (yet): it's all typing, except a partial reply.
    assert_eq!(split_osc11(b"hello"), (None, b"hello".to_vec()));
    assert_eq!(split_osc11(b"hi\x1b]11;rgb:0"), (None, b"hi".to_vec()));
}

#[test]
fn keeps_single_line_breaks() {
    let md = render_markdown("alpha: 1\nbeta: 2\ngamma: 3");
    let all: Vec<String> = md.iter().map(|l| text(&l.line)).collect();
    assert_eq!(all, ["alpha: 1", "beta: 2", "gamma: 3"]);
}

#[test]
fn aligns_table_columns() {
    let md = render_markdown(
        "| | sessions | cost |\n|---|---|---|\n| This project, last 7 days | 3 | $0.00 |\n| All | 12 | $1.50 |\n",
    );
    let all: Vec<String> = md
        .iter()
        .map(|l| text(&l.line))
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(all[0], "                          │ sessions │ cost");
    assert_eq!(all[1], "──────────────────────────┼──────────┼──────");
    assert_eq!(all[2], "This project, last 7 days │ 3        │ $0.00");
    assert_eq!(all[3], "All                       │ 12       │ $1.50");
}

#[test]
fn parses_ansi() {
    let l = ansi_lines("\x1b[1mbold\x1b[0m plain\nnext");
    assert_eq!(l.len(), 2);
    assert!(l[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
}

fn is_link(s: &Span) -> bool {
    s.style.fg == Some(pal().link) && s.style.add_modifier.contains(Modifier::UNDERLINED)
}

#[test]
fn bare_urls_read_as_links() {
    let md = render_markdown("open https://example.com/a?b=1, then continue");
    let spans = &md[0].line.spans;
    let url = spans
        .iter()
        .find(|s| s.content.contains("example.com"))
        .unwrap();
    assert!(is_link(url), "{url:?}");
    // Sentence punctuation trailing the URL is not part of the link.
    assert_eq!(url.content.as_ref(), "https://example.com/a?b=1");
    assert!(
        spans
            .iter()
            .any(|s| s.content.starts_with(',') && !is_link(s))
    );
}

#[test]
fn a_paren_the_url_did_not_open_stays_outside_it() {
    let md = render_markdown("(see https://example.com/x)");
    let url = md[0]
        .line
        .spans
        .iter()
        .find(|s| s.content.starts_with("https://"))
        .unwrap();
    assert!(is_link(url));
    assert_eq!(url.content.as_ref(), "https://example.com/x");
}

#[test]
fn a_plain_http_word_is_not_a_link() {
    let md = render_markdown("the http protocol, not a scheme");
    assert!(md[0].line.spans.iter().all(|s| !is_link(s)));
}

#[test]
fn markdown_link_text_is_not_styled_twice() {
    // A bare URL written as Markdown link text must keep the Markdown link's
    // styling rather than gain a second, nested link span.
    let md = render_markdown("[https://example.com/x](https://example.com/x)");
    let spans = &md[0].line.spans;
    assert_eq!(spans.iter().filter(|s| is_link(s)).count(), 1, "{spans:?}");
    assert_eq!(text(&md[0].line), "https://example.com/x");
}

#[test]
fn long_urls_break_between_parameters() {
    let url = "https://dash.cloudflare.com/oauth2/auth?response_type=code&client_id=abcdef123&redirect_uri=http%3A%2F%2Flocalhost%3A8762%2Foauth%2Fcallback&scope=k2.read%20k2.write";
    let w = wrap(&Line::raw(url), 40, 0);
    assert!(w.len() > 3);
    assert!(
        w.iter().all(|l| l.width() <= 40),
        "{:?}",
        w.iter().map(text).collect::<Vec<_>>()
    );
    assert_eq!(w.iter().map(text).collect::<String>(), url);
    // Cuts land before a `&`, so continuation lines start with the key.
    assert!(
        w.iter().skip(1).any(|l| text(l).starts_with('&')),
        "{:?}",
        w.iter().map(text).collect::<Vec<_>>()
    );
}

#[test]
fn a_wrap_never_splits_a_percent_escape() {
    let url = "https://example.com/cb?redirect_uri=http%3A%2F%2Flocalhost%3A8762%2Foauth%2Fcallback&scope=read";
    for width in 12..60 {
        let w = wrap(&Line::raw(url), width, 0);
        let mut at = 0;
        for l in &w {
            at += l.width();
            let head = &url[..at];
            if let Some(i) = head.rfind('%') {
                let after = &head[i + 1..];
                assert!(
                    !(after.len() < 2 && after.chars().all(|c| c.is_ascii_hexdigit())),
                    "width {width} splits %{after:?} at {at}"
                );
            }
        }
    }
}

#[test]
fn a_short_url_is_left_whole() {
    let l = Line::raw("see https://example.com/x ok");
    let w = wrap(&l, 60, 0);
    assert_eq!(w.len(), 1);
}

#[test]
fn link_runs_find_a_bare_url() {
    let md = render_markdown("open https://example.com/a?b=1, then");
    let runs = link_runs(&md[0].line);
    assert_eq!(runs.len(), 1, "{:?}", runs.len());
    assert_eq!(runs[0].text, "https://example.com/a?b=1");
    // "open " is five columns wide.
    assert_eq!((runs[0].start, runs[0].end), (5, 30));
    assert!(runs[0].dest.is_none());
}

#[test]
fn link_runs_give_a_label_its_destination() {
    let md = render_markdown("see [docs](https://example.com/x) now");
    let runs = link_runs(&md[0].line);
    let label = runs.iter().find(|r| r.text == "docs").unwrap();
    assert_eq!(label.dest.as_deref(), Some("https://example.com/x"));
    // The dim URL shown after the label is clickable on its own.
    let shown: Vec<_> = runs
        .iter()
        .filter(|r| r.text == "https://example.com/x")
        .collect();
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].dest.as_deref(), Some("https://example.com/x"));
}

#[test]
fn a_wrapped_url_arrives_as_link_fragments() {
    let url = "https://example.com/a?b=1&c=2&d=3&e=4";
    let md = render_markdown(url);
    let wrapped = wrap(&md[0].line, 20, 0);
    assert!(wrapped.len() > 1);
    let text: String = wrapped
        .iter()
        .map(|l| link_runs(l).into_iter().map(|r| r.text).collect::<String>())
        .collect();
    assert_eq!(text, url);
}

fn rendered(md: &str) -> Vec<String> {
    render_markdown(md).iter().map(|l| text(&l.line)).collect()
}

#[test]
fn nested_lists_alternate_bullets_and_number_items() {
    let lines = rendered("- outer\n  - inner\n    - deeper\n\n1. first\n2. second\n");
    assert!(lines.iter().any(|l| l == "• outer"), "{lines:?}");
    assert!(lines.iter().any(|l| l == "  ◦ inner"), "{lines:?}");
    assert!(lines.iter().any(|l| l == "    • deeper"), "{lines:?}");
    assert!(lines.iter().any(|l| l == "1. first"), "{lines:?}");
    assert!(lines.iter().any(|l| l == "2. second"), "{lines:?}");
    let md = render_markdown("3. third\n");
    assert_eq!(text(&md[0].line), "3. third");
    assert_eq!(md[0].indent, 3);
}

#[test]
fn task_lists_show_boxes() {
    let lines = rendered("- [x] done\n- [ ] todo\n");
    assert!(lines.iter().any(|l| l.contains("☒ done")), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("☐ todo")), "{lines:?}");
}

#[test]
fn blockquotes_are_prefixed_and_dim_italic() {
    let md = render_markdown("> quoted *text*\n\nafter");
    let q = &md[0];
    assert_eq!(text(&q.line), "│ quoted text");
    assert_eq!(q.indent, 2);
    let span = q.line.spans.iter().find(|s| s.content == "text").unwrap();
    assert!(span.style.add_modifier.contains(Modifier::ITALIC));
    assert_eq!(text(&md.last().unwrap().line), "after");
}

#[test]
fn inline_styles_and_rules() {
    let md = render_markdown("**bold** _it_ ~~gone~~\n\n---\n\nend");
    let first = &md[0].line;
    let find = |t: &str| first.spans.iter().find(|s| s.content == t).unwrap().style;
    assert!(find("bold").add_modifier.contains(Modifier::BOLD));
    assert!(find("it").add_modifier.contains(Modifier::ITALIC));
    assert!(find("gone").add_modifier.contains(Modifier::CROSSED_OUT));
    assert!(md.iter().any(|l| text(&l.line) == "─".repeat(40)));
}

#[test]
fn links_show_their_destination_unless_already_visible() {
    let lines = rendered("[docs](https://example.com/docs) and [top](#top)");
    assert_eq!(lines[0], "docs (https://example.com/docs) and top");
    let lines = rendered("[https://example.com](https://example.com)");
    assert_eq!(lines[0], "https://example.com");
}

#[test]
fn html_and_hard_breaks_are_kept() {
    let lines = rendered("a <b>tag</b>  \nnext line");
    assert_eq!(lines[0], "a <b>tag</b>");
    assert_eq!(lines[1], "next line");
}

#[test]
fn tables_with_code_and_ragged_rows() {
    let md = render_markdown("| a | `b` |\n|---|---|\n| long cell | x |\n| y |\n");
    let lines: Vec<String> = md.iter().map(|l| text(&l.line)).collect();
    assert_eq!(lines[0], "a         │ b");
    assert!(lines[1].contains("┼"));
    assert_eq!(lines[2], "long cell │ x");
    assert_eq!(lines[3], "y         │");
    assert!(md[0].line.style.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn code_blocks_cover_each_highlighter_path() {
    for (lang, code) in [
        ("python", "def f():\n    return 1  # c"),
        ("ts", "const x = 1;"),
        ("sh", "echo hi"),
        ("rs", "fn main() {}"),
        ("yml", "a: 1"),
        ("jsonc", "{\"a\": 1}"),
        ("golang", "package main"),
        ("diff", "+add\n-del\n@@ hunk\n ctx"),
        ("", "let s = \"q\\\"x\"; // tail"),
        ("nosuchlang", "fn x() { 42 }\n// comment"),
        ("lua", "-- comment\nlocal x = 'a'"),
    ] {
        let md = render_markdown(&format!("```{lang}\n{code}\n```\n"));
        let all: Vec<String> = md.iter().map(|l| text(&l.line)).collect();
        for want in code.lines() {
            let want = format!("  {want}");
            assert!(
                all.iter().any(|l| l.trim_end() == want.trim_end()),
                "{lang}: {all:?}"
            );
        }
        assert!(md.iter().all(|l| l.indent == 2));
    }
    let indented = rendered("para\n\n    indented code\n");
    assert!(indented.iter().any(|l| l == "  indented code"));
}

#[test]
fn light_code_line_highlighting() {
    let spans = highlight_code_line("let x = 42 \"s\" 'c' `t`", "rust");
    let style = |t: &str| spans.iter().find(|s| s.content == t).unwrap().style;
    assert_eq!(style("let").fg, Some(Color::Rgb(200, 140, 220)));
    assert_eq!(style("42").fg, Some(Color::Rgb(240, 160, 110)));
    assert_eq!(style("\"s\"").fg, Some(Color::Rgb(150, 200, 120)));
    assert_eq!(style("x").fg, Some(pal().code_block));
    let c = highlight_code_line("  # note", "py");
    assert!(c[0].style.add_modifier.contains(Modifier::ITALIC));
    let d = highlight_code_line("+x", "diff");
    assert_eq!(d[0].style.fg, Some(pal().diff_add));
    assert_eq!(
        highlight_code_line("@@ x", "patch")[0].style.fg,
        Some(cyan())
    );
}

#[test]
fn syntect_plain_colour_is_reset() {
    let c = syntect::highlighting::Color {
        r: 1,
        g: 2,
        b: 3,
        a: 255,
    };
    assert_eq!(syntect_fg(c, Some(c)), Color::Reset);
    assert_eq!(syntect_fg(c, None), Color::Rgb(1, 2, 3));
}

#[test]
fn sgr_codes_set_and_clear_styles() {
    let s = apply_sgr(Style::default(), "1;2;3;4");
    assert!(
        s.add_modifier
            .contains(Modifier::BOLD | Modifier::DIM | Modifier::ITALIC | Modifier::UNDERLINED)
    );
    let s = apply_sgr(s, "22;23");
    assert!(!s.add_modifier.contains(Modifier::BOLD));
    assert!(!s.add_modifier.contains(Modifier::ITALIC));
    assert_eq!(apply_sgr(Style::default(), "0"), Style::default());
    if !crate::tui::theme::no_color() {
        assert_eq!(
            apply_sgr(Style::default(), "31").fg,
            Some(Color::Indexed(1))
        );
        assert_eq!(
            apply_sgr(Style::default(), "92").fg,
            Some(Color::Indexed(10))
        );
        assert_eq!(
            apply_sgr(Style::default(), "38;5;200").fg,
            Some(Color::Indexed(200))
        );
        assert_eq!(
            apply_sgr(Style::default(), "38;2;1;2;3").fg,
            Some(Color::Rgb(1, 2, 3))
        );
        assert!(apply_sgr(Style::default(), "48;5;1").bg.is_some());
        assert!(apply_sgr(Style::default(), "41").bg.is_some());
        assert_eq!(apply_sgr(apply_sgr(Style::default(), "31"), "39").fg, None);
        assert_eq!(apply_sgr(apply_sgr(Style::default(), "41"), "49").bg, None);
    }
    // Unknown and malformed codes are ignored.
    assert_eq!(apply_sgr(Style::default(), "38;9;x;77"), Style::default());
}

#[test]
fn ansi_lines_expand_tabs_and_skip_non_sgr_escapes() {
    let l = ansi_lines("a\tb\r\x1b[2Kc");
    assert_eq!(text(&l[0]), "a    bc");
}

#[test]
fn wrap_handles_long_words_and_wide_glyphs() {
    let l = Line::raw("short averyveryveryverylongwordthatdoesnotfit end");
    let w = wrap(&l, 10, 2);
    assert!(w.iter().all(|x| x.width() <= 10), "{w:?}");
    let joined: String = w.iter().map(|x| text(x).trim().to_string()).collect();
    assert_eq!(joined, "shortaveryveryveryverylongwordthatdoesnotfitend");
    // Wide CJK glyphs never overflow.
    let w = wrap(&Line::raw("漢字漢字漢字漢字漢字漢字"), 8, 0);
    assert!(w.iter().all(|x| x.width() <= 8));
    // An indent too large for the width is dropped.
    let w = wrap(&Line::raw("aaaa bbbb cccc dddd"), 8, 6);
    assert!(text(&w[1]).starts_with('b'));
    // Styles survive wrapping as separate spans.
    let l = Line::from(vec![
        Span::styled("red words here ", Style::default().fg(Color::Red)),
        Span::raw("plain words"),
    ]);
    let w = wrap(&l, 10, 0);
    assert!(
        w.iter()
            .flat_map(|x| x.spans.iter())
            .any(|s| s.style.fg == Some(Color::Red))
    );
}

#[test]
fn colour_shorthands_follow_the_palette() {
    assert_eq!(warn(), pal().warning);
    assert_eq!(amber(), pal().auto_mode);
    assert_eq!(panel_bg(), pal().panel_bg);
    assert_eq!(highlight_bg(), pal().selection_bg);
    assert_eq!(highlight_fg(), pal().selection_fg);
    assert_eq!(faint(), pal().faint);
    let _ = take_typeahead();
}
