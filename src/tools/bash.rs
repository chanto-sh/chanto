//! Shell execution: Bash (foreground with timeout, or background), BashOutput,
//! KillShell. The working directory persists between calls.

use super::{Ctx, Tool, ToolKind, ToolOutput, arg_bool, arg_str, arg_u64};
use crate::util::Canonical as _;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;

const MAX_OUTPUT: usize = 30_000;

/// Keep a command's output bounded in memory: only the head and the tail
/// survive (the model sees at most MAX_OUTPUT anyway). Without this, a command
/// like `yes` grows the buffer by gigabytes before its timeout.
fn append_bounded(buf: &mut Vec<u8>, data: &[u8]) {
    const HEAD: usize = 64 * 1024;
    const TAIL: usize = 512 * 1024;
    buf.extend_from_slice(data);
    if buf.len() > HEAD + 2 * TAIL {
        let cut_to = buf.len() - TAIL;
        buf.drain(HEAD..cut_to);
        // Mark the gap once per trim.
        let marker = b"\n... [output truncated] ...\n";
        buf.splice(HEAD..HEAD, marker.iter().copied());
    }
}

pub struct BgShell {
    pub id: String,
    pub command: String,
    /// The originating Bash call's short description, for human-facing status.
    pub description: Option<String>,
    /// Set for a Monitor watch: its description.
    pub monitor: Option<String>,
    pub output: Mutex<String>,
    /// How much of `output` has already been returned by BashOutput.
    pub read_pos: Mutex<usize>,
    /// How much of `output` a Monitor watch has already passed on.
    pub watch_pos: Mutex<usize>,
    pub exit: Mutex<Option<i32>>,
    pub pid: Option<u32>,
    network: Mutex<Option<crate::network::Lease>>,
}

impl BgShell {
    pub(crate) fn title(&self) -> String {
        crate::util::one_line(
            self.description
                .as_deref()
                .or(self.monitor.as_deref())
                .unwrap_or(&self.command),
            80,
        )
    }
}

/// Where a foreground command's output goes. It starts writing into the
/// bounded `Vec<u8>` the tool returns; if the command outlives its timeout we
/// hand the still-running process to the background and point the readers at
/// its `BgShell` instead.
enum Sink {
    Fg,
    Bg(Arc<BgShell>),
}

fn push_sink(sink: &Mutex<Sink>, buf: &Mutex<Vec<u8>>, data: &[u8]) {
    // Read the sink and write under its lock, so a hand-off to the background
    // can't race a chunk: whoever holds this lock decides where a chunk lands.
    match &*sink.lock().unwrap() {
        Sink::Fg => append_bounded(&mut buf.lock().unwrap(), data),
        Sink::Bg(sh) => push_capped(sh, &strip_ansi(&String::from_utf8_lossy(data))),
    }
}

/// A command running in a background run's container: which one, and the
/// token its processes carry (see `runs::container`).
type InContainer = Option<(Arc<crate::runs::container::Container>, String)>;

/// Stop a shell and everything it spawned. Unix has process groups; Windows
/// uses `taskkill /T`, which walks the child tree the same way.
#[cfg(unix)]
pub(crate) fn kill_group(pid: u32) {
    crate::runs::container::kill_tracked(pid);
    unsafe {
        libc::killpg(pid as i32, libc::SIGTERM);
    }
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        unsafe {
            libc::killpg(pid as i32, libc::SIGKILL);
        }
    });
}

/// Stops a foreground command's process group if the tool is dropped while
/// it runs: an interrupted turn stops waiting for the tool rather than letting
/// it finish, and the command would otherwise outlive Chanto.
struct KillOnDrop(Option<u32>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            kill_group(pid);
        }
    }
}

