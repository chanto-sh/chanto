//! `chanto setup`'s picker, run as the real binary inside a pseudo-terminal so
//! it sees the terminal it checks for. Covers choosing a credentialed provider
//! straight after login and pasting a key for one, with a throwaway home for
//! every run and no network beyond 127.0.0.1.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_chanto")
}

/// A project dir with its own home (`CHANTO_HOME`, and the other agents'
/// config homes), removed on drop.
struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("chanto-setup-{}-{nanos}-{n}", std::process::id()));
        std::fs::create_dir_all(dir.join("home")).expect("create sandbox");
        Sandbox { dir }
    }

    fn home(&self) -> PathBuf {
        self.dir.join("home")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `chanto setup --plain` as the binary in a pseudo-terminal.
#[cfg(unix)]
struct SetupPty {
    child: std::process::Child,
    input: std::process::ChildStdin,
    lines: std::sync::mpsc::Receiver<String>,
    home: PathBuf,
    seen: String,
}

#[cfg(unix)]
fn setup_pty(s: &Sandbox, env: &[(&str, &str)]) -> SetupPty {
    let wrapper = s.dir.join("wrapper");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nstty rows 40 cols 240\nexec '{}' setup --plain\n",
            bin()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut cmd = Command::new("script");
    if cfg!(target_os = "macos") {
        cmd.args(["-q", "/dev/null", wrapper.to_str().unwrap()]);
    } else {
        cmd.args(["-qfc", wrapper.to_str().unwrap(), "/dev/null"]);
    }
    let mut child = cmd
        .current_dir(&s.dir)
        .env("CHANTO_HOME", s.home())
        .env("CHANTO_COMPAT_HOME", s.home())
        .env("CODEX_HOME", s.home().join(".codex"))
        .env("CHANTO_NO_UPDATE_CHECK", "1")
        .env("TERM", "xterm-256color")
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start chanto under script");
    let input = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    SetupPty {
        child,
        input,
        lines,
        home: s.home(),
        seen: String::new(),
    }
}

#[cfg(unix)]
impl SetupPty {
    /// The next printed line containing `needle`, or a panic naming everything
    /// printed so far.
    fn expect(&mut self, needle: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            let Ok(line) = self
                .lines
                .recv_timeout(std::time::Duration::from_millis(200))
            else {
                continue;
            };
            self.seen.push_str(&line);
            self.seen.push('\n');
            if line.contains(needle) {
                return line;
            }
        }
        panic!("never saw {needle:?}:\n{}", self.seen);
    }

    /// Answers a numbered choice, using the number that starts `line`.
    fn pick(&mut self, line: &str) {
        use std::io::Write;
        let n: String = line
            .trim()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        assert!(!n.is_empty(), "not a numbered choice: {line}");
        writeln!(self.input, "{n}").unwrap();
        self.input.flush().unwrap();
    }

    /// Answers the API key prompt, which reads raw key events: Enter is a
    /// carriage return there, not the newline a line reader wants.
    fn key(&mut self, key: &str) {
        use std::io::Write;
        write!(self.input, "{key}\r").unwrap();
        self.input.flush().unwrap();
    }

    /// Waits for the chosen model to reach the config.
    fn wait_saved(&mut self, needle: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            if std::fs::read_to_string(self.home.join("config.toml"))
                .is_ok_and(|c| c.contains(needle))
            {
                return;
            }
            // Keep draining, so the child never blocks on a full pipe.
            if let Ok(line) = self
                .lines
                .recv_timeout(std::time::Duration::from_millis(100))
            {
                self.seen.push_str(&line);
                self.seen.push('\n');
            }
        }
        panic!("config never contained {needle:?}:\n{}", self.seen);
    }
}

#[cfg(unix)]
impl Drop for SetupPty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Pasting a key for an ordinary provider saves the key and the provider's
/// default model.
#[cfg(unix)]
#[test]
fn setup_picker_takes_a_pasted_key_for_a_provider() {
    let s = Sandbox::new();
    let mut picker = setup_pty(&s, &[]);
    let line = picker.expect("Paste an API key for a provider");
    picker.pick(&line);
    let line = picker.expect("openrouter");
    picker.pick(&line);
    // The key prompt switches the terminal into raw mode before reading; if we
    // write first, canonical mode turns the carriage return into a newline.
    std::thread::sleep(std::time::Duration::from_secs(1));
    picker.key("sk-or-test-never-sent");
    picker.wait_saved("openrouter/anthropic/claude-sonnet-5.5");
}

/// A provider with credentials but no known default model is still offered, so
/// setup can ask its catalog. It used to be hidden from the picker entirely.
#[cfg(unix)]
#[test]
fn setup_picker_offers_a_credentialed_provider_without_a_default() {
    let s = Sandbox::new();
    let mut picker = setup_pty(&s, &[("COHERE_API_KEY", "sk-test-never-sent")]);
    let line = picker.expect("Cohere");
    assert!(line.contains("Credential available"), "{line}");
}
