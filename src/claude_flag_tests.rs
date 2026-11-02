use super::{Cli, print_only_error, run_structured};
use crate::agent::Agent;
use crate::agent::tests::{make_env, mock_server, tmpdir};
use crate::permissions::Mode;
use clap::Parser;
use serde_json::json;

fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from([&["chanto"][..], args].concat()).unwrap()
}

#[test]
fn safe_mode_and_restricted_launch_flags() {
    let l = parse(&["--safe-mode"]).launch().unwrap();
    assert!(l.safe_mode && l.no_customizations() && !l.bare && l.wants("user"));
    let l = parse(&[
        "--restricted",
        "--setting-sources",
        "user,project",
        "--settings",
        "{}",
    ])
    .launch()
    .unwrap();
    // Only managed settings and --settings.
    assert!(
        l.restricted
            && !l.wants("user")
            && !l.wants("project")
            && !l.wants("local")
            && l.settings.len() == 1
    );
    let e = parse(&["--restricted", "--dangerously-skip-permissions"])
        .launch()
        .unwrap_err();
    assert!(e.to_string().contains("bypass"), "{e}");
}

#[test]
fn claude_code_flags_parse() {
    let c = parse(&[
        "-p",
        "--system-prompt",
        "S",
        "--append-system-prompt-file",
        "a.txt",
        "--mcp-config",
        "a.json",
        "--mcp-config",
        "{\"mcpServers\":{}}",
        "--strict-mcp-config",
        "--settings",
        "{}",
        "--setting-sources",
        "user,project",
        "--session-id",
        "550e8400-e29b-41d4-a716-446655440000",
        "-n",
        "my-work",
        "--no-session-persistence",
        "--json-schema",
        "{\"type\":\"object\"}",
        "--agent",
        "reviewer",
        "--tools",
        "",
        "--plugin-dir",
        "p1",
        "--plugin-dir",
        "p2",
        "--bare",
        "--include-partial-messages",
        "--permission-prompt-tool",
        "mcp__auth__ok",
        "do it",
    ]);
    assert_eq!(c.system_prompt.as_deref(), Some("S"));
    assert_eq!(c.mcp_config.len(), 2);
    assert_eq!(c.plugin_dir, vec!["p1", "p2"]);
    assert_eq!(c.tools.as_deref(), Some(""));
    assert_eq!(c.name.as_deref(), Some("my-work"));
    assert_eq!(c.prompt, vec!["do it"]);
    assert!(
        c.strict_mcp_config && c.bare && c.no_session_persistence && c.include_partial_messages
    );
    assert!(
        Cli::try_parse_from([
            "chanto",
            "--system-prompt",
            "a",
            "--system-prompt-file",
            "b"
        ])
        .is_err()
    );
}

