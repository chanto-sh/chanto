use super::*;
use std::sync::{Arc, Mutex};

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

fn user() -> Value {
    json!({"id": "u1", "login": "octo", "name": "Octo Cat"})
}

#[tokio::test]
async fn browser_sign_in_returns_the_token() {
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let (s1, s2, s3) = (seen.clone(), seen.clone(), seen.clone());
    let polls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let app = axum::Router::new()
        .route(
            "/v1/auth/providers",
            axum::routing::get(|| async {
                axum::Json(json!({"providers": [
                    {"id": "github", "name": "GitHub", "browser": true, "device": true},
                    {"id": "google", "name": "Google", "browser": true, "device": false}]}))
            }),
        )
        .route(
            "/v1/auth/browser/start",
            axum::routing::post(move |axum::Json(b): axum::Json<Value>| async move {
                s1.lock().unwrap().push(b);
                axum::Json(json!({"authorize_url": "https://github.example/authorize?x=1", "poll_code": "pc_secret", "expires_in": 60}))
            }),
        )
        .route(
            "/v1/auth/browser/poll",
            axum::routing::post(move |axum::Json(b): axum::Json<Value>| async move {
                s2.lock().unwrap().push(b);
                if polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    axum::Json(json!({"status": "pending", "interval": 1}))
                } else {
                    axum::Json(json!({"status": "complete", "token": "crt_token", "user": user()}))
                }
            }),
        );
    let url = serve(app).await;
    let (token, who) = sign_in(&url, Some("google"), false).await.unwrap();
    assert_eq!((token.as_str(), who.login.as_str()), ("crt_token", "octo"));
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0]["provider"], "google");
    assert!(
        seen[0]["client_name"]
            .as_str()
            .unwrap()
            .starts_with("chanto CLI on ")
    );
    assert_eq!(
        (seen[1]["poll_code"].as_str(), seen[2]["poll_code"].as_str()),
        (Some("pc_secret"), Some("pc_secret"))
    );

    let err = sign_in(&url, Some("bitbucket"), false)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("github, google"), "{err}");
    drop(seen);

    // Inside a session (`/login chanto`): GitHub, without asking.
    let (token, _) = sign_in_with(&url, None, false, true).await.unwrap();
    assert_eq!(token, "crt_token");
    assert_eq!(seen_now(&s3).last().unwrap()["provider"], "github");
}

fn seen_now(seen: &Arc<Mutex<Vec<Value>>>) -> Vec<Value> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|b| b.get("provider").is_some())
        .cloned()
        .collect()
}

#[tokio::test]
async fn device_sign_in_when_the_browser_is_not_offered_and_errors_are_verbatim() {
    let app = axum::Router::new()
        .route(
            "/v1/auth/providers",
            axum::routing::get(|| async { axum::Json(json!({"providers": [{"id": "github", "name": "GitHub", "browser": false, "device": true}]})) }),
        )
        .route(
            "/v1/auth/device/start",
            axum::routing::post(|| async {
                axum::Json(json!({"device_code": "dc", "user_code": "ABCD-1234", "verification_uri": "https://github.example/device", "expires_in": 60, "interval": 1}))
            }),
        )
        .route(
            "/v1/auth/device/poll",
            axum::routing::post(|axum::Json(b): axum::Json<Value>| async move {
                assert_eq!(b["device_code"], "dc");
                (axum::http::StatusCode::FORBIDDEN, axum::Json(json!({"error": "access_denied", "message": "the sign-in was denied"})))
            }),
        );
    let url = serve(app).await;
    // Inside a session the code would have nowhere to show: the terminal command instead.
    let err = sign_in_with(&url, None, false, true)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("run `chanto cloud login` in a terminal"),
        "{err}"
    );
    let err = sign_in(&url, None, false).await.unwrap_err().to_string();
    assert_eq!(err, "Chanto cloud: the sign-in was denied");
}

