use super::*;

#[test]
fn patterns_match_globs_and_extensions() {
    let p = Path::new("src/app/main.rs");
    assert!(
        matches("rs", p)
            && matches(".rs", p)
            && matches("*.rs", p)
            && matches("src/**/*.rs", p)
            && matches("*.{rs,toml}", p)
    );
    assert!(!matches("py", p) && !matches("tests/*.rs", p) && !matches("", p));
    let cfg = FileCommands {
        commands: BTreeMap::from([
            ("*.rs".into(), "a".into()),
            ("src/app/*.rs".into(), "b".into()),
            ("py".into(), "c".into()),
        ]),
        ..Default::default()
    };
    assert_eq!(
        configured(&cfg, p).as_deref(),
        Some("b"),
        "the most specific pattern wins"
    );
    assert_eq!(configured(&cfg, Path::new("x.py")).as_deref(), Some("c"));
    assert_eq!(configured(&cfg, Path::new("x.go")), None);
}

#[test]
fn command_lines_place_the_file() {
    use crate::shell::Flavor::*;
    assert_eq!(
        command_line("rustfmt", "src/a b.rs", Posix),
        "rustfmt 'src/a b.rs'"
    );
    assert_eq!(
        command_line("fmt {file} --check", "a.rs", Posix),
        "fmt a.rs --check"
    );
    // Nothing in a file name expands, whatever the shell.
    let evil = r"src\$(calc) %PATH% `id`'.rs";
    assert_eq!(
        command_line("fmt", evil, Posix),
        format!("fmt '{}'", evil.replace('\'', r"'\''"))
    );
    assert_eq!(
        command_line("fmt", evil, PowerShell),
        r"fmt 'src\$(calc) %PATH% `id`''.rs'"
    );
    assert_eq!(
        command_line("fmt {file} --check", evil, Cmd),
        r#"fmt "%CHANTO_FILE%" --check"#
    );
}