#[test]
fn agents_is_a_team_size_or_subagent_json() {
    let c = parse(&["team", "x", "--agents", "4"]);
    assert_eq!(c.team_size(), Some(4));
    assert!(c.launch().is_ok());
    let c = parse(&["--agents", r#"{"r":{"description":"d","prompt":"p"}}"#]);
    assert_eq!(c.team_size(), None);
    assert!(c.launch().is_ok());
    assert!(
        parse(&["--agents", r#"{"r":{"prompt":"p"}}"#])
            .launch()
            .is_err()
    );
}

#[test]
fn launch_flags_are_checked_up_front() {
    assert!(
        parse(&["--json-schema", "{\"type\":\"objekt\"}"])
            .launch()
            .unwrap_err()
            .to_string()
            .contains("not a valid JSON Schema")
    );
    assert!(parse(&["--json-schema", "nope"]).launch().is_err());
    assert!(parse(&["--session-id", "not-a-uuid"]).launch().is_err());
    assert!(
        parse(&["--permission-prompt-tool", "approve"])
            .launch()
            .is_err()
    );
    assert!(
        parse(&["--setting-sources", "everything"])
            .launch()
            .is_err()
    );
    assert!(
        parse(&["--mcp-config", "/no/such/file.json"])
            .launch()
            .is_err()
    );
    let l = parse(&[
        "--strict-mcp-config",
        "--mcp-config",
        r#"{"mcpServers":{"a":{"command":"x"}}}"#,
        "--bare",
    ])
    .launch()
    .unwrap();
    assert!(l.strict_mcp && l.bare && l.mcp[0].1.contains_key("a"));
    assert!(print_only_error(&parse(&["--json-schema", "{}"]), false).is_some());
    assert!(print_only_error(&parse(&["-p", "--json-schema", "{}"]), true).is_none());
    assert!(
        print_only_error(
            &parse(&["--session-id", "550e8400-e29b-41d4-a716-446655440000", "-c"]),
            true
        )
        .is_some()
    );
}

#[test]
fn quiet_presets_auto_mode_unless_a_mode_is_given() {
    let mode = |args: &[&str], configured| super::start_mode(&parse(args), configured).unwrap();
    assert_eq!(mode(&["--quiet"], None), Mode::Auto);
    // The preset beats the config's default mode; anything explicit beats the preset.
    assert_eq!(mode(&["--quiet"], Some(Mode::Default)), Mode::Auto);
    assert_eq!(mode(&["--quiet", "--mode", "plan"], None), Mode::Plan);
    assert_eq!(
        mode(&["--quiet", "--permission-mode", "default"], None),
        Mode::Default
    );
    assert_eq!(
        mode(&["--quiet", "--dangerously-skip-permissions"], None),
        Mode::Bypass
    );
    assert_eq!(
        mode(&["--quiet", "--sandbox", "read-only"], None),
        Mode::Plan
    );
    // Without --quiet nothing changes.
    assert_eq!(mode(&[], Some(Mode::AcceptEdits)), Mode::AcceptEdits);
    assert_eq!(mode(&[], None), Mode::Default);
}

#[test]
fn quiet_print_text_is_the_summary_only_and_json_is_unchanged() {
    use crate::plain::OutputFormat;
    let fmt = |args: &[&str]| super::print_format(&parse(args));
    assert!(fmt(&["-p", "--quiet", "--verbose", "x"]) == OutputFormat::Text);
    assert!(fmt(&["-p", "--verbose", "x"]) == OutputFormat::Live);
    assert!(fmt(&["-p", "--quiet", "--output-format", "json", "x"]) == OutputFormat::Json);
    assert!(
        fmt(&[
            "-p",
            "--quiet",
            "--output-format",
            "stream-json",
            "--verbose",
            "x"
        ]) == OutputFormat::StreamJson
    );
}

#[tokio::test]
async fn quiet_print_mode_answers_with_the_final_summary() {
    let dir = tmpdir("quiet-print");
    std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
    let (base, _) = mock_server(vec![
        json!({"content": "Let me look.", "tool_calls": crate::agent::tests::tc("Read", json!({"file_path": "a.txt"}))}),
        json!({"content": "Read a.txt. Nothing changed."}),
    ])
    .await;
    let env = crate::agent::tests::make_env_with(&base, &dir, Mode::Bypass, 32768, |c| {
        c.optimize.quiet = Some(true)
    });
    let mut agent = Agent::new(env);
    // What `-p` text prints: the turn's answer, without the narration before the tool call.
    let (text, ok, _) = run_structured(
        &mut agent,
        "read a.txt",
        None,
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert!(ok);
    assert_eq!(text, "Read a.txt. Nothing changed.");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn json_schema_output_is_validated_and_repaired_once() {
    let schema =
        json!({"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]});
    // Valid on the first try (inside prose).
    let (base, seen) = mock_server(vec![json!({"content": "Done: {\"n\": 2}"})]).await;
    let dir = tmpdir("schema-ok");
    let mut agent = Agent::new(make_env(&base, &dir, Mode::Bypass, 32768));
    let (_, ok, v) = run_structured(
        &mut agent,
        "count",
        Some(&schema),
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert!(ok);
    assert_eq!(v, Some(json!({"n": 2})));
    assert!(seen.lock().unwrap()[0].to_string().contains("JSON Schema"));
    let _ = std::fs::remove_dir_all(dir);

    // Wrong, then repaired.
    let (base, seen) = mock_server(vec![
        json!({"content": "{\"n\": \"two\"}"}),
        json!({"content": "{\"n\": 2}"}),
    ])
    .await;
    let dir = tmpdir("schema-fix");
    let mut agent = Agent::new(make_env(&base, &dir, Mode::Bypass, 32768));
    let (_, ok, v) = run_structured(
        &mut agent,
        "count",
        Some(&schema),
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert!(ok);
    assert_eq!(v, Some(json!({"n": 2})));
    assert!(
        seen.lock().unwrap()[1]
            .to_string()
            .contains("expected integer")
    );
    let _ = std::fs::remove_dir_all(dir);

    // Wrong twice: an error.
    let (base, _) = mock_server(vec![
        json!({"content": "no"}),
        json!({"content": "still no"}),
    ])
    .await;
    let dir = tmpdir("schema-bad");
    let mut agent = Agent::new(make_env(&base, &dir, Mode::Bypass, 32768));
    let (text, ok, v) = run_structured(
        &mut agent,
        "count",
        Some(&schema),
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert!(!ok && v.is_none());
    assert!(text.contains("doesn't match --json-schema"), "{text}");
    let _ = std::fs::remove_dir_all(dir);
}