#[test]
fn the_login_is_saved_privately() {
    let _home = crate::runs::tests::TestHome::new("cloud-login");
    save_login("https://cloud.example", "crt_saved").unwrap();
    // SAFETY: test-only, under the TestHome lock.
    let prev = (
        std::env::var_os("CHANTO_CLOUD_URL"),
        std::env::var_os("CHANTO_CLOUD_TOKEN"),
    );
    unsafe {
        std::env::remove_var("CHANTO_CLOUD_URL");
        std::env::remove_var("CHANTO_CLOUD_TOKEN");
    }
    let c = Cloud::signed_in().unwrap();
    assert_eq!(
        (c.url(), c.token.as_str()),
        ("https://cloud.example", "crt_saved")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(crate::config::home_dir().join("auth.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    set_default_org(Some("acme")).unwrap();
    assert_eq!(default_org().as_deref(), Some("acme"));
    // A token from the environment alone goes to the saved server (else DEFAULT_URL),
    // and the `chanto` provider bills the default org.
    unsafe { std::env::set_var("CHANTO_CLOUD_TOKEN", "crt_env") };
    let c = Cloud::signed_in().unwrap();
    assert_eq!((c.url(), c.token()), ("https://cloud.example", "crt_env"));
    let p = crate::chanto_cloud::provider("claude-sonnet-4-5").unwrap();
    assert_eq!(p.base_url, "https://cloud.example/v1/model/anthropic");
    assert!(
        p.api_key.as_deref() == Some("crt_env")
            && p.headers.contains(&("x-chanto-org", "acme".to_string()))
    );
    // A server from the environment is held to the same rule as `--url`.
    unsafe { std::env::set_var("CHANTO_CLOUD_URL", "http://cloud.example") };
    assert!(Cloud::signed_in().is_err());
    assert!(crate::chanto_cloud::provider("claude-sonnet-4-5").is_none());
    unsafe { std::env::remove_var("CHANTO_CLOUD_URL") };
    unsafe { std::env::remove_var("CHANTO_CLOUD_TOKEN") };
    unsafe {
        if let Some(v) = prev.0 {
            std::env::set_var("CHANTO_CLOUD_URL", v)
        }
        if let Some(v) = prev.1 {
            std::env::set_var("CHANTO_CLOUD_TOKEN", v)
        }
    }
}

#[test]
fn remotes_become_https_urls() {
    assert_eq!(
        https_url("git@github.com:acme/app.git"),
        "https://github.com/acme/app.git"
    );
    assert_eq!(
        https_url("git@github.com:acme/app"),
        "https://github.com/acme/app.git"
    );
    assert_eq!(
        https_url("ssh://git@gitlab.example:2222/group/app.git"),
        "https://gitlab.example/group/app.git"
    );
    assert_eq!(
        https_url("https://x-token:secret@github.com/acme/app.git"),
        "https://github.com/acme/app.git"
    );
    assert_eq!(
        https_url("https://github.com/acme/app"),
        "https://github.com/acme/app"
    );
    assert_eq!(https_url("/srv/git/app.git"), "/srv/git/app.git");
    assert_eq!(
        compare_url(
            "https://github.com/acme/app.git",
            Some("main"),
            "chanto/run/ab12cd34"
        )
        .as_deref(),
        Some("https://github.com/acme/app/compare/main...chanto/run/ab12cd34?expand=1")
    );
}

#[test]
fn cloud_run_flags() {
    let args: Vec<String> = [
        "--cloud",
        "fix",
        "the",
        "--org",
        "acme",
        "--label",
        "gpu",
        "--placement=self-hosted",
        "--push",
        "bug",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let o = parse_run(&args).unwrap();
    assert_eq!(o.task, "fix the bug");
    assert_eq!(
        (o.org.as_deref(), o.labels.as_slice(), o.placement, o.push),
        (
            Some("acme"),
            &["gpu".to_string()][..],
            Some(Placement::SelfHosted),
            true
        )
    );
    assert!(parse_run(&["--cloud".to_string()]).is_err());
    assert!(
        parse_run(&["-m".to_string(), "x".to_string(), "task".to_string()])
            .unwrap_err()
            .to_string()
            .contains("doesn't apply")
    );
    assert!(
        parse_run(&[
            "--placement".to_string(),
            "moon".to_string(),
            "t".to_string()
        ])
        .is_err()
    );
}

#[cfg(unix)]
#[test]
fn the_branch_must_be_committed_and_pushed() {
    let dir = std::env::temp_dir().join(format!(
        "chanto-cloud-src-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let (origin, work) = (dir.join("origin.git"), dir.join("work"));
    std::fs::create_dir_all(&work).unwrap();
    let git = |cwd: &Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&dir, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    git(&work, &["init", "-q"]);
    git(&work, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(work.join("a.txt"), "a").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "a"]);
    git(
        &work,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );

    let err = local_source(&work, false).unwrap_err().to_string();
    assert!(
        err.contains("feature isn't on origin") && err.contains("--push"),
        "{err}"
    );
    let src = local_source(&work, true).unwrap();
    assert_eq!(
        src,
        Source {
            repo: origin.to_str().unwrap().to_string(),
            branch: "feature".into()
        }
    );
    assert!(local_source(&work, false).is_ok(), "pushed");

    std::fs::write(work.join("b.txt"), "b").unwrap();
    let err = local_source(&work, true).unwrap_err().to_string();
    assert!(err.contains("uncommitted changes (1 file(s))"), "{err}");
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "b"]);
    let err = local_source(&work, false).unwrap_err().to_string();
    assert!(
        err.contains("1 commit(s) on feature aren't on origin"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn run_json(status: &str, result: Value) -> Value {
    json!({
        "id": "0b9e6f1c-3f6c-4a7e-9d61-2a1b3c4d5e6f", "org": "acme", "status": status,
        "repo": "https://github.com/acme/app.git", "git_ref": "feature", "task": "fix the bug",
        "placement": "auto", "labels": ["gpu"], "runner_kind": if status == "queued" { Value::Null } else { json!("hosted") },
        "attempts": if status == "queued" { 0 } else { 1 }, "cancel_requested": false,
        "created_at": "2026-10-14T12:00:00Z", "started_at": if status == "queued" { Value::Null } else { json!("2026-10-14T12:00:05Z") },
        "finished_at": if status == "succeeded" { json!("2026-10-14T12:04:05Z") } else { Value::Null },
        "result": result,
    })
}

/// Queue a run, then follow it from queued through running to its branch.
#[tokio::test]
async fn submit_and_follow_to_completion() {
    type Created = Arc<Mutex<Option<(String, Option<String>, Value)>>>;
    let created: Created = Arc::default();
    let afters: Arc<Mutex<Vec<u64>>> = Arc::default();
    let gets = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let (cr, af) = (created.clone(), afters.clone());
    let app =
        axum::Router::new()
            .route(
                "/v1/orgs/{org}/runs",
                axum::routing::post(
                    move |axum::extract::Path(org): axum::extract::Path<String>,
                          h: axum::http::HeaderMap,
                          axum::Json(b): axum::Json<Value>| async move {
                        let auth = h
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string);
                        *cr.lock().unwrap() = Some((org, auth, b));
                        (
                            axum::http::StatusCode::CREATED,
                            axum::Json(run_json("queued", Value::Null)),
                        )
                    },
                ),
            )
            .route(
                "/v1/runs/{id}",
                axum::routing::get(move || async move {
                    match gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                        0 => axum::Json(run_json("queued", Value::Null)),
                        1 => axum::Json(run_json("running", Value::Null)),
                        _ => axum::Json(run_json(
                            "succeeded",
                            json!({"summary": "Fixed it.", "branch": "chanto/run/ab12cd34"}),
                        )),
                    }
                }),
            )
            .route(
                "/v1/runs/{id}/events",
                axum::routing::get(
                    move |axum::extract::Query(q): axum::extract::Query<
                        std::collections::HashMap<String, String>,
                    >| async move {
                        let after: u64 = q.get("after").and_then(|a| a.parse().ok()).unwrap_or(0);
                        af.lock().unwrap().push(after);
                        let all = [
                            json!({"type": "assistant_start"}),
                            json!({"type": "text", "text": "Looking at the bug."}),
                            json!({"type": "tool_start", "label": "Bash cargo test"}),
                        ];
                        let events: Vec<Value> = all
                    .iter()
                    .enumerate()
                    .map(|(i, d)| (i as u64 + 1, d))
                    .filter(|(seq, _)| *seq > after)
                    .map(|(seq, d)| json!({"seq": seq, "at": "2026-10-14T12:00:06Z", "data": d}))
                    .collect();
                        axum::Json(json!({"events": events}))
                    },
                ),
            );
    let url = serve(app).await;
    let c = Cloud::new(&url, "crt_token").unwrap();
    let req = CreateRun {
        repo: "https://github.com/acme/app.git".into(),
        git_ref: Some("feature".into()),
        task: "fix the bug".into(),
        placement: Some(Placement::SelfHosted),
        labels: vec!["gpu".into()],
        ..Default::default()
    };
    let queued = c.submit("acme", &req).await.unwrap();
    assert_eq!(queued.status, RunStatus::Queued);
    let (org, auth, body) = created.lock().unwrap().clone().unwrap();
    assert_eq!(
        (org.as_str(), auth.as_deref()),
        ("acme", Some("Bearer crt_token"))
    );
    assert_eq!(
        serde_json::from_value::<CreateRun>(body.clone()).unwrap(),
        req
    );
    assert_eq!(body["placement"], "self_hosted");

    let done = follow(&c, &queued.id, Duration::from_millis(10))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.status, RunStatus::Succeeded);
    assert_eq!(
        done.result.as_ref().and_then(|r| r.branch.as_deref()),
        Some("chanto/run/ab12cd34")
    );
    let afters = afters.lock().unwrap().clone();
    assert_eq!(afters.first(), Some(&0));
    assert_eq!(
        afters.last(),
        Some(&3),
        "asks only for events it hasn't seen: {afters:?}"
    );
    let text = finished_text(&done);
    assert!(
        text.contains("git fetch origin chanto/run/ab12cd34"),
        "{text}"
    );
    assert!(
        text.contains("https://github.com/acme/app/compare/feature...chanto/run/ab12cd34"),
        "{text}"
    );
}

#[tokio::test]
async fn server_refusals_are_shown_with_what_to_do() {
    let app = axum::Router::new()
        .route(
            "/v1/orgs/{org}/runs",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::PAYMENT_REQUIRED,
                    axum::Json(json!({"error": "quota_exceeded", "message": "hosted runners need a card on file: add one in acme's billing settings"})),
                )
            }),
        )
        .route(
            "/v1/me",
            axum::routing::get(|| async { (axum::http::StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "unauthorized", "message": "invalid token"}))) }),
        );
    let url = serve(app).await;
    let c = Cloud::new(&url, "crt_old").unwrap();
    let req = CreateRun {
        repo: "https://github.com/acme/app.git".into(),
        task: "t".into(),
        placement: Some(Placement::Hosted),
        ..Default::default()
    };
    let err = c.submit("acme", &req).await.unwrap_err().to_string();
    assert!(
        err.starts_with(
            "Chanto cloud: hosted runners need a card on file: add one in acme's billing settings"
        ),
        "{err}"
    );
    assert!(err.contains("--placement self-hosted"), "{err}");
    let err = c.get::<User>("/v1/me").await.unwrap_err().to_string();
    assert!(
        err.contains("invalid token") && err.contains("chanto cloud login"),
        "{err}"
    );
    let err = c.get::<Value>("/v1/nothing").await.unwrap_err().to_string();
    assert!(
        err.contains("doesn't look like a Chanto cloud server"),
        "{err}"
    );
}
