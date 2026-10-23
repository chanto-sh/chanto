//! Minimal Language Server Protocol client. Servers are spawned lazily, one
//! per (language server, project root), and used for two things: reporting
//! errors back to the model right after it edits a file, and the `Lsp` tool
//! (definition, references, hover, symbols).

use crate::agent::Env;
use crate::tools::ToolOutput;
use crate::util::Canonical as _;
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{OnceCell, oneshot, watch};
use tokio::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LspConfig {
    /// Set to false to turn off language servers entirely.
    pub enabled: Option<bool>,
    /// How long to wait for diagnostics after an edit (default 2500 ms; 8000 for rust-analyzer).
    pub diagnostics_wait_ms: Option<u64>,
    /// Extra or overriding servers, keyed by name (a built-in name replaces it).
    pub servers: BTreeMap<String, LspServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LspServerConfig {
    pub command: String,
    pub args: Vec<String>,
    /// File extensions without the dot, e.g. ["ts", "tsx"].
    pub extensions: Vec<String>,
    pub disabled: bool,
}

impl LspConfig {
    pub fn merge(&mut self, o: LspConfig) {
        if o.enabled.is_some() {
            self.enabled = o.enabled;
        }
        if o.diagnostics_wait_ms.is_some() {
            self.diagnostics_wait_ms = o.diagnostics_wait_ms;
        }
        self.servers.extend(o.servers);
    }
    fn on(&self) -> bool {
        self.enabled != Some(false)
    }
}

#[derive(Debug, Clone)]
struct Spec {
    name: String,
    command: String,
    args: Vec<String>,
    extensions: Vec<String>,
    /// Files marking the project root for this language.
    markers: Vec<&'static str>,
    wait_ms: u64,
    /// After the first publish, keep listening this long for follow-ups (e.g. cargo check).
    settle_ms: u64,
    /// The server never executes project code, so it may run in untrusted folders.
    /// (rust-analyzer runs build scripts and proc macros, typescript-language-server
    /// loads the project's own tsserver, gopls may fetch toolchains, pylsp loads linter plugins.)
    safe_untrusted: bool,
}

fn builtin() -> Vec<Spec> {
    let s = |name: &str, cmd: &str, args: &[&str], exts: &[&str], markers: &[&'static str]| Spec {
        name: name.into(),
        command: cmd.into(),
        args: args.iter().map(|a| a.to_string()).collect(),
        extensions: exts.iter().map(|e| e.to_string()).collect(),
        markers: markers.to_vec(),
        wait_ms: 2500,
        settle_ms: 300,
        safe_untrusted: false,
    };
    let mut ra = s(
        "rust-analyzer",
        "rust-analyzer",
        &[],
        &["rs"],
        &["Cargo.toml"],
    );
    ra.wait_ms = 8000;
    ra.settle_ms = 400;
    let ts_markers: &[&'static str] = &["tsconfig.json", "jsconfig.json", "package.json"];
    let py_markers: &[&'static str] = &[
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "pyrightconfig.json",
        "requirements.txt",
    ];
    let py_exts = &["py", "pyi"];
    let safe = |mut x: Spec| {
        x.safe_untrusted = true;
        x
    };
    vec![
        ra,
        s(
            "typescript-language-server",
            "typescript-language-server",
            &["--stdio"],
            &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"],
            ts_markers,
        ),
        safe(s(
            "pyright",
            "pyright-langserver",
            &["--stdio"],
            py_exts,
            py_markers,
        )),
        safe(s(
            "basedpyright",
            "basedpyright-langserver",
            &["--stdio"],
            py_exts,
            py_markers,
        )),
        s("pylsp", "pylsp", &[], py_exts, py_markers),
        s("gopls", "gopls", &[], &["go"], &["go.mod", "go.work"]),
        safe(s(
            "clangd",
            "clangd",
            &["--background-index=false"],
            &["c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx"],
            &[
                "compile_commands.json",
                "compile_flags.txt",
                ".clangd",
                "CMakeLists.txt",
                "Makefile",
            ],
        )),
    ]
}

fn specs(cfg: &LspConfig) -> Vec<Spec> {
    let mut out: Vec<Spec> = Vec::new();
    // Configured servers take precedence over built-ins.
    for (name, c) in &cfg.servers {
        if c.disabled || c.command.is_empty() {
            continue;
        }
        let base = builtin().into_iter().find(|b| &b.name == name);
        out.push(Spec {
            name: name.clone(),
            command: c.command.clone(),
            args: c.args.clone(),
            extensions: if c.extensions.is_empty() {
                base.as_ref()
                    .map(|b| b.extensions.clone())
                    .unwrap_or_default()
            } else {
                c.extensions
                    .iter()
                    .map(|e| e.trim_start_matches('.').to_string())
                    .collect()
            },
            markers: base.as_ref().map(|b| b.markers.clone()).unwrap_or_default(),
            wait_ms: base.as_ref().map(|b| b.wait_ms).unwrap_or(2500),
            settle_ms: base.as_ref().map(|b| b.settle_ms).unwrap_or(300),
            // Same binary as a safe built-in (e.g. a plugin's clangd) stays safe.
            // Extra flags (e.g. clangd --query-driver) could make it run project-chosen binaries.
            safe_untrusted: base.as_ref().is_some_and(|b| {
                b.safe_untrusted
                    && b.command == c.command
                    && (c.args.is_empty() || c.args == b.args)
            }),
        });
    }
    for b in builtin() {
        if !cfg.servers.contains_key(&b.name) {
            out.push(b);
        }
    }
    out
}

/// Is a rustup-proxied tool (e.g. rust-analyzer) installed in some toolchain?
/// Looks at the toolchain folders instead of starting `rustup which`.
fn rustup_component_installed(cmd: &str) -> bool {
    let home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".rustup")));
    let Some(Ok(rd)) = home.map(|h| std::fs::read_dir(h.join("toolchains"))) else {
        return std::process::Command::new("rustup")
            .args(["which", cmd])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    };
    rd.flatten()
        .any(|t| t.path().join("bin").join(cmd).is_file())
}

