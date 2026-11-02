use super::*;

#[cfg(unix)]
#[test]
fn the_outcome_diff_runs_no_diff_drivers() {
    use std::os::unix::fs::PermissionsExt as _;
    let d = std::env::temp_dir().join(format!("chanto-outcome-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    let sh = |args: &[&str]| {
        let o = std::process::Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .current_dir(&d)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    };
    let program = |name: &str| {
        let p = d.join(format!(".{name}.sh"));
        let marker = d.join(format!(".{name}-ran"));
        std::fs::write(
            &p,
            format!("#!/bin/sh\ntouch '{}'\ncat \"$1\"\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        (p.display().to_string(), marker)
    };
    let (external, ext_ran) = program("external");
    let (textconv, tc_ran) = program("textconv");
    sh(&["init", "-q"]);
    std::fs::write(d.join(".gitattributes"), "*.txt diff=tc\n").unwrap();
    std::fs::write(d.join("a.txt"), "one\n").unwrap();
    sh(&["add", "-A"]);
    sh(&["commit", "-q", "-m", "init"]);
    sh(&["config", "diff.external", &external]);
    sh(&["config", "diff.tc.textconv", &textconv]);
    std::fs::write(d.join("a.txt"), "two\n").unwrap();
    let (files, diff) = changed_files(&d, "HEAD");
    assert!(
        files.iter().any(|(f, st)| f == "a.txt" && *st == 'M'),
        "{files:?}"
    );
    assert!(diff.contains("+two"), "{diff}");
    assert!(!ext_ran.exists(), "diff.external ran");
    assert!(!tc_ran.exists(), "the textconv driver ran");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn flags_risky_changes() {
    let files = vec![
        ("src/auth/login.py".to_string(), 'M'),
        ("migrations/0003_users.sql".to_string(), 'A'),
        ("package.json".to_string(), 'M'),
        ("old.py".to_string(), 'D'),
        ("src/util.py".to_string(), 'M'),
        (".chanto/outcome/x/criteria.json".to_string(), 'A'),
    ];
    let diff = "+++ b/src/util.py\n+import subprocess\n+subprocess.run(cmd, shell=True)\n+++ b/src/db.py\n+cur.execute(f\"SELECT * FROM t WHERE id={x}\")\n";
    let r = risks(&files, diff);
    let has = |f: &str, why: &str| r.iter().any(|x| x.file == f && x.reason == why);
    assert!(has("src/auth/login.py", "auth / security code"));
    assert!(has(
        "migrations/0003_users.sql",
        "database schema or migration"
    ));
    assert!(has("package.json", "dependencies changed"));
    assert!(has("old.py", "file deleted"));
    assert!(has("src/util.py", "runs commands / dynamic code"));
    assert!(has("src/db.py", "injection-prone code"));
    assert!(!r.iter().any(|x| x.file.starts_with(".chanto/")));
}
