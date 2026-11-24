use super::*;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "chanto-ide-{name}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d.canonical().unwrap()
}

#[test]
fn discovery_matches_workspace_and_skips_dead() {
    let dir = tmp("locks");
    let ws = tmp("ws");
    std::fs::create_dir_all(ws.join("sub")).unwrap();
    let me = std::process::id();
    let lock = |pid: u32, folders: &[&Path]| {
        json!({"pid": pid, "workspaceFolders": folders, "ideName": "VS Code", "authToken": "t", "transport": "ws"}).to_string()
    };
    std::fs::write(dir.join("1111.lock"), lock(me, &[ws.as_path()])).unwrap();
    std::fs::write(dir.join("2222.lock"), lock(me, &[Path::new("/elsewhere")])).unwrap();
    std::fs::write(dir.join("3333.lock"), lock(999_999_999, &[ws.as_path()])).unwrap();
    std::fs::write(dir.join("4444.lock"), lock(me, &[ws.join("sub").as_path()])).unwrap();
    std::fs::write(dir.join("junk.lock"), "{").unwrap();
    let found = discover(&dir, &ws.join("sub"));
    let ports: Vec<u16> = found.iter().map(|l| l.port).collect();
    assert_eq!(
        ports,
        vec![4444, 1111],
        "deepest workspace first; other folders and dead pids skipped"
    );
    assert!(!dir.join("3333.lock").exists(), "stale lockfile removed");
}

#[test]
fn diagnostics_format_errors_only() {
    let v = json!([{"file": "/a.ts", "diagnostics": [
        {"line": 3, "character": 7, "severity": "error", "message": "bad", "source": "ts"},
        {"line": 4, "character": 1, "severity": "warning", "message": "meh"}
    ]}]);
    assert_eq!(format_diagnostics(&v).unwrap(), "L3:7 error: bad [ts]");
    assert!(format_diagnostics(&json!([])).is_none());
}

#[test]
fn proposed_change_for_edits() {
    let d = tmp("prop");
    std::fs::write(d.join("a.txt"), "one\ntwo\n").unwrap();
    let (p, t) = proposed_change(
        &d,
        "Edit",
        &json!({"file_path": "a.txt", "old_string": "two", "new_string": "2"}),
    )
    .unwrap();
    assert_eq!((p, t.as_str()), (d.join("a.txt"), "one\n2\n"));
    let (_, t) = proposed_change(&d, "MultiEdit", &json!({"file_path": "a.txt", "edits": [{"old_string": "one", "new_string": "1"}, {"old_string": "two", "new_string": "2"}]})).unwrap();
    assert_eq!(t, "1\n2\n");
    let patch = "*** Begin Patch\n*** Update File: a.txt\n one\n-two\n+zwei\n*** End Patch";
    assert_eq!(
        proposed_change(&d, "apply_patch", &json!({"input": patch}))
            .unwrap()
            .1,
        "one\nzwei\n"
    );
    assert!(
        proposed_change(
            &d,
            "Edit",
            &json!({"file_path": "a.txt", "old_string": "nope", "new_string": "x"})
        )
        .is_none()
    );
}

type MockIde = (
    tokio::process::Child,
    tokio::io::BufReader<tokio::process::ChildStdout>,
    PathBuf,
    PathBuf,
);

/// The extension's real server code (ide/vscode) driven by
/// test/mock-ide.js. None (test skipped) if node or the build is missing.
async fn spawn_mock_ide(mode: &str) -> Option<MockIde> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("ide/vscode");
    let node = std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !root.join("out/server.js").exists() || !root.join("node_modules/ws").exists() || !node {
        eprintln!("skipping: run `npm install && npm run compile` in ide/vscode");
        return None;
    }
    let locks = tmp("e2e-locks");
    let ws = tmp("e2e-ws");
    let mut child = tokio::process::Command::new("node")
        .arg(root.join("test/mock-ide.js"))
        .arg(&locks)
        .arg(&ws)
        .arg(mode)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut out = tokio::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    tokio::io::AsyncBufReadExt::read_line(&mut out, &mut line)
        .await
        .unwrap();
    assert!(line.starts_with("READY"), "{line}");
    Some((child, out, locks, ws))
}

