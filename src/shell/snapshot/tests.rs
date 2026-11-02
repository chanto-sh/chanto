use super::*;

pub(crate) fn home() -> PathBuf {
    let d = std::env::temp_dir().join(format!("chanto-snaphome-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(d.join("mybin")).unwrap();
    let tool = d.join("mybin/hx-only-tool");
    std::fs::write(&tool, "#!/bin/sh\necho tool-ran\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    d
}

pub(crate) const RC: &str = "echo noisy rc output\necho more noise >&2\nexport PATH=\"$HOME/mybin:$PATH\"\nexport HX_SNAP_VAR=from-rc\nexport GITHUB_TOKEN=ghp_shouldnotleak1234567890abcdefABCDEF\nalias hxgreet='echo hello-from-alias'\nhxfn() { echo \"fn-got-$1\"; }\n_hx_completion() { :; }\nfalse\n";

fn run_with(snapshot: &Path, cmd: &str) -> String {
    let script = format!("{}{cmd}", source_line(snapshot));
    let o = std::process::Command::new("/bin/bash")
        .args(["-c", &script])
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn check(shell: &str, rc: &str) {
    if !Path::new(shell).exists() {
        return;
    }
    // The snapshot dir is under Chanto's home, which other tests point
    // elsewhere (and delete) while holding this lock.
    let _home_lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let h = home();
    std::fs::write(h.join(rc), RC).unwrap();
    let p = create(
        Path::new(shell),
        Some(&h),
        Duration::from_secs(20),
        &Default::default(),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let functions = p.with_file_name("functions.sh");
    assert_eq!(
        std::fs::metadata(&functions).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let text = std::fs::read_to_string(&p).unwrap() + &std::fs::read_to_string(&functions).unwrap();
    assert!(
        !text.contains("ghp_"),
        "secrets stay out of the snapshot: {text}"
    );
    assert!(
        !text.contains("_hx_completion") && !text.contains("noisy"),
        "{text}"
    );
    // Nothing else stays beside the snapshot, the raw capture included.
    let mut left: Vec<_> = std::fs::read_dir(p.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(left, ["functions.sh", "snapshot.sh"]);
    let out = run_with(
        &p,
        "hx-only-tool && hxgreet && hxfn x && echo \"$HX_SNAP_VAR\" && echo \"tok=${GITHUB_TOKEN:-unset}\"",
    );
    assert!(
        out.contains("tool-ran"),
        "{shell}: PATH from the rc file: {out}\n{text}"
    );
    assert!(
        out.contains("hello-from-alias"),
        "{shell}: alias: {out}\n{text}"
    );
    assert!(out.contains("fn-got-x"), "{shell}: function: {out}\n{text}");
    assert!(out.contains("from-rc"), "{out}");
    assert!(!out.contains("noisy"), "{out}");
    let _ = std::fs::remove_dir_all(p.parent().unwrap());
    let _ = std::fs::remove_dir_all(&h);
}

#[test]
fn snapshot_lives_under_chanto_home_not_temp() {
    // The sandbox can write the temp dir but not Chanto's home, so the
    // snapshot a later unsandboxed command sources must sit under home.
    // The snapshot dir is under Chanto's home, which other tests point
    // elsewhere (and delete) while holding this lock.
    let _home_lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = private_dir().unwrap();
    assert!(
        dir.starts_with(crate::config::home_dir()),
        "snapshot dir {dir:?} is not under CHANTO_HOME"
    );
    assert!(
        !dir.starts_with(std::env::temp_dir())
            || crate::config::home_dir().starts_with(std::env::temp_dir())
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stale_capture_is_removed_and_a_fresh_one_kept() {
    let _home_lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let base = crate::config::home_dir().join("shell-snapshots");
    let (stale, fresh) = (
        base.join(format!("stale-{}", uuid::Uuid::new_v4().simple())),
        base.join(format!("fresh-{}", uuid::Uuid::new_v4().simple())),
    );
    for d in [&stale, &fresh] {
        std::fs::create_dir_all(d).unwrap();
        std::fs::write(d.join("capture"), "E\0GITHUB_TOKEN\0ghp_x\0").unwrap();
        std::fs::write(d.join("snapshot.sh"), "").unwrap();
    }
    // Left by a crash an hour ago; the other is a capture under way.
    std::fs::File::options()
        .write(true)
        .open(stale.join("capture"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
    let dir = private_dir().unwrap();
    assert!(!stale.join("capture").exists(), "the stale capture stayed");
    assert!(stale.join("snapshot.sh").exists(), "only the capture goes");
    assert!(
        fresh.join("capture").exists(),
        "a capture under way was removed"
    );
    for d in [&stale, &fresh, &dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[test]
fn functions_load_on_first_call_without_aliases_rewriting_them() {
    let dir = std::env::temp_dir().join(format!("chanto-lazy-fn-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let c = Captured {
        vars: vec![],
        aliases: vec![("ls".into(), "echo ALIASED".into())],
        alias_defs: vec![],
        functions: vec![
            ("listit".into(), "listit () { ls -d /; }".into()),
            ("twice".into(), "twice () { listit; listit; }".into()),
        ],
    };
    let functions = dir.join("functions.sh");
    let r = render(&c, &|_| None, &Default::default(), &|_| false, &functions);
    std::fs::write(&functions, &r.functions).unwrap();
    let snap = dir.join("snapshot.sh");
    std::fs::write(&snap, &r.snapshot).unwrap();
    // Defining the functions costs nothing until one is called; then the
    // real ones replace the stubs, and the alias defined before them
    // doesn't leak into their bodies.
    let out = std::process::Command::new("/bin/bash")
        .arg("-c")
        .arg(format!(
            // The last line is parsed after the snapshot ran, as a later
            // command would be: aliases still apply there.
            ". '{}'; twice; declare -f listit | grep -c 'functions.sh'\nls -d /",
            snap.display()
        ))
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out, "/\n/\n0\nALIASED -d /\n", "{out}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn zsh_rc_path_aliases_and_functions_reach_bash() {
    check("/bin/zsh", ".zshrc");
}

#[test]
fn bash_rc_path_aliases_and_functions_reach_bash() {
    check("/bin/bash", ".bashrc");
}

#[test]
fn a_slow_rc_file_times_out() {
    let h = home();
    std::fs::write(h.join(".bashrc"), "sleep 30\nexport HX_SNAP_VAR=late\n").unwrap();
    let t0 = Instant::now();
    let r = create(
        Path::new("/bin/bash"),
        Some(&h),
        Duration::from_millis(500),
        &Default::default(),
    );
    assert!(matches!(r, Err(Error::TimedOut)), "{r:?}");
    assert!(t0.elapsed() < Duration::from_secs(5));
    // An rc file that waits for input gets /dev/null and moves on.
    std::fs::write(
        h.join(".bashrc"),
        "read -r answer\nexport HX_SNAP_VAR=after-read\n",
    )
    .unwrap();
    let c = capture(Path::new("/bin/bash"), Some(&h), Duration::from_secs(20)).unwrap();
    assert!(
        c.vars
            .iter()
            .any(|(k, v)| k == "HX_SNAP_VAR" && v == "after-read")
    );
    let _ = std::fs::remove_dir_all(&h);
}

#[test]
fn render_filters_and_merges_path() {
    let c = Captured {
        vars: vec![
            ("PATH".into(), "/opt/new/bin:/usr/bin:/bin".into()),
            ("SAME".into(), "x".into()),
            ("PINNED".into(), "from-rc".into()),
            ("NEW".into(), "it's".into()),
            ("PS1".into(), "$ ".into()),
            ("HTTPS_PROXY".into(), "http://x".into()),
            ("MY_API_KEY".into(), "abc".into()),
            ("bad-name".into(), "x".into()),
        ],
        aliases: vec![
            ("ll".into(), "ls -l".into()),
            ("cd".into(), "echo no".into()),
        ],
        alias_defs: vec![],
        functions: vec![
            ("_comp".into(), "_comp () { :; }".into()),
            ("prompt_x".into(), "prompt_x () { :; }".into()),
            ("ok".into(), "ok () { echo 'q'; }".into()),
        ],
    };
    let inherited = |k: &str| match k {
        "PATH" => Some("/venv/bin:/usr/bin:/bin".to_string()),
        "SAME" => Some("x".to_string()),
        _ => None,
    };
    let r = render(
        &c,
        &inherited,
        &Default::default(),
        &|k| k == "PINNED",
        Path::new("/snap/functions.sh"),
    );
    let s = format!("{}{}", r.snapshot, r.functions);
    assert!(
        s.contains("export PATH='/venv/bin:/opt/new/bin:/usr/bin:/bin'"),
        "{s}"
    );
    assert!(s.contains("export NEW='it'\\''s'"), "{s}");
    for bad in [
        "SAME",
        "PINNED",
        "PS1",
        "PROXY",
        "MY_API_KEY",
        "bad-name",
        "_comp",
        "prompt_x",
        "echo no",
    ] {
        assert!(!s.contains(bad), "{bad}: {s}");
    }
    assert!(r.snapshot.contains("alias 'll'='ls -l'"), "{s}");
    // A stub in the snapshot; the definition in the file it loads.
    assert!(
        r.snapshot
            .contains("ok () { . '/snap/functions.sh' 2>/dev/null; ok \"$@\"; }"),
        "{s}"
    );
    assert!(r.functions.starts_with("unset -f ok "), "{s}");
    assert!(
        r.functions.contains("eval 'ok () { echo '\\''q'\\''; }'"),
        "{s}"
    );
    assert_eq!(merge_path(None, "/a::/b:/a"), "/a:/b");
}

#[test]
fn an_activated_venv_stays_first_after_path_helper() {
    // macOS's login shell runs path_helper: system directories first, the
    // inherited entries (the venv) moved after them.
    assert_eq!(
        merge_path(
            Some("/venv/bin:/usr/bin:/bin"),
            "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/venv/bin"
        ),
        "/venv/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin"
    );
}

#[test]
fn startup_files_extend_a_bare_launch_path() {
    // Launched from a GUI with a bare PATH: what the startup files add
    // comes before the system directories, as they put it.
    assert_eq!(
        merge_path(
            Some("/usr/bin:/bin"),
            "/opt/homebrew/bin:/Users/u/.cargo/bin:/usr/bin:/bin"
        ),
        "/opt/homebrew/bin:/Users/u/.cargo/bin:/usr/bin:/bin"
    );
    assert_eq!(merge_path(Some("/usr/bin"), "/usr/bin:/x"), "/usr/bin:/x");
}
