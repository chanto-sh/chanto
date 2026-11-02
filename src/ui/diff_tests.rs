/// The diff of the changed region must render exactly like a diff of the
/// whole file.
#[test]
fn region_diff_matches_full_diff() {
    fn full(old: &str, new: &str, max: usize) -> String {
        use similar::{ChangeTag, TextDiff};
        let d = TextDiff::from_lines(old, new);
        let mut v = Vec::new();
        for g in d.grouped_ops(2) {
            for op in g {
                for ch in d.iter_changes(&op) {
                    let ln = ch
                        .new_index()
                        .or(ch.old_index())
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    let t = match ch.tag() {
                        ChangeTag::Delete => '-',
                        ChangeTag::Insert => '+',
                        ChangeTag::Equal => ' ',
                    };
                    v.push(format!("{ln} {t}{}", ch.value().trim_end_matches('\n')));
                }
            }
        }
        v.truncate(max);
        v.join("\n")
    }
    fn mine(old: &str, new: &str, max: usize) -> String {
        use similar::{ChangeTag, TextDiff};
        let (off, o, n) = crate::util::changed_region(old, new, 2);
        let d = TextDiff::from_lines(o, n);
        let mut v = Vec::new();
        for g in d.grouped_ops(2) {
            for op in g {
                for ch in d.iter_changes(&op) {
                    let ln = ch
                        .new_index()
                        .or(ch.old_index())
                        .map(|i| i + 1 + off)
                        .unwrap_or(0);
                    let t = match ch.tag() {
                        ChangeTag::Delete => '-',
                        ChangeTag::Insert => '+',
                        ChangeTag::Equal => ' ',
                    };
                    v.push(format!("{ln} {t}{}", ch.value().trim_end_matches('\n')));
                }
            }
        }
        v.truncate(max);
        v.join("\n")
    }
    let base: String = (0..200).map(|i| format!("line {i}\n")).collect();
    let cases: Vec<String> = vec![
        base.replace("line 100\n", "line 100 changed\n"),
        base.replace("line 0\n", "first\n"),
        base.replace("line 199\n", "last\n"),
        base.replace("line 50\n", "line 50\ninserted a\ninserted b\n"),
        base.replace("line 70\nline 71\n", ""),
        base.trim_end_matches('\n').to_string(),
        format!("{base}tail without newline"),
        base.replace("line 10\n", "x\n")
            .replace("line 150\n", "y\n"),
        base.replace("line 1", "line one"),
        base.clone(),
        String::new(),
    ];
    for (i, new) in cases.iter().enumerate() {
        assert_eq!(mine(&base, new, 100), full(&base, new, 100), "case {i}");
        assert_eq!(
            mine(new, &base, 100),
            full(new, &base, 100),
            "reverse case {i}"
        );
    }
}