#[cfg(windows)]
pub(crate) fn kill_group(pid: u32) {
    crate::runs::container::kill_tracked(pid);
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod config_guard;
mod guards;
mod sandbox;
#[cfg(target_os = "linux")]
mod sandbox_linux;
#[cfg(feature = "fuzzing")]
pub(crate) use guards::{Rule, guard_rules, write_guards};
pub use sandbox::*;
#[cfg(target_os = "linux")]
use sandbox_linux::*;

fn looks_sandbox_blocked(out: &str) -> bool {
    let o = out.to_lowercase();
    o.contains("operation not permitted")
        || o.contains("permission denied")
        || o.contains("access is denied")
        || o.contains("access denied")
        || o.contains("read-only file system")
        || o.contains("sandbox")
        // A login from the keychain, which the macOS sandbox keeps out of reach.
        || (cfg!(target_os = "macos")
            && (o.contains("could not read username")
                || o.contains("terminal prompts disabled")
                || o.contains("gh auth login")
                || o.contains("keychain")))
}

/// A command that failed to reach the network (curl, wget, pip, npm, git, Python).
fn looks_network_failure(out: &str) -> bool {
    let o = out.to_lowercase();
    [
        "couldn't connect to server",
        "could not resolve host",
        "failed to connect",
        "connection refused",
        "network is unreachable",
        "failed to establish a new connection",
        "temporary failure in name resolution",
        "nodename nor servname",
        "name or service not known",
        "getaddrinfo",
        "unable to resolve host",
        "unable to connect",
    ]
    .iter()
    .any(|s| o.contains(s))
}

async fn report_network_blocks(domains: Vec<String>, ctx: &Ctx) -> String {
    use crate::agent::{PermissionReply, PermissionRequest};
    let mut report = String::new();
    for domain in domains {
        report.push_str(&format!(
            "\nnetwork access to {domain} was blocked by the sandbox allowlist\n"
        ));
        if !ctx.env.interactive {
            continue;
        }
        let denied = ctx
            .env
            .config
            .sandbox
            .denied_domains
            .iter()
            .any(|p| crate::network::domain_matches(p, &domain));
        if denied {
            report.push_str(
                "Remove the matching denied_domains entry before allowing this domain.\n",
            );
            continue;
        }
        let req = PermissionRequest {
            tool: "SandboxNetwork".into(), label: format!("Allow network access to {domain}?"),
            detail: "Allow once permits this domain for the next sandboxed command; run the command again to retry.".into(),
            always_label: "Always allow for this project (save to .chanto/config.toml)".into(), reason: None, proposed: None, auto_deny: None,
        };
        let reply = tokio::select! {
            r = ctx.env.frontend.permission(req) => r,
            _ = ctx.cancel.cancelled() => PermissionReply::Deny(None),
        };
        if matches!(reply, PermissionReply::Allow | PermissionReply::AllowAlways) {
            let root = crate::permissions::project_root().unwrap_or_else(|| ctx.env.cwd());
            if let Err(e) = crate::network::grant(
                &root,
                &domain,
                reply == PermissionReply::AllowAlways,
                &ctx.env.config.sandbox,
            ) {
                report.push_str(&format!("Could not save network permission: {e}\n"));
            } else {
                report.push_str("Domain approved; retry the command to use it.\n");
            }
        }
    }
    report
}

pub(crate) fn build_command_with(
    script: &str,
    cwd: &std::path::Path,
    sandbox: Option<&crate::config::SandboxConfig>,
    secrets: Option<&crate::secrets::SecretsConfig>,
    unsandboxed: bool,
) -> Result<(
    tokio::process::Command,
    Option<crate::network::Lease>,
    InContainer,
)> {
    // A background run's container is its sandbox: commands from its worktree
    // run there (with only the environment Chanto gives them), whatever
    // dangerouslyDisableSandbox says.
    if let Some(c) = crate::runs::container::for_path(cwd) {
        let lease = if c.network == crate::runs::container::NetMode::Allowlist {
            let root = crate::permissions::project_root().unwrap_or_else(|| cwd.to_path_buf());
            Some(crate::network::Lease::new(
                &sandbox.cloned().unwrap_or_default(),
                &root,
            )?)
        } else {
            None
        };
        let env = lease.as_ref().map(|l| c.proxy_env(l)).unwrap_or_default();
        let (cmd, token) = c.command(script, cwd, &env);
        return Ok((cmd, lease, Some((c, token))));
    }
    let (cmd, lease) =
        build_command_for_policy(script, cwd, sandbox, secrets, unsandboxed, sandbox_on())
            .inspect_err(|e| crate::feedback::report_error("sandbox_setup", &format!("{e:#}")))?;
    Ok((cmd, lease, None))
}

fn build_command_for_policy(
    script: &str,
    cwd: &std::path::Path,
    sandbox: Option<&crate::config::SandboxConfig>,
    secrets: Option<&crate::secrets::SecretsConfig>,
    unsandboxed: bool,
    sandbox_active: bool,
) -> Result<(tokio::process::Command, Option<crate::network::Lease>)> {
    let active = sandbox_active && !unsandboxed;
    // Windows can't restrict the network (allowlist fails below; deny only
    // gets a startup warning), so there is nothing to fail closed on.
    let restricted = sandbox
        .is_some_and(|s| s.network_policy() != crate::config::NetworkPolicy::Allow)
        && !cfg!(windows);
    // Say plainly that Windows can't do allowlists before probing anything:
    // an "unavailable sandbox" error would suggest the OS sandbox is broken.
    #[cfg(windows)]
    if !unsandboxed
        && sandbox.is_some_and(|s| {
            s.enabled && s.network_policy() == crate::config::NetworkPolicy::Allowlist
        })
    {
        anyhow::bail!(
            "network allowlist sandbox is unsupported on Windows; choose sandbox.network = \"deny\" or \"allow\""
        );
    }
    if !unsandboxed && restricted && sandbox.is_some_and(|s| s.enabled) && !sandbox_available() {
        anyhow::bail!(
            "network sandbox unavailable; choose sandbox.network = \"allow\" or use a supported OS sandbox"
        );
    }
    #[cfg(target_os = "linux")]
    if active && restricted && landlock_abi() < 4 {
        anyhow::bail!(
            "network sandbox requires Linux Landlock ABI v4; choose sandbox.network = \"allow\" to permit unrestricted networking"
        );
    }
    let root = crate::permissions::project_root().unwrap_or_else(|| cwd.to_path_buf());
    let network = if active
        && sandbox.is_some_and(|s| s.network_policy() == crate::config::NetworkPolicy::Allowlist)
    {
        Some(crate::network::Lease::new(sandbox.unwrap(), &root)?)
    } else {
        None
    };
    let proxy_port = network.as_ref().map(|p| p.port);
    let (prog, flags) = crate::shell::tool_shell();
    let mut cmd = match sandbox {
        Some(sb) if active && cfg!(target_os = "macos") => {
            let root = crate::permissions::project_root().unwrap_or_else(|| cwd.to_path_buf());
            let mut c = tokio::process::Command::new("/usr/bin/sandbox-exec");
            c.arg("-p")
                .arg(sandbox_profile_with_port(&root, sb, proxy_port))
                .arg(&prog);
            c
        }
        _ => tokio::process::Command::new(&prog),
    };
    #[cfg(windows)]
    let windows_sandboxed = sandbox.is_some() && active;
    #[cfg(windows)]
    if windows_sandboxed {
        let sb = sandbox.unwrap();
        let root = crate::permissions::project_root().unwrap_or_else(|| cwd.to_path_buf());
        let program =
            crate::shell::find_on_path(&prog).unwrap_or_else(|| std::path::PathBuf::from(&prog));
        // Only writes are confined on Windows: read denials and the network
        // policy can't be enforced there (startup warns about the latter).
        let request = crate::windows_sandbox::Request {
            program,
            args: flags
                .iter()
                .map(|s| s.to_string())
                .chain(std::iter::once(script.to_string()))
                .collect(),
            cwd: cwd.to_path_buf(),
            writable: writable_paths(&root, sb)
                .into_iter()
                .map(std::path::PathBuf::from)
                .collect(),
            parent_pid: std::process::id(),
        };
        cmd = tokio::process::Command::new(
            std::env::current_exe().expect("current chanto executable"),
        );
        cmd.arg(crate::windows_sandbox::LAUNCH_FLAG)
            .arg(serde_json::to_string(&request).expect("sandbox request serialization"));
    }
    #[cfg(windows)]
    if !windows_sandboxed {
        cmd.args(&flags).arg(script);
    }
    #[cfg(not(windows))]
    cmd.args(&flags).arg(script);
    cmd.current_dir(cwd);
    #[cfg(target_os = "linux")]
    if let Some(sb) = sandbox
        && active
    {
        let root = crate::permissions::project_root().unwrap_or_else(|| cwd.to_path_buf());
        let home = sandbox_home();
        let home = home.canonical().unwrap_or(home);
        let readable = read_allow_paths(&read_denied_paths(&home, &sb.deny_read));
        // landlock_writes narrows these writable trees around the write guards
        // (git internals, agent config, toolchain bins).
        landlock_writes(
            &mut cmd,
            &writable_paths(&root, sb),
            &readable,
            restricted,
            proxy_port,
        );
    }
    // IPC handles that would let a sandboxed command reach a process outside
    // the sandbox: an SSH or GPG agent, the D-Bus session bus (systemd-run),
    // the X11/Wayland display (keystroke injection into other windows).
    // On macOS Seatbelt also refuses the sockets themselves; on Linux this is
    // all there is, and a command that knows where a socket lives (D-Bus at
    // `/run/user/<uid>/bus`, Docker at `/var/run/docker.sock`) can still
    // connect to it. `XDG_RUNTIME_DIR` goes too: it names where most of them
    // are.
    if active {
        let keep_agent = sandbox.is_some_and(|s| s.allow_ssh_agent);
        for k in [
            "SSH_AUTH_SOCK",
            "SSH_AGENT_PID",
            "DBUS_SESSION_BUS_ADDRESS",
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "GPG_AGENT_INFO",
            "DOCKER_HOST",
            "XAUTHORITY",
            "XDG_RUNTIME_DIR",
        ] {
            if !(keep_agent && k.starts_with("SSH_")) {
                cmd.env_remove(k);
            }
        }
        // Its own temp dir: Seatbelt lets Unix sockets connect only there and
        // in the project.
        if cfg!(target_os = "macos") && sandbox.is_some() {
            cmd.env("TMPDIR", sandbox_tmpdir());
        }
    }
    if let Some(proxy) = &network {
        for name in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            cmd.env(name, proxy.url());
        }
        cmd.env("NO_PROXY", "localhost")
            .env("no_proxy", "localhost");
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.env("CHANTO", "1")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("GIT_EDITOR", "true")
        .env("CLICOLOR", "0")
        .env("NO_COLOR", "1")
        .env("TERM", "dumb");
    // Secret-named variables stay out of commands the model writes.
    if let Some(sc) = secrets {
        for (k, _) in std::env::vars_os() {
            if let Some(k) = k.to_str().filter(|k| crate::secrets::withhold_env(k, sc)) {
                cmd.env_remove(k);
            }
        }
    }
    // Put the command in its own process group so KillShell reaps its whole
    // tree. Sandboxed Windows launchers contain the tree in a Job Object;
    // kill_group stops the launcher (and its job) via taskkill /T.
    //
    // A sandboxed command gets a whole new session (`setsid`) instead, so it
    // has no controlling terminal: it can't open `/dev/tty` to inject input
    // with TIOCSTI or paint fake prompts on Chanto's terminal. setsid also
    // makes it a process-group leader (pgid == pid), so kill_group still reaps
    // the tree. On Linux, seccomp additionally refuses the terminal-injection
    // ioctls. kill_group signals the leader's group; setsid keeps that intact.
    #[cfg(unix)]
    if active {
        // SAFETY: setsid and the seccomp prctl take no locks and don't
        // allocate; they only create a new session and install a BPF filter.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                #[cfg(target_os = "linux")]
                block_tty_injection()?;
                Ok(())
            });
        }
    } else {
        cmd.process_group(0);
    }
    Ok((cmd, network))
}

