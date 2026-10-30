//! Chanto desktop: a native window around the local daemon's web UI.
//!
//! On launch it finds or starts the daemon (`chanto daemon start`), opens
//! its web UI in the system webview with the bearer token injected into the
//! page's session storage (never in a URL, so never in history), and adds
//! native menus, a project-folder picker, a dock badge for runs waiting on
//! you, a single instance and a remembered window size. Notifications for
//! runs come from the daemon itself (`[notify] desktop`).

// A GUI app on Windows: no console window next to it.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod daemon;
mod state;

use muda::accelerator::Accelerator;
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use serde_json::Value;
use state::{Instance, Request, WindowState};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::{UserAttentionType, Window, WindowBuilder};
use wry::{NewWindowResponse, PermissionKind, PermissionResponse, WebView, WebViewBuilder};

enum UserEvent {
    Menu(String),
    Instance(Request),
    /// A message from the page (`window.ipc.postMessage`).
    Ipc(String),
    /// The daemon came up (or didn't) after the error page's Retry.
    Retried(Result<daemon::Server, String>),
    /// Time to take the `CHANTO_DESKTOP_SNAPSHOT` picture.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    TakeSnapshot(PathBuf),
    /// It was written (or not).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Snapshot(Result<PathBuf, String>),
}

const USAGE: &str = "chanto desktop: the Chanto web UI in its own window.

Usage: chanto-desktop [FOLDER]

FOLDER opens a session in that project. A second launch focuses the
running window (and opens FOLDER there). Set CHANTO_BIN to choose the
Chanto CLI (default: next to this app, else on PATH).

CHANTO_DESKTOP_SNAPSHOT=out.png (macOS) saves a picture of the page once
it has loaded, then quits: for docs and smoke tests.";

/// The app mark, inlined so the error page needs no files or network.
const LOGO: &str = include_str!("../../assets/logo.svg");

/// Save the page as a PNG (WebKit renders it, so this works with the
/// screen locked or the window covered).
#[cfg(target_os = "macos")]
fn snapshot(webview: &WebView, out: PathBuf, proxy: tao::event_loop::EventLoopProxy<UserEvent>) {
    use wry::WebViewExtMacOS;
    let block = block2::RcBlock::new(
        move |image: *mut objc2_app_kit::NSImage, _error: *mut objc2_foundation::NSError| {
            let tiff = out.with_extension("tiff");
            let result = unsafe { image.as_ref() }
                .and_then(|i| i.TIFFRepresentation())
                .ok_or_else(|| "WebKit returned no image".to_string())
                .and_then(|data| std::fs::write(&tiff, data.to_vec()).map_err(|e| e.to_string()))
                .and_then(|_| {
                    let o = std::process::Command::new("sips")
                        .args(["-s", "format", "png"])
                        .arg(&tiff)
                        .arg("--out")
                        .arg(&out)
                        .output()
                        .map_err(|e| e.to_string())?;
                    if o.status.success() {
                        Ok(out.clone())
                    } else {
                        Err(String::from_utf8_lossy(&o.stderr).into_owned())
                    }
                });
            let _ = std::fs::remove_file(&tiff);
            let _ = proxy.send_event(UserEvent::Snapshot(result));
        },
    );
    let wk = webview.webview();
    unsafe { wk.takeSnapshotWithConfiguration_completionHandler(None, &block) };
}

/// Runs before the page's own scripts on every load of the daemon's origin.
/// It only fills in a token when the page has none: a daemon that restarted
/// gets a new token, which the page stores from its fragment; a later reload
/// must keep that one, not this script's startup copy.
fn init_script(server: &daemon::Server) -> String {
    let origin = serde_json::to_string(&server.url).unwrap();
    let token = serde_json::to_string(&server.token).unwrap();
    format!(
        "(() => {{ if (location.origin !== {origin}) return; \
         try {{ if (!sessionStorage.getItem('chanto-token')) sessionStorage.setItem('chanto-token', {token}); }} catch (e) {{}} }})();"
    )
}

/// Only the daemon's own pages load in the window; anything else opens in the browser.
fn same_origin(origin: &str, url: &str) -> bool {
    url == origin
        || url.strip_prefix(origin).is_some_and(|rest| {
            rest.starts_with('/') || rest.starts_with('#') || rest.starts_with('?')
        })
}

/// What the window may navigate to. Before a daemon exists, only the shell's
/// own error page (which has no HTTP origin) is allowed.
fn navigation_allowed(origin: Option<&str>, url: &str) -> bool {
    match origin {
        Some(origin) => same_origin(origin, url) || url == "about:blank",
        None => !url.starts_with("http://") && !url.starts_with("https://"),
    }
}

