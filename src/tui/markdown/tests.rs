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
