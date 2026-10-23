//! `chanto runner connect` against a fake run queue on localhost: jobs are
//! claimed, run in-process against a mock model in a clone of a local
//! "origin" repository, and their events, heartbeats, pushed branches and
//! results checked.

use super::*;
use crate::agent::tests::mock_server;
use crate::runs::tests::{TestHome, make_repo, needs_permission, sh, wait_for, write_hello};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};

/// What the fake queue was told, and how it answers.
struct Queue {
    events: Mutex<Vec<Value>>,
    completes: Mutex<Vec<CompleteRun>>,
    beats: AtomicUsize,
    cancel: AtomicBool,
    heartbeat_status: AtomicU16,
    creds_status: AtomicU16,
    creds: Mutex<Value>,
    claims: Mutex<VecDeque<(u16, Value)>>,
    registered: Mutex<Vec<Value>>,
    auth: Mutex<Vec<String>>,
}

impl Queue {
    fn new() -> Arc<Queue> {
        Arc::new(Queue {
            events: Mutex::default(),
            completes: Mutex::default(),
            beats: AtomicUsize::new(0),
            cancel: AtomicBool::new(false),
            heartbeat_status: AtomicU16::new(200),
            creds_status: AtomicU16::new(404),
            creds: Mutex::new(json!({})),
            claims: Mutex::default(),
            registered: Mutex::default(),
            auth: Mutex::default(),
        })
    }

    fn complete(&self) -> CompleteRun {
        let c = self.completes.lock().unwrap();
        assert_eq!(c.len(), 1, "{c:?}");
        c[0].clone()
    }

    fn event_types(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e["type"].as_str().unwrap_or("").to_string())
            .collect()
    }
}

fn api_err(status: u16, code: &str, msg: &str) -> axum::response::Response {
    (
        StatusCode::from_u16(status).unwrap(),
        axum::Json(json!({"error": code, "message": msg})),
    )
        .into_response()
}

