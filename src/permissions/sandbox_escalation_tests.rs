use super::*;
use serde_json::json;

/// Windows turns the sandbox on at startup before proving it can start.
/// If it then can't, commands must lose everything the sandbox justified.
#[test]
fn a_sandbox_that_fails_to_start_justifies_nothing() {
    use crate::tools::bash::{SANDBOX_ACTIVE, SANDBOX_FAILS, SANDBOX_TEST_LOCK, sandbox_on};
    use std::sync::atomic::Ordering::Relaxed;
    let _g = SANDBOX_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cwd = std::env::temp_dir();
    let plain = json!({"command": "./build-something.sh"});
    SANDBOX_ACTIVE.store(true, Relaxed);
    SANDBOX_AUTO_ALLOW.store(true, Relaxed);
    assert!(sandbox_on());
    assert!(matches!(
        static_decision(Mode::Auto, ToolKind::Execute, "Bash", &plain, &cwd),
        Some(Decision::Allow)
    ));
    SANDBOX_FAILS.store(true, Relaxed);
    let on = sandbox_on();
    let auto = static_decision(Mode::Auto, ToolKind::Execute, "Bash", &plain, &cwd);
    SANDBOX_FAILS.store(false, Relaxed);
    SANDBOX_AUTO_ALLOW.store(false, Relaxed);
    SANDBOX_ACTIVE.store(false, Relaxed);
    assert!(!on);
    assert!(!matches!(auto, Some(Decision::Allow)), "{auto:?}");
}

#[test]
fn leaving_the_sandbox_is_never_automatic() {
    let _g = crate::tools::bash::SANDBOX_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::tools::bash::SANDBOX_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
    let cwd = std::env::temp_dir();
    let esc = json!({"command": "cat README.md", "dangerouslyDisableSandbox": true});
    let plain = json!({"command": "cat README.md"});
    assert!(matches!(
        static_decision(Mode::AcceptEdits, ToolKind::Execute, "Bash", &plain, &cwd),
        Some(Decision::Allow)
    ));
    assert!(matches!(
        static_decision(Mode::AcceptEdits, ToolKind::Execute, "Bash", &esc, &cwd),
        Some(Decision::Ask)
    ));
    assert!(matches!(
        static_decision(Mode::Default, ToolKind::Execute, "Bash", &esc, &cwd),
        Some(Decision::Ask)
    ));
    // Auto mode makes the model's *other* actions autonomous, but leaving
    // the sandbox still needs a human: the sandbox is what justifies the
    // autonomy, so the classifier must not be able to hand it back.
    assert!(matches!(
        static_decision(Mode::Auto, ToolKind::Execute, "Bash", &esc, &cwd),
        Some(Decision::Ask)
    ));
    // Even a "safe" read needs approval once it's out of the sandbox.
    assert!(matches!(
        static_decision(Mode::Plan, ToolKind::Execute, "Bash", &esc, &cwd),
        Some(Decision::Ask)
    ));
    assert!(matches!(
        static_decision(Mode::Bypass, ToolKind::Execute, "Bash", &esc, &cwd),
        Some(Decision::Allow)
    ));
    crate::tools::bash::SANDBOX_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
    // Without an active sandbox the flag changes nothing.
    assert!(matches!(
        static_decision(Mode::AcceptEdits, ToolKind::Execute, "Bash", &esc, &cwd),
        Some(Decision::Allow)
    ));
}
