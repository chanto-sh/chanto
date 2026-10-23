//! `chanto runs`, `attach`, `remote`, `daemon` and the `/bg` and `/runs`
//! slash commands, driven against the runs API served in-process on
//! localhost (a stand-in daemon) with a mock model.

use super::*;
use crate::agent::tests::{make_env, mock_server, tc};
use crate::agent::{Event, PermissionReply, PermissionRequest};
use crate::permissions::Mode;
use crate::runs::manager::Manager;
use crate::runs::tests::{
    TestHome, finished, make_repo, needs_permission, new_run, sh, wait_for, write_hello,
};
use crate::runs::{RunState, Store};
use async_trait::async_trait;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use std::path::Path;

const TOKEN: &str = "test-token";

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

async fn serve(app: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let h = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, h)
}

/// The daemon's surface: `/health`, `/runs/jobs`, and the runs API behind
/// the bearer token.
fn daemon_app(m: Arc<Manager>, runs: bool) -> axum::Router {
    crate::runs::api::router::<()>(m)
        .route(
            "/runs/jobs",
            get(|| async { axum::Json(json!({"jobs": [{"name": "nightly"}]})) }),
        )
        .layer(axum::middleware::from_fn(
            |req: axum::extract::Request, next: axum::middleware::Next| async move {
                let auth = req
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                if auth.as_deref() != Some(&format!("Bearer {TOKEN}")) {
                    return (
                        StatusCode::UNAUTHORIZED,
                        axum::Json(json!({"error": "bad token"})),
                    )
                        .into_response();
                }
                next.run(req).await
            },
        ))
        .route(
            "/health",
            get(move || async move {
                axum::Json(json!({"ok": true, "version": "0.0.0-test", "runs": runs}))
            }),
        )
}

struct Daemon {
    home: TestHome,
    m: Arc<Manager>,
    url: String,
    server: tokio::task::JoinHandle<()>,
}

impl Daemon {
    async fn start(name: &str) -> Daemon {
        let home = TestHome::new(name);
        let m = home.manager();
        let (url, server) = serve(daemon_app(m.clone(), true)).await;
        write_server_file(&url, Some(std::process::id()));
        Daemon {
            home,
            m,
            url,
            server,
        }
    }

    fn client(&self) -> Client {
        Client::new(&self.url, TOKEN)
    }

    /// Let every run end before the home goes away.
    async fn finish(self) {
        for r in self.m.list() {
            if r.state.is_active() {
                let _ = self.m.stop(&r.id);
            }
            wait_for(&self.m, &r.id, "finished", finished).await;
        }
        self.m.close().await;
        self.server.abort();
    }
}

fn write_server_file(url: &str, pid: Option<u32>) {
    let mut v = json!({"url": url, "token": TOKEN});
    if let Some(p) = pid {
        v["pid"] = json!(p);
    }
    std::fs::write(crate::server::server_file(), v.to_string()).unwrap();
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    !sh(repo, &["branch", "--list", branch]).is_empty()
}

fn err_of(r: Result<()>) -> String {
    format!("{:#}", r.unwrap_err())
}