async fn serve_queue(q: Arc<Queue>) -> String {
    use axum::routing::post;
    let app = axum::Router::new()
        .route(
            "/v1/runners/register",
            post(
                |State(q): State<Arc<Queue>>, axum::Json(b): axum::Json<Value>| async move {
                    q.registered.lock().unwrap().push(b);
                    axum::Json(json!({"runner_id": "rn_1", "runner_token": "rt_secret", "kind": "self_hosted"}))
                },
            ),
        )
        .route(
            "/v1/runner/claim",
            post(
                |State(q): State<Arc<Queue>>, h: axum::http::HeaderMap| async move {
                    let a = h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
                    q.auth.lock().unwrap().push(a.to_string());
                    match q.claims.lock().unwrap().pop_front() {
                        Some((200, job)) => axum::Json(job).into_response(),
                        Some((s, _)) => api_err(s, "forbidden", "no more"),
                        None => api_err(403, "forbidden", "no more"),
                    }
                },
            ),
        )
        .route(
            "/v1/runner/runs/{id}/credentials",
            post(|State(q): State<Arc<Queue>>| async move {
                match q.creds_status.load(Ordering::SeqCst) {
                    200 => axum::Json(q.creds.lock().unwrap().clone()).into_response(),
                    409 => api_err(409, "conflict", "another runner has it"),
                    s => api_err(s, "forbidden", "no credentials for you"),
                }
            }),
        )
        .route(
            "/v1/runner/runs/{id}/events",
            post(
                |State(q): State<Arc<Queue>>, axum::Json(b): axum::Json<AppendEvents>| async move {
                    q.events.lock().unwrap().extend(b.events);
                    StatusCode::NO_CONTENT
                },
            ),
        )
        .route(
            "/v1/runner/runs/{id}/heartbeat",
            post(|State(q): State<Arc<Queue>>| async move {
                q.beats.fetch_add(1, Ordering::SeqCst);
                match q.heartbeat_status.load(Ordering::SeqCst) {
                    409 => api_err(409, "conflict", "lease lost"),
                    _ => axum::Json(json!({"cancel_requested": q.cancel.load(Ordering::SeqCst), "lease_secs": 60}))
                        .into_response(),
                }
            }),
        )
        .route(
            "/v1/runner/runs/{id}/complete",
            post(
                |State(q): State<Arc<Queue>>, axum::Json(b): axum::Json<CompleteRun>| async move {
                    q.completes.lock().unwrap().push(b);
                    StatusCode::NO_CONTENT
                },
            ),
        )
        .with_state(q);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

/// A temp home whose model is the mock at `base` (pulled runs use the
/// machine's own configuration), and the runner's manager.
fn runner_home(name: &str, base: &str) -> (TestHome, Arc<Manager>) {
    let home = TestHome::new(name);
    let cfg = home.dir.join("home").join("config.toml");
    let text = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(
        &cfg,
        format!(
            "provider = \"openai\"\nbase_url = \"{base}\"\nmodel = \"mock\"\n\n[background]\nsandbox = false\nverify = false\n{text}"
        ),
    )
    .unwrap();
    let m = Manager::open(Store::new(store_dir()), Launcher::InProcess, 1);
    (home, m)
}

fn job_for(origin: &Path, task: &str) -> Job {
    Job {
        run_id: uuid::Uuid::new_v4().simple().to_string(),
        repo: origin.display().to_string(),
        git_ref: None,
        task: task.into(),
        labels: vec![],
        attempt: 1,
        lease_secs: 60,
        script: None,
        model_runs: Default::default(),
        checks: None,
    }
}

fn client(url: &str) -> Client {
    Client {
        http: crate::http::client(),
        url: url.into(),
        token: "rt_secret".into(),
    }
}

fn branches(repo: &Path) -> String {
    sh(repo, &["branch", "--list", "chanto/*"])
}

#[tokio::test]
async fn a_model_run_is_pushed_as_a_branch_and_reported() {
    let (base, _) = mock_server(write_hello()).await;
    let (home, m) = runner_home("pull-model", &base);
    let origin = make_repo(&home);
    let q = Queue::new();
    let c = client(&serve_queue(q.clone()).await);
    let mut job = job_for(&origin, "create hello.txt");
    job.checks = Some(vec!["true".into()]);
    let status = execute(&c, &m, &job, None, false).await.unwrap();
    assert_eq!(status, RunStatus::Succeeded);
    let done = q.complete();
    assert_eq!(done.status, RunStatus::Succeeded);
    let branch = done.result.branch.clone().expect("a pushed branch");
    assert!(branch.starts_with("chanto/run/"), "{branch}");
    assert_eq!(sh(&origin, &["show", &format!("{branch}:hello.txt")]), "hi");
    assert!(done.result.summary.contains("Created hello.txt."));
    assert_eq!(
        done.result.commit.as_deref(),
        Some(sh(&origin, &["rev-parse", "main"]).as_str())
    );
    // Events went up, streaming noise didn't, and the run ended done.
    let types = q.event_types();
    assert!(types.iter().any(|t| t == "tool_start"), "{types:?}");
    assert!(!types.iter().any(|t| t == "usage" || t == "busy"));
    assert!(
        q.events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "run_state" && e["state"] == "done")
    );
    assert!(q.beats.load(Ordering::SeqCst) >= 1);
    // The run's files are gone once the server has the result.
    assert!(m.list().is_empty());
    m.close().await;
}

#[tokio::test]
async fn unattended_runs_deny_prompts_and_report_no_changes() {
    let (base, seen) = mock_server(needs_permission()).await;
    let (home, m) = runner_home("pull-deny", &base);
    let origin = make_repo(&home);
    let q = Queue::new();
    let c = client(&serve_queue(q.clone()).await);
    let mut job = job_for(&origin, "build it");
    job.git_ref = Some("main".into());
    let status = execute(&c, &m, &job, Some("acceptEdits".into()), false)
        .await
        .unwrap();
    assert_eq!(status, RunStatus::Succeeded);
    let second = seen.lock().unwrap()[1]["messages"].to_string();
    assert!(second.contains("unattended"), "{second}");
    let done = q.complete();
    assert!(done.result.branch.is_none());
    assert!(
        done.result.summary.contains("No changes were made."),
        "{}",
        done.result.summary
    );
    assert!(q.event_types().iter().any(|t| t == "permission_request"));
    assert_eq!(branches(&origin), "");
    m.close().await;
}

