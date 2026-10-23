//! `chanto cloud ...`, `chanto run --cloud` and `chanto runs ... --cloud`
//! against a fake Chanto cloud on localhost, in a temp home.

use super::*;
use crate::runs::tests::{TestHome, sh};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

const ID_A: &str = "0b9e6f1c-3f6c-4a7e-9d61-2a1b3c4d5e6f";
const ID_B: &str = "0b9e7777-3f6c-4a7e-9d61-2a1b3c4d5e6f";
const ID_C: &str = "c0ffee00-3f6c-4a7e-9d61-2a1b3c4d5e6f";

fn run_json(id: &str, status: &str, result: Value) -> Value {
    let started = status != "queued";
    json!({
        "id": id, "org": "acme", "status": status,
        "repo": "https://github.com/acme/app.git", "git_ref": "feature", "task": "fix the bug",
        "placement": "auto", "labels": ["gpu"], "runner_kind": if started { json!("self_hosted") } else { Value::Null },
        "attempts": if started { 1 } else { 0 }, "cancel_requested": false,
        "created_at": "2026-10-14T12:00:00Z", "started_at": if started { json!("2026-10-14T12:00:05Z") } else { Value::Null },
        "finished_at": if started { json!("2026-10-14T12:04:05Z") } else { Value::Null },
        "result": result,
    })
}

fn run(v: Value) -> Run {
    serde_json::from_value(v).unwrap()
}

/// What the fake cloud holds and was asked.
struct Fake {
    runs: Mutex<HashMap<String, Value>>,
    /// Status a newly created run is reported with afterwards.
    then: Mutex<String>,
    created: Mutex<Vec<(String, Value)>>,
    listed: Mutex<Vec<String>>,
    cancelled: Mutex<Vec<String>>,
    revoke: AtomicU16,
}