#[tokio::test]
async fn runs_commands_show_stop_and_remove_runs_through_the_daemon() {
    let d = Daemon::start("cli-runs").await;
    let repo = make_repo(&d.home);
    let (base, _) = mock_server(write_hello()).await;
    let run =
        d.m.create(new_run(&repo, &base, "create hello.txt"))
            .await
            .unwrap();
    let done = wait_for(&d.m, &run.id, "finished", finished).await;
    assert_eq!(done.state, RunState::Done, "{:?}", done.error);
    assert!(branch_exists(&repo, &done.branch));

    runs_cmd(&args(&[])).await.unwrap();
    runs_cmd(&args(&["ls"])).await.unwrap();
    runs_cmd(&args(&["help"])).await.unwrap();
    // Ids may be abbreviated.
    runs_cmd(&args(&["show", &run.id[..5]])).await.unwrap();
    assert!(err_of(runs_cmd(&args(&["show"])).await).contains("usage: chanto runs show <id>"));
    assert!(err_of(runs_cmd(&args(&["show", "zzzzzz"])).await).contains("no run"));
    assert!(err_of(runs_cmd(&args(&["frobnicate"])).await).contains("unknown subcommand"));
    let e = err_of(runs_cmd(&args(&["stop", &run.id])).await);
    assert!(e.contains("already finished"), "{e}");

    // The detail the daemon serves, as `chanto runs show` prints it.
    let v = d.client().get(&format!("/runs/{}", run.id)).await.unwrap();
    let text = detail_text(&v);
    assert!(text.starts_with(&format!("Run {}  done", run.id)), "{text}");
    assert!(text.contains("(1 commit(s) after"), "{text}");
    assert!(text.contains("hello.txt"), "diff stat: {text}");
    assert!(text.contains("Created hello.txt."), "last message: {text}");
    assert!(text.contains("worktree only (sandbox off)"), "{text}");

    // Attaching to a finished run replays it and returns.
    attach_cmd(&args(&[&run.id])).await.unwrap();
    runs_cmd(&args(&["attach", &run.id])).await.unwrap();
    attach_cmd(&args(&["--help"])).await.unwrap();
    assert!(err_of(attach_cmd(&args(&[])).await).contains("usage: chanto attach"));

    // rm deletes the worktree, the branch and the record.
    runs_cmd(&args(&["rm", &run.id])).await.unwrap();
    assert!(!done.worktree.exists());
    assert!(!branch_exists(&repo, &done.branch));
    assert!(d.home.store().load(&run.id).is_err());
    assert!(d.m.list().is_empty());
    assert!(err_of(runs_cmd(&args(&["rm", &run.id])).await).contains("no run"));
    d.finish().await;
}

#[tokio::test]
async fn runs_commands_refuse_active_runs_and_stop_then_resume_them() {
    let d = Daemon::start("cli-active").await;
    let repo = make_repo(&d.home);
    let (base, _) = mock_server(needs_permission()).await;
    let run = d.m.create(new_run(&repo, &base, "build it")).await.unwrap();
    wait_for(&d.m, &run.id, "needs-input", |r| {
        r.state == RunState::NeedsInput
    })
    .await;
    let e = err_of(runs_cmd(&args(&["apply", &run.id])).await);
    assert!(e.contains("still needs-input"), "{e}");
    let e = err_of(runs_cmd(&args(&["resume", &run.id])).await);
    assert!(e.contains("is needs-input"), "{e}");
    let e = err_of(runs_cmd(&args(&["rm", &run.id])).await);
    assert!(e.contains("stop it first"), "{e}");
    assert!(err_of(runs_cmd(&args(&["apply"])).await).contains("usage: chanto runs apply"));

    let v = d.client().get(&format!("/runs/{}", run.id)).await.unwrap();
    let text = detail_text(&v);
    assert!(
        text.contains("Waiting for input: ") && text.contains("make build-it"),
        "{text}"
    );
    assert!(
        text.contains(&format!("chanto attach {}", run.id)),
        "{text}"
    );

    runs_cmd(&args(&["stop", &run.id])).await.unwrap();
    let r = wait_for(&d.m, &run.id, "cancelled", finished).await;
    assert_eq!(r.state, RunState::Cancelled);
    assert!(r.worktree.exists(), "stop keeps the worktree");

    runs_cmd(&args(&["resume", &run.id])).await.unwrap();
    assert!(d.m.get(&run.id).unwrap().state.is_active());
    // It asks again; answering lets it finish.
    let r = wait_for(&d.m, &run.id, "needs-input or finished", |r| {
        r.state == RunState::NeedsInput || !r.state.is_active()
    })
    .await;
    if r.state == RunState::NeedsInput {
        d.m.respond(&run.id, json!({"decision": "allow"})).unwrap();
    }
    let r = wait_for(&d.m, &run.id, "finished", finished).await;
    assert_eq!(r.state, RunState::Done, "{:?}", r.error);
    d.finish().await;
}