fn which(cmd: &str) -> Option<PathBuf> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<PathBuf>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(v) = cache.lock().unwrap().get(cmd) {
        return v.clone();
    }
    let found = if cmd.contains('/') || cmd.contains('\\') {
        Some(PathBuf::from(cmd)).filter(|p| p.is_file())
    } else {
        crate::shell::find_on_path(cmd)
    };
    // rustup installs proxies for components that may not be installed.
    let found = found.filter(|p| {
        let is_proxy = p
            .canonical()
            .ok()
            .and_then(|c| c.file_stem().map(|s| s == "rustup"))
            .unwrap_or(false);
        !is_proxy || rustup_component_installed(cmd)
    });
    cache.lock().unwrap().insert(cmd.to_string(), found.clone());
    found
}

fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// The first installed server handling this file's extension.
fn spec_for(cfg: &LspConfig, path: &Path) -> Option<Spec> {
    if !cfg.on() {
        return None;
    }
    let ext = ext_of(path);
    if ext.is_empty() {
        return None;
    }
    specs(cfg)
        .into_iter()
        .find(|s| s.extensions.iter().any(|e| e == &ext) && which(&s.command).is_some())
}

fn language_id(path: &Path) -> &'static str {
    match ext_of(path).as_str() {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" | "pyi" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        _ => "plaintext",
    }
}

fn project_root(env: &Env) -> PathBuf {
    crate::permissions::project_root().unwrap_or_else(|| env.cwd())
}

/// Nearest ancestor (not above the project root) containing a marker file.
fn root_for(spec: &Spec, file: &Path, project: &Path) -> PathBuf {
    // Cargo workspaces: one rust-analyzer for the outermost Cargo.toml, not one per member crate.
    let outermost = spec.markers.contains(&"Cargo.toml");
    let mut found: Option<PathBuf> = None;
    let mut d = file.parent();
    while let Some(dir) = d {
        if spec.markers.iter().any(|m| dir.join(m).exists()) {
            if !outermost {
                return dir.to_path_buf();
            }
            found = Some(dir.to_path_buf());
        }
        if dir == project || !dir.starts_with(project) {
            break;
        }
        d = dir.parent();
    }
    if let Some(f) = found {
        return f;
    }
    if file.starts_with(project) {
        project.to_path_buf()
    } else {
        file.parent().unwrap_or(project).to_path_buf()
    }
}

static TRUSTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Language servers execute project code (build scripts, proc macros, the
/// workspace's own tsserver), so they only run in trusted folders.
pub fn set_trusted(on: bool) {
    TRUSTED.store(on, Ordering::Relaxed);
}

const MAX_SERVERS: usize = 4;

/// Only files inside the project (or `--add-dir` directories) get a server.
fn eligible(env: &Env, path: &Path, spec: &Spec) -> bool {
    if !TRUSTED.load(Ordering::Relaxed) && !spec.safe_untrusted {
        return false;
    }
    let p = crate::permissions::normalize(path);
    p.starts_with(crate::permissions::normalize(&project_root(env)))
        || crate::permissions::extra_dirs()
            .iter()
            .any(|d| p.starts_with(d))
}

/// Is any language server usable for this project? Cheap: PATH lookup plus marker files in the root.
pub fn available(cfg: &LspConfig, root: &Path) -> bool {
    cfg.on()
        && specs(cfg).iter().any(|s| {
            // Cheap checks first: looking up a rustup proxy starts a process.
            (TRUSTED.load(Ordering::Relaxed) || s.safe_untrusted)
                && (s.markers.iter().any(|m| root.join(m).exists())
                    || (s.markers.is_empty() && !s.extensions.is_empty()))
                && which(&s.command).is_some()
        })
}

pub fn path_to_uri(p: &Path) -> String {
    let mut s = String::from("file://");
    for b in p.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                s.push(b as char)
            }
            _ => s.push_str(&format!("%{b:02X}")),
        }
    }
    s
}

pub fn uri_to_path(u: &str) -> PathBuf {
    let raw = u.strip_prefix("file://").unwrap_or(u);
    PathBuf::from(
        urlencoding::decode(raw)
            .map(|c| c.into_owned())
            .unwrap_or_else(|_| raw.to_string()),
    )
}

// ---------------------------------------------------------------------------
// Wire format

pub async fn write_message<W: AsyncWrite + Unpin>(w: &mut W, msg: &Value) -> std::io::Result<()> {
    let body = msg.to_string();
    w.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    w.write_all(body.as_bytes()).await?;
    w.flush().await
}

/// Read one framed message; Ok(None) on EOF.
pub async fn read_message<R: tokio::io::AsyncBufRead + Unpin>(
    r: &mut R,
) -> std::io::Result<Option<Value>> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            if len.is_some() {
                break;
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse().ok();
        }
    }
    let mut buf = vec![0u8; len.unwrap_or(0)];
    r.read_exact(&mut buf).await?;
    Ok(Some(serde_json::from_slice(&buf).unwrap_or(Value::Null)))
}

