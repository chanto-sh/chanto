use super::*;
use crate::fs::MemFs;

/// The old whole-file implementation, as the reference.
fn reference(text: &str, offset: usize, limit: usize) -> (String, usize, usize) {
    let total = text.lines().count();
    let mut out = String::new();
    let mut shown = 0;
    for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
        if out.len() > MAX_READ_BYTES {
            out.push_str(&format!(
                "… [output limit reached at line {}; use offset/limit to read more]\n",
                i + 1
            ));
            break;
        }
        let line = if line.len() > MAX_LINE {
            truncated_line(line, line.len())
        } else {
            line.into()
        };
        out.push_str(&format!("{}\t{}\n", i + 1, line));
        shown += 1;
    }
    (out, shown, total)
}

/// The window of `text`, streamed in from memory.
fn window(text: &[u8], offset: usize, limit: usize) -> Window {
    read_window(std::io::Cursor::new(text), text.len() as u64, offset, limit).unwrap()
}

#[test]
fn streaming_read_matches_whole_file_read() {
    let long = "é".repeat(MAX_LINE); // multi-byte, longer than MAX_LINE bytes
    let big: String = (1..=200_000).map(|i| format!("line {i}\n")).collect();
    let cases: Vec<(&str, String)> = vec![
        ("plain", "a\nb\nc\n".into()),
        ("no-final-newline", "a\nb".into()),
        ("crlf", "a\r\nb\r\n\r\nc".into()),
        ("blank-lines", "\n\n\nx\n".into()),
        ("long-lines", format!("{long}\nshort\n{long}")),
        ("long-crlf-lines", format!("{long}\r\nshort\r\n{long}\r\n")),
        ("big", big),
    ];
    for (name, text) in &cases {
        for (offset, limit) in [(1, 2000), (2, 1), (3, 5), (150_000, 3), (199_999, 10)] {
            let w = window(text.as_bytes(), offset, limit);
            let (out, shown, total) = reference(text, offset, limit);
            assert_eq!(
                (w.out.as_str(), w.shown, w.total),
                (out.as_str(), shown, total),
                "{name} offset {offset} limit {limit}"
            );
        }
    }
    assert!(window(b"abc\0def", 1, 10).binary);
}

#[test]
fn long_lines_say_they_were_cut() {
    // Longer than one 64 KiB read chunk, so the dropped bytes span chunks.
    let w = window(format!("{}\r\nok\n", "x".repeat(100_000)).as_bytes(), 1, 10);
    let first = w.out.lines().next().unwrap();
    assert!(
        first.ends_with("… [line truncated: 2000 of 100000 bytes]"),
        "{}",
        &first[first.len() - 60..]
    );
    assert!(w.out.contains("2\tok\n"));
}

#[test]
fn a_typo_suggests_close_file_names_and_directories() {
    let fs = MemFs::new();
    fs.mkdir_p("/p/src/agents");
    fs.insert("/p/src/agent.rs", "x");
    let hint = similar_files(&fs, Path::new("/p/src/agnet.rs"));
    assert!(hint.contains("agent.rs"), "{hint}");
    let hint = similar_files(&fs, Path::new("/p/src/agentss/mod.rs"));
    assert!(hint.contains("agents/"), "{hint}");
    assert!(similar_files(&fs, Path::new("/p/src/zzzzzzz.rs")).is_empty());
}

#[test]
fn notebooks_show_cells_and_text_output() {
    let nb = br##"{"cells":[{"cell_type":"code","source":["x = 1\n","x"],"outputs":[{"text":["1\n"]}]},{"cell_type":"markdown","source":"# Title"}]}"##;
    assert_eq!(
        render_notebook(nb),
        "<cell 0 type=code>\nx = 1\nx\n<output>\n1\n\n</output>\n</cell>\n<cell 1 type=markdown>\n# Title\n</cell>\n"
    );
}

#[test]
fn text_that_isnt_utf8_names_its_likely_encoding() {
    let p = Path::new("/p/a.txt");
    assert!(not_utf8(p, b"caf\xe9").contains("Latin-1"));
    assert!(not_utf8(p, &[0xFF, 0xFE, b'h', 0]).contains("UTF-16 LE"));
    assert!(not_utf8(p, b"a\0b").contains("NUL"));
}