#[tokio::test]
async fn listing_reads_the_store_when_no_daemon_runs() {
    let home = TestHome::new("cli-nodaemon");
    // A saved run, and no server.json: `runs list` reads the store, and the
    // commands that need a daemon say it isn't running.
    let mut v = json!({"id": "abc12345", "task": "saved", "state": "done", "created": "", "worktree": "", "branch": "b", "base": "x",
        "settings": {}, "source": {"kind": "local", "path": "/r", "head": "x"}});
    v["cost_usd"] = json!(0.5);
    let saved: Run = serde_json::from_value(v).unwrap();
    Store::new(Store::default_dir()).save(&saved).unwrap();
    runs_cmd(&args(&["list"])).await.unwrap();
    let e = format!("{:#}", connect(false).await.err().unwrap());
    assert!(e.contains("not running"), "{e}");
    // A stale connection file: nothing answers there.
    write_server_file("http://127.0.0.1:1", Some(1));
    runs_cmd(&args(&["list"])).await.unwrap();
    assert!(connect(false).await.is_err());
    // `daemon stop` cleans it up; `daemon status` reports it.
    daemon_cmd(&args(&["status"])).await.unwrap();
    daemon_cmd(&args(&["stop"])).await.unwrap();
    assert!(!crate::server::server_file().exists());
    daemon_cmd(&args(&["status"])).await.unwrap();
    daemon_cmd(&args(&["stop"])).await.unwrap();
    write_server_file("http://127.0.0.1:1", None);
    assert!(err_of(daemon_cmd(&args(&["stop"])).await).contains("no pid"));
    assert!(err_of(daemon_cmd(&args(&["bogus"])).await).contains("unknown subcommand"));
    daemon_cmd(&args(&["help"])).await.unwrap();
    // Starting a run outside a git repository fails before any daemon starts.
    let plain = home.dir.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let e = format!(
        "{:#}",
        start(new_run_body(&plain, "x", None)).await.unwrap_err()
    );
    assert!(e.contains("not inside a git repository"), "{e}");
}

#[tokio::test]
async fn daemon_status_and_a_server_without_runs() {
    let d = Daemon::start("cli-daemon").await;
    daemon_cmd(&args(&[])).await.unwrap();
    daemon_cmd(&args(&["start"])).await.unwrap();
    let c = connect(false).await.unwrap();
    assert_eq!(c.url, d.url);
    assert_eq!(c.pid, Some(std::process::id()));
    // A bad token is refused with the server's own message.
    let e = format!(
        "{:#}",
        Client::new(&d.url, "wrong").get("/runs").await.unwrap_err()
    );
    assert_eq!(e, "bad token");
    // A route the server doesn't have, without an error body.
    let e = format!("{:#}", c.get("/nope").await.unwrap_err());
    assert!(e.contains("doesn't host background runs"), "{e}");
    assert!(c.get_bytes("/runs/zzzz/bundle").await.is_err());
    assert!(c.events("zzzz", 0).await.is_err());
    assert!(
        c.post_bytes("/runs/bundles", b"junk".to_vec())
            .await
            .is_err()
    );

    // A server that listens beyond this machine doesn't host runs.
    let (url, server) = serve(daemon_app(d.m.clone(), false)).await;
    write_server_file(&url, Some(4242));
    let e = format!("{:#}", connect(false).await.err().unwrap());
    assert!(
        e.contains("doesn't host background runs") && e.contains("4242"),
        "{e}"
    );
    daemon_cmd(&args(&["status"])).await.unwrap();
    server.abort();
    d.finish().await;
}

