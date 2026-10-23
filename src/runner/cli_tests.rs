//! `chanto runner ...` and `/schedule` against the daemon's routes served in
//! process on localhost, with jobs in a temp home and a mock model.

use super::*;
use crate::agent::tests::mock_server_routed;
use crate::runs::tests::{TestHome, make_repo, needs_permission, sh, wait_for, write_hello};

const SERVER_TOKEN: &str = "srv-token";

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

fn toml_path(p: &Path) -> String {
    toml::Value::String(p.display().to_string()).to_string()
}

struct Daemon {
    home: TestHome,
    m: Arc<Manager>,
    s: Arc<Scheduler>,
    url: String,
    repo: PathBuf,
    base: String,
    server: tokio::task::JoinHandle<()>,
}

impl Daemon {
    async fn start(name: &str) -> Daemon {
        let home = TestHome::new(name);
        let repo = make_repo(&home);
        let (base, _) = mock_server_routed(vec![
            ("do hello", write_hello()),
            (
                "do waits",
                [needs_permission(), needs_permission()].concat(),
            ),
        ])
        .await;
        let cfg = home.dir.join("home").join("config.toml");
        let mut text = std::fs::read_to_string(&cfg).unwrap();
        text.push_str("\n[background]\nsandbox = false\nverify = false\n");
        std::fs::write(&cfg, text).unwrap();
        let m = home.manager();
        let s = Scheduler::new(m.clone());
        let app = crate::runs::api::router::<()>(m.clone())
            .merge(router(s.clone()))
            .layer(axum::middleware::from_fn(
                |req: axum::extract::Request, next: axum::middleware::Next| async move {
                    let ok = req
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        == Some(&format!("Bearer {SERVER_TOKEN}"));
                    if !ok {
                        return err(StatusCode::UNAUTHORIZED, "bad token");
                    }
                    next.run(req).await
                },
            ))
            .merge(public_router(s.clone(), Arc::new(SERVER_TOKEN.into())))
            .route(
                "/health",
                get(|| async { Json(json!({"ok": true, "version": "t", "runs": true})) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        std::fs::write(
            crate::server::server_file(),
            json!({"url": url, "token": SERVER_TOKEN, "pid": std::process::id()}).to_string(),
        )
        .unwrap();
        Daemon {
            home,
            m,
            s,
            url,
            repo,
            base,
            server,
        }
    }

    fn job(&self, name: &str, schedule: Option<&str>, cwd: &Path) -> String {
        format!(
            "[[job]]\nname = \"{name}\"\n{}prompt = \"do {name}\"\ncwd = {}\nprovider = \"openai\"\nbase_url = \"{}\"\nmodel = \"mock\"\n\n",
            schedule
                .map(|s| format!("schedule = \"{s}\"\n"))
                .unwrap_or_default(),
            toml_path(cwd),
            self.base
        )
    }

    fn client(&self) -> crate::runs::cli::Client {
        crate::runs::cli::Client::new(&self.url, SERVER_TOKEN)
    }

    fn runs_of(&self, job: &str) -> Vec<Run> {
        self.m
            .list()
            .into_iter()
            .filter(|r| r.job.as_deref() == Some(job))
            .collect()
    }

    async fn finish(self) {
        for r in self.m.list() {
            if r.state.is_active() {
                let _ = self.m.stop(&r.id);
            }
            wait_for(&self.m, &r.id, "finished", |r| !r.state.is_active()).await;
        }
        self.m.close().await;
        self.server.abort();
    }
}

fn err_of(r: Result<()>) -> String {
    format!("{:#}", r.unwrap_err())
}

#[tokio::test]
async fn once_trigger_logs_and_tokens_drive_jobs_through_the_daemon() {
    let d = Daemon::start("runner-cli").await;
    let plain = d.home.dir.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(
        global_file(),
        [
            d.job("hello", Some("10m"), &d.repo),
            d.job("manual", None, &d.repo),
            d.job("broken", None, &plain),
        ]
        .concat(),
    )
    .unwrap();

    // Starts what is due and waits for it.
    cli(&args(&["--once"])).await.unwrap();
    let hello = d.runs_of("hello");
    assert_eq!(hello.len(), 1);
    assert_eq!(hello[0].state, RunState::Done, "{:?}", hello[0].error);
    assert_eq!(hello[0].trigger.as_deref(), Some("once"));
    assert_eq!(
        sh(
            &d.repo,
            &["show", &format!("{}:hello.txt", hello[0].branch)]
        ),
        "hi"
    );
    // Nothing more is due; a dry run starts nothing.
    cli(&args(&["--once"])).await.unwrap();
    cli(&args(&["--once", "--job", "manual", "--dry-run"]))
        .await
        .unwrap();
    assert_eq!(d.m.list().len(), 1);
    assert_eq!(
        err_of(cli(&args(&["--once", "--job", "nope"])).await),
        "no job named nope"
    );
    let e = err_of(cli(&args(&["--once", "--job", "broken", "--no-wait"])).await);
    assert!(e.starts_with("job broken: ") && e.contains("git"), "{e}");
    let st = load_state();
    assert!(st.jobs["broken"].last_error.is_some());
    assert_eq!(st.jobs["hello"].runs, 1);
    let t = table(&load(None), None);
    assert!(t.contains(&format!("{} done", hello[0].id)), "{t}");
    assert!(t.contains("couldn't start"), "{t}");
    assert!(t.contains("on trigger"), "{t}");

    // Trigger: a manual run, waited for.
    cli(&args(&["trigger", "manual", "--wait"])).await.unwrap();
    let manual = d.runs_of("manual");
    assert_eq!(manual.len(), 1);
    assert_eq!(
        (manual[0].trigger.as_deref(), manual[0].state),
        (Some("manual"), RunState::Done)
    );
    assert!(err_of(cli(&args(&["trigger"])).await).starts_with("usage: chanto runner trigger"));
    let e = err_of(cli(&args(&["run", "nope"])).await);
    assert!(e.starts_with("no job named nope"), "{e}");

    // The jobs as the daemon lists them.
    let v = d.client().get("/runs/jobs").await.unwrap();
    let row = |n: &str| {
        v["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["name"] == n)
            .unwrap()
            .clone()
    };
    assert_eq!(row("hello")["last"]["id"], json!(hello[0].id));
    assert_ne!(row("hello")["next"], "due now");
    assert_eq!(row("manual")["next"], "on trigger");
    assert!(row("broken")["last_error"].is_string());

    // Logs, from the daemon and from the store.
    cli(&args(&["logs", "hello", "-n", "1"])).await.unwrap();
    cli(&args(&["log", "nope"])).await.unwrap();
    assert!(err_of(cli(&args(&["logs"])).await).starts_with("usage: chanto runner logs"));

    // Webhook tokens: created once, rotated on request.
    cli(&args(&["token", "manual"])).await.unwrap();
    let t1 = tokens()["manual"].clone();
    cli(&args(&["token", "manual"])).await.unwrap();
    assert_eq!(tokens()["manual"], t1);
    cli(&args(&["token", "manual", "--rotate"])).await.unwrap();
    assert_ne!(tokens()["manual"], t1);
    assert!(err_of(cli(&args(&["token", "nope"])).await).starts_with("no job named nope"));
    assert!(err_of(cli(&args(&["token"])).await).starts_with("usage"));

    cli(&args(&[])).await.unwrap();
    cli(&args(&["status"])).await.unwrap();
    cli(&args(&["help"])).await.unwrap();
    assert!(err_of(cli(&args(&["frob"])).await).starts_with("unknown subcommand `frob`"));

    // `/schedule` in a session.
    let out = slash(&d.repo, "").await;
    assert!(out.starts_with("```\nJOB "), "{out}");
    assert!(out.contains(&format!(
        "Schedules are live: the chanto daemon is running at {}",
        d.url
    )));
    let out = slash(&d.repo, "run manual").await;
    assert!(
        out.starts_with("Started job `manual` as background run `"),
        "{out}"
    );
    let out = slash(&d.repo, "trigger nope").await;
    assert!(
        out.starts_with("Couldn't start job nope: no job named nope"),
        "{out}"
    );
    assert_eq!(
        slash(&d.repo, "a b c").await,
        "Usage: `/schedule [list | run <job>]`"
    );
    for r in d.runs_of("manual") {
        wait_for(&d.m, &r.id, "finished", |r| !r.state.is_active()).await;
    }

    // Without the daemon: logs read the saved runs; nothing fires on schedule.
    std::fs::remove_file(crate::server::server_file()).unwrap();
    cli(&args(&["logs", "hello"])).await.unwrap();
    cli(&args(&["status"])).await.unwrap();
    cli(&args(&["token", "manual"])).await.unwrap();
    assert!(
        slash(&d.repo, "list")
            .await
            .contains("The chanto daemon isn't running")
    );
    d.finish().await;
}

#[tokio::test]
async fn a_job_never_overlaps_itself_and_bad_jobs_are_refused() {
    let d = Daemon::start("runner-trigger").await;
    let plain = d.home.dir.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(
        global_file(),
        [d.job("waits", None, &d.repo), d.job("broken", None, &plain)].concat(),
    )
    .unwrap();
    let http = crate::http::client();
    let trigger = |job: &str| {
        http.post(format!("{}/runs/jobs/{job}/trigger", d.url))
            .bearer_auth(SERVER_TOKEN)
            .json(&json!({"trigger": "api"}))
            .send()
    };
    let r = trigger("waits").await.unwrap();
    assert_eq!(r.status(), 201);
    let run: Run = r.json().await.unwrap();
    assert_eq!(run.trigger.as_deref(), Some("api"));
    wait_for(&d.m, &run.id, "needs-input", |r| {
        r.state == RunState::NeedsInput
    })
    .await;
    let r = trigger("waits").await.unwrap();
    assert_eq!(r.status(), 409);
    let v: Value = r.json().await.unwrap();
    assert!(
        v["error"].as_str().unwrap().contains("still running"),
        "{v}"
    );
    // A scheduled pass skips it too.
    let pass =
        d.s.run_due(None, Some("waits".into()), false, "manual")
            .await;
    assert!(
        pass["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .starts_with("still running")
    );
    // A job that can't start: 400 with the reason.
    let r = trigger("broken").await.unwrap();
    assert_eq!(r.status(), 400);
    // A GitHub signature made with the job's token is accepted.
    let token = job_token("waits", false).unwrap();
    let body = br#"{"action":"opened"}"#;
    let sig = format!("sha256={}", hex(&hmac_sha256(token.as_bytes(), body)));
    let r = http
        .post(format!("{}/runs/jobs/waits/trigger", d.url))
        .header("x-hub-signature-256", sig)
        .body(&body[..])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409, "authenticated, but the job is busy");
    d.m.respond(&run.id, json!({"decision": "allow"})).unwrap();
    let done = wait_for(&d.m, &run.id, "finished", |r| !r.state.is_active()).await;
    assert_eq!(done.state, RunState::Done, "{:?}", done.error);
    // The `--once` waiter reports a run that ended badly.
    d.m.stop(&run.id).unwrap_err();
    let failed =
        d.m.create(NewRun {
            task: "do waits".into(),
            cwd: Some(d.repo.clone()),
            provider: Some("openai".into()),
            base_url: Some(d.base.clone()),
            model: Some("mock".into()),
            sandbox: Some(false),
            verify: Some(false),
            ..Default::default()
        })
        .await
        .unwrap();
    wait_for(&d.m, &failed.id, "needs-input", |r| {
        r.state == RunState::NeedsInput
    })
    .await;
    let id = failed.id.clone();
    let c = d.client();
    let waiter = tokio::spawn(async move { wait_for_runs(&c, vec![("waits".into(), id)]).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    d.m.stop(&failed.id).unwrap();
    assert!(!waiter.await.unwrap().unwrap(), "a cancelled run isn't ok");
    d.finish().await;
}

#[tokio::test]
async fn projects_are_enabled_only_when_trusted() {
    let d = Daemon::start("runner-enable").await;
    let proj = d.repo.clone();
    std::fs::create_dir_all(proj.join(".chanto")).unwrap();
    std::fs::write(
        project_file(&proj),
        "[[job]]\nname = \"proj\"\nprompt = \"p\"\nschedule = \"1h\"\n\n[[job]]\nname = \"shared\"\nprompt = \"from the project\"\n",
    )
    .unwrap();
    std::fs::write(global_file(), d.job("shared", None, &proj)).unwrap();
    let p = proj.display().to_string();

    let e = err_of(cli(&args(&["enable", &p])).await);
    assert!(e.contains("isn't trusted"), "{e}");
    assert!(enabled_projects().is_empty());
    // Listed from here, but not scheduled.
    let t = table(&load(Some(&proj)), Some(&proj));
    assert!(t.contains("the project isn't trusted"), "{t}");

    crate::config::trust(&proj).unwrap();
    cli(&args(&["enable", &p])).await.unwrap();
    assert_eq!(enabled_projects(), vec![proj.clone()]);
    cli(&args(&["enable", &p])).await.unwrap();
    assert_eq!(enabled_projects().len(), 1, "enabled once");
    let jobs = load(None);
    assert!(jobs.problems.is_empty(), "{:?}", jobs.problems);
    assert_eq!(jobs.get("proj").unwrap().repo, proj);
    // The project's job replaces the global one of the same name.
    assert_eq!(jobs.get("shared").unwrap().prompt, "from the project");
    assert_eq!(jobs.jobs.iter().filter(|j| j.name == "shared").count(), 1);

    // A second enabled project with the same job name: the first keeps it.
    let other = d.home.dir.join("other");
    std::fs::create_dir_all(other.join(".chanto")).unwrap();
    sh(&other, &["init", "-q"]);
    let other = other.canonicalize().unwrap();
    std::fs::write(
        project_file(&other),
        "[[job]]\nname = \"proj\"\nprompt = \"again\"\n",
    )
    .unwrap();
    crate::config::trust(&other).unwrap();
    enable(&other).unwrap();
    let jobs = load(None);
    assert!(
        jobs.problems
            .iter()
            .any(|p| p.contains("job `proj`") && p.contains("using the latter")),
        "{:?}",
        jobs.problems
    );
    // An enabled project that lost its job file.
    std::fs::remove_file(project_file(&other)).unwrap();
    assert!(
        load(None)
            .problems
            .iter()
            .any(|p| p.contains("has no .chanto/runner.toml"))
    );
    // A broken job file is a problem, not a crash.
    std::fs::write(project_file(&other), "[[job]]\nname = \"x\"\n").unwrap();
    assert!(!load(None).problems.is_empty());
    std::fs::write(global_file(), "not = [toml").unwrap();
    assert!(!load(None).problems.is_empty());

    let jobs = load(Some(&proj));
    assert!(!table(&jobs, Some(&proj)).contains("(not enabled)"));
    cli(&args(&["disable", &p])).await.unwrap();
    assert_eq!(enabled_projects(), vec![other.clone()]);
    cli(&args(&["disable", &p])).await.unwrap();
    assert_eq!(disable(&proj).unwrap(), None);
    std::fs::remove_file(global_file()).unwrap();
    let t = table(&load(Some(&proj)), Some(&proj));
    assert!(t.contains("(not enabled)"), "{t}");
    assert!(table(&Jobs::default(), None).starts_with("No jobs. Define them in "));
    d.finish().await;
}

fn job(schedule: Option<&str>, enabled: bool) -> Job {
    Job {
        name: "j".into(),
        schedule: schedule.map(str::to_string),
        prompt: "p".into(),
        cwd: None,
        mode: None,
        verify: None,
        model: None,
        provider: None,
        base_url: None,
        max_minutes: None,
        max_cost_usd: None,
        kind: None,
        enabled,
        repo: PathBuf::from("/r"),
        file: PathBuf::from("/r/f"),
    }
}

#[test]
fn next_labels_say_when_a_job_fires() {
    let now = Local::now();
    assert_eq!(next_label(&job(None, true), None, now), "on trigger");
    assert_eq!(next_label(&job(Some("1h"), false), None, now), "paused");
    assert_eq!(next_label(&job(Some("soon"), true), None, now), "invalid");
    assert_eq!(next_label(&job(Some("1h"), true), None, now), "due now");
    let st = JobState {
        last_run: Some(now.to_rfc3339()),
        ..Default::default()
    };
    assert_eq!(
        next_label(&job(Some("1h"), true), Some(&st), now),
        (now + ChronoDuration::hours(1))
            .format("%Y-%m-%d %H:%M")
            .to_string()
    );
    // February 30th never comes.
    assert_eq!(
        next_label(&job(Some("0 0 30 2 *"), true), Some(&st), now),
        "never"
    );
    assert!(valid_name("nightly-1.x") && !valid_name(".hidden") && !valid_name(""));
    assert_eq!(expand_home(Path::new("/abs")), PathBuf::from("/abs"));
    assert!(expand_home(Path::new("~/x")).ends_with("x"));
    assert_eq!(short_path(Path::new("/srv/x")), "/srv/x");
    if let Some(h) = dirs::home_dir() {
        assert_eq!(short_path(&h.join("code")), "~/code");
    }
}