/// Starts the changed-files section of a Bash display. The TUI shows only
/// its first line until expanded; elsewhere it's an invisible character.
pub const CHANGES_MARK: char = '\u{2063}';

/// Whether files a Bash command changed are shown as a diff:
/// `CHANTO_BASH_DIFF` wins over `bash_diff` in the config (default on;
/// Claude Code's `bashEditDiffEnabled` sets that).
pub fn bash_diff_enabled(config: &crate::config::Config) -> bool {
    match std::env::var("CHANTO_BASH_DIFF")
        .map(|v| v.trim().to_ascii_lowercase())
        .as_deref()
    {
        Ok("0" | "false" | "off" | "no") => false,
        Ok("1" | "true" | "on" | "yes") => true,
        _ => config.bash_diff.unwrap_or(true),
    }
}

/// What a Bash command changed in the project.
pub struct Changes {
    /// For the model: the changed paths only.
    pub note: String,
    /// For people: a summary line, then each file's diff.
    pub display: String,
    /// Text files as (path, before, after), for frontends that show diffs.
    pub texts: Vec<(std::path::PathBuf, Option<String>, String)>,
}

/// Files bigger than this are listed, not diffed.
const DIFF_MAX_BYTES: u64 = 256 * 1024;
const DIFF_MAX_FILES: usize = 20;
const DIFF_FILE_LINES: usize = 40;
const DIFF_TOTAL_LINES: usize = 300;

/// What changed in the project since the snapshot `tree` was taken (the
/// shadow history leaves out ignored files, build outputs and files over its
/// size limit). None when nothing did.
pub fn changed_files(
    shadow: &crate::history::Shadow,
    tree: &str,
    root: &std::path::Path,
) -> Option<Changes> {
    let changed = shadow.changed_blobs(tree).ok()?;
    if changed.is_empty() {
        return None;
    }
    let n = changed.len();
    let names: Vec<&str> = changed.iter().map(|(p, ..)| p.as_str()).collect();
    let list = |max: usize| {
        let mut s = names
            .iter()
            .take(max)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        if n > max {
            s.push_str(&format!(" (+{} more)", n - max));
        }
        s
    };
    let note = format!(
        "\n\n[This command changed {}: {}]",
        crate::util::plural(n, "file"),
        list(DIFF_MAX_FILES)
    );
    let mut display = format!(
        "{CHANGES_MARK}Changed {}: {}",
        crate::util::plural(n, "file"),
        list(3)
    );
    let mut texts = Vec::new();
    let mut lines = 0;
    for (i, (path, before, after, link)) in changed.iter().enumerate() {
        let load = |b: &Option<String>| -> Result<Option<String>, &'static str> {
            let Some(id) = b else { return Ok(None) };
            match shadow.blob(id, DIFF_MAX_BYTES) {
                Ok(Some(bytes)) if !bytes.contains(&0) => {
                    String::from_utf8(bytes).map(Some).map_err(|_| "binary")
                }
                Ok(Some(_)) => Err("binary"),
                Ok(None) => Err("too large to show"),
                Err(_) => Err("not shown"),
            }
        };
        let tag = match (before, after) {
            (None, _) => " (new)",
            (_, None) => " (deleted)",
            _ => "",
        };
        display.push_str(&format!("\n{}", crate::ui::bold(&format!("{path}{tag}"))));
        if i >= DIFF_MAX_FILES || lines >= DIFF_TOTAL_LINES {
            continue;
        }
        if *link {
            display.push_str(&format!(" {}", crate::ui::dim("(symlink)")));
            continue;
        }
        match (load(before), load(after)) {
            (Ok(old), Ok(new)) => {
                let new = new.unwrap_or_default();
                let d = crate::ui::diff(
                    old.as_deref().unwrap_or(""),
                    &new,
                    DIFF_FILE_LINES.min(DIFF_TOTAL_LINES - lines),
                );
                lines += d.lines().count();
                display.push('\n');
                display.push_str(d.trim_end());
                texts.push((root.join(path), old, new));
            }
            (Err(why), _) | (_, Err(why)) => {
                display.push_str(&format!(" {}", crate::ui::dim(&format!("({why})"))))
            }
        }
    }
    Some(Changes {
        note,
        display,
        texts,
    })
}