#[tokio::test]
async fn talks_to_extension_server() {
    for mode in ["accept", "edit", "reject", "hang"] {
        let Some((mut child, mut out, locks, ws)) = spawn_mock_ide(mode).await else {
            return;
        };
        let lock = discover(&locks, &ws)
            .into_iter()
            .next()
            .expect("lockfile found");
        let ide = Ide::connect(&lock).await.unwrap();
        assert_eq!(ide.name, "Mock IDE");
        // A wrong token is refused.
        let bad = LockInfo {
            auth_token: "wrong".into(),
            ..lock.clone()
        };
        assert!(Ide::connect(&bad).await.is_err());
        // Pushed selection notification arrives, then a fresh request wins.
        // The push arrives asynchronously; give a busy machine time.
        for _ in 0..60 {
            if ide.selection.lock().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            ide.selection
                .lock()
                .unwrap()
                .as_ref()
                .map(|s| s.text.clone()),
            Some("pushed".into())
        );
        let sel = ide.selection().await.unwrap();
        assert_eq!((sel.start_line, sel.text.as_str()), (3, "fn selected() {}"));
        let diags = ide
            .request(
                "getDiagnostics",
                json!({"path": "/x.rs"}),
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        assert_eq!(
            format_diagnostics(&diags).unwrap(),
            "L2:5 error: mock type error [mockc]"
        );
        let file = ws.join("f.rs");
        match mode {
            "accept" => assert_eq!(
                ide.open_diff(&file, "new\n").await.unwrap(),
                DiffOutcome {
                    accepted: true,
                    contents: Some("new\n".into())
                }
            ),
            "edit" => assert_eq!(
                ide.open_diff(&file, "new\n")
                    .await
                    .unwrap()
                    .contents
                    .as_deref(),
                Some("new\n// edited in IDE\n")
            ),
            "reject" => assert!(!ide.open_diff(&file, "new\n").await.unwrap().accepted),
            _ => {
                // The terminal answers first: closeDiffs settles the open diff.
                let pending = ide.open_diff(&file, "new\n");
                tokio::pin!(pending);
                assert!(
                    tokio::time::timeout(Duration::from_millis(300), &mut pending)
                        .await
                        .is_err(),
                    "diff waits for the user"
                );
                ide.close_diffs(&file);
                let mut l = String::new();
                tokio::io::AsyncBufReadExt::read_line(&mut out, &mut l)
                    .await
                    .unwrap();
                assert_eq!(l.trim(), "CLOSED");
                assert!(!pending.await.unwrap().accepted);
            }
        }
        drop(child.stdin.take());
        let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
        // The server removes its lockfile on shutdown.
        assert!(discover(&locks, &ws).is_empty());
    }
}

/// The agent loop: an Edit needing approval is shown in the IDE; the
/// terminal prompt never answers, so the IDE decides.
#[tokio::test]
async fn permission_race_resolved_in_ide() {
    use crate::agent::tests::{mock_server, tc};
    use crate::agent::{
        Agent, EnvInit, Event, Frontend, PermissionReply, PermissionRequest, TurnEnd,
    };
    struct WaitingTerminal(Arc<AtomicBool>);
    #[async_trait::async_trait]
    impl Frontend for WaitingTerminal {
        fn emit(&self, _: Event) {}
        async fn permission(&self, _: PermissionRequest) -> PermissionReply {
            self.0.store(true, Ordering::Relaxed);
            std::future::pending().await
        }
        async fn question(&self, _: &str, _: &[String]) -> Option<String> {
            None
        }
    }
    for mode in ["edit", "reject"] {
        let Some((_child, _out, locks, ws)) = spawn_mock_ide(mode).await else {
            return;
        };
        std::fs::write(ws.join("a.txt"), "hello world\n").unwrap();
        let (base, seen) = mock_server(vec![
            json!({"tool_calls": tc("Read", json!({"file_path": "a.txt"}))}),
            json!({"tool_calls": tc("Edit", json!({"file_path": "a.txt", "old_string": "world", "new_string": "there"}))}),
            json!({"content": "done"}),
        ])
        .await;
        let asked = Arc::new(AtomicBool::new(false));
        let mut config = crate::config::Config::default();
        config.provider = crate::llm::ProviderKind::Openai;
        let env = crate::agent::Env::new(EnvInit {
            config,
            provider: crate::llm::Provider::new(
                crate::llm::ProviderKind::Openai,
                &base,
                None,
                32768,
            ),
            model: "mock".into(),
            mode: crate::permissions::Mode::Default,
            vision: false,
            cwd: ws.clone(),
            tools: crate::tools::Registry::builtin(),
            mcp: vec![],
            frontend: Arc::new(WaitingTerminal(asked.clone())),
            interactive: true,
        });
        env.set_cwd(ws.clone());
        let lock = discover(&locks, &ws).into_iter().next().unwrap();
        env.set_ide(Some(Ide::connect(&lock).await.unwrap()));
        let mut agent = Agent::new(env);
        let end = tokio::time::timeout(
            Duration::from_secs(20),
            agent.run_turn(
                "edit it",
                vec![],
                tokio_util::sync::CancellationToken::new(),
            ),
        )
        .await
        .expect("turn finished")
        .unwrap();
        assert!(
            asked.load(Ordering::Relaxed),
            "terminal prompt was shown too"
        );
        let text = std::fs::read_to_string(ws.join("a.txt")).unwrap();
        let first = seen.lock().unwrap()[0]["messages"].to_string();
        assert!(
            first.contains("selected lines 3 to 3"),
            "selection context in the prompt"
        );
        if mode == "edit" {
            assert!(matches!(end, TurnEnd::Done(t) if t == "done"));
            assert_eq!(
                text, "hello there\n// edited in IDE\n",
                "the user's edit in the IDE was applied"
            );
        } else {
            assert!(
                matches!(end, TurnEnd::Interrupted),
                "rejecting in the IDE stops the turn"
            );
            assert_eq!(text, "hello world\n");
        }
    }
}
