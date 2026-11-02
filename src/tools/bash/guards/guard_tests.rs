use super::*;

#[test]
fn deny_paths_cover_persistence_and_toolchains() {
    let cwd = std::path::Path::new("/tmp/proj");
    let home = std::path::Path::new("/home/u");
    let denied = write_deny_paths(cwd, home);
    let has = |p: &str| denied.iter().any(|d| d == std::path::Path::new(p));
    for p in [
        "/tmp/proj/.chanto",
        "/tmp/proj/.claude/settings.json",
        "/tmp/proj/.mcp.json",
        "/tmp/proj/.husky",
        "/tmp/proj/.githooks",
        "/tmp/proj/.vscode/tasks.json",
        "/home/u/.cargo/bin",
        "/home/u/.rustup",
        "/home/u/.pyenv",
        "/home/u/.gradle/init.d",
    ] {
        assert!(has(p), "missing deny for {p}");
    }
}

#[test]
fn git_directory_is_pinned() {
    let dir = std::env::temp_dir().join(format!("chanto-gitguard-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join(".git/hooks")).unwrap();
    let guards = write_guards(&dir, std::path::Path::new("/home/u"));
    assert!(guards.contains(&Guard::Pin(dir.join(".git"))), "{guards:?}");
    assert!(
        guards
            .iter()
            .any(|g| matches!(g, Guard::Deny(p) if p == &dir.join(".git/config")))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A worktree's `.git` is a file (`gitdir: …`); its target dir's hooks and
/// config are protected too, and the `.git` file itself is denied.
#[test]
fn worktree_gitfile_and_its_gitdir_are_protected() {
    let base = std::env::temp_dir().join(format!("chanto-wtguard-{}", uuid::Uuid::new_v4()));
    let wt = base.join("wt");
    let gd = base.join("realgit");
    std::fs::create_dir_all(gd.join("hooks")).unwrap();
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gd.display())).unwrap();
    let guards = write_guards(&wt, std::path::Path::new("/home/u"));
    assert!(
        guards
            .iter()
            .any(|g| matches!(g, Guard::Deny(p) if p == &wt.join(".git")))
    );
    assert!(
        guards
            .iter()
            .any(|g| matches!(g, Guard::Deny(p) if p == &gd.join("hooks")))
    );
    assert!(
        guards
            .iter()
            .any(|g| matches!(g, Guard::Deny(p) if p == &gd.join("config")))
    );
    let _ = std::fs::remove_dir_all(&base);
}
