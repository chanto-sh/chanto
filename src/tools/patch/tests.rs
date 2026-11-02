use super::*;

#[test]
fn parse_and_apply() {
    let patch = "*** Begin Patch\n*** Update File: a.py\n@@ def f():\n     x = 1\n-    return x\n+    return x + 1\n*** Add File: b.txt\n+hello\n*** Delete File: c.txt\n*** End Patch";
    let ops = parse(patch).unwrap();
    assert_eq!(ops.len(), 3);
    let Op::Update { chunks, .. } = &ops[0] else {
        panic!()
    };
    let src = "def g():\n    return x\n\ndef f():\n    x = 1\n    return x\n";
    let out = apply_chunks(src, chunks, "a.py").unwrap();
    assert_eq!(
        out,
        "def g():\n    return x\n\ndef f():\n    x = 1\n    return x + 1\n"
    );
    assert_eq!(
        ops[1],
        Op::Add {
            path: "b.txt".into(),
            content: "hello\n".into()
        }
    );
    assert_eq!(paths(patch), vec!["a.py", "b.txt", "c.txt"]);
}

#[test]
fn crlf_files_stay_crlf() {
    let chunks = vec![Chunk {
        header: None,
        old: vec!["b".into()],
        new: vec!["B".into()],
        eof: false,
    }];
    assert_eq!(
        apply_chunks("a\r\nb\r\nc\r\n", &chunks, "w.txt").unwrap(),
        "a\r\nB\r\nc\r\n"
    );
    // LF files are untouched by the same rule.
    assert_eq!(
        apply_chunks("a\nb\nc\n", &chunks, "u.txt").unwrap(),
        "a\nB\nc\n"
    );
}

#[test]
fn whitespace_tolerant_and_errors() {
    let chunks = vec![Chunk {
        header: None,
        old: vec!["a  ".into(), "b".into()],
        new: vec!["a".into(), "c".into()],
        eof: false,
    }];
    assert_eq!(apply_chunks("a\nb\n", &chunks, "f").unwrap(), "a\nc\n");
    let add = vec![Chunk {
        header: None,
        old: vec![],
        new: vec!["z".into()],
        eof: false,
    }];
    assert_eq!(apply_chunks("a\nb\n", &add, "f").unwrap(), "a\nb\nz\n");
    let bad = vec![Chunk {
        header: None,
        old: vec!["zzz".into()],
        new: vec![],
        eof: false,
    }];
    assert!(apply_chunks("a\n", &bad, "f").is_err());
}

#[test]
fn shell_heredoc() {
    let cmd = "apply_patch <<'EOF'\n*** Begin Patch\n*** Add File: x\n+1\n*** End Patch\nEOF";
    assert_eq!(
        from_shell(cmd).unwrap(),
        "*** Begin Patch\n*** Add File: x\n+1\n*** End Patch"
    );
    assert!(from_shell("apply_patches foo").is_none());
}
