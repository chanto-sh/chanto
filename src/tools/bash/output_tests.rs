use super::*;
use crate::agent::{Event, Frontend, PermissionReply, PermissionRequest};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Capture(Mutex<Vec<Event>>);

#[async_trait]
impl Frontend for Capture {
    fn emit(&self, ev: Event) {
        self.0.lock().unwrap().push(ev);
    }
    async fn permission(&self, _: PermissionRequest) -> PermissionReply {
        PermissionReply::Deny(None)
    }
    async fn question(&self, _: &str, _: &[String]) -> Option<String> {
        None
    }
}

fn setup() -> (Ctx, Arc<BgShell>, Arc<Capture>, std::path::PathBuf) {
    let dir = crate::agent::tests::tmpdir("background-output-status");
    let mut env = crate::agent::tests::make_env(
        "http://127.0.0.1:9",
        &dir,
        crate::permissions::Mode::Bypass,
        8192,
    );
    let capture = Arc::new(Capture::default());
    Arc::get_mut(&mut env).unwrap().frontend = capture.clone();
    let shell = Arc::new(BgShell {
        id: "bash_6".into(),
        command: "cd /some/long/path && cargo build --release".into(),
        description: Some("Build release binary".into()),
        monitor: None,
        output: Mutex::new(String::new()),
        read_pos: Mutex::new(0),
        watch_pos: Mutex::new(0),
        exit: Mutex::new(None),
        pid: None,
        network: Mutex::new(None),
    });
    env.shells
        .lock()
        .unwrap()
        .insert(shell.id.clone(), shell.clone());
    (
        Ctx {
            env,
            cancel: CancellationToken::new(),
            depth: 0,
            call_id: Some("poll-1".into()),
            cwd_override: None,
            reads: None,
        },
        shell,
        capture,
        dir,
    )
}

#[tokio::test]
async fn stopping_a_job_names_it_without_its_id() {
    let (ctx, _shell, _, _dir) = setup();
    assert_eq!(
        KillShell.label_in(&json!({"shell_id":"bash_6"}), &ctx.env),
        "KillShell(Build release binary)"
    );
    assert_eq!(
        KillShell.label_in(&json!({"shell_id":"missing"}), &ctx.env),
        "KillShell(missing)"
    );
}

