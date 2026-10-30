use super::*;

fn scratch() -> PathBuf {
    let d = std::env::temp_dir().join(format!("chanto-hostgit-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    crate::util::canonical(&d).unwrap()
}

#[test]
fn explicit_git_directories_work_under_a_deep_home() {
    let d = scratch();
    let mut home = d.clone();
    while home.as_os_str().len() < 225 {
        home.push("nested-home");
    }
    let repo = home.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    sh(&repo, &["-c", "core.longpaths=true", "init", "-q"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    let mut c = with_dirs(&repo.join(".git"), &repo);
    let o = c.args(["add", "-A", "."]).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(c.get_args().any(|a| a == "--git-dir=.git"));
    let bare_repo = home.join("bare.git");
    std::fs::create_dir_all(&bare_repo).unwrap();
    sh(
        &bare_repo,
        &["-c", "core.longpaths=true", "init", "-q", "--bare"],
    );
    let o = bare(&bare_repo)
        .args(["rev-parse", "--is-bare-repository"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let _ = std::fs::remove_dir_all(d);
}

fn sh(dir: &Path, args: &[&str]) {
    let o = std::process::Command::new("git")
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}

/// A program that leaves `marker` behind when git runs it.
#[cfg(unix)]
fn payload(dir: &Path, marker: &Path) -> String {
    let p = dir.join("payload.sh");
    std::fs::write(&p, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    p.display().to_string()
}

#[cfg(unix)]
#[test]
fn a_hostile_repo_config_runs_nothing() {
    let d = scratch();
    let (repo, tools) = (d.join("repo"), d.join("tools"));
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&tools).unwrap();
    let marker = d.join("ran");
    let p = payload(&tools, &marker);
    sh(&repo, &["init", "-q"]);
    sh(&repo, &["config", "core.fsmonitor", &p]);
    sh(&repo, &["config", "filter.evil.clean", &p]);
    sh(&repo, &["config", "filter.evil.required", "true"]);
    std::fs::create_dir_all(repo.join(".git/hooks")).unwrap();
    std::fs::copy(&p, repo.join(".git/hooks/pre-commit")).unwrap();
    std::fs::write(repo.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    assert!(read(&repo, &["status", "--short"]).is_some());
    assert!(read(&repo, &["add", "-A"]).is_some());
    assert!(
        read(
            &repo,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "x"
            ]
        )
        .is_some()
    );
    assert!(!marker.exists(), "a repository's config ran a program");
    // The same repository without the overrides does run it.
    sh(&repo, &["status", "--short"]);
    assert!(
        marker.exists(),
        "the fixture's payload should run under plain git"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_bare_repo_inside_a_project_is_not_discovered() {
    let d = scratch();
    sh(&d, &["init", "-q", "--bare"]);
    assert!(read(&d, &["rev-parse", "--git-dir"]).is_none());
    let _ = std::fs::remove_dir_all(&d);
}

#[cfg(unix)]
#[test]
fn a_rewritten_worktree_git_file_is_ignored() {
    let d = scratch();
    let (repo, wt, evil) = (d.join("repo"), d.join("wt"), d.join("evil.git"));
    std::fs::create_dir_all(&repo).unwrap();
    sh(&repo, &["init", "-q"]);
    sh(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "w",
            &wt.display().to_string(),
        ],
    );
    let gd = worktree_git_dir(&repo, &wt).unwrap();
    assert!(
        gd.starts_with(repo.join(".git/worktrees")),
        "{}",
        gd.display()
    );
    // The worktree points its `.git` at a repository with a payload.
    let marker = d.join("ran");
    let p = payload(&d, &marker);
    sh(&d, &["init", "-q", "--bare", &evil.display().to_string()]);
    sh(
        &evil,
        &["config", "core.worktree", &wt.display().to_string()],
    );
    sh(&evil, &["config", "core.bare", "false"]);
    sh(&evil, &["config", "core.fsmonitor", &p]);
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", evil.display())).unwrap();
    std::fs::write(wt.join("new.txt"), "x").unwrap();
    let o = in_worktree(&repo, &wt)
        .unwrap()
        .args(["status", "--short"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).contains("new.txt"));
    assert!(!marker.exists(), "the planted git directory's config ran");
    // Not a worktree of this repository at all.
    assert!(worktree_git_dir(&repo, &d).is_err());
    let _ = std::fs::remove_dir_all(&d);
}

#[cfg(unix)]
#[test]
fn the_startup_log_never_verifies_signatures() {
    let d = scratch();
    let repo = d.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let marker = d.join("ran");
    let p = payload(&d, &marker);
    sh(&repo, &["init", "-q"]);
    sh(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
    // A commit carrying a (bogus) signature, which `log` hands to gpg.program.
    let o = std::process::Command::new("git")
        .args(["cat-file", "commit", "HEAD"])
        .current_dir(&repo)
        .output()
        .unwrap();
    let body = String::from_utf8_lossy(&o.stdout).into_owned();
    let (head, msg) = body.split_once("\n\n").unwrap();
    let signed = format!(
        "{head}\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n iQEz\n -----END PGP SIGNATURE-----\n\n{msg}"
    );
    let mut c = std::process::Command::new("git")
        .args(["hash-object", "-t", "commit", "-w", "--stdin"])
        .current_dir(&repo)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(c.stdin.as_mut().unwrap(), signed.as_bytes()).unwrap();
    drop(c.stdin.take());
    let id = String::from_utf8_lossy(&c.wait_with_output().unwrap().stdout)
        .trim()
        .to_string();
    sh(&repo, &["update-ref", "HEAD", &id]);
    sh(&repo, &["config", "log.showSignature", "true"]);
    sh(&repo, &["config", "gpg.program", &p]);
    assert!(read(&repo, &["log", "--oneline", "-5"]).is_some());
    assert!(!marker.exists(), "log ran gpg.program");
    sh(&repo, &["log", "--oneline", "-5"]);
    assert!(
        marker.exists(),
        "the fixture's signature should reach gpg.program"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn worktree_records_must_be_unique_and_lead_back_to_the_repository() {
    let d = scratch();
    let (repo, wt) = (d.join("repo"), d.join("wt"));
    std::fs::create_dir_all(&repo).unwrap();
    sh(&repo, &["init", "-q"]);
    sh(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "w",
            &wt.display().to_string(),
        ],
    );
    let gd = worktree_git_dir(&repo, &wt).unwrap();
    // `commondir` pointed at another repository.
    let other = d.join("other.git");
    sh(&d, &["init", "-q", "--bare", &other.display().to_string()]);
    let kept = std::fs::read_to_string(gd.join("commondir")).unwrap();
    std::fs::write(gd.join("commondir"), format!("{}\n", other.display())).unwrap();
    let e = worktree_git_dir(&repo, &wt).unwrap_err();
    assert!(e.to_string().contains("another repository"), "{e}");
    assert!(in_worktree(&repo, &wt).is_err());
    std::fs::write(gd.join("commondir"), &kept).unwrap();
    assert!(worktree_git_dir(&repo, &wt).is_ok());
    // A second record claiming the same worktree.
    let twin = gd.with_file_name("twin");
    std::fs::create_dir_all(&twin).unwrap();
    for f in ["gitdir", "commondir", "HEAD"] {
        std::fs::copy(gd.join(f), twin.join(f)).unwrap();
    }
    let e = worktree_git_dir(&repo, &wt).unwrap_err();
    assert!(e.to_string().contains("more than one"), "{e}");
    let _ = std::fs::remove_dir_all(&d);
}