#[tokio::test]
async fn runs_that_cannot_start_are_reported_failed() {
    let (home, m) = runner_home("pull-fail", "http://127.0.0.1:1/v1");
    let origin = make_repo(&home);
    let q = Queue::new();
    let c = client(&serve_queue(q.clone()).await);

    // The server refuses the credentials.
    q.creds_status.store(403, Ordering::SeqCst);
    let job = job_for(&origin, "x");
    assert_eq!(
        execute(&c, &m, &job, None, false).await.unwrap(),
        RunStatus::Failed
    );
    let done = q.completes.lock().unwrap().pop().unwrap();
    assert_eq!(done.status, RunStatus::Failed);
    assert!(
        done.result
            .summary
            .starts_with("Could not get the run's credentials:"),
        "{}",
        done.result.summary
    );
    // Another runner holds it: nothing is reported.
    q.creds_status.store(409, Ordering::SeqCst);
    let e = execute(&c, &m, &job, None, false).await.unwrap_err();
    assert!(is_lost_lease(&e), "{e:#}");
    assert!(q.completes.lock().unwrap().is_empty());

    // A repository that isn't there.
    q.creds_status.store(404, Ordering::SeqCst);
    let job = job_for(&home.dir.join("no-such-repo"), "x");
    assert_eq!(
        execute(&c, &m, &job, None, false).await.unwrap(),
        RunStatus::Failed
    );
    let done = q.completes.lock().unwrap().pop().unwrap();
    assert!(
        done.result.summary.starts_with("Could not start the run:"),
        "{}",
        done.result.summary
    );
    // A script that can't even be checked out.
    let mut job = job_for(&origin, "");
    job.script = Some("echo hi".into());
    job.git_ref = Some("no-such-branch".into());
    assert_eq!(
        execute(&c, &m, &job, None, false).await.unwrap(),
        RunStatus::Failed
    );
    let done = q.completes.lock().unwrap().pop().unwrap();
    assert!(
        done.result.summary.starts_with("Could not run the script:"),
        "{}",
        done.result.summary
    );
    let _ =
        std::fs::remove_dir_all(std::env::temp_dir().join(format!("chanto-script-{}", job.run_id)));
    assert!(m.list().is_empty());
    m.close().await;
}

#[tokio::test]
async fn script_only_runs_push_their_changes() {
    let (home, m) = runner_home("pull-script", "http://127.0.0.1:1/v1");
    let origin = make_repo(&home);
    let q = Queue::new();
    let c = client(&serve_queue(q.clone()).await);

    // Changes: committed and pushed as the run's branch.
    let mut job = job_for(&origin, "");
    job.script = Some("echo fresh > data.txt\necho wrote it".into());
    assert_eq!(
        execute(&c, &m, &job, None, false).await.unwrap(),
        RunStatus::Succeeded
    );
    let done = q.completes.lock().unwrap().pop().unwrap();
    let branch = format!("chanto/run/{}", &job.run_id[..8]);
    assert_eq!(done.result.branch.as_deref(), Some(branch.as_str()));
    assert_eq!(
        sh(&origin, &["show", &format!("{branch}:data.txt")]),
        "fresh"
    );
    assert_eq!(
        sh(&origin, &["log", "-1", "--format=%s", &branch]),
        "Changes from the run's script"
    );
    assert!(
        done.result
            .summary
            .contains("**Script finished, with changes.**"),
        "{}",
        done.result.summary
    );
    assert!(done.result.summary.contains("wrote it"));
    let ev = q.events.lock().unwrap().clone();
    assert_eq!(ev[0]["type"], "tool_start");
    assert_eq!(ev[1]["display"], "Script finished, with changes");
    assert!(
        !std::env::temp_dir()
            .join(format!("chanto-script-{}", job.run_id))
            .exists()
    );

    // A failing script with nothing changed: the model only runs on a change.
    q.events.lock().unwrap().clear();
    let mut job = job_for(&origin, "fix it");
    job.script = Some("echo checking\nexit 3".into());
    job.model_runs = ModelRuns::OnChange;
    assert_eq!(
        execute(&c, &m, &job, None, false).await.unwrap(),
        RunStatus::Failed
    );
    let done = q.completes.lock().unwrap().pop().unwrap();
    assert!(done.result.branch.is_none());
    assert!(
        done.result.summary.contains("Script failed (exit 3)")
            && done.result.summary.contains("No changes were made."),
        "{}",
        done.result.summary
    );
    assert!(m.list().is_empty(), "no model run was started");
    m.close().await;
}

