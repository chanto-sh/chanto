use super::*;
use crate::fs::MemFs;

fn tree_of(files: &[(&str, &str)]) -> MemFs {
    let fs = MemFs::new();
    for (p, text) in files {
        fs.insert(Path::new("/p").join(p), *text);
    }
    fs
}

fn visited(fs: &MemFs, max_depth: Option<usize>, budget: usize) -> Vec<String> {
    let mut seen = Vec::new();
    walk(
        fs,
        Path::new("/p"),
        max_depth,
        budget,
        &|_| false,
        &mut |v| {
            seen.push(format!(
                "{}{}",
                v.path.strip_prefix("/p").unwrap().display(),
                if v.is_dir { "/" } else { "" }
            ));
            true
        },
    );
    seen
}

#[test]
fn the_walk_honours_ignore_files_from_their_own_directory_down() {
    let fs = tree_of(&[
        (".gitignore", "*.log\n!keep.log\nbuild/\n"),
        ("a.log", ""),
        ("keep.log", ""),
        ("build/out.txt", ""),
        ("src/build", ""),
        ("src/.ignore", "gen.rs\n"),
        ("src/gen.rs", ""),
        ("src/lib.rs", ""),
        ("other/gen.rs", ""),
        (".git/HEAD", ""),
        (".hidden", ""),
    ]);
    assert_eq!(
        visited(&fs, None, usize::MAX),
        [
            ".gitignore",
            ".hidden",
            "keep.log",
            "other/",
            "other/gen.rs",
            // `build/` names directories only: this file stays.
            "src/",
            "src/.ignore",
            "src/build",
            "src/lib.rs",
        ]
    );
}

#[test]
fn a_parent_directorys_rules_apply_below_the_root() {
    let fs = MemFs::new();
    fs.insert("/.gitignore", "secret.txt\n");
    fs.insert("/p/secret.txt", "");
    fs.insert("/p/plain.txt", "");
    assert_eq!(visited(&fs, None, usize::MAX), ["plain.txt"]);
}

#[test]
fn depth_budget_and_stopping_bound_the_walk() {
    let fs = tree_of(&[("a/b/c.txt", ""), ("a/d.txt", ""), ("e.txt", "")]);
    assert_eq!(visited(&fs, Some(1), usize::MAX), ["a/", "e.txt"]);
    assert_eq!(visited(&fs, None, 2), ["a/", "a/b/"]);
    let mut seen = 0;
    walk(
        &fs,
        Path::new("/p"),
        None,
        usize::MAX,
        &|_| false,
        &mut |_| {
            seen += 1;
            seen < 3
        },
    );
    assert_eq!(seen, 3, "returning false stops the walk");
}

#[test]
fn bounded_keeps_the_first_lines_and_counts_every_one() {
    let mut b = Bounded::new(3);
    for k in [5, 1, 4, 2, 3] {
        b.push(k, format!("{k}\n"));
    }
    assert_eq!(b.into_text(), ("1\n2\n3\n".to_string(), 5));
}

#[test]
fn grep_output_follows_ripgrep() {
    let opts = |mode: &str, context: usize| GrepOptions {
        mode: mode.into(),
        case_insensitive: false,
        multiline: false,
        glob: None,
        file_type: None,
        before: context,
        after: context,
        head: 100,
    };
    let re = opts("content", 0).regex("x").unwrap();
    let text = b"x\na\nb\nc\nx\n";
    assert_eq!(
        grep_file(text, Some("f"), &re, &opts("content", 0)).unwrap(),
        "f:1:x\nf:5:x\n",
        "no separator without context"
    );
    assert_eq!(
        grep_file(text, Some("f"), &re, &opts("content", 1)).unwrap(),
        "--\nf:1:x\nf-2-a\n--\nf-4-c\nf:5:x\n",
        "each group starts with one; the last newline adds no line"
    );
    assert_eq!(
        grep_file(text, None, &re, &opts("count", 0)).unwrap(),
        "2\n"
    );
    assert_eq!(
        grep_file(b"a\0x", Some("f"), &re, &opts("content", 0)),
        None
    );
    let mut found = Bounded::new(10);
    found.push(1, "--\nf:1:x\n".to_string());
    found.push(2, "--\ng:1:x\n".to_string());
    assert_eq!(finish(found), ("f:1:x\n--\ng:1:x\n".to_string(), 3));
}
