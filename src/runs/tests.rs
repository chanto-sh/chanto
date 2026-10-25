//! Background runs end to end, in-process: a temp git repo, a mock
//! OpenAI-compatible model, and the manager with in-process workers.

use super::manager::{Launcher, Manager, NewRun};
use super::*;
use crate::agent::tests::{mock_server, tc};
use crate::util::Canonical as _;
use serde_json::json;
use std::ffi::OsString;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

/// A temp CHANTO_HOME (and no Claude Code / Codex imports from the real home).
pub(crate) struct TestHome {
    pub(crate) dir: PathBuf,
    prev: Vec<(&'static str, Option<OsString>)>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl TestHome {
    pub(crate) fn new(name: &str) -> TestHome {
        let guard = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "chanto-runs-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(dir.join("compat")).unwrap();
        std::fs::write(
            home.join("config.toml"),
            "[compat]\nclaude = false\ncodex = false\n\n[guardrails]\nchecks = [\"test -f hello.txt\"]\n",
        )
        .unwrap();
        let mut prev = Vec::new();
        for (k, v) in [
            ("CHANTO_HOME", home.clone()),
            ("CHANTO_COMPAT_HOME", dir.join("compat")),
        ] {
            prev.push((k, std::env::var_os(k)));
            // SAFETY: test-only, under TEST_HOME_LOCK.
            unsafe { std::env::set_var(k, v) };
        }
        TestHome {
            dir,
            prev,
            _guard: guard,
        }
    }

    pub(crate) fn store(&self) -> Store {
        Store::new(self.dir.join("home").join("runs"))
    }

    pub(crate) fn manager(&self) -> Arc<Manager> {
        Manager::open(self.store(), Launcher::InProcess, 2)
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        for (k, v) in self.prev.drain(..) {
            // SAFETY: test-only, under TEST_HOME_LOCK.
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub(crate) fn sh(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

pub(crate) fn make_repo(home: &TestHome) -> PathBuf {
    let repo = home.dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    sh(&repo, &["init", "-q", "-b", "main"]);
    sh(&repo, &["config", "user.email", "t@example.com"]);
    sh(&repo, &["config", "user.name", "T"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    repo.canonical().unwrap()
}

/// A file git checked out, with line endings as committed: Git for Windows
/// defaults to `core.autocrlf=true`, which writes CRLF into checkouts.
pub(crate) fn checked_out(p: impl AsRef<Path>) -> String {
    std::fs::read_to_string(p).unwrap().replace("\r\n", "\n")
}

/// Every path in the working tree, except `.git`.
pub(crate) fn tree(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if e.file_name() == ".git" {
                continue;
            }
            if p.is_dir() {
                stack.push(p.clone());
            }
            out.push(p.strip_prefix(dir).unwrap().to_path_buf());
        }
    }
    out.sort();
    out
}

pub(crate) fn new_run(repo: &Path, base: &str, task: &str) -> NewRun {
    NewRun {
        task: task.into(),
        cwd: Some(repo.to_path_buf()),
        provider: Some("openai".into()),
        base_url: Some(base.into()),
        model: Some("mock".into()),
        sandbox: Some(false),
        verify: Some(false),
        ..Default::default()
    }
}

pub(crate) async fn wait_for(m: &Manager, id: &str, what: &str, f: impl Fn(&Run) -> bool) -> Run {
    for _ in 0..600 {
        let r = m.get(id).unwrap();
        if f(&r) {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("run {id} never became {what}: {:?}", m.get(id).unwrap());
}

pub(crate) fn finished(r: &Run) -> bool {
    !r.state.is_active()
}

pub(crate) fn write_hello() -> Vec<Value> {
    vec![
        json!({"tool_calls": tc("Write", json!({"file_path": "hello.txt", "content": "hi\n"}))}),
        json!({"content": "Created hello.txt."}),
    ]
}

#[tokio::test]
async fn run_lifecycle_in_its_own_worktree() {
    let home = TestHome::new("life");
    let repo = make_repo(&home);
    // Uncommitted work comes along; the working copy itself is never touched.
    std::fs::write(repo.join("README.md"), "hello\nlocal edit\n").unwrap();
    std::fs::write(repo.join("notes.txt"), "draft\n").unwrap();
    let status_before = sh(&repo, &["status", "--porcelain"]);
    let files_before = tree(&repo);
    let (base, _) = mock_server(write_hello()).await;
    let m = home.manager();
    let mut req = new_run(&repo, &base, "create hello.txt");
    req.verify = Some(true);
    let run = m.create(req).await.unwrap();
    assert_eq!(run.branch, format!("chanto/run/{}", run.id));
    assert!(run.source.snapshot.is_some());
    assert_eq!(run.settings.mode, "acceptEdits");
    assert_eq!(run.settings.max_cost_usd, 2.0);
    // The worktree lives under the Chanto home, not in the project.
    assert!(
        run.worktree.starts_with(home.store().dir.join("worktrees")),
        "{}",
        run.worktree.display()
    );
    assert!(!run.worktree.starts_with(&repo));
    assert_eq!(
        tree(&repo),
        files_before,
        "starting a run must not add files to the project"
    );
    // It inherits the source repository's trust (untrusted here).
    assert!(!crate::config::is_trusted(&run.worktree));

    let done = wait_for(&m, &run.id, "finished", finished).await;
    assert_eq!(done.state, RunState::Done, "{:?}", done.error);
    assert_eq!(
        done.result
            .as_deref()
            .map(|r| r.starts_with("Created hello.txt.")),
        Some(true)
    );
    let verify = done.verify.clone().unwrap();
    assert_eq!(verify.status, "passed", "{verify:?}");
    assert!(verify.detail.contains("test -f hello.txt"), "{verify:?}");
    assert!(done.session_id.is_some());

    // The branch has the snapshot plus the run's own commit.
    assert_eq!(
        sh(&repo, &["show", &format!("{}:hello.txt", done.branch)]),
        "hi"
    );
    assert_eq!(
        sh(&repo, &["show", &format!("{}:notes.txt", done.branch)]),
        "draft"
    );
    assert_eq!(
        sh(&repo, &["show", &format!("{}:README.md", done.branch)]),
        "hello\nlocal edit"
    );
    assert_eq!(git::commit_count(&repo, &done.base, &done.branch), 1);
    assert_eq!(
        sh(&repo, &["rev-parse", &format!("{}^", done.base)]),
        done.source.head
    );

    // The user's working copy, index and branch are as they were.
    assert_eq!(tree(&repo), files_before);
    assert!(!repo.join("hello.txt").exists());
    assert_eq!(sh(&repo, &["status", "--porcelain"]), status_before);
    assert_eq!(sh(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    assert_eq!(checked_out(repo.join("README.md")), "hello\nlocal edit\n");

    // Persisted: the record and its event log.
    let stored = home.store().load(&run.id).unwrap();
    assert_eq!(stored.state, RunState::Done);
    assert_eq!(stored.verify, done.verify);
    let events = home.store().events(&run.id);
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "run_state" && e["state"] == "done")
    );
    assert!(
        events
            .windows(2)
            .all(|w| w[0]["seq"].as_u64() < w[1]["seq"].as_u64())
    );
    let detail = m.detail(&run.id[..4]).await.unwrap();
    assert_eq!(detail["commits"], 1);
    assert!(detail["diff_stat"].as_str().unwrap().contains("hello.txt"));

    // With local changes present, apply leaves the run's diff uncommitted.
    let msg = git::apply(&repo, &done).unwrap();
    assert!(msg.contains("working tree"), "{msg}");
    assert_eq!(checked_out(repo.join("hello.txt")), "hi\n");
    assert_eq!(sh(&repo, &["log", "--oneline"]).lines().count(), 1);

    m.remove(&run.id).await.unwrap();
    assert!(!done.worktree.exists());
    assert!(!sh(&repo, &["worktree", "list"]).contains(&run.id));
    assert!(sh(&repo, &["branch", "--list", &done.branch]).is_empty());
    assert!(m.get(&run.id).is_err());
}

#[tokio::test]
async fn apply_cherry_picks_onto_the_current_branch() {
    let home = TestHome::new("apply");
    let repo = make_repo(&home);
    let (base, _) = mock_server(write_hello()).await;
    let m = home.manager();
    let run = m
        .create(new_run(&repo, &base, "add a hello file"))
        .await
        .unwrap();
    assert!(run.source.snapshot.is_none());
    let done = wait_for(&m, &run.id, "finished", finished).await;
    assert_eq!(done.state, RunState::Done, "{:?}", done.error);
    assert_eq!(done.verify.as_ref().unwrap().status, "off");
    // Trusting the project trusts its runs' worktrees.
    crate::config::trust(&repo).unwrap();
    assert!(crate::config::is_trusted(&done.worktree));
    assert!(crate::config::is_trusted(&done.worktree.join("src")));
    // Work moved on meanwhile; the run's commit still lands on top.
    std::fs::write(repo.join("other.txt"), "x\n").unwrap();
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "meanwhile"]);
    let msg = git::apply(&repo, &done).unwrap();
    assert!(msg.contains("Applied 1 commit(s)"), "{msg}");
    assert_eq!(sh(&repo, &["log", "-1", "--format=%s"]), "add a hello file");
    assert_eq!(checked_out(repo.join("hello.txt")), "hi\n");
    assert!(sh(&repo, &["status", "--porcelain"]).is_empty());
    assert_eq!(sh(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
}

/// The source checkout's gitignored `.env` never reaches the worktree, but
/// its values are masked in the run all the same, and the run's files are
/// the owner's alone.
#[tokio::test]
async fn runs_mask_the_checkouts_env_files_and_keep_their_files_private() {
    let home = TestHome::new("secrets");
    let _secrets = crate::secrets::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let repo = make_repo(&home);
    std::fs::write(repo.join(".gitignore"), ".env\n").unwrap();
    sh(&repo, &["add", ".gitignore"]);
    sh(&repo, &["commit", "-q", "-m", "ignore .env"]);
    let pw = "bg-run-pw-4d3c2b1a0f";
    std::fs::write(repo.join(".env"), format!("DB_PASSWORD={pw}\n")).unwrap();
    let (base, seen) = mock_server(vec![
        bash(&format!("echo pw={pw}")),
        json!({"content": "Done."}),
    ])
    .await;
    let m = home.manager();
    let run = m
        .create(new_run(&repo, &base, "print the password"))
        .await
        .unwrap();
    assert!(!run.worktree.join(".env").exists());
    let done = wait_for(&m, &run.id, "finished", finished).await;
    assert_eq!(done.state, RunState::Done, "{:?}", done.error);
    let sent = seen.lock().unwrap()[1]["messages"].to_string();
    assert!(
        !sent.contains(pw) && sent.contains("[REDACTED:DB_PASSWORD]"),
        "{sent}"
    );
    let log = std::fs::read_to_string(home.store().events_path(&run.id)).unwrap();
    assert!(
        !log.contains(pw) && log.contains("[REDACTED:DB_PASSWORD]"),
        "{log}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let store = home.store();
        assert_eq!(mode(&store.dir), 0o700);
        assert_eq!(mode(&store.run_dir(&run.id)), 0o700);
        assert_eq!(mode(&store.run_dir(&run.id).join("run.json")), 0o600);
        assert_eq!(mode(&store.events_path(&run.id)), 0o600);
    }
    crate::secrets::init(&crate::config::Config::default(), &home.dir);
}

pub(crate) fn needs_permission() -> Vec<Value> {
    vec![
        json!({"tool_calls": tc("Bash", json!({"command": "make build-it"}))}),
        json!({"content": "Finished after the command."}),
    ]
}

#[tokio::test]
async fn permission_prompts_pause_the_run_until_answered() {
    let home = TestHome::new("input");
    let repo = make_repo(&home);
    let (base, seen) = mock_server(needs_permission()).await;
    let m = home.manager();
    let run = m.create(new_run(&repo, &base, "build it")).await.unwrap();
    let waiting = wait_for(&m, &run.id, "needs-input", |r| {
        r.state == RunState::NeedsInput
    })
    .await;
    let p = waiting.pending.clone().unwrap();
    assert_eq!(p["type"], "permission_request");
    assert!(p.to_string().contains("make build-it"), "{p}");
    assert!(
        home.store().load(&run.id).unwrap().pending.is_some(),
        "persisted for `chanto runs`"
    );
    // Answers must name a decision.
    assert!(m.respond(&run.id, json!({"decision": "maybe"})).is_err());
    assert!(
        m.respond(
            &run.id,
            json!({"request_id": "req-other", "decision": "allow"})
        )
        .is_err()
    );
    m.respond(&run.id, json!({"decision": "allow"})).unwrap();
    let done = wait_for(&m, &run.id, "finished", finished).await;
    assert_eq!(done.state, RunState::Done, "{:?}", done.error);
    assert!(done.pending.is_none());
    // The command ran: its output went back to the model.
    let second = seen.lock().unwrap()[1].clone();
    assert!(
        second["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool")
    );
    let events = home.store().events(&run.id);
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "request_done" && e["answered"] == true)
    );
}

#[tokio::test]
async fn stop_cancels_a_waiting_run() {
    let home = TestHome::new("stop");
    let repo = make_repo(&home);
    let (base, _) = mock_server(needs_permission()).await;
    let m = home.manager();
    let run = m.create(new_run(&repo, &base, "build it")).await.unwrap();
    wait_for(&m, &run.id, "needs-input", |r| {
        r.state == RunState::NeedsInput
    })
    .await;
    m.stop(&run.id).unwrap();
    let r = wait_for(&m, &run.id, "finished", finished).await;
    assert_eq!(r.state, RunState::Cancelled);
    assert!(r.pending.is_none());
    assert_eq!(
        home.store().load(&run.id).unwrap().state,
        RunState::Cancelled
    );
    // Stopping twice is an error, and the worktree stays for inspection.
    assert!(m.stop(&run.id).is_err());
    assert!(r.worktree.exists());
}

#[tokio::test]
async fn daemon_restart_marks_interrupted_runs_and_resumes_them() {
    let home = TestHome::new("restart");
    let repo = make_repo(&home);
    let (base, seen) = mock_server(vec![
        json!({"tool_calls": tc("Bash", json!({"command": "make build-it"}))}),
        json!({"content": "Picked up where I left off."}),
    ])
    .await;
    let m = home.manager();
    let run = m.create(new_run(&repo, &base, "build it")).await.unwrap();
    wait_for(&m, &run.id, "needs-input", |r| {
        r.state == RunState::NeedsInput
    })
    .await;
    // Graceful daemon stop: the worker saves its session and ends interrupted.
    m.close().await;
    let r = wait_for(&m, &run.id, "interrupted", |r| {
        r.state == RunState::Interrupted
    })
    .await;
    assert!(r.session_id.is_some());
    drop(m);
    // A record a crashed daemon left behind as running.
    let mut crashed = r.clone();
    crashed.id = "deadbeef".into();
    crashed.state = RunState::Running;
    home.store().save(&crashed).unwrap();

    let m = home.manager();
    assert_eq!(m.get("deadbeef").unwrap().state, RunState::Interrupted);
    assert!(m.get("deadbeef").unwrap().error.unwrap().contains("resume"));
    assert_eq!(m.get(&run.id).unwrap().state, RunState::Interrupted);
    assert_eq!(
        home.store().load("deadbeef").unwrap().state,
        RunState::Interrupted
    );

    // Resume continues the saved conversation.
    m.resume(&run.id).unwrap();
    let done = wait_for(&m, &run.id, "finished", finished).await;
    assert_eq!(done.state, RunState::Done, "{:?}", done.error);
    assert_eq!(done.result.as_deref(), Some("Picked up where I left off."));
    let last = seen.lock().unwrap().last().unwrap().clone();
    let text = last["messages"].to_string();
    assert!(
        text.contains("build it") && text.contains("interrupted"),
        "{text}"
    );
}

#[tokio::test]
async fn non_git_directories_are_refused() {
    let home = TestHome::new("nogit");
    let plain = home.dir.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let m = home.manager();
    let err = m
        .create(new_run(&plain, "http://127.0.0.1:1", "anything"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not inside a git repository"), "{err}");
    assert!(m.list().is_empty());
    let err = m
        .create(NewRun {
            task: " ".into(),
            cwd: Some(plain),
            ..Default::default()
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("task"), "{err}");
}

#[test]
fn last_messages_rebuild_from_events() {
    let ev = |t: &str, text: &str| json!({"type": t, "text": text});
    let events = vec![
        ev("assistant_start", ""),
        ev("text", "first "),
        ev("text", "answer"),
        ev("assistant_end", ""),
        ev("assistant_start", ""),
        ev("text", "second"),
        ev("assistant_end", ""),
    ];
    assert_eq!(last_messages(&events, 1), vec!["second"]);
    assert_eq!(last_messages(&events, 5), vec!["first answer", "second"]);
    assert_eq!(fmt_duration(42), "42s");
    assert_eq!(fmt_duration(192), "3m12s");
    assert_eq!(fmt_duration(3900), "1h05m");
}

/// A local HTTP endpoint that records every request (headers and body).
async fn capture_server() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let s2 = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let seen = s2.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let s = String::from_utf8_lossy(&buf).to_string();
                    if let Some(h) = s.find("\r\n\r\n") {
                        let cl = s[..h]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buf.len() >= h + 4 + cl {
                            break;
                        }
                    }
                }
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf).to_string());
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
            });
        }
    });
    (base, seen)
}