#[tokio::test]
async fn the_model_continues_from_the_scripts_commit() {
    let (base, seen) = mock_server(write_hello()).await;
    let (home, m) = runner_home("pull-script-model", &base);
    let origin = make_repo(&home);
    let q = Queue::new();
    let c = client(&serve_queue(q.clone()).await);
    let mut job = job_for(&origin, "Finish the job");
    job.script = Some("echo step1 > step.txt".into());
    job.model_runs = ModelRuns::OnChange;
    assert_eq!(
        execute(&c, &m, &job, None, false).await.unwrap(),
        RunStatus::Succeeded
    );
    let done = q.complete();
    let branch = done.result.branch.clone().unwrap();
    // Both the script's file and the model's are on the run's branch.
    assert_eq!(
        sh(&origin, &["show", &format!("{branch}:step.txt")]),
        "step1"
    );
    assert_eq!(sh(&origin, &["show", &format!("{branch}:hello.txt")]), "hi");
    // The temporary script branch is gone; the model was told what ran.
    assert!(
        !branches(&origin).contains("chanto/script/"),
        "{}",
        branches(&origin)
    );
    let first = seen.lock().unwrap()[0]["messages"].to_string();
    assert!(first.contains("this run's script ran"), "{first}");
    assert!(
        done.result
            .summary
            .contains("Script finished, with changes")
    );
    assert_eq!(
        done.result.commit.as_deref(),
        Some(sh(&origin, &["rev-parse", "main"]).as_str()),
        "the run started before its script's commit"
    );
    m.close().await;
}

#[tokio::test]
async fn cancelling_and_losing_the_lease_stop_the_run() {
    let (base, _) = mock_server(vec![json!({"stall": true}), json!({"stall": true})]).await;
    let (home, m) = runner_home("pull-cancel", &base);
    let origin = make_repo(&home);
    let q = Queue::new();
    let c = client(&serve_queue(q.clone()).await);

    // Cancelled from the server while the model works.
    q.cancel.store(true, Ordering::SeqCst);
    let mut job = job_for(&origin, "long task");
    job.lease_secs = 3;
    let status = tokio::time::timeout(Duration::from_secs(60), execute(&c, &m, &job, None, false))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status, RunStatus::Cancelled);
    assert_eq!(
        q.completes.lock().unwrap().pop().unwrap().status,
        RunStatus::Cancelled
    );

    // Cancelled while its script runs.
    let mut job = job_for(&origin, "");
    job.lease_secs = 3;
    job.script = Some("sleep 30".into());
    let status = tokio::time::timeout(Duration::from_secs(20), execute(&c, &m, &job, None, false))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status, RunStatus::Cancelled);
    let done = q.completes.lock().unwrap().pop().unwrap();
    assert_eq!(done.result.summary, "Cancelled while its script ran.");
    let _ =
        std::fs::remove_dir_all(std::env::temp_dir().join(format!("chanto-script-{}", job.run_id)));

    // The lease went to another runner: the run stops, nothing is reported.
    q.cancel.store(false, Ordering::SeqCst);
    q.heartbeat_status.store(409, Ordering::SeqCst);
    let mut job = job_for(&origin, "long task");
    job.lease_secs = 3;
    let e = tokio::time::timeout(Duration::from_secs(60), execute(&c, &m, &job, None, false))
        .await
        .unwrap()
        .unwrap_err();
    assert!(is_lost_lease(&e), "{e:#}");
    assert!(q.completes.lock().unwrap().is_empty());
    for r in m.list() {
        let r = wait_for(&m, &r.id, "stopped", |r| !r.state.is_active()).await;
        assert_eq!(r.state, RunState::Cancelled);
    }
    m.close().await;
}

/// The env vars `parse` reads, cleared for the test (restored after).
struct NoRunnerEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl NoRunnerEnv {
    fn new() -> NoRunnerEnv {
        let keys = [
            "CHANTO_RUNNER_REGISTRATION_TOKEN",
            "CHANTO_RUNNER_OWN_MODEL",
            "CHANTO_CLOUD_URL",
        ];
        let prev = keys.iter().map(|k| (*k, std::env::var_os(k))).collect();
        for k in keys {
            // SAFETY: test-only, under TEST_HOME_LOCK (held by the caller's TestHome).
            unsafe { std::env::remove_var(k) };
        }
        NoRunnerEnv(prev)
    }
}