#[tokio::test]
async fn remote_servers_are_added_used_and_removed() {
    let d = Daemon::start("cli-remote").await;
    let e = err_of(remote_cmd(&args(&["add", "srv", &d.url])).await);
    assert!(e.contains("--token is required"), "{e}");
    let e = err_of(remote_cmd(&args(&["add", "srv", "ftp://x", "--token", "t"])).await);
    assert!(e.contains("http://"), "{e}");
    assert!(err_of(remote_cmd(&args(&["add", "srv"])).await).contains("usage"));
    remote_cmd(&args(&["list"])).await.unwrap();

    let url = format!("{}/", d.url);
    remote_cmd(&args(&["add", "srv", &url, &format!("--token={TOKEN}")]))
        .await
        .unwrap();
    // Saved without the trailing slash, under `remote:<name>`.
    assert_eq!(
        crate::auth::remotes().get("srv"),
        Some(&(d.url.clone(), TOKEN.to_string()))
    );
    remote_cmd(&args(&["ls"])).await.unwrap();
    let c = client_for(Some("srv")).await.unwrap();
    assert_eq!(c.url, d.url);
    runs_cmd(&args(&["--on", "srv", "list"])).await.unwrap();
    runs_cmd(&args(&["list", "--on=srv"])).await.unwrap();
    assert!(err_of(runs_cmd(&args(&["--on", "srv", "show", "nope"])).await).contains("no run"));
    // A wrong token, a server without runs, an unreachable one.
    remote_cmd(&args(&["add", "bad", &d.url, "--token", "wrong"]))
        .await
        .unwrap();
    let (norun, server) = serve(daemon_app(d.m.clone(), false)).await;
    remote_cmd(&args(&["add", "norun", &norun, "--token", TOKEN]))
        .await
        .unwrap();
    let e = format!("{:#}", client_for(Some("norun")).await.err().unwrap());
    assert!(e.contains("serve --runs"), "{e}");
    remote_cmd(&args(&[
        "add",
        "gone",
        "http://127.0.0.1:1",
        "--token",
        "t",
    ]))
    .await
    .unwrap();
    let e = format!("{:#}", client_for(Some("gone")).await.err().unwrap());
    assert!(e.contains("can't reach"), "{e}");
    let e = format!("{:#}", client_for(Some("missing")).await.err().unwrap());
    assert!(e.contains("no run server named missing"), "{e}");

    remote_cmd(&args(&["rm", "srv"])).await.unwrap();
    assert!(!crate::auth::remotes().contains_key("srv"));
    remote_cmd(&args(&["remove", "srv"])).await.unwrap();
    assert!(err_of(remote_cmd(&args(&["rm"])).await).contains("usage"));
    assert!(err_of(remote_cmd(&args(&["bogus"])).await).contains("unknown subcommand"));
    remote_cmd(&args(&["help"])).await.unwrap();
    server.abort();
    d.finish().await;
}

#[test]
fn take_on_splits_the_server_out() {
    assert_eq!(
        take_on(&args(&["show", "--on", "box", "abc"])),
        (Some("box".into()), args(&["show", "abc"]))
    );
    assert_eq!(
        take_on(&args(&["--on=box", "ls"])),
        (Some("box".into()), args(&["ls"]))
    );
    assert_eq!(take_on(&args(&["ls"])), (None, args(&["ls"])));
    assert_eq!(arg_id(&args(&["--pr", "abc"]), "u").unwrap(), "abc");
    assert_eq!(
        arg_id(&args(&["--pr"]), "chanto x")
            .unwrap_err()
            .to_string(),
        "usage: chanto x"
    );
}

fn cli(a: &[&str]) -> crate::Cli {
    use clap::Parser as _;
    crate::Cli::try_parse_from([&["chanto"][..], a].concat()).unwrap()
}

#[test]
fn run_options_come_from_the_flags() {
    let cwd = Path::new("/w");
    let b = new_run_body(cwd, "t", None);
    assert_eq!(b, json!({"task": "t", "cwd": "/w"}));
    let b = new_run_body(
        cwd,
        "t",
        Some(&cli(&[
            "-m",
            "m1",
            "--provider",
            "p",
            "--base-url",
            "http://b",
            "--mode",
            "plan",
            "--no-verify",
            "--max-cost",
            "1.5",
            "--sandbox",
            "danger-full-access",
        ])),
    );
    assert_eq!(
        b,
        json!({"task": "t", "cwd": "/w", "model": "m1", "provider": "p", "base_url": "http://b",
            "mode": "plan", "verify": false, "max_cost_usd": 1.5, "sandbox": false})
    );
    let b = new_run_body(
        cwd,
        "t",
        Some(&cli(&["--verify", "--dangerously-skip-permissions"])),
    );
    assert_eq!(b["verify"], true);
    assert_eq!(b["mode"], "bypassPermissions");
    assert!(b.get("sandbox").is_none());
}