async fn wait_requests(seen: &std::sync::Mutex<Vec<String>>, n: usize) -> Vec<String> {
    for _ in 0..200 {
        if seen.lock().unwrap().len() >= n {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    seen.lock().unwrap().clone()
}

#[tokio::test]
async fn notifications_carry_state_and_link_but_no_task_or_code() {
    use super::notify::{Notifier, NotifyConfig};
    let home = TestHome::new("notify");
    let repo = make_repo(&home);
    let (hooks, seen) = capture_server().await;
    let notifier = || {
        Notifier::new(
            NotifyConfig {
                ntfy: Some(format!("{hooks}/my-topic")),
                webhook: Some(format!("{hooks}/hook")),
                desktop: Some(false),
                link: None,
            },
            false,
            Some("http://phone.example:4141".into()),
        )
    };
    // Through the manager: needs-input, then done.
    let (base, _) = mock_server(vec![
        json!({"tool_calls": tc("Bash", json!({"command": "make SECRET_BUILD_TARGET"}))}),
        json!({"content": "fn main() { println!(\"SECRET_CODE\") }"}),
    ])
    .await;
    let m = home.manager();
    m.set_notifier(notifier());
    let run = m
        .create(new_run(&repo, &base, "fix SECRET_TASK_TEXT in src/main.rs"))
        .await
        .unwrap();
    wait_for(&m, &run.id, "needs-input", |r| {
        r.state == RunState::NeedsInput
    })
    .await;
    let got = wait_requests(&seen, 2).await;
    assert_eq!(got.len(), 2, "{got:?}");
    m.respond(&run.id, json!({"decision": "allow"})).unwrap();
    wait_for(&m, &run.id, "finished", finished).await;
    let got = wait_requests(&seen, 4).await;
    assert_eq!(got.len(), 4, "{got:?}");
    for req in &got {
        assert!(req.contains(&run.id), "{req}");
        // Report what leaked and where, without echoing the leaked text.
        let request_line = req.lines().next().unwrap_or_default();
        for (what, private) in [
            ("the task", "SECRET_TASK_TEXT"),
            ("a command", "SECRET_BUILD_TARGET"),
            ("the model's code", "SECRET_CODE"),
            ("the model's code", "fn main"),
            ("a file path", "src/main.rs"),
        ] {
            assert!(!req.contains(private), "{what} leaked into {request_line}");
        }
    }
    let ntfy: Vec<&String> = got
        .iter()
        .filter(|r| r.starts_with("POST /my-topic"))
        .collect();
    assert_eq!(ntfy.len(), 2);
    assert!(
        ntfy.iter()
            .any(|r| r.to_lowercase().contains("title: chanto: run") && r.contains("needs input"))
    );
    assert!(ntfy.iter().all(|r| {
        r.to_lowercase()
            .contains(&format!("click: http://phone.example:4141/#run={}", run.id))
    }));
    let hooks: Vec<Value> = got
        .iter()
        .filter(|r| r.starts_with("POST /hook"))
        .map(|r| serde_json::from_str(&r[r.find("\r\n\r\n").unwrap() + 4..]).unwrap())
        .collect();
    assert!(hooks.iter().any(|h| h["event"] == "run.needs-input"));
    assert!(hooks.iter().any(|h| h["event"] == "run.done"
        && h["state"] == "done"
        && h["url"] == format!("http://phone.example:4141/#run={}", run.id)));
    // Nothing for states nobody needs to hear about.
    let mut r = m.get(&run.id).unwrap();
    r.state = RunState::Running;
    assert!(notifier().note(&r).is_none());
}

#[tokio::test]
async fn remote_runs_round_trip_through_a_bundle_and_a_shared_git_remote() {
    let home = TestHome::new("remote");
    // The "server": the runs API in-process, with its own store.
    let m = home.manager();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = super::api::router::<()>(m.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let c = super::cli::Client::new(&url, "unused");
    let (base, _) = mock_server(vec![
        json!({"tool_calls": tc("Write", json!({"file_path": "hello.txt", "content": "hi\n"}))}),
        json!({"content": "Created hello.txt."}),
        json!({"tool_calls": tc("Write", json!({"file_path": "second.txt", "content": "two\n"}))}),
        json!({"content": "Created second.txt."}),
    ])
    .await;
    let body = |task: &str| json!({"task": task, "model": "mock", "provider": "openai", "base_url": base, "sandbox": false, "verify": false});

    // 1. Uncommitted changes and no shared remote: HEAD + snapshot go up as a bundle.
    let repo = make_repo(&home);
    std::fs::write(repo.join("README.md"), "hello\nlocal edit\n").unwrap();
    let files_before = tree(&repo);
    let run = super::cli::start_remote(&c, "srv", &repo, body("create hello.txt"))
        .await
        .unwrap();
    assert_eq!(run.source.kind, "bundle");
    assert!(run.source.snapshot.is_some());
    assert!(!run.worktree.starts_with(&repo));
    assert_eq!(tree(&repo), files_before);
    let done = wait_for(&m, &run.id, "finished", finished).await;
    assert_eq!(done.state, RunState::Done, "{:?}", done.error);
    assert_eq!(
        checked_out(done.worktree.join("README.md")),
        "hello\nlocal edit\n"
    );
    // Bring it back: the run's commit lands in the dirty working tree.
    let msg = super::cli::apply_remote(&c, &repo, &done, false)
        .await
        .unwrap();
    assert!(msg.contains("working tree"), "{msg}");
    assert_eq!(checked_out(repo.join("hello.txt")), "hi\n");
    // Applying on the server is refused for code from elsewhere.
    assert!(
        m.apply(&run.id, false)
            .await
            .unwrap_err()
            .to_string()
            .contains("--on")
    );

    // 2. A clean tree and `[remote.srv] git_remote`: the branch is pushed and fetched.
    let origin = home.dir.join("origin.git");
    sh(
        &home.dir,
        &["init", "-q", "--bare", &origin.display().to_string()],
    );
    sh(
        &repo,
        &["remote", "add", "origin", &origin.display().to_string()],
    );
    sh(&repo, &["add", "-A"]);
    sh(&repo, &["commit", "-q", "-m", "with hello"]);
    let cfg = home.dir.join("home").join("config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str("\n[remote.srv]\ngit_remote = \"origin\"\n");
    std::fs::write(&cfg, text).unwrap();
    let run = super::cli::start_remote(&c, "srv", &repo, body("create second.txt"))
        .await
        .unwrap();
    assert_eq!(run.source.kind, "git");
    assert_eq!(
        run.source.url.as_deref(),
        Some(origin.display().to_string().as_str())
    );
    assert_eq!(
        sh(&origin, &["rev-parse", "main"]),
        sh(&repo, &["rev-parse", "HEAD"])
    );
    let done = wait_for(&m, &run.id, "finished", finished).await;
    assert_eq!(done.state, RunState::Done, "{:?}", done.error);
    let msg = super::cli::apply_remote(&c, &repo, &done, false)
        .await
        .unwrap();
    assert!(msg.contains("Applied 1 commit(s)"), "{msg}");
    assert_eq!(
        sh(&repo, &["log", "-1", "--format=%s"]),
        "create second.txt"
    );
    assert_eq!(checked_out(repo.join("second.txt")), "two\n");
    // Bad sources are refused clearly.
    let err = m
        .create(NewRun {
            task: "x".into(),
            source: Some(json!({"kind": "bundle", "bundle_id": "nope", "ref": "refs/x"})),
            ..Default::default()
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no uploaded bundle"), "{err}");
    assert!(m.save_bundle(b"not a bundle").await.is_err());
    server.abort();
}

#[tokio::test]
async fn goal_runs_work_in_the_background_and_wait_for_criteria_approval() {
    let home = TestHome::new("goal-run");
    let repo = make_repo(&home);
    let step = std::sync::atomic::AtomicUsize::new(0);
    let crit = r#"[{"id": "c1", "title": "hello.txt exists", "check": "test -f hello.txt"}]"#;
    let (base, _) = crate::agent::tests::mock_server_with(Arc::new(move |body: &Value| {
        let first = body["messages"].as_array().and_then(|m| m.iter().find(|m| m["role"] == "user")).and_then(|m| m["content"].as_str()).unwrap_or("").to_string();
        Some(match step.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
            0 => {
                let dir = regex::Regex::new(r"Write them to (\S+)/criteria\.json").unwrap().captures(&first).unwrap()[1].to_string();
                json!({"tool_calls": tc("Write", json!({"file_path": format!("{dir}/criteria.json"), "content": crit}))})
            }
            1 => json!({"content": "One criterion."}),
            2 => json!({"tool_calls": tc("Write", json!({"file_path": "hello.txt", "content": "hi\n"}))}),
            _ => json!({"content": "Created hello.txt."}),
        })
    }))
    .await;
    let m = home.manager();
    let run = m
        .create(NewRun {
            kind: Some("goal".into()),
            ..new_run(&repo, &base, "create hello.txt")
        })
        .await
        .unwrap();
    assert_eq!(run.kind.as_deref(), Some("goal"));
    // The criteria wait for approval like any other question.
    let waiting = wait_for(&m, &run.id, "needs-input", |r| {
        r.state == RunState::NeedsInput
    })
    .await;
    let p = waiting.pending.unwrap();
    assert_eq!(p["type"], "question");
    assert!(
        p["question"]
            .as_str()
            .unwrap()
            .contains("test -f hello.txt"),
        "{p}"
    );
    m.respond(&run.id, json!({"answer": "Approve and start"}))
        .unwrap();
    let done = wait_for(&m, &run.id, "finished", finished).await;
    assert_eq!(
        done.state,
        RunState::Done,
        "{:?} {:?}",
        done.error,
        done.result
    );
    assert!(
        done.result
            .as_deref()
            .unwrap()
            .contains("Done: all 1 acceptance criteria pass"),
        "{:?}",
        done.result
    );
    // The goal's state is under the Chanto home, keyed by the run id; the branch has only the work.
    let g = crate::goal::Goal::load(&home.dir.join("home").join("goals").join(&run.id)).unwrap();
    assert_eq!(g.state, crate::goal::GoalState::Done);
    assert_eq!(
        sh(&repo, &["show", &format!("{}:hello.txt", done.branch)]),
        "hi"
    );
    assert_eq!(
        sh(&repo, &["diff", "--name-only", &done.base, &done.branch]),
        "hello.txt"
    );
    // Unknown kinds are refused.
    let err = m
        .create(NewRun {
            kind: Some("epic".into()),
            ..new_run(&repo, &base, "x")
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown run kind"), "{err}");
}

/// A path as a TOML string (Windows paths have backslashes to escape).
fn toml_str(p: &Path) -> String {
    toml::Value::String(p.display().to_string()).to_string()
}

#[tokio::test]
async fn scheduled_jobs_become_background_runs() {
    use crate::runner::{self, Scheduler};
    let home = TestHome::new("sched");
    let repo = make_repo(&home);
    let mut script = needs_permission();
    script.extend(write_hello());
    let (base, _) = mock_server(script).await;
    let cfg = home.dir.join("home").join("config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str("\n[background]\nsandbox = false\nverify = false\n");
    std::fs::write(&cfg, text).unwrap();
    let job = |name: &str, schedule: &str| {
        format!(
            "[[job]]\nname = \"{name}\"\n{schedule}prompt = \"do {name}\"\ncwd = {}\nprovider = \"openai\"\nbase_url = \"{base}\"\nmodel = \"mock\"\n\n",
            toml_str(&repo)
        )
    };
    std::fs::write(
        home.dir.join("home").join("runner.toml"),
        [
            job("hello", "schedule = \"10m\"\n"),
            job("leap", "schedule = \"0 0 29 2 *\"\n"),
            job("manual", ""),
        ]
        .concat(),
    )
    .unwrap();
    let m = home.manager();
    let s = Scheduler::new(m.clone());

    // The interval job is due at once; the cron one waits for its slot; the
    // unscheduled one only runs when triggered.
    let pass = s.run_due(None, None, false, "schedule").await;
    assert_eq!(pass["errors"], json!([]), "{pass}");
    let fired = pass["fired"].as_array().unwrap();
    assert_eq!(fired.len(), 1, "{pass}");
    assert_eq!(fired[0]["job"], "hello");
    let id = fired[0]["run"]["id"].as_str().unwrap().to_string();
    let run = m.get(&id).unwrap();
    assert_eq!(
        (
            run.job.as_deref(),
            run.trigger.as_deref(),
            run.task.as_str()
        ),
        (Some("hello"), Some("schedule"), "do hello")
    );
    assert!(
        run.worktree.starts_with(home.store().dir.join("worktrees")),
        "isolated like any background run"
    );

    // A job never overlaps itself.
    wait_for(&m, &id, "needs-input", |r| r.state == RunState::NeedsInput).await;
    let again = s.run_due(None, Some("hello".into()), false, "manual").await;
    assert!(again["fired"].as_array().unwrap().is_empty(), "{again}");
    assert!(
        again["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("still running"),
        "{again}"
    );
    m.respond(&id, json!({"decision": "allow"})).unwrap();
    let done = wait_for(&m, &id, "finished", finished).await;
    assert_eq!(done.state, RunState::Done, "{:?}", done.error);
    let st = runner::load_state();
    assert_eq!(
        (
            st.jobs["hello"].last_run_id.as_deref(),
            st.jobs["hello"].runs
        ),
        (Some(id.as_str()), 1)
    );
    assert!(st.jobs["leap"].first_seen.is_some() && st.jobs["leap"].last_run.is_none());
    // Not due again until the interval passes.
    let later = s.run_due(None, None, false, "schedule").await;
    assert!(later["fired"].as_array().unwrap().is_empty(), "{later}");
    let table = runner::table(&runner::load(None), None);
    assert!(table.contains(&format!("{id} done")), "{table}");

    // Webhooks: the per-job token, as a bearer token or a GitHub signature.
    let app = runner::public_router::<()>(s.clone(), Arc::new("server-token".into()))
        .merge(runner::router(s.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/runs/jobs/manual/trigger",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = crate::http::client();
    assert_eq!(http.post(&url).send().await.unwrap().status(), 401);
    let token = runner::job_token("manual", false).unwrap();
    assert_eq!(
        runner::job_token("manual", false).unwrap(),
        token,
        "stable until rotated"
    );
    assert_eq!(
        http.post(format!("{url}?token=wrong"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let ping = http
        .post(&url)
        .header("x-github-event", "ping")
        .header("x-chanto-token", &token)
        .send()
        .await
        .unwrap();
    assert_eq!(ping.status(), 200);
    assert_eq!(m.list().len(), 1, "a ping starts nothing");
    let r = http
        .post(&url)
        .bearer_auth(&token)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let hooked: Run = r.json().await.unwrap();
    assert_eq!(
        (hooked.job.as_deref(), hooked.trigger.as_deref()),
        (Some("manual"), Some("webhook"))
    );
    let done = wait_for(&m, &hooked.id, "finished", finished).await;
    assert_eq!(done.state, RunState::Done, "{:?}", done.error);
    assert_eq!(
        sh(&repo, &["show", &format!("{}:hello.txt", done.branch)]),
        "hi"
    );
    // The daemon's own token works too; unknown jobs are 404 for it.
    let missing = url.replace("/manual/", "/nope/");
    assert_eq!(
        http.post(&missing)
            .bearer_auth("server-token")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        http.post(&missing)
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        401,
        "a job token is only good for its job"
    );
    server.abort();
}

#[test]
fn notifications_name_goal_team_and_job_runs_by_id_only() {
    use super::notify::{Notifier, NotifyConfig};
    let n = Notifier::new(
        NotifyConfig {
            desktop: Some(false),
            ..Default::default()
        },
        false,
        None,
    );
    let run = |extra: Value| -> Run {
        let mut v = json!({"id": "abc12345", "task": "SECRET task", "state": "done", "created": "", "worktree": "", "branch": "b", "base": "x",
            "settings": {}, "source": {"kind": "local", "path": "/r", "head": "x"}});
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        serde_json::from_value(v).unwrap()
    };
    let title = |extra: Value| n.note(&run(extra)).unwrap().title;
    assert_eq!(title(json!({})), "chanto: run abc12345 done");
    assert_eq!(
        title(json!({"kind": "goal"})),
        "chanto: goal run abc12345 done"
    );
    assert_eq!(
        title(json!({"kind": "goal", "job": "nightly", "trigger": "schedule"})),
        "chanto: job nightly (goal run abc12345) done"
    );
    assert_eq!(
        title(json!({"team": "t1"})),
        "chanto: team t1 run abc12345 done"
    );
}

/// Docker answering (the container tests are skipped without it).
fn docker_works() -> bool {
    Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .output()
        .is_ok_and(|o| o.status.success())
        && Command::new("docker")
            .args(["run", "--rm", "--pull", "never", "alpine:latest", "true"])
            .output()
            .is_ok_and(|o| o.status.success())
}

fn labeled(id: &str) -> String {
    let o = Command::new("docker")
        .args(["ps", "-aq", "--filter", &format!("label=chanto.run={id}")])
        .output()
        .unwrap();
    let n = Command::new("docker")
        .args([
            "network",
            "ls",
            "-q",
            "--filter",
            &format!("label=chanto.run={id}"),
        ])
        .output()
        .unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout).trim(),
        String::from_utf8_lossy(&n.stdout).trim()
    )
}

async fn wait_removed(id: &str) {
    for _ in 0..200 {
        if labeled(id).is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("run {id}'s container was not removed: {}", labeled(id));
}

fn tool_outputs(home: &TestHome, id: &str) -> String {
    home.store()
        .events(id)
        .iter()
        .filter(|e| e["type"] == "tool_end")
        .map(|e| e["output"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn bash(cmd: &str) -> Value {
    json!({"tool_calls": tc("Bash", json!({"command": cmd}))})
}

#[tokio::test]
async fn container_isolation_runs_commands_in_docker() {
    if !docker_works() {
        eprintln!("skipping: docker (with a local alpine:latest image) isn't available");
        return;
    }
    let home = TestHome::new("docker");
    // Whatever happens, nothing this test started outlives it.
    struct Sweep(PathBuf);
    impl Drop for Sweep {
        fn drop(&mut self) {
            container::sweep("docker", &self.0);
        }
    }
    let _sweep = Sweep(home.store().dir);
    let cfg = home.dir.join("home").join("config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str("\n[background]\nisolate = \"docker\"\nimage = \"alpine:latest\"\nnetwork = \"none\"\nmemory = \"256m\"\ncpus = 1\n");
    std::fs::write(&cfg, &text).unwrap();
    let repo = make_repo(&home);
    // Network none: the file lands in the worktree, outbound fails, and the
    // verified-mode check (`test -f hello.txt`) runs in the container too.
    let hook = repo.join(".git/hooks/post-checkout");
    let (base, _) = mock_server(vec![
        bash(&format!("echo hi > hello.txt && echo alpine $(cat /etc/alpine-release) && cat .git && (touch {} && echo HOOK-WRITTEN || echo HOOK-REFUSED)", hook.display())),
        bash("wget -q -T 3 -O /dev/null http://example.com && echo NET-OPEN || echo NET-BLOCKED"),
        // Killing a background shell stops its processes in the container,
        // not just the host's `docker exec` client.
        json!({"tool_calls": tc("Bash", json!({"command": "sleep 321", "run_in_background": true}))}),
        json!({"tool_calls": tc("KillShell", json!({"shell_id": "bash_1"}))}),
        bash("sleep 2; ps -o args | grep -c '^sleep 321' || true"),
        json!({"content": "Done."}),
    ])
    .await;
    let m = home.manager();
    let mut req = new_run(&repo, &base, "write hello in a container");
    req.verify = Some(true);
    let run = m.create(req).await.unwrap();
    let iso = run.settings.isolate.clone().expect("isolated");
    assert_eq!(
        iso.label(),
        "docker · alpine:latest · network none · 1 cpus · 256m memory"
    );
    assert!(
        run.to_json()["isolation"]
            .as_str()
            .unwrap()
            .contains("chanto-run-")
    );
    let done = wait_for(&m, &run.id, "finished", finished).await;
    assert_eq!(
        done.state,
        RunState::Done,
        "{:?} {}",
        done.error,
        tool_outputs(&home, &run.id)
    );
    let out = tool_outputs(&home, &run.id);
    assert!(
        out.contains("alpine 3."),
        "the command ran in the alpine container: {out}"
    );
    assert!(out.contains("gitdir: "), "{out}");
    assert!(
        out.contains("HOOK-REFUSED") && !hook.exists(),
        "the repository's git dir is read-only in the container: {out}"
    );
    assert!(
        out.contains("NET-BLOCKED") && !out.contains("NET-OPEN"),
        "{out}"
    );
    assert!(
        out.contains("Killed bash_1") && out.trim_end().ends_with("\n0"),
        "{out}"
    );
    assert_eq!(
        done.verify.as_ref().unwrap().status,
        "passed",
        "{:?}",
        done.verify
    );
    assert_eq!(
        sh(&repo, &["show", &format!("{}:hello.txt", done.branch)]),
        "hi"
    );
    wait_removed(&run.id).await;
    assert!(
        super::cli::detail_text(&m.detail(&run.id).await.unwrap())
            .contains("Isolation: container (docker · alpine:latest")
    );
    m.remove(&run.id).await.unwrap();

    // Allowlist: only allowed hosts, through Chanto's proxy via the relay.
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = upstream.local_addr().unwrap().port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut s, _)) = upstream.accept().await {
            let mut b = [0u8; 4096];
            let _ = s.read(&mut b).await;
            let _ = s
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\nUPSTREAM\n",
                )
                .await;
        }
    });
    let text = text.replace("network = \"none\"", "network = \"allowlist\"")
        + "\n[sandbox]\nallowed_domains = [\"127.0.0.1\"]\n";
    std::fs::write(&cfg, text).unwrap();
    let (base, _) = mock_server(vec![
        bash(&format!("wget -q -T 5 -O- http://127.0.0.1:{port}/ ; echo; wget -q -T 5 -O- http://example.com/ >/dev/null 2>&1 && echo DENIED-OPEN || echo DENIED-BLOCKED")),
        bash("unset http_proxy HTTP_PROXY; wget -q -T 3 -O /dev/null http://example.com/ && echo DIRECT-OPEN || echo DIRECT-BLOCKED"),
        json!({"content": "Done."}),
    ])
    .await;
    let run = m
        .create(new_run(&repo, &base, "fetch through the allowlist"))
        .await
        .unwrap();
    assert_eq!(
        run.settings.isolate.as_ref().unwrap().network,
        container::NetMode::Allowlist
    );
    // The blocked domain is offered for approval, like any sandboxed command's.
    let asked = wait_for(&m, &run.id, "needs-input", |r| {
        r.state == RunState::NeedsInput || finished(r)
    })
    .await;
    assert_eq!(
        asked.pending.as_ref().map(|p| p["label"].clone()),
        Some(json!("Allow network access to example.com?")),
        "{:?}",
        asked.error
    );
    m.respond(&run.id, json!({"decision": "deny"})).unwrap();
    let done = wait_for(&m, &run.id, "finished", finished).await;
    let out = tool_outputs(&home, &run.id);
    assert_eq!(done.state, RunState::Done, "{:?} {out}", done.error);
    assert!(
        out.contains("UPSTREAM"),
        "allowed host reachable through the proxy: {out}"
    );
    assert!(
        out.contains("DENIED-BLOCKED") && out.contains("example.com was blocked"),
        "{out}"
    );
    assert!(
        out.contains("DIRECT-BLOCKED"),
        "no way around the proxy: {out}"
    );
    wait_removed(&run.id).await;
    m.remove(&run.id).await.unwrap();

    // Daemon start sweeps containers a previous daemon left behind (this
    // home's only).
    let key = container::store_key(&home.store().dir);
    let orphan = Command::new("docker")
        .args([
            "run",
            "-d",
            "--label",
            &format!("chanto.store={key}"),
            "--label",
            "chanto.run=orphan01",
            "--entrypoint",
            "tail",
            "alpine:latest",
            "-f",
            "/dev/null",
        ])
        .output()
        .unwrap();
    assert!(orphan.status.success());
    assert!(!labeled("orphan01").is_empty());
    assert_eq!(container::sweep("docker", &home.dir.join("elsewhere")), 0);
    assert_eq!(container::sweep("docker", &home.store().dir), 1);
    assert!(labeled("orphan01").is_empty());
}