impl Drop for NoRunnerEnv {
    fn drop(&mut self) {
        for (k, v) in self.0.drain(..) {
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

#[test]
fn connect_flags() {
    let _home = TestHome::new("pull-parse");
    let _env = NoRunnerEnv::new();
    let o = parse(&args(&[
        "https://cloud.example/",
        "--registration-token",
        "reg",
        "--name",
        "box",
        "--label",
        "gpu",
        "--label",
        "linux",
        "--mode",
        "acceptEdits",
        "--ephemeral",
        "--once",
        "--own-model",
    ]))
    .unwrap();
    assert_eq!(o.url, "https://cloud.example");
    assert_eq!(o.registration_token.as_deref(), Some("reg"));
    assert_eq!(o.name, "box");
    assert_eq!(o.labels, ["gpu", "linux"]);
    assert_eq!(o.mode.as_deref(), Some("acceptEdits"));
    assert!(o.ephemeral && o.once && o.own_model);
    let o = parse(&args(&["http://localhost:8080"])).unwrap();
    assert!(o.registration_token.is_none() && !o.ephemeral && !o.own_model);
    let err = |a: &[&str]| format!("{:#}", parse(&args(a)).err().unwrap());
    assert!(err(&[]).starts_with("which server?"));
    assert!(err(&["http://example.com"]).contains("must use https://"));
    assert!(err(&["https://x", "--mode", "wild"]).contains("unknown mode wild"));
    assert!(err(&["https://x", "--name"]).contains("--name needs a value"));
    assert!(err(&["https://x", "--bogus"]).starts_with("unknown flag --bogus"));
    assert!(err(&["--help"]).starts_with("usage: chanto runner connect"));
    // From the environment.
    // SAFETY: test-only, under TEST_HOME_LOCK.
    unsafe {
        std::env::set_var("CHANTO_CLOUD_URL", "https://env.example");
        std::env::set_var("CHANTO_RUNNER_REGISTRATION_TOKEN", "envreg");
        std::env::set_var("CHANTO_RUNNER_OWN_MODEL", "1");
    }
    let o = parse(&[]).unwrap();
    assert_eq!(
        (o.url.as_str(), o.registration_token.as_deref(), o.own_model),
        ("https://env.example", Some("envreg"), true)
    );
    for k in [
        "CHANTO_CLOUD_URL",
        "CHANTO_RUNNER_REGISTRATION_TOKEN",
        "CHANTO_RUNNER_OWN_MODEL",
    ] {
        // SAFETY: as above.
        unsafe { std::env::remove_var(k) };
    }
    assert_eq!(status_label(RunStatus::Queued), "queued");
    assert_eq!(status_label(RunStatus::Running), "running");
    assert_eq!(status_label(RunStatus::Succeeded), "succeeded");
    assert_eq!(status_label(RunStatus::Failed), "failed");
    assert_eq!(status_label(RunStatus::Cancelled), "cancelled");
}

#[tokio::test]
async fn connect_registers_once_and_takes_jobs() {
    let (home, _) = runner_home("pull-connect", "http://127.0.0.1:1/v1");
    let _env = NoRunnerEnv::new();
    let origin = make_repo(&home);
    let q = Queue::new();
    let url = serve_queue(q.clone()).await;

    // Unregistered, without a token.
    let e = format!("{:#}", cli(&args(&[&url])).await.unwrap_err());
    assert!(
        e.contains("isn't registered") && e.contains("--registration-token"),
        "{e}"
    );

    // An ephemeral runner registers without saving, and stops when the
    // server has nothing for it.
    cli(&args(&[
        &url,
        "--registration-token",
        "reg",
        "--ephemeral",
        "--name",
        "box",
        "--label",
        "gpu",
    ]))
    .await
    .unwrap();
    let reg = q.registered.lock().unwrap().clone();
    assert_eq!(reg.len(), 1);
    assert_eq!(
        (
            &reg[0]["name"],
            &reg[0]["labels"],
            &reg[0]["ephemeral"],
            &reg[0]["registration_token"]
        ),
        (&json!("box"), &json!(["gpu"]), &json!(true), &json!("reg"))
    );
    assert!(!connections_path().exists());

    // A leftover finished run from a runner that stopped is cleared out.
    let left = Manager::open(Store::new(store_dir()), Launcher::InProcess, 1);
    let (base, _) = mock_server(vec![]).await;
    let mut req = crate::runs::tests::new_run(&origin, &base, "old");
    req.task = "leftover".into();
    let old = left.create(req).await.unwrap();
    wait_for(&left, &old.id, "finished", |r| !r.state.is_active()).await;
    left.close().await;
    drop(left);
    assert!(Store::new(store_dir()).load(&old.id).is_ok());

    // A registered runner: one job (whose credentials are refused), --once.
    q.creds_status.store(403, Ordering::SeqCst);
    let job = job_for(&origin, "x");
    q.claims
        .lock()
        .unwrap()
        .extend([(503, json!({})), (200, serde_json::to_value(&job).unwrap())]);
    cli(&args(&[&url, "--registration-token", "reg", "--once"]))
        .await
        .unwrap();
    let saved = load_connections();
    assert_eq!(saved.servers.len(), 1);
    assert_eq!(
        (
            saved.servers[0].runner_id.as_str(),
            saved.servers[0].token.as_str()
        ),
        ("rn_1", "rt_secret")
    );
    assert_eq!(q.complete().status, RunStatus::Failed);
    assert!(
        Store::new(store_dir()).load(&old.id).is_err(),
        "leftovers are removed"
    );
    assert!(
        q.auth
            .lock()
            .unwrap()
            .iter()
            .all(|a| a == "Bearer rt_secret")
    );

    // Next time the saved registration is used (no new register call).
    q.completes.lock().unwrap().clear();
    q.claims
        .lock()
        .unwrap()
        .push_back((200, serde_json::to_value(&job).unwrap()));
    cli(&args(&[&url, "--once"])).await.unwrap();
    assert_eq!(q.registered.lock().unwrap().len(), 2);
    assert_eq!(q.complete().status, RunStatus::Failed);
}

#[test]
fn refs_resolve_against_the_remote() {
    let home = TestHome::new("pull-refs");
    let origin = make_repo(&home);
    let url = origin.display().to_string();
    assert_eq!(
        resolve_ref(&url, None, &[]).unwrap(),
        ("refs/heads/main".into(), None)
    );
    assert_eq!(
        resolve_ref(&url, Some("feature"), &[]).unwrap(),
        ("feature".into(), None)
    );
    let head = sh(&origin, &["rev-parse", "HEAD"]);
    assert_eq!(
        resolve_ref(&url, Some(&head), &[]).unwrap(),
        ("refs/heads/main".into(), Some(head.clone()))
    );
    let e = format!(
        "{:#}",
        resolve_ref(&home.dir.join("nope").display().to_string(), None, &[]).unwrap_err()
    );
    assert!(e.contains("default branch"), "{e}");
    let j = job_for(&origin, "t");
    assert_eq!(
        run_path(&j, "events"),
        format!("/v1/runner/runs/{}/events?attempt=1", j.run_id)
    );
}

#[test]
fn script_outcomes_read_well() {
    let o = |ok, code, output: &str, changed| ScriptOutcome {
        dir: PathBuf::from("/nowhere"),
        ok,
        code,
        output: output.into(),
        changed,
        base: None,
    };
    assert_eq!(o(true, Some(0), "", false).headline(), "Script finished");
    assert_eq!(
        o(false, None, "", false).headline(),
        "Script stopped (timed out or killed)"
    );
    assert_eq!(
        o(true, Some(0), "", false).report(),
        "**Script finished.**\n\n```\n(no output)\n```"
    );
    // Fences in the output can't close the block.
    let r = o(false, Some(2), "a ``` b\n", true).report();
    assert_eq!(r, "**Script failed (exit 2).**\n\n```\na ʼʼʼ b\n```");
    let n = o(false, Some(2), "x", true).note_for_model();
    assert!(
        n.starts_with("Before you, this run's script ran: script failed (exit 2). Its changes")
    );
    let env = script_env(&[
        ("CHANTO_MODEL".into(), "m".into()),
        ("CHANTO_BASE_URL".into(), "b".into()),
        ("DB".into(), "x".into()),
    ]);
    assert_eq!(
        env,
        vec![
            ("DB".to_string(), "x".to_string()),
            ("PYTHONDONTWRITEBYTECODE".to_string(), "1".to_string())
        ]
    );
}