#[tokio::test]
async fn run_bg_starts_a_run_from_the_command_line() {
    let d = Daemon::start("cli-bg").await;
    let repo = make_repo(&d.home);
    let (base, seen) = mock_server(write_hello()).await;
    let e = format!("{:#}", run_bg(&cli(&["--bg", "run"])).await.unwrap_err());
    assert!(e.contains("usage: chanto run --bg"), "{e}");
    let c = cli(&[
        "--bg",
        "-C",
        repo.to_str().unwrap(),
        "--provider",
        "openai",
        "--base-url",
        &base,
        "-m",
        "mock",
        "--no-verify",
        "-s",
        "danger-full-access",
        "run",
        "create",
        "hello.txt",
    ]);
    run_bg(&c).await.unwrap();
    let runs = d.m.list();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].task, "create hello.txt");
    assert_eq!(runs[0].source.path, repo);
    let done = wait_for(&d.m, &runs[0].id, "finished", finished).await;
    assert_eq!(done.state, RunState::Done, "{:?}", done.error);
    // The scripted turn and nothing else: the Write, then the answer. If the
    // work isn't on the branch, the run's events say why (a failed Write, or
    // a "could not commit" warning when git can't, e.g. on a full disk).
    let events = d.home.store().events(&done.id);
    assert_eq!(seen.lock().unwrap().len(), 2, "{events:#?}");
    let shown = std::process::Command::new("git")
        .args(["show", &format!("{}:hello.txt", done.branch)])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&shown.stdout).trim(),
        "hi",
        "hello.txt isn't on {}: {}\nrun events: {events:#?}",
        done.branch,
        String::from_utf8_lossy(&shown.stderr).trim()
    );
    // `--on` a server nobody added.
    let e = format!(
        "{:#}",
        run_bg(&cli(&[
            "--bg",
            "-C",
            repo.to_str().unwrap(),
            "--on",
            "nowhere",
            "x"
        ]))
        .await
        .unwrap_err()
    );
    assert!(e.contains("no run server named nowhere"), "{e}");
    d.finish().await;
}

fn run_from(extra: Value) -> Run {
    let mut v = json!({"id": "abc12345", "task": "fix the bug", "state": "done", "created": "", "worktree": "/wt", "branch": "chanto/run/abc12345",
        "base": "0123456789abcdef", "settings": {"verify": true, "max_cost_usd": 2.0, "sandbox": true},
        "source": {"kind": "local", "path": "/repo", "head": "x"}});
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    serde_json::from_value(v).unwrap()
}

#[test]
fn tables_and_details_say_what_each_run_is() {
    assert!(table(&[]).starts_with("No background runs."));
    let t = table(&[
        run_from(json!({})),
        run_from(json!({"id": "goal0001", "kind": "goal", "state": "running", "cost_usd": 0.25})),
        run_from(json!({"id": "job00001", "job": "nightly", "verify": {"status": "passed"}})),
        run_from(json!({"id": "off00001", "settings": {"verify": false}})),
    ]);
    let lines: Vec<&str> = t.lines().collect();
    assert_eq!(lines.len(), 5, "{t}");
    assert!(lines[0].starts_with("ID ") && lines[0].contains("VERIFY"));
    assert!(
        lines[1].contains("abc12345") && lines[1].contains(" -  "),
        "{t}"
    );
    assert!(lines[2].contains("goal: fix the bug") && lines[2].contains("$0.2500"));
    assert!(lines[3].contains("[nightly] fix the bug") && lines[3].contains("passed"));
    assert!(lines[4].contains(" off "), "{t}");

    // Detail: job, goal, team, snapshot, error, pending, container isolation.
    let r = run_from(json!({
        "job": "nightly", "kind": "goal", "team": "t1", "error": "it broke",
        "verify": {"status": "failed", "detail": "2 tests"},
        "source": {"kind": "local", "path": "/repo", "head": "x", "snapshot": "s"},
        "pending": {"type": "question", "question": "Which one?"},
        "settings": {"isolate": {"engine": "docker", "image": "rust:1", "network": "none"}},
    }));
    let mut v = r.to_json();
    v["commits"] = json!(3);
    v["diff_stat"] = json!(" a.rs | 2 +-");
    v["last_messages"] = json!(["first", "second"]);
    let text = detail_text(&v);
    for want in [
        "Run abc12345  done",
        "(3 commit(s) after 0123456789)",
        "Model:    default",
        "Verify:   failed: 2 tests",
        "Isolation: container (docker",
        "Job:      nightly (schedule)",
        "Goal:     criteria and progress in",
        "Team:     t1 (`chanto team show t1`)",
        "Started from your uncommitted changes",
        "Error:    it broke",
        "Waiting for input: question: Which one?",
        "Changes:\n a.rs | 2 +-",
        "Last messages:\n---\nfirst\n---\nsecond",
    ] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
    // Something that isn't a run is shown as-is.
    assert_eq!(detail_text(&json!({"x": 1})), "{\"x\":1}");
    assert_eq!(
        pending_summary(&json!({"label": "Bash", "detail": "make\n  all"})),
        "Bash — make all"
    );
    assert_eq!(pending_summary(&json!({})), "permission — ");

    let mut r = run_from(
        json!({"source": {"kind": "local", "path": "/repo", "head": "x", "snapshot": "s"}}),
    );
    let s = started_message(&r, Some("box"));
    assert!(
        s.starts_with(
            "Started background run abc12345 on box on branch chanto/run/abc12345 (with your uncommitted changes)."
        ),
        "{s}"
    );
    assert!(s.contains("chanto attach abc12345 --on box"), "{s}");
    r.source.snapshot = None;
    let s = started_message(&r, None);
    assert!(s.starts_with("Started background run abc12345 on branch chanto/run/abc12345.\n"));
    assert!(s.contains("chanto runs apply abc12345 "), "{s}");
}

