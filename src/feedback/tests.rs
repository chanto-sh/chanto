use super::*;
use std::sync::Arc;

/// A feedback endpoint that records what it receives.
async fn server() -> (String, Arc<Mutex<Vec<Value>>>) {
    let got: Arc<Mutex<Vec<Value>>> = Arc::default();
    let store = got.clone();
    let app = axum::Router::new().route(
        "/api/feedback",
        axum::routing::post(move |axum::Json(b): axum::Json<Value>| {
            let store = store.clone();
            async move {
                store.lock().unwrap().push(b);
                (
                    axum::http::StatusCode::ACCEPTED,
                    axum::Json(json!({"ok": true})),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/api/feedback"), got)
}

struct Answer(&'static str, Mutex<Vec<String>>);
#[async_trait::async_trait]
impl Frontend for Answer {
    fn emit(&self, _: crate::agent::Event) {}
    async fn permission(
        &self,
        _: crate::agent::PermissionRequest,
    ) -> crate::agent::PermissionReply {
        crate::agent::PermissionReply::Allow
    }
    async fn question(&self, q: &str, _: &[String]) -> Option<String> {
        self.1.lock().unwrap().push(q.to_string());
        Some(self.0.to_string())
    }
}

#[test]
fn redaction_removes_home_secrets_and_url_details() {
    let text = "failed reading /Users/alice/proj/src/main.rs with key sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG at https://user:hunter2@api.example.com/v1/chat?key=abc and OPENAI_API_KEY=Zx81kQpLm29vBnTr4sYw";
    let out = scrub_with(text, Some("/Users/alice"));
    assert!(out.contains("~/proj/src/main.rs"), "{out}");
    assert!(
        !out.contains("/Users/alice") && !out.contains("hunter2") && !out.contains("key=abc"),
        "{out}"
    );
    assert!(
        !out.contains("sk-ant-api03") && !out.contains("Zx81kQpLm29vBnTr4sYw"),
        "{out}"
    );
    assert!(out.contains("https://api.example.com/…"), "{out}");
    let p = feedback_payload(
        "it broke in /Users/alice/x",
        &Diagnostics {
            provider: Some("openai".into()),
            model: Some("gpt-5".into()),
            mode: Some("auto".into()),
        },
        &[],
    );
    let home = dirs::home_dir().unwrap().display().to_string();
    assert!(!p.to_string().contains(&home), "{p}");
    assert_eq!(p["type"], "feedback");
    assert_eq!(p["provider"], "openai");
    assert!(p.get("log").is_none());
}

#[test]
fn windows_home_is_redacted_plain_escaped_and_with_slashes() {
    let home = r"C:\Users\sam";
    let s = r#"C:\Users\sam\a {"path":"C:\\Users\\sam\\b"} C:/Users/sam/c"#;
    assert_eq!(scrub_with(s, Some(home)), r#"~\a {"path":"~\\b"} ~/c"#);
}

#[test]
fn log_lines_are_errors_and_warnings_only_and_redacted() {
    let dir = tempfile_dir();
    std::fs::create_dir_all(dir.join("logs")).unwrap();
    let home = dirs::home_dir().unwrap().display().to_string();
    let mut lines =
        vec![json!({"time": "t0", "level": "info", "event": "model_call", "data": {}}).to_string()];
    for i in 0..25 {
        lines.push(json!({"time": format!("t{i}"), "level": "error", "event": "tool", "data": {"path": format!("{home}/secret/{i}")}}).to_string());
    }
    std::fs::write(dir.join("logs/chanto-2026-10-15.log"), lines.join("\n")).unwrap();
    let got = recent_problems(&dir, 20);
    assert_eq!(got.len(), 20);
    assert!(
        got.iter()
            .all(|l| l.contains(" error tool") && !l.contains(&home)),
        "{got:?}"
    );
    let last = got.last().unwrap();
    assert!(
        last.contains("~/secret/24") || last.contains("~\\\\secret/24"),
        "{last}"
    );
}

fn tempfile_dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("chanto-feedback-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[tokio::test]
async fn feedback_needs_confirmation() {
    let (url, got) = server().await;
    let d = Diagnostics::default();
    let cancel = Answer(CANCEL, Mutex::default());
    assert_eq!(
        confirm_and_send(&cancel, &url, "hello", &d, &["line".into()]).await,
        "Feedback not sent."
    );
    assert!(got.lock().unwrap().is_empty());
    // The question shows the exact payload.
    assert!(cancel.1.lock().unwrap()[0].contains("\"message\": \"hello\""));

    let send = Answer(SEND_NO_LOG, Mutex::default());
    assert!(
        confirm_and_send(&send, &url, "hello", &d, &["line".into()])
            .await
            .starts_with("Thanks")
    );
    let send = Answer(SEND, Mutex::default());
    assert!(
        confirm_and_send(&send, &url, "again", &d, &["line".into()])
            .await
            .starts_with("Thanks")
    );
    let got = got.lock().unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0]["message"], "hello");
    assert!(got[0].get("log").is_none());
    assert_eq!(got[1]["log"], json!(["line"]));
}

#[tokio::test]
async fn error_reports_respect_opt_in_and_rate_limit() {
    let (url, got) = server().await;
    let dir = tempfile_dir();
    assert!(
        !reports_on(None, false)
            && !reports_on(Some(false), false)
            && !reports_on(Some(true), true)
            && reports_on(Some(true), false)
    );
    // Off: nothing is even started.
    assert!(
        spawn_report(
            false,
            dir.clone(),
            url.clone(),
            "model_request",
            "boom",
            None
        )
        .is_none()
    );
    // On: sent once, then the same kind is held back for an hour.
    spawn_report(
        true,
        dir.clone(),
        url.clone(),
        "model_request",
        "boom at /x",
        Some("ollama".into()),
    )
    .unwrap()
    .await
    .unwrap();
    spawn_report(
        true,
        dir.clone(),
        url.clone(),
        "model_request",
        "boom again",
        None,
    )
    .unwrap()
    .await
    .unwrap();
    {
        let got = got.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["type"], "error");
        assert_eq!(got[0]["kind"], "model_request");
        assert_eq!(got[0]["provider"], "ollama");
        assert!(
            got[0]["install_id"]
                .as_str()
                .is_some_and(|id| id.len() == 36)
        );
    }
    let mut state = load(&dir);
    let now = chrono::Utc::now().timestamp();
    assert!(!state.allow("model_request", now + 60));
    assert!(state.allow("model_request", now + PER_KIND_SECS + 1));
    // A daily cap across kinds.
    let mut s = State::default();
    let day = 1_760_000_000;
    assert_eq!(
        (0..20).filter(|i| s.allow(&format!("k{i}"), day)).count(),
        PER_DAY as usize
    );
    // The install id is stable.
    let id = load(&dir).install_id;
    spawn_report(
        true,
        dir.clone(),
        url.clone(),
        "classifier_unavailable",
        "x",
        None,
    )
    .unwrap()
    .await
    .unwrap();
    assert_eq!(got.lock().unwrap()[1]["install_id"].as_str(), id.as_deref());
}

#[tokio::test]
async fn diagnostic_mode_tags_reports_relaxes_limits_and_sends_new_log_lines() {
    let (url, got) = server().await;
    let dir = tempfile_dir();
    let now = chrono::Utc::now().timestamp();
    let mut state = State {
        diagnostics_until: Some(now + 3600),
        diagnostics_name: Some("sam".into()),
        ..Default::default()
    };
    // The same kind again within the hour is fine, and far more per day.
    assert!(state.allow("model_request", now) && state.allow("model_request", now + 1));
    assert_eq!(
        (0..50)
            .filter(|i| state.allow(&format!("k{i}"), now + 2))
            .count(),
        50
    );
    // Expired: back to the normal limits, and no label.
    let mut expired = State {
        diagnostics_until: Some(now - 1),
        ..Default::default()
    };
    assert!(expired.diagnostics(now).is_none());
    assert!(expired.allow("x", now) && !expired.allow("x", now + 1));

    state.sent.clear();
    state.day_count = 0;
    state.diagnostics_sent_upto = Some("2026-10-15T10:00:00Z".into());
    save(&dir, &state);
    spawn_report(
        true,
        dir.clone(),
        url.clone(),
        "model_request",
        "boom",
        None,
    )
    .unwrap()
    .await
    .unwrap();
    assert_eq!(got.lock().unwrap()[0]["label"], "sam");

    std::fs::create_dir_all(dir.join("logs")).unwrap();
    let line = |t: &str, level: &str, event: &str| {
        json!({"time": t, "level": level, "event": event, "data": {}}).to_string()
    };
    let log = [
        line("2026-10-15T09:00:00Z", "error", "old"),
        line("2026-10-15T11:00:00Z", "info", "fine"),
        line("2026-10-15T11:00:01Z", "warn", "new"),
    ];
    std::fs::write(dir.join("logs/chanto-2026-10-15.log"), log.join("\n")).unwrap();
    assert_eq!(
        send_diagnostics_batch(&dir, &url, Some("deepseek"))
            .await
            .unwrap(),
        1
    );
    {
        let got = got.lock().unwrap();
        assert_eq!(got[1]["kind"], "diagnostics");
        assert_eq!(got[1]["label"], "sam");
        assert_eq!(got[1]["log"].as_array().unwrap().len(), 1);
        assert!(got[1]["log"][0].as_str().unwrap().contains("warn new"));
    }
    // Nothing new: nothing sent.
    assert_eq!(send_diagnostics_batch(&dir, &url, None).await.unwrap(), 0);
    assert_eq!(got.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn crash_files_are_sent_when_enabled_then_deleted() {
    let (url, got) = server().await;
    let dir = tempfile_dir();
    let home = dirs::home_dir().unwrap().display().to_string();
    write_crash(
        &crash_dir(&dir),
        &format!("index out of range in {home}/proj"),
        "src/agent.rs:10",
    )
    .unwrap();
    // Off: kept locally, nothing sent.
    assert_eq!(send_crashes(&dir, &url, false, None).await, 0);
    assert_eq!(std::fs::read_dir(crash_dir(&dir)).unwrap().count(), 1);
    assert!(got.lock().unwrap().is_empty());
    assert_eq!(send_crashes(&dir, &url, true, Some("openai")).await, 1);
    assert_eq!(std::fs::read_dir(crash_dir(&dir)).unwrap().count(), 0);
    let got = got.lock().unwrap();
    assert_eq!(got[0]["kind"], "panic");
    let msg = got[0]["message"].as_str().unwrap();
    assert!(
        msg.contains("~/proj") && msg.contains("src/agent.rs:10") && !msg.contains(&home),
        "{msg}"
    );
}

#[test]
fn answer_is_recorded_in_config_and_state() {
    let dir = tempfile_dir();
    std::fs::write(dir.join("config.toml"), "model = \"qwen3\"\n").unwrap();
    record_answer(&dir, false);
    let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
    assert!(
        text.contains("model = \"qwen3\"") && text.contains("[feedback]\nshare_errors = false"),
        "{text}"
    );
    let state = load(&dir);
    assert!(state.asked && state.install_id.is_some());
    let cfg: crate::config::Config = toml::from_str(&text).unwrap();
    assert_eq!(cfg.feedback.share_errors, Some(false));
}
