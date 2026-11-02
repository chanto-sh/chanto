use super::*;
use chanto_proto::runs::{GitCredential, ModelAccess};
use std::collections::BTreeMap;

fn job(repo: &str) -> Job {
    Job {
        run_id: "r1".into(),
        repo: repo.into(),
        git_ref: None,
        task: "t".into(),
        labels: vec![],
        attempt: 1,
        lease_secs: 60,
        script: None,
        model_runs: Default::default(),
        checks: None,
    }
}

fn creds(expires_in: Option<chrono::Duration>) -> RunCredentials {
    RunCredentials {
        git: Some(GitCredential {
            url: "https://github.com/acme/app.git".into(),
            username: "x-access-token".into(),
            password: "ghs_secret".into(),
            expires_at: expires_in.map(|d| chrono::Utc::now() + d),
        }),
        env: BTreeMap::from([
            ("DATABASE_URL".to_string(), "postgres://x".to_string()),
            ("CHANTO_MODEL".to_string(), "sneaky".to_string()),
            ("1BAD".to_string(), "x".to_string()),
        ]),
        model: Some(ModelAccess {
            provider: "anthropic".into(),
            base_url: "https://cloud.example/v1/model/anthropic".into(),
            api_key: "hrm_run".into(),
            model: None,
        }),
        tools: Some(chanto_proto::runs::RunTools {
            url: "https://cloud.example".into(),
            token: "crx_run".into(),
        }),
    }
}

/// A repository with one commit, as a clone URL.
fn origin() -> (std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("chanto-origin-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap()
                .status
                .success(),
            "{args:?}"
        )
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("prices.json"), "{\"a\": 1}\n").unwrap();
    git(&["add", "-A"]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "-m",
        "first",
    ]);
    let url = dir.to_string_lossy().to_string();
    (dir, url)
}

/// A refreshed credential (fetched again before the push) brings its
/// org secrets too, and they're masked like the first ones.
#[test]
fn refreshed_credentials_are_masked() {
    let _g = crate::secrets::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut creds = RunCredentials::default();
    creds.env.insert(
        "ROTATED_DEPLOY_TOKEN".into(),
        "rotated-org-secret-8e7d".into(),
    );
    // As `finish` calls it: no worker environment.
    register_secrets(&creds, &[]);
    assert_eq!(
        crate::secrets::redact_always("x rotated-org-secret-8e7d"),
        "x [REDACTED:ROTATED_DEPLOY_TOKEN]"
    );
}

