use super::*;
fn fresh_snapshot(shadow: &Shadow) -> String {
    shadow.git(&["read-tree", "--empty"]).unwrap();
    let paths = shadow.paths().unwrap();
    let list = shadow.repo.join("benchmark-paths");
    std::fs::write(
        &list,
        paths
            .iter()
            .flat_map(|p| p.bytes().chain(std::iter::once(0)))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    if !paths.is_empty() {
        shadow
            .git(&[
                "add",
                "-A",
                "--pathspec-from-file",
                list.to_str().unwrap(),
                "--pathspec-file-nul",
            ])
            .unwrap();
    }
    std::fs::remove_file(list).unwrap();
    let tree = String::from_utf8(shadow.git(&["write-tree"]).unwrap())
        .unwrap()
        .trim()
        .to_owned();
    shadow
        .git(&["update-ref", &format!("refs/checkpoints/{tree}"), &tree])
        .unwrap();
    tree
}

#[test]
fn reused_index_matches_fresh_trees() {
    // An explicit base, not CHANTO_HOME: other tests read the environment concurrently.
    let base = std::env::temp_dir().join(format!("chanto-history-home-{}", uuid::Uuid::new_v4()));
    let dir = std::env::temp_dir().join(format!("chanto-index-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = HistoryConfig {
        timeout_ms: 120000,
        ..Default::default()
    };
    for step in 0..8 {
        match step {
            0 => {
                std::fs::write(dir.join("a"), "first").unwrap();
            }
            1 => {
                std::fs::write(dir.join("a"), "modified").unwrap();
                std::fs::write(dir.join("b"), "new").unwrap();
                std::fs::write(dir.join("large"), "small").unwrap();
            }
            2 => {
                std::fs::rename(dir.join("b"), dir.join("renamed")).unwrap();
            }
            3 => {
                std::fs::remove_file(dir.join("a")).unwrap();
            }
            4 => {
                std::fs::write(dir.join(".gitignore"), "renamed\n").unwrap();
            }
            5 => {
                std::fs::write(dir.join("renamed"), vec![0; 6 * 1024 * 1024]).unwrap();
                std::fs::write(dir.join("large"), vec![0; 6 * 1024 * 1024]).unwrap();
            }
            6 => {
                std::fs::remove_file(dir.join(".gitignore")).unwrap();
                std::fs::write(dir.join("renamed"), "eligible again").unwrap();
            }
            _ => {
                std::fs::write(dir.join(".gitignore"), "").unwrap();
                std::fs::remove_file(dir.join(".gitignore")).unwrap();
                std::fs::remove_file(dir.join("renamed")).unwrap();
                std::fs::remove_file(dir.join("large")).unwrap();
            }
        }
        let shadow = Shadow::in_dir(&base, &dir, &config).unwrap();
        let reused = shadow.snapshot().unwrap();
        assert_eq!(
            reused,
            fresh_snapshot(&Shadow::in_dir(&base, &dir, &config).unwrap()),
            "step {step}"
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn restore_preview_lists_deletions_first_and_limits_paths() {
    // An explicit base, not CHANTO_HOME: other tests read the environment concurrently.
    let base = std::env::temp_dir().join(format!("chanto-history-home-{}", uuid::Uuid::new_v4()));
    let dir = std::env::temp_dir().join(format!("chanto-preview-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("changed"), "before").unwrap();
    let config = HistoryConfig {
        timeout_ms: 120000,
        ..Default::default()
    };
    let tree = Shadow::in_dir(&base, &dir, &config)
        .unwrap()
        .snapshot()
        .unwrap();
    std::fs::write(dir.join("changed"), "after").unwrap();
    for i in 0..16 {
        std::fs::write(dir.join(format!("editor-{i:02}")), "user").unwrap();
    }
    let shadow = Shadow::in_dir(&base, &dir, &config).unwrap();
    let summary = shadow.restore_summary(&tree).unwrap();
    assert_eq!(
        summary.lines().filter(|p| p.starts_with("Delete:")).count(),
        15
    );
    assert!(summary.ends_with("+2 more"));
    std::fs::remove_file(dir.join("editor-15")).unwrap();
    std::fs::remove_file(dir.join("editor-14")).unwrap();
    let summary = Shadow::in_dir(&base, &dir, &config)
        .unwrap()
        .restore_summary(&tree)
        .unwrap();
    assert!(summary.ends_with("Write: changed"));
    assert!(summary.find("Delete:").unwrap() < summary.find("Write:").unwrap());
    std::fs::remove_dir_all(dir).unwrap();
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn a_git_that_cannot_start_leaves_no_files_behind() {
    // Each failed start (no git, or the project gone) left an output and a
    // stderr file in the shadow repository.
    let base = std::env::temp_dir().join(format!("chanto-nogit-{}", uuid::Uuid::new_v4()));
    let dir = std::env::temp_dir().join(format!("chanto-nogit-proj-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = HistoryConfig {
        timeout_ms: 120000,
        ..Default::default()
    };
    let shadow = Shadow::in_dir(&base, &dir, &config).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    let e = shadow.snapshot().unwrap_err().to_string();
    assert!(!e.contains("git isn't installed"), "{e}");
    let leftovers: Vec<_> = std::fs::read_dir(&shadow.repo)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("output-") || n.starts_with("stderr-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn first_snapshot_is_packed_and_restores_exactly() {
    // The first snapshot streams every file into one pack (fast on big
    // projects); it must record the same tree `git add` would, and
    // rewinding to it must bring back every byte and mode.
    let base = std::env::temp_dir().join(format!("chanto-first-{}", uuid::Uuid::new_v4()));
    let dir = std::env::temp_dir().join(format!("chanto-first-proj-{}", uuid::Uuid::new_v4()));
    for d in 0..10 {
        std::fs::create_dir_all(dir.join(format!("src/m{d}"))).unwrap();
        for f in 0..30 {
            std::fs::write(
                dir.join(format!("src/m{d}/f{f}.rs")),
                format!("fn f{f}() {{ /* {d} */ }}\n"),
            )
            .unwrap();
        }
    }
    std::fs::write(dir.join("empty"), "").unwrap();
    std::fs::write(
        dir.join("bin.dat"),
        (0..=255u8).cycle().take(70_000).collect::<Vec<u8>>(),
    )
    .unwrap();
    std::fs::write(dir.join("name with spaces & $(x).txt"), "odd\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(dir.join("run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::os::unix::fs::symlink("empty", dir.join("link")).unwrap();
    }
    let config = HistoryConfig {
        timeout_ms: 120000,
        ..Default::default()
    };
    let shadow = Shadow::in_dir(&base, &dir, &config).unwrap();
    assert!(shadow.first);
    let tree = shadow.snapshot().unwrap();
    let packs = std::fs::read_dir(shadow.repo.join("objects/pack"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".pack"))
        .count();
    assert!(packs >= 1, "the first snapshot is written as a pack");
    let leftovers: Vec<_> = std::fs::read_dir(&shadow.repo)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("input-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    let later = Shadow::in_dir(&base, &dir, &config).unwrap();
    assert!(!later.first);
    assert_eq!(
        later.snapshot().unwrap(),
        tree,
        "an unchanged project gives the same tree"
    );
    assert_eq!(
        fresh_snapshot(&later),
        tree,
        "same tree as a plain `git add`"
    );
    // Change, add and delete files, then rewind.
    let originals: Vec<(PathBuf, Vec<u8>)> = [
        "src/m3/f7.rs",
        "bin.dat",
        "empty",
        "name with spaces & $(x).txt",
    ]
    .iter()
    .map(|p| (dir.join(p), std::fs::read(dir.join(p)).unwrap()))
    .collect();
    std::fs::write(dir.join("src/m3/f7.rs"), "changed").unwrap();
    std::fs::write(dir.join("bin.dat"), "short").unwrap();
    std::fs::remove_file(dir.join("name with spaces & $(x).txt")).unwrap();
    std::fs::write(dir.join("src/new.rs"), "new").unwrap();
    Shadow::in_dir(&base, &dir, &config)
        .unwrap()
        .restore(&tree)
        .unwrap();
    for (p, bytes) in &originals {
        assert_eq!(&std::fs::read(p).unwrap(), bytes, "{}", p.display());
    }
    assert!(!dir.join("src/new.rs").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.join("run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::read_link(dir.join("link")).unwrap(),
            Path::new("empty")
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn sessions_starting_together_share_one_shadow_repository() {
    let base = std::env::temp_dir().join(format!("chanto-init-race-{}", uuid::Uuid::new_v4()));
    let dir = std::env::temp_dir().join(format!("chanto-init-race-proj-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "a").unwrap();
    let config = HistoryConfig {
        timeout_ms: 120000,
        ..Default::default()
    };
    let opened: Vec<Result<Shadow>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|_| s.spawn(|| Shadow::in_dir(&base, &dir, &config)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for o in opened {
        o.unwrap().snapshot().unwrap();
    }
    // A listed file that's gone by the time it's added (a concurrent
    // session's temp file) is skipped, not a failed snapshot.
    let shadow = Shadow::in_dir(&base, &dir, &config).unwrap();
    let mut paths = vec!["a.txt".to_string(), ".a.txt.123.tmp".to_string()];
    shadow.add_paths(&mut paths).unwrap();
    assert_eq!(paths, vec!["a.txt".to_string()]);
    std::fs::remove_dir_all(dir).unwrap();
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn a_project_holding_chanto_home_leaves_it_out() {
    // Started in ~, the snapshot took in ~/.chanto (sessions, and the
    // shadow repository itself) and failed on its vanishing temp files.
    let dir = std::env::temp_dir().join(format!("chanto-home-proj-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join(".hh/sessions")).unwrap();
    let dir = dir.canonical().unwrap();
    std::fs::write(dir.join("a.txt"), "a").unwrap();
    std::fs::write(dir.join(".hh/sessions/s.json"), "{}").unwrap();
    let config = HistoryConfig {
        timeout_ms: 120000,
        ..Default::default()
    };
    let shadow = Shadow::in_dir(&dir.join(".hh/history"), &dir, &config).unwrap();
    shadow.snapshot().unwrap();
    let listed = String::from_utf8(shadow.git(&["ls-files"]).unwrap()).unwrap();
    assert_eq!(listed.trim(), "a.txt");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "first-snapshot measurement; set CHANTO_BENCH_DIR"]
fn first_snapshot_benchmark() {
    let Ok(root) = std::env::var("CHANTO_BENCH_DIR") else {
        return;
    };
    let base = std::env::temp_dir().join(format!("chanto-bench-home-{}", uuid::Uuid::new_v4()));
    let config = HistoryConfig {
        timeout_ms: 600_000,
        ..Default::default()
    };
    let t = Instant::now();
    let s = Shadow::in_dir(&base, Path::new(&root), &config).unwrap();
    let open = t.elapsed();
    let t = Instant::now();
    let mut paths = s.paths().unwrap();
    let list = t.elapsed();
    let t = Instant::now();
    s.add_paths(&mut paths).unwrap();
    let add = t.elapsed();
    let t = Instant::now();
    let s = Shadow::in_dir(&base, Path::new(&root), &config).unwrap();
    s.snapshot().unwrap();
    let second = t.elapsed();
    eprintln!(
        "files {} open {open:?} list {list:?} add {add:?}; next snapshot {second:?}",
        paths.len()
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
#[ignore = "snapshot performance measurement"]
fn snapshot_benchmark() {
    let _guard = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let synthetic = std::env::temp_dir().join(format!("chanto-bench-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&synthetic).unwrap();
    for i in 0..20000 {
        std::fs::write(synthetic.join(format!("file-{i}")), vec![b'x'; 4096]).unwrap();
    }
    let config = HistoryConfig {
        timeout_ms: 120000,
        ..Default::default()
    };
    for root in [std::env::current_dir().unwrap(), synthetic.clone()] {
        let mut before = Vec::new();
        let mut after = Vec::new();
        Shadow::new(&root, &config).unwrap().snapshot().unwrap();
        for _ in 0..7 {
            let s = Shadow::new(&root, &config).unwrap();
            let start = Instant::now();
            fresh_snapshot(&s);
            before.push(start.elapsed().as_secs_f64() * 1000.0);
            let start = Instant::now();
            Shadow::new(&root, &config).unwrap().snapshot().unwrap();
            after.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        before.sort_by(f64::total_cmp);
        after.sort_by(f64::total_cmp);
        eprintln!(
            "{}: empty-index {:.1} ms; reused-index {:.1} ms (median of 7, warm)",
            root.display(),
            before[3],
            after[3]
        );
    }
    let s = Shadow::new(&synthetic, &config).unwrap();
    std::fs::remove_dir_all(&s.repo).unwrap();
    std::fs::remove_dir_all(synthetic).unwrap();
}

#[test]
fn shell_changes_and_git_isolation() {
    // An explicit base, not CHANTO_HOME: other tests read the environment concurrently.
    let base = std::env::temp_dir().join(format!("chanto-shadow-home-{}", uuid::Uuid::new_v4()));
    let dir = std::env::temp_dir().join(format!("chanto-shadow-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    };
    git(&["init", "-q"]);
    // Git for Windows defaults to autocrlf, which would rewrite the files on stash.
    git(&["config", "core.autocrlf", "false"]);
    std::fs::write(dir.join(".gitignore"), "ignored\n").unwrap();
    for name in ["edit", "shell", "deleted"] {
        std::fs::write(dir.join(name), "before\n").unwrap();
    }
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-qm",
        "initial",
    ]);
    std::fs::write(dir.join("edit"), "stashed content").unwrap();
    git(&["stash", "push", "-qm", "saved change"]);
    let head = git(&["rev-parse", "HEAD"]);
    let index = std::fs::read(dir.join(".git/index")).unwrap();
    let refs = git(&["show-ref"]);
    let stash = git(&["stash", "list"]);
    std::fs::write(dir.join("ignored"), "ignored before").unwrap();
    std::fs::create_dir_all(dir.join("node_modules")).unwrap();
    std::fs::write(dir.join("node_modules/cache"), "before").unwrap();
    std::fs::write(dir.join("large"), vec![0; 6 * 1024 * 1024]).unwrap();
    let config = HistoryConfig {
        timeout_ms: 10000,
        ..Default::default()
    };
    let shadow = Shadow::in_dir(&base, &dir, &config).unwrap();
    let tree = shadow.snapshot().unwrap();
    std::fs::write(dir.join("edit"), "edited").unwrap();
    // What `sed -i` and friends do: write a replacement and rename it into place.
    std::fs::write(dir.join("shell.tmp"), "after\n").unwrap();
    std::fs::rename(dir.join("shell.tmp"), dir.join("shell")).unwrap();
    std::fs::write(dir.join("new"), "created\n").unwrap();
    std::fs::remove_file(dir.join("deleted")).unwrap();
    std::fs::write(dir.join("ignored"), "ignored after").unwrap();
    std::fs::write(dir.join("node_modules/cache"), "after").unwrap();
    std::fs::write(dir.join("large"), vec![1; 6 * 1024 * 1024]).unwrap();
    let shadow = Shadow::in_dir(&base, &dir, &config).unwrap();
    assert_eq!(shadow.changes(&tree).unwrap(), (2, 1, 1));
    assert_eq!(shadow.restore(&tree).unwrap(), 4);
    for name in ["edit", "shell", "deleted"] {
        assert_eq!(std::fs::read_to_string(dir.join(name)).unwrap(), "before\n");
    }
    assert!(!dir.join("new").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("ignored")).unwrap(),
        "ignored after"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("node_modules/cache")).unwrap(),
        "after"
    );
    assert_eq!(std::fs::read(dir.join("large")).unwrap()[0], 1);
    assert_eq!(head, git(&["rev-parse", "HEAD"]));
    assert_eq!(index, std::fs::read(dir.join(".git/index")).unwrap());
    assert_eq!(refs, git(&["show-ref"]));
    assert_eq!(stash, git(&["stash", "list"]));
    std::fs::remove_dir_all(dir).unwrap();
    std::fs::remove_dir_all(base).unwrap();
}
#[test]
#[ignore = "manual snapshot overhead measurement on the current project"]
fn snapshot_overhead() {
    let _guard = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("CHANTO_HOME");
    let home = std::env::temp_dir().join(format!("chanto-measure-{}", uuid::Uuid::new_v4()));
    unsafe {
        std::env::set_var("CHANTO_HOME", &home);
    }
    let root = std::env::current_dir().unwrap();
    for i in 0..6 {
        let start = Instant::now();
        let shadow = Shadow::new(&root, &HistoryConfig::default()).unwrap();
        let tree = shadow.snapshot().unwrap();
        println!(
            "snapshot {i}: {:.1} ms ({tree})",
            start.elapsed().as_secs_f64() * 1000.0
        );
    }
    unsafe {
        if let Some(previous) = previous {
            std::env::set_var("CHANTO_HOME", previous);
        } else {
            std::env::remove_var("CHANTO_HOME");
        }
    }
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn a_projects_first_snapshot_gets_a_longer_allowance() {
    let base = std::env::temp_dir().join(format!("chanto-shadow-home-{}", uuid::Uuid::new_v4()));
    let dir = std::env::temp_dir().join(format!("chanto-shadow-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..50 {
        std::fs::write(dir.join(format!("f{i}.txt")), "x".repeat(10_000)).unwrap();
    }
    // A budget no snapshot could meet: the first one still completes,
    // since it can't get faster until it has run once.
    let tight = HistoryConfig {
        timeout_ms: 0,
        ..Default::default()
    };
    assert!(
        Shadow::in_dir(&base, &dir, &tight)
            .unwrap()
            .snapshot()
            .is_ok()
    );
    // Later snapshots are held to the configured budget again.
    std::fs::write(dir.join("f0.txt"), "changed").unwrap();
    assert!(
        Shadow::in_dir(&base, &dir, &tight)
            .unwrap()
            .snapshot()
            .unwrap_err()
            .to_string()
            .contains("time budget")
    );
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_lock_left_by_a_killed_git_does_not_break_later_snapshots() {
    let base = std::env::temp_dir().join(format!("chanto-shadow-home-{}", uuid::Uuid::new_v4()));
    let dir = std::env::temp_dir().join(format!("chanto-shadow-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "one").unwrap();
    let config = HistoryConfig {
        timeout_ms: 10000,
        ..Default::default()
    };
    let shadow = Shadow::in_dir(&base, &dir, &config).unwrap();
    assert!(shadow.snapshot().is_ok());
    // A fresh lock may belong to a snapshot running in another session: git's reason is reported.
    let lock = shadow.repo.join("index.lock");
    std::fs::write(&lock, "").unwrap();
    std::fs::write(dir.join("a.txt"), "two").unwrap();
    let err = Shadow::in_dir(&base, &dir, &config)
        .unwrap()
        .snapshot()
        .unwrap_err()
        .to_string();
    assert!(err.contains("index.lock"), "{err}");
    // An old one has no live owner and is cleared.
    let old = std::time::SystemTime::now() - Duration::from_secs(600);
    std::fs::File::options()
        .write(true)
        .open(&lock)
        .unwrap()
        .set_modified(old)
        .unwrap();
    assert!(
        Shadow::in_dir(&base, &dir, &config)
            .unwrap()
            .snapshot()
            .is_ok()
    );
    assert!(!lock.exists());
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn budgets_and_empty_project() {
    // An explicit base, not CHANTO_HOME: other tests read the environment concurrently.
    let base = std::env::temp_dir().join(format!("chanto-shadow-home-{}", uuid::Uuid::new_v4()));
    let dir = std::env::temp_dir().join(format!("chanto-shadow-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = HistoryConfig {
        timeout_ms: 10000,
        ..Default::default()
    };
    let shadow = Shadow::in_dir(&base, &dir, &config).unwrap();
    assert!(shadow.snapshot().is_ok());
    std::fs::write(dir.join("file"), "content").unwrap();
    let limited = Shadow::in_dir(
        &base,
        &dir,
        &HistoryConfig {
            max_snapshot_bytes: 1,
            ..config.clone()
        },
    )
    .unwrap();
    assert!(
        limited
            .snapshot()
            .unwrap_err()
            .to_string()
            .contains("size budget")
    );
    let limited = Shadow::in_dir(
        &base,
        &dir,
        &HistoryConfig {
            timeout_ms: 0,
            ..config
        },
    )
    .unwrap();
    assert!(limited.snapshot().is_err());
    std::fs::remove_dir_all(dir).unwrap();
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn maintenance_leaves_a_project_without_snapshots_alone() {
    let _guard = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("CHANTO_HOME");
    let home = std::env::temp_dir().join(format!("chanto-maint-{}", uuid::Uuid::new_v4().simple()));
    let dir = home.join("project");
    std::fs::create_dir_all(&dir).unwrap();
    unsafe { std::env::set_var("CHANTO_HOME", &home) };
    maintain(&dir, &HistoryConfig::default()).unwrap();
    let repo = repo_dir(&home.join("history"), &dir.canonical().unwrap());
    assert!(!repo.exists(), "maintenance created a repository");
    // Once a snapshot made one, maintenance runs as before.
    Shadow::new(&dir, &HistoryConfig::default()).unwrap();
    maintain(&dir, &HistoryConfig::default()).unwrap();
    assert!(repo.join("maintenance").exists());
    unsafe {
        match previous {
            Some(p) => std::env::set_var("CHANTO_HOME", p),
            None => std::env::remove_var("CHANTO_HOME"),
        }
    }
    let _ = std::fs::remove_dir_all(&home);
}
