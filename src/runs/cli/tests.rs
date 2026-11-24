use super::*;

#[test]
fn sse_blocks_split_across_chunks() {
    let mut p = SseParser { buf: String::new() };
    assert!(p.push(b"data: {\"type\":\"te").is_empty());
    let v = p.push(b"xt\",\"text\":\"hi\"}\n\n: keep-alive\n\ndata: {\"seq\":2}\n\n");
    assert_eq!(v.len(), 2);
    assert_eq!(v[0]["text"], "hi");
    assert_eq!(v[1]["seq"], 2);
}

/// A loopback server whose `/health` says whether it hosts runs, recorded
/// in this test home's server.json. Returns its URL.
async fn fake_host(runs: bool) -> String {
    let app = axum::Router::new().route(
        "/health",
        axum::routing::get(move || async move { axum::Json(serde_json::json!({ "runs": runs })) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    url
}

fn record_server(url: &str) {
    let home = crate::config::home_dir();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        crate::server::server_file(),
        serde_json::json!({ "url": url, "token": "t", "pid": 1 }).to_string(),
    )
    .unwrap();
}

/// Runs `f` on a fresh runtime while holding the test home: server.json and
/// the runs store live there.
fn in_home<T>(f: impl std::future::Future<Output = T>) -> T {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let out = rt.block_on(f);
    let _ = std::fs::remove_file(crate::server::server_file());
    out
}

#[test]
fn an_existing_runs_host_is_used_instead_of_starting_another() {
    in_home(async {
        let store = crate::runs::Store::new(crate::runs::Store::default_dir());
        let _held = store.lock_host().unwrap().expect("lock");
        let url = fake_host(true).await;
        record_server(&url);
        let c = start_daemon().await.unwrap();
        assert_eq!(c.url, url);
    });
}

#[test]
fn a_server_that_does_not_host_runs_is_not_the_runs_host() {
    in_home(async {
        record_server(&fake_host(false).await);
        assert!(runs_host().await.is_none());
        let _ = std::fs::remove_file(crate::server::server_file());
        assert!(runs_host().await.is_none(), "no server.json");
    });
}

#[cfg(unix)]
#[test]
fn a_started_daemon_is_used_once_it_answers() {
    in_home(async {
        let store = crate::runs::Store::new(crate::runs::Store::default_dir());
        let url = fake_host(true).await;
        let child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let pid = child.id();
        let later = url.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            record_server(&later);
        });
        let c = await_daemon(child, &store, std::path::Path::new("daemon.log"))
            .await
            .unwrap();
        assert_eq!(c.url, url);
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status();
    });
}

#[cfg(unix)]
#[test]
fn a_daemon_that_exits_says_where_its_log_is() {
    in_home(async {
        let store = crate::runs::Store::new(crate::runs::Store::default_dir());
        let child = std::process::Command::new("true").spawn().unwrap();
        let e = await_daemon(child, &store, std::path::Path::new("/x/daemon.log"))
            .await
            .err()
            .expect("an error")
            .to_string();
        assert!(
            e.contains("didn't start") && e.contains("/x/daemon.log"),
            "{e}"
        );
    });
}

#[cfg(unix)]
#[test]
fn a_daemon_that_lost_the_race_hands_over_to_the_winner() {
    in_home(async {
        let store = crate::runs::Store::new(crate::runs::Store::default_dir());
        // The winner holds the store and answers once it has written server.json.
        let _winner = store.lock_host().unwrap().expect("lock");
        let url = fake_host(true).await;
        let child = std::process::Command::new("true").spawn().unwrap();
        let later = url.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            record_server(&later);
        });
        let c = await_daemon(child, &store, std::path::Path::new("daemon.log"))
            .await
            .unwrap();
        assert_eq!(c.url, url);
    });
}
