//! Container isolation for background runs (`[background] isolate = "docker"`
//! or `"podman"`).
//!
//! The agent loop stays on the host; only the commands the model writes run in
//! the container. Each run gets one long-lived container with its worktree
//! bind-mounted at the same path, and every shell command (Bash, background
//! shells, the verified-mode checks) is a `docker exec` into it. Why not run the
//! whole worker inside the container: on macOS there is no Linux build of
//! Chanto to mount, the model credentials would have to go into the container,
//! and the container would need the user's provider configuration. This way
//! file edits by Chanto's own tools happen on the host worktree (the bind mount
//! makes them visible inside), provider keys never enter the container, and it
//! works the same with Docker Desktop on macOS and with Docker or Podman on
//! Linux.
//!
//! Lifecycle: the worker creates the container when the run starts (or
//! resumes) and registers it for the worktree; the daemon removes it (and the
//! run's proxy relay and network) when the worker ends, on `chanto runs rm`,
//! and sweeps leftovers from a previous daemon when it starts.
//!
//! Network: `none` (no network at all), `full` (the engine's default bridge),
//! or `allowlist`: the container sits on an internal network whose only other
//! member is a tiny relay container that forwards to Chanto's filtering proxy
//! on the host, so only the `[sandbox] allowed_domains` are reachable.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Port the relay listens on inside the run's network, and its DNS alias.
const RELAY_PORT: u16 = 3128;
const RELAY_ALIAS: &str = "chanto-proxy";
/// The relay needs only busybox `nc`.
const RELAY_IMAGE: &str = "alpine:latest";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetMode {
    #[default]
    None,
    Allowlist,
    Full,
}

impl NetMode {
    pub fn parse(s: &str) -> Option<NetMode> {
        match s {
            "none" => Some(NetMode::None),
            "allowlist" => Some(NetMode::Allowlist),
            "full" => Some(NetMode::Full),
            _ => None,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            NetMode::None => "none",
            NetMode::Allowlist => "allowlist",
            NetMode::Full => "full",
        }
    }
}

/// How a run is isolated, fixed when the run is created.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Isolation {
    /// "docker" or "podman".
    pub engine: String,
    pub image: String,
    #[serde(default)]
    pub network: NetMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
}

impl Isolation {
    /// From `[background]`: None when runs aren't containerized. `dir` is the
    /// checkout, for picking an image by project type.
    pub fn from_config(b: &super::BackgroundConfig, dir: &Path) -> Result<Option<Isolation>> {
        let engine = match b.isolate.as_deref().map(str::trim) {
            None | Some("" | "none") => return Ok(None),
            Some(e @ ("docker" | "podman")) => e.to_string(),
            Some(other) => bail!(
                "[background] isolate = \"{other}\" is not supported (use \"none\", \"docker\" or \"podman\")"
            ),
        };
        let network = match b.network.as_deref() {
            None => NetMode::None,
            Some(n) => NetMode::parse(n).ok_or_else(|| anyhow::anyhow!("[background] network = \"{n}\" is not supported (use \"none\", \"allowlist\" or \"full\")"))?,
        };
        if let Some(c) = b.cpus
            && !(c > 0.0 && c.is_finite())
        {
            bail!("[background] cpus must be a positive number");
        }
        if let Some(m) = &b.memory
            && !valid_memory(m)
        {
            bail!("[background] memory = \"{m}\" is not a size like \"4g\" or \"512m\"");
        }
        let image = b
            .image
            .clone()
            .filter(|i| !i.trim().is_empty())
            .unwrap_or_else(|| default_image(dir).to_string());
        if image.starts_with('-') || image.chars().any(char::is_whitespace) {
            bail!("[background] image = \"{image}\" is not an image name");
        }
        Ok(Some(Isolation {
            engine,
            image,
            network,
            cpus: b.cpus,
            memory: b.memory.clone(),
        }))
    }

    /// One line for `chanto runs show` and the web UI.
    pub fn label(&self) -> String {
        let mut s = format!(
            "{} · {} · network {}",
            self.engine,
            self.image,
            self.network.label()
        );
        if let Some(c) = self.cpus {
            s.push_str(&format!(" · {c} cpus"));
        }
        if let Some(m) = &self.memory {
            s.push_str(&format!(" · {m} memory"));
        }
        s
    }
}