// ---------------------------------------------------------------------------
// Client

struct Published {
    seq: u64,
    version: Option<i64>,
    items: Vec<Value>,
}

struct Doc {
    version: i64,
    text: String,
}

pub struct Client {
    name: String,
    root: PathBuf,
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>,
    next_id: AtomicI64,
    diags: Mutex<HashMap<String, Published>>,
    seq: watch::Sender<u64>,
    docs: tokio::sync::Mutex<HashMap<String, Doc>>,
    alive: AtomicBool,
    child: Mutex<Option<Child>>,
    #[cfg(unix)]
    pid: Option<u32>,
    /// Work-done progress tokens currently running (indexing, cargo check…).
    progress: Mutex<std::collections::HashSet<String>>,
    /// rust-analyzer's `experimental/serverStatus`: None until the first report.
    quiescent: Mutex<Option<bool>>,
    started: Instant,
}

impl Client {
    async fn start(spec: &Spec, root: &Path) -> Result<Arc<Client>> {
        let mut cmd = tokio::process::Command::new(
            which(&spec.command).unwrap_or_else(|| spec.command.clone().into()),
        );
        cmd.args(&spec.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow!("starting {}: {e}", spec.command))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (seq, _) = watch::channel(0u64);
        let client = Arc::new(Client {
            name: spec.name.clone(),
            root: root.to_path_buf(),
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Default::default(),
            next_id: AtomicI64::new(1),
            diags: Default::default(),
            seq,
            docs: Default::default(),
            alive: AtomicBool::new(true),
            #[cfg(unix)]
            pid: child.id(),
            child: Mutex::new(Some(child)),
            progress: Default::default(),
            quiescent: Mutex::new(None),
            started: Instant::now(),
        });
        let weak = Arc::downgrade(&client);
        tokio::spawn(async move {
            let mut r = BufReader::new(stdout);
            while let Ok(Some(msg)) = read_message(&mut r).await {
                let Some(c) = weak.upgrade() else { break };
                c.dispatch(msg).await;
            }
            if let Some(c) = weak.upgrade() {
                c.alive.store(false, Ordering::Relaxed);
                for (_, tx) in c.pending.lock().unwrap().drain() {
                    let _ = tx.send(Err("language server exited".into()));
                }
            }
        });
        let uri = path_to_uri(root);
        let init = json!({
            "processId": std::process::id(),
            "rootUri": uri,
            "rootPath": root,
            "workspaceFolders": [{"uri": uri, "name": root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()}],
            "capabilities": {
                "textDocument": {
                    "synchronization": {"didSave": true, "dynamicRegistration": false},
                    "publishDiagnostics": {"versionSupport": true, "relatedInformation": false},
                    "hover": {"contentFormat": ["plaintext", "markdown"]},
                    "definition": {"linkSupport": true},
                    "references": {},
                    "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                },
                "workspace": {"workspaceFolders": true, "configuration": true, "symbol": {}},
                "window": {"workDoneProgress": true},
                "general": {"positionEncodings": ["utf-16"]},
                "experimental": {"serverStatusNotification": true},
            },
            "clientInfo": {"name": "chanto", "version": env!("CARGO_PKG_VERSION")},
        });
        client
            .request("initialize", init, Duration::from_secs(30))
            .await?;
        client.notify("initialized", json!({})).await?;
        Ok(client)
    }

    async fn dispatch(self: &Arc<Self>, msg: Value) {
        if std::env::var_os("CHANTO_LSP_DEBUG").is_some() {
            eprintln!(
                "[lsp {} {:?}] {}",
                self.name,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis()
                    % 100000,
                crate::util::truncate(&msg.to_string(), 300)
            );
        }
        let method = msg.get("method").and_then(|m| m.as_str());
        let id = msg.get("id").cloned();
        match (method, id) {
            (Some(m), Some(id)) => {
                // Server -> client request.
                let result = match m {
                    "workspace/configuration" => {
                        let n = msg["params"]["items"]
                            .as_array()
                            .map(|a| a.len())
                            .unwrap_or(0);
                        Some(Value::Array(vec![Value::Null; n]))
                    }
                    "workspace/workspaceFolders" => {
                        Some(json!([{"uri": path_to_uri(&self.root), "name": "root"}]))
                    }
                    "client/registerCapability"
                    | "client/unregisterCapability"
                    | "window/workDoneProgress/create"
                    | "window/showMessageRequest"
                    | "workspace/diagnostic/refresh"
                    | "workspace/semanticTokens/refresh"
                    | "workspace/inlayHint/refresh"
                    | "workspace/codeLens/refresh" => Some(Value::Null),
                    _ => None,
                };
                let reply = match result {
                    Some(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
                    None => {
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}})
                    }
                };
                let mut w = self.stdin.lock().await;
                let _ = write_message(&mut *w, &reply).await;
            }
            (Some("experimental/serverStatus"), None) => {
                *self.quiescent.lock().unwrap() = msg["params"]["quiescent"].as_bool();
                self.seq.send_modify(|_| {});
            }
            (Some("$/progress"), None) => {
                let token = msg["params"]["token"].to_string();
                let mut p = self.progress.lock().unwrap();
                match msg["params"]["value"]["kind"].as_str() {
                    Some("begin") => {
                        p.insert(token);
                    }
                    Some("end") => {
                        p.remove(&token);
                        drop(p);
                        // Wake waiters: the server may now be done.
                        self.seq.send_modify(|_| {});
                    }
                    _ => {}
                }
            }
            (Some("textDocument/publishDiagnostics"), None) => {
                let p = &msg["params"];
                let Some(uri) = p["uri"].as_str() else { return };
                let next = *self.seq.borrow() + 1;
                self.diags.lock().unwrap().insert(
                    normalize_uri(uri),
                    Published {
                        seq: next,
                        version: p["version"].as_i64(),
                        items: p["diagnostics"].as_array().cloned().unwrap_or_default(),
                    },
                );
                self.seq.send_replace(next);
            }
            (None, Some(id)) => {
                let Some(id) = id.as_i64() else { return };
                if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
                    let r = match msg.get("error") {
                        Some(e) if !e.is_null() => {
                            Err(e["message"].as_str().unwrap_or("error").to_string())
                        }
                        _ => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = tx.send(r);
                }
            }
            _ => {}
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let mut w = self.stdin.lock().await;
        write_message(
            &mut *w,
            &json!({"jsonrpc": "2.0", "method": method, "params": params}),
        )
        .await?;
        Ok(())
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        if !self.alive.load(Ordering::Relaxed) {
            bail!("{} is not running", self.name);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        {
            let mut w = self.stdin.lock().await;
            write_message(
                &mut *w,
                &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
            )
            .await?;
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => bail!("{}: {method}: {e}", self.name),
            Ok(Err(_)) => bail!("{} exited", self.name),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                let _ = self.notify("$/cancelRequest", json!({"id": id})).await;
                bail!("{}: {method} timed out", self.name)
            }
        }
    }