#[cfg(unix)]
#[tokio::test]
async fn file_names_are_not_interpreted_by_the_shell() {
    let d = std::env::temp_dir().join(format!(
        "chanto-fmt-quote-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&d).unwrap();
    let name = "$(touch pwned) `touch pwned2` 'q'.txt";
    std::fs::write(d.join(name), "x").unwrap();
    let cfg = FileCommands {
        commands: BTreeMap::from([(
            "txt".into(),
            "cat {file} && printf %s \"$CHANTO_FILE\"".into(),
        )]),
        ..Default::default()
    };
    let cmd = command_for(&cfg, Stage::Format, &d, &d.join(name)).unwrap();
    let r = run(&cmd, &d, Path::new(name), 10).await;
    assert!(r.ok && r.output == format!("x{name}"), "{r:?}");
    assert!(!d.join("pwned").exists() && !d.join("pwned2").exists());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn detection_needs_the_project_to_use_the_tool() {
    let d = std::env::temp_dir().join(format!(
        "chanto-fmt-detect-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&d).unwrap();
    let all = |_: &str| true;
    let none = |_: &str| false;
    let f = |n: &str| d.join(n);
    assert_eq!(
        detect(Stage::Format, &d, &f("a.rs"), &all),
        None,
        "no Cargo.toml"
    );
    std::fs::write(
        d.join("Cargo.toml"),
        "[package]\nname = \"x\"\nedition = \"2024\"\n",
    )
    .unwrap();
    assert_eq!(
        detect(Stage::Format, &d, &f("a.rs"), &all).as_deref(),
        Some("rustfmt --edition 2024")
    );
    assert_eq!(
        detect(Stage::Format, &d, &f("a.rs"), &none),
        None,
        "rustfmt not installed"
    );
    assert_eq!(
        detect(Stage::Format, &d, &f("a.py"), &all),
        None,
        "no Python tool configured"
    );
    std::fs::write(
        d.join("pyproject.toml"),
        "[tool.black]\nline-length = 100\n",
    )
    .unwrap();
    assert_eq!(
        detect(Stage::Format, &d, &f("a.py"), &all).as_deref(),
        Some("black --quiet")
    );
    std::fs::write(d.join("pyproject.toml"), "[tool.ruff]\n").unwrap();
    assert_eq!(
        detect(Stage::Format, &d, &f("a.py"), &all).as_deref(),
        Some("ruff format --quiet")
    );
    assert_eq!(
        detect(Stage::Lint, &d, &f("a.py"), &all).as_deref(),
        Some("ruff check --quiet")
    );
    assert_eq!(detect(Stage::Format, &d, &f("a.ts"), &all), None);
    std::fs::write(
        d.join("package.json"),
        r#"{"devDependencies": {"prettier": "^3", "eslint": "^9"}}"#,
    )
    .unwrap();
    assert_eq!(
        detect(Stage::Format, &d, &f("a.ts"), &all).as_deref(),
        Some("npx --no-install prettier --write --log-level warn")
    );
    assert_eq!(
        detect(Stage::Lint, &d, &f("a.ts"), &all).as_deref(),
        Some("npx --no-install eslint")
    );
    assert_eq!(detect(Stage::Format, &d, &f("a.go"), &all), None);
    std::fs::write(d.join("go.mod"), "module x\n").unwrap();
    assert_eq!(
        detect(Stage::Format, &d, &f("a.go"), &all).as_deref(),
        Some("gofmt -w")
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[cfg(unix)]
#[tokio::test]
async fn edits_are_formatted_and_linted_for_the_model() {
    use crate::agent::tests::{make_env_with, mock_server, tc, tmpdir};
    use crate::agent::{Agent, TurnEnd};
    use serde_json::json;
    let dir = tmpdir("format-lint");
    let (base, seen) = mock_server(vec![
        json!({"tool_calls": tc("Write", json!({"file_path": "a.txt", "content": "hello\n"}))}),
        // The formatter's rewrite doesn't count as an outside change.
        json!({"tool_calls": tc("Edit", json!({"file_path": "a.txt", "old_string": "HELLO", "new_string": "hi"}))}),
        json!({"content": "Done."}),
    ])
    .await;
    let env = make_env_with(
        &base,
        &dir,
        crate::permissions::Mode::AcceptEdits,
        32768,
        |c| {
            c.format.commands.insert(
                "txt".into(),
                "tr a-z A-Z < {file} > {file}.tmp && mv {file}.tmp {file}".into(),
            );
            c.lint.commands.insert(
                "*.txt".into(),
                "grep -n HI {file} && exit 1 || exit 0".into(),
            );
        },
    );
    let mut agent = Agent::new(env);
    let end = agent
        .run_turn(
            "write it",
            vec![],
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(end, TurnEnd::Done(ref t) if t == "Done."));
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "HI\n");
    let reqs = seen.lock().unwrap();
    let tool_msg = |i: usize| {
        reqs[i]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|m| m["role"] == "tool")
            .unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let first = tool_msg(1);
    assert!(
        first.contains("a.txt was formatted with") && first.contains("+HELLO"),
        "{first}"
    );
    assert!(!first.contains("<lint"), "clean file: no lint report");
    let second = tool_msg(2);
    assert!(
        second.contains("<lint file=\"a.txt\"") && second.contains("1:HI"),
        "{second}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn layers_merge_commands() {
    let mut a = FileCommands {
        auto: Some(true),
        commands: BTreeMap::from([("rs".into(), "rustfmt".into())]),
        ..Default::default()
    };
    a.merge(FileCommands {
        timeout_secs: Some(5),
        commands: BTreeMap::from([("py".into(), "black".into())]),
        ..Default::default()
    });
    assert_eq!(
        (a.auto, a.timeout_secs, a.commands.len()),
        (Some(true), Some(5), 2)
    );
    assert!(a.on());
    a.merge(FileCommands {
        enabled: Some(false),
        ..Default::default()
    });
    assert!(!a.on());
    assert!(!FileCommands::default().on(), "off unless configured");
}