fn valid_memory(m: &str) -> bool {
    let m = m.trim().to_ascii_lowercase();
    let digits = m.trim_end_matches(['b', 'k', 'm', 'g']);
    !digits.is_empty()
        && m.len() - digits.len() <= 1
        && digits.parse::<f64>().is_ok_and(|n| n > 0.0)
}

/// A sensible image for the project: its toolchain, with git and a shell.
pub fn default_image(dir: &Path) -> &'static str {
    let has = |f: &str| dir.join(f).exists();
    if has("Cargo.toml") {
        "rust:1-bookworm"
    } else if has("go.mod") {
        "golang:1-bookworm"
    } else if has("package.json") {
        "node:22-bookworm"
    } else if has("pyproject.toml") || has("requirements.txt") || has("setup.py") {
        "python:3.12-bookworm"
    } else if has("Gemfile") {
        "ruby:3-bookworm"
    } else if has("pom.xml") || has("build.gradle") || has("build.gradle.kts") {
        "eclipse-temurin:21-jdk"
    } else {
        "buildpack-deps:bookworm"
    }
}

pub fn container_name(run_id: &str) -> String {
    format!("chanto-run-{run_id}")
}
fn relay_name(run_id: &str) -> String {
    format!("chanto-run-{run_id}-proxy")
}
fn network_name(run_id: &str) -> String {
    format!("chanto-run-{run_id}-net")
}

/// Labels tie resources to the run and to this Chanto home, so cleanup never
/// touches another home's (or anyone else's) containers.
pub fn store_key(store_dir: &Path) -> String {
    use sha2::Digest;
    let p = store_dir
        .canonicalize()
        .unwrap_or_else(|_| store_dir.to_path_buf());
    let h = sha2::Sha256::digest(p.to_string_lossy().as_bytes());
    h.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

fn labels(run_id: &str, store: &str) -> Vec<String> {
    vec![
        "--label".into(),
        format!("chanto.run={run_id}"),
        "--label".into(),
        format!("chanto.store={store}"),
    ]
}

/// A bind mount: host path, container path (the same), read-only?
#[derive(Debug, Clone, PartialEq)]
pub struct Mount {
    pub path: PathBuf,
    pub read_only: bool,
}

/// Everything `docker run` needs for the run's container.
#[derive(Debug, Clone)]
pub struct Spec {
    pub run_id: String,
    pub store: String,
    pub iso: Isolation,
    pub mounts: Vec<Mount>,
    pub workdir: PathBuf,
    /// `--network` value: "none", the run's internal network, or None (default bridge).
    pub network: Option<String>,
    /// `--user uid:gid` (Linux, so files in the worktree stay the user's).
    pub user: Option<String>,
}

/// `docker run` arguments for the long-lived container (without the engine).
pub fn run_args(s: &Spec) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        container_name(&s.run_id),
    ];
    a.extend(labels(&s.run_id, &s.store));
    if let Some(n) = &s.network {
        a.extend(["--network".into(), n.clone()]);
    }
    if let Some(c) = s.iso.cpus {
        a.extend(["--cpus".into(), format!("{c}")]);
    }
    if let Some(m) = &s.iso.memory {
        a.extend(["--memory".into(), m.trim().to_string()]);
    }
    a.extend([
        "--pids-limit".into(),
        "4096".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
    ]);
    if let Some(u) = &s.user {
        a.extend(["--user".into(), u.clone()]);
    }
    for m in &s.mounts {
        let p = m.path.display().to_string();
        a.extend([
            "-v".into(),
            format!("{p}:{p}{}", if m.read_only { ":ro" } else { "" }),
        ]);
    }
    a.extend(["-w".into(), s.workdir.display().to_string()]);
    // Keep it alive doing nothing; commands arrive through `exec`.
    a.extend([
        "--entrypoint".into(),
        "tail".into(),
        s.iso.image.clone(),
        "-f".into(),
        "/dev/null".into(),
    ]);
    a
}

