//! What the app keeps between launches (window geometry) and the
//! single-instance channel: a second launch hands its request (focus, or
//! open a folder) to the running app and exits.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub width: u32,
    pub height: u32,
    pub x: Option<i32>,
    pub y: Option<i32>,
    #[serde(default)]
    pub maximized: bool,
    /// The folder the last session used, reopened on the next launch.
    #[serde(default)]
    pub project: Option<String>,
    /// Webview zoom, 1.0 is actual size (View → Zoom In/Out).
    #[serde(default = "actual_size")]
    pub zoom: f64,
}

fn actual_size() -> f64 {
    1.0
}

impl Default for WindowState {
    fn default() -> Self {
        WindowState {
            width: 1200,
            height: 820,
            x: None,
            y: None,
            maximized: false,
            project: None,
            zoom: actual_size(),
        }
    }
}

impl WindowState {
    pub fn load(file: &Path) -> WindowState {
        let s: WindowState = std::fs::read(file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        // A size from a since-disconnected display shouldn't open a sliver.
        WindowState {
            width: s.width.clamp(480, 10_000),
            height: s.height.clamp(360, 10_000),
            zoom: if s.zoom.is_finite() {
                s.zoom.clamp(0.5, 3.0)
            } else {
                actual_size()
            },
            ..s
        }
    }

    pub fn save(&self, file: &Path) -> std::io::Result<()> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = file.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, file)
    }
}

/// A request from another launch of the app.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Focus,
    Open(PathBuf),
}

impl Request {
    pub fn encode(&self) -> String {
        match self {
            Request::Focus => "focus\n".into(),
            Request::Open(p) => format!("open\t{}\n", p.display()),
        }
    }

    pub fn decode(line: &str) -> Option<Request> {
        let line = line.trim_end_matches(['\r', '\n']);
        match line.split_once('\t') {
            Some(("open", p)) if !p.is_empty() => Some(Request::Open(PathBuf::from(p))),
            None if line == "focus" => Some(Request::Focus),
            _ => None,
        }
    }
}

/// The running instance's endpoint: a Unix socket (owner-only) or, on
/// Windows, a loopback port written to a file.
pub struct Instance {
    path: PathBuf,
}

#[cfg(unix)]
type Listener = std::os::unix::net::UnixListener;
#[cfg(windows)]
type Listener = std::net::TcpListener;

impl Instance {
    pub fn new(home: &Path) -> Instance {
        let mut path = home.join(if cfg!(windows) {
            "desktop.port"
        } else {
            "desktop.sock"
        });
        // Unix socket paths are limited to ~100 bytes; a deep CHANTO_HOME
        // gets one in the per-user temp dir, named after the home.
        if cfg!(unix) && path.as_os_str().len() > 100 {
            let hash = home
                .to_string_lossy()
                .bytes()
                .fold(0xcbf29ce484222325u64, |h, b| {
                    (h ^ b as u64).wrapping_mul(0x100000001b3)
                });
            path = std::env::temp_dir().join(format!("chanto-desktop-{hash:016x}.sock"));
        }
        Instance { path }
    }

    /// Hand `req` to a running instance. True when one took it.
    pub fn forward(&self, req: &Request) -> bool {
        #[cfg(unix)]
        let stream = std::os::unix::net::UnixStream::connect(&self.path);
        #[cfg(windows)]
        let stream = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|p| p.trim().parse::<u16>().ok())
            .ok_or(std::io::ErrorKind::NotFound)
            .and_then(|port| {
                std::net::TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.kind())
            });
        match stream {
            Ok(mut s) => s.write_all(req.encode().as_bytes()).is_ok(),
            Err(_) => false,
        }
    }

    /// Become the running instance; requests arrive through `on_request`.
    pub fn listen(&self, on_request: impl Fn(Request) + Send + 'static) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let listener = self.bind()?;
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                // One request line, at most 4 KiB.
                let mut line = String::new();
                if BufReader::new(std::io::Read::take(stream, 4096))
                    .read_line(&mut line)
                    .is_ok_and(|n| n > 0)
                    && let Some(r) = Request::decode(&line)
                {
                    on_request(r);
                }
            }
        });
        Ok(())
    }

    #[cfg(unix)]
    fn bind(&self) -> std::io::Result<Listener> {
        // A stale socket from a crashed instance (nobody answered `forward`).
        let _ = std::fs::remove_file(&self.path);
        let l = Listener::bind(&self.path)?;
        std::fs::set_permissions(
            &self.path,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )?;
        Ok(l)
    }

    #[cfg(windows)]
    fn bind(&self) -> std::io::Result<Listener> {
        let l = Listener::bind(("127.0.0.1", 0))?;
        std::fs::write(&self.path, l.local_addr()?.port().to_string())?;
        Ok(l)
    }

    pub fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests;