#[tokio::test]
async fn output_poll_names_the_job_and_previews_only_new_lines() {
    let (ctx, shell, _, dir) = setup();
    assert_eq!(
        BashOutput.label_in(&json!({"bash_id":"bash_6"}), &ctx.env),
        "BashOutput(Build release binary)"
    );
    *shell.output.lock().unwrap() = "one\ntwo\nthree\nfour\n".into();
    let out = BashOutput
        .run(json!({"bash_id":"bash_6"}), &ctx)
        .await
        .unwrap();
    assert_eq!(
        out.display.as_deref(),
        Some("Still running · 4 new lines\ntwo\nthree\nfour")
    );
    assert!(out.content.contains("one\ntwo\nthree\nfour"));
    let out = BashOutput
        .run(json!({"bash_id":"bash_6"}), &ctx)
        .await
        .unwrap();
    assert_eq!(
        out.display.as_deref(),
        Some("Still running · no new output")
    );
    assert!(!out.is_error);
    *shell.output.lock().unwrap() = "x".repeat(100_000);
    *shell.read_pos.lock().unwrap() = 0;
    let out = BashOutput
        .run(json!({"bash_id":"bash_6"}), &ctx)
        .await
        .unwrap();
    assert!(
        out.display.as_ref().unwrap().len() < 200,
        "a giant line must not flood the preview"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn output_status_distinguishes_success_failure_and_filtered_silence() {
    let (ctx, shell, capture, dir) = setup();
    *shell.output.lock().unwrap() = "noise\n".into();
    let out = BashOutput
        .run(json!({"bash_id":"bash_6","filter":"ERROR"}), &ctx)
        .await
        .unwrap();
    assert_eq!(
        out.display.as_deref(),
        Some("Still running · no new matching output")
    );
    for (code, expected, error) in [
        (0, "Completed · exit 0 · no new output", false),
        (7, "Failed · exit 7 · no new output", true),
        (-1, "Stopped · no new output", true),
    ] {
        *shell.exit.lock().unwrap() = Some(code);
        let out = BashOutput
            .run(json!({"bash_id":"bash_6","wait_secs":5}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.display.as_deref(), Some(expected));
        assert_eq!(out.is_error, error);
    }
    assert!(capture.0.lock().unwrap().iter().any(
        |e| matches!(e,Event::ToolProgress{text,..} if text=="Waiting for output to settle (up to 5s)")
    ));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn output_labels_fall_back_when_a_shell_is_unknown() {
    let (ctx, _, _, dir) = setup();
    assert_eq!(
        BashOutput.label_in(&json!({"bash_id":"missing"}), &ctx.env),
        "BashOutput(missing)"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn waiting_poll_reports_what_it_is_waiting_for_then_shows_arriving_output() {
    let (ctx, shell, capture, dir) = setup();
    let poll = BashOutput.run(json!({"bash_id":"bash_6","wait_secs":5}), &ctx);
    tokio::pin!(poll);
    tokio::select! {
        result = &mut poll => panic!("a quiet running command should wait: {result:?}"),
        _ = tokio::time::sleep(Duration::from_millis(10)) => {}
    }
    assert!(capture.0.lock().unwrap().iter().any(
        |e| matches!(e,Event::ToolProgress{text,..} if text=="Waiting for output to settle (up to 5s)")
    ));
    *shell.output.lock().unwrap() = "Compiling chanto\n".into();
    let out = tokio::time::timeout(Duration::from_secs(2), poll)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        out.display.as_deref(),
        Some("Still running · 1 new line\nCompiling chanto")
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_wait_collects_output_until_it_stops_arriving() {
    // A test run prints a dot per test: the wait must not end on the first one.
    let (ctx, shell, _, dir) = setup();
    let writer = shell.clone();
    let feed = tokio::spawn(async move {
        for _ in 0..10 {
            writer.output.lock().unwrap().push('.');
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    let started = std::time::Instant::now();
    let out = tokio::time::timeout(
        Duration::from_secs(4),
        BashOutput.run(json!({"bash_id":"bash_6","wait_secs":5}), &ctx),
    )
    .await
    .unwrap()
    .unwrap();
    feed.await.unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "{:?}",
        started.elapsed()
    );
    assert!(out.content.contains(".........."), "{}", out.content);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn completion_waits_for_both_output_readers() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let output = Arc::new(Mutex::new(String::new()));
    let buffered = output.clone();
    let stdout = tokio::spawn(async {});
    let stderr = tokio::spawn(async move {
        rx.await.unwrap();
        buffered.lock().unwrap().push_str("final failure line");
    });
    let drain = tokio::spawn(drain_output(stdout, stderr));
    tokio::task::yield_now().await;
    assert!(!drain.is_finished());
    tx.send(()).unwrap();
    drain.await.unwrap();
    assert_eq!(*output.lock().unwrap(), "final failure line");
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_a_running_command_stops_it() {
    let (ctx, _, _, dir) = setup();
    let pidfile = dir.join("child.pid");
    let args = serde_json::json!({
        "command": format!("tail -f /dev/null & echo $! > '{}'; wait", pidfile.display())
    });
    let run = Bash.run(args, &ctx);
    let started = async {
        while std::fs::read_to_string(&pidfile).map_or(true, |s| s.trim().is_empty()) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    // Like an interrupted turn: stop waiting for the tool once the command runs.
    tokio::select! {
        _ = run => panic!("the command finished on its own"),
        _ = started => {}
    }
    let pid = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .to_string();
    let gone = || {
        !std::process::Command::new("kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    };
    for _ in 0..100 {
        if gone() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        gone(),
        "the command's child {pid} outlived the dropped tool call"
    );
}
