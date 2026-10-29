//! The OS sandbox for shell commands: what they may write and read, the
//! macOS Seatbelt profile, and whether a sandbox can start here.

use super::guards::{norm, seatbelt_guards, write_guards};
use super::*;

/// The package managers' download caches sandboxed builds may fill.
///
/// One policy for the whole class: what a package manager downloads and
/// unpacks (crates and their extracted sources, Go modules, Maven and Gradle
/// jars, npm, pip and uv caches, Playwright's browsers) is writable, like the
/// project's own `node_modules` or `.venv`. A command can tamper with it, but
/// no more than with the dependencies vendored in the project. What is not
/// a download stays out: toolchains (`~/.rustup`, pyenv, uv's Pythons),
/// installed tools (`~/.cargo/bin`, `~/.local/bin`), package-manager settings
/// and init scripts, and the rest of `~/.cache` and `~/Library/Caches`
/// (shell prompt caches, pre-commit and poetry environments, other apps'
/// state). Installing one of those needs an unsandboxed run, which asks.
const UNIX_CACHES: &[&str] = &[
    // XDG caches (Linux; also used on macOS by several tools).
    ".cache/pip",
    ".cache/uv",
    ".cache/pnpm",
    ".cache/yarn",
    ".cache/deno",
    ".cache/node-gyp",
    ".cache/go-build",
    ".cache/ms-playwright",
    ".cache/Homebrew",
    ".cache/typescript",
    ".cache/prisma",
    // macOS caches.
    "Library/Caches/pip",
    "Library/Caches/pnpm",
    "Library/Caches/Yarn",
    "Library/Caches/deno",
    "Library/Caches/node-gyp",
    "Library/Caches/go-build",
    "Library/Caches/ms-playwright",
    "Library/Caches/Homebrew",
    "Library/Caches/typescript",
    "Library/Caches/org.swift.swiftpm",
    "Library/Caches/CocoaPods",
    // Package managers' own trees.
    ".npm/_cacache",
    ".npm/_logs",
    ".npm/_npx",
    ".pnpm-store",
    ".yarn/berry/cache",
    ".bun/install/cache",
    ".cargo/registry",
    ".cargo/git",
    ".cargo/advisory-dbs",
    // Cargo's lock files and its last-use database (with SQLite's sidecars).
    ".cargo/.package-cache",
    ".cargo/.package-cache-mutate",
    ".cargo/.global-cache",
    ".cargo/.global-cache-journal",
    ".cargo/.global-cache-wal",
    ".cargo/.global-cache-shm",
    ".gradle/caches",
    ".gradle/daemon",
    ".gradle/native",
    ".gradle/.tmp",
    ".gradle/notifications",
    ".gradle/kotlin-profile",
    ".gradle/build-scan-data",
    ".m2/repository",
    "go/pkg",
];

/// Where sandboxed commands may write: the project, temp dirs, devices and
/// download caches, plus configured and --add-dir paths.
pub fn writable_paths(cwd: &std::path::Path, cfg: &crate::config::SandboxConfig) -> Vec<String> {
    let home = sandbox_home();
    let mut writable: Vec<String> = vec![cwd.display().to_string()];
    if !cfg!(windows) {
        writable.push("/dev".into());
    }
    if cfg!(target_os = "macos") {
        writable.extend([
            "/private/tmp".to_string(),
            "/private/var/folders".to_string(),
        ]);
    } else if cfg!(windows) {
        writable.push(std::env::temp_dir().display().to_string());
    } else {
        writable.extend([
            "/tmp".to_string(),
            "/var/tmp".to_string(),
            "/dev/shm".to_string(),
        ]);
        if let Ok(t) = std::env::var("TMPDIR") {
            writable.push(t);
        }
    }
    if cfg!(windows) {
        writable.extend(windows_caches(&home));
    } else {
        for rel in UNIX_CACHES {
            writable.push(home.join(rel).display().to_string());
        }
    }
    for p in &cfg.allow_write {
        writable.push(crate::util::resolve(cwd, p).display().to_string());
    }
    writable.extend(
        crate::permissions::extra_dirs()
            .iter()
            .map(|d| d.display().to_string()),
    );
    writable
}

