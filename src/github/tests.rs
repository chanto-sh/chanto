use super::*;

fn tmp() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("chanto-github-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn install_and_preserve_existing_workflows() {
    let tmp = tmp();
    let out = install(&tmp).unwrap();
    assert!(out.contains("ANTHROPIC_API_KEY") && out.contains("Created .chanto/review.toml"));
    let path = tmp.join(".github/workflows/chanto-review.yml");
    let before = std::fs::read_to_string(&path).unwrap();
    assert!(before.contains("pull_request:"));
    assert!(!before.contains("pull_request_target"));
    assert!(before.contains("permissions:") && before.contains("contents: read"));
    let commands = std::fs::read_to_string(tmp.join(".github/workflows/chanto.yml")).unwrap();
    assert!(
        commands.contains("author_association") && commands.contains("persist-credentials: false")
    );
    assert!(install(&tmp).is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), before);
    std::fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn keeps_existing_policy() {
    let tmp = tmp();
    std::fs::create_dir_all(tmp.join(".chanto")).unwrap();
    std::fs::write(tmp.join(".chanto/review.toml"), "strictness = \"all\"\n").unwrap();
    let out = install(&tmp).unwrap();
    assert!(out.contains("Kept existing .chanto/review.toml"));
    assert_eq!(
        std::fs::read_to_string(tmp.join(".chanto/review.toml")).unwrap(),
        "strictness = \"all\"\n"
    );
    std::fs::remove_dir_all(tmp).unwrap();
}