pub struct Bash;

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> String {
        "Bash".into()
    }
    fn description(&self) -> String {
        "Execute a bash command. The working directory persists between calls (cd works). stdout and stderr \
are combined. Default timeout 120s (max 600s); a command that outlives its timeout is not killed, it moves \
to the background (read it with BashOutput). Set run_in_background=true for long-running processes \
(dev servers, watchers) and poll them with BashOutput. Quote paths with spaces. Do not use this for \
reading/searching files (use Read/Grep/Glob) or editing files (use Edit/Write). Avoid interactive \
commands (they get no stdin); pass flags like --yes or -y. Chain dependent commands with &&; make \
independent calls in parallel."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "description": {"type": "string", "description": "5-10 word description of what this does"},
                "timeout": {"type": "integer", "description": "Timeout in milliseconds (max 600000)"},
                "run_in_background": {"type": "boolean"},
                "dangerouslyDisableSandbox": {"type": "boolean", "description": "Run outside the sandbox. Use only after a command failed because the sandbox blocked it (writing outside the project, reading private keys); it needs the user's approval."}
            },
            "required": ["command"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Execute
    }
    fn rule_subjects(&self, args: &Value) -> Vec<String> {
        crate::permissions::split_shell(args["command"].as_str().unwrap_or(""))
    }
    fn label(&self, args: &Value) -> String {
        format!(
            "Bash({})",
            crate::util::one_line(args["command"].as_str().unwrap_or(""), 100)
        )
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let command = arg_str(&args, "command")?.to_string();
        let description = args["description"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| crate::util::one_line(s, 100));
        let cwd = ctx.cwd();
        let unsandboxed = arg_bool(&args, "dangerouslyDisableSandbox");
        if let Some(n) = nested_sandbox_notice() {
            ctx.env.emit(crate::agent::Event::Warn(n.into()));
        }
        crate::shell::snapshot::ensure(&ctx.env).await;
        if arg_bool(&args, "run_in_background") {
            return start_background(ctx, &command, &cwd, unsandboxed, description).await;
        }
        if let Some(secs) = leading_sleep(&command).filter(|s| *s >= 10.0) {
            return Ok(ToolOutput::err(format!(
                "Blocked: a foreground `sleep {secs}` only uses up time. To wait for a background command, call BashOutput with `wait_secs`; \
                 to wait for something else, poll for it in one command (`until <check>; do sleep 2; done`) with a timeout."
            )));
        }
        let timeout_ms = arg_u64(&args, "timeout")
            .unwrap_or(120_000)
            .clamp(1000, 600_000);
        // In a run's container the cwd file must be somewhere both sides see.
        let tmp = crate::runs::container::for_path(&cwd)
            .map(|c| c.scratch.clone())
            .unwrap_or_else(std::env::temp_dir);
        let cwd_file = tmp.join(format!("chanto-cwd-{}", uuid::Uuid::new_v4().simple()));
        let script = wrap_command(
            crate::shell::tool_flavor(),
            &env_prelude(ctx, &cwd),
            &command,
            &cwd_file,
        );
        // Bounded concurrency across agents, and don't pile onto a machine that's out of memory.
        let _slot = crate::resources::command_slot().await;
        if let Err(e) =
            crate::resources::wait_for_memory("this command", Duration::from_secs(30)).await
        {
            return Ok(ToolOutput::err(e));
        }
        let (mut cmd, network, exec) = build_command_with(
            &script,
            &cwd,
            Some(&ctx.env.config.sandbox),
            Some(&ctx.env.config.secrets),
            unsandboxed,
        )?;
        apply_env_vars(ctx, &cwd, unsandboxed, &mut cmd);
        #[cfg(target_os = "linux")]
        let config_guard = (sandbox_on() && !unsandboxed && exec.is_none()).then(|| {
            config_guard::ConfigGuard::take(
                &crate::permissions::project_root().unwrap_or_else(|| cwd.clone()),
            )
        });
        let mut child = cmd.spawn()?;
        let pid = child.id();
        let mut reaper = KillOnDrop(pid);
        if let Some((c, token)) = &exec {
            crate::runs::container::track(pid, c, token);
        }
        let mut so = child.stdout.take().unwrap();
        let mut se = child.stderr.take().unwrap();
        // Interleave stdout/stderr into one buffer as data arrives.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = Arc::new(Mutex::new(Sink::Fg));
        let (b1, b2) = (buf.clone(), buf.clone());
        let (s1, s2) = (sink.clone(), sink.clone());
        let r1 = tokio::spawn(async move {
            let mut chunk = [0u8; 8192];
            while let Ok(n) = so.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                push_sink(&s1, &b1, &chunk[..n]);
            }
        });
        let r2 = tokio::spawn(async move {
            let mut chunk = [0u8; 8192];
            while let Ok(n) = se.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                push_sink(&s2, &b2, &chunk[..n]);
            }
        });
        let t0 = std::time::Instant::now();
        let b3 = buf.clone();
        let progress = async {
            let mut last_len = 0;
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let snapshot = {
                    let b = b3.lock().unwrap();
                    if b.len() == last_len {
                        continue;
                    }
                    last_len = b.len();
                    let start = b.len().saturating_sub(4000);
                    String::from_utf8_lossy(&b[start..]).to_string()
                };
                let clean = strip_ansi(&snapshot);
                let tail: Vec<&str> = clean.lines().rev().take(5).collect();
                ctx.progress(tail.into_iter().rev().collect::<Vec<_>>().join("\n"));
            }
        };
        let status = tokio::select! {
            s = child.wait() => Some(s?),
            _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => None,
            _ = ctx.cancel.cancelled() => None,
            _ = progress => None,
        };
        // From here the command has exited, is killed below, or moves to the background.
        reaper.0 = None;
        if status.is_none()
            && !ctx.cancel.is_cancelled()
            && !pid.is_some_and(crate::resources::was_killed)
            && !scans_everything(&command)
        {
            // The command outlived its timeout but is still alive, so don't throw
            // its work away: hand the running process to the background the way a
            // foreground command moves to the background at its timeout. Taking
            // the sink lock means no reader is mid-write as we snapshot, and the
            // switch is atomic with that snapshot.
            let mut guard = sink.lock().unwrap();
            let text = strip_ansi(&String::from_utf8_lossy(&buf.lock().unwrap()));
            let id = format!("bash_{}", ctx.env.next_shell_id());
            let shell = Arc::new(BgShell {
                id: id.clone(),
                command: command.clone(),
                description,
                monitor: None,
                output: Mutex::new(text.clone()),
                read_pos: Mutex::new(text.len()),
                watch_pos: Mutex::new(text.len()),
                exit: Mutex::new(None),
                pid,
                network: Mutex::new(network),
            });
            *guard = Sink::Bg(shell.clone());
            drop(guard);
            ctx.env
                .shells
                .lock()
                .unwrap()
                .insert(id.clone(), shell.clone());
            let cf = cwd_file.clone();
            let title = shell.title();
            tokio::spawn(async move {
                let code = child.wait().await.ok().and_then(|s| s.code());
                drain_output(r1, r2).await;
                if exec.is_some() {
                    crate::runs::container::untrack(shell.pid);
                }
                *shell.exit.lock().unwrap() = Some(code.unwrap_or(-1));
                let _ = std::fs::remove_file(&cf);
            });
            let mut out = crate::util::truncate_middle(text.trim_end(), MAX_OUTPUT);
            if out.trim().is_empty() {
                out = "(no output yet)".into();
            }
            out.push_str(&format!(
                "\n[command timed out after {}s; it was not killed, it is still running in the background as {id}. \
                 Read new output with BashOutput bash_id=\"{id}\", or stop it with KillShell.]",
                timeout_ms / 1000
            ));
            return Ok(ToolOutput {
                content: out,
                is_error: true,
                display: Some(format!("Moved to background · {id}\n{title}")),
                images: vec![],
            });
        }
        if status.is_none() {
            if let Some(p) = pid {
                kill_group(p);
            }
            let _ = child.kill().await;
        }
        drain_output(r1, r2).await;
        crate::runs::container::untrack(pid);
        let raw = buf.lock().unwrap().clone();
        let text = strip_ansi(&String::from_utf8_lossy(&raw));
        let mut out = crate::util::truncate_middle(text.trim_end(), MAX_OUTPUT);
        if pid.is_some_and(crate::resources::was_killed) {
            out.push_str("\n\n[Killed by chanto's memory watchdog: this command used too much memory. Use less parallelism (e.g. -j2), a smaller input, or run it in pieces.]");
        }
        let mut cwd_reset = None;
        if let Ok(newcwd) = std::fs::read_to_string(&cwd_file) {
            let newcwd = std::path::PathBuf::from(newcwd.trim());
            // An isolated (worktree) agent's `cd` must not move the main session.
            if newcwd.is_dir() && ctx.cwd_override.is_none() {
                let root = ctx.env.project();
                let (newcwd, reset) = settle_cwd(
                    &newcwd,
                    &root,
                    &crate::permissions::extra_dirs(),
                    maintain_project_dir(),
                );
                if reset {
                    cwd_reset = Some(root);
                }
                if newcwd != cwd {
                    ctx.env.set_cwd(newcwd.clone());
                    if ctx.depth == 0 {
                        crate::hooks::cwd_changed(&ctx.env, &cwd, &newcwd).await;
                    }
                }
            }
        }
        let _ = std::fs::remove_file(&cwd_file);
        #[cfg(target_os = "linux")]
        if let Some(note) = config_guard.and_then(|g| g.restore()) {
            out.push_str(&note);
        }
        let elapsed = t0.elapsed();
        let code = status.as_ref().and_then(|s| s.code());
        let mut is_error;
        match status {
            None if ctx.cancel.is_cancelled() => {
                out.push_str("\n[interrupted by user]");
                is_error = true;
            }
            None => {
                out.push_str(&format!(
                    "\n[command timed out after {}s and was killed; use run_in_background for long-running processes]",
                    timeout_ms / 1000
                ));
                if scans_everything(&command) {
                    out.push_str("\n[searching the whole disk or home directory takes minutes and reads the user's other files: search the project, or one specific directory, instead]");
                }
                is_error = true;
            }
            Some(s) => {
                is_error = !s.success();
                if !s.success() {
                    out.push_str(&format!(
                        "\n[exit code {}]",
                        code.map(|c| c.to_string())
                            .unwrap_or_else(|| "signal".into())
                    ));
                    if sandbox_on() && !unsandboxed && exec.is_none() && looks_sandbox_blocked(&out)
                    {
                        out.push_str(if cfg!(windows) {
                            "\n[This may have been blocked by the sandbox: commands can write only inside the project and temp dir. \
                             If it really needs that, run it again with dangerouslyDisableSandbox: true; the user will be asked to approve.]"
                        } else {
                            "\n[This may have been blocked by the sandbox: commands can write only inside the project, temp dirs and package \
                             download caches, can't change git hooks or config, agent or editor settings, and can't read private keys or \
                             credential files. Installing a toolchain or tool, `git init`/`clone`/`worktree add`/`submodule`/`config` and \
                             git over SSH or with a saved login (`git push`, `gh`) need to run outside it: run it again with dangerouslyDisableSandbox: true, and the user will be \
                             asked to approve.]"
                        });
                    } else if sandbox_on()
                        && !unsandboxed
                        && exec.is_none()
                        && !cfg!(windows)
                        && ctx.env.config.sandbox.network_policy()
                            == crate::config::NetworkPolicy::Deny
                        && looks_network_failure(&out)
                    {
                        // Otherwise it reads like the server is down.
                        out.push_str(
                            "\n[Network access is off for shell commands (sandbox network = \"deny\"), which is why this couldn't connect. \
                             If it really needs the network, run it again with dangerouslyDisableSandbox: true; the user will be asked to approve.]",
                        );
                    }
                }
            }
        }
        if let Some(network) = &network {
            let blocked = network.take_blocked();
            if !blocked.is_empty() {
                is_error = true;
            }
            drop(_slot);
            out.push_str(&report_network_blocks(blocked, ctx).await);
        }
        if out.trim().is_empty() {
            out = "(no output)".into();
        }
        if let Some(root) = cwd_reset {
            out.push_str(&format!("\nShell cwd was reset to {}", root.display()));
        }
        let n = out.lines().count();
        let preview: Vec<&str> = out.lines().take(4).collect();
        let mut display = preview.join("\n");
        if n > 4 {
            display.push_str(&format!("\n… +{}", crate::util::plural(n - 4, "line")));
        }
        if elapsed.as_secs() >= 5 {
            display.push_str(&format!(" ({:.0}s)", elapsed.as_secs_f32()));
        }
        Ok(ToolOutput {
            content: out,
            is_error,
            display: Some(display),
            images: vec![],
        })
    }
}

