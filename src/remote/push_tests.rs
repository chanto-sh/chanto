//! Notifications when a session needs input, and the `/remote` command's
//! answers before and after it's running.

use super::*;
use crate::agent::tests::{make_env_with, tmpdir};
use crate::permissions::Mode;
use axum::Json;
use axum::http::HeaderMap;
use axum::routing::post;

#[test]
fn nothing_configured_means_no_notifier() {
    let cfg = crate::runs::notify::NotifyConfig::default();
    assert!(Push::new(&cfg, "http://base").is_none());
}

/// A notification goes to a third party, so it must never carry session
/// content: only that something is waiting, and the link back.
#[tokio::test]
async fn notifications_reach_both_channels_without_session_content() {
    let seen: Arc<Mutex<Vec<String>>> = Default::default();
    let (s1, s2) = (seen.clone(), seen.clone());
    let app = axum::Router::new()
        .route(
            "/ntfy",
            post(move |h: HeaderMap, body: String| async move {
                s1.lock().unwrap().push(format!(
                    "ntfy click={} prio={} {body}",
                    h["click"].to_str().unwrap(),
                    h["priority"].to_str().unwrap(),
                ));
            }),
        )
        .route(
            "/hook",
            post(move |Json(b): Json<Value>| async move {
                s2.lock()
                    .unwrap()
                    .push(format!("hook {} {}", b["event"], b["url"]));
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });

    let cfg = crate::runs::notify::NotifyConfig {
        ntfy: Some(format!("{base}/ntfy")),
        webhook: Some(format!("{base}/hook")),
        ..Default::default()
    };
    let push = Push::new(&cfg, "http://session-link").unwrap();
    push.send().await;
    let log = seen.lock().unwrap().join("\n");
    assert!(
        log.contains("ntfy click=http://session-link prio=high"),
        "{log}"
    );
    assert!(
        log.contains("hook \"session.needs-input\" \"http://session-link\""),
        "{log}"
    );

    // A configured link wins over the session's address; a dead endpoint is quiet.
    let cfg = crate::runs::notify::NotifyConfig {
        webhook: Some("http://127.0.0.1:1/x".into()),
        link: Some("http://mine".into()),
        ..Default::default()
    };
    let push = Push::new(&cfg, "http://session-link").unwrap();
    assert_eq!(push.link, "http://mine");
    push.send().await;
}

#[tokio::test]
async fn the_command_explains_itself_when_nothing_is_running() {
    if crate::test_support::isolated(
        "remote::push_tests::the_command_explains_itself_when_nothing_is_running",
    ) {
        return;
    }
    let dir = tmpdir("remote-cmd");
    let env = make_env_with("http://127.0.0.1:9", &dir, Mode::Bypass, 32768, |_| {});
    for arg in ["status", "link"] {
        assert_eq!(
            command(&env, arg).await,
            "Remote control is off. `/remote` turns it on."
        );
    }
    assert!(
        command(&env, "banana")
            .await
            .starts_with("Usage: `/remote`")
    );
    // Outside the TUI there's no session to mirror.
    assert_eq!(command(&env, "local").await, NOT_HERE);
    assert!(active().is_none());
    assert_eq!(quietly(|| 7), 7);
}
