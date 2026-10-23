use super::*;

fn text(lines: &[Line<'static>]) -> String {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_streaming_answer_renders_markdown_and_matches_the_final_layout() {
    let b = Block::new(Kind::Assistant("**bold** and `code` here".into()));
    let live = text(&render_block(&b, 80, false, true));
    assert!(live.starts_with("❖ "), "first line gets the bullet: {live}");
    assert!(
        live.contains("bold") && !live.contains("**") && !live.contains('`'),
        "Markdown is rendered while streaming: {live}"
    );
    // A finished reply looks the same streamed as closed, so nothing jumps.
    let doc = "# Title\n\nA paragraph with **bold**,\nwrapped over two source lines.\n\n- one\n- two\n\n```rust\nfn main() {\n\n    println!(\"x\");\n}\n```\nAfter the code.\n\n1. first\n2. second\n\n> quoted\n\nThe end.\n";
    let b = Block::new(Kind::Assistant(doc.into()));
    for w in [30, 80] {
        assert_eq!(
            text(&render_block(&b, w, false, true)),
            text(&render_block(&b, w, false, false)),
            "width {w}"
        );
    }
    // An unclosed fence shows its code as plain lines, indented like the final render.
    let b = Block::new(Kind::Assistant("Intro\n\n```rust\nlet x = 1;\n".into()));
    let live = text(&render_block(&b, 80, false, true));
    assert!(
        live.contains("    let x = 1;") && !live.contains("```"),
        "{live}"
    );
}

#[test]
fn reference_links_stream_as_they_render() {
    let doc = "Intro.\n\nSee [the docs][d] for more.\n\nMiddle paragraph.\n\n[d]: https://example.com\n\nEnd.\n";
    let b = Block::new(Kind::Assistant(doc.into()));
    assert_eq!(
        text(&render_block(&b, 60, false, true)),
        text(&render_block(&b, 60, false, false))
    );
    // Nothing after the reference is split off.
    assert_eq!(next_md_boundary(doc, doc.find("See").unwrap()), None);
}

#[test]
fn markdown_boundaries_skip_fences_and_continuations() {
    let t = "para\n\n```\na\n\nb\n```\nnext\n\n- item\n\n  more\n\nend\n";
    let mut cuts = vec![];
    let mut at = 0;
    while let Some(b) = next_md_boundary(t, at) {
        cuts.push(&t[b..b + 4]);
        at = b;
    }
    // Not inside the fence, not before the list item or its indented continuation.
    assert_eq!(cuts, vec!["```\n", "next", "end\n"]);
}

#[test]
fn visible_rows_match_the_old_prepend_algorithm() {
    // The old way: concatenate every measured row, then slice.
    fn old(spans: &[(usize, usize)], h: usize, scroll: &mut usize) -> Vec<(usize, usize)> {
        let all: Vec<(usize, usize)> = spans
            .iter()
            .flat_map(|&(i, n)| (0..n).map(move |k| (i, k)))
            .collect();
        let total = all.len();
        *scroll = (*scroll).min(total.saturating_sub(h));
        let start = total.saturating_sub(h + *scroll);
        all[start..(start + h).min(total)].to_vec()
    }
    let mut seed = 7u64;
    let mut rnd = |m: usize| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) as usize % m
    };
    for _ in 0..500 {
        let spans: Vec<(usize, usize)> = (0..rnd(12)).map(|i| (i, 1 + rnd(30))).collect();
        let h = 1 + rnd(40);
        let scroll = rnd(200);
        let (mut s1, mut s2) = (scroll, scroll);
        let (rows, _, _) = visible_rows(&spans, h, &mut s1);
        assert_eq!(
            rows,
            old(&spans, h, &mut s2),
            "spans {spans:?} h {h} scroll {scroll}"
        );
        assert_eq!(s1, s2);
    }
}

#[test]
fn welcome_shows_the_mark_and_session_info() {
    let b = Block {
        kind: Kind::Welcome {
            model: "deepseek-flash".into(),
            cwd: "~/code/app".into(),
        },
        expanded: false,
        version: 0,
        cache: None,
    };
    let wide = text(&render_block(&b, 80, false, false));
    assert!(wide.contains("❖") && wide.contains("chanto v") && wide.contains("deepseek-flash"));
    // Every row of the box has the same width.
    let widths: Vec<usize> = wide.lines().take(8).map(|l| l.chars().count()).collect();
    assert!(widths.windows(2).all(|w| w[0] == w[1]), "{widths:?}");
    let narrow = text(&render_block(&b, 40, false, false));
    assert!(!narrow.contains("▄█▀") && narrow.contains("chanto v"));
}
