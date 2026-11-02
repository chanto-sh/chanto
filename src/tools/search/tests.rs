use super::*;
use serde_json::json;

#[test]
fn bounded_keeps_only_the_first_lines_but_counts_all() {
    let mut b = Bounded::new(3);
    for k in (0..6).rev() {
        b.push(k, format!("line {k}"));
    }
    let (text, total) = b.into_text();
    assert_eq!(text, "line 0\nline 1\nline 2\n");
    assert_eq!(total, 6);
    // Multi-line entries: keep whole entries until the limit is covered.
    let mut b = Bounded::new(3);
    b.push("c", "c1".to_string());
    b.push("b", "b1\nb2".to_string());
    b.push("a", "a1\na2".to_string());
    let (text, total) = b.into_text();
    assert_eq!(text, "a1\na2\nb1\nb2\n");
    assert_eq!(total, 5);
}

#[tokio::test]
async fn ripgrep_paths_are_newest_first() {
    if which_rg().is_none() {
        return;
    }
    let d = std::env::temp_dir().join(format!("chanto-rg-order-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let now = std::time::SystemTime::now();
    for (name, age) in [("old.txt", 300), ("new.txt", 0), ("mid.txt", 100)] {
        let f = d.join(name);
        std::fs::write(&f, "needle\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(now - std::time::Duration::from_secs(age))
            .unwrap();
    }
    let args = json!({"pattern": "needle", "output_mode": "files_with_matches"});
    let (out, total, _) = run_rg(&args, "needle", &d, "files_with_matches", &d, 2)
        .await
        .unwrap();
    assert_eq!(out, "new.txt\nmid.txt\n");
    assert_eq!(total, 3);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn builtin_grep_is_bounded_and_ordered() {
    let d = std::env::temp_dir().join(format!("chanto-grep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    for i in 0..50 {
        std::fs::write(
            d.join(format!("f{i:02}.txt")),
            "needle one\nhay\nneedle two\n",
        )
        .unwrap();
    }
    let args = json!({"pattern": "needle", "output_mode": "content"});
    let (text, total) = builtin_grep(&grep_options(&args, "content", 5), "needle", &d, &d).unwrap();
    // Two matches per file, with no `--` between them (no context was
    // asked for, as with ripgrep).
    assert_eq!(total, 100, "every output line is counted");
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines.len() <= 6,
        "only what's needed is kept: {}",
        lines.len()
    );
    assert!(lines[0].starts_with("f00.txt:1:needle one"), "{}", lines[0]);
    assert!(
        lines[2].starts_with("f01.txt:1:"),
        "path order: {}",
        lines[2]
    );
    let (text, total) = builtin_grep(
        &grep_options(&json!({"pattern": "needle"}), "count", 3),
        "needle",
        &d,
        &d,
    )
    .unwrap();
    assert_eq!((text.lines().count(), total), (3, 50));
    let (files, total) = glob_files(&d, "*.txt", 10, WALK_BUDGET).unwrap();
    assert_eq!((files.len(), total), (10, 50));
    // A small traversal budget cuts the walk short rather than visiting the
    // whole tree (what stops `/**` from walking the entire filesystem).
    let (_, capped) = glob_files(&d, "*.txt", 100, 3).unwrap();
    assert!(capped < 50, "the budget did not bound the walk: {capped}");
    let _ = std::fs::remove_dir_all(&d);
}

/// The same tree on disk and in memory searches the same, ignore files
/// included: what the browser build sees is what the CLI would.
#[tokio::test]
async fn searching_memory_matches_searching_the_disk() {
    let d = std::env::temp_dir().join(format!("chanto-parity-{}", uuid::Uuid::new_v4()));
    let mem = std::sync::Arc::new(chanto_core::fs::MemFs::new());
    let files = [
        (".gitignore", "target/\n*.log\n!keep.log\n"),
        ("src/main.rs", "fn main() {\n    let needle = 1;\n}\n"),
        ("src/lib.rs", "pub fn needle() {}\n// Needle in a comment\n"),
        ("src/deep/mod.rs", "needle\nhay\nhay\nneedle\n"),
        ("src/deep/.ignore", "skip.rs\n"),
        ("src/deep/skip.rs", "needle\n"),
        ("target/out.rs", "needle\n"),
        ("debug.log", "needle\n"),
        ("keep.log", "needle\n"),
        ("README.md", "No match here.\n"),
        (".hidden/notes.md", "needle\n"),
        (".git/config", "needle\n"),
    ];
    for (p, text) in files {
        let path = d.join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        mem.insert(&path, text);
    }
    // The CLI's walker only honours .gitignore inside a repository.
    std::fs::create_dir_all(d.join(".git")).unwrap();
    let env = crate::agent::tests::make_env(
        "http://localhost:1",
        &d,
        crate::permissions::Mode::Default,
        32768,
    );
    let disk = Ctx {
        env,
        cancel: tokio_util::sync::CancellationToken::new(),
        depth: 0,
        call_id: None,
        cwd_override: None,
        reads: None,
    };
    let env = crate::agent::tests::make_env(
        "http://localhost:1",
        &d,
        crate::permissions::Mode::Default,
        32768,
    );
    env.set_fs(mem.clone());
    let memory = Ctx {
        env,
        cancel: tokio_util::sync::CancellationToken::new(),
        depth: 0,
        call_id: None,
        cwd_override: None,
        reads: None,
    };
    let sorted = |s: String| {
        let mut l: Vec<String> = s.lines().map(str::to_string).collect();
        l.sort();
        l
    };
    for args in [
        json!({"pattern": "**/*.rs"}),
        json!({"pattern": "*.log"}),
        json!({"pattern": "*.md"}),
    ] {
        let a = Glob.run(args.clone(), &disk).await.unwrap().content;
        let b = Glob.run(args.clone(), &memory).await.unwrap().content;
        // Same files; their order follows modification times, which differ.
        assert_eq!(sorted(a), sorted(b), "Glob {args}");
    }
    for args in [
        json!({"pattern": "needle", "output_mode": "content"}),
        json!({"pattern": "needle", "output_mode": "content", "-C": 1}),
        json!({"pattern": "NEEDLE", "-i": true, "output_mode": "count"}),
        json!({"pattern": "needle", "glob": "*.rs", "output_mode": "content"}),
        json!({"pattern": "needle", "type": "rust", "output_mode": "count"}),
        json!({"pattern": "needle", "path": "src/deep", "output_mode": "content"}),
        json!({"pattern": "needle", "path": "src/lib.rs", "output_mode": "content"}),
    ] {
        let a = Grep.run(args.clone(), &disk).await.unwrap().content;
        let b = Grep.run(args.clone(), &memory).await.unwrap().content;
        assert_eq!(sorted(a.clone()), sorted(b), "Grep {args}");
        // The fallback for a machine without ripgrep agrees too.
        let path = args["path"].as_str().map_or(d.clone(), |p| d.join(p));
        let opts = grep_options(&args, args["output_mode"].as_str().unwrap(), 400);
        let (c, _) = builtin_grep(&opts, args["pattern"].as_str().unwrap(), &path, &d).unwrap();
        assert_eq!(sorted(a), sorted(c), "built-in Grep {args}");
    }
    let files_with = json!({"pattern": "needle"});
    let a = Grep.run(files_with.clone(), &disk).await.unwrap().content;
    let b = Grep.run(files_with, &memory).await.unwrap().content;
    assert_eq!(sorted(a), sorted(b), "Grep files_with_matches");
    for args in [json!({}), json!({"depth": 1}), json!({"path": "src"})] {
        let a = Ls.run(args.clone(), &disk).await.unwrap().content;
        let b = Ls.run(args.clone(), &memory).await.unwrap().content;
        assert_eq!(a, b, "Ls {args}");
    }
    let _ = std::fs::remove_dir_all(&d);
}
