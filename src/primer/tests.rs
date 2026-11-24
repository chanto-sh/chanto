use super::*;

#[test]
fn listing_and_mentions() {
    let dir = std::env::temp_dir().join(format!("chanto-primer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("INSTRUCTIONS.md"), "Do the thing.\n").unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    std::fs::write(dir.join(".env"), "API_KEY=abc\n").unwrap();
    std::fs::write(dir.join("big.txt"), "x".repeat(40_000)).unwrap();
    let l = project_listing(&dir).unwrap();
    assert!(
        l.contains("INSTRUCTIONS.md") && l.contains("src/lib.rs") && !l.contains(".env"),
        "{l}"
    );
    let m = mentioned_files(
        &dir,
        "Implement it in src/lib.rs (see INSTRUCTIONS.md). Also .env, big.txt and https://x.io/a.md",
    );
    let names: Vec<String> = m
        .iter()
        .map(|(p, _)| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(names, vec!["lib.rs", "INSTRUCTIONS.md"]);
    assert_eq!(numbered("a\nb"), "1\ta\n2\tb\n");
    let _ = std::fs::remove_dir_all(&dir);
}
