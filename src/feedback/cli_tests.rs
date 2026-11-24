//! `chanto feedback` and `chanto diagnostics`, in a child process with its own
//! state directory and a loopback collector.

use super::*;
use crate::test_support::isolated;
use axum::Json;
use axum::routing::post;

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn feedback_needs_a_message_and_consent() {
    if isolated("feedback::cli_tests::feedback_needs_a_message_and_consent") {
        return;
    }
    assert_eq!(cli(&args(&["--help"])).await.unwrap(), USAGE);
    let e = cli(&args(&["--yes", "--no-log"])).await.unwrap_err();
    assert!(e.to_string().starts_with("Usage:"), "{e}");
    // No terminal to ask on, and no --yes: nothing leaves.
    let e = cli(&args(&["it", "broke"])).await.unwrap_err();
    assert_eq!(
        e.to_string(),
        "Not sent: add --yes to send without a prompt"
    );
}

#[tokio::test]
async fn feedback_sends_what_it_showed() {
    if isolated("feedback::cli_tests::feedback_sends_what_it_showed") {
        return;
    }
    let got: std::sync::Arc<std::sync::Mutex<Vec<Value>>> = Default::default();
    let g2 = got.clone();
    let app = axum::Router::new().route(
        "/",
        post(move |Json(b): Json<Value>| async move {
            g2.lock().unwrap().push(b);
            Json(serde_json::json!({"ok": true}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    // SAFETY: the test runs alone in its own child process.
    unsafe { std::env::set_var("CHANTO_FEEDBACK_URL", &base) };
    let out = cli(&args(&["-y", "--no-log", "it", "broke"]))
        .await
        .unwrap();
    assert_eq!(out, "Thanks! Your feedback was sent.");
    let got = got.lock().unwrap();
    assert_eq!(got[0]["message"], "it broke");
}

#[test]
fn diagnostics_mode_turns_on_off_and_explains_bad_options() {
    if isolated("feedback::cli_tests::diagnostics_mode_turns_on_off_and_explains_bad_options") {
        return;
    }
    let run = |a: &[&str]| diagnostics_command(&args(a));
    assert!(run(&[]).unwrap().starts_with("Diagnostic mode is off."));
    assert_eq!(run(&["--help"]).unwrap(), DIAGNOSTICS_USAGE);
    assert!(
        run(&["sideways"])
            .unwrap_err()
            .to_string()
            .starts_with("unknown subcommand sideways")
    );
    assert!(
        run(&["on", "--hours"])
            .unwrap_err()
            .to_string()
            .contains("--hours needs a number")
    );
    assert!(
        run(&["on", "--hours", "x"])
            .unwrap_err()
            .to_string()
            .contains("--hours needs a number")
    );
    assert!(
        run(&["on", "--name"])
            .unwrap_err()
            .to_string()
            .contains("--name needs a value")
    );
    assert!(
        run(&["on", "--what"])
            .unwrap_err()
            .to_string()
            .starts_with("unknown option --what")
    );
    assert!(
        run(&[]).unwrap().starts_with("Diagnostic mode is off."),
        "a bad option changes nothing"
    );

    let on = run(&["on", "--hours", "9999", "--name", " dan "]).unwrap();
    assert!(
        on.starts_with("Diagnostic mode is on for 168 hour(s), tagged \"dan\""),
        "capped: {on}"
    );
    let status = run(&["status"]).unwrap();
    assert!(
        status.contains("is on for 16") && status.contains("tagged \"dan\""),
        "{status}"
    );
    assert_eq!(run(&["off"]).unwrap(), "Diagnostic mode is off.");
    assert!(run(&[]).unwrap().starts_with("Diagnostic mode is off."));

    let on = run(&["on", "--hours", "0", "--name", "  "]).unwrap();
    assert!(
        on.starts_with("Diagnostic mode is on for 1 hour(s):"),
        "{on}"
    );
    // SAFETY: the test runs alone in its own child process.
    unsafe { std::env::set_var("CHANTO_NO_ERROR_REPORTS", "1") };
    assert!(
        run(&["on"])
            .unwrap_err()
            .to_string()
            .contains("CHANTO_NO_ERROR_REPORTS")
    );
}
