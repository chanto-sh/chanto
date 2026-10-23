use super::*;

fn sel(anchor: (u16, u16), head: (u16, u16)) -> Selection {
    Selection { anchor, head }
}

/// The rows a widget drew, as `row_text` would hold them: text starting at
/// column 1 from screen row 2, so column 2 is the second character.
fn rows(items: &[&str]) -> Vec<Option<(u16, String)>> {
    let mut v: Vec<Option<(u16, String)>> = vec![None; 2 + items.len()];
    for (i, s) in items.iter().enumerate() {
        v[2 + i] = Some((1, (*s).to_string()));
    }
    v
}

#[test]
fn reading_order_is_the_same_forwards_and_backwards() {
    assert_eq!(sel((2, 2), (3, 6)).ordered(), ((2, 2), (3, 6)));
    assert_eq!(sel((3, 6), (2, 2)).ordered(), ((2, 2), (3, 6)));
}

#[test]
fn dragging_backwards_selects_the_same_text() {
    let fwd = selection_text(sel((2, 2), (3, 6)), &rows(&["alpha", "bravo"]));
    let back = selection_text(sel((3, 6), (2, 2)), &rows(&["alpha", "bravo"]));
    assert_eq!(fwd, back);
    // Text starts at column 1, so column 2 is the second character.
    assert_eq!(fwd, "lpha\nbravo");
}

#[test]
fn a_single_row_selection_is_bounded_by_both_columns() {
    // The head cell is included, so a drag to column 4 takes "alph".
    assert_eq!(
        selection_text(sel((2, 1), (2, 4)), &rows(&["alpha"])),
        "alph"
    );
    // A press and a drag back to the same cell still selects that one glyph.
    assert_eq!(selection_text(sel((2, 4), (2, 4)), &rows(&["alpha"])), "h");
}

#[test]
fn rows_outside_the_selection_are_ignored() {
    assert_eq!(selected_cols(sel((2, 1), (2, 4)), 1, 0), None);
    assert_eq!(selected_cols(sel((2, 1), (2, 4)), 1, 99), None);
    // A row no widget drew contributes nothing.
    let mut r = rows(&["alpha", "bravo"]);
    r[3] = None;
    assert_eq!(selection_text(sel((2, 1), (3, 6)), &r), "alpha");
}

#[test]
fn multibyte_columns_never_split_a_character() {
    // "héllo": 'é' is two bytes, so a column offset must be mapped through
    // glyph width instead of used as a byte index.
    let line = "héllo".to_string();
    let out = selection_text(sel((2, 1), (2, 3)), &rows(&[&line]));
    assert!(line.contains(&out), "unexpected {out:?}");
    // Columns are cells: column 1 is the gutter, so text cells 0..=2 → "hél".
    assert_eq!(out, "hél");
    // The char-snapping helpers still guard byte ranges that land mid-glyph.
    assert_eq!(ceil_char("héllo", 2), 3);
    assert_eq!(floor_char("héllo", 1), 1);
    assert_eq!(floor_char("héllo", 2), 1);
}

#[test]
fn wide_glyphs_map_to_whole_cells() {
    // '你' is one glyph covering two cells: a drag over either of its cells
    // must copy the whole character, never half of it.
    let line = "a你b".to_string();
    // Text cell 1 is the second cell of the wide glyph, which must map to the
    // whole glyph rather than to a byte inside it.
    let (a, b) = cols_to_bytes(&line, 1, 2);
    assert_eq!(&line[a..b], "你");
    let (a, b) = cols_to_bytes(&line, 1, 4);
    assert_eq!(&line[a..b], "你b");
}

#[test]
fn a_column_past_the_end_selects_the_rest() {
    let line = "abc";
    assert_eq!(cols_to_bytes(line, 1, usize::MAX), (1, 3));
    // Past the end of a shorter row clamps instead of panicking.
    assert_eq!(cols_to_bytes(line, 9, 12), (3, 3));
}