    /// Make the server's view of `path` match `text`. Returns (uri, version, changed).
    async fn sync(&self, path: &Path, text: &str) -> Result<(String, i64, bool)> {
        let uri = path_to_uri(path);
        let mut docs = self.docs.lock().await;
        match docs.get_mut(&uri) {
            Some(d) if d.text == text => Ok((uri, d.version, false)),
            Some(d) => {
                d.version += 1;
                d.text = text.to_string();
                let v = d.version;
                self.notify("textDocument/didChange", json!({"textDocument": {"uri": uri, "version": v}, "contentChanges": [{"text": text}]})).await?;
                Ok((uri, v, true))
            }
            None => {
                docs.insert(
                    uri.clone(),
                    Doc {
                        version: 1,
                        text: text.to_string(),
                    },
                );
                self.notify("textDocument/didOpen", json!({"textDocument": {"uri": uri, "languageId": language_id(path), "version": 1, "text": text}})).await?;
                Ok((uri, 1, true))
            }
        }
    }

    /// Sync from disk (the file may have been changed by Bash etc).
    async fn sync_disk(&self, path: &Path) -> Result<(String, i64, bool)> {
        let text = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| anyhow!("reading {}: {e}", path.display()))?;
        self.sync(path, &text).await
    }

    /// Wait for diagnostics published after sequence `after` (and for at least `version`).
    async fn wait_diags(
        &self,
        uri: &str,
        after: u64,
        version: i64,
        deadline: Instant,
        settle: Duration,
    ) -> Option<Vec<Value>> {
        let key = normalize_uri(uri);
        let mut rx = self.seq.subscribe();
        let mut got: Option<(Vec<Value>, Instant)> = None;
        loop {
            {
                let d = self.diags.lock().unwrap();
                if let Some(p) = d.get(&key)
                    && p.seq > after
                    && p.version.map(|v| v >= version).unwrap_or(true)
                {
                    // Keep the latest items, but time the settle window from the first publish.
                    let first = got.as_ref().map(|g| g.1).unwrap_or_else(Instant::now);
                    got = Some((p.items.clone(), first));
                }
            }
            // Once something arrived, finish after the settle window unless the
            // server is still working (e.g. cargo check), bounded by the deadline.
            let busy = self.busy();
            let until = match &got {
                Some((_, first)) if !busy => (*first + settle).min(deadline),
                _ => deadline,
            };
            if Instant::now() >= until || !self.alive.load(Ordering::Relaxed) {
                return got.map(|g| g.0);
            }
            match tokio::time::timeout_at(until, rx.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return got.map(|g| g.0),
                Err(_) => return got.map(|g| g.0),
            }
        }
    }

    /// Still loading or checking: results so far may be incomplete.
    fn busy(&self) -> bool {
        if let Some(q) = *self.quiescent.lock().unwrap() {
            return !q;
        }
        !self.progress.lock().unwrap().is_empty()
    }

    /// Wait (bounded) until the server has loaded the project, so queries
    /// don't come back empty from a half-indexed workspace.
    async fn wait_ready(&self, deadline: Instant) {
        let mut rx = self.seq.subscribe();
        loop {
            let settled = self.started.elapsed() > Duration::from_millis(500);
            let ready = match *self.quiescent.lock().unwrap() {
                Some(q) => q,
                None => settled && self.progress.lock().unwrap().is_empty(),
            };
            if ready || !self.alive.load(Ordering::Relaxed) {
                return;
            }
            let until = if settled {
                deadline
            } else {
                deadline.min(self.started + Duration::from_millis(500))
            };
            if Instant::now() >= deadline {
                return;
            }
            let _ = tokio::time::timeout_at(until, rx.changed()).await;
        }
    }

    fn cached(&self, uri: &str) -> Option<Vec<Value>> {
        self.diags
            .lock()
            .unwrap()
            .get(&normalize_uri(uri))
            .map(|p| p.items.clone())
    }

    fn kill(&self) {
        self.alive.store(false, Ordering::Relaxed);
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            unsafe {
                libc::killpg(pid as i32, libc::SIGKILL);
            }
        }
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.start_kill();
        }
    }
}

