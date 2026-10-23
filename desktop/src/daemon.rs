//! Finding or starting the local Chanto daemon (`chanto web --runs`), the
//! same one `chanto run --bg` and `chanto daemon start` use. Its address and
//! bearer token are in `~/.chanto/server.json`.

use serde_json::Value;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Chanto's home directory (`CHANTO_HOME`, else `~/.chanto`).
pub fn home() -> PathBuf {
    if let Some(h) = std::env::var_os("CHANTO_HOME").filter(|h| !h.is_empty()) {
        return PathBuf::from(h);
    }
    home_dir().join(".chanto")
}

fn home_dir() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// A running server: where it listens and its bearer token.
#[derive(Debug, Clone, PartialEq)]
pub struct Server {
    pub url: String,
    pub token: String,
}

impl Server {
    /// The connection file the server writes at startup.
    pub fn parse(json: &[u8]) -> Option<Server> {
        let v: Value = serde_json::from_slice(json).ok()?;
        let url = v["url"].as_str()?.trim_end_matches('/').to_string();
        let token = v["token"].as_str().filter(|t| !t.is_empty())?.to_string();
        url.starts_with("http://").then_some(Server { url, token })
    }

    pub fn from_home(home: &Path) -> Option<Server> {
        Server::parse(&std::fs::read(home.join("server.json")).ok()?)
    }

    /// `/health`, when the server answers.
    pub fn health(&self) -> Option<Value> {
        let (status, body) = get(&self.url, "/health").ok()?;
        (status == 200)
            .then(|| serde_json::from_slice(&body).ok())
            .flatten()
    }

    /// Whether it serves the web UI (the daemon does; plain `chanto serve` doesn't).
    pub fn serves_web(&self) -> bool {
        get(&self.url, "/").is_ok_and(|(status, _)| status == 200)
    }
}