async fn serve(f: Arc<Fake>) -> String {
    let app = axum::Router::new()
        .route(
            "/v1/auth/providers",
            get(|| async {
                axum::Json(json!({"providers": [
                    {"id": "github", "name": "GitHub", "browser": true, "device": true},
                    {"id": "google", "name": "Google", "browser": true, "device": false}]}))
            }),
        )
        .route(
            "/v1/auth/browser/start",
            post(|| async {
                axum::Json(json!({"authorize_url": "https://github.example/authorize", "poll_code": "pc", "user_code": "WXYZ", "expires_in": 60}))
            }),
        )
        .route(
            "/v1/auth/browser/poll",
            post(|| async {
                axum::Json(json!({"status": "complete", "token": "crt_new", "user": {"id": "u1", "login": "octo", "name": "Octo Cat"}}))
            }),
        )
        .route(
            "/v1/auth/token",
            delete(|State(f): State<Arc<Fake>>| async move {
                match f.revoke.load(Ordering::SeqCst) {
                    204 => StatusCode::NO_CONTENT.into_response(),
                    s => (StatusCode::from_u16(s).unwrap(), axum::Json(json!({"error": "internal", "message": "database down"}))).into_response(),
                }
            }),
        )
        .route(
            "/v1/me",
            get(|| async { axum::Json(json!({"id": "u1", "login": "octo", "name": "Octo Cat"})) }),
        )
        .route(
            "/v1/orgs",
            get(|| async {
                axum::Json(json!({"orgs": [
                    {"slug": "octo", "name": "octo", "personal": true, "role": "owner", "plan": "free"},
                    {"slug": "acme", "name": "Acme Inc", "role": "admin", "plan": "team", "hosted_runners": true, "suspended": "unpaid invoice"}]}))
            }),
        )
        .route(
            "/v1/orgs/{org}/runs",
            get(
                |State(f): State<Arc<Fake>>, UrlPath(org): UrlPath<String>, Query(q): Query<HashMap<String, String>>| async move {
                    let mut qs: Vec<String> = q.iter().map(|(k, v)| format!("{k}={v}")).collect();
                    qs.sort();
                    f.listed.lock().unwrap().push(format!("{org}?{}", qs.join("&")));
                    let mut runs: Vec<Value> = f.runs.lock().unwrap().values().cloned().collect();
                    runs.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
                    axum::Json(json!({"runs": runs}))
                },
            )
            .post(
                |State(f): State<Arc<Fake>>, UrlPath(org): UrlPath<String>, axum::Json(b): axum::Json<Value>| async move {
                    f.created.lock().unwrap().push((org, b.clone()));
                    let mut queued = run_json(ID_C, "queued", Value::Null);
                    queued["task"] = b["task"].clone();
                    let then = f.then.lock().unwrap().clone();
                    let mut later = run_json(ID_C, &then, json!({"summary": "Done.", "branch": "chanto/run/c0ffee00"}));
                    later["task"] = b["task"].clone();
                    f.runs.lock().unwrap().insert(ID_C.into(), later);
                    (StatusCode::CREATED, axum::Json(queued))
                },
            ),
        )
        .route(
            "/v1/runs/{id}",
            get(|State(f): State<Arc<Fake>>, UrlPath(id): UrlPath<String>| async move {
                match f.runs.lock().unwrap().get(&id) {
                    Some(r) => axum::Json(r.clone()).into_response(),
                    None => (StatusCode::NOT_FOUND, axum::Json(json!({"error": "not_found", "message": "no such run"}))).into_response(),
                }
            }),
        )
        .route(
            "/v1/runs/{id}/events",
            get(|| async {
                axum::Json(json!({"events": [
                    {"seq": 1, "at": "2026-10-14T12:00:06Z", "data": {"type": "permission_request", "label": "Bash(rm -rf build)"}},
                    {"seq": 2, "at": "2026-10-14T12:00:07Z", "data": {"type": "question", "question": "Which DB?"}},
                    {"seq": 3, "at": "2026-10-14T12:00:08Z", "data": {"type": "text", "text": "Done"}}]}))
            }),
        )
        .route(
            "/v1/runs/{id}/cancel",
            post(|State(f): State<Arc<Fake>>, UrlPath(id): UrlPath<String>| async move {
                f.cancelled.lock().unwrap().push(id.clone());
                let r = f.runs.lock().unwrap().get(&id).cloned().unwrap();
                let mut r = r;
                if r["status"] == "queued" {
                    r["status"] = json!("cancelled");
                }
                axum::Json(r)
            }),
        )
        .with_state(f);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

/// A temp home with no cloud settings from the environment.
struct CloudHome {
    home: TestHome,
    prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
    fake: Arc<Fake>,
    url: String,
}

impl CloudHome {
    async fn new(name: &str) -> CloudHome {
        let home = TestHome::new(name);
        let keys = ["CHANTO_CLOUD_URL", "CHANTO_CLOUD_TOKEN", "CHANTO_CLOUD_ORG"];
        let prev = keys.iter().map(|k| (*k, std::env::var_os(k))).collect();
        for k in keys {
            // SAFETY: test-only, under TEST_HOME_LOCK (held by `home`).
            unsafe { std::env::remove_var(k) };
        }
        let fake = Arc::new(Fake {
            runs: Mutex::default(),
            then: Mutex::new("succeeded".into()),
            created: Mutex::default(),
            listed: Mutex::default(),
            cancelled: Mutex::default(),
            revoke: AtomicU16::new(204),
        });
        let url = serve(fake.clone()).await;
        CloudHome {
            home,
            prev,
            fake,
            url,
        }
    }

    fn add(&self, v: Value) {
        self.fake
            .runs
            .lock()
            .unwrap()
            .insert(v["id"].as_str().unwrap().to_string(), v);
    }
}

impl Drop for CloudHome {
    fn drop(&mut self) {
        for (k, v) in self.prev.drain(..) {
            if let Some(v) = v {
                // SAFETY: as above.
                unsafe { std::env::set_var(k, v) };
            }
        }
    }
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

async fn cmd(a: &[&str]) -> Result<String> {
    cloud_cmd(&args(a)).await
}

#[tokio::test]
async fn sign_in_status_orgs_and_sign_out() {
    let h = CloudHome::new("cloud-cmd").await;
    // Not signed in yet.
    assert!(
        cmd(&[])
            .await
            .unwrap()
            .starts_with("Not signed in to Chanto cloud.")
    );
    assert_eq!(
        cmd(&["logout"]).await.unwrap(),
        "You're not signed in to Chanto cloud."
    );
    assert!(format!("{:#}", cmd(&["org"]).await.unwrap_err()).contains("not signed in"));
    assert!(runs_cmd(&args(&["list"])).await.is_err());

    // Login flags.
    let e = |a: &'static [&'static str]| async move { format!("{:#}", cmd(a).await.unwrap_err()) };
    assert!(
        e(&["login", "--bogus"])
            .await
            .starts_with("unexpected argument --bogus")
    );
    assert!(e(&["login", "--url"]).await.contains("--url needs a value"));
    assert!(
        e(&["login", "--url=http://example.com"])
            .await
            .contains("must use https://")
    );
    assert_eq!(cmd(&["login", "--help"]).await.unwrap(), USAGE);

    let out = cmd(&["login", "--url", &format!("{}/", h.url), "--no-open"])
        .await
        .unwrap();
    assert!(
        out.starts_with(&format!(
            "Signed in to Chanto cloud ({}) as octo.\nOrgs (* = where runs go;",
            h.url
        )),
        "{out}"
    );
    assert!(out.contains("\n  * octo "), "personal org first: {out}");
    assert!(out.contains("acme (Acme Inc)"), "{out}");
    assert!(out.ends_with("`chanto run --cloud \"<task>\"`."), "{out}");
    let c = Cloud::signed_in().unwrap();
    assert_eq!((c.url.as_str(), c.token()), (h.url.as_str(), "crt_new"));
    // Again, with the provider named: the saved server is reused.
    cmd(&["login", "--provider=google", "--no-open"])
        .await
        .unwrap();

    let status = cmd(&["status"]).await.unwrap();
    assert!(
        status.starts_with(&format!(
            "Chanto cloud: {}\nSigned in as octo (Octo Cat)",
            h.url
        )),
        "{status}"
    );
    assert!(
        status.contains("* octo                 owner · free plan · hosted runners off · personal"),
        "{status}"
    );
    assert!(
        status.contains("  acme (Acme Inc)      admin · team plan · hosted runners on · SUSPENDED: unpaid invoice"),
        "{status}"
    );

    // Picking the default org.
    let e = format!("{:#}", cmd(&["org", "nope"]).await.unwrap_err());
    assert_eq!(e, "you're not in an org named nope; yours: octo, acme");
    assert_eq!(
        cmd(&["org", "acme"]).await.unwrap(),
        "Cloud runs go to acme unless you pass --org."
    );
    assert_eq!(default_org().as_deref(), Some("acme"));
    let orgs = cmd(&["org"]).await.unwrap();
    assert!(orgs.lines().nth(1).unwrap().starts_with("* acme"), "{orgs}");
    assert_eq!(
        resolve_org(&c, None).await.unwrap(),
        "acme",
        "the saved default"
    );
    assert_eq!(resolve_org(&c, Some("x")).await.unwrap(), "x");
    set_default_org(Some("gone")).unwrap();
    let status = cmd(&["whoami"]).await.unwrap();
    assert!(
        status.contains("The default org gone isn't one of yours"),
        "{status}"
    );
    set_default_org(None).unwrap();
    assert_eq!(default_org(), None);
    assert_eq!(resolve_org(&c, None).await.unwrap(), "octo");

    assert_eq!(cmd(&["help"]).await.unwrap(), USAGE);
    assert!(
        format!("{:#}", cmd(&["frob"]).await.unwrap_err()).starts_with("unknown subcommand `frob`")
    );

    // Sign out: the token is revoked and forgotten.
    let out = cmd(&["logout"]).await.unwrap();
    assert_eq!(
        out,
        format!(
            "Signed out of Chanto cloud ({}); the token is revoked.",
            h.url
        )
    );
    assert!(Cloud::signed_in().is_err());
    // A server that can't revoke it: forgotten anyway, and said so.
    cmd(&["login", "--url", &h.url, "--no-open"]).await.unwrap();
    h.fake.revoke.store(500, Ordering::SeqCst);
    let out = cmd(&["logout"]).await.unwrap();
    assert!(
        out.starts_with("Signed out of Chanto cloud on this machine. The server didn't revoke the token: Chanto cloud: database down"),
        "{out}"
    );
    assert!(crate::auth::get(CREDENTIAL).is_none());
}

#[tokio::test]
async fn runs_are_listed_shown_cancelled_and_followed() {
    let h = CloudHome::new("cloud-runs").await;
    save_login(&h.url, "crt_saved").unwrap();
    h.add(run_json(ID_A, "running", Value::Null));
    h.add(run_json(
        ID_B,
        "succeeded",
        json!({"summary": "Fixed.", "branch": "chanto/run/0b9e7777"}),
    ));
    h.add(run_json(ID_C, "queued", Value::Null));

    assert_eq!(runs_cmd(&args(&["--cloud"])).await.unwrap(), 0);
    runs_cmd(&args(&[
        "ls", "--org", "acme", "--status", "running", "--limit", "500",
    ]))
    .await
    .unwrap();
    assert_eq!(
        h.fake.listed.lock().unwrap().clone(),
        ["octo?limit=30", "acme?limit=100&status=running"]
    );

    // By a unique prefix, or the full id.
    assert_eq!(runs_cmd(&args(&["show", "c0ff"])).await.unwrap(), 0);
    assert_eq!(runs_cmd(&args(&["show", ID_B])).await.unwrap(), 0);
    let e = |a: &'static [&'static str]| async move {
        format!("{:#}", runs_cmd(&args(a)).await.unwrap_err())
    };
    assert_eq!(
        e(&["show", "0b9e"]).await,
        "0b9e matches 2 cloud runs; give more of the id"
    );
    assert!(
        e(&["show", "ffff"])
            .await
            .starts_with("no cloud run ffff among octo's")
    );
    assert_eq!(e(&["show"]).await, "usage: chanto runs show <id> --cloud");
    assert!(
        e(&["rm", "x"])
            .await
            .starts_with("`chanto runs rm` isn't available")
    );
    assert!(e(&["--bogus"]).await.starts_with("unknown flag --bogus"));
    assert_eq!(
        e(&["--limit", "many"]).await,
        "--limit needs a number: invalid digit found in string"
    );
    assert_eq!(e(&["--org"]).await, "--org needs a value");
    assert_eq!(runs_cmd(&args(&["--help"])).await.unwrap(), 0);

    // Cancel: queued ones stop at once, running ones at their next heartbeat.
    for id in ["c0ff", "0b9e6", "0b9e7"] {
        assert_eq!(runs_cmd(&args(&["cancel", id])).await.unwrap(), 0);
    }
    assert_eq!(h.fake.cancelled.lock().unwrap().clone(), [ID_C, ID_A, ID_B]);

    // Following a finished run: 0 when it succeeded, 1 otherwise.
    assert_eq!(runs_cmd(&args(&["attach", "0b9e7"])).await.unwrap(), 0);
    h.add(run_json(
        ID_A,
        "failed",
        json!({"summary": "no runner took the run in time"}),
    ));
    assert_eq!(runs_cmd(&args(&["follow", "0b9e6"])).await.unwrap(), 1);

    // The details as shown.
    let mut r = run_json(ID_A, "running", Value::Null);
    r["cancel_requested"] = json!(true);
    let text = detail_text(&run(r));
    assert!(
        text.starts_with(&format!("Cloud run {ID_A}  running\nOrg:       acme")),
        "{text}"
    );
    assert!(text.contains("Repo:      https://github.com/acme/app.git @ feature"));
    assert!(text.contains("Placement: auto · labels gpu · runner self-hosted · attempts 1"));
    assert!(text.contains("Cancelling (the runner stops at its next heartbeat)."));
    let text = detail_text(&run(run_json(
        ID_B,
        "succeeded",
        json!({"summary": "Fixed.", "branch": "b"}),
    )));
    assert!(
        text.contains("● Cloud run 0b9e7777 succeeded · 4m00s\n\nFixed.\n"),
        "{text}"
    );
}