#[cfg(test)]
#[test]
fn a_missing_login_points_at_the_sandbox_on_macos() {
    for out in [
        "fatal: could not read Username for 'https://github.com'",
        "fatal: terminal prompts disabled",
        "To get started with GitHub CLI, please run: gh auth login",
        "error: keychain not found",
    ] {
        assert_eq!(
            looks_sandbox_blocked(out),
            cfg!(target_os = "macos"),
            "{out}"
        );
    }
    assert!(!looks_sandbox_blocked("fatal: not a git repository"));
}

#[cfg(test)]
#[path = "bash/output_tests.rs"]
mod output_tests;

#[cfg(test)]
mod wrap_tests;

/// Prints the shell's directory as a path Chanto can open: Git Bash's
/// `pwd -P` says `/c/Users/…`, which Windows can't resolve, so ask for `C:/Users/…`.
#[cfg(windows)]
const PWD: &str = "{ pwd -W || pwd -P; }";
#[cfg(not(windows))]
const PWD: &str = "pwd -P";

/// `command` as a script that keeps its exit status and leaves the directory
/// it ended in at `cwd_file`. cmd.exe runs only one line through `/C`, so a
/// command there can't report its directory.
fn wrap_command(
    flavor: crate::shell::Flavor,
    prelude: &str,
    command: &str,
    cwd_file: &std::path::Path,
) -> String {
    use crate::shell::{Flavor, powershell_quote};
    let file = cwd_file.display();
    match flavor {
        Flavor::Posix => {
            format!("{prelude}{command}\n__hx_ec=$?\n{PWD} > '{file}' 2>/dev/null\nexit $__hx_ec")
        }
        // `$?` first: anything after it resets it. `$LASTEXITCODE` alone would
        // miss a failed cmdlet and repeat an earlier program's status.
        Flavor::PowerShell => format!(
            "{prelude}{command}\n$__hx_ok = $?; $__hx_ec = $LASTEXITCODE\n\
             try {{ [IO.File]::WriteAllText({}, (Get-Location).ProviderPath) }} catch {{}}\n\
             if ($__hx_ok) {{ exit 0 }} elseif ($__hx_ec) {{ exit $__hx_ec }} else {{ exit 1 }}",
            powershell_quote(&file.to_string())
        ),
        Flavor::Cmd => format!("{prelude}{command}"),
    }
}