/// Servers differ in how they escape URIs; compare decoded paths.
fn normalize_uri(u: &str) -> String {
    let p = uri_to_path(u);
    p.canonical().unwrap_or(p).display().to_string()
}

// ---------------------------------------------------------------------------
// Manager

type Cell = Arc<OnceCell<Option<Arc<Client>>>>;

fn clients() -> &'static Mutex<HashMap<(String, PathBuf), Cell>> {
    static M: OnceLock<Mutex<HashMap<(String, PathBuf), Cell>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// Get (starting if needed) the server for `spec` at `root`, waiting at most `wait`.
/// Startup continues in the background if the wait runs out.
async fn client(spec: &Spec, root: &Path, wait: Duration) -> Option<Arc<Client>> {
    let key = (spec.name.clone(), root.to_path_buf());
    let cell = {
        let mut m = clients().lock().unwrap();
        // A server that crashed is dropped so the next call restarts it.
        if let Some(Some(Some(c))) = m.get(&key).map(|cell| cell.get())
            && !c.alive.load(Ordering::Relaxed)
        {
            m.remove(&key);
        }
        if !m.contains_key(&key) && m.len() >= MAX_SERVERS {
            return None;
        }
        m.entry(key).or_default().clone()
    };
    if let Some(c) = cell.get() {
        return c.clone().filter(|c| c.alive.load(Ordering::Relaxed));
    }
    let (spec, root) = (spec.clone(), root.to_path_buf());
    let task = tokio::spawn(async move {
        cell.get_or_init(|| async { Client::start(&spec, &root).await.ok() })
            .await
            .clone()
    });
    tokio::time::timeout(wait, task)
        .await
        .ok()?
        .ok()?
        .filter(|c| c.alive.load(Ordering::Relaxed))
}

async fn client_for_file(env: &Env, path: &Path, wait: Duration) -> Option<(Arc<Client>, Spec)> {
    let spec = spec_for(&env.config.lsp, path)?;
    if !eligible(env, path, &spec) {
        return None;
    }
    let root = root_for(&spec, path, &project_root(env));
    Some((client(&spec, &root, wait).await?, spec))
}

pub fn shutdown() {
    let all: Vec<Cell> = clients().lock().unwrap().drain().map(|(_, c)| c).collect();
    for cell in all {
        if let Some(Some(c)) = cell.get() {
            c.kill();
        }
    }
}

fn wait_for(cfg: &LspConfig, spec: &Spec) -> Duration {
    Duration::from_millis(cfg.diagnostics_wait_ms.unwrap_or(spec.wait_ms))
}

/// Start the server for a file in the background (e.g. when it is first read),
/// so it has indexed the project by the time the model edits something.
pub fn warm(env: &Env, path: &Path) {
    let Some(spec) = spec_for(&env.config.lsp, path) else {
        return;
    };
    if !eligible(env, path, &spec) {
        return;
    }
    let root = root_for(&spec, path, &project_root(env));
    let path = path.to_path_buf();
    tokio::spawn(async move {
        if let Some(c) = client(&spec, &root, Duration::from_secs(60)).await {
            let _ = c.sync_disk(&path).await;
        }
    });
}

fn format_diags(items: &[Value], max_severity: i64) -> Option<(String, usize)> {
    let mut errs: Vec<&Value> = items
        .iter()
        .filter(|d| d["severity"].as_i64().unwrap_or(1) <= max_severity)
        .collect();
    if errs.is_empty() {
        return None;
    }
    errs.sort_by_key(|d| {
        (
            d["severity"].as_i64().unwrap_or(1),
            d["range"]["start"]["line"].as_i64().unwrap_or(0),
        )
    });
    let n = errs.len();
    let mut lines: Vec<String> = errs
        .iter()
        .take(20)
        .map(|d| {
            let s = &d["range"]["start"];
            let sev = match d["severity"].as_i64().unwrap_or(1) {
                1 => "error",
                2 => "warning",
                3 => "info",
                _ => "hint",
            };
            let msg = d["message"]
                .as_str()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            let code = match &d["code"] {
                Value::String(c) => format!(" {c}"),
                Value::Number(c) => format!(" {c}"),
                _ => String::new(),
            };
            let src = d["source"]
                .as_str()
                .map(|s| format!(" [{s}{code}]"))
                .unwrap_or_else(|| {
                    if code.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", code.trim())
                    }
                });
            format!(
                "L{}:{} {sev}: {msg}{src}",
                s["line"].as_i64().unwrap_or(0) + 1,
                s["character"].as_i64().unwrap_or(0) + 1
            )
        })
        .collect();
    if n > 20 {
        lines.push(format!("… and {} more", n - 20));
    }
    Some((lines.join("\n"), n))
}

/// Errors reported by the language server after `path` was edited, or None
/// if it is clean, no server is available, or none answered in time.
pub async fn diagnostics_after_edit(env: &Env, path: &Path) -> Option<(String, usize)> {
    let spec = spec_for(&env.config.lsp, path)?;
    if !eligible(env, path, &spec) {
        return None;
    }
    let wait = wait_for(&env.config.lsp, &spec);
    let deadline = Instant::now() + wait;
    let root = root_for(&spec, path, &project_root(env));
    let c = client(&spec, &root, wait).await?;
    let after = *c.seq.borrow();
    let (uri, version, changed) = c.sync_disk(path).await.ok()?;
    let _ = c
        .notify(
            "textDocument/didSave",
            json!({"textDocument": {"uri": uri}}),
        )
        .await;
    let items = match c
        .wait_diags(
            &uri,
            after,
            version,
            deadline,
            Duration::from_millis(spec.settle_ms),
        )
        .await
    {
        Some(i) => i,
        None if !changed => c.cached(&uri)?,
        None => return None,
    };
    format_diags(&items, 1)
}