#[tokio::test]
async fn following_retries_a_server_that_is_briefly_unreachable() {
    let c = Cloud::new("http://127.0.0.1:1", "t");
    let e = follow(&c, ID_A, Duration::from_millis(1))
        .await
        .unwrap_err();
    assert!(
        format!("{e:#}").contains("contacting Chanto cloud at http://127.0.0.1:1"),
        "{e:#}"
    );
}

/// A checkout whose branch `feature` is pushed to a local "origin".
fn pushed_checkout(home: &TestHome) -> (std::path::PathBuf, std::path::PathBuf) {
    let origin = home.dir.join("origin.git");
    sh(
        &home.dir,
        &["init", "-q", "--bare", &origin.display().to_string()],
    );
    let work = crate::runs::tests::make_repo(home);
    sh(&work, &["checkout", "-q", "-b", "feature"]);
    sh(
        &work,
        &["remote", "add", "origin", &origin.display().to_string()],
    );
    sh(&work, &["push", "-q", "-u", "origin", "feature"]);
    (work, origin)
}

#[tokio::test]
async fn run_cloud_queues_the_pushed_branch_and_follows_it() {
    let h = CloudHome::new("cloud-run").await;
    let (work, origin) = pushed_checkout(&h.home);
    let w = work.display().to_string();
    // Signed out: refused before anything is sent.
    assert!(
        format!(
            "{:#}",
            run_cmd(&args(&["-C", &w, "fix it"])).await.unwrap_err()
        )
        .contains("not signed in")
    );
    save_login(&h.url, "crt_saved").unwrap();

    let code = run_cmd(&args(&[
        "--cloud",
        "-C",
        &w,
        "--org=acme",
        "--label",
        "gpu",
        "--placement",
        "hosted",
        "--detach",
        "fix",
        "it",
    ]))
    .await
    .unwrap();
    assert_eq!(code, 0);
    let (org, body) = h.fake.created.lock().unwrap()[0].clone();
    assert_eq!(org, "acme");
    assert_eq!(body["repo"], json!(origin.display().to_string()));
    assert_eq!(
        (
            &body["git_ref"],
            &body["task"],
            &body["placement"],
            &body["labels"]
        ),
        (
            &json!("feature"),
            &json!("fix it"),
            &json!("hosted"),
            &json!(["gpu"])
        )
    );

    // Followed to the end: the exit code says how it went.
    assert_eq!(
        run_cmd(&args(&["--cd", &w, "--", "--weird", "task"]))
            .await
            .unwrap(),
        0
    );
    assert_eq!(h.fake.created.lock().unwrap()[1].1["task"], "--weird task");
    assert_eq!(h.fake.created.lock().unwrap()[1].0, "octo");
    *h.fake.then.lock().unwrap() = "failed".into();
    assert_eq!(run_cmd(&args(&["-C", &w, "break it"])).await.unwrap(), 1);

    // Unpushed work is refused.
    std::fs::write(work.join("new.txt"), "x").unwrap();
    let e = format!("{:#}", run_cmd(&args(&["-C", &w, "x"])).await.unwrap_err());
    assert!(e.contains("uncommitted changes"), "{e}");
    assert!(
        format!("{:#}", run_cmd(&args(&["--help"])).await.unwrap_err())
            .starts_with("usage: chanto cloud login")
    );
}