/// Where the shell stays after a command that ended in `new`: there if it's
/// inside the project or an added directory (or back at `root` when
/// `maintain` is set), otherwise back at `root`. The flag says it was outside.
fn settle_cwd(
    new: &std::path::Path,
    root: &std::path::Path,
    extra: &[std::path::PathBuf],
    maintain: bool,
) -> (std::path::PathBuf, bool) {
    let n = crate::permissions::normalize(new);
    let inside = n.starts_with(root) || extra.iter().any(|d| n.starts_with(d));
    if !inside {
        (root.to_path_buf(), true)
    } else if maintain {
        (root.to_path_buf(), false)
    } else {
        (new.to_path_buf(), false)
    }
}

/// `CLAUDE_BASH_MAINTAIN_PROJECT_WORKING_DIR` (or `CHANTO_BASH_MAINTAIN_PROJECT_WORKING_DIR`):
/// every Bash command starts at the project root; `cd` doesn't carry over.
fn maintain_project_dir() -> bool {
    [
        "CHANTO_BASH_MAINTAIN_PROJECT_WORKING_DIR",
        "CLAUDE_BASH_MAINTAIN_PROJECT_WORKING_DIR",
    ]
    .iter()
    .any(|v| {
        std::env::var(v).is_ok_and(|s| {
            matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
    })
}

/// What runs before the command: the user's shell snapshot (see
/// [`crate::shell::snapshot`]), then the session's `CLAUDE_ENV_FILE`s, so
/// they can build on it. Neither inside a background run's container,
/// which can't see the files.
fn env_prelude(ctx: &Ctx, cwd: &std::path::Path) -> String {
    if crate::runs::container::for_path(cwd).is_some()
        || crate::shell::tool_flavor() != crate::shell::Flavor::Posix
    {
        return String::new();
    }
    format!(
        "{}{}",
        crate::shell::snapshot::prelude(&ctx.env),
        crate::hooks::bash_prelude(&ctx.env)
    )
}

/// PowerShell / cmd can't run the env files: copy their plain assignments
/// into the command's environment instead (not when sandboxed).
fn apply_env_vars(
    ctx: &Ctx,
    cwd: &std::path::Path,
    unsandboxed: bool,
    cmd: &mut tokio::process::Command,
) {
    if crate::runs::container::for_path(cwd).is_some() || (sandbox_on() && !unsandboxed) {
        return;
    }
    for (k, v) in crate::hooks::plain_env_vars(&ctx.env) {
        cmd.env(k, v);
    }
}

async fn start_background(
    ctx: &Ctx,
    command: &str,
    cwd: &std::path::Path,
    unsandboxed: bool,
    description: Option<String>,
) -> Result<ToolOutput> {
    if let Err(e) =
        crate::resources::wait_for_memory("this background command", Duration::from_secs(30)).await
    {
        return Ok(ToolOutput::err(e));
    }
    let id = format!("bash_{}", ctx.env.next_shell_id());
    let shell = spawn_background(ctx, &id, command, cwd, unsandboxed, None, description)?;
    Ok(ToolOutput::ok(format!(
        "Started background shell {id}. Use BashOutput with bash_id=\"{id}\" to read its output, KillShell to stop it."
    ))
    .display(format!("Started in background · {id}\n{}", shell.title())))
}

/// Start `command` as background shell `id` (registered in `env.shells`, so
/// BashOutput, KillShell, `/bashes` and the TUI's panel see it). `monitor`
/// is the Monitor tool's description of the watch.
pub(crate) fn spawn_background(
    ctx: &Ctx,
    id: &str,
    command: &str,
    cwd: &std::path::Path,
    unsandboxed: bool,
    monitor: Option<String>,
    description: Option<String>,
) -> Result<Arc<BgShell>> {
    let id = id.to_string();
    let script = format!("{}{command}", env_prelude(ctx, cwd));
    let (mut cmd, network, exec) = build_command_with(
        &script,
        cwd,
        Some(&ctx.env.config.sandbox),
        Some(&ctx.env.config.secrets),
        unsandboxed,
    )?;
    apply_env_vars(ctx, cwd, unsandboxed, &mut cmd);
    #[cfg(target_os = "linux")]
    let config_guard = (sandbox_on() && !unsandboxed && exec.is_none()).then(|| {
        config_guard::ConfigGuard::take(
            &crate::permissions::project_root().unwrap_or_else(|| cwd.to_path_buf()),
        )
    });
    let mut child = cmd.spawn()?;
    if let Some((c, token)) = &exec {
        crate::runs::container::track(child.id(), c, token);
    }
    let shell = Arc::new(BgShell {
        id: id.clone(),
        command: command.to_string(),
        description,
        monitor,
        output: Mutex::new(String::new()),
        read_pos: Mutex::new(0),
        watch_pos: Mutex::new(0),
        exit: Mutex::new(None),
        pid: child.id(),
        network: Mutex::new(network),
    });
    let mut so = child.stdout.take().unwrap();
    let mut se = child.stderr.take().unwrap();
    let s1 = shell.clone();
    let r1 = tokio::spawn(async move {
        let mut chunk = [0u8; 8192];
        while let Ok(n) = so.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            push_capped(&s1, &strip_ansi(&String::from_utf8_lossy(&chunk[..n])));
        }
    });
    let s2 = shell.clone();
    let r2 = tokio::spawn(async move {
        let mut chunk = [0u8; 8192];
        while let Ok(n) = se.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            push_capped(&s2, &strip_ansi(&String::from_utf8_lossy(&chunk[..n])));
        }
    });
    let s3 = shell.clone();
    tokio::spawn(async move {
        let status = child.wait().await;
        drain_output(r1, r2).await;
        #[cfg(target_os = "linux")]
        if let Some(note) = config_guard.and_then(|g| g.restore()) {
            push_capped(&s3, &note);
        }
        if exec.is_some() {
            crate::runs::container::untrack(s3.pid);
        }
        *s3.exit.lock().unwrap() = Some(status.ok().and_then(|s| s.code()).unwrap_or(-1));
    });
    ctx.env.shells.lock().unwrap().insert(id, shell.clone());
    Ok(shell)
}

