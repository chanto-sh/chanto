use super::*;

fn project(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "chanto-guard-{tag}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(d.join(".git/hooks")).unwrap();
    std::fs::write(d.join(".git/config"), "[core]\n").unwrap();
    std::fs::write(d.join(".git/HEAD"), "ref\n").unwrap();
    std::fs::write(d.join("a.txt"), "a\n").unwrap();
    d
}

#[test]
fn nothing_changed_says_nothing() {
    let d = project("same");
    let g = ConfigGuard::take(&d);
    std::fs::write(d.join("a.txt"), "edited\n").unwrap();
    std::fs::write(d.join(".git/HEAD"), "moved\n").unwrap();
    assert!(g.restore().is_none(), "ordinary work isn't touched");
    assert_eq!(
        std::fs::read_to_string(d.join(".git/HEAD")).unwrap(),
        "moved\n"
    );
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn planted_and_edited_files_are_put_back() {
    let d = project("plant");
    std::fs::write(d.join(".mcp.json"), "{}\n").unwrap();
    let g = ConfigGuard::take(&d);
    std::fs::write(d.join(".git/hooks/post-commit"), "evil").unwrap();
    std::fs::write(d.join(".git/config"), "[core]\n\tfsmonitor = evil\n").unwrap();
    std::fs::write(d.join(".mcp.json"), "evil").unwrap();
    std::fs::create_dir_all(d.join(".chanto")).unwrap();
    std::fs::write(d.join(".chanto/config.toml"), "evil").unwrap();
    let note = g.restore().expect("the changes should be reported");
    assert!(
        note.contains(".git/config") && note.contains(".mcp.json"),
        "{note}"
    );
    assert!(!d.join(".git/hooks/post-commit").exists());
    assert!(!d.join(".chanto").exists());
    assert_eq!(
        std::fs::read_to_string(d.join(".git/config")).unwrap(),
        "[core]\n"
    );
    assert_eq!(
        std::fs::read_to_string(d.join(".mcp.json")).unwrap(),
        "{}\n"
    );
    assert!(g.restore().is_none(), "nothing is left to undo");
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn a_directory_swapped_for_a_link_is_put_back() {
    let d = project("swap");
    let elsewhere = project("elsewhere");
    std::fs::write(d.join(".git/hooks/keep"), "mine").unwrap();
    let g = ConfigGuard::take(&d);
    std::fs::remove_dir_all(d.join(".git/hooks")).unwrap();
    std::os::unix::fs::symlink(elsewhere.join(".git/hooks"), d.join(".git/hooks")).unwrap();
    assert!(g.restore().is_some());
    assert!(
        !d.join(".git/hooks")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(d.join(".git/hooks/keep")).unwrap(),
        "mine"
    );
    assert_eq!(
        std::fs::read_dir(elsewhere.join(".git/hooks"))
            .unwrap()
            .count(),
        0
    );
    std::fs::remove_dir_all(&d).unwrap();
    std::fs::remove_dir_all(&elsewhere).unwrap();
}

#[test]
fn chantos_own_working_files_are_left_alone() {
    let d = project("own");
    let g = ConfigGuard::take(&d);
    std::fs::create_dir_all(d.join(".chanto/worktrees/w")).unwrap();
    std::fs::write(d.join(".chanto/worktrees/w/a"), "x").unwrap();
    std::fs::write(d.join(".chanto/config.toml"), "evil").unwrap();
    assert!(g.restore().is_some(), ".chanto itself is new");
    assert!(!d.join(".chanto/config.toml").exists());
    assert!(
        d.join(".chanto/worktrees/w/a").exists(),
        "a worktree survives"
    );
    std::fs::remove_dir_all(&d).unwrap();
}
