//! `chanto mcp add-json|get|list|remove`, in a child process with its own home
//! and working directory.

use super::*;
use crate::agent::tests::tmpdir;
use crate::test_support::isolated;

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[test]
fn servers_can_be_added_as_json_read_back_and_removed() {
    if isolated("mcp_cli::run_tests::servers_can_be_added_as_json_read_back_and_removed") {
        return;
    }
    std::env::set_current_dir(tmpdir("mcp-cli")).unwrap();
    assert_eq!(run(&args(&["help"])).unwrap(), USAGE);
    assert_eq!(run(&[]).unwrap(), USAGE);
    let e = run(&args(&["get"])).unwrap_err();
    assert_eq!(e.to_string(), "usage: chanto mcp get <name>");
    let e = run(&args(&["get", "ghost"])).unwrap_err();
    assert_eq!(e.to_string(), "no MCP server named ghost");

    let e = run(&args(&["add-json", "bad", "not json", "--scope", "user"])).unwrap_err();
    assert!(!e.to_string().is_empty());
    // A secret isn't read from a closed stdin, and nothing is saved.
    let e = run(&args(&[
        "add-json",
        "s",
        r#"{"url":"https://m.test"}"#,
        "--client-secret",
        "--scope",
        "user",
    ]))
    .unwrap_err();
    assert!(e.to_string().contains("MCP_CLIENT_SECRET"), "{e}");

    let json = r#"{"command":"srv","args":["--x"],"env":{"TOKEN":"hidden-value"}}"#;
    run(&args(&["add-json", "mine", json, "--scope", "user"])).unwrap();
    let got = run(&args(&["get", "mine"])).unwrap();
    assert!(got.contains("srv"), "{got}");
    assert!(got.contains("env TOKEN=…"), "{got}");
    assert!(!got.contains("hidden-value"), "values stay hidden: {got}");
    let json = r#"{"url":"https://m.test/mcp","headers":{"Authorization":"hidden-header"}}"#;
    run(&args(&["add-json", "remote", json, "--scope", "user"])).unwrap();
    let got = run(&args(&["get", "remote"])).unwrap();
    assert!(got.contains("header Authorization: …"), "{got}");
    assert!(!got.contains("hidden-header"), "values stay hidden: {got}");
    assert!(run(&args(&["ls"])).unwrap().contains("mine"));
    run(&args(&["rm", "mine", "--scope", "user"])).unwrap();
    assert!(run(&args(&["get", "mine"])).is_err());
}

#[test]
fn claude_desktop_servers_are_read_from_its_config() {
    if isolated("mcp_cli::run_tests::claude_desktop_servers_are_read_from_its_config") {
        return;
    }
    std::env::set_current_dir(tmpdir("mcp-desktop")).unwrap();
    let e = run(&args(&["add-from-claude-desktop", "--scope", "user"]));
    assert!(
        e.is_err() || e.unwrap().to_lowercase().contains("no "),
        "nothing to import"
    );
    for p in claude_desktop_paths() {
        assert!(
            p.starts_with(dirs::home_dir().unwrap()) || !p.exists(),
            "{}",
            p.display()
        );
    }
}