/// The relay: on the default bridge (to reach the host) and, once connected,
/// the run's internal network; forwards each connection to the host proxy.
pub fn relay_args(
    run_id: &str,
    store: &str,
    host_alias: &str,
    proxy_port: u16,
    add_host_gateway: bool,
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        relay_name(run_id),
    ];
    a.extend(labels(run_id, store));
    if add_host_gateway {
        a.extend(["--add-host".into(), format!("{host_alias}:host-gateway")]);
    }
    a.extend([
        "--pids-limit".into(),
        "256".into(),
        "--memory".into(),
        "64m".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
    ]);
    a.extend([
        RELAY_IMAGE.into(),
        "nc".into(),
        "-lk".into(),
        "-p".into(),
        RELAY_PORT.to_string(),
        "-e".into(),
        "nc".into(),
        host_alias.into(),
        proxy_port.to_string(),
    ]);
    a
}

/// Runs a command in the container: `$1` through bash when the image has it.
const EXEC_WRAP: &str =
    "if command -v bash >/dev/null 2>&1; then exec bash -c \"$1\"; else exec sh -c \"$1\"; fi";

/// `docker exec` arguments for one command. `token` tags every process the
/// command starts (via the environment), so it can be killed as a tree.
/// `pass`: variables the engine copies from its own environment, so their
/// values (secrets) never appear in its arguments.
pub fn exec_args(
    name: &str,
    cwd: &Path,
    env: &[(String, String)],
    pass: &[String],
    token: &str,
    script: &str,
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "exec".into(),
        "-w".into(),
        cwd.display().to_string(),
        "-e".into(),
        format!("CHANTO_EXEC={token}"),
    ];
    for (k, v) in env {
        a.extend(["-e".into(), format!("{k}={v}")]);
    }
    for k in pass {
        a.extend(["-e".into(), k.clone()]);
    }
    a.extend([
        name.to_string(),
        "sh".into(),
        "-c".into(),
        EXEC_WRAP.into(),
        "sh".into(),
        script.to_string(),
    ]);
    a
}

/// Kills every process in the container carrying `CHANTO_EXEC=<token>`.
pub fn kill_args(name: &str, token: &str) -> Vec<String> {
    let script = format!(
        "for d in /proc/[0-9]*; do if tr '\\000' '\\n' < \"$d/environ\" 2>/dev/null | grep -qx 'CHANTO_EXEC={token}'; then kill -9 \"${{d#/proc/}}\" 2>/dev/null; fi; done; true"
    );
    vec!["exec".into(), name.into(), "sh".into(), "-c".into(), script]
}

/// A user-facing message for an engine that can't be used.
pub fn engine_error(engine: &str, installed: bool, stderr: &str) -> String {
    if !installed {
        return format!(
            "[background] isolate = \"{engine}\", but `{engine}` isn't installed or isn't on PATH. Install {}, or set isolate = \"none\".",
            if engine == "podman" {
                "Podman"
            } else {
                "Docker (Docker Desktop on macOS)"
            }
        );
    }
    let first = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no answer");
    let how = if engine == "podman" {
        "start it (`podman machine start` on macOS)"
    } else {
        "start Docker Desktop (or the docker service)"
    };
    format!(
        "[background] isolate = \"{engine}\", but the {engine} engine isn't running ({first}). Please {how}, or set isolate = \"none\"."
    )
}

/// Is the engine installed and its daemon answering?
pub fn check_engine(engine: &str) -> Result<()> {
    if !crate::shell::command_exists(engine) {
        bail!("{}", engine_error(engine, false, ""));
    }
    let out = output_timeout(
        std::process::Command::new(engine).args(["info", "--format", "{{.ServerVersion}}"]),
        Duration::from_secs(20),
    );
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => bail!(
            "{}",
            engine_error(engine, true, &String::from_utf8_lossy(&o.stderr))
        ),
        Err(e) => bail!("{}", engine_error(engine, true, &format!("{e:#}"))),
    }
}