// Collect trailing pipe data before publishing completion. Descendants can keep
// inherited pipes open, so use the same bounded drain as foreground commands.
async fn drain_output(r1: tokio::task::JoinHandle<()>, r2: tokio::task::JoinHandle<()>) {
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let _ = r1.await;
        let _ = r2.await;
    })
    .await;
}

fn push_capped(sh: &BgShell, s: &str) {
    let mut o = sh.output.lock().unwrap();
    o.push_str(s);
    // Cap memory at ~2MB; keep the tail and shift the read cursor accordingly.
    if o.len() > 2_000_000 {
        let mut cut = o.len() - 1_000_000;
        while !o.is_char_boundary(cut) {
            cut += 1;
        }
        o.drain(..cut);
        let mut pos = sh.read_pos.lock().unwrap();
        *pos = pos.saturating_sub(cut);
        let mut watched = sh.watch_pos.lock().unwrap();
        *watched = watched.saturating_sub(cut);
    }
}

pub fn strip_ansi(s: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07").unwrap()
    });
    let s = re.replace_all(s, "");
    if !s.contains('\r') {
        return s.into_owned();
    }
    // Carriage-return progress output: keep what the terminal would show.
    s.split('\n')
        .map(|l| {
            let l = l.strip_suffix('\r').unwrap_or(l);
            l.rsplit('\r').next().unwrap_or(l)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub struct BashOutput;

/// How long unfiltered output must stay quiet before `wait_secs` returns it.
#[cfg(not(test))]
const SETTLE: Duration = Duration::from_secs(5);
#[cfg(test)]
const SETTLE: Duration = Duration::from_millis(300);

#[async_trait]
impl Tool for BashOutput {
    fn name(&self) -> String {
        "BashOutput".into()
    }
    fn description(&self) -> String {
        "Read new output from a background shell started with run_in_background. Returns only output produced since the last read, plus status. Optional `filter` regex keeps matching lines only. Optional `wait_secs` (max 600) waits instead of sleeping: until the shell exits, new output has stopped arriving for a few seconds, or (with `filter`) a matching line appears. One call with a long `wait_secs` is enough for a test run or build; don't poll.".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "bash_id": {"type": "string"},
                "filter": {"type": "string"},
                "wait_secs": {"type": "integer", "description": "Wait up to this long for new output or the exit (max 600)"}
            },
            "required": ["bash_id"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }
    fn label(&self, args: &Value) -> String {
        format!("BashOutput({})", args["bash_id"].as_str().unwrap_or(""))
    }
    fn label_in(&self, args: &Value, env: &crate::agent::Env) -> String {
        let id = args["bash_id"].as_str().unwrap_or("");
        let shell = env.shells.lock().unwrap().get(id).cloned();
        shell.map_or_else(
            || self.label(args),
            |sh| format!("BashOutput({})", sh.title()),
        )
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let id = arg_str(&args, "bash_id")?;
        let Some(sh) = ctx.env.shells.lock().unwrap().get(id).cloned() else {
            return Ok(ToolOutput::err(format!("No background shell with id {id}")));
        };
        let filter = args["filter"]
            .as_str()
            .and_then(|f| regex::Regex::new(f).ok());
        if let Some(secs) = arg_u64(&args, "wait_secs").filter(|s| *s > 0) {
            let waiting = if filter.is_some() {
                "Waiting for matching output"
            } else {
                "Waiting for output to settle"
            };
            ctx.progress(format!("{waiting} (up to {}s)", secs.min(600)));
            let deadline = std::time::Instant::now() + Duration::from_secs(secs.min(600));
            // Without a filter, return once new output has stopped arriving
            // for a moment, not on its first byte: a test run printing a dot
            // per test otherwise ends every wait at once, and each return
            // costs the model another call.
            let mut seen = sh.output.lock().unwrap().len();
            let mut changed = std::time::Instant::now();
            loop {
                if sh.exit.lock().unwrap().is_some()
                    || ctx.cancel.is_cancelled()
                    || std::time::Instant::now() >= deadline
                {
                    break;
                }
                let fresh = {
                    let out = sh.output.lock().unwrap();
                    let pos = (*sh.read_pos.lock().unwrap()).min(out.len());
                    let mut start = pos;
                    while !out.is_char_boundary(start) {
                        start -= 1;
                    }
                    match &filter {
                        Some(re) => out[start..].lines().any(|l| re.is_match(l)),
                        None => {
                            if out.len() != seen {
                                seen = out.len();
                                changed = std::time::Instant::now();
                            }
                            out.len() > pos && changed.elapsed() >= SETTLE
                        }
                    }
                };
                if fresh {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        let blocked = sh
            .network
            .lock()
            .unwrap()
            .as_ref()
            .map(|n| n.take_blocked())
            .unwrap_or_default();
        let network_error = !blocked.is_empty();
        let network_report = report_network_blocks(blocked, ctx).await;
        if sh.exit.lock().unwrap().is_some() {
            sh.network.lock().unwrap().take();
        }
        let out = sh.output.lock().unwrap().clone();
        let mut pos = sh.read_pos.lock().unwrap();
        let mut start = (*pos).min(out.len());
        while !out.is_char_boundary(start) {
            start -= 1;
        }
        let mut new = out[start..].to_string();
        *pos = out.len();
        if let Some(re) = &filter {
            new = new
                .lines()
                .filter(|l| re.is_match(l))
                .collect::<Vec<_>>()
                .join("\n");
        }
        let new_lines = new.lines().count();
        new.push_str(&network_report);
        let exit = *sh.exit.lock().unwrap();
        let status = match exit {
            Some(c) => format!("exited with code {c}"),
            None => "running".into(),
        };
        let body = if new.trim().is_empty() {
            "(no new output)".to_string()
        } else {
            crate::util::truncate_middle(&new, MAX_OUTPUT)
        };
        let human_status = match exit {
            Some(0) => "Completed · exit 0".to_string(),
            Some(-1) => "Stopped".to_string(),
            Some(code) => format!("Failed · exit {code}"),
            None => "Still running".to_string(),
        };
        let activity = if new_lines == 0 {
            if filter.is_some() {
                "no new matching output".to_string()
            } else {
                "no new output".to_string()
            }
        } else {
            crate::util::plural(new_lines, "new line")
        };
        let mut display = format!("{human_status} · {activity}");
        if new_lines > 0 || network_error {
            let tail: Vec<_> = new
                .lines()
                .rev()
                .take(3)
                .map(|line| crate::util::one_line(line, 120))
                .collect();
            display.push('\n');
            display.push_str(&tail.into_iter().rev().collect::<Vec<_>>().join("\n"));
        }
        let mut result = ToolOutput::ok(format!(
            "<status>{status}</status>\n<output>\n{body}\n</output>"
        ))
        .display(display);
        result.is_error = network_error || exit.is_some_and(|code| code != 0);
        Ok(result)
    }
}

/// A `find` (or similar) over `/` or the home directory.
fn scans_everything(command: &str) -> bool {
    let words: Vec<&str> = command.split_whitespace().collect();
    words.windows(2).any(|w| {
        matches!(w[0], "find" | "fd" | "locate" | "mdfind" | "du")
            && matches!(
                w[1].trim_matches(|c| c == '"' || c == '\''),
                "/" | "~" | "~/" | "$HOME" | "${HOME}" | "/Users" | "/home"
            )
    }) || command.contains("glob.glob('/")
        || command.contains("glob.glob(\"/")
}

/// Seconds of a `sleep N` the command starts with (alone, or before `&&`/`;`).
pub(crate) fn leading_sleep(command: &str) -> Option<f64> {
    let rest = command.trim_start().strip_prefix("sleep ")?.trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == 's'))
        .unwrap_or(rest.len());
    let (n, after) = rest.split_at(end);
    let after = after.trim_start();
    if !(after.is_empty() || after.starts_with("&&") || after.starts_with(';')) {
        return None;
    }
    let n = n.strip_suffix('s').unwrap_or(n);
    n.parse().ok()
}

pub struct KillShell;

#[async_trait]
impl Tool for KillShell {
    fn name(&self) -> String {
        "KillShell".into()
    }
    fn description(&self) -> String {
        "Kill a background shell by id.".into()
    }
    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {"shell_id": {"type": "string"}}, "required": ["shell_id"]})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }
    fn label(&self, args: &Value) -> String {
        format!("KillShell({})", args["shell_id"].as_str().unwrap_or(""))
    }
    fn label_in(&self, args: &Value, env: &crate::agent::Env) -> String {
        let id = args["shell_id"].as_str().unwrap_or("");
        let shell = env.shells.lock().unwrap().get(id).cloned();
        shell.map_or_else(
            || self.label(args),
            |sh| format!("KillShell({})", sh.title()),
        )
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let id = args["shell_id"]
            .as_str()
            .or(args["bash_id"].as_str())
            .unwrap_or("");
        let sh = ctx.env.shells.lock().unwrap().remove(id);
        match sh {
            Some(sh) => {
                if let Some(p) = sh.pid {
                    kill_group(p);
                }
                Ok(ToolOutput::ok(format!("Killed {id}")))
            }
            None => Ok(ToolOutput::err(format!("No background shell with id {id}"))),
        }
    }
}

