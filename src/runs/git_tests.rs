use super::*;

fn scratch() -> PathBuf {
    let d = std::env::temp_dir().join(format!("chanto-rungit-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    std::fs::canonicalize(&d).unwrap()
}

fn sh(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
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
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// A program that leaves `marker` behind when git runs it.
#[cfg(unix)]
fn payload(dir: &Path, marker: &Path) -> String {
    use std::os::unix::fs::PermissionsExt as _;
    let p = dir.join("payload.sh");
    std::fs::write(
        &p,
        format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.display().to_string()
}

/// A repository with one commit, and a linked worktree of it on `w`.
fn repo_and_worktree(d: &Path) -> (PathBuf, PathBuf) {
    let (repo, wt) = (d.join("repo"), d.join("wt"));
    std::fs::create_dir_all(&repo).unwrap();
    sh(&repo, &["init", "-q", "-b", "main"]);
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
    (repo, wt)
}

#[cfg(unix)]
#[test]
fn finalize_never_signs_with_the_repositorys_gpg_program() {
    let d = scratch();
    let (repo, wt) = repo_and_worktree(&d);
    let marker = d.join("ran");
    let p = payload(&d, &marker);
    sh(&repo, &["config", "commit.gpgSign", "true"]);
    sh(&repo, &["config", "gpg.program", &p]);
    std::fs::write(wt.join("a.txt"), "a\n").unwrap();
    assert!(finalize(&repo, &wt, "r1", "task").unwrap());
    assert!(!marker.exists(), "finalize ran gpg.program");
    assert_eq!(sh(&wt, &["status", "--porcelain"]), "");
    let _ = std::fs::remove_dir_all(&d);
}

#[cfg(unix)]
#[test]
fn a_nested_repositorys_filter_does_not_run_on_finalize() {
    let d = scratch();
    let (repo, wt) = repo_and_worktree(&d);
    let marker = d.join("ran");
    let p = payload(&d, &marker);
    // A repository inside the worktree, with a clean filter of its own.
    let sub = wt.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    sh(&sub, &["init", "-q"]);
    std::fs::write(sub.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
    std::fs::write(sub.join("f.txt"), "one\n").unwrap();
    sh(&sub, &["add", "-A"]);
    sh(&sub, &["commit", "-q", "-m", "sub"]);
    sh(&sub, &["config", "filter.evil.clean", &p]);
    std::fs::write(sub.join("f.txt"), "two, longer\n").unwrap();
    std::fs::write(wt.join("a.txt"), "a\n").unwrap();
    assert!(finalize(&repo, &wt, "r1", "task").unwrap());
    let r = run_json(&repo, &wt);
    let _ = diff_stat(&r);
    assert!(!marker.exists(), "the nested repository's filter ran");
    let _ = std::fs::remove_dir_all(&d);
}

fn run_json(repo: &Path, wt: &Path) -> super::super::Run {
    serde_json::from_value(serde_json::json!({
        "id": "r1", "task": "t", "state": "done", "created": "",
        "worktree": wt, "branch": "w", "base": sh(repo, &["rev-parse", "main"]),
        "settings": {}, "source": {"kind": "local", "path": repo, "head": "x"}
    }))
    .unwrap()
}

fn imported(id: &str, base: &str) -> super::super::Run {
    serde_json::from_value(serde_json::json!({
        "id": id, "task": "t", "state": "done", "created": "",
        "worktree": "", "branch": format!("chanto/run/{id}"), "base": base,
        "settings": {}, "source": {"kind": "local", "path": "/r", "head": "x"}
    }))
    .unwrap()
}

#[test]
fn imported_runs_only_create_their_own_branch_from_a_known_base() {
    let d = scratch();
    let server = d.join("server");
    std::fs::create_dir_all(&server).unwrap();
    sh(&server, &["init", "-q", "-b", "main"]);
    sh(&server, &["commit", "-q", "--allow-empty", "-m", "base"]);
    let base = sh(&server, &["rev-parse", "HEAD"]);
    let client = d.join("client");
    sh(
        &d,
        &["clone", "-q", &server.display().to_string(), "client"],
    );
    // The run's work on the server, as a bundle of the commits after base.
    let id = "ab12cd34";
    let branch = format!("chanto/run/{id}");
    sh(&server, &["checkout", "-q", "-b", &branch]);
    sh(&server, &["commit", "-q", "--allow-empty", "-m", "work"]);
    let tip = sh(&server, &["rev-parse", "HEAD"]);
    let bundle = d.join("run.bundle");
    sh(
        &server,
        &[
            "bundle",
            "create",
            "-q",
            &bundle.display().to_string(),
            &format!("refs/heads/{branch}"),
            &format!("^{base}"),
        ],
    );
    let run = imported(id, &base);
    import_run(&client, &run, &bundle).unwrap();
    assert_eq!(sh(&client, &["rev-parse", &branch]), tip);
    // Again: the branch is already there at the same commit.
    import_run(&client, &run, &bundle).unwrap();
    // A branch of that name with other commits is never moved.
    sh(&client, &["branch", "-f", &branch, "main"]);
    let e = import_run(&client, &run, &bundle).unwrap_err();
    assert!(e.to_string().contains("already exists"), "{e}");
    assert_eq!(sh(&client, &["rev-parse", &branch]), base);
    sh(&client, &["branch", "-D", &branch]);
    // A base that isn't below the run's work.
    sh(&client, &["commit", "-q", "--allow-empty", "-m", "local"]);
    let local = sh(&client, &["rev-parse", "HEAD"]);
    let e = import_run(&client, &imported(id, &local), &bundle).unwrap_err();
    assert!(e.to_string().contains("isn't a commit"), "{e}");
    assert!(sh(&client, &["branch", "--list", &branch]).is_empty());
    // The server's id, branch and base are checked before any git runs.
    let mut other = imported(id, &base);
    other.branch = "main".into();
    assert!(check_imported(&other).is_err());
    assert!(check_imported(&imported(id, "--output=/tmp/x")).is_err());
    assert!(check_imported(&imported(id, &base[..12])).is_err());
    assert!(check_imported(&imported("../x", &base)).is_err());
    assert!(check_imported(&imported(id, &base)).is_ok());
    let _ = std::fs::remove_dir_all(&d);
}
