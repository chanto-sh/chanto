use super::*;

fn run_json(id: &str, status: &str) -> Value {
    json!({"id": id, "org": "acme", "status": status, "repo": "https://github.com/acme/app.git", "git_ref": "main",
           "task": "fix the flaky test", "placement": "auto", "labels": [], "runner_kind": null, "attempts": 1,
           "cancel_requested": false, "created_at": "2026-10-15T10:00:00Z", "started_at": null, "finished_at": null, "result": null})
}

#[tokio::test]
async fn lists_and_shows_runs_and_starts_agents() {
    let _lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let app = axum::Router::new()
        .route(
            "/v1/orgs/acme/runs",
            axum::routing::get(|| async {
                axum::Json(
                    json!({"runs": [run_json("0f9e8d7c-0000-0000-0000-000000000001", "running")]}),
                )
            }),
        )
        .route(
            "/v1/orgs/acme/agents",
            axum::routing::get(|| async { axum::Json(json!({"agents": []})) }),
        )
        .route(
            "/v1/orgs/acme/agents/docs/runs",
            axum::routing::post(|axum::Json(b): axum::Json<Value>| async move {
                assert_eq!(b["input"], "check issue 42");
                axum::Json(run_json("ab12cd34-0000-0000-0000-000000000002", "queued"))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    unsafe {
        std::env::set_var("CHANTO_CLOUD_TOKEN", "hct_test");
        std::env::set_var("CHANTO_CLOUD_URL", &url);
    }
    assert!(available());
    let dir = std::env::temp_dir();
    let env = crate::agent::tests::make_env(
        "http://localhost:1",
        &dir,
        crate::permissions::Mode::Default,
        32768,
    );
    let ctx = Ctx {
        env,
        cancel: tokio_util::sync::CancellationToken::new(),
        depth: 0,
        call_id: None,
        cwd_override: None,
        reads: None,
    };
    let out = ChantoCloud
        .run(json!({"action": "runs", "org": "acme"}), &ctx)
        .await
        .unwrap();
    assert!(
        !out.is_error
            && out.content.contains("0f9e8d7c")
            && out.content.contains("fix the flaky test"),
        "{}",
        out.content
    );
    let out = ChantoCloud
        .run(
            json!({"action": "show", "id": "0f9e8d7c", "org": "acme"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        out.content
            .contains("Cloud run 0f9e8d7c-0000-0000-0000-000000000001"),
        "{}",
        out.content
    );
    let out = ChantoCloud
        .run(json!({"action": "agents", "org": "acme"}), &ctx)
        .await
        .unwrap();
    assert!(out.content.contains("No agents yet"), "{}", out.content);
    let out = ChantoCloud.run(json!({"action": "run_agent", "name": "docs", "input": "check issue 42", "org": "acme"}), &ctx).await.unwrap();
    assert!(
        !out.is_error && out.content.contains("ab12cd34"),
        "{}",
        out.content
    );
    // Reading is free; starting or cancelling asks.
    assert_eq!(
        ChantoCloud.kind_for(&json!({"action": "runs"})),
        ToolKind::ReadOnly
    );
    assert_eq!(
        ChantoCloud.kind_for(&json!({"action": "start"})),
        ToolKind::External
    );
    assert_eq!(
        ChantoCloud.kind_for(&json!({"action": "cancel"})),
        ToolKind::External
    );
    unsafe {
        std::env::remove_var("CHANTO_CLOUD_TOKEN");
        std::env::remove_var("CHANTO_CLOUD_URL");
    }
}
