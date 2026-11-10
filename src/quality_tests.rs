use super::*;

fn scratch() -> PathBuf {
    let d = std::env::temp_dir().join(format!("chanto-quality-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

const SAMPLE: &str = r#"
version = 1

[gate]
enabled = true
scope = "full"
oracle = true
unknown = 1

[[check]]
name = "tests"
run = "cargo test"

[[check]]
name = "slow"
run = "cargo bench"
when = "nightly"

[[check]]
name = "broken"
run = "npm run lint"
when = "off"

[[rule]]
id = "no-unwrap"
title = "No unwrap in src"
regex = '\.unwrap\(\)'
paths = ["src/**"]
exclude = ["src/**/*_tests.rs"]

[[rule]]
id = "prose-only"
title = "Errors in plain language"

[review]
no_touch = ["vendor/**"]
"#;

#[test]
fn profile_maps_to_guardrails_and_denies() {
    let p = parse(SAMPLE).unwrap();
    let t = to_table(&p, Some("chanto quality check"));
    let g = t["guardrails"].as_table().unwrap();
    assert_eq!(g["enabled"].as_bool(), Some(true));
    assert_eq!(g["scope"].as_str(), Some("full"));
    assert_eq!(g["oracle"].as_bool(), Some(true));
    assert!(!g.contains_key("unknown"));
    let checks: Vec<&str> = g["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(checks, ["cargo test"], "nightly and off checks stay out");
    assert_eq!(
        g["quality_checks"].as_array().unwrap()[0].as_str(),
        Some("chanto quality check")
    );
    let deny = t["permissions"]["deny"].as_array().unwrap();
    assert_eq!(deny[0].as_str(), Some("Edit(vendor/**)"));
    // The table parses as the guardrails section of a config.
    let cfg: crate::guardrails::GuardrailsConfig = g.clone().try_into().unwrap();
    assert_eq!(cfg.checks, ["cargo test"]);
    assert_eq!(cfg.quality_checks, ["chanto quality check"]);
    assert_eq!(cfg.scope, Some(crate::guardrails::Scope::Full));
}

#[test]
fn no_checks_keeps_detection_and_the_secrets_scan_runs_by_default() {
    let p = parse("[[rule]]\nid = \"x\"\ntitle = \"prose\"\n").unwrap();
    assert!(p.secrets.scan);
    let t = to_table(&p, Some("chanto quality check"));
    let g = t["guardrails"].as_table().unwrap();
    assert!(
        !g.contains_key("checks"),
        "no checks listed: detection stays"
    );
    assert_eq!(g["quality_checks"].as_array().unwrap().len(), 1);

    // Scan off and no regex rules: nothing for the gate to run.
    let p = parse("[secrets]\nscan = false\n[[rule]]\nid = \"x\"\n").unwrap();
    assert!(to_table(&p, Some("chanto quality check")).is_empty());
}

#[test]
fn rule_defaults_to_new_code_only() {
    let p = parse("[[rule]]\nid = \"x\"\nregex = \"a\"\n").unwrap();
    assert!(p.rules[0].new_code_only);
}

#[test]
fn bad_profile_is_an_error() {
    assert!(parse("[[check]]\nrun = 3\n").is_err());
}

#[test]
fn diff_parsing_collects_added_lines_with_numbers() {
    let diff = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -3,0 +4,2 @@ fn x() {
+    let v = y.unwrap();
+    ok();
@@ -10 +12 @@
-old
+new
diff --git a/gone.rs b/gone.rs
--- a/gone.rs
+++ /dev/null
@@ -1 +0,0 @@
-bye
";
    let m = pattern::parse_diff(diff);
    assert_eq!(
        m["src/a.rs"],
        vec![
            (4, "    let v = y.unwrap();".to_string()),
            (5, "    ok();".to_string()),
            (12, "new".to_string())
        ]
    );
    assert!(!m.contains_key("gone.rs"));
}

#[test]
fn rules_check_new_lines_or_whole_files() {
    let dir = scratch();
    let root = dir.as_path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "old.unwrap();\nfine();\n").unwrap();
    std::fs::write(root.join("src/a_tests.rs"), "t.unwrap();\n").unwrap();
    let mut p = parse(SAMPLE).unwrap();
    p.secrets.scan = false;

    // Only the added line counts.
    let mut changed = std::collections::BTreeMap::new();
    changed.insert(
        "src/a.rs".to_string(),
        vec![(2, "fine();".to_string()), (3, "new.unwrap();".to_string())],
    );
    changed.insert(
        "src/a_tests.rs".to_string(),
        vec![(1, "t.unwrap();".into())],
    );
    let v = pattern::check(&p, root, Some(&changed)).unwrap();
    assert_eq!(
        v,
        vec![pattern::Violation {
            rule: "no-unwrap".into(),
            file: "src/a.rs".into(),
            line: 3,
            text: "new.unwrap();".into()
        }]
    );
    let out = pattern::report(&p, &v);
    assert!(out.failed);
    assert!(
        out.text.contains("src/a.rs:3: new.unwrap();"),
        "{}",
        out.text
    );

    // Whole files: the existing line, but not the excluded test file.
    p.rules[0].new_code_only = false;
    let v = pattern::check(&p, root, Some(&changed)).unwrap();
    assert_eq!(v.len(), 1);
    assert_eq!((v[0].file.as_str(), v[0].line), ("src/a.rs", 1));

    // Nothing broken.
    let ok = pattern::report(&p, &[]);
    assert!(!ok.failed);
    assert!(ok.text.contains("1 rule,"), "{}", ok.text);
}

#[test]
fn bad_regex_names_the_rule() {
    let p = parse("[[rule]]\nid = \"oops\"\nregex = \"(\"\n").unwrap();
    let e = pattern::check(&p, Path::new("."), None).unwrap_err();
    assert!(format!("{e:#}").contains("oops"), "{e:#}");
}

#[test]
fn prime_renders_baseline_into_when() {
    let c = |name: &str, run: &str| prime::Candidate {
        name: name.into(),
        run: run.into(),
        kind: "test",
    };
    let cands = [
        c("tests", "cargo test"),
        c("lint", "npm run lint"),
        c("e2e", "make e2e"),
    ];
    let text = prime::render(&cands, &[(true, 4.0), (false, 1.0), (true, 900.0)]);
    let p = parse(&text).unwrap();
    let when: Vec<When> = p.checks.iter().map(|c| c.when).collect();
    assert_eq!(when, [When::Turn, When::Off, When::Nightly]);
    assert_eq!(p.checks[1].baseline.as_deref(), Some("failing"));
    assert_eq!(p.gate["enabled"].as_bool(), Some(true));
    // The example rule is commented out.
    assert!(p.rules.is_empty());
}

#[test]
fn prime_discovers_rust_and_node_checks() {
    let dir = scratch();
    std::fs::write(
        dir.as_path().join("Cargo.toml"),
        "[package]\nname = \"x\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.as_path().join("package.json"),
        r#"{"scripts":{"test":"jest","lint":"eslint ."}}"#,
    )
    .unwrap();
    let found = prime::discover(dir.as_path());
    let runs: Vec<&str> = found.iter().map(|c| c.run.as_str()).collect();
    assert!(runs.contains(&"cargo test --quiet"), "{runs:?}");
    assert!(runs.contains(&"npm test --silent"), "{runs:?}");
    assert!(runs.contains(&"npm run lint --silent"), "{runs:?}");
    let lint = found
        .iter()
        .find(|c| c.run == "npm run lint --silent")
        .unwrap();
    assert_eq!((lint.name.as_str(), lint.kind), ("lint", "lint"));
}

#[test]
fn profile_is_a_project_layer_only_in_trusted_folders() {
    let dir = scratch();
    std::fs::create_dir_all(dir.join(".chanto")).unwrap();
    std::fs::write(path(&dir), SAMPLE).unwrap();
    let (cfg, _) = crate::config::Config::load(&dir, true).unwrap();
    assert!(cfg.guardrails.enabled);
    assert_eq!(cfg.guardrails.checks, ["cargo test"]);
    assert_eq!(cfg.guardrails.quality_checks.len(), 1);
    assert!(cfg.guardrails.quality_checks[0].ends_with("quality check"));
    assert!(cfg.layers.iter().any(|l| l.ends_with("quality.toml")));

    let (cfg, skipped) = crate::config::Config::load(&dir, false).unwrap();
    assert!(
        !cfg.guardrails.enabled,
        "untrusted: the gate isn't turned on"
    );
    assert!(cfg.guardrails.checks.is_empty());
    assert!(
        skipped.iter().any(|s| s.contains("guardrails")),
        "{skipped:?}"
    );
}

#[test]
fn rules_skip_chanto_files() {
    let dir = scratch();
    std::fs::create_dir_all(dir.join(".chanto")).unwrap();
    std::fs::write(dir.join(".chanto/quality.toml"), "x.unwrap()\n").unwrap();
    let p = parse("[[rule]]\nid = \"u\"\nregex = 'unwrap'\nnew_code_only = false\n").unwrap();
    let mut changed = std::collections::BTreeMap::new();
    changed.insert(
        ".chanto/quality.toml".to_string(),
        vec![(1, "x.unwrap()".to_string())],
    );
    assert!(pattern::check(&p, &dir, Some(&changed)).unwrap().is_empty());
    assert!(pattern::check(&p, &dir, None).unwrap().is_empty());
}

/// A token in a format the secrets detector knows, built at run time so the
/// source holds no secret-shaped literal.
fn fake_token() -> String {
    format!("gh{}_{}", "p", "a1B2c3D4".repeat(5))
}

#[test]
fn secrets_scan_flags_added_lines_without_echoing_them() {
    let dir = scratch();
    let token = fake_token();
    let mut p = parse("").unwrap();
    let mut changed = std::collections::BTreeMap::new();
    changed.insert(
        "src/config.rs".to_string(),
        vec![
            (7, format!("let token = \"{token}\";")),
            (8, "ok();".into()),
        ],
    );
    changed.insert(
        "tests/fixtures/keys.txt".to_string(),
        vec![(1, token.clone())],
    );
    let v = pattern::check(&p, &dir, Some(&changed)).unwrap();
    assert_eq!(v.len(), 2, "{v:?}");
    assert!(v.iter().all(|v| v.rule == pattern::SECRETS));
    let out = pattern::report(&p, &v);
    assert!(out.failed);
    assert!(out.text.contains("src/config.rs:7:"), "{}", out.text);
    assert!(
        !out.text.contains(&token),
        "the report must not echo the secret"
    );
    assert!(out.text.contains("[secrets] exclude"), "{}", out.text);

    // Fixtures excluded; then the scan turned off.
    p.secrets.exclude = vec!["tests/fixtures/**".into()];
    assert_eq!(pattern::check(&p, &dir, Some(&changed)).unwrap().len(), 1);
    p.secrets.scan = false;
    assert!(pattern::check(&p, &dir, Some(&changed)).unwrap().is_empty());
}

#[test]
fn commits_between_two_revisions_are_checked() {
    let dir = scratch();
    let git = |args: &[&str]| {
        let o = std::process::Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgSign=false",
            ])
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    git(&["init", "-q"]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "one"]);
    let base = git(&["rev-parse", "HEAD"]);
    std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
    git(&["commit", "-qam", "two"]);
    std::fs::write(dir.join("a.txt"), "one\ntwo\nuncommitted\n").unwrap();
    let m = pattern::added_between(&dir, &base, "HEAD").unwrap();
    assert_eq!(m["a.txt"], vec![(2, "two".to_string())]);
    assert!(pattern::added_between(&dir, "--output=x", "HEAD").is_none());
    assert!(pattern::added_between(&dir, "nope", "HEAD").is_none());
}

#[test]
fn hook_script_is_valid_sh_and_marked() {
    let s = hook::script("/opt/my tools/chanto");
    assert!(s.starts_with("#!/bin/sh\n# Installed by `chanto prime --hook`"));
    assert!(s.contains("chanto='/opt/my tools/chanto'"), "{s}");
    assert!(s.contains("quality check $checks --base"));
    if cfg!(unix) {
        let f = scratch().join("pre-push");
        std::fs::write(&f, &s).unwrap();
        let o = std::process::Command::new("sh")
            .arg("-n")
            .arg(&f)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
}

#[cfg(unix)]
#[test]
fn hook_install_leaves_someone_elses_hook_alone() {
    let dir = scratch();
    assert!(hook::install(&dir).is_err(), "no profile yet");
    std::fs::create_dir_all(dir.join(".chanto")).unwrap();
    std::fs::write(path(&dir), "version = 1\n").unwrap();
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&dir)
        .output()
        .unwrap();
    let hook = dir.join(".git/hooks/pre-push");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, "#!/bin/sh\nmine\n").unwrap();
    let msg = hook::install(&dir).unwrap();
    assert!(msg.contains("Add this"), "{msg}");
    assert_eq!(std::fs::read_to_string(&hook).unwrap(), "#!/bin/sh\nmine\n");

    // Without one, it's installed (unless global config moves hooks elsewhere).
    std::fs::remove_file(&hook).unwrap();
    let msg = hook::install(&dir).unwrap();
    if msg.starts_with("Installed") {
        let text = std::fs::read_to_string(&hook).unwrap();
        assert!(text.contains("Installed by `chanto prime --hook`"));
        // Running it again replaces Chanto's own hook.
        assert!(hook::install(&dir).unwrap().starts_with("Installed"));
    } else {
        assert!(msg.contains("core.hooksPath"), "{msg}");
    }
}