pub fn kill_all(env: &crate::agent::Env) {
    for (_, sh) in env.shells.lock().unwrap().drain() {
        if let Some(p) = sh.pid {
            kill_group(p);
        }
    }
}

#[cfg(test)]
mod cwd_tests;

#[cfg(test)]
mod profile_tests;

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
fn test_sandbox_available() -> bool {
    if !sandbox_available() {
        return false;
    }
    #[cfg(target_os = "macos")]
    return std::process::Command::new("/usr/bin/sandbox-exec")
        .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
        .output()
        .is_ok_and(|o| o.status.success());
    #[cfg(not(target_os = "macos"))]
    true
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod sandbox_tests;

#[cfg(test)]
mod network_tests;

#[cfg(all(test, windows))]
mod windows_secret_tests;

#[cfg(test)]
mod sink_tests;

// The libc calls stay in this file with the sandbox's other FFI; guards.rs
// uses the result to deny writes to Chanto's own terminal.
/// The terminal devices Chanto's own stdin, stdout and stderr are on.
#[cfg(not(unix))]
pub(super) fn own_terminals() -> Vec<std::path::PathBuf> {
    Vec::new()
}

/// The terminal devices Chanto's own stdin, stdout and stderr are on.
#[cfg(unix)]
pub(super) fn own_terminals() -> Vec<std::path::PathBuf> {
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    for fd in 0..3 {
        let mut buf = [0 as libc::c_char; 256];
        // SAFETY: the buffer is valid for its length; ttyname_r NUL-terminates.
        if unsafe { libc::isatty(fd) } == 1
            && unsafe { libc::ttyname_r(fd, buf.as_mut_ptr(), buf.len()) } == 0
        {
            // SAFETY: ttyname_r succeeded, so `buf` holds a C string.
            let s = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
            let p = std::path::PathBuf::from(s.to_string_lossy().into_owned());
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}