/// A plain HTTP/1.1 GET (the server is on loopback; no TLS, no proxies).
pub fn get(base: &str, path: &str) -> std::io::Result<(u16, Vec<u8>)> {
    let host = base
        .strip_prefix("http://")
        .ok_or_else(|| std::io::Error::other("only http:// servers are supported"))?;
    let host = host.split('/').next().unwrap_or(host);
    let addr = host
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other("unresolvable server address"))?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nAccept: */*\r\n\r\n"
    )?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf)?;
    parse_response(&buf).ok_or_else(|| std::io::Error::other("malformed HTTP response"))
}

/// Status and body of an HTTP/1.1 response (plain or chunked).
pub fn parse_response(raw: &[u8]) -> Option<(u16, Vec<u8>)> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..end]).ok()?;
    let status = head.split_whitespace().nth(1)?.parse().ok()?;
    let body = &raw[end + 4..];
    let chunked = head.lines().any(|l| {
        l.to_ascii_lowercase().starts_with("transfer-encoding:")
            && l.to_ascii_lowercase().contains("chunked")
    });
    if !chunked {
        return Some((status, body.to_vec()));
    }
    let (mut out, mut rest) = (Vec::new(), body);
    loop {
        let line_end = rest.windows(2).position(|w| w == b"\r\n")?;
        let size = usize::from_str_radix(
            std::str::from_utf8(&rest[..line_end])
                .ok()?
                .split(';')
                .next()?
                .trim(),
            16,
        )
        .ok()?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Some((status, out));
        }
        out.extend_from_slice(rest.get(..size)?);
        rest = rest.get(size + 2..)?;
    }
}

/// Whether two paths name the same folder (a server records an absolute one).
fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The folder recorded in `server.json` (sessions without one start there).
fn parse_cwd(json: &[u8]) -> Option<PathBuf> {
    let v: Value = serde_json::from_slice(json).ok()?;
    v["cwd"]
        .as_str()
        .filter(|c| !c.is_empty())
        .map(PathBuf::from)
}

/// Whether the server in `home` runs in Chanto's own folder: a daemon it
/// started, rather than one a person pointed at a project.
pub fn runs_in_home(home: &Path) -> bool {
    std::fs::read(home.join("server.json"))
        .ok()
        .and_then(|json| parse_cwd(&json))
        .is_some_and(|cwd| same_dir(&cwd, home))
}

fn exe_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// The `chanto` CLI: `$CHANTO_BIN`, next to this app (inside the .app
/// bundle, or `target/<profile>/` in a checkout), else on PATH.
pub fn chanto_bin(env_bin: Option<PathBuf>, exe: Option<PathBuf>, path: &str) -> Option<PathBuf> {
    if let Some(b) = env_bin.filter(|b| b.is_file()) {
        return Some(b);
    }
    if let Some(sibling) = exe
        .as_ref()
        .and_then(|e| e.parent())
        .map(|d| d.join(exe_name("chanto")))
        .filter(|p| p.is_file())
    {
        return Some(sibling);
    }
    std::env::split_paths(path)
        .map(|d| d.join(exe_name("chanto")))
        .find(|p| p.is_file())
}

/// PATH for the daemon and the agents it runs. Apps opened from the Dock or
/// Finder inherit a bare PATH, so the login shell's is used (or, failing
/// that, the usual install prefixes are added).
pub fn daemon_path(current: &str, login_shell_path: Option<String>) -> String {
    let mut dirs: Vec<PathBuf> = std::env::split_paths(current).collect();
    let mut add = |d: PathBuf| {
        if !d.as_os_str().is_empty() && !dirs.contains(&d) {
            dirs.push(d);
        }
    };
    if let Some(p) = login_shell_path {
        for d in std::env::split_paths(&p) {
            add(d);
        }
    }
    if cfg!(unix) {
        let home = home_dir();
        for d in [
            "/opt/homebrew/bin",
            "/opt/homebrew/sbin",
            "/usr/local/bin",
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
        ] {
            add(PathBuf::from(d));
        }
        add(home.join(".cargo/bin"));
        add(home.join(".local/bin"));
    }
    std::env::join_paths(dirs)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| current.to_string())
}

#[cfg(unix)]
fn login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/zsh".into());
    let mut child = Command::new(shell)
        .args(["-l", "-c", "printf '\\n__PATH__%s' \"$PATH\""])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    // A slow shell profile mustn't hold up the window for long.
    for _ in 0..30 {
        if let Ok(Some(_)) = child.try_wait() {
            let mut out = String::new();
            child.stdout.take()?.read_to_string(&mut out).ok()?;
            return out
                .rsplit("__PATH__")
                .next()
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    None
}

#[cfg(not(unix))]
fn login_shell_path() -> Option<String> {
    None
}

/// Keeps console programs started from the (windowless) app from opening a console window.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Connect to the daemon, starting it (`chanto daemon start`) when needed.
pub fn connect() -> Result<Server, String> {
    let home = home();
    if let Some(s) = Server::from_home(&home)
        && let Some(h) = s.health()
    {
        if h["runs"] != true {
            return Err(format!(
                "The chanto server at {} doesn't host background runs (it listens beyond this machine). Stop it, or restart it with --runs.",
                s.url
            ));
        }
        if !s.serves_web() {
            return Err(format!(
                "The chanto server at {} doesn't serve the web UI. Run `chanto daemon stop` and reopen the app.",
                s.url
            ));
        }
        return Ok(s);
    }
    let path = daemon_path(
        &std::env::var("PATH").unwrap_or_default(),
        login_shell_path(),
    );
    let bin = chanto_bin(std::env::var_os("CHANTO_BIN").map(PathBuf::from), std::env::current_exe().ok(), &path)
        .ok_or("The chanto command wasn't found. Install chanto (https://chanto.sh/docs/quickstart), or set CHANTO_BIN to its path.")?;
    let mut cmd = Command::new(&bin);
    cmd.args(["daemon", "start"])
        .env("PATH", &path)
        .stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("Couldn't run {}: {e}", bin.display()))?;
    if !out.status.success() {
        return Err(format!(
            "`chanto daemon start` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let s = Server::from_home(&home).ok_or_else(|| {
        format!(
            "The daemon started but {} is missing.",
            home.join("server.json").display()
        )
    })?;
    s.health().ok_or_else(|| {
        format!(
            "The daemon at {} isn't answering; see {}.",
            s.url,
            home.join("daemon.log").display()
        )
    })?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_file_is_parsed() {
        let s = Server::parse(br#"{"url": "http://127.0.0.1:4141/", "token": "abc", "pid": 7}"#)
            .unwrap();
        assert_eq!(
            s,
            Server {
                url: "http://127.0.0.1:4141".into(),
                token: "abc".into()
            }
        );
        assert!(Server::parse(br#"{"url": "http://127.0.0.1:1", "token": ""}"#).is_none());
        assert!(Server::parse(br#"{"url": "https://x", "token": "t"}"#).is_none());
        assert!(Server::parse(b"not json").is_none());
    }

    #[test]
    fn server_file_records_its_folder() {
        let json = br#"{"url": "http://127.0.0.1:4141", "token": "abc", "cwd": "/Users/me/api"}"#;
        assert_eq!(parse_cwd(json), Some("/Users/me/api".into()));
        // An older server.json has no cwd; an empty one means the same.
        assert_eq!(
            parse_cwd(br#"{"url": "http://127.0.0.1:4141", "token": "abc"}"#),
            None
        );
        assert_eq!(
            parse_cwd(br#"{"url": "http://127.0.0.1:4141", "cwd": ""}"#),
            None
        );
        assert_eq!(parse_cwd(b"not json"), None);
    }

    #[test]
    fn a_server_in_the_chanto_folder_is_detected() {
        let dir = std::env::temp_dir().join(format!("chanto-daemon-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!runs_in_home(&dir));
        let write = |cwd: &Path| {
            let json = format!(
                r#"{{"url": "http://127.0.0.1:1", "token": "t", "cwd": {:?}}}"#,
                cwd.to_str().unwrap()
            );
            std::fs::write(dir.join("server.json"), json).unwrap();
        };
        write(&dir);
        assert!(runs_in_home(&dir));
        // A daemon a person started in their project is not in Chanto's folder.
        write(&std::env::temp_dir());
        assert!(!runs_in_home(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn responses_are_parsed() {
        assert_eq!(
            parse_response(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}"),
            Some((200, b"{}".to_vec()))
        );
        let chunked = b"HTTP/1.1 404 Not Found\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked), Some((404, b"abcde".to_vec())));
        assert_eq!(parse_response(b"garbage"), None);
    }

    #[test]
    fn health_against_a_local_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for mut c in listener.incoming().flatten().take(2) {
                // Read the whole request head: it may arrive in several pieces.
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match c.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let req = String::from_utf8_lossy(&req).to_string();
                let body = if req.starts_with("GET /health ") {
                    r#"{"ok":true,"runs":true}"#
                } else {
                    ""
                };
                let status = if body.is_empty() {
                    "404 Not Found"
                } else {
                    "200 OK"
                };
                let _ = write!(
                    c,
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        let s = Server {
            url,
            token: "t".into(),
        };
        assert_eq!(s.health().unwrap()["runs"], true);
        assert!(!s.serves_web());
        // Nobody listening.
        assert!(
            Server {
                url: "http://127.0.0.1:1".into(),
                token: "t".into()
            }
            .health()
            .is_none()
        );
    }

    #[test]
    fn chanto_is_found_next_to_the_app_first() {
        let dir = std::env::temp_dir().join(format!("chanto-desktop-bin-{}", std::process::id()));
        let (app, other) = (dir.join("app"), dir.join("bin"));
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        for d in [&app, &other] {
            std::fs::write(d.join(exe_name("chanto")), "").unwrap();
        }
        let path = std::env::join_paths([&other])
            .unwrap()
            .into_string()
            .unwrap();
        let exe = Some(app.join("chanto-desktop"));
        assert_eq!(
            chanto_bin(None, exe.clone(), &path),
            Some(app.join(exe_name("chanto")))
        );
        assert_eq!(
            chanto_bin(None, Some(dir.join("x/chanto-desktop")), &path),
            Some(other.join(exe_name("chanto")))
        );
        let explicit = other.join(exe_name("chanto"));
        assert_eq!(
            chanto_bin(Some(explicit.clone()), exe, &path),
            Some(explicit)
        );
        assert_eq!(chanto_bin(None, None, ""), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn daemon_path_keeps_order_and_adds_install_prefixes() {
        let p = daemon_path("/usr/bin:/bin", Some("/Users/me/bin:/usr/bin".into()));
        let dirs: Vec<&str> = p.split(':').collect();
        assert_eq!(&dirs[..3], ["/usr/bin", "/bin", "/Users/me/bin"]);
        assert!(dirs.contains(&"/opt/homebrew/bin"));
        assert_eq!(dirs.iter().filter(|d| **d == "/usr/bin").count(), 1);
    }
}