/// The package managers' caches on Windows, as the macOS and Linux sandboxes
/// allow theirs: some live in the profile, most under %LOCALAPPDATA%.
pub(super) fn windows_caches(home: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = [
        ".cargo\\registry",
        ".cargo\\git",
        ".cargo\\.package-cache",
        ".cargo\\.package-cache-mutate",
        ".cargo\\.global-cache",
        ".gradle\\caches",
        ".m2\\repository",
        ".bun\\install\\cache",
        "go\\pkg",
        ".nuget\\packages",
    ]
    .iter()
    .map(|r| home.join(r).display().to_string())
    .collect();
    if let Some(local) = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from) {
        for rel in [
            "npm-cache",
            "pnpm",
            "pnpm-cache",
            "Yarn\\Cache",
            "pip\\Cache",
            "uv\\cache",
            "go-build",
            "pypoetry\\Cache",
        ] {
            out.push(local.join(rel).display().to_string());
        }
    }
    out
}

/// Windows: grant the sandbox's write access to the tool caches in the
/// background at startup. The first grant on a big cache (~/.cargo) spreads
/// to every file in it and can take a while; done here, a command doesn't wait
/// for it. Later grants only check the existing entry.
pub fn prime_sandbox_caches(cfg: &crate::config::SandboxConfig) {
    #[cfg(windows)]
    if cfg.enabled {
        let caches: Vec<std::path::PathBuf> = windows_caches(&sandbox_home())
            .into_iter()
            .map(std::path::PathBuf::from)
            .collect();
        std::thread::spawn(move || {
            if crate::windows_sandbox::available() {
                crate::windows_sandbox::prime(&caches);
            }
        });
    }
    #[cfg(not(windows))]
    let _ = cfg;
}

/// macOS: the temp directory sandboxed commands get as `$TMPDIR`, private
/// to this Chanto process. Unix sockets connect only there and inside the
/// project, so a test's own server is reachable but other programs' sockets
/// in the shared temp dirs (browser bridges, terminal services) are not.
pub(crate) fn sandbox_tmpdir() -> std::path::PathBuf {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("chanto-sandbox-{}", std::process::id()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let _ = std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&d);
        }
        #[cfg(not(unix))]
        let _ = std::fs::create_dir_all(&d);
        crate::util::canonical(&d).unwrap_or(d)
    })
    .clone()
}

