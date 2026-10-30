use super::*;

#[test]
fn edit_unique_and_ambiguous() {
    assert_eq!(apply_edit("a b a", "b", "c", false).unwrap().0, "a c a");
    assert!(apply_edit("a b a", "a", "c", false).is_err());
    assert_eq!(apply_edit("a b a", "a", "c", true).unwrap().0, "c b c");
    assert!(apply_edit("abc", "x", "y", false).is_err());
}

#[test]
fn edit_fuzzy_indentation() {
    let text = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
    let (out, _) = apply_edit(text, "let x = 1;\nlet y = 2;", "    let z = 3;", false).unwrap();
    assert_eq!(out, "fn main() {\n    let z = 3;\n}\n");
}

#[test]
fn snippet_shows_change() {
    let s = changed_snippet("a\nb\nc\n", "a\nX\nc\n");
    assert!(s.contains("X"));
}

#[test]
fn changed_snippet_matches_whole_file_version() {
    let base: String = (0..300).map(|i| format!("line {i}\n")).collect();
    let cases = [
        base.replace("line 150\n", "line 150 edited\n"),
        base.replace("line 0\n", "start\n"),
        base.replace("line 299\n", "end\n"),
        base.replace("line 40\n", "line 40\nnew 1\nnew 2\n"),
        base.replace("line 80\nline 81\nline 82\n", ""),
        base.replace("line 5\n", "a\n").replace("line 250\n", "b\n"),
        base.trim_end_matches('\n').to_string(),
        base.clone(),
    ];
    for (i, new) in cases.iter().enumerate() {
        assert_eq!(
            changed_snippet(&base, new),
            changed_snippet_full(&base, new, 0),
            "case {i}"
        );
    }
}

#[test]
fn a_crlf_file_takes_an_lf_edit() {
    let (out, n) = apply_edit("a\r\nb\r\nc\r\n", "a\nb", "x\ny", false).unwrap();
    assert_eq!((out.as_str(), n), ("x\r\ny\r\nc\r\n", 1));
}

#[test]
fn changes_are_counted_by_line() {
    assert_eq!(count_changes("a\nb\nc\n", "a\nB\nc\nd\n"), (2, 1));
}