/// Whether an IPC message from `uri` may drive the shell: the error page
/// (before there is an origin) or the daemon's own pages.
fn ipc_trusted(origin: Option<&str>, uri: &str) -> bool {
    origin.is_none_or(|origin| same_origin(origin, uri))
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The message for the error page: escaped, with `backticks` as code spans
/// (errors name commands and paths that way).
fn error_html(message: &str) -> String {
    message
        .split('`')
        .enumerate()
        .map(|(i, part)| {
            let part = escape_html(part);
            if i % 2 == 1 && !part.is_empty() {
                format!("<code>{part}</code>")
            } else {
                part
            }
        })
        .collect()
}

/// Shown before the daemon answers.
#[cfg(test)]
fn error_page(message: &str) -> String {
    error_page_with_log(message, None)
}

/// The same page, plus a button that opens `log`.
fn error_page_with_log(message: &str, log: Option<&Path>) -> String {
    let log_button = log
        .map(|_| "<button id=logs type=button>Open log</button>")
        .unwrap_or_default();
    format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>chanto</title>\
         <style>\
         :root{{color-scheme:light dark}}\
         body{{font:14px/1.55 -apple-system,BlinkMacSystemFont,'Segoe UI',system-ui,sans-serif;margin:0;min-height:100vh;display:grid;place-items:center;background:#faf9f7;color:#262624}}\
         main{{box-sizing:border-box;max-width:34rem;margin:24px;padding:26px 28px;border:1px solid #e4e1db;border-radius:14px;background:#fff}}\
         .mark svg{{display:block;width:44px;height:44px;margin-bottom:14px}}\
         h1{{font-size:17px;margin:0;letter-spacing:-.01em}}\
         p{{color:#6b6a66;margin:12px 0 0}}\
         pre{{white-space:pre-wrap;overflow-wrap:anywhere;margin:14px 0 0;padding:10px 12px;border-radius:8px;background:#f4f1ec;color:#3f3d39;font:12.5px/1.5 ui-monospace,SFMono-Regular,Menlo,monospace}}\
         pre code{{padding:1px 4px;border-radius:4px;background:#eae6df;color:#262624;font:inherit}}\
         button{{margin-top:16px;padding:7px 14px;border:1px solid #d7d3cc;border-radius:8px;background:#fff;color:#262624;font:inherit;font-weight:600;cursor:pointer}}\
         button + button{{margin-left:8px}}\
         button:hover:not(:disabled){{background:#f4f1ec}}\
         button:disabled{{opacity:.6;cursor:default}}\
         @media (prefers-color-scheme:dark){{\
         body{{background:#262624;color:#ecebe8}}\
         main{{border-color:#3d3c39;background:#2e2e2b}}\
         p{{color:#a5a29b}}\
         pre{{background:#383835;color:#dedcd6}}\
         pre code{{background:#444340;color:#ecebe8}}\
         button{{border-color:#4a4844;background:#333330;color:#ecebe8}}\
         button:hover:not(:disabled){{background:#3d3d3a}}\
         }}\
         </style>\
         <main><div class=mark>{}</div><h1>chanto couldn't start</h1><pre>{}</pre><p>Fix the problem, then try again (or reopen the app).</p>\
         <div><button id=retry type=button>Try again</button>{log_button}</div></main>\
         <script>\
         const retry = document.getElementById('retry');\
         retry.onclick = () => {{ retry.disabled = true; retry.textContent = 'Starting…'; window.ipc.postMessage(JSON.stringify({{type: 'retry'}})); }};\
         const logs = document.getElementById('logs');\
         if (logs) logs.onclick = () => window.ipc.postMessage(JSON.stringify({{type: 'logs'}}));\
         window.ipc.postMessage(JSON.stringify({{type: 'error-page'}}));\
         </script>",
        LOGO,
        error_html(message)
    )
}

/// A JavaScript call to the web UI's hooks (`window.chanto` in app.js).
fn hook(name: &str, arg: Option<&str>) -> String {
    let arg = arg
        .map(|a| serde_json::to_string(a).unwrap())
        .unwrap_or_default();
    format!("window.chanto && window.chanto.{name}({arg});")
}

fn open_externally(target: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(target).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(target).spawn();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(target)
            .creation_flags(daemon::CREATE_NO_WINDOW)
            .spawn();
    }
}

/// Quote one argument for the shell that will run setup.
#[cfg(not(windows))]
fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}
#[cfg(windows)]
fn shell_quote(s: &str) -> String {
    if s.chars().any(|c| c.is_whitespace() || "&()^".contains(c)) {
        format!("\"{s}\"")
    } else {
        s.to_string()
    }
}

/// The remembered project, unless it has since been moved or deleted; the
/// page then starts in the daemon's own folder and forgets it.
fn live_project(project: Option<&str>) -> Option<&str> {
    project.filter(|p| !p.is_empty() && Path::new(p).is_dir())
}

/// Whether a launch has to ask where to work: nothing is remembered, or the
/// folder is gone. Without one, sessions would start in the daemon's own
/// folder (`~/.chanto`).
fn needs_project(project: Option<&str>) -> bool {
    live_project(project).is_none()
}

/// The page to load on startup. The remembered project rides in the fragment
/// (read before the page's scripts run); the token is injected by the init
/// script instead, so it never reaches an address bar.
fn page_url(server: &daemon::Server, project: Option<&str>) -> String {
    with_fragment(server, live_project(project), None)
}

/// A recovered daemon's page: the token rides in the fragment once, then
/// app.js moves it to sessionStorage and clears the address bar.
fn retry_url(server: &daemon::Server, project: Option<&str>) -> String {
    with_fragment(server, live_project(project), Some(&server.token))
}

fn with_fragment(server: &daemon::Server, project: Option<&str>, token: Option<&str>) -> String {
    let mut parts = Vec::new();
    if let Some(token) = token {
        parts.push(format!("token={}", fragment_value(token)));
    }
    if let Some(project) = project.filter(|p| !p.is_empty()) {
        parts.push(format!("project={}", fragment_value(project)));
    }
    if parts.is_empty() {
        format!("{}/", server.url)
    } else {
        format!("{}/#{}", server.url, parts.join("&"))
    }
}

/// Percent-encode a fragment value (`URLSearchParams` on the page decodes it).
fn fragment_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The shell command the setup terminal runs.
fn setup_command(bin: Option<PathBuf>) -> String {
    match bin {
        Some(b) => format!("{} setup", shell_quote(&b.display().to_string())),
        None => "chanto setup".to_string(),
    }
}

/// Open the user's terminal running `chanto setup`: first-run desktop needs
/// no shell of its own.
fn open_setup() {
    let path = std::env::var("PATH").unwrap_or_default();
    let bin = daemon::chanto_bin(
        std::env::var_os("CHANTO_BIN").map(PathBuf::from),
        std::env::current_exe().ok(),
        &path,
    );
    let command = setup_command(bin);
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::PermissionsExt;
        let script =
            std::env::temp_dir().join(format!("chanto-setup-{}.command", std::process::id()));
        let body = format!(
            "#!/bin/sh\nrm -f \"$0\"\n{command}\necho\nprintf 'Setup finished. Return to chanto and reload (Command-R).\\n'\n"
        );
        if std::fs::write(&script, body).is_ok() {
            let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755));
            let _ = std::process::Command::new("open")
                .args(["-a", "Terminal"])
                .arg(&script)
                .spawn();
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let run = format!(
            "{command}; echo; printf 'Setup finished. Return to chanto and reload (Ctrl-R).\\n'; read -r _"
        );
        for (term, flag) in [
            ("x-terminal-emulator", "-e"),
            ("gnome-terminal", "--"),
            ("konsole", "-e"),
            ("xterm", "-e"),
        ] {
            if std::process::Command::new(term)
                .arg(flag)
                .args(["sh", "-c", &run])
                .spawn()
                .is_ok()
            {
                break;
            }
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", "cmd", "/k", &command])
            .creation_flags(daemon::CREATE_NO_WINDOW)
            .spawn();
    }
}

/// A text file in the user's editor.
fn open_text(file: &Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open")
        .arg("-t")
        .arg(file)
        .spawn();
    #[cfg(not(target_os = "macos"))]
    open_externally(&file.display().to_string());
}

/// Settings: the user's config.toml in their editor.
fn open_settings(home: &Path) {
    let file = home.join("config.toml");
    if !file.exists() {
        let _ = std::fs::create_dir_all(home);
        let _ = std::fs::write(
            &file,
            "# Chanto configuration. `chanto --example-config` prints every option.\n",
        );
    }
    open_text(&file);
}

fn accel(s: &str) -> Option<Accelerator> {
    s.parse().ok()
}

fn build_menu() -> (Menu, MenuItem) {
    let menu = Menu::new();
    let item = |id: &str, text: &str, key: &str| MenuItem::with_id(id, text, true, accel(key));
    #[cfg(target_os = "macos")]
    {
        let app = Submenu::new("chanto", true);
        let _ = app.append_items(&[
            &PredefinedMenuItem::about(Some("About chanto"), None),
            &PredefinedMenuItem::separator(),
            &item("settings", "Settings…", "CmdOrCtrl+,"),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::services(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            &item("quit", "Quit chanto", "CmdOrCtrl+Q"),
        ]);
        let _ = menu.append(&app);
    }
    let file = Submenu::new("File", true);
    let _ = file.append_items(&[
        &item("new-session", "New Session", "CmdOrCtrl+N"),
        &item("open-project", "Open Project Folder…", "CmdOrCtrl+O"),
        &PredefinedMenuItem::separator(),
        &item("dictate", "Dictate", "CmdOrCtrl+Shift+D"),
    ]);
    #[cfg(not(target_os = "macos"))]
    let _ = file.append_items(&[
        &PredefinedMenuItem::separator(),
        &item("settings", "Settings…", "CmdOrCtrl+,"),
        &item("quit", "Quit", "CmdOrCtrl+Q"),
    ]);
    let edit = Submenu::new("Edit", true);
    let _ = edit.append_items(&[
        &PredefinedMenuItem::undo(None),
        &PredefinedMenuItem::redo(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::cut(None),
        &PredefinedMenuItem::copy(None),
        &PredefinedMenuItem::paste(None),
        &PredefinedMenuItem::select_all(None),
    ]);
    let view = Submenu::new("View", true);
    // Enabled once the page has a visual piece to show.
    let preview = item("preview", "Visual Preview", "CmdOrCtrl+Shift+P");
    preview.set_enabled(false);
    let _ = view.append_items(&[
        &preview,
        &item("runs", "Background Runs", "CmdOrCtrl+Shift+R"),
        &PredefinedMenuItem::separator(),
        &item("actual-size", "Actual Size", "CmdOrCtrl+0"),
        &item("zoom-in", "Zoom In", "CmdOrCtrl+="),
        &item("zoom-out", "Zoom Out", "CmdOrCtrl+-"),
        &PredefinedMenuItem::separator(),
        &item("reload", "Reload", "CmdOrCtrl+R"),
    ]);
    let window = Submenu::new("Window", true);
    let _ = window.append_items(&[
        &PredefinedMenuItem::minimize(None),
        &PredefinedMenuItem::maximize(None),
        &PredefinedMenuItem::fullscreen(None),
        &PredefinedMenuItem::close_window(None),
    ]);
    let _ = menu.append_items(&[&file, &edit, &view, &window]);
    #[cfg(target_os = "macos")]
    window.set_as_windows_menu_for_nsapp();
    (menu, preview)
}

fn set_badge(window: &Window, waiting: u64) {
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::WindowExtMacOS;
        window.set_badge_label((waiting > 0).then(|| waiting.to_string()));
    }
    #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    {
        use tao::platform::unix::WindowExtUnix;
        window.set_badge_count(
            (waiting > 0).then_some(waiting as i64),
            Some("chanto-desktop.desktop".into()),
        );
    }
    #[cfg(windows)]
    let _ = (window, waiting);
}

fn title(project: &str, waiting: u64) -> String {
    let name = Path::new(project)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|_| !project.is_empty());
    let mut t = match name {
        Some(n) if n != "chanto" => format!("{n} — chanto"),
        _ => "chanto".into(),
    };
    if waiting > 0 {
        t.push_str(&format!(" · {waiting} waiting"));
    }
    t
}

fn theme_color(theme: tao::window::Theme) -> wry::RGBA {
    // Match the page's tokens so the first paint doesn't flash white.
    match theme {
        tao::window::Theme::Dark => (0x26, 0x26, 0x24, 0xff),
        _ => (0xff, 0xff, 0xff, 0xff),
    }
}

struct App {
    window: Window,
    webview: WebView,
    home: PathBuf,
    geometry: WindowState,
    saved_at: Instant,
    dirty: bool,
    /// The page has loaded its hooks; calls made before wait here.
    ready: bool,
    /// The daemon's origin, shared with the navigation and IPC handlers.
    /// Empty while the error page is showing (there is no daemon yet).
    origin: Arc<Mutex<Option<String>>>,
    /// The daemon's bearer token (a restart may produce a new one).
    token: String,
    /// The daemon runs in Chanto's own folder, so sessions have no project
    /// of their own yet.
    daemon_in_home: bool,
    queued: Vec<String>,
    waiting: u64,
    project: String,
    snapshot: Option<PathBuf>,
    /// Launched with a folder whose session is still being opened.
    opening: bool,
    /// The first launch has already offered to pick a folder.
    asked_project: bool,
    /// View → Visual Preview, enabled while the page has a piece to show.
    preview_item: MenuItem,
    /// Webview zoom, 1.0 is actual size.
    zoom: f64,
    proxy: tao::event_loop::EventLoopProxy<UserEvent>,
}

impl App {
    fn call(&mut self, js: String) {
        if self.ready {
            let _ = self.webview.evaluate_script(&js);
        } else {
            self.queued.push(js);
        }
    }

    fn focus(&self) {
        self.window.set_visible(true);
        self.window.set_minimized(false);
        self.window.set_focus();
    }

    fn set_zoom(&mut self, zoom: f64) {
        self.zoom = zoom.clamp(0.5, 3.0);
        let _ = self.webview.zoom(self.zoom);
        self.geometry.zoom = self.zoom;
        self.save_geometry(true);
    }

    fn open_project(&mut self, dir: &Path) {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        if dir.is_dir() {
            self.call(hook("openProject", Some(&dir.display().to_string())));
        }
    }

    fn pick_project(&mut self) {
        if let Some(dir) = rfd::FileDialog::new()
            .set_title("Open a project folder")
            .pick_folder()
        {
            self.open_project(&dir);
        }
    }

    fn on_menu(&mut self, id: &str) -> bool {
        match id {
            "new-session" => self.call(hook("newSession", None)),
            "open-project" => self.pick_project(),
            "runs" => self.call(hook("showRuns", None)),
            "preview" => self.call(hook("togglePreview", None)),
            "dictate" => self.call(hook("dictate", None)),
            "actual-size" => self.set_zoom(1.0),
            "zoom-in" => self.set_zoom(self.zoom + 0.1),
            "zoom-out" => self.set_zoom(self.zoom - 0.1),
            "settings" => open_settings(&self.home),
            "reload" => {
                self.ready = false;
                if self.origin.lock().unwrap().is_none() {
                    self.retry();
                } else {
                    let _ = self.webview.reload();
                }
            }
            "quit" => return true,
            _ => {}
        }
        false
    }

    /// The error page's Retry (or Reload there): find or start the daemon
    /// off the UI thread, then load its web UI.
    fn retry(&mut self) {
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let _ = proxy.send_event(UserEvent::Retried(daemon::connect()));
        });
    }

    fn retried(&mut self, result: Result<daemon::Server, String>) {
        match result {
            Ok(s) => {
                let old = self.origin.lock().unwrap().replace(s.url.clone());
                let same = old.as_deref() == Some(s.url.as_str()) && self.token == s.token;
                self.token = s.token.clone();
                self.daemon_in_home = daemon::runs_in_home(&self.home);
                self.ready = false;
                // The same daemon: reload in place (keeping the open session
                // or run in the address bar). A new one gets its token in the
                // fragment, which the page clears, and project.
                if same {
                    let _ = self.webview.reload();
                    return;
                }
                // Calls made while the error page was up (an
                // `openProject`, say) wait for the real page.
                let url = retry_url(&s, self.geometry.project.as_deref());
                if let Err(e) = self.webview.load_url(&url) {
                    self.show_error(&format!("Couldn't open {}: {e}", s.url));
                }
            }
            Err(e) => self.show_error(&e),
        }
    }

    fn show_error(&mut self, message: &str) {
        self.ready = false;
        *self.origin.lock().unwrap() = None;
        let log = self.home.join("daemon.log");
        let log = log.is_file().then_some(log);
        let _ = self
            .webview
            .load_html(&error_page_with_log(message, log.as_deref()));
    }

    fn open_log(&self) {
        let log = self.home.join("daemon.log");
        if log.is_file() {
            open_text(&log);
        }
    }

    fn on_ipc(&mut self, body: &str) {
        let Ok(m) = serde_json::from_str::<Value>(body) else {
            return;
        };
        match m["type"].as_str() {
            Some("ready") => {
                self.ready = true;
                for js in std::mem::take(&mut self.queued) {
                    let _ = self.webview.evaluate_script(&js);
                }
                if !self.opening {
                    // A daemon in Chanto's own folder has no project of its
                    // own yet; ask where to work instead of starting sessions
                    // in `~/.chanto`. One a person pointed at a folder keeps it.
                    if !self.asked_project
                        && self.snapshot.is_none()
                        && self.daemon_in_home
                        && needs_project(self.geometry.project.as_deref())
                    {
                        self.asked_project = true;
                        self.focus();
                        self.pick_project();
                    }
                    self.schedule_snapshot();
                }
            }
            Some("error-page") => {
                // Nothing to flush into the error page; a retry still has
                // the calls queued for the real page.
                self.schedule_snapshot();
            }
            Some("runs") => {
                let waiting = m["waiting"].as_u64().unwrap_or(0);
                if waiting > self.waiting && !self.window.is_focused() {
                    self.window
                        .request_user_attention(Some(UserAttentionType::Informational));
                }
                if waiting != self.waiting {
                    self.waiting = waiting;
                    set_badge(&self.window, waiting);
                    self.window.set_title(&title(&self.project, waiting));
                }
            }
            Some("project") => {
                let path = m["path"].as_str().unwrap_or_default();
                // Reopened on the next launch so New session doesn't start
                // in the daemon's own folder.
                if path != self.geometry.project.as_deref().unwrap_or_default() {
                    self.geometry.project = (!path.is_empty()).then(|| path.to_string());
                    self.save_geometry(true);
                }
                self.project = path.to_string();
                // Launched with a folder: the picture shows its session.
                if self.opening && !self.project.is_empty() {
                    self.opening = false;
                    self.schedule_snapshot();
                }
                self.window.set_title(&title(&self.project, self.waiting));
            }
            Some("pick-project") => self.pick_project(),
            Some("setup") => open_setup(),
            Some("logs") => self.open_log(),
            Some("retry") => self.retry(),
            // The page lost the daemon: find or start it again.
            Some("restart") => self.retry(),
            Some("preview") => self
                .preview_item
                .set_enabled(m["available"].as_bool().unwrap_or(false)),
            _ => {}
        }
    }

    fn schedule_snapshot(&mut self) {
        if let Some(out) = self.snapshot.take() {
            // Give the page a moment to list sessions and runs.
            let proxy = self.proxy.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(3));
                let _ = proxy.send_event(UserEvent::TakeSnapshot(out));
            });
        }
    }

    fn save_geometry(&mut self, force: bool) {
        if force {
            self.track_geometry();
        }
        if self.dirty && (force || self.saved_at.elapsed() > Duration::from_millis(500)) {
            let _ = self.geometry.save(&self.home.join("desktop.json"));
            self.dirty = false;
            self.saved_at = Instant::now();
        }
    }

    /// Remembered in logical units, so a Retina and a regular display agree.
    fn track_geometry(&mut self) {
        self.geometry.maximized = self.window.is_maximized();
        if !self.geometry.maximized && !self.window.is_minimized() {
            let scale = self.window.scale_factor();
            let size: LogicalSize<f64> = self.window.inner_size().to_logical(scale);
            (self.geometry.width, self.geometry.height) =
                (size.width.round() as u32, size.height.round() as u32);
            if let Ok(p) = self.window.outer_position() {
                let p: LogicalPosition<f64> = p.to_logical(scale);
                (self.geometry.x, self.geometry.y) =
                    (Some(p.x.round() as i32), Some(p.y.round() as i32));
            }
        }
        self.dirty = true;
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("chanto-desktop {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    // macOS passes `-psn_...` to apps opened from Finder on older systems.
    let folder = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(PathBuf::from)
        .map(|p| p.canonicalize().unwrap_or(p));
    let home = daemon::home();
    let instance = Instance::new(&home);
    if instance.forward(&folder.clone().map(Request::Open).unwrap_or(Request::Focus)) {
        return;
    }

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    {
        let proxy = proxy.clone();
        if let Err(e) = instance.listen(move |r| {
            let _ = proxy.send_event(UserEvent::Instance(r));
        }) {
            eprintln!("chanto-desktop: single-instance socket unavailable: {e}");
        }
    }
    {
        let proxy = proxy.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let _ = proxy.send_event(UserEvent::Menu(e.id().0.clone()));
        }));
    }

    let geometry = WindowState::load(&home.join("desktop.json"));
    let mut builder = WindowBuilder::new()
        .with_title("chanto")
        .with_inner_size(LogicalSize::new(geometry.width, geometry.height))
        .with_min_inner_size(LogicalSize::new(480.0, 360.0))
        .with_maximized(geometry.maximized);
    if let (Some(x), Some(y)) = (geometry.x, geometry.y) {
        builder = builder.with_position(LogicalPosition::new(x, y));
    }
    let window = builder.build(&event_loop).expect("creating the window");

    let (menu, preview_item) = build_menu();
    #[cfg(target_os = "macos")]
    menu.init_for_nsapp();
    #[cfg(windows)]
    {
        use tao::platform::windows::WindowExtWindows;
        let _ = unsafe { menu.init_for_hwnd(window.hwnd()) };
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use tao::platform::unix::WindowExtUnix;
        let _ = menu.init_for_gtk_window(window.gtk_window(), window.default_vbox());
    }

    let server = daemon::connect();
    let daemon_in_home = server.is_ok() && daemon::runs_in_home(&home);
    let token = server.as_ref().map(|s| s.token.clone()).unwrap_or_default();
    let origin = Arc::new(Mutex::new(server.as_ref().ok().map(|s| s.url.clone())));
    let (nav_origin, ipc_origin, ipc) = (origin.clone(), origin.clone(), proxy.clone());
    let mut builder = WebViewBuilder::new()
        // Nothing the page stores (the token included) outlives the window.
        .with_incognito(true)
        .with_background_color(theme_color(window.theme()))
        .with_permission_handler(|kind| match kind {
            PermissionKind::Microphone => PermissionResponse::Allow,
            _ => PermissionResponse::Deny,
        })
        .with_navigation_handler(move |url| {
            let current = nav_origin.lock().ok().and_then(|o| o.clone());
            let ok = navigation_allowed(current.as_deref(), &url);
            if !ok {
                open_externally(&url);
            }
            ok
        })
        .with_new_window_req_handler(|url, _| {
            open_externally(&url);
            NewWindowResponse::Deny
        })
        .with_ipc_handler(move |req| {
            let current = ipc_origin.lock().ok().and_then(|o| o.clone());
            if ipc_trusted(current.as_deref(), &req.uri().to_string()) {
                let _ = ipc.send_event(UserEvent::Ipc(req.body().clone()));
            }
        });
    builder = match &server {
        Ok(s) => builder
            .with_url(page_url(s, geometry.project.as_deref()))
            .with_initialization_script(init_script(s)),
        Err(e) => {
            let log = home.join("daemon.log");
            builder.with_html(error_page_with_log(
                e,
                log.is_file().then_some(log).as_deref(),
            ))
        }
    };
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    let webview = builder.build(&window).expect("creating the webview");
    #[cfg(all(unix, not(target_os = "macos")))]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        builder
            .build_gtk(window.default_vbox().expect("gtk container"))
            .expect("creating the webview")
    };

    let snapshot_to = std::env::var_os("CHANTO_DESKTOP_SNAPSHOT")
        .filter(|_| cfg!(target_os = "macos"))
        .map(PathBuf::from);
    let zoom = geometry.zoom;
    let mut app = App {
        window,
        webview,
        home,
        geometry,
        saved_at: Instant::now(),
        dirty: false,
        ready: false,
        origin,
        token,
        daemon_in_home,
        queued: vec![],
        waiting: 0,
        project: String::new(),
        snapshot: snapshot_to,
        opening: folder.is_some(),
        asked_project: false,
        preview_item,
        zoom,
        proxy: proxy.clone(),
    };
    // The zoom last used in this window (View → Zoom In/Out remembers it).
    if app.zoom != 1.0 {
        let _ = app.webview.zoom(app.zoom);
    }
    if let Some(dir) = folder {
        app.open_project(&dir);
    }
    let _menu = menu;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::Menu(id)) => {
                if app.on_menu(&id) {
                    app.save_geometry(true);
                    instance.cleanup();
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::Instance(r)) => {
                app.focus();
                if let Request::Open(dir) = r {
                    app.open_project(&dir);
                }
            }
            Event::UserEvent(UserEvent::Ipc(body)) => app.on_ipc(&body),
            Event::UserEvent(UserEvent::Retried(result)) => app.retried(result),
            #[cfg(target_os = "macos")]
            Event::UserEvent(UserEvent::TakeSnapshot(out)) => {
                snapshot(&app.webview, out, app.proxy.clone())
            }
            #[cfg(not(target_os = "macos"))]
            Event::UserEvent(UserEvent::TakeSnapshot(_)) => {}
            Event::UserEvent(UserEvent::Snapshot(result)) => {
                match result {
                    Ok(p) => println!("snapshot: {}", p.display()),
                    Err(e) => eprintln!("snapshot failed: {e}"),
                }
                app.save_geometry(true);
                instance.cleanup();
                *control_flow = ControlFlow::Exit;
            }
            // Folders dropped on the Dock icon, or `open -a chanto <dir>`.
            #[cfg(target_os = "macos")]
            Event::Opened { urls } => {
                for dir in urls.iter().filter_map(|u| u.to_file_path().ok()) {
                    app.open_project(&dir);
                }
            }
            Event::WindowEvent {
                event: WindowEvent::Resized(_) | WindowEvent::Moved(_),
                ..
            } => {
                app.track_geometry();
                app.save_geometry(false);
            }
            Event::WindowEvent {
                event: WindowEvent::ThemeChanged(theme),
                ..
            } => {
                let _ = app.webview.set_background_color(theme_color(theme));
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                app.save_geometry(true);
                instance.cleanup();
                *control_flow = ControlFlow::Exit;
            }
            Event::LoopDestroyed => {
                app.save_geometry(true);
                instance.cleanup();
            }
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_command_quotes_paths_with_spaces() {
        #[cfg(not(windows))]
        {
            assert_eq!(
                setup_command(Some(PathBuf::from("/usr/local/bin/chanto"))),
                "/usr/local/bin/chanto setup"
            );
            assert_eq!(
                setup_command(Some(PathBuf::from("/My Apps/chanto"))),
                "'/My Apps/chanto' setup"
            );
        }
        assert_eq!(setup_command(None), "chanto setup");
    }

    #[test]
    fn token_is_injected_only_into_the_daemons_origin() {
        let s = daemon::Server {
            url: "http://127.0.0.1:4141".into(),
            token: "t'\"</script>".into(),
        };
        let js = init_script(&s);
        assert!(
            js.starts_with("(() => { if (location.origin !== \"http://127.0.0.1:4141\") return;")
        );
        // JSON-encoded, so a token can't break out of the string.
        assert!(js.contains(r#"sessionStorage.setItem('chanto-token', "t'\"</script>")"#));
        assert!(!js.contains("#token="));
    }

    #[test]
    fn init_keeps_a_token_the_page_already_has() {
        let s = daemon::Server {
            url: "http://127.0.0.1:4141".into(),
            token: "abc".into(),
        };
        let js = init_script(&s);
        // A daemon restarted outside the app hands the page a new token in its
        // fragment; a later reload must keep that one, not this startup copy.
        assert!(js.contains("if (!sessionStorage.getItem('chanto-token'))"));
    }

    #[test]
    fn navigation_stays_on_the_daemon() {
        let o = "http://127.0.0.1:4141";
        for ok in [
            "http://127.0.0.1:4141",
            "http://127.0.0.1:4141/",
            "http://127.0.0.1:4141/#run=abc",
            "http://127.0.0.1:4141/app.js",
        ] {
            assert!(same_origin(o, ok), "{ok}");
        }
        for bad in [
            "http://127.0.0.1:41410/",
            "http://127.0.0.1:4141.evil.com/",
            "https://github.com/",
            "file:///etc/passwd",
        ] {
            assert!(!same_origin(o, bad), "{bad}");
        }
        // Before a daemon exists only the shell's error page may load.
        assert!(navigation_allowed(None, "about:blank"));
        assert!(navigation_allowed(None, "data:text/html,hi"));
        assert!(!navigation_allowed(None, "https://example.com/"));
        assert!(navigation_allowed(Some(o), "http://127.0.0.1:4141/app.js"));
        assert!(!navigation_allowed(Some(o), "https://example.com/"));
        // The error page may talk to the shell; other pages may not.
        assert!(ipc_trusted(None, "about:blank"));
        assert!(ipc_trusted(Some(o), "http://127.0.0.1:4141/"));
        assert!(!ipc_trusted(Some(o), "https://example.com/"));
    }

    #[test]
    fn a_launch_without_a_live_project_asks_for_one() {
        assert!(needs_project(None));
        assert!(needs_project(Some("")));
        assert!(needs_project(Some("/definitely/not/here")));
        let dir = std::env::temp_dir();
        assert!(!needs_project(Some(dir.to_str().unwrap())));
    }

    #[test]
    fn hooks_and_titles() {
        assert_eq!(
            hook("openProject", Some("/a \"b\"")),
            r#"window.chanto && window.chanto.openProject("/a \"b\"");"#
        );
        assert_eq!(
            hook("newSession", None),
            "window.chanto && window.chanto.newSession();"
        );
        assert_eq!(title("", 0), "chanto");
        assert_eq!(title("/Users/me/api", 2), "api — chanto · 2 waiting");
        assert!(error_page("<b>x</b>").contains("&lt;b&gt;x&lt;/b&gt;"));
    }

    #[test]
    fn a_project_named_chanto_does_not_repeat_in_the_title() {
        assert_eq!(title("/Users/me/chanto", 0), "chanto");
        assert_eq!(title("/Users/me/chanto", 1), "chanto · 1 waiting");
    }

    #[test]
    fn the_error_page_offers_a_retry_and_the_log() {
        let page = error_page_with_log("<b>x</b>", None);
        assert!(page.contains("&lt;b&gt;x&lt;/b&gt;"));
        // The error page can start the daemon without reopening the app.
        assert!(page.contains("id=retry"));
        assert!(page.contains("Try again"));
        assert!(page.contains("type: 'retry'"));
        // Commands in backticks read as code, not as literal backticks.
        let page = error_page_with_log("`chanto daemon start` failed", None);
        assert!(page.contains("<code>chanto daemon start</code>"));
        assert!(!page.contains("`chanto"));
        // A log that exists gets a button that opens it.
        assert!(!page.contains("id=logs"));
        let logged = error_page_with_log("it broke", Some(Path::new("/tmp/daemon.log")));
        assert!(logged.contains("id=logs"));
        assert!(logged.contains("Open log"));
        assert!(logged.contains("type: 'logs'"));
        // It reports itself, so CHANTO_DESKTOP_SNAPSHOT doesn't wait forever.
        assert!(page.contains("type: 'error-page'"));
    }

    #[test]
    fn the_error_page_carries_the_app_mark() {
        let page = error_page("it broke");
        assert!(page.contains("class=mark"));
        assert!(page.contains(r#"aria-label="chanto logo""#));
    }

    #[test]
    fn a_page_url_carries_the_project_and_a_recovered_one_its_token() {
        let server = daemon::Server {
            url: "http://127.0.0.1:4141".into(),
            token: "abc".into(),
        };
        assert_eq!(page_url(&server, None), "http://127.0.0.1:4141/");
        let dir = std::env::temp_dir();
        let dir = dir.to_str().unwrap();
        assert_eq!(
            page_url(&server, Some(dir)),
            format!("http://127.0.0.1:4141/#project={dir}")
        );
        // A folder that no longer exists is not reopened.
        assert_eq!(
            page_url(&server, Some("/definitely/not/here")),
            "http://127.0.0.1:4141/"
        );
        assert_eq!(
            retry_url(&server, Some(dir)),
            format!("http://127.0.0.1:4141/#token=abc&project={dir}")
        );
        assert_eq!(retry_url(&server, None), "http://127.0.0.1:4141/#token=abc");
        assert_eq!(fragment_value("/Users/me/My App"), "/Users/me/My%20App");
        assert_eq!(fragment_value("a/b?c#d&e"), "a/b%3Fc%23d%26e");
    }
}
