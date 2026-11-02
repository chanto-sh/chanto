//! The whole TUI in a child process whose terminal is a pty, provided by the
//! system's `script` program: real terminal setup and restore, real key input
//! and a scripted model. The child is this test binary running one of the
//! `*_child` tests below.

use super::*;
use crate::agent::tests::{mock_server, tmpdir};
use crate::test_support::shim;
use serde_json::json;
use std::io::{Read, Write};
use std::process::Stdio;
use std::sync::Mutex;

const CHILD_ENV: &str = "CHANTO_PTY_TEST_CHILD";

struct Pty {
    input: std::process::ChildStdin,
    host: std::process::Child,
    screen: Arc<Mutex<Vec<u8>>>,
    home: std::path::PathBuf,
}

/// Terminal bytes without escape sequences, to look for text on screen.
fn plain(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.next_if_eq(&'[').is_some() {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            chars.next();
        }
    }
    out
}

impl Pty {
    fn start(test: &str, model: &str, cwd: &std::path::Path) -> Pty {
        let home = tmpdir("pty-home");
        std::fs::write(
            home.join("config.toml"),
            "[compat]\nclaude = false\ncodex = false\n",
        )
        .unwrap();
        // Sizes the pty and records the child's exit status, which `script`
        // doesn't pass on everywhere.
        let wrapper = home.join("wrapper");
        shim(
            &wrapper,
            &format!(
                "[ $# -eq 0 ] && exit 0\nstty rows 30 cols 100\n'{}' --exact {test} --nocapture --test-threads=1\necho $? > '{}/status'",
                std::env::current_exe().unwrap().display(),
                home.display()
            ),
        );
        let wrapper = wrapper.display().to_string();
        // The status line asks `gh` about pull requests; this one knows none.
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        shim(&bin.join("gh"), "exit 1");
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = std::process::Command::new("script");
        if cfg!(target_os = "macos") {
            cmd.args(["-q", "/dev/null", &wrapper, "go"]);
        } else {
            cmd.args(["-qfc", &format!("{wrapper} go"), "/dev/null"]);
        }
        let mut host = cmd
            .current_dir(cwd)
            .env(CHILD_ENV, model)
            .env("CHANTO_HOME", &home)
            .env("CHANTO_COMPAT_HOME", &home)
            .env("HOME", &home)
            .env("TERM", "xterm-256color")
            .env("PATH", path)
            // No update check or price download: nothing leaves the machine.
            .env("CHANTO_NO_UPDATE_CHECK", "1")
            .env("CI", "1")
            .env("CODEX_HOME", home.join(".codex"))
            .env_remove("TMUX")
            .env_remove("CHANTO_MODEL")
            .env_remove("CHANTO_BASE_URL")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the system's `script` program");
        let input = host.stdin.take().unwrap();
        let mut reader = host.stdout.take().unwrap();
        let screen: Arc<Mutex<Vec<u8>>> = Default::default();
        let s2 = screen.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                s2.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Pty {
            input,
            host,
            screen,
            home,
        }
    }

    fn bytes(&self) -> Vec<u8> {
        self.screen.lock().unwrap().clone()
    }

    fn text(&self) -> String {
        plain(&self.bytes())
    }

