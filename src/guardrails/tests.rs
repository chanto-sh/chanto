use super::*;

#[cfg(unix)]
#[tokio::test]
async fn checks_do_not_wait_for_daemons_holding_their_output() {
    let d = std::env::temp_dir();
    // The background `sleep` inherits stdout and stderr and keeps them open.
    let t0 = Instant::now();
    let r = run_check(
        "echo started; sleep 30 & echo to-stderr >&2; exit 3",
        &d,
        Duration::from_secs(20),
    )
    .await;
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "waited for the daemon: {:?}",
        t0.elapsed()
    );
    assert!(!r.ok && !timed_out(&r), "{r:?}");
    assert!(
        r.output.contains("started") && r.output.contains("to-stderr"),
        "{r:?}"
    );
    // Output written just after exit by a straggler still arrives within the grace period.
    let r = run_check(
        "(sleep 0.05; echo late) & exit 0",
        &d,
        Duration::from_secs(20),
    )
    .await;
    assert!(r.ok && r.output.contains("late"), "{r:?}");
    // A main process that outlives the limit is still timed out.
    let r = run_check("sleep 30", &d, Duration::from_millis(300)).await;
    assert!(timed_out(&r), "{r:?}");
}

#[test]
fn covers_only_plain_full_runs_of_the_same_runner() {
    assert!(covers("cargo test", "cargo test --quiet"));
    assert!(covers(
        "cd /x/proj && cargo test --quiet 2>&1",
        "cargo test --quiet"
    ));
    assert!(covers("python3 -m pytest -q", "python3 -m pytest -q -rfE"));
    assert!(covers(
        "go vet ./... && go test ./...",
        "go vet ./... && go test ./..."
    ));
    assert!(
        !covers("cargo test 2>&1 | tail -5", "cargo test --quiet"),
        "a pipe hides the exit status"
    );
    assert!(
        !covers("cargo test parse_", "cargo test --quiet"),
        "filtered to some tests"
    );
    assert!(!covers(
        "python3 -m pytest -k login",
        "python3 -m pytest -q -rfE"
    ));
    assert!(
        !covers("go test ./pkg/...", "go vet ./... && go test ./..."),
        "different scope, and go vet not run"
    );
    assert!(!covers("cargo test || true", "cargo test"));
    assert!(!covers("npm test", "cargo test"));
    assert!(covers(
        "python3 -m unittest discover -q -p '*test*.py'",
        "python3 -m unittest discover -q -p '*test*.py'"
    ));
    assert!(
        !covers("cargo test -p core", "cargo test"),
        "one package only"
    );
}

#[test]
fn names_failing_tests_and_splits_pre_existing() {
    let py = "FAILED tests/test_a.py::test_one - assert 1 == 2\nFAILED tests/test_b.py::test_two\n";
    assert_eq!(failing_tests(py).unwrap().len(), 2);
    assert!(
        failing_tests("test parse::empty ... FAILED\n")
            .unwrap()
            .contains("parse::empty")
    );
    assert!(
        failing_tests("    --- FAIL: TestLRU (0.00s)\n")
            .unwrap()
            .contains("TestLRU")
    );
    assert!(
        failing_tests("FAIL: test_add (test_calc.T)\n")
            .unwrap()
            .contains("test_add (test_calc.T)")
    );
    assert!(failing_tests("all good").is_none());
    assert!(
        ran_no_tests("----\nRan 0 tests in 0.000s\n\nNO TESTS RAN\n")
            && ran_no_tests("collected 0 items")
            && !ran_no_tests("FAILED (failures=1)")
    );
    let r = |out: &str, ok: bool| CheckResult {
        command: "pytest".into(),
        ok,
        output: out.into(),
        secs: 0.0,
    };
    let before = vec![r("FAILED t.py::old\n", false)];
    let (new, old) =
        pre_existing(&r("FAILED t.py::old\nFAILED t.py::mine\n", false), &before).unwrap();
    assert_eq!(
        (new, old),
        (
            vec!["t.py::mine".to_string()],
            vec!["t.py::old".to_string()]
        )
    );
    assert!(
        pre_existing(&r("FAILED t.py::x\n", false), &[r("", true)]).is_none(),
        "passed before: all new"
    );
    // Output that names no tests: the check itself is what was already failing.
    assert_eq!(
        pre_existing(&r("build broke", false), &[r("build broke", false)]),
        Some((vec![], vec!["`pytest`".to_string()]))
    );
}

