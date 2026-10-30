//! IDE bridge client (protocol: ide/PROTOCOL.md). An editor extension runs a
//! localhost WebSocket server and advertises it with a lockfile in
//! ~/.chanto/ide/<port>.lock. Chanto connects to the one whose workspace
//! contains the working directory, then:
//! - adds the editor selection to each prompt as context,
//! - shows proposed edits as diffs in the editor (accepting there approves the
//!   edit, possibly with the user's changes; rejecting stops the turn),
//! - reads the editor's diagnostics after edits when no language server ran.

use crate::agent::Env;
use crate::util::Canonical as _;
use anyhow::{Context, Result, anyhow, bail};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

pub const AUTH_HEADER: &str = "x-chanto-ide-authorization";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockInfo {
    #[serde(skip)]
    pub port: u16,
    pub pid: u32,
    #[serde(default)]
    pub workspace_folders: Vec<PathBuf>,
    #[serde(default)]
    pub ide_name: String,
    pub auth_token: String,
    #[serde(default)]
    pub transport: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Selection {
    pub file: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub start_line: u64,
    #[serde(default)]
    pub end_line: u64,
    #[serde(default)]
    pub is_empty: bool,
}

pub fn lock_dir() -> PathBuf {
    crate::config::home_dir().join("ide")
}

fn pid_alive(pid: u32) -> bool {
    crate::resources::pid_alive(pid)
}

/// Running IDEs whose workspace contains `cwd`, best match first. The editor
/// that launched this terminal (CHANTO_IDE_PORT) always comes first.
pub fn discover(dir: &Path, cwd: &Path) -> Vec<LockInfo> {
    let want_port: Option<u16> = std::env::var("CHANTO_IDE_PORT")
        .ok()
        .and_then(|p| p.parse().ok());
    let cwd = cwd.canonical().unwrap_or_else(|_| cwd.to_path_buf());
    let mut found: Vec<(usize, LockInfo)> = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("lock") {
            continue;
        }
        let Some(port) = p
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u16>().ok())
        else {
            continue;
        };
        let Some(mut lock) = std::fs::read(&p)
            .ok()
            .and_then(|b| serde_json::from_slice::<LockInfo>(&b).ok())
        else {
            continue;
        };
        lock.port = port;
        if !pid_alive(lock.pid) {
            // The editor exited without cleaning up.
            let _ = std::fs::remove_file(&p);
            continue;
        }
        let depth = lock
            .workspace_folders
            .iter()
            .filter_map(|f| {
                let f = f.canonical().unwrap_or_else(|_| f.clone());
                cwd.starts_with(&f).then(|| f.components().count())
            })
            .max();
        let score = match (want_port == Some(port), depth) {
            (true, _) => usize::MAX,
            (false, Some(d)) => d,
            (false, None) => continue,
        };
        found.push((score, lock));
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.into_iter().map(|(_, l)| l).collect()
}

pub struct Ide {
    pub name: String,
    pub port: u16,
    tx: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    selection: Arc<Mutex<Option<Selection>>>,
    /// Last selection added to a prompt (so an unchanged one isn't repeated).
    last_sent: Mutex<Option<Selection>>,
}