#[tokio::test]
async fn scripts_run_in_a_checkout_and_say_what_they_did() {
    // Script git directories go under the Chanto home: keep it still.
    let _home = crate::runs::tests::TestHome::new("pull-script-unit");
    let (origin, url) = origin();
    let mut j = job(&url);
    j.run_id = format!("{}", uuid::Uuid::new_v4().simple());
    let env = vec![
        ("PRICE".to_string(), "2".to_string()),
        ("CHANTO_API_KEY".to_string(), "hrm_x".to_string()),
    ];

    // Changes files: the run sees them, and the model key never reaches the script.
    let o = run_script(&j, &url, &[], &script_env(&env), "echo \"{\\\"a\\\": $PRICE}\" > prices.json\necho \"key=${CHANTO_API_KEY:-none}\"\necho updated").await.unwrap();
    assert!(o.ok && o.changed && o.base.is_some(), "{}", o.output);
    assert!(
        o.output.contains("key=none") && o.output.contains("updated"),
        "{}",
        o.output
    );
    assert!(
        o.report().contains("Script finished, with changes")
            && o.note_for_model().contains("committed on the branch")
    );
    // Pushed as a branch of the origin.
    o.push(&url, &[], "chanto/run/test", "Keep prices current")
        .await
        .unwrap();
    let branches = std::process::Command::new("git")
        .args(["branch", "--list", "chanto/*"])
        .current_dir(&origin)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&branches.stdout).contains("chanto/run/test"));
    o.cleanup();

    // Nor do this machine's secrets, and the git directory is kept out of
    // the checkout, where the script could configure it.
    let name = format!("CHANTO_PULL_PROBE_{}_TOKEN", std::process::id());
    unsafe { std::env::set_var(&name, "machine-secret") };
    let o2 = run_script(
        &j,
        &url,
        &[],
        &[],
        &format!("echo \"[${{{name}:-withheld}}]\""),
    )
    .await;
    unsafe { std::env::remove_var(&name) };
    let o2 = o2.unwrap();
    assert!(o2.output.contains("[withheld]"), "{}", o2.output);
    assert!(o2.dir.join(".git").is_file() && o2.git_dir.join("HEAD").is_file());
    assert!(!o2.git_dir.starts_with(&o2.dir));
    o2.cleanup();

    // Stops at the first failing command, and says so.
    let o = run_script(&j, &url, &[], &[], "echo before\nfalse\necho after")
        .await
        .unwrap();
    assert!(!o.ok && o.code == Some(1) && !o.changed);
    assert!(o.output.contains("before") && !o.output.contains("after"));
    assert!(o.report().contains("Script failed (exit 1)"));
    o.cleanup();

    // What it prints of the run's secrets is masked before it's sent.
    {
        let _g = crate::secrets::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut creds = RunCredentials::default();
        creds
            .env
            .insert("ORG_DEPLOY_TOKEN".into(), "org-deploy-secret-31f2".into());
        let (env, _) = worker_env(&creds, false);
        register_secrets(&creds, &env);
        let o = run_script(
            &j,
            &url,
            &[],
            &script_env(&env),
            "echo token=$ORG_DEPLOY_TOKEN",
        )
        .await
        .unwrap();
        assert!(
            o.output.contains("token=[REDACTED:ORG_DEPLOY_TOKEN]"),
            "{}",
            o.output
        );
        o.cleanup();
    }

    // Nothing to change.
    let o = run_script(&j, &url, &[], &[], "echo prices are current")
        .await
        .unwrap();
    assert!(o.ok && !o.changed && o.note_for_model().contains("changed no files"));
    o.cleanup();
    let _ = std::fs::remove_dir_all(&origin);
}

#[test]
fn worker_env_carries_secrets_and_the_model_endpoint() {
    let (env, skipped) = worker_env(&creds(None), false);
    let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    assert_eq!(get("DATABASE_URL"), Some("postgres://x"));
    assert_eq!(get("CHANTO_RUN_SECRETS"), Some("DATABASE_URL"));
    assert_eq!(
        (
            get("CHANTO_PROVIDER"),
            get("CHANTO_BASE_URL"),
            get("CHANTO_API_KEY")
        ),
        (
            Some("anthropic"),
            Some("https://cloud.example/v1/model/anthropic"),
            Some("hrm_run")
        )
    );
    assert_eq!(
        get("CHANTO_MODEL"),
        None,
        "an org secret can't pick the model, and the server didn't"
    );
    assert_eq!(
        (get("CHANTO_CLOUD_URL"), get("CHANTO_RUN_TOKEN")),
        (Some("https://cloud.example"), Some("crx_run"))
    );
    assert_eq!(skipped, ["1BAD", "CHANTO_MODEL"]);
    let (env, skipped) = worker_env(&RunCredentials::default(), false);
    assert!(env.is_empty() && skipped.is_empty());
    // --own-model: the secrets still come, the server's model doesn't.
    let (env, _) = worker_env(&creds(None), true);
    assert!(env.iter().any(|(k, _)| k == "DATABASE_URL"));
    assert!(!env.iter().any(|(k, _)| k.starts_with("CHANTO_PROVIDER")
        || k == "CHANTO_BASE_URL"
        || k == "CHANTO_API_KEY"));
}

#[test]
fn git_source_prefers_the_credential_url() {
    let (url, auth) = git_source(&job("git@github.com:acme/app.git"), &creds(None));
    assert_eq!(url, "https://github.com/acme/app.git");
    assert!(auth.contains(&(
        "GIT_CONFIG_KEY_0".to_string(),
        "http.https://github.com/acme/app.git.extraHeader".to_string()
    )));
    assert!(
        !auth.iter().any(|(_, v)| v.contains("ghs_secret")),
        "only base64 in the header value"
    );
    assert_eq!(
        git_source(
            &job("https://example.com/r.git"),
            &RunCredentials::default()
        ),
        ("https://example.com/r.git".to_string(), vec![])
    );
    assert!(expiring(&creds(Some(chrono::Duration::minutes(2)))));
    assert!(!expiring(&creds(Some(chrono::Duration::minutes(50)))) && !expiring(&creds(None)));
}

