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

#[test]
fn fuzzy_edit_keeps_the_first_lines_indentation() {
    // old_string drops the first line's indent; the file has trailing spaces.
    let text = "def f():\n    if x:  \n        return 1\n    return 2\n";
    let (out, _) = apply_edit(
        text,
        "if x:\n        return 1",
        "if y:\n        return 1",
        false,
    )
    .unwrap();
    assert_eq!(out, "def f():\n    if y:\n        return 1\n    return 2\n");
    // A line added deeper keeps its place relative to the others.
    let (out, _) = apply_edit(
        text,
        "if x:\n        return 1",
        "if y:\n        log()\n            more()\n        return 1",
        false,
    )
    .unwrap();
    assert_eq!(
        out,
        "def f():\n    if y:\n        log()\n            more()\n        return 1\n    return 2\n"
    );
}

#[test]
fn fuzzy_edit_adds_no_blank_line() {
    let (out, _) = apply_edit("x\na  \nb\ny\n", "a\nb\n", "A\nB\n", false).unwrap();
    assert_eq!(out, "x\nA\nB\ny\n");
}

#[test]
fn fuzzy_edit_keeps_tabs_and_crlf() {
    let text = "x\r\n\ta\r\n\tb\r\ny\r\n";
    let (out, _) = apply_edit(text, "    a\n    b", "    A\n    B", false).unwrap();
    assert_eq!(out, "x\r\n\tA\r\n\tB\r\ny\r\n");
}

#[test]
fn fuzzy_edit_in_the_files_own_indentation_is_kept() {
    let text = "fn main() {\n    let x = 1;  \n    let y = 2;\n}\n";
    let (out, _) = apply_edit(
        text,
        "let x = 1;\nlet y = 2;",
        "    let x = 1;\n    if y {\n        z();\n    }",
        false,
    )
    .unwrap();
    assert_eq!(
        out,
        "fn main() {\n    let x = 1;\n    if y {\n        z();\n    }\n}\n"
    );
}

#[test]
fn fuzzy_edit_refuses_indentation_it_cant_place() {
    // old_string's lines are all unindented but sit at two different file
    // indents, and new_string's unindented line could belong to either.
    let text = "a:\n    b\nc\n";
    let err = apply_edit(text, "a:\nb\nc", "a:\nB\nc\n  odd", false).unwrap_err();
    assert!(
        err.contains("Retry with old_string copied exactly"),
        "{err}"
    );
    assert!(err.contains("    b"), "{err}");
}

#[test]
fn fuzzy_edit_in_a_tab_file_indents_new_lines_with_tabs() {
    // A deeper line added in spaces lands in tabs, and the lines it kept
    // stay as they were.
    let text = "x\n\tif a:\n\t\tb()\ny\n";
    let (out, _) = apply_edit(
        text,
        "    if a:\n        b()",
        "    if a:\n        b()\n            c()",
        false,
    )
    .unwrap();
    assert_eq!(out, "x\n\tif a:\n\t\tb()\n\t\t\tc()\ny\n");
}

#[test]
fn fuzzy_delete_of_whole_lines_leaves_no_empty_line() {
    let (out, _) = apply_edit("x\n    a  \n    b\ny\n", "a\nb\n", "", false).unwrap();
    assert_eq!(out, "x\ny\n");
    let (out, _) = apply_edit("x\r\n  a \r\n  b\r\ny\r\n", "a\nb\n", "", false).unwrap();
    assert_eq!(out, "x\r\ny\r\n");
    // Without a trailing newline in old_string, only the text goes.
    let (out, _) = apply_edit("x\n    a  \n    b\ny\n", "a\nb", "", false).unwrap();
    assert_eq!(out, "x\n\ny\n");
}

#[test]
fn fuzzy_edit_at_the_end_of_a_file_without_a_newline() {
    let (out, _) = apply_edit("x\n    a  \n    b", "a\nb", "A\nB", false).unwrap();
    assert_eq!(out, "x\n    A\n    B");
}