#[test]
fn detects_test_tampering() {
    let d = std::env::temp_dir().join(format!("chanto-guard-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(d.join("tests")).unwrap();
    let t = d.join("test_math.py");
    std::fs::write(
        &t,
        "def test_a():\n    assert add(1, 2) == 3\n    assert add(0, 0) == 0\n",
    )
    .unwrap();
    std::fs::write(
        d.join("tests/b_test.go"),
        "func TestB(t *testing.T) { if f() != 2 { t.Fatal(\"x\") } }\n",
    )
    .unwrap();
    std::fs::write(d.join("math.py"), "x").unwrap();
    let s = snapshot(&d);
    assert_eq!(s.files.len(), 2);
    assert!(s.violations().is_empty());
    std::fs::write(
        &t,
        "import pytest\n@pytest.mark.skip\ndef test_a():\n    assert add(1, 2) == 3\n",
    )
    .unwrap();
    let v = s.violations().join("\n");
    assert!(
        v.contains("1 assertion(s) removed") && v.contains("skip marker"),
        "{v}"
    );
    std::fs::write(
        &t,
        "def test_a():\n    assert add(1, 2) == 4\n    assert add(0, 0) == 0\n",
    )
    .unwrap();
    assert!(s.violations().join("\n").contains("changed"));
    std::fs::remove_file(d.join("tests/b_test.go")).unwrap();
    assert!(s.violations().join("\n").contains("deleted"));
    // Adding a new test file or new tests is fine.
    std::fs::write(&t, "def test_a():\n    assert add(1, 2) == 3\n    assert add(0, 0) == 0\n\ndef test_c():\n    assert add(2, 2) == 4\n").unwrap();
    std::fs::write(
        d.join("tests/b_test.go"),
        "func TestB(t *testing.T) { if f() != 2 { t.Fatal(\"x\") } }\n",
    )
    .unwrap();
    std::fs::write(d.join("test_new.py"), "def test_n():\n    assert True\n").unwrap();
    assert!(s.violations().is_empty(), "{:?}", s.violations());
}

#[test]
fn changing_an_existing_test_body_is_a_violation() {
    let d = std::env::temp_dir().join(format!(
        "chanto-guard-body-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&d).unwrap();
    let t = d.join("test_mod.py");
    let before = "def test_a():\n    assert f(1) == 2\n\n\ndef test_b():\n    data = load(\"a b\")\n    assert parse(data) == [\"a\", \"b\"]\n";
    std::fs::write(&t, before).unwrap();
    let s = snapshot(&d);
    // The same assertion lines, fed different input: the old checks can't
    // see it, but the test no longer exercises what it used to.
    std::fs::write(
        &t,
        "def test_a():\n    assert f(1) == 2\n\n\ndef test_b():\n    data = load(\"a-b\")\n    assert parse(data) == [\"a\", \"b\"]\n",
    )
    .unwrap();
    let v = s.violations().join("\n");
    assert!(v.contains("test_b (existing test) changed"), "{v}");
    // A pure reformat is not a change.
    std::fs::write(
        &t,
        "def test_a():\n\n    assert f(1) == 2\n\n\ndef test_b():\n    data = load(\"a b\")\n    assert parse(data) == [\"a\", \"b\"]\n",
    )
    .unwrap();
    assert!(s.violations().is_empty(), "{:?}", s.violations());
    // Adding tests alongside, and removing one outright.
    std::fs::write(
        &t,
        "def test_a():\n    assert f(1) == 2\n\n\ndef test_b():\n    data = load(\"a b\")\n    assert parse(data) == [\"a\", \"b\"]\n\n\ndef test_c():\n    assert True\n",
    )
    .unwrap();
    assert!(s.violations().is_empty(), "{:?}", s.violations());
    std::fs::write(&t, "def test_c():\n    assert True\n").unwrap();
    let v = s.violations().join("\n");
    assert!(
        v.contains("test_a (existing test) removed")
            && v.contains("test_b (existing test) removed"),
        "{v}"
    );
}

#[test]
fn collection_errors_still_fail_the_gate() {
    // pytest prints "no tests ran" when collection itself breaks: a genuine
    // failure, not a benign empty run, so it must not pass the gate.
    let err = "ERROR collecting test_x.py\nImportError: no module named y\n\
                   !!!!!!!! Interrupted: 1 error during collection !!!!!!!!\nno tests ran in 0.01s\n";
    assert!(!ran_no_tests(err), "a collection error is a real failure");
    assert!(!ran_no_tests(
        "Traceback (most recent call last):\n  ...\ncollected 0 items in 0.01s\n"
    ));
    assert!(!ran_no_tests(
        "cannot import name 'x'\nno tests ran in 0.02s\n"
    ));
    // A genuinely empty run (nothing to collect) is benign.
    assert!(ran_no_tests("collected 0 items in 0.01s"));
    assert!(ran_no_tests("Ran 0 tests in 0.000s\n\nNO TESTS RAN\n"));
}

#[test]
fn protects_inline_rust_tests_in_source_files() {
    let d = std::env::temp_dir().join(format!(
        "chanto-guard-inline-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(d.join("src")).unwrap();
    let f = d.join("src/foo.rs");
    let good = "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn \
                    adds() {\n        assert_eq!(super::add(1, 2), 3);\n        assert_eq!(super::add(0, 0), 0);\n    }\n}\n";
    std::fs::write(&f, good).unwrap();
    let s = snapshot(&d);
    assert!(
        s.files.contains_key(&f),
        "inline tests in src/*.rs must be snapshotted"
    );
    assert!(s.violations().is_empty());
    // Weakening an inline test is caught just like a separate test file.
    let weak = good.replace("        assert_eq!(super::add(0, 0), 0);\n", "");
    std::fs::write(&f, weak).unwrap();
    let v = s.violations().join("\n");
    assert!(v.contains("assertion(s) removed"), "{v}");
    // A plain source file with no tests stays out of the snapshot.
    let plain = d.join("src/plain.rs");
    std::fs::write(&plain, "pub const X: i32 = 1;\n").unwrap();
    assert!(!snapshot(&d).files.contains_key(&plain));
}

fn repo(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "chanto-scope-{name}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    for (p, t) in files {
        let p = d.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, t).unwrap();
    }
    d
}

fn changed(paths: &[&str]) -> Vec<Changed> {
    paths
        .iter()
        .map(|p| Changed {
            path: PathBuf::from(p),
            new: false,
        })
        .collect()
}

fn commands(p: &Plan) -> Vec<String> {
    p.runs.iter().map(|r| r.command.clone()).collect()
}

const PYTEST: &str = "python3 -m pytest -q -rfE";

fn python_repo() -> PathBuf {
    repo(
        "py",
        &[
            ("pkg/__init__.py", ""),
            ("pkg/mod.py", "def f():\n    return 1\n"),
            ("pkg/other.py", "from pkg.mod import f\n"),
            ("pkg/lonely.py", "X = 1\n"),
            ("tests/test_mod.py", "def test_f():\n    assert True\n"),
            (
                "tests/test_uses.py",
                "from pkg import (\n    other,\n    mod,\n)\n",
            ),
            ("tests/test_dotted.py", "import pkg.mod as m\n"),
            ("tests/test_unrelated.py", "from pkg.other import f\n"),
            ("tests/conftest.py", ""),
            ("README.md", "# x\n"),
        ],
    )
}

#[test]
fn python_related_tests_by_name_and_import() {
    let d = python_repo();
    let p = plan(
        &[PYTEST.into()],
        &d,
        Some(&changed(&["pkg/mod.py"])),
        Scope::Related,
        false,
    );
    assert_eq!(
        commands(&p),
        vec![format!(
            "{PYTEST} tests/test_dotted.py tests/test_mod.py tests/test_uses.py"
        )]
    );
    assert!(p.note.is_none() && p.runs[0].is_scoped());
    // A changed test file runs itself; docs don't count; absolute paths work.
    let abs = Changed {
        path: d.join("tests/test_unrelated.py"),
        new: false,
    };
    let p = plan(
        &[PYTEST.into()],
        &d,
        Some(&[
            abs,
            Changed {
                path: "README.md".into(),
                new: false,
            },
        ]),
        Scope::Related,
        false,
    );
    assert_eq!(
        commands(&p),
        vec![format!("{PYTEST} tests/test_unrelated.py")]
    );
    // A new script nothing imports (a reproduction) doesn't widen the selection.
    let mut c = changed(&["pkg/mod.py"]);
    c.push(Changed {
        path: "repro.py".into(),
        new: true,
    });
    assert_eq!(
        commands(&plan(&[PYTEST.into()], &d, Some(&c), Scope::Related, false)).len(),
        1
    );
    assert!(plan(&[PYTEST.into()], &d, Some(&c), Scope::Related, false).runs[0].is_scoped());
    // An existing module no test mentions: unsure, so the full suite.
    let p = plan(
        &[PYTEST.into()],
        &d,
        Some(&changed(&["pkg/lonely.py"])),
        Scope::Related,
        false,
    );
    assert_eq!(commands(&p), vec![PYTEST.to_string()]);
    assert!(
        p.note
            .as_deref()
            .unwrap()
            .contains("no tests found for `pkg/lonely.py`"),
        "{:?}",
        p.note
    );
    // A check that isn't a plain test run (or already narrowed) stays as is.
    let p = plan(
        &[
            "make test".into(),
            "python3 -m pytest tests/test_mod.py".into(),
        ],
        &d,
        Some(&changed(&["pkg/mod.py"])),
        Scope::Related,
        false,
    );
    assert_eq!(
        commands(&p),
        vec![
            "make test".to_string(),
            "python3 -m pytest tests/test_mod.py".to_string()
        ]
    );
}

#[test]
fn python_related_falls_back_to_tests_named_after_the_package() {
    // pytest's own layout: `src/_pytest/mark/evaluate.py` is imported by
    // nobody in the test suite by name, but `testing/test_mark.py` covers
    // it. Without the fallback the gate runs all 2,800 tests.
    let d = repo(
        "py-pytest",
        &[
            ("src/_pytest/__init__.py", ""),
            (
                "src/_pytest/mark/__init__.py",
                "from .evaluate import _istrue\n",
            ),
            (
                "src/_pytest/mark/evaluate.py",
                "def _istrue():\n    return True\n",
            ),
            ("src/_pytest/skipping.py", "X = 1\n"),
            ("testing/test_mark.py", "from _pytest.mark import _istrue\n"),
            (
                "testing/test_skipping.py",
                "from _pytest.mark import _istrue\n",
            ),
            ("testing/test_terminal.py", "import pytest\n"),
        ],
    );
    let p = plan(
        &[PYTEST.into()],
        &d,
        Some(&changed(&["src/_pytest/mark/evaluate.py"])),
        Scope::Related,
        false,
    );
    // Named after the package (`test_mark.py`) and importing the package that
    // re-exports it (`test_skipping.py`) — but not the unrelated test.
    assert_eq!(
        commands(&p),
        vec![format!(
            "{PYTEST} testing/test_mark.py testing/test_skipping.py"
        )]
    );
    assert!(p.runs[0].is_scoped(), "{:?}", p.note);
    // A module whose name matches no test and no re-exporting package still
    // escalates to the full suite.
    let p = plan(
        &[PYTEST.into()],
        &d,
        Some(&changed(&["src/_pytest/never_imported.py"])),
        Scope::Related,
        false,
    );
    assert_eq!(commands(&p), vec![PYTEST.to_string()]);
}

#[test]
fn shared_setup_and_wide_changes_escalate_to_the_full_suite() {
    let d = python_repo();
    for (files, why) in [
        (
            vec!["tests/conftest.py", "pkg/mod.py"],
            "`tests/conftest.py` changed",
        ),
        (vec!["pyproject.toml"], "`pyproject.toml` changed"),
        (vec![".github/workflows/ci.yml"], "ci.yml` changed"),
        (vec!["Cargo.toml"], "`Cargo.toml` changed"),
        (vec!["package.json"], "`package.json` changed"),
        (vec!["go.mod"], "`go.mod` changed"),
        (vec!["pkg/data.json"], "isn't Python"),
    ] {
        let p = plan(
            &[PYTEST.into()],
            &d,
            Some(&changed(&files)),
            Scope::Related,
            false,
        );
        assert_eq!(commands(&p), vec![PYTEST.to_string()], "{files:?}");
        assert!(
            p.note.as_deref().is_some_and(|n| n.contains(why)),
            "{files:?}: {:?}",
            p.note
        );
    }
    let many: Vec<String> = (0..21).map(|i| format!("pkg/m{i}.py")).collect();
    let p = plan(
        &[PYTEST.into()],
        &d,
        Some(&changed(
            &many.iter().map(String::as_str).collect::<Vec<_>>(),
        )),
        Scope::Related,
        false,
    );
    assert_eq!(
        (commands(&p), p.note.as_deref()),
        (vec![PYTEST.to_string()], Some("21 files changed"))
    );
    let p = plan(&[PYTEST.into()], &d, None, Scope::Related, false);
    assert_eq!(
        (commands(&p), p.note.as_deref()),
        (
            vec![PYTEST.to_string()],
            Some("couldn't tell which files changed")
        )
    );
    // Forced full, a fast full suite, or no changes: everything, nothing to explain.
    for (scope, fast, files) in [
        (Scope::Full, false, vec!["pkg/mod.py"]),
        (Scope::Related, true, vec!["pkg/mod.py"]),
        (Scope::Related, false, vec![]),
    ] {
        let p = plan(&[PYTEST.into()], &d, Some(&changed(&files)), scope, fast);
        assert_eq!((commands(&p), p.note), (vec![PYTEST.to_string()], None));
    }
    assert!(small_project(&d));
    let big: Vec<(String, &str)> = (0..=SMALL_PROJECT_TESTS)
        .map(|i| (format!("tests/test_{i}.py"), ""))
        .collect();
    assert!(!small_project(&repo(
        "big",
        &big.iter()
            .map(|(p, t)| (p.as_str(), *t))
            .collect::<Vec<_>>()
    )));
}

#[test]
fn scope_config() {
    let raw: toml::Table = "enabled = true\nscope = \"full\"\n".parse().unwrap();
    let mut c = GuardrailsConfig::default();
    c.merge(raw.clone().try_into().unwrap(), &raw);
    assert_eq!(c.scope, Some(Scope::Full));
    let raw: toml::Table = "checks = [\"make test\"]\n".parse().unwrap();
    c.merge(raw.clone().try_into().unwrap(), &raw);
    assert_eq!(c.scope, Some(Scope::Full), "a layer without scope keeps it");
    assert!(
        "scope = \"partial\""
            .parse::<toml::Table>()
            .unwrap()
            .try_into::<GuardrailsConfig>()
            .is_err()
    );
    assert_eq!(
        (
            Scope::parse("related"),
            Scope::parse("full"),
            Scope::parse("x")
        ),
        (Some(Scope::Related), Some(Scope::Full), None)
    );
}

#[test]
fn unittest_selection_names_modules() {
    let d = python_repo();
    let check = "python3 -m unittest discover -q -p '*test*.py'";
    let p = plan(
        &[check.into()],
        &d,
        Some(&changed(&["tests/test_mod.py", "pkg/other.py"])),
        Scope::Related,
        false,
    );
    assert_eq!(
        commands(&p),
        vec![
            "python3 -m unittest -q tests.test_mod tests.test_unrelated tests.test_uses"
                .to_string()
        ]
    );
    // A start directory changes how modules are named: leave it alone.
    let p = plan(
        &["python3 -m unittest discover -s tests".into()],
        &d,
        Some(&changed(&["pkg/mod.py"])),
        Scope::Related,
        false,
    );
    assert!(!p.runs[0].is_scoped());
}

#[test]
fn cargo_workspace_selects_changed_crates_and_dependents() {
    let d = repo(
        "rs",
        &[
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
            ("crates/core/Cargo.toml", "[package]\nname = \"core-lib\"\n"),
            ("crates/core/src/lib.rs", ""),
            (
                "crates/app/Cargo.toml",
                "[package]\nname = \"app\"\n[dependencies]\ncore-lib = { path = \"../core\" }\n",
            ),
            ("crates/app/src/main.rs", ""),
            ("crates/other/Cargo.toml", "[package]\nname = \"other\"\n"),
            ("crates/other/src/lib.rs", ""),
        ],
    );
    let p = plan(
        &["cargo test --quiet".into()],
        &d,
        Some(&changed(&["crates/core/src/lib.rs"])),
        Scope::Related,
        false,
    );
    assert_eq!(
        commands(&p),
        vec!["cargo test --quiet -p app -p core-lib".to_string()]
    );
    let p = plan(
        &["cargo test --quiet".into()],
        &d,
        Some(&changed(&["crates/other/src/lib.rs"])),
        Scope::Related,
        false,
    );
    assert_eq!(
        commands(&p),
        vec!["cargo test --quiet -p other".to_string()]
    );
    // A single crate: its whole suite is the related one.
    let single = repo(
        "rs1",
        &[
            ("Cargo.toml", "[package]\nname = \"one\"\n"),
            ("src/lib.rs", ""),
        ],
    );
    let p = plan(
        &["cargo test --quiet".into()],
        &single,
        Some(&changed(&["src/lib.rs"])),
        Scope::Related,
        false,
    );
    assert_eq!(
        (commands(&p), p.note),
        (vec!["cargo test --quiet".to_string()], None)
    );
}

#[test]
fn go_selects_changed_packages_and_importers() {
    let d = repo(
        "go",
        &[
            ("go.mod", "module example.com/m\n\ngo 1.22\n"),
            ("pkg/x/x.go", "package x\n"),
            ("pkg/x/sub/s.go", "package sub\n"),
            (
                "pkg/y/y.go",
                "package y\n\nimport \"example.com/m/pkg/x\"\n",
            ),
            ("cmd/z/z.go", "package main\n"),
        ],
    );
    let check = "go vet ./... && go test ./...";
    let p = plan(
        &[check.into()],
        &d,
        Some(&changed(&["pkg/x/x.go"])),
        Scope::Related,
        false,
    );
    assert_eq!(
        commands(&p),
        vec!["go vet ./pkg/x/... ./pkg/y && go test ./pkg/x/... ./pkg/y".to_string()]
    );
    // The root package: `./...` is the related set already.
    let p = plan(
        &[check.into()],
        &d,
        Some(&changed(&["main.go"])),
        Scope::Related,
        false,
    );
    assert_eq!((commands(&p), p.note), (vec![check.to_string()], None));
}

#[test]
fn jest_and_vitest_use_their_related_modes() {
    let jest = repo(
        "jest",
        &[
            ("package.json", r#"{"scripts": {"test": "jest --ci"}}"#),
            ("src/a.js", ""),
            ("src/a.test.js", ""),
        ],
    );
    let p = plan(
        &["npm test --silent".into()],
        &jest,
        Some(&changed(&["src/a.js"])),
        Scope::Related,
        false,
    );
    assert_eq!(
        commands(&p),
        vec!["npx jest --findRelatedTests src/a.js".to_string()]
    );
    let vitest = repo(
        "vitest",
        &[
            ("package.json", r#"{"scripts": {"test": "vitest run"}}"#),
            ("src/a.ts", ""),
        ],
    );
    let p = plan(
        &[
            "npm test --silent".into(),
            "npm run typecheck --silent".into(),
        ],
        &vitest,
        Some(&changed(&["src/a.ts"])),
        Scope::Related,
        false,
    );
    assert_eq!(
        commands(&p),
        vec![
            "npx vitest related --run src/a.ts".to_string(),
            "npm run typecheck --silent".to_string()
        ]
    );
    let mocha = repo(
        "mocha",
        &[
            ("package.json", r#"{"scripts": {"test": "mocha"}}"#),
            ("src/a.js", ""),
        ],
    );
    assert!(
        !plan(
            &["npm test --silent".into()],
            &mocha,
            Some(&changed(&["src/a.js"])),
            Scope::Related,
            false
        )
        .runs[0]
            .is_scoped()
    );
}

#[test]
fn covers_counts_supersets_of_a_selection_not_subsets() {
    let sel = "python3 -m pytest -q -rfE tests/test_a.py tests/lint/test_b.py";
    assert!(covers(
        "python -m pytest tests/test_a.py tests/lint/test_b.py tests/test_c.py",
        sel
    ));
    assert!(covers("pytest tests", sel), "a directory holds its files");
    assert!(covers("python3 -m pytest", sel), "the whole suite");
    assert!(
        !covers("python3 -m pytest tests/test_a.py", sel),
        "only part of the selection"
    );
    assert!(
        !covers(
            "python3 -m pytest tests/test_a.py::test_x tests/lint/test_b.py",
            sel
        ),
        "one test of a file"
    );
    assert!(!covers("python3 -m pytest tests -k slow", sel));
    assert!(
        !covers("python3 -m pytest --collect-only tests", sel),
        "collecting isn't running"
    );
    assert!(
        !covers(
            "python3 -m pytest tests/test_a.py",
            "python3 -m pytest -q -rfE"
        ),
        "a selection doesn't cover the full suite"
    );
    assert!(covers(
        "cargo test -p a -p b -p c",
        "cargo test --quiet -p a -p b"
    ));
    assert!(covers("cargo test --workspace", "cargo test --quiet -p a"));
    assert!(!covers("cargo test -p a", "cargo test --quiet -p a -p b"));
    assert!(
        !covers("cargo test -p a -p b parse", "cargo test --quiet -p a -p b"),
        "filtered by name"
    );
    assert!(
        !covers("cargo test --no-run", "cargo test"),
        "compiling isn't testing"
    );
    assert!(covers(
        "go vet ./... && go test ./...",
        "go vet ./pkg/x/... ./pkg/y && go test ./pkg/x/... ./pkg/y"
    ));
    assert!(covers(
        "go vet ./pkg/... && go test ./pkg/...",
        "go vet ./pkg/x/... ./pkg/y && go test ./pkg/x/... ./pkg/y"
    ));
    assert!(
        !covers(
            "go test ./pkg/x/...",
            "go vet ./pkg/x/... && go test ./pkg/x/..."
        ),
        "go vet not run"
    );
    assert!(!covers(
        "go vet ./pkg/x/... && go test ./pkg/x/...",
        "go vet ./pkg/x/... ./pkg/y && go test ./pkg/x/... ./pkg/y"
    ));
    assert!(covers(
        "python3 -m unittest tests.test_a tests.test_b",
        "python3 -m unittest -q tests.test_a"
    ));
    assert!(!covers(
        "python3 -m unittest tests.test_a.T.test_x",
        "python3 -m unittest -q tests.test_a"
    ));
    assert!(covers("npx jest", "npx jest --findRelatedTests src/a.js"));
    assert!(covers(
        "npx jest --findRelatedTests src/a.js src/b.js",
        "npx jest --findRelatedTests src/a.js"
    ));
    assert!(!covers(
        "npx jest src/b.test.js",
        "npx jest --findRelatedTests src/a.js"
    ));
    assert!(covers(
        "npx vitest run",
        "npx vitest related --run src/a.ts"
    ));
    assert!(!covers(
        "npx vitest related --run src/a.ts",
        "npx vitest run"
    ));
}

#[test]
fn the_guard_puts_changes_back_when_chanto_dies_mid_run() {
    use base64::Engine;
    let d = repo("guard", &[("a.txt", "original\n")]);
    let journal = put_back_journal(&d.join(".journal"), &d);
    let edited = base64::engine::general_purpose::STANDARD.encode("edited\n");
    let write_journal = || {
        std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
        std::fs::write(
            &journal,
            serde_json::to_vec(&Journal {
                root: d.clone(),
                files: vec![(d.join("a.txt"), Some(edited.clone()))],
            })
            .unwrap(),
        )
        .unwrap();
    };
    // The owner dies (killed, reaped) with the originals on disk.
    write_journal();
    let mut owner = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = owner.id();
    owner.kill().unwrap();
    owner.wait().unwrap();
    assert!(guard(&journal, pid));
    assert_eq!(
        std::fs::read_to_string(d.join("a.txt")).unwrap(),
        "edited\n"
    );
    assert!(!journal.exists());
    // The owner finishes normally: it removes the journal and the guard leaves the files alone.
    std::fs::write(d.join("a.txt"), "kept\n").unwrap();
    write_journal();
    let j = journal.clone();
    let done = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        std::fs::remove_file(j).unwrap();
    });
    assert!(!guard(&journal, std::process::id()));
    done.join().unwrap();
    assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "kept\n");
}

#[test]
fn old_pytest_failures_are_named_and_matched_to_the_baseline() {
    // Older pytest prints no "FAILED x::y" summary, only section headers.
    let out = |names: &[&str]| {
        format!(
            "....F.F\n=== FAILURES ===\n{}\n2 failed, 5 passed",
            names
                .iter()
                .map(|n| format!("______ {n} ______\nassert 0"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    let before = CheckResult {
        command: "python3 -m pytest -q -rfE".into(),
        ok: false,
        output: out(&["test_code_highlight[with markup]"]),
        secs: 1.0,
    };
    let now = CheckResult {
        output: out(&["test_code_highlight[with markup]", "test_new_thing"]),
        ..before.clone()
    };
    let (new, old) = pre_existing(&now, &[before]).unwrap();
    assert_eq!(new, ["test_new_thing"]);
    assert_eq!(old, ["test_code_highlight[with markup]"]);
}

#[tokio::test]
async fn baseline_runs_on_the_original_files_and_puts_changes_back() {
    let d = repo("orig", &[("a.txt", "changed\n"), ("new.txt", "new\n")]);
    let mut originals = HashMap::new();
    originals.insert(d.join("a.txt"), Some(b"original\n".to_vec()));
    originals.insert(d.join("new.txt"), None);
    originals.insert(d.join("gone.txt"), Some(b"was here\n".to_vec()));
    let journal = put_back_journal(&d.join(".journal"), &d);
    let r = run_on_originals(
        "cat a.txt gone.txt; test ! -e new.txt",
        &d,
        Duration::from_secs(30),
        &originals,
        &journal,
    )
    .await
    .unwrap();
    assert!(
        r.ok && r.output.contains("original") && r.output.contains("was here"),
        "{}",
        r.output
    );
    assert_eq!(
        std::fs::read_to_string(d.join("a.txt")).unwrap(),
        "changed\n"
    );
    assert_eq!(std::fs::read_to_string(d.join("new.txt")).unwrap(), "new\n");
    assert!(!d.join("gone.txt").exists());
    assert!(
        !journal.exists(),
        "journal removed once the changes are back"
    );
    // Killed mid-run: the journal is still there and puts the changes back.
    let run = tokio::spawn({
        let (d, originals, journal) = (d.clone(), originals.clone(), journal.clone());
        async move {
            run_on_originals(
                "sleep 30",
                &d,
                Duration::from_secs(60),
                &originals,
                &journal,
            )
            .await
        }
    });
    for _ in 0..100 {
        if std::fs::read_to_string(d.join("a.txt")).unwrap() == "original\n" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        std::fs::read_to_string(d.join("a.txt")).unwrap(),
        "original\n"
    );
    let saved = std::fs::read(&journal).unwrap();
    run.abort();
    let _ = run.await;
    // Simulate a kill: originals back on disk, journal left behind.
    std::fs::write(d.join("a.txt"), "original\n").unwrap();
    std::fs::remove_file(d.join("new.txt")).unwrap();
    std::fs::write(d.join("gone.txt"), "was here\n").unwrap();
    std::fs::write(&journal, saved).unwrap();
    assert!(
        recover_put_backs(&d.join(".journal"), &d.join("elsewhere")).is_empty(),
        "other projects' journals are left alone"
    );
    assert_eq!(recover_put_backs(&d.join(".journal"), &d).len(), 3);
    assert_eq!(
        std::fs::read_to_string(d.join("a.txt")).unwrap(),
        "changed\n"
    );
    assert_eq!(std::fs::read_to_string(d.join("new.txt")).unwrap(), "new\n");
    assert!(!d.join("gone.txt").exists() && !journal.exists());
    assert!(recover_put_back(&journal).is_empty());
    // The baseline selection leaves out test files that are new this turn.
    let py = python_repo();
    let p = plan(
        &[PYTEST.into()],
        &py,
        Some(&changed(&["tests/test_mod.py", "tests/test_unrelated.py"])),
        Scope::Related,
        false,
    );
    let mut originals = HashMap::new();
    originals.insert(
        crate::permissions::normalize(&py.join("tests/test_unrelated.py")),
        None,
    );
    assert_eq!(
        p.runs[0].without_new_files(&originals, &py).unwrap(),
        format!("{PYTEST} tests/test_mod.py")
    );
}

#[tokio::test]
async fn gate_reports_the_scope_it_verified() {
    let d = repo(
        "gate",
        &[
            ("calc.py", "def add(a, b):\n    return a + b\n"),
            (
                "tests/test_calc.py",
                "import unittest\nfrom calc import add\nclass T(unittest.TestCase):\n    def test_add(self):\n        self.assertEqual(add(2, 3), 5)\n",
            ),
            (
                "tests/test_other.py",
                "import unittest\nclass T(unittest.TestCase):\n    def test_x(self):\n        self.assertTrue(True)\n",
            ),
            // A package, so `unittest discover` from the root finds the tests.
            ("tests/__init__.py", ""),
        ],
    );
    let check = "python3 -m unittest discover -q -p 'test_*.py'".to_string();
    let t = Duration::from_secs(60);
    let p = plan(
        std::slice::from_ref(&check),
        &d,
        Some(&changed(&["calc.py"])),
        Scope::Related,
        false,
    );
    assert_eq!(
        commands(&p),
        vec!["python3 -m unittest -q tests.test_calc".to_string()]
    );
    let (Gate::Passed(e), ran) = gate(&p, &[], &[], None, &d, t).await else {
        panic!("gate failed")
    };
    assert!(
        e.starts_with("`python3 -m unittest -q tests.test_calc` (")
            && e.ends_with("passed — related to your changes; full suite not run"),
        "{e}"
    );
    assert_eq!(ran.len(), 1);
    let (Gate::Passed(e), _) = gate(&p, &[p.runs[0].command.clone()], &[], None, &d, t).await
    else {
        panic!()
    };
    assert!(
        e.contains("(your run, no edits since)") && e.contains("full suite not run"),
        "{e}"
    );
    // Escalated: says why the full suite ran.
    let p = plan(
        std::slice::from_ref(&check),
        &d,
        Some(&changed(&["tox.ini"])),
        Scope::Related,
        false,
    );
    let (Gate::Passed(e), _) = gate(&p, &[], &[], None, &d, t).await else {
        panic!()
    };
    assert!(
        e.contains(&format!("`{check}`")) && e.ends_with("passed (full suite: `tox.ini` changed)"),
        "{e}"
    );
    // A selection that can't run on its own falls back to the full suite.
    let mut p = plan(
        std::slice::from_ref(&check),
        &d,
        Some(&changed(&["calc.py"])),
        Scope::Related,
        false,
    );
    p.runs[0] = Run::scoped(
        &check,
        vec!["python3 -m unittest -q".into()],
        vec![(
            "tests.test_missing".into(),
            Some("tests/test_missing.py".into()),
        )],
        false,
    );
    let (Gate::Passed(e), ran) = gate(&p, &[], &[], None, &d, t).await else {
        panic!()
    };
    assert_eq!(ran.len(), 2);
    assert!(
        e.contains(&format!("`{check}`")) && e.contains("couldn't be run on their own"),
        "{e}"
    );
    // A check that runs no tests at all verifies nothing.
    let empty = repo(
        "gate-empty",
        &[("calc.py", "def add(a, b):\n    return a + b\n")],
    );
    let p = plan(
        std::slice::from_ref(&check),
        &empty,
        Some(&changed(&["calc.py"])),
        Scope::Full,
        false,
    );
    let (Gate::NoTests(why), _) = gate(&p, &[], &[], None, &empty, t).await else {
        panic!("a run of no tests passed the gate")
    };
    assert_eq!(why, format!("`{check}` found no tests to run"));
}

#[tokio::test]
async fn gate_reuses_the_agent_own_failing_run() {
    let d = repo(
        "gate-reuse",
        &[
            (
                "tests/test_x.py",
                "import unittest\nclass T(unittest.TestCase):\n    def test_ok(self):\n        self.assertTrue(True)\n",
            ),
            ("tests/__init__.py", ""),
        ],
    );
    let check = "python3 -m unittest discover -q -p 'test_*.py'".to_string();
    let t = Duration::from_secs(60);
    let p = plan(
        std::slice::from_ref(&check),
        &d,
        Some(&changed(&["x.py"])),
        Scope::Full,
        false,
    );
    // The agent already ran this check and it failed with this output. The
    // gate must judge that run, not repeat the suite (here the suite would
    // pass, so a repeated run is observable as a Passed gate).
    let mine = CheckResult {
        command: check.clone(),
        ok: false,
        output: "FAILED tests/test_x.py::test_broke - AssertionError\n".into(),
        secs: 0.0,
    };
    let (g, ran) = gate(&p, &[], std::slice::from_ref(&mine), None, &d, t).await;
    let Gate::Failed(feedback, failed) = g else {
        panic!("the reused failure didn't fail the gate")
    };
    assert!(feedback.contains("test_broke"), "{feedback}");
    assert_eq!(failed.map(|f| f.command), Some(check.clone()));
    assert_eq!(ran.len(), 1);
    assert_eq!(ran[0].command, check);
    // With nothing to reuse the gate runs the check itself, and it passes.
    let (g, ran) = gate(&p, &[], &[], None, &d, t).await;
    assert!(matches!(g, Gate::Passed(_)), "fresh run should pass");
    assert_eq!(ran.len(), 1);
}

#[test]
fn test_file_patterns() {
    for p in [
        "test_x.py",
        "x_test.go",
        "a.test.ts",
        "b.spec.js",
        "tests/integration.rs",
        "src/__tests__/a.js",
        "FooTest.java",
        // pytest's suite lives under `testing/`, not `tests/`.
        "testing/python/fixtures.py",
        "test_utils/helpers.rb",
    ] {
        assert!(is_test_file(Path::new(p)), "{p}");
    }
    for p in [
        "src/main.rs",
        "testing.md",
        "contest.py",
        "latest.go",
        "src/attestation.py",
    ] {
        assert!(!is_test_file(Path::new(p)), "{p}");
    }
}