/// Escape a path for a seatbelt string literal. Stripping quotes (the old
/// behavior) let a path containing `"`, `(` or `)` break out of the string and
/// inject profile directives — and a directory name is attacker-controllable in
/// a cloned repo, so escaping is the only safe option.
pub(super) fn sbpl_quote(p: &str) -> String {
    p.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

#[cfg(test)]
pub fn sandbox_profile(cwd: &std::path::Path, cfg: &crate::config::SandboxConfig) -> String {
    sandbox_profile_with_port(cwd, cfg, None)
}

pub(super) fn sandbox_home() -> std::path::PathBuf {
    // Test fixtures never inspect the real user home. CHANTO_HOME relocates
    // config, not private keys, so production still protects the OS home.
    if cfg!(test) {
        std::env::var_os("CHANTO_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    } else {
        dirs::home_dir().unwrap_or_default()
    }
}

pub(super) fn sandbox_profile_with_port(
    cwd: &std::path::Path,
    cfg: &crate::config::SandboxConfig,
    proxy_port: Option<u16>,
) -> String {
    let home = sandbox_home();
    let writable = writable_paths(cwd, cfg);

    sandbox_profile_with_paths(&writable, &home, cfg, proxy_port)
}

pub(crate) fn sandbox_profile_with_paths(
    writable: &[String],
    home: &std::path::Path,
    cfg: &crate::config::SandboxConfig,
    proxy_port: Option<u16>,
) -> String {
    let canon: Vec<String> = writable
        .iter()
        .map(|p| {
            crate::util::canonical(p)
                .map(|c| c.display().to_string())
                .unwrap_or_else(|_| p.clone())
        })
        .collect();
    let mut prof =
        String::from("(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write*\n");
    for p in canon {
        prof.push_str(&format!("  (subpath \"{}\")\n", sbpl_quote(&p)));
    }
    prof.push_str(")\n");
    let canon_home = crate::util::canonical(home).unwrap_or_else(|_| home.to_path_buf());
    // Last match wins in SBPL, so these override the writable grants above.
    let cwd_guess = writable
        .first()
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    // Seatbelt checks resolved paths, so a deny names the resolved path even
    // when it doesn't exist yet: a raw `~/.cargo/bin` under a symlinked
    // `~/.cargo` would never match, and a command could create it.
    let resolved: Vec<String> = writable
        .iter()
        .map(|p| norm(std::path::Path::new(p)))
        .collect();
    prof.push_str(&seatbelt_guards(
        &write_guards(&cwd_guess, &canon_home),
        &resolved,
    ));
    for (regex, p) in secret_read_denies(&canon_home, &cfg.deny_read) {
        if regex {
            // An ordinary string, not a `#"…"` literal: those are raw, so the
            // escaped backslashes made `\\.ssh` match no key at all.
            prof.push_str(&format!(
                "(deny file-read* (regex \"{}\"))\n",
                sbpl_quote(&p)
            ));
        } else {
            // Seatbelt compares resolved paths (e.g. /tmp is /private/tmp),
            // also for a denied path that doesn't exist yet.
            let p = crate::permissions::normalize(std::path::Path::new(&p))
                .display()
                .to_string();
            prof.push_str(&format!(
                "(deny file-read* (subpath \"{}\"))\n",
                sbpl_quote(&p)
            ));
        }
    }
    if cfg.network_policy() != crate::config::NetworkPolicy::Allow {
        prof.push_str("(deny network*)\n(allow network* (remote unix-socket))\n");
        if let Some(port) = proxy_port {
            prof.push_str(&format!(
                "(allow network-outbound (remote ip \"localhost:{port}\"))\n"
            ));
        }
    }
    prof.push_str(&seatbelt_ipc(
        resolved.first().map(String::as_str),
        &norm(&sandbox_tmpdir()),
        cfg.allow_ssh_agent,
    ));
    prof
}

/// Seatbelt rules for local IPC, which `(allow default)` would leave open: a
/// Unix socket or a launch request reaches a process outside the sandbox.
///
/// - Unix sockets connect only inside the project and the command's own
///   `$TMPDIR` ([`sandbox_tmpdir`]), plus DNS and syslog: not Docker's
///   socket (root on the host), an SSH or GPG agent, tmux, X11, editors' or
///   other apps' servers in the shared temp dirs. `allow_ssh_agent` lets the
///   SSH agent back in for git over SSH.
/// - No Apple Events (`osascript` driving Terminal), no LaunchServices
///   (`open -a Terminal`, `open x.command`), no preference writes through
///   cfprefsd (`defaults write` of another app's settings), no clipboard and
///   no Shortcuts (`shortcuts run` acts outside the sandbox).
pub(super) fn seatbelt_ipc(project: Option<&str>, tmp: &str, ssh_agent: bool) -> String {
    let mut prof = String::from("(deny network-outbound (remote unix-socket))\n");
    for w in project.into_iter().chain([tmp]) {
        prof.push_str(&format!(
            "(allow network-outbound (remote unix-socket (subpath \"{}\")))\n",
            sbpl_quote(w)
        ));
    }
    for p in ["/private/var/run/mDNSResponder", "/private/var/run/syslog"] {
        prof.push_str(&format!(
            "(allow network-outbound (remote unix-socket (path-literal \"{p}\")))\n"
        ));
    }
    let agent = std::env::var_os("SSH_AUTH_SOCK").map(|s| norm(std::path::Path::new(&s)));
    let mut risky = vec![RISKY_SOCKETS.to_string()];
    if let Some(s) = &agent {
        risky.push(format!("^{}$", regex::escape(s)));
    }
    for re in risky {
        prof.push_str(&format!(
            "(deny network-outbound (remote unix-socket (path-regex \"{}\")))\n",
            sbpl_quote(&re)
        ));
    }
    if let Some(s) = agent.filter(|_| ssh_agent) {
        prof.push_str(&format!(
            "(allow network-outbound (remote unix-socket (path-literal \"{}\")))\n",
            sbpl_quote(&s)
        ));
    }
    prof.push_str(IPC_DENIES);
    prof
}

/// The fixed part of [`seatbelt_ipc`]: launch requests, Apple Events,
/// preference writes, the clipboard and Shortcuts.
pub(crate) const IPC_DENIES: &str = concat!(
    "(deny mach-lookup (global-name \"com.apple.coreservices.launchservicesd\")",
    " (global-name \"com.apple.coreservices.appleevents\")",
    " (global-name \"com.apple.pasteboard.1\")",
    " (global-name-regex \"^com\\\\.apple\\\\.lsd\\\\.\")",
    " (global-name-regex \"^com\\\\.apple\\\\.shortcuts\")",
    // The Shortcuts daemon (`shortcuts run` talks to it).
    " (global-name-regex \"^com\\\\.apple\\\\.siri\\\\.VoiceShortcuts\"))\n",
    "(deny appleevent-send)\n(deny lsopen)\n(deny user-preference-write)\n",
);

/// Unix sockets in temp dirs that lead out of the sandbox: terminal
/// multiplexers, launchd's per-user sockets (the SSH agent, XQuartz), X11,
/// editors' servers, agents, container engines.
pub(crate) const RISKY_SOCKETS: &str = r"/(tmux|screen)-[0-9]+/|/com\.apple\.launchd\.[^/]+/|/\.X11-unix/|/emacs[0-9]+/|/vscode-[^/]*\.sock$|/ssh-[^/]+/agent\.|/S\.gpg-agent|/nvim\.|/kitty|docker\.sock$|podman\.sock$|containerd\.sock$";

/// Whether shell commands run sandboxed this session (set at startup from
/// config and platform support; `/sandbox` toggles it).
pub static SANDBOX_ACTIVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
pub(crate) static SANDBOX_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn sandbox_on() -> bool {
    SANDBOX_ACTIVE.load(std::sync::atomic::Ordering::Relaxed) && sandbox_works()
}

/// macOS refuses to start a sandbox inside another one (`sandbox_apply:
/// Operation not permitted`), e.g. when Chanto itself runs inside another
/// tool's sandbox. Then every command would fail; instead treat the sandbox as
/// off (the outer one still applies) so nothing assumes isolation it lacks.
/// Only a process already in a sandbox can hit this, and the kernel says
/// whether it is, so the usual unsandboxed process starts no probe at all.
/// Otherwise the probe starts at launch in the background; the first check
/// waits for it.
pub(super) static PROBE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
pub(super) static PROBE_THREAD: std::sync::Mutex<Option<std::thread::JoinHandle<bool>>> =
    std::sync::Mutex::new(None);
pub(super) static NESTED_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn start_sandbox_probe() {
    if !already_sandboxed() {
        let _ = PROBE.set(true);
        return;
    }
    if cfg!(target_os = "macos") && PROBE.get().is_none() {
        let mut t = PROBE_THREAD.lock().unwrap_or_else(|e| e.into_inner());
        if t.is_none() {
            *t = Some(std::thread::spawn(probe_nested));
        }
    }
}

/// Whether this process already runs inside a sandbox: `sandbox_check` with
/// no operation, a system call where the probe below starts a process.
#[cfg(target_os = "macos")]
pub(super) fn already_sandboxed() -> bool {
    unsafe extern "C" {
        fn sandbox_check(
            pid: libc::pid_t,
            operation: *const libc::c_char,
            kind: libc::c_int,
            ...
        ) -> libc::c_int;
    }
    unsafe { sandbox_check(libc::getpid(), std::ptr::null(), 0) != 0 }
}

/// Nothing to probe off macOS: its sandbox is the only one that can't nest.
#[cfg(not(target_os = "macos"))]
pub(super) fn already_sandboxed() -> bool {
    false
}

pub(super) fn probe_nested() -> bool {
    std::process::Command::new("/usr/bin/sandbox-exec")
        // A profile with a restriction: an allow-everything profile is
        // accepted even inside another sandbox, unlike the real one.
        .args([
            "-p",
            "(version 1)(allow default)(deny file-write* (literal \"/chanto-sandbox-probe\"))",
            "/usr/bin/true",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Whether the OS sandbox can actually start here (always true off macOS).
pub fn sandbox_works() -> bool {
    if !cfg!(target_os = "macos") || cfg!(test) {
        return true;
    }
    *PROBE.get_or_init(|| {
        let handle = PROBE_THREAD
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        handle
            .and_then(|h| h.join().ok())
            .unwrap_or_else(probe_nested)
    })
}

/// A one-time notice when the sandbox is configured but can't start here.
pub(super) fn nested_sandbox_notice() -> Option<&'static str> {
    let configured = SANDBOX_ACTIVE.load(std::sync::atomic::Ordering::Relaxed);
    (configured && !sandbox_works() && !NESTED_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed)).then_some(
        "chanto's sandbox can't start here: this process already runs inside another sandbox (likely the tool that launched chanto). Commands run under that outer sandbox only, and nothing is auto-approved on the assumption of chanto's own.",
    )
}

/// Private keys and credential files sandboxed commands can't read.
pub fn secret_read_denies(home: &std::path::Path, extra: &[String]) -> Vec<(bool, String)> {
    // (is_regex, path or pattern)
    let h = home.display().to_string();
    let mut out = vec![
        (
            true,
            format!(
                "^{}/\\.ssh/id_(rsa|dsa|ecdsa|ed25519)(_sk)?$",
                regex::escape(&h)
            ),
        ),
        (false, format!("{h}/.aws/credentials")),
        (false, format!("{h}/.netrc")),
        (false, format!("{h}/.git-credentials")),
        (false, format!("{h}/.pypirc")),
        (false, format!("{h}/.gnupg/private-keys-v1.d")),
        (false, format!("{h}/.config/gcloud/credentials.db")),
        (
            false,
            format!("{h}/.config/gcloud/application_default_credentials.json"),
        ),
        (false, format!("{h}/.password-store")),
        // CLI logins: GitHub, Docker registries, Kubernetes, npm, crates.io, 1Password.
        (false, format!("{h}/.config/gh/hosts.yml")),
        (false, format!("{h}/.docker/config.json")),
        (false, format!("{h}/.kube/config")),
        (false, format!("{h}/.npmrc")),
        (false, format!("{h}/.cargo/credentials")),
        (false, format!("{h}/.cargo/credentials.toml")),
        (false, format!("{h}/.config/op")),
        // Agent-CLI credential stores (provider keys, OAuth tokens).
        (false, format!("{h}/.claude/.credentials.json")),
        (false, format!("{h}/.codex/auth.json")),
    ];
    // Ours, wherever Chanto's home is (CHANTO_HOME, XDG, ~/.chanto): provider
    // keys, the server token, MCP OAuth tokens, runner tokens, IDE lock files
    // (which carry the IDE bridge's auth token).
    let mut homes = vec![crate::config::home_dir()];
    if !homes.contains(&home.join(".chanto")) {
        homes.push(home.join(".chanto"));
    }
    for c in homes {
        for rel in [
            "auth.json",
            "server.json",
            "cloud.json",
            "mcp-auth",
            "runner/tokens.json",
            "runner/connections.json",
            "ide",
        ] {
            out.push((false, c.join(rel).display().to_string()));
        }
    }
    // Windows credential stores, addressed with the platform separator.
    if cfg!(windows) {
        let w = |p: &str| format!("{h}\\{}", p.replace('/', "\\"));
        out.extend([
            (false, w("AppData/Roaming/gcloud/credentials.db")),
            (
                false,
                w("AppData/Roaming/gcloud/application_default_credentials.json"),
            ),
            (false, w("AppData/Roaming/Claude/.credentials.json")),
            (false, w("AppData/Roaming/chanto/auth.json")),
            (false, w(".aws/credentials")),
        ]);
    }
    out.extend(
        extra
            .iter()
            .map(|p| (false, crate::util::resolve(home, p).display().to_string())),
    );
    out
}

pub fn sandbox_available() -> bool {
    #[cfg(windows)]
    {
        crate::windows_sandbox::available()
    }
    #[cfg(not(windows))]
    {
        if cfg!(target_os = "macos") {
            return std::path::Path::new("/usr/bin/sandbox-exec").exists();
        }
        // Ask the kernel: /sys/kernel/security is often absent in containers.
        landlock_abi() > 0
    }
}

#[cfg(target_os = "linux")]
pub(super) fn landlock_abi() -> i64 {
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    // SAFETY: the version query takes no ruleset and only returns a number.
    unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<u8>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(super) fn landlock_abi() -> i64 {
    0
}

/// Existing files and directories the sandbox must not read: the secret
/// deny list with its patterns expanded against the directory listing.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn read_denied_paths(home: &std::path::Path, extra: &[String]) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    #[cfg(windows)]
    if let Ok(entries) = std::fs::read_dir(home.join(".ssh")) {
        static KEYS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"^id_(rsa|dsa|ecdsa|ed25519)(_sk)?$").unwrap()
        });
        out.extend(
            entries
                .flatten()
                .filter(|e| KEYS.is_match(&e.file_name().to_string_lossy()))
                .map(|e| e.path()),
        );
    }
    for (is_regex, p) in secret_read_denies(home, extra) {
        if !is_regex {
            let p = std::path::PathBuf::from(p);
            if p.symlink_metadata().is_ok() {
                out.push(p.canonical().unwrap_or(p));
            }
            continue;
        }
        if cfg!(windows) {
            continue;
        }
        // Patterns are anchored full paths; match them against their
        // literal parent directory (e.g. ~/.ssh).
        let Ok(re) = regex::Regex::new(&p) else {
            continue;
        };
        let lit: String = p
            .trim_start_matches('^')
            .replace("\\.", ".")
            .replace("\\-", "-");
        let parent = lit.rfind('/').map(|i| &lit[..i]).unwrap_or("/");
        if let Ok(rd) = std::fs::read_dir(parent) {
            out.extend(
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|q| re.is_match(&q.display().to_string())),
            );
        }
    }
    out
}

/// Landlock only grants access, so "everything but these paths" becomes:
/// every sibling along the way from / to each denied path, minus the
/// denied paths themselves.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn read_allow_paths(denied: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    let denied: HashSet<PathBuf> = denied.iter().filter(|p| p.is_absolute()).cloned().collect();
    let ancestors: HashSet<PathBuf> = denied
        .iter()
        .flat_map(|p| p.ancestors().skip(1).map(Path::to_path_buf))
        .collect();
    if denied.is_empty() {
        return vec![PathBuf::from("/")];
    }
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::from("/")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for child in rd.flatten().map(|e| e.path()) {
            if denied.contains(&child) {
                continue;
            }
            if ancestors.contains(&child) {
                stack.push(child);
            } else {
                out.push(child);
            }
        }
    }
    out
}