#[test]
fn local_sources_need_an_origin_and_a_branch() {
    let home = TestHome::new("cloud-src");
    let repo = crate::runs::tests::make_repo(&home);
    let e = local_source(&repo, false).unwrap_err().to_string();
    assert!(e.contains("no `origin` remote"), "{e}");
    sh(
        &repo,
        &["remote", "add", "origin", "git@github.com:acme/app.git"],
    );
    let head = sh(&repo, &["rev-parse", "HEAD"]);
    sh(&repo, &["checkout", "-q", "--detach", &head]);
    let e = local_source(&repo, false).unwrap_err().to_string();
    assert!(e.contains("HEAD is detached"), "{e}");
}

#[test]
fn finished_runs_say_where_their_work_is() {
    // A pull request was opened.
    let t = finished_text(&run(run_json(
        ID_A,
        "succeeded",
        json!({"summary": "  ", "branch": "chanto/run/x", "pr_url": "https://github.com/acme/app/pull/7"}),
    )));
    assert!(
        t.contains("\nPull request: https://github.com/acme/app/pull/7"),
        "{t}"
    );
    assert!(t.contains("git fetch origin chanto/run/x"));
    assert!(!t.contains("Open a pull request"), "{t}");
    assert!(!t.contains("\n\n  \n"));
    // Nothing changed.
    let t = finished_text(&run(run_json(ID_A, "succeeded", json!({"summary": "ok"}))));
    assert!(
        t.ends_with("No branch was pushed (the run made no changes)."),
        "{t}"
    );
    // Not on GitHub: no compare link.
    let mut v = run_json(ID_A, "succeeded", json!({"summary": "ok", "branch": "b"}));
    v["repo"] = json!("https://gitlab.example/acme/app.git");
    v["git_ref"] = Value::Null;
    assert!(!finished_text(&run(v)).contains("Open a pull request"));
    assert_eq!(
        compare_url("https://github.com/acme/app", None, "b").as_deref(),
        Some("https://github.com/acme/app/compare/b?expand=1")
    );
    // No runner took it.
    let t = finished_text(&run(run_json(
        ID_A,
        "failed",
        json!({"summary": "no runner took the run within 1h"}),
    )));
    assert!(
        t.contains("No runner was available: connect one of acme's own"),
        "{t}"
    );
    // Still queued: no duration yet.
    assert!(finished_text(&run(run_json(ID_A, "queued", Value::Null))).ends_with("queued · -"));

    assert!(table(&[]).starts_with("No cloud runs."));
    let t = table(&[
        run(run_json(ID_A, "queued", Value::Null)),
        run(run_json(
            ID_B,
            "succeeded",
            json!({"summary": "s", "branch": "chanto/run/0b9e7777"}),
        )),
    ]);
    let lines: Vec<&str> = t.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[1].starts_with("0b9e6f1c  queued     -  "), "{t}");
    assert!(
        lines[2].contains("self-hosted")
            && lines[2].contains("4m00s")
            && lines[2].contains("chanto/run/0b9e7777"),
        "{t}"
    );

    let mut q = run_json(ID_A, "queued", Value::Null);
    q["labels"] = json!([]);
    q["git_ref"] = Value::Null;
    let m = queued_message(&run(q));
    assert!(m.starts_with(&format!("Queued cloud run {ID_A} in acme: fix the bug\n  from the default branch of https://github.com/acme/app.git · placement auto\n")), "{m}");
    assert!(m.contains("chanto attach 0b9e6f1c --cloud"));
    let m = queued_message(&run(run_json(ID_A, "queued", Value::Null)));
    assert!(
        m.contains("from feature of") && m.contains(" · labels gpu"),
        "{m}"
    );

    assert_eq!(placement_label(Placement::SelfHosted), "self-hosted");
    assert_eq!(placement_label(Placement::Hosted), "hosted");
    assert_eq!(runner_label(Some(RunnerKind::Hosted)), "hosted");
    assert_eq!(runner_label(None), "-");
    assert_eq!(
        parse_placement("self_hosted").unwrap(),
        Placement::SelfHosted
    );
    assert_eq!(parse_placement("auto").unwrap(), Placement::Auto);
    assert_eq!(status_label(RunStatus::Cancelled), "cancelled");
    assert_eq!(short_id("abc"), "abc");
    assert!(check_url("https://x").is_ok() && check_url("http://localhost:3").is_ok());
    assert!(check_url("ftp://x").is_err());
    let mut in_text = false;
    for ev in [
        json!({"type": "permission_request", "label": "Bash"}),
        json!({"type": "question", "question": "q"}),
        json!({"type": "other"}),
        json!({"type": "text", "text": "t"}),
    ] {
        print_cloud_event(&ev, &mut in_text);
    }
    assert!(in_text);
}

#[test]
fn errors_without_a_server_message() {
    let e = api_error(
        reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        b"oops",
        "https://c",
        "GET /v1/me",
    );
    assert_eq!(
        e.to_string(),
        "Chanto cloud: GET /v1/me failed (500 Internal Server Error)"
    );
    let e = api_error(
        reqwest::StatusCode::NOT_FOUND,
        br#"{"error": "not_found", "message": "no org acme"}"#,
        "https://c",
        "POST /v1/orgs/acme/runs",
    );
    assert!(
        e.to_string()
            .ends_with("Check the org with `chanto cloud status`.")
    );
    let e = api_error(
        reqwest::StatusCode::NOT_IMPLEMENTED,
        br#"{"error": "not_configured", "message": "no gateway"}"#,
        "https://c",
        "GET /v1/models",
    );
    assert!(e.to_string().ends_with("ask its operator."));
}