fn output_timeout(
    cmd: &mut std::process::Command,
    limit: Duration,
) -> Result<std::process::Output> {
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let t0 = std::time::Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        if t0.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            bail!("no answer within {}s", limit.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn engine_cmd(engine: &str, args: &[String]) -> std::process::Command {
    let mut c = std::process::Command::new(engine);
    c.args(args);
    c
}

/// Run an engine command; its stderr becomes the error.
async fn engine(engine: &str, args: Vec<String>) -> Result<String> {
    let out = tokio::process::Command::new(engine)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| format!("running {engine}"))?;
    if !out.status.success() {
        let verb = args.iter().take(2).cloned().collect::<Vec<_>>().join(" ");
        bail!(
            "`{engine} {verb}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// IDs from `<engine> <kind> ls -q --filter label=...` (kind "container" or "network").
fn list_labeled(engine: &str, kind: &str, label: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![kind.into(), "ls".into(), "-q".into()];
    if kind == "container" {
        args.push("-a".into());
    }
    args.extend(["--filter".into(), format!("label={label}")]);
    output_timeout(&mut engine_cmd(engine, &args), Duration::from_secs(30))
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn remove_labeled(engine: &str, label: &str) -> usize {
    let ids = list_labeled(engine, "container", label);
    if !ids.is_empty() {
        let mut args: Vec<String> = vec!["rm".into(), "-f".into()];
        args.extend(ids.iter().cloned());
        let _ = output_timeout(&mut engine_cmd(engine, &args), Duration::from_secs(60));
    }
    let nets = list_labeled(engine, "network", label);
    if !nets.is_empty() {
        let mut args: Vec<String> = vec!["network".into(), "rm".into()];
        args.extend(nets.iter().cloned());
        let _ = output_timeout(&mut engine_cmd(engine, &args), Duration::from_secs(30));
    }
    ids.len()
}

/// Remove a run's container, relay and network (blocking).
pub fn remove_run(engine: &str, run_id: &str) {
    if crate::shell::command_exists(engine) {
        remove_labeled(engine, &format!("chanto.run={run_id}"));
    }
}

/// Remove everything this Chanto home left behind (daemon start: no run is
/// active yet, so every labeled container is an orphan). Returns how many.
pub fn sweep(engine: &str, store_dir: &Path) -> usize {
    if !crate::shell::command_exists(engine) {
        return 0;
    }
    remove_labeled(engine, &format!("chanto.store={}", store_key(store_dir)))
}

/// The run's live container, as the worker uses it.
#[derive(Debug)]
pub struct Container {
    pub engine: String,
    pub name: String,
    pub worktree: PathBuf,
    /// Shared host/container scratch directory (same path in both).
    pub scratch: PathBuf,
    pub network: NetMode,
    /// `host:port` of the proxy relay, for allowlist networking.
    pub proxy: Option<String>,
    /// Extra environment for every command (HOME for a non-root user).
    pub env: Vec<(String, String)>,
    /// Run sandboxed-in-the-container commands without asking (acceptEdits).
    pub auto_allow: bool,
}

static REGISTRY: Mutex<Vec<(PathBuf, Arc<Container>)>> = Mutex::new(Vec::new());
/// Live `docker exec` clients (host pid) and the tokens of their commands.
type Tracked = HashMap<u32, (Arc<Container>, String)>;
static TRACKED: Mutex<Option<Tracked>> = Mutex::new(None);

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Route commands whose working directory is inside `c.worktree` into `c`.
pub fn register(c: Arc<Container>) {
    let mut r = REGISTRY.lock().unwrap();
    r.retain(|(_, x)| x.name != c.name);
    r.push((canon(&c.worktree), c));
}

pub fn unregister(name: &str) {
    REGISTRY.lock().unwrap().retain(|(_, c)| c.name != name);
}

/// The container that commands run from `cwd` go to, if any.
pub fn for_path(cwd: &Path) -> Option<Arc<Container>> {
    let r = REGISTRY.lock().unwrap();
    if r.is_empty() {
        return None;
    }
    let cwd = canon(cwd);
    r.iter()
        .find(|(root, _)| cwd.starts_with(root))
        .map(|(_, c)| c.clone())
}

impl Container {
    /// A `docker exec` command running `script` from `cwd`, and its kill token.
    pub fn command(
        &self,
        script: &str,
        cwd: &Path,
        extra_env: &[(String, String)],
    ) -> (tokio::process::Command, String) {
        let token = uuid::Uuid::new_v4().simple().to_string();
        let mut env: Vec<(String, String)> = [
            ("CHANTO", "1"),
            ("GIT_PAGER", "cat"),
            ("PAGER", "cat"),
            ("GIT_EDITOR", "true"),
            ("CLICOLOR", "0"),
            ("NO_COLOR", "1"),
            ("TERM", "dumb"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        env.extend(self.env.iter().cloned());
        env.extend(extra_env.iter().cloned());
        // Commands start in the worktree unless they're somewhere inside it.
        let cwd = if canon(cwd).starts_with(canon(&self.worktree)) {
            cwd.to_path_buf()
        } else {
            self.worktree.clone()
        };
        // A pulled run's org secrets reach its commands, by name.
        let pass: Vec<String> = crate::secrets::run_secret_names()
            .into_iter()
            .filter(|k| std::env::var_os(k).is_some())
            .collect();
        let mut cmd = tokio::process::Command::new(&self.engine);
        cmd.args(exec_args(&self.name, &cwd, &env, &pass, &token, script));
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        cmd.process_group(0);
        (cmd, token)
    }

    /// Proxy settings for one command under allowlist networking.
    pub fn proxy_env(&self, lease: &crate::network::Lease) -> Vec<(String, String)> {
        let Some(authority) = &self.proxy else {
            return vec![];
        };
        let url = lease.url_at(authority);
        let mut v: Vec<(String, String)> = [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ]
        .iter()
        .map(|k| (k.to_string(), url.clone()))
        .collect();
        v.push(("NO_PROXY".into(), "localhost".into()));
        v.push(("no_proxy".into(), "localhost".into()));
        v
    }

    /// Kill a command's processes inside the container (in the background).
    pub fn kill(&self, token: &str) {
        let (engine, args) = (self.engine.clone(), kill_args(&self.name, token));
        std::thread::spawn(move || {
            let _ = output_timeout(&mut engine_cmd(&engine, &args), Duration::from_secs(15));
        });
    }
}

/// Remember which container command a host `docker exec` client runs.
pub fn track(pid: Option<u32>, c: &Arc<Container>, token: &str) {
    if let Some(pid) = pid {
        TRACKED
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(pid, (c.clone(), token.to_string()));
    }
}

pub fn untrack(pid: Option<u32>) {
    if let (Some(pid), Some(m)) = (pid, TRACKED.lock().unwrap().as_mut()) {
        m.remove(&pid);
    }
}

/// Killing the `docker exec` client leaves the command running in the
/// container; this stops it there too.
pub fn kill_tracked(pid: u32) {
    let t = TRACKED
        .lock()
        .unwrap()
        .as_mut()
        .and_then(|m| m.remove(&pid));
    if let Some((c, token)) = t {
        c.kill(&token);
    }
}

/// Bind mounts for the run: the worktree and scratch dir read-write; the
/// repository's git directory read-only (so the container can't plant hooks or
/// config the host's git would run) except the worktree's own git dir, which
/// holds its index and HEAD.
pub fn mounts(worktree: &Path, scratch: &Path) -> Vec<Mount> {
    let mut m = vec![
        Mount {
            path: worktree.to_path_buf(),
            read_only: false,
        },
        Mount {
            path: scratch.to_path_buf(),
            read_only: false,
        },
    ];
    let git = |arg: &str| -> Option<PathBuf> {
        let o = std::process::Command::new("git")
            .args(["rev-parse", "--path-format=absolute", arg])
            .current_dir(worktree)
            .output()
            .ok()?;
        o.status
            .success()
            .then(|| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
    };
    if let (Some(common), Some(own)) = (git("--git-common-dir"), git("--git-dir"))
        && !common.starts_with(worktree)
    {
        m.push(Mount {
            path: common.clone(),
            read_only: true,
        });
        if own != common && !own.starts_with(worktree) {
            m.push(Mount {
                path: own,
                read_only: false,
            });
        }
    }
    m
}

/// Create the run's container (and, for allowlist networking, its network and
/// proxy relay). `note` reports progress (pulling an image can take a while).
pub async fn start(
    run: &super::Run,
    iso: &Isolation,
    auto_allow: bool,
    note: &(dyn Fn(String) + Send + Sync),
) -> Result<Arc<Container>> {
    let eng = iso.engine.clone();
    {
        let eng = eng.clone();
        tokio::task::spawn_blocking(move || check_engine(&eng)).await??;
    }
    let store_dir = super::Store::default_dir();
    let store = store_key(&store_dir);
    // A previous worker of this run (resume) may have left its container.
    {
        let (eng, id) = (eng.clone(), run.id.clone());
        tokio::task::spawn_blocking(move || remove_run(&eng, &id)).await?;
    }
    let scratch = store_dir.join(&run.id).join("container");
    std::fs::create_dir_all(&scratch)?;
    let (worktree, scratch2) = (run.worktree.clone(), scratch.clone());
    let mounts = tokio::task::spawn_blocking(move || mounts(&worktree, &scratch2)).await?;
    let linux = cfg!(target_os = "linux");
    let host_alias = if eng == "podman" {
        "host.containers.internal"
    } else {
        "host.docker.internal"
    };
    let mut proxy = None;
    let network = match iso.network {
        NetMode::None => Some("none".to_string()),
        NetMode::Full => None,
        NetMode::Allowlist => {
            // On Linux the engine's VM is the host itself: the proxy has to
            // listen where the bridge can reach it, not just on loopback.
            let port = if linux {
                let gw = engine(
                    &eng,
                    vec![
                        "network".into(),
                        "inspect".into(),
                        "bridge".into(),
                        "--format".into(),
                        "{{range .IPAM.Config}}{{.Gateway}}{{end}}".into(),
                    ],
                )
                .await
                .unwrap_or_default();
                let ip: std::net::IpAddr = gw.trim().parse().context(
                    "finding the container bridge's gateway address for the network proxy",
                )?;
                crate::network::listen_on(ip)?
            } else {
                crate::network::proxy_port()?
            };
            let net = network_name(&run.id);
            let mut a: Vec<String> = vec!["network".into(), "create".into(), "--internal".into()];
            a.extend(labels(&run.id, &store));
            a.push(net.clone());
            engine(&eng, a).await?;
            note(format!("Starting the network proxy relay ({RELAY_IMAGE})"));
            engine(&eng, relay_args(&run.id, &store, host_alias, port, linux)).await?;
            engine(
                &eng,
                vec![
                    "network".into(),
                    "connect".into(),
                    "--alias".into(),
                    RELAY_ALIAS.into(),
                    net.clone(),
                    relay_name(&run.id),
                ],
            )
            .await?;
            proxy = Some(format!("{RELAY_ALIAS}:{RELAY_PORT}"));
            Some(net)
        }
    };
    #[cfg(unix)]
    let user = linux.then(|| unsafe { format!("{}:{}", libc::getuid(), libc::getgid()) });
    #[cfg(not(unix))]
    let user: Option<String> = None;
    let env = if user.is_some() {
        vec![("HOME".to_string(), scratch.display().to_string())]
    } else {
        vec![]
    };
    let spec = Spec {
        run_id: run.id.clone(),
        store: store.clone(),
        iso: iso.clone(),
        mounts,
        workdir: run.worktree.clone(),
        network,
        user,
    };
    note(format!(
        "Starting the run's {eng} container ({}; pulled first if it isn't local)",
        iso.image
    ));
    if let Err(e) = engine(&eng, run_args(&spec)).await {
        let (eng, id) = (eng.clone(), run.id.clone());
        let _ = tokio::task::spawn_blocking(move || remove_run(&eng, &id)).await;
        return Err(e.context(format!(
            "could not start the {} container for this run",
            iso.engine
        )));
    }
    let c = Arc::new(Container {
        engine: eng,
        name: container_name(&run.id),
        worktree: run.worktree.clone(),
        scratch,
        network: iso.network,
        proxy,
        env,
        auto_allow,
    });
    register(c.clone());
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iso() -> Isolation {
        Isolation {
            engine: "docker".into(),
            image: "rust:1-bookworm".into(),
            network: NetMode::None,
            cpus: Some(2.0),
            memory: Some("4g".into()),
        }
    }

    #[test]
    fn config_parsing_and_validation() {
        let dir =
            std::env::temp_dir().join(format!("chanto-iso-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut b = super::super::BackgroundConfig::default();
        assert_eq!(Isolation::from_config(&b, &dir).unwrap(), None);
        b.isolate = Some("none".into());
        assert_eq!(Isolation::from_config(&b, &dir).unwrap(), None);
        b.isolate = Some("lxc".into());
        assert!(
            Isolation::from_config(&b, &dir)
                .unwrap_err()
                .to_string()
                .contains("not supported")
        );
        b.isolate = Some("docker".into());
        let i = Isolation::from_config(&b, &dir).unwrap().unwrap();
        assert_eq!(
            (i.image.as_str(), i.network),
            ("buildpack-deps:bookworm", NetMode::None)
        );
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        assert_eq!(
            Isolation::from_config(&b, &dir).unwrap().unwrap().image,
            "node:22-bookworm"
        );
        b.image = Some("alpine:3".into());
        b.network = Some("allowlist".into());
        b.memory = Some("512m".into());
        b.cpus = Some(1.5);
        let i = Isolation::from_config(&b, &dir).unwrap().unwrap();
        assert_eq!(
            i.label(),
            "docker · alpine:3 · network allowlist · 1.5 cpus · 512m memory"
        );
        b.network = Some("open".into());
        assert!(Isolation::from_config(&b, &dir).is_err());
        b.network = None;
        for bad in ["lots", "4gb", "-1g", ""] {
            b.memory = Some(bad.into());
            assert!(Isolation::from_config(&b, &dir).is_err(), "{bad}");
        }
        b.memory = None;
        b.cpus = Some(0.0);
        assert!(Isolation::from_config(&b, &dir).is_err());
        b.cpus = None;
        b.image = Some("--privileged".into());
        assert!(Isolation::from_config(&b, &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_images_by_project_type() {
        let dir =
            std::env::temp_dir().join(format!("chanto-img-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        for (file, image) in [
            ("Gemfile", "ruby:3-bookworm"),
            ("pyproject.toml", "python:3.12-bookworm"),
            ("package.json", "node:22-bookworm"),
            ("go.mod", "golang:1-bookworm"),
            ("Cargo.toml", "rust:1-bookworm"),
        ] {
            std::fs::write(dir.join(file), "").unwrap();
            assert_eq!(default_image(&dir), image, "{file}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_command_has_limits_mounts_and_labels() {
        let spec = Spec {
            run_id: "ab12cd34".into(),
            store: "0011aabbccdd".into(),
            iso: iso(),
            mounts: vec![
                Mount {
                    path: "/w/t".into(),
                    read_only: false,
                },
                Mount {
                    path: "/repo/.git".into(),
                    read_only: true,
                },
            ],
            workdir: "/w/t".into(),
            network: Some("none".into()),
            user: None,
        };
        let a = run_args(&spec).join(" ");
        assert_eq!(
            a,
            "run -d --name chanto-run-ab12cd34 --label chanto.run=ab12cd34 --label chanto.store=0011aabbccdd --network none --cpus 2 --memory 4g \
             --pids-limit 4096 --security-opt no-new-privileges -v /w/t:/w/t -v /repo/.git:/repo/.git:ro -w /w/t --entrypoint tail rust:1-bookworm -f /dev/null"
        );
        // Full networking is the engine's default bridge; Linux maps the user.
        let a = run_args(&Spec {
            network: None,
            user: Some("1000:1000".into()),
            iso: Isolation {
                cpus: None,
                memory: None,
                ..iso()
            },
            ..spec
        });
        assert!(
            !a.contains(&"--network".to_string())
                && !a.contains(&"--cpus".to_string())
                && !a.contains(&"--memory".to_string())
        );
        assert!(a.windows(2).any(|w| w == ["--user", "1000:1000"]));
    }

    #[test]
    fn exec_command_carries_only_the_given_environment() {
        let env = vec![
            ("NO_COLOR".to_string(), "1".to_string()),
            (
                "HTTP_PROXY".to_string(),
                "http://k:@chanto-proxy:3128".to_string(),
            ),
        ];
        let a = exec_args(
            "chanto-run-x",
            Path::new("/w/t/src"),
            &env,
            &[],
            "tok",
            "echo hi > f && cat f",
        );
        assert_eq!(&a[..5], ["exec", "-w", "/w/t/src", "-e", "CHANTO_EXEC=tok"]);
        assert!(
            a.windows(2)
                .any(|w| w == ["-e", "HTTP_PROXY=http://k:@chanto-proxy:3128"])
        );
        assert_eq!(
            a[a.len() - 6..a.len() - 2],
            ["chanto-run-x", "sh", "-c", EXEC_WRAP]
        );
        assert_eq!(
            a.last().unwrap(),
            "echo hi > f && cat f",
            "the script is one argument, never re-split by the host shell"
        );
        // Nothing from the host environment rides along.
        // SAFETY: test-only; the variable name is unique to this test.
        unsafe { std::env::set_var("CHANTO_TEST_PROVIDER_KEY", "sk-secret") };
        let c = Container {
            engine: "docker".into(),
            name: "chanto-run-x".into(),
            worktree: "/w/t".into(),
            scratch: "/s".into(),
            network: NetMode::None,
            proxy: None,
            env: vec![],
            auto_allow: true,
        };
        let (cmd, token) = c.command("env", Path::new("/elsewhere"), &[]);
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();
        assert!(!args.iter().any(|a| a.contains("sk-secret")));
        assert!(
            cmd.as_std()
                .get_envs()
                .all(|(k, _)| k != "CHANTO_TEST_PROVIDER_KEY")
        );
        assert!(
            args.windows(2).any(|w| w == ["-w", "/w/t"]),
            "outside the worktree falls back to its root: {args:?}"
        );
        assert!(args.contains(&format!("CHANTO_EXEC={token}")));
        unsafe { std::env::remove_var("CHANTO_TEST_PROVIDER_KEY") };
    }

    #[test]
    fn relay_and_kill_commands() {
        let a = relay_args("r1", "s", "host.docker.internal", 40123, false).join(" ");
        assert!(
            a.starts_with(
                "run -d --name chanto-run-r1-proxy --label chanto.run=r1 --label chanto.store=s"
            ),
            "{a}"
        );
        assert!(
            a.ends_with("alpine:latest nc -lk -p 3128 -e nc host.docker.internal 40123"),
            "{a}"
        );
        assert!(!a.contains("--add-host"));
        assert!(
            relay_args("r1", "s", "host.docker.internal", 1, true)
                .join(" ")
                .contains("--add-host host.docker.internal:host-gateway")
        );
        let k = kill_args("chanto-run-r1", "abc");
        assert_eq!(&k[..4], ["exec", "chanto-run-r1", "sh", "-c"]);
        assert!(k[4].contains("grep -qx 'CHANTO_EXEC=abc'"));
    }

    #[test]
    fn engine_errors_say_what_to_do() {
        let e = engine_error("docker", false, "");
        assert!(
            e.contains("isn't installed") && e.contains("isolate = \"none\""),
            "{e}"
        );
        let e = engine_error(
            "docker",
            true,
            "\nCannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?\n",
        );
        assert!(
            e.contains("isn't running (Cannot connect to the Docker daemon")
                && e.contains("start Docker Desktop"),
            "{e}"
        );
        assert!(engine_error("podman", true, "").contains("podman machine start"));
        assert!(
            check_engine("chanto-no-such-engine")
                .unwrap_err()
                .to_string()
                .contains("isn't installed")
        );
    }

    #[test]
    fn registry_routes_by_worktree() {
        let dir =
            std::env::temp_dir().join(format!("chanto-reg-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let c = Arc::new(Container {
            engine: "docker".into(),
            name: format!("reg-{}", uuid::Uuid::new_v4().simple()),
            worktree: dir.clone(),
            scratch: dir.clone(),
            network: NetMode::None,
            proxy: None,
            env: vec![],
            auto_allow: false,
        });
        assert!(for_path(&dir.join("src")).is_none_or(|x| x.name != c.name));
        register(c.clone());
        assert_eq!(for_path(&dir.join("src")).unwrap().name, c.name);
        assert!(for_path(&std::env::temp_dir()).is_none_or(|x| x.name != c.name));
        unregister(&c.name);
        assert!(for_path(&dir).is_none_or(|x| x.name != c.name));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