#[test]
fn printed_events_and_sse_parsing() {
    let mut in_text = false;
    assert!(print_event(
        &json!({"type": "text", "text": "hi"}),
        &mut in_text
    ));
    assert!(in_text);
    assert!(print_event(&json!({"type": "assistant_end"}), &mut in_text));
    assert!(!in_text);
    assert!(print_event(
        &json!({"type": "text", "text": "x"}),
        &mut in_text
    ));
    // Anything else ends the line first.
    assert!(print_event(
        &json!({"type": "tool_start", "label": "Read(a)"}),
        &mut in_text
    ));
    assert!(!in_text);
    for ev in [
        json!({"type": "tool_start", "label": "sub", "parent": "p"}),
        json!({"type": "tool_end", "is_error": true, "output": "boom"}),
        json!({"type": "tool_end", "is_error": false}),
        json!({"type": "warning", "message": "w"}),
        json!({"type": "error", "message": "e"}),
        json!({"type": "info", "message": "i"}),
    ] {
        assert!(print_event(&ev, &mut in_text), "{ev}");
    }
    for ty in ["permission_request", "question", "run_state", "busy"] {
        assert!(!print_event(&json!({"type": ty}), &mut in_text), "{ty}");
    }

    // Multi-line data joins with newlines; non-JSON blocks are skipped.
    let mut p = SseParser { buf: String::new() };
    let v = p.push(b"data: {\"a\":\ndata:1}\n\ndata: not json\n\nevent: x\ndata: [2]\n\n");
    assert_eq!(v, vec![json!({"a": 1}), json!([2])]);
    assert!(p.buf.is_empty());
}

/// Records what it was shown and answers prompts as told.
struct Recorder {
    events: Mutex<Vec<String>>,
    asked: Mutex<Vec<PermissionRequest>>,
    questions: Mutex<Vec<(String, Vec<String>)>>,
    reply: fn() -> PermissionReply,
    answer: Option<&'static str>,
}

impl Recorder {
    fn new(reply: fn() -> PermissionReply, answer: Option<&'static str>) -> Arc<Recorder> {
        Arc::new(Recorder {
            events: Mutex::default(),
            asked: Mutex::default(),
            questions: Mutex::default(),
            reply,
            answer,
        })
    }
    fn shown(&self) -> String {
        self.events.lock().unwrap().join("\n")
    }
}

#[async_trait]
impl Frontend for Recorder {
    fn emit(&self, ev: Event) {
        match ev {
            Event::Info(s) => self.events.lock().unwrap().push(format!("info: {s}")),
            Event::Warn(s) => self.events.lock().unwrap().push(format!("warn: {s}")),
            _ => {}
        }
    }
    async fn permission(&self, req: PermissionRequest) -> PermissionReply {
        self.asked.lock().unwrap().push(req);
        (self.reply)()
    }
    async fn question(&self, q: &str, options: &[String]) -> Option<String> {
        self.questions
            .lock()
            .unwrap()
            .push((q.to_string(), options.to_vec()));
        self.answer.map(str::to_string)
    }
}