#[test]
fn the_highlight_covers_the_selected_cells_of_each_row() {
    // Text starts at column 1: a drag from (2,2) to (3,4) highlights
    // "lpha…" on the first row and the first cells of the second.
    let rects = selection_rects(sel((2, 2), (3, 4)), &rows(&["alpha", "bravo"]));
    assert_eq!(
        rects,
        vec![
            Rect {
                x: 2,
                y: 2,
                width: 4,
                height: 1
            },
            Rect {
                x: 1,
                y: 3,
                width: 4,
                height: 1
            }
        ]
    );
    // The highlight never runs past the end of the text on the head row.
    assert!(
        selection_rects(sel((2, 1), (2, 99)), &rows(&["alpha"]))
            .iter()
            .all(|r| r.width <= 5)
    );
}

#[test]
fn the_highlight_never_covers_a_row_no_widget_drew() {
    let mut r = rows(&["alpha", "bravo"]);
    r[3] = None;
    let rects = selection_rects(sel((2, 1), (3, 99)), &r);
    assert_eq!(rects.len(), 1, "only the drawn row is highlighted");
}

#[test]
fn double_click_selects_the_word_under_the_cursor() {
    // Text starts at column 1, so "alpha" covers columns 5..=9.
    let r = rows(&["let alpha beta = 1"]);
    let w = word_at(&r, 2, 6).expect("a word under the cursor");
    assert_eq!(w, sel((2, 5), (2, 9)));
    assert_eq!(selection_text(w, &r), "alpha");
    // The next word, and the digits after it.
    assert_eq!(selection_text(word_at(&r, 2, 12).unwrap(), &r), "beta");
    assert_eq!(selection_text(word_at(&r, 2, 18).unwrap(), &r), "1");
}

#[test]
fn double_click_takes_the_punctuation_run_but_declines_blank_space() {
    let r = rows(&["a -> b"]);
    // '->' is its own run: neither side is a word character.
    assert_eq!(selection_text(word_at(&r, 2, 3).unwrap(), &r), "->");
    // A blank click has no run to grab: the caller keeps the plain cell
    // selection rather than selecting spaces that would copy as nothing.
    assert_eq!(word_at(&r, 2, 2), None);
}

#[test]
fn a_word_selection_snaps_to_whole_glyphs() {
    let line = "x 你好 y".to_string();
    let r = rows(&[&line]);
    // Both cells of the wide glyph select the whole two-character word.
    assert_eq!(selection_text(word_at(&r, 2, 3).unwrap(), &r), "你好");
    assert_eq!(selection_text(word_at(&r, 2, 4).unwrap(), &r), "你好");
}

#[test]
fn triple_click_selects_the_whole_row() {
    let r = rows(&["let alpha beta = 1"]);
    let w = line_at(&r, 2).expect("a drawn row");
    assert_eq!(w.anchor, (2, 1));
    assert_eq!(selection_text(w, &r), "let alpha beta = 1");
}

#[test]
fn releasing_a_double_click_keeps_the_whole_word() {
    let now = Instant::now();
    let click = Some((now, 4u16, 7u16));
    // A double or triple click released on the same cell keeps the word or
    // row picked on the press.
    assert!(release_keeps_pick(2, click, 4, 7));
    assert!(release_keeps_pick(3, click, 4, 7));
    // A plain click still takes the release cell, and so does a drag that
    // ends somewhere else.
    assert!(!release_keeps_pick(1, click, 4, 7));
    assert!(!release_keeps_pick(2, click, 4, 8));
    assert!(!release_keeps_pick(2, None, 4, 7));
}

#[test]
fn word_and_line_helpers_decline_blank_and_unrecorded_rows() {
    let r = rows(&["alpha"]);
    // No widget drew row 0.
    assert_eq!(word_at(&r, 0, 3), None);
    assert_eq!(line_at(&r, 0), None);
    // Past the end of a row there is no glyph, and an empty row has no line.
    assert_eq!(word_at(&r, 2, 99), None);
    assert_eq!(line_at(&rows(&[""]), 2), None);
}