/// Paths an edit-like tool call changed.
fn edited_paths(env: &Env, tool: &str, args: &Value) -> Vec<PathBuf> {
    let cwd = env.cwd();
    match tool {
        "Write" | "Edit" | "MultiEdit" => args["file_path"]
            .as_str()
            .map(|p| vec![crate::util::resolve(&cwd, p)])
            .unwrap_or_default(),
        t if t.to_ascii_lowercase().contains("patch") => {
            let text = ["input", "patch", "diff"]
                .iter()
                .find_map(|k| args[*k].as_str())
                .unwrap_or("");
            text.lines()
                .filter_map(|l| {
                    let l = l.trim();
                    l.strip_prefix("*** Update File:")
                        .or_else(|| l.strip_prefix("*** Add File:"))
                        .or_else(|| l.strip_prefix("*** Move to:"))
                        .or_else(|| l.strip_prefix("+++ b/"))
                        .map(|p| crate::util::resolve(&cwd, p.trim()))
                })
                .collect()
        }
        _ => vec![],
    }
}

/// Called after every tool run: warms servers on Read, and appends language
/// server errors to the output of successful edits.
pub async fn after_tool(env: &Env, tool: &str, args: &Value, out: &mut ToolOutput) {
    if out.is_error || !env.config.lsp.on() {
        return;
    }
    if tool == "Read" {
        if let Some(p) = args["file_path"].as_str() {
            warm(env, &crate::util::resolve(&env.cwd(), p));
        }
        return;
    }
    let paths = edited_paths(env, tool, args);
    if paths.is_empty() {
        return;
    }
    let t0 = std::time::Instant::now();
    let results =
        futures::future::join_all(paths.iter().map(|p| diagnostics_after_edit(env, p))).await;
    if std::env::var_os("CHANTO_LSP_DEBUG").is_some() {
        eprintln!("[lsp] diagnostics after {tool} took {:?}", t0.elapsed());
    }
    let cwd = env.cwd();
    let mut total = 0;
    for (p, r) in paths.iter().zip(results) {
        if let Some((text, n)) = r {
            total += n;
            out.content.push_str(&format!(
                "\n\n<diagnostics file=\"{}\">\n{text}\n</diagnostics>",
                crate::util::rel(&cwd, p)
            ));
        }
    }
    if total > 0 {
        let note = format!(
            "\x1b[31m{total} error{} reported by the language server\x1b[0m",
            if total == 1 { "" } else { "s" }
        );
        out.display = Some(match out.display.take() {
            Some(d) => format!("{d}\n{note}"),
            None => note,
        });
    }
}

// ---------------------------------------------------------------------------
// Queries for the Lsp tool

/// Convert a 1-based (line, character-in-chars) position to LSP's 0-based UTF-16 position.
fn position(text: &str, line: u64, col: u64) -> Value {
    let l = line.saturating_sub(1);
    let line_text = text.lines().nth(l as usize).unwrap_or("");
    let utf16: usize = line_text
        .chars()
        .take(col.saturating_sub(1) as usize)
        .map(|c| c.len_utf16())
        .sum();
    json!({"line": l, "character": utf16})
}

fn fmt_location(cwd: &Path, uri: &str, range: &Value) -> String {
    let p = uri_to_path(uri);
    let line = range["start"]["line"].as_u64().unwrap_or(0);
    let col = range["start"]["character"].as_u64().unwrap_or(0);
    let snippet = std::fs::read_to_string(&p)
        .ok()
        .and_then(|t| {
            t.lines()
                .nth(line as usize)
                .map(|l| crate::util::one_line(l, 120))
        })
        .unwrap_or_default();
    format!(
        "{}:{}:{}: {snippet}",
        crate::util::rel(cwd, &p),
        line + 1,
        col + 1
    )
}

fn locations(cwd: &Path, v: &Value, max: usize) -> String {
    let list: Vec<Value> = match v {
        Value::Array(a) => a.clone(),
        Value::Null => vec![],
        o => vec![o.clone()],
    };
    if list.is_empty() {
        return "No results".into();
    }
    let mut out: Vec<String> = list
        .iter()
        .take(max)
        .map(|l| {
            let uri = l["uri"].as_str().or(l["targetUri"].as_str()).unwrap_or("");
            let range = if l["range"].is_object() {
                &l["range"]
            } else {
                &l["targetSelectionRange"]
            };
            fmt_location(cwd, uri, range)
        })
        .collect();
    if list.len() > max {
        out.push(format!("… and {} more", list.len() - max));
    }
    out.join("\n")
}

const SYMBOL_KINDS: &[&str] = &[
    "",
    "file",
    "module",
    "namespace",
    "package",
    "class",
    "method",
    "property",
    "field",
    "constructor",
    "enum",
    "interface",
    "function",
    "variable",
    "constant",
    "string",
    "number",
    "boolean",
    "array",
    "object",
    "key",
    "null",
    "enum member",
    "struct",
    "event",
    "operator",
    "type parameter",
];

fn kind_name(v: &Value) -> &'static str {
    SYMBOL_KINDS
        .get(v.as_u64().unwrap_or(0) as usize)
        .copied()
        .unwrap_or("symbol")
}