impl Ide {
    pub async fn connect(lock: &LockInfo) -> Result<Arc<Ide>> {
        if !lock.transport.is_empty() && lock.transport != "ws" {
            bail!("unsupported IDE transport {}", lock.transport);
        }
        let mut req = format!("ws://127.0.0.1:{}", lock.port).into_client_request()?;
        req.headers_mut().insert(
            AUTH_HEADER,
            lock.auth_token.parse().context("bad auth token")?,
        );
        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(3),
            tokio_tungstenite::connect_async(req),
        )
        .await
        .map_err(|_| anyhow!("timed out connecting to the IDE"))?
        .context("connecting to the IDE")?;
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> = Default::default();
        let alive = Arc::new(AtomicBool::new(true));
        let selection: Arc<Mutex<Option<Selection>>> = Default::default();
        tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if sink.send(WsMsg::text(m)).await.is_err() {
                    break;
                }
            }
            let _ = sink.send(WsMsg::Close(None)).await;
        });
        let (p2, a2, s2) = (pending.clone(), alive.clone(), selection.clone());
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let text = match msg {
                    WsMsg::Text(t) => t.to_string(),
                    WsMsg::Close(_) => break,
                    _ => continue,
                };
                let Ok(v) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if let Some(id) = v["id"].as_u64() {
                    if let Some(s) = p2.lock().unwrap().remove(&id) {
                        let _ = s.send(v);
                    }
                } else if v["method"] == "selectionChanged" {
                    *s2.lock().unwrap() = serde_json::from_value(v["params"].clone()).ok();
                }
            }
            a2.store(false, Ordering::Relaxed);
            // Fail outstanding requests (e.g. an open diff) instead of hanging.
            p2.lock().unwrap().clear();
        });
        let name = if lock.ide_name.is_empty() {
            "IDE".to_string()
        } else {
            lock.ide_name.clone()
        };
        Ok(Arc::new(Ide {
            name,
            port: lock.port,
            tx,
            pending,
            next_id: AtomicU64::new(1),
            alive,
            selection,
            last_sent: Mutex::new(None),
        }))
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn send(&self, method: &str, params: Value) -> Result<(u64, oneshot::Receiver<Value>)> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.tx
            .send(
                json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
            )
            .map_err(|_| anyhow!("IDE disconnected"))?;
        Ok((id, rx))
    }

    async fn wait(
        &self,
        id: u64,
        rx: oneshot::Receiver<Value>,
        timeout: Option<Duration>,
    ) -> Result<Value> {
        let v = match timeout {
            Some(t) => match tokio::time::timeout(t, rx).await {
                Ok(r) => r,
                Err(_) => {
                    self.pending.lock().unwrap().remove(&id);
                    bail!("IDE did not answer in time");
                }
            },
            None => rx.await,
        }
        .map_err(|_| anyhow!("IDE disconnected"))?;
        if let Some(e) = v.get("error") {
            bail!("IDE error: {}", e["message"].as_str().unwrap_or("unknown"));
        }
        Ok(v["result"].clone())
    }

    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let (id, rx) = self.send(method, params)?;
        self.wait(id, rx, Some(timeout)).await
    }

    /// Fresh selection (falls back to the last pushed one).
    pub async fn selection(&self) -> Option<Selection> {
        if let Ok(v) = self
            .request("getSelection", json!({}), Duration::from_millis(400))
            .await
        {
            let s: Option<Selection> = serde_json::from_value(v).ok();
            *self.selection.lock().unwrap() = s.clone();
            return s;
        }
        self.selection.lock().unwrap().clone()
    }

    /// Show `contents` as a proposed change to `path` and wait for the user.
    pub async fn open_diff(&self, path: &Path, contents: &str) -> Result<DiffOutcome> {
        let title = format!(
            "{}: current ↔ proposed (chanto)",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
        );
        let (id, rx) = self.send(
            "openDiff",
            json!({"path": path, "newContents": contents, "title": title}),
        )?;
        let v = self.wait(id, rx, None).await?;
        Ok(DiffOutcome {
            accepted: v["accepted"].as_bool().unwrap_or(false),
            contents: v["contents"].as_str().map(str::to_string),
        })
    }

    /// Close the open diff for `path` (the change was answered in the
    /// terminal). Fire and forget.
    pub fn close_diffs(&self, path: &Path) {
        if let Ok((id, _rx)) = self.send("closeDiffs", json!({"path": path})) {
            self.pending.lock().unwrap().remove(&id);
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct DiffOutcome {
    pub accepted: bool,
    pub contents: Option<String>,
}

/// Prompt context for the editor selection, if it changed since the last prompt.
pub async fn turn_context(env: &Env) -> Option<String> {
    let ide = env.ide()?;
    let sel = ide.selection().await?;
    let mut last = ide.last_sent.lock().unwrap();
    if last.as_ref() == Some(&sel) {
        return None;
    }
    *last = Some(sel.clone());
    let path = crate::util::rel(&env.cwd(), Path::new(&sel.file));
    if sel.is_empty || sel.text.trim().is_empty() {
        return Some(format!(
            "The user opened the file {path} in the IDE. This may or may not be related to the current task."
        ));
    }
    Some(format!(
        "The user selected lines {} to {} from {path} in the IDE:\n{}\n\nThis may or may not be related to the current task.",
        sel.start_line,
        sel.end_line,
        crate::util::truncate(&sel.text, 4000)
    ))
}

/// The file an edit call changes and its proposed new contents, for showing
/// a diff in the editor. None for edits that can't be previewed (multi-file
/// patches, deletes) or that would fail anyway.
pub fn proposed_change(cwd: &Path, tool: &str, args: &Value) -> Option<(PathBuf, String)> {
    match tool {
        "Write" => Some((
            crate::util::resolve(cwd, args["file_path"].as_str()?),
            args["content"].as_str()?.to_string(),
        )),
        "Edit" | "MultiEdit" => {
            let path = crate::util::resolve(cwd, args["file_path"].as_str()?);
            let mut text = std::fs::read_to_string(&path).ok()?;
            let edits: Vec<Value> = if tool == "Edit" {
                vec![args.clone()]
            } else {
                args["edits"].as_array()?.clone()
            };
            for e in edits {
                let all = e["replace_all"].as_bool() == Some(true);
                text = chanto_core::edit::apply_edit(
                    &text,
                    e["old_string"].as_str()?,
                    e["new_string"].as_str()?,
                    all,
                )
                .ok()?
                .0;
            }
            Some((path, text))
        }
        "apply_patch" => {
            use crate::tools::patch::{Op, apply_chunks, parse};
            let ops = parse(args["input"].as_str()?).ok()?;
            match ops.as_slice() {
                [Op::Add { path, content }] => {
                    Some((crate::util::resolve(cwd, path), content.clone()))
                }
                [
                    Op::Update {
                        path,
                        move_to: None,
                        chunks,
                    },
                ] => {
                    let p = crate::util::resolve(cwd, path);
                    let old = std::fs::read_to_string(&p).ok()?;
                    Some((p, apply_chunks(&old, chunks, path).ok()?))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// After an edit, add the editor's errors for the file when no language
/// server reported (the editor may have servers Chanto doesn't).
pub async fn after_tool(env: &Env, tool: &str, args: &Value, out: &mut crate::tools::ToolOutput) {
    if out.is_error
        || out.content.contains("<diagnostics")
        || !matches!(tool, "Write" | "Edit" | "MultiEdit" | "apply_patch")
    {
        return;
    }
    let Some(ide) = env.ide() else { return };
    let rel_path = match args["file_path"].as_str() {
        Some(p) => p.to_string(),
        None => match crate::tools::patch::paths(args["input"].as_str().unwrap_or("")).last() {
            Some(p) => p.clone(),
            None => return,
        },
    };
    let path = crate::util::resolve(&env.cwd(), &rel_path);
    // Give the editor's language services a moment to re-check the file.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let Ok(v) = ide
        .request(
            "getDiagnostics",
            json!({"path": path}),
            Duration::from_secs(2),
        )
        .await
    else {
        return;
    };
    if let Some(text) = format_diagnostics(&v) {
        out.content.push_str(&format!(
            "\n\n<diagnostics file=\"{}\" source=\"{}\">\n{text}\n</diagnostics>",
            crate::util::rel(&env.cwd(), &path),
            ide.name
        ));
    }
}

/// Errors only, like the language-server path.
fn format_diagnostics(v: &Value) -> Option<String> {
    let mut lines = Vec::new();
    for f in v.as_array().into_iter().flatten() {
        for d in f["diagnostics"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|d| d["severity"] == "error")
        {
            let src = d["source"]
                .as_str()
                .map(|s| format!(" [{s}]"))
                .unwrap_or_default();
            lines.push(format!(
                "L{}:{} error: {}{src}",
                d["line"],
                d["character"],
                d["message"].as_str().unwrap_or("")
            ));
        }
    }
    lines.truncate(20);
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// Connect to the best matching running IDE, if any. Returns a status line.
pub async fn auto_connect(env: &Env) -> Option<String> {
    for lock in discover(&lock_dir(), &env.cwd()) {
        if let Ok(ide) = Ide::connect(&lock).await {
            let msg = format!(
                "Connected to {} (selection and diffs are shared; /ide for details)",
                ide.name
            );
            env.set_ide(Some(ide));
            return Some(msg);
        }
    }
    None
}

/// `/ide`, `/ide connect [port]`, `/ide disconnect`.
pub async fn command(env: &Env, arg: &str) -> String {
    let mut words = arg.split_whitespace();
    match words.next().unwrap_or("") {
        "disconnect" => {
            env.set_ide(None);
            "Disconnected from the IDE.".into()
        }
        "connect" => {
            let port: Option<u16> = words.next().and_then(|p| p.parse().ok());
            let all = discover(&lock_dir(), &env.cwd());
            let candidates: Vec<&LockInfo> = all
                .iter()
                .filter(|l| port.is_none_or(|p| l.port == p))
                .collect();
            if candidates.is_empty() {
                return "No running IDE found for this folder. Install the VS Code extension (see README, \"IDE integration\") and open this folder in VS Code.".into();
            }
            let mut errs = Vec::new();
            for lock in candidates {
                match Ide::connect(lock).await {
                    Ok(ide) => {
                        let name = ide.name.clone();
                        env.set_ide(Some(ide));
                        return format!("Connected to {name} on port {}.", lock.port);
                    }
                    Err(e) => errs.push(format!("port {}: {e:#}", lock.port)),
                }
            }
            format!("Could not connect: {}", errs.join("; "))
        }
        _ => {
            let mut s = String::from("## IDE\n");
            match env.ide() {
                Some(ide) => s.push_str(&format!(
                    "- connected to **{}** on port {}\n",
                    ide.name, ide.port
                )),
                None => s.push_str("- not connected\n"),
            }
            let all = discover(&lock_dir(), &env.cwd());
            if !all.is_empty() {
                s.push_str("\nAvailable for this folder:\n");
                for l in &all {
                    s.push_str(&format!(
                        "- {} (port {}, {})\n",
                        l.ide_name,
                        l.port,
                        l.workspace_folders
                            .iter()
                            .map(|f| f.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            s.push_str("\n`/ide connect [port]` · `/ide disconnect`");
            s
        }
    }
}

#[cfg(test)]
mod tests {
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
}
