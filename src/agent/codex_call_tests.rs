use super::*;

#[test]
fn maps_codex_tools() {
    let c = |name: &str, args: Value| codex_call(mk("1".into(), name, args));
    let r = c(
        "shell",
        json!({"command": ["bash", "-lc", "ls -la"], "workdir": "/tmp/x"}),
    );
    assert_eq!(
        (r.name.as_str(), r.arguments["command"].as_str().unwrap()),
        ("Bash", "cd /tmp/x && ls -la")
    );
    let r = c(
        "shell",
        json!({"command": ["apply_patch", "*** Begin Patch\n*** Delete File: a\n*** End Patch"]}),
    );
    assert_eq!(r.name, "apply_patch");
    let r = c(
        "Bash",
        json!({"command": "apply_patch <<'EOF'\n*** Begin Patch\n*** Delete File: a\n*** End Patch\nEOF"}),
    );
    assert_eq!(r.name, "apply_patch");
    let r = c("Bash", json!({"command": "ls"}));
    assert_eq!(r.arguments, json!({"command": "ls"}));
    let r = c(
        "update_plan",
        json!({"plan": [{"step": "a", "status": "in_progress"}]}),
    );
    assert_eq!(r.arguments["todos"][0]["content"], "a");
}