/// A server answering the credentials route with `status` and `body`.
async fn server(status: u16, body: Value) -> Client {
    let app = axum::Router::new().route(
        "/v1/runner/runs/{id}/credentials",
        axum::routing::post(move || async move {
            (
                axum::http::StatusCode::from_u16(status).unwrap(),
                axum::Json(body),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Client {
        http: crate::http::client(),
        url,
        token: "t".into(),
    }
}

#[tokio::test]
async fn credentials_from_old_and_new_servers() {
    let c = server(200, serde_json::to_value(creds(None)).unwrap()).await;
    let j = job("x");
    assert_eq!(credentials(&c, &j).await.unwrap(), creds(None));
    let c = server(404, json!({"error": "not_found", "message": "not found"})).await;
    assert_eq!(
        credentials(&c, &j).await.unwrap(),
        RunCredentials::default(),
        "an older server"
    );
    let c = server(409, json!({"error": "conflict", "message": "stop"})).await;
    assert!(is_lost_lease(&credentials(&c, &j).await.unwrap_err()));
}

/// A server error is retried; events stay queued until the server takes them.
#[tokio::test]
async fn transient_failures_are_retried_and_events_kept() {
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let (c1, c2) = (calls.clone(), calls.clone());
    let fail_first = move |n: u32| {
        if n == 0 {
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        } else {
            axum::http::StatusCode::OK
        }
    };
    let app = axum::Router::new()
        .route(
            "/v1/runner/runs/{id}/credentials",
            axum::routing::post(move || async move {
                let n = c1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                (
                    fail_first(n),
                    axum::Json(serde_json::to_value(creds(None)).unwrap()),
                )
            }),
        )
        .route(
            "/v1/runner/runs/{id}/events",
            axum::routing::post(move || async move {
                let n = c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                match n {
                    2 => axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    4 => axum::http::StatusCode::BAD_REQUEST,
                    _ => axum::http::StatusCode::NO_CONTENT,
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let c = Client {
        http: crate::http::client(),
        url,
        token: "t".into(),
    };

    let j = job("x");
    assert_eq!(
        credentials(&c, &j).await.unwrap(),
        creds(None),
        "retried after a 503"
    );
    let mut batch = vec![json!({"type": "a"})];
    let e = send_events(&c, &j, &mut batch).await.unwrap_err();
    assert!(is_transient(&e));
    assert_eq!(batch.len(), 1, "kept for the next try");
    send_events(&c, &j, &mut batch).await.unwrap();
    assert!(batch.is_empty());
    // Refused outright (call 4): dropped, and the rest still go.
    let mut batch = vec![json!({"type": "b"})];
    send_events(&c, &j, &mut batch).await.unwrap();
    assert!(batch.is_empty());
}

/// git really sends the credential, and only as a header.
#[tokio::test]
async fn git_authenticates_with_the_header() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
    let s = seen.clone();
    let app =
        axum::Router::new().fallback(move |uri: axum::http::Uri, h: axum::http::HeaderMap| {
            let s = s.clone();
            async move {
                let auth = h
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                s.lock().unwrap().push((uri.to_string(), auth));
                axum::http::StatusCode::NOT_FOUND
            }
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/acme/app.git", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // A fresh token per run: nothing else in the output can match it by chance.
    let token = format!("ghs_{}", uuid::Uuid::new_v4().simple());
    let auth = super::super::git::auth_env(&url, "x-access-token", &token);
    let u = url.clone();
    let err = tokio::task::spawn_blocking(move || default_branch(&u, &auth))
        .await
        .unwrap()
        .unwrap_err();
    assert!(!format!("{err:#}").contains(&token));
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter()
            .any(|(path, _)| path.starts_with("/acme/app.git/info/refs")),
        "{seen:?}"
    );
    use base64::Engine as _;
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"))
    );
    assert!(seen.iter().all(|(_, a)| *a == expected), "{seen:?}");
}
