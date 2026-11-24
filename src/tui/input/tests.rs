use super::*;

#[test]
fn editing_and_history() {
    let mut i = Input::default();
    i.insert("hello world");
    i.word_left();
    assert_eq!(&i.text[i.cursor..], "world");
    i.delete_word_back();
    assert_eq!(i.text, "world");
    i.set("first");
    i.take();
    i.insert("draft");
    assert!(i.up());
    assert_eq!(i.text, "first");
    assert!(i.down());
    assert_eq!(i.text, "draft");
}

#[test]
fn unicode_whitespace_is_safe() {
    let mut i = Input::default();
    i.insert("a\u{00A0}bé");
    let _ = i.current_word();
    i.word_left();
    i.word_right();
    i.delete_word_back();
    i.insert("\u{00A0}x");
    let (_, w) = i.current_word();
    assert_eq!(w, "x");
}

#[test]
fn multiline_navigation() {
    let mut i = Input::default();
    i.insert("abc\ndefgh");
    assert!(i.up());
    assert_eq!(i.cursor, 3);
    let (rows, cur) = i.layout(80);
    assert_eq!(rows.len(), 2);
    assert_eq!(cur, (0, 3));
}

#[test]
fn long_lines_wrap_at_a_word_boundary() {
    let mut i = Input::default();
    i.insert("hello world");
    // The space is dropped and "world" starts the next row.
    let (rows, _) = i.layout(8);
    assert_eq!(rows, vec!["hello".to_string(), "world".to_string()]);
    // The cursor follows the last row.
    let (_, cur) = i.layout(8);
    assert_eq!(cur, (1, 5));
    // A word too long for the row still breaks on the character.
    let mut j = Input::default();
    j.insert("abcdefgh");
    let (rows, _) = j.layout(5);
    assert_eq!(rows, vec!["abcde".to_string(), "fgh".to_string()]);
    // An explicit newline still wins over wrapping.
    let mut k = Input::default();
    k.insert("a\nb");
    let (rows, _) = k.layout(80);
    assert_eq!(rows, vec!["a".to_string(), "b".to_string()]);
}