fn document_symbols(v: &Value, depth: usize, out: &mut Vec<String>) {
    for s in v.as_array().into_iter().flatten() {
        if out.len() >= 300 {
            return;
        }
        let range = if s["selectionRange"].is_object() {
            &s["selectionRange"]
        } else if s["range"].is_object() {
            &s["range"]
        } else {
            &s["location"]["range"]
        };
        out.push(format!(
            "{}{} {} L{}",
            "  ".repeat(depth),
            kind_name(&s["kind"]),
            s["name"].as_str().unwrap_or("?"),
            range["start"]["line"].as_u64().unwrap_or(0) + 1
        ));
        document_symbols(&s["children"], depth + 1, out);
    }
}

fn hover_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(hover_text).collect::<Vec<_>>().join("\n\n"),
        Value::Object(o) => o.get("value").map(hover_text).unwrap_or_default(),
        _ => String::new(),
    }
}

pub async fn query(
    env: &Env,
    op: &str,
    file: Option<&Path>,
    line: u64,
    col: u64,
    q: &str,
) -> Result<String> {
    let cwd = env.cwd();
    let start_wait = Duration::from_secs(30);
    if op == "workspaceSymbol" {
        let mut results = Vec::new();
        let cs: Vec<Arc<Client>> = match file {
            Some(f) => client_for_file(env, f, start_wait)
                .await
                .map(|c| vec![c.0])
                .unwrap_or_default(),
            None => {
                let root = project_root(env);
                let mut v = Vec::new();
                for s in specs(&env.config.lsp) {
                    if (TRUSTED.load(Ordering::Relaxed) || s.safe_untrusted)
                        && which(&s.command).is_some()
                        && s.markers.iter().any(|m| root.join(m).exists())
                        && !v.iter().any(|x: &Spec| x.extensions == s.extensions)
                    {
                        v.push(s);
                    }
                }
                let mut cs = Vec::new();
                for s in v {
                    if let Some(c) = client(&s, &root, start_wait).await {
                        cs.push(c);
                    }
                }
                cs
            }
        };
        if cs.is_empty() {
            bail!("no language server available for this project");
        }
        for c in cs {
            c.wait_ready(Instant::now() + Duration::from_secs(20)).await;
            let r = c
                .request(
                    "workspace/symbol",
                    json!({"query": q}),
                    Duration::from_secs(20),
                )
                .await?;
            for s in r.as_array().into_iter().flatten().take(50) {
                let loc = &s["location"];
                let range = if loc["range"].is_object() {
                    loc["range"].clone()
                } else {
                    json!({"start": {"line": 0, "character": 0}})
                };
                results.push(format!(
                    "{} {} — {}",
                    kind_name(&s["kind"]),
                    s["name"].as_str().unwrap_or("?"),
                    fmt_location(&cwd, loc["uri"].as_str().unwrap_or(""), &range)
                ));
            }
        }
        return Ok(if results.is_empty() {
            "No results".into()
        } else {
            results.join("\n")
        });
    }
    let file = file.ok_or_else(|| anyhow!("file_path is required for {op}"))?;
    let (c, spec) = client_for_file(env, file, start_wait)
        .await
        .ok_or_else(|| {
            anyhow!(
                "no language server available for {} (or it failed to start)",
                file.display()
            )
        })?;
    let after = *c.seq.borrow();
    let (uri, version, changed) = c.sync_disk(file).await?;
    if op != "diagnostics" {
        c.wait_ready(Instant::now() + Duration::from_secs(20)).await;
    }
    let text = c
        .docs
        .lock()
        .await
        .get(&uri)
        .map(|d| d.text.clone())
        .unwrap_or_default();
    let td = json!({"uri": uri});
    let timeout = Duration::from_secs(20);
    Ok(match op {
        "diagnostics" => {
            let wait = wait_for(&env.config.lsp, &spec).max(Duration::from_secs(5));
            // Unchanged file with results already in hand: answer immediately.
            let known = if changed || c.busy() {
                None
            } else {
                c.cached(&uri)
            };
            let items = match known {
                Some(i) => Some(i),
                None => match c
                    .wait_diags(
                        &uri,
                        after,
                        version,
                        Instant::now() + wait,
                        Duration::from_millis(spec.settle_ms),
                    )
                    .await
                {
                    Some(i) => Some(i),
                    None if !changed => c.cached(&uri),
                    None => None,
                },
            };
            match items {
                None => format!(
                    "{} did not report diagnostics in time (it may still be indexing)",
                    c.name
                ),
                Some(i) => format_diags(&i, 2)
                    .map(|(t, _)| t)
                    .unwrap_or_else(|| "No errors or warnings".into()),
            }
        }
        "definition" | "references" | "hover" | "implementation" | "typeDefinition" => {
            let mut params = json!({"textDocument": td, "position": position(&text, line, col)});
            if op == "references" {
                params["context"] = json!({"includeDeclaration": true});
            }
            let r = c
                .request(&format!("textDocument/{op}"), params, timeout)
                .await?;
            if op == "hover" {
                let t = hover_text(&r["contents"]);
                if t.trim().is_empty() {
                    "No hover information".into()
                } else {
                    crate::util::truncate(t.trim(), 4000)
                }
            } else {
                locations(&cwd, &r, 100)
            }
        }
        "documentSymbol" => {
            let r = c
                .request(
                    "textDocument/documentSymbol",
                    json!({"textDocument": td}),
                    timeout,
                )
                .await?;
            let mut out = Vec::new();
            document_symbols(&r, 0, &mut out);
            if out.is_empty() {
                "No symbols".into()
            } else {
                out.join("\n")
            }
        }
        o => bail!(
            "unknown operation `{o}` (use diagnostics, definition, references, hover, documentSymbol, workspaceSymbol)"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn framing_roundtrip() {
        let mut buf: Vec<u8> = Vec::new();
        write_message(&mut buf, &json!({"a": "é"})).await.unwrap();
        write_message(&mut buf, &json!({"b": 2})).await.unwrap();
        let mut r = BufReader::new(&buf[..]);
        assert_eq!(read_message(&mut r).await.unwrap().unwrap()["a"], "é");
        assert_eq!(read_message(&mut r).await.unwrap().unwrap()["b"], 2);
        assert!(read_message(&mut r).await.unwrap().is_none());
    }

    #[test]
    fn uri_roundtrip_and_positions() {
        let p = Path::new("/tmp/a b/ü.rs");
        assert_eq!(uri_to_path(&path_to_uri(p)), p);
        assert_eq!(
            position("x\n😀ab", 2, 2),
            json!({"line": 1, "character": 2})
        );
    }

    #[test]
    fn formats_errors_only() {
        let items = vec![
            json!({"severity": 2, "message": "unused", "range": {"start": {"line": 0, "character": 0}}}),
            json!({"severity": 1, "message": "mismatched types\nmore", "source": "rustc", "code": "E0308", "range": {"start": {"line": 4, "character": 8}}}),
        ];
        let (t, n) = format_diags(&items, 1).unwrap();
        assert_eq!(n, 1);
        assert_eq!(t, "L5:9 error: mismatched types [rustc E0308]");
        assert!(format_diags(&items[..1], 1).is_none());
    }

    const FAKE: &str = r#"
import sys, json
def read():
    n = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        line = line.strip()
        if not line: break
        k, v = line.split(b':', 1)
        if k.lower() == b'content-length': n = int(v)
    return json.loads(sys.stdin.buffer.read(n))
def send(m):
    b = json.dumps(m).encode()
    sys.stdout.buffer.write(b'Content-Length: %d\r\n\r\n' % len(b) + b); sys.stdout.buffer.flush()
while True:
    m = read()
    meth = m.get('method')
    if meth == 'initialize':
        send({'jsonrpc': '2.0', 'id': 99, 'method': 'workspace/configuration', 'params': {'items': [{}]}})
        send({'jsonrpc': '2.0', 'id': m['id'], 'result': {'capabilities': {}}})
    elif meth in ('textDocument/didOpen', 'textDocument/didChange'):
        td = m['params']['textDocument']
        text = m['params']['textDocument'].get('text') or m['params']['contentChanges'][0]['text']
        diags = [{'severity': 1, 'message': 'bad thing', 'source': 'fake', 'range': {'start': {'line': i, 'character': 0}, 'end': {'line': i, 'character': 1}}}
                 for i, l in enumerate(text.split('\n')) if 'ERR' in l]
        send({'jsonrpc': '2.0', 'method': 'textDocument/publishDiagnostics', 'params': {'uri': td['uri'], 'version': td['version'], 'diagnostics': diags}})
    elif meth == 'textDocument/hover':
        send({'jsonrpc': '2.0', 'id': m['id'], 'result': {'contents': {'kind': 'markdown', 'value': 'fake hover %d' % m['params']['position']['line']}}})
    elif 'id' in m and meth:
        send({'jsonrpc': '2.0', 'id': m['id'], 'result': None})
"#;

    #[tokio::test]
    async fn fake_server_diagnostics_and_hover() {
        let dir =
            std::env::temp_dir().join(format!("chanto-lsp-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake_lsp.py");
        std::fs::write(&script, FAKE).unwrap();
        let spec = Spec {
            name: "fake".into(),
            command: "python3".into(),
            args: vec![script.display().to_string()],
            extensions: vec!["fk".into()],
            markers: vec![],
            wait_ms: 3000,
            settle_ms: 100,
            safe_untrusted: true,
        };
        let file = dir.join("a.fk");
        std::fs::write(&file, "ok\nERR here\n").unwrap();
        let c = client(&spec, &dir, Duration::from_secs(10))
            .await
            .expect("fake server starts");
        let after = *c.seq.borrow();
        let (uri, v, changed) = c.sync_disk(&file).await.unwrap();
        assert!(changed);
        let items = c
            .wait_diags(
                &uri,
                after,
                v,
                Instant::now() + Duration::from_secs(3),
                Duration::from_millis(100),
            )
            .await
            .unwrap();
        assert_eq!(
            format_diags(&items, 1).unwrap().0,
            "L2:1 error: bad thing [fake]"
        );
        // Fix the file: diagnostics clear.
        std::fs::write(&file, "ok\nfine\n").unwrap();
        let after = *c.seq.borrow();
        let (uri, v, _) = c.sync_disk(&file).await.unwrap();
        assert_eq!(v, 2);
        let items = c
            .wait_diags(
                &uri,
                after,
                v,
                Instant::now() + Duration::from_secs(3),
                Duration::from_millis(100),
            )
            .await
            .unwrap();
        assert!(format_diags(&items, 1).is_none());
        let h = c
            .request(
                "textDocument/hover",
                json!({"textDocument": {"uri": uri}, "position": {"line": 1, "character": 0}}),
                Duration::from_secs(3),
            )
            .await
            .unwrap();
        assert_eq!(hover_text(&h["contents"]), "fake hover 1");
        c.kill();
    }
}