    async fn wait_for(&self, what: &str) {
        // Redraws move the cursor over blank cells instead of printing spaces.
        let squeeze = |t: &str| t.split_whitespace().collect::<String>();
        let what_squeezed = squeeze(what);
        for _ in 0..3000 {
            if squeeze(&self.text()).contains(&what_squeezed) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("never saw {what:?} on screen:\n{}", self.text());
    }

    /// Like `wait_for`, in text printed after this call was made.
    async fn wait_for_any_case(&self, what: &str) {
        let start = self.bytes().len();
        let what = what.to_lowercase().replace(' ', "");
        for _ in 0..3000 {
            let new = plain(&self.bytes()[start..]);
            let new: String = new.split_whitespace().collect();
            if new.to_lowercase().contains(&what) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "never saw {what:?} on screen:\n{}",
            plain(&self.bytes()[start..])
        );
    }

    fn keys(&mut self, keys: &str) {
        self.input.write_all(keys.as_bytes()).unwrap();
        self.input.flush().unwrap();
    }

    /// Send a signal to the TUI process, which records its pid at startup.
    fn signal(&self, name: &str) {
        let pid = std::fs::read_to_string(self.home.join("pid")).unwrap();
        let ok = std::process::Command::new("kill")
            .args([name, pid.trim()])
            .status()
            .unwrap()
            .success();
        assert!(ok, "kill {name} {pid}");
    }

    /// Wait for the child's exit; its exit code and the raw terminal bytes.
    async fn finish(self) -> (String, Vec<u8>) {
        let path = self.home.join("status");
        for _ in 0..3000 {
            if let Ok(code) = std::fs::read_to_string(&path)
                && !code.trim().is_empty()
            {
                // Whatever is still on its way through the pty.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let bytes = self.bytes();
                let _ = std::fs::remove_dir_all(&self.home);
                return (code.trim().to_string(), bytes);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the TUI didn't exit:\n{}", self.text());
    }
}

/// A failed test must not leave its TUI running.
impl Drop for Pty {
    fn drop(&mut self) {
        if let Ok(pid) = std::fs::read_to_string(self.home.join("pid")) {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", pid.trim()])
                .stderr(Stdio::null())
                .status();
        }
        let _ = self.host.kill();
        let _ = self.host.wait();
    }
}

/// Run the TUI as the CLI would, in the child.
async fn run_tui(extra: &[&str]) {
    let model = std::env::var(CHILD_ENV).unwrap();
    let mut args = vec![
        "chanto",
        "--provider",
        "openai",
        "--base-url",
        &model,
        "--model",
        "mock",
        "--mode",
        "bypass",
    ];
    args.extend_from_slice(extra);
    let cli = <Cli as clap::Parser>::parse_from(args);
    let home = std::env::var("CHANTO_HOME").unwrap();
    std::fs::write(
        std::path::Path::new(&home).join("pid"),
        std::process::id().to_string(),
    )
    .unwrap();
    run(cli).await.unwrap();
}

#[tokio::test]
async fn fullscreen_child() {
    if std::env::var_os(CHILD_ENV).is_some() {
        run_tui(&[]).await;
    }
}

#[tokio::test]
async fn inline_child() {
    if std::env::var_os(CHILD_ENV).is_some() {
        run_tui(&["--inline"]).await;
    }
}

const ANSWER: &str = "PTY-ANSWER";

#[tokio::test]
async fn a_full_screen_session_answers_and_restores_the_terminal() {
    if std::env::var_os(CHILD_ENV).is_some() {
        return;
    }
    let (model, seen) = mock_server(vec![json!({"content": ANSWER})]).await;
    let dir = tmpdir("pty-full");
    let mut pty = Pty::start("tui::pty_tests::fullscreen_child", &model, &dir);
    pty.wait_for("mock").await;
    pty.keys("say hello\r");
    pty.wait_for(ANSWER).await;
    let request = seen.lock().unwrap()[0]["messages"].to_string();
    assert!(request.contains("say hello"), "{request}");

    pty.keys("/exit\r");
    let (code, bytes) = pty.finish().await;
    assert_eq!(code, "0", "{}", plain(&bytes));
    let raw = String::from_utf8_lossy(&bytes);
    assert!(raw.contains("\x1b[?1049h"), "entered the alternate screen");
    assert!(raw.contains("\x1b[?1049l"), "left the alternate screen");
    assert!(
        raw.contains("\x1b[?2004h") && raw.contains("\x1b[?2004l"),
        "paste mode undone"
    );
    let mouse_on = raw.find("\x1b[?1000h").expect("mouse capture on");
    let mouse_off = raw.rfind("\x1b[?1000l").expect("mouse capture off");
    assert!(mouse_on < mouse_off);
}

#[tokio::test]
async fn an_inline_session_stays_on_the_normal_screen() {
    if std::env::var_os(CHILD_ENV).is_some() {
        return;
    }
    let (model, _) = mock_server(vec![json!({"content": ANSWER})]).await;
    let dir = tmpdir("pty-inline");
    let mut pty = Pty::start("tui::pty_tests::inline_child", &model, &dir);
    pty.wait_for("mock").await;
    pty.keys("hello inline\r");
    pty.wait_for(ANSWER).await;
    pty.keys("/exit\r");
    let (code, bytes) = pty.finish().await;
    assert_eq!(code, "0", "{}", plain(&bytes));
    let raw = String::from_utf8_lossy(&bytes);
    assert!(!raw.contains("\x1b[?1049h"), "inline keeps the scrollback");
    assert!(raw.contains("\x1b[?2004l"), "paste mode undone");
}

#[tokio::test]
async fn a_termination_signal_restores_the_terminal() {
    if std::env::var_os(CHILD_ENV).is_some() {
        return;
    }
    let (model, _) = mock_server(vec![]).await;
    let dir = tmpdir("pty-term");
    let pty = Pty::start("tui::pty_tests::fullscreen_child", &model, &dir);
    pty.wait_for("mock").await;
    pty.signal("-TERM");
    let (_, bytes) = pty.finish().await;
    let raw = String::from_utf8_lossy(&bytes);
    assert!(raw.contains("\x1b[?1049l"), "left the alternate screen");
}

#[tokio::test]
async fn remote_control_starts_reports_and_stops_from_the_session() {
    if std::env::var_os(CHILD_ENV).is_some() {
        return;
    }
    let (model, _) = mock_server(vec![]).await;
    let dir = tmpdir("pty-remote");
    let mut pty = Pty::start("tui::pty_tests::fullscreen_child", &model, &dir);
    pty.wait_for("mock").await;
    pty.keys("/remote status\r");
    pty.wait_for("Remote control is off.").await;
    pty.keys("/remote local\r");
    pty.wait_for("127.0.0.1:").await;
    pty.keys("/remote off\r");
    pty.wait_for("no longer works").await;
    pty.keys("/exit\r");
    let (code, bytes) = pty.finish().await;
    assert_eq!(code, "0", "{}", plain(&bytes));
}

#[tokio::test]
async fn plain_child() {
    if let Ok(model) = std::env::var(CHILD_ENV) {
        let cli = <Cli as clap::Parser>::parse_from([
            "chanto",
            "--provider",
            "openai",
            "--base-url",
            &model,
            "--model",
            "mock",
            "--mode",
            "bypass",
            "--plain",
        ]);
        let home = std::env::var("CHANTO_HOME").unwrap();
        std::fs::write(
            std::path::Path::new(&home).join("pid"),
            std::process::id().to_string(),
        )
        .unwrap();
        crate::repl::run(cli).await.unwrap();
    }
}

#[tokio::test]
async fn the_plain_prompt_answers_runs_commands_and_exits() {
    if std::env::var_os(CHILD_ENV).is_some() {
        return;
    }
    let (model, seen) = mock_server(vec![json!({"content": ANSWER})]).await;
    let dir = tmpdir("pty-plain");
    let mut pty = Pty::start("tui::pty_tests::plain_child", &model, &dir);
    pty.wait_for("mock").await;
    pty.keys("say hello\r");
    pty.wait_for(ANSWER).await;
    assert!(
        seen.lock().unwrap()[0]["messages"]
            .to_string()
            .contains("say hello")
    );
    pty.keys("/help\r");
    pty.wait_for("/exit").await;
    pty.keys("/exit\r");
    let (code, bytes) = pty.finish().await;
    assert_eq!(code, "0", "{}", plain(&bytes));
    assert!(
        !String::from_utf8_lossy(&bytes).contains("\x1b[?1049h"),
        "no alternate screen"
    );
}

/// The panels a session opens. Redraws only print the cells that changed, so
/// each check looks for one word.
#[tokio::test]
async fn session_commands_open_their_panels() {
    if std::env::var_os(CHILD_ENV).is_some() {
        return;
    }
    let (model, _) = mock_server(vec![json!({"content": ANSWER})]).await;
    let dir = tmpdir("pty-panels");
    let mut pty = Pty::start("tui::pty_tests::fullscreen_child", &model, &dir);
    pty.wait_for("mock").await;
    // A saved turn, so the resume and rewind pickers have something to list.
    pty.keys("first prompt\r");
    pty.wait_for(ANSWER).await;
    for (command, expected) in [
        ("/hooks\r", "hooks"),
        ("/agents\r", "agent"),
        ("/remote status\r", "off"),
        ("/goal list\r", "goals"),
        ("/team\r", "team"),
        ("/mcp\r", "MCP"),
    ] {
        pty.keys(command);
        pty.wait_for_any_case(expected).await;
    }
    // The MCP panel stays open until dismissed.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    pty.keys("\x1b");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    // The resume picker lists the saved turn; Escape closes it. The pause keeps
    // the Escape from reading as Alt with the next key.
    pty.keys("/resume\r");
    pty.wait_for_any_case("prompt").await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    pty.keys("\x1b");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    pty.keys("/exit\r");
    let (code, bytes) = pty.finish().await;
    assert_eq!(code, "0", "{}", plain(&bytes));
}