#[tokio::test]
async fn following_a_run_answers_its_permission_prompts_here() {
    let d = Daemon::start("cli-follow").await;
    let repo = make_repo(&d.home);
    let (base, seen) = mock_server(needs_permission()).await;
    let run = d.m.create(new_run(&repo, &base, "build it")).await.unwrap();
    wait_for(&d.m, &run.id, "needs-input", |r| {
        r.state == RunState::NeedsInput
    })
    .await;
    let fe = Recorder::new(|| PermissionReply::Deny(Some("use cargo".into())), None);
    let token = CancellationToken::new();
    *ATTACHED.lock().unwrap() = Some((run.id.clone(), token.clone()));
    tokio::time::timeout(
        Duration::from_secs(30),
        follow(fe.clone(), d.client(), run.id.clone(), token),
    )
    .await
    .expect("follow ends with the run");
    let done = d.m.get(&run.id).unwrap();
    assert_eq!(done.state, RunState::Done, "{:?}", done.error);
    let asked = fe.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert!(asked[0].label.starts_with(&format!("[run {}] ", run.id)));
    assert!(asked[0].detail.contains("make build-it"), "{:?}", asked[0]);
    assert!(asked[0].proposed.is_none());
    // The denial and its feedback reached the model.
    let second = seen.lock().unwrap()[1]["messages"].to_string();
    assert!(second.contains("use cargo"), "{second}");
    let shown = fe.shown();
    assert!(
        shown.contains(&format!("[run {}] Finished after the command.", run.id)),
        "{shown}"
    );
    assert!(
        shown.contains(&format!("[run {}] done · $", run.id))
            && shown.contains(&format!("`chanto runs apply {}`", run.id)),
        "{shown}"
    );
    assert!(ATTACHED.lock().unwrap().is_none(), "following ended");
    d.finish().await;
}

