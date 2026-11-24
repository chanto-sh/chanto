use super::*;

#[test]
fn create_and_cleanup() {
    let d = std::env::temp_dir().join(format!("chanto-wt-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    let run = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&d)
                .output()
                .unwrap()
                .status
                .success()
        )
    };
    run(&["init", "-q"]);
    run(&[
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "init",
    ]);
    let wt = create(&d, "a1").unwrap();
    assert!(wt.path.join(".git").exists());
    assert!(wt.cleanup_if_unchanged());
    assert!(!wt.path.exists());
    let wt = create(&d, "a2").unwrap();
    std::fs::write(wt.path.join("new.txt"), "x").unwrap();
    assert!(wt.changes().contains("new.txt"));
    assert!(!wt.cleanup_if_unchanged());
    // The main checkout doesn't see the worktree directory as untracked.
    let st = Command::new("git")
        .args(["status", "--short"])
        .current_dir(&d)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&st.stdout).trim().is_empty());
}