#[tokio::test]
async fn following_a_run_answers_its_questions_here() {
    let d = Daemon::start("cli-question").await;
    let repo = make_repo(&d.home);
    let (base, seen) = mock_server(vec![
        json!({"tool_calls": tc("AskUserQuestion", json!({"question": "Which file?", "options": ["a.txt", "b.txt"]}))}),
        json!({"content": "Using b.txt."}),
    ])
    .await;
    let run = d.m.create(new_run(&repo, &base, "pick one")).await.unwrap();
    let r = wait_for(&d.m, &run.id, "needs-input", |r| {
        r.state == RunState::NeedsInput || !r.state.is_active()
    })
    .await;
    assert_eq!(r.state, RunState::NeedsInput, "{:?}", r.error);
    assert_eq!(r.pending.as_ref().unwrap()["type"], "question");
    let fe = Recorder::new(|| PermissionReply::Allow, Some("b.txt"));
    tokio::time::timeout(
        Duration::from_secs(30),
        follow(
            fe.clone(),
            d.client(),
            run.id.clone(),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("follow ends with the run");
    let q = fe.questions.lock().unwrap().clone();
    assert_eq!(
        q,
        vec![(
            format!("[run {}] Which file?", run.id),
            vec!["a.txt".to_string(), "b.txt".to_string()]
        )]
    );
    let second = seen.lock().unwrap()[1]["messages"].to_string();
    assert!(second.contains("User answered: b.txt"), "{second}");
    assert_eq!(d.m.get(&run.id).unwrap().state, RunState::Done);
    d.finish().await;
}

#[tokio::test]
async fn following_stops_when_cancelled_or_the_run_is_unknown() {
    let d = Daemon::start("cli-cancel").await;
    let repo = make_repo(&d.home);
    let (base, _) = mock_server(needs_permission()).await;
    let fe = Recorder::new(|| PermissionReply::Allow, None);
    // An unknown run: nothing to follow.
    follow(
        fe.clone(),
        d.client(),
        "zzzz".into(),
        CancellationToken::new(),
    )
    .await;
    assert!(fe.shown().is_empty());
    let run = d.m.create(new_run(&repo, &base, "build it")).await.unwrap();
    wait_for(&d.m, &run.id, "needs-input", |r| {
        r.state == RunState::NeedsInput
    })
    .await;
    // Detaching while the prompt is up leaves the run waiting.
    struct Never;
    #[async_trait]
    impl Frontend for Never {
        fn emit(&self, _: Event) {}
        async fn permission(&self, _: PermissionRequest) -> PermissionReply {
            std::future::pending().await
        }
        async fn question(&self, _: &str, _: &[String]) -> Option<String> {
            None
        }
    }
    let token = CancellationToken::new();
    let h = tokio::spawn(follow(
        Arc::new(Never),
        d.client(),
        run.id.clone(),
        token.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(200)).await;
    token.cancel();
    tokio::time::timeout(Duration::from_secs(10), h)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(d.m.get(&run.id).unwrap().state, RunState::NeedsInput);
    d.finish().await;
}

#[tokio::test]
async fn slash_commands_start_list_follow_and_stop_runs() {
    let d = Daemon::start("cli-slash").await;
    let repo = make_repo(&d.home);
    let (base, _) = mock_server(needs_permission()).await;
    // Runs started from a session use the configured provider.
    let cfg = d.home.dir.join("home").join("config.toml");
    let text = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(
        &cfg,
        format!("provider = \"openai\"\nbase_url = \"{base}\"\n\n[background]\nsandbox = false\nverify = false\n{text}"),
    )
    .unwrap();
    let env = make_env(&base, &repo, Mode::Default, 32768);
    assert!(
        slash_bg(&env, "  ")
            .await
            .starts_with("Usage: `/bg <task>`")
    );
    let out = slash_bg(&env, "build it").await;
    assert!(out.starts_with("Started background run `"), "{out}");
    let runs = d.m.list();
    assert_eq!(runs.len(), 1);
    let id = runs[0].id.clone();
    assert_eq!(runs[0].task, "build it");
    assert_eq!(runs[0].settings.model.as_deref(), Some("mock"));
    assert!(out.contains(&format!("/runs attach {id}")), "{out}");
    wait_for(&d.m, &id, "needs-input", |r| {
        r.state == RunState::NeedsInput
    })
    .await;

    let list = slash_runs(&env, "").await;
    assert!(list.starts_with("```\nID ") && list.contains(&id), "{list}");
    assert!(list.contains("needs-input"), "{list}");
    let show = slash_runs(&env, &id).await;
    assert!(show.contains(&format!("Run {id}  needs-input")), "{show}");
    assert_eq!(slash_runs(&env, &format!("show {id}")).await, show);
    assert!(slash_runs(&env, "show zzzz").await.contains("no run"));
    assert!(slash_runs(&env, "a b c").await.starts_with("Usage: `/runs"));
    assert_eq!(slash_runs(&env, "detach").await, "Not attached to a run.");
    let att = slash_runs(&env, &format!("attach {id}")).await;
    assert!(
        att.starts_with(&format!("Following run `{id}` (needs-input)")),
        "{att}"
    );
    assert_eq!(
        slash_runs(&env, "detach").await,
        format!("Detached from run `{id}`; it keeps going.")
    );
    let r = slash_runs(&env, &format!("resume {id}")).await;
    assert!(r.contains("is needs-input"), "{r}");
    assert_eq!(
        slash_runs(&env, &format!("stop {id}")).await,
        format!("Stopping run `{id}`.")
    );
    let r = wait_for(&d.m, &id, "cancelled", finished).await;
    assert_eq!(r.state, RunState::Cancelled);
    assert_eq!(
        slash_runs(&env, &format!("resume {id}")).await,
        format!("Resumed run `{id}`.")
    );
    // A goal run says so.
    let out = slash_bg_kind(&env, "reach it", Some("goal")).await;
    assert!(out.starts_with("Started background goal `"), "{out}");
    assert!(d.m.list().iter().any(|r| r.kind.as_deref() == Some("goal")));
    d.finish().await;
}

#[tokio::test]
async fn slash_bg_reports_a_failed_start() {
    let home = TestHome::new("cli-slash-fail");
    let plain = home.dir.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let env = make_env("http://127.0.0.1:1", &plain, Mode::Default, 32768);
    let out = slash_bg(&env, "x").await;
    assert!(
        out.starts_with("Couldn't start a background run:") && out.contains("git"),
        "{out}"
    );
}
