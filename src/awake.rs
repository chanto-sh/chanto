//! Keeping the machine awake while Chanto works. A long turn, a `/loop` or a
//! background run dies quietly when the laptop idles into sleep; holding a
//! [`Guard`] for as long as the work lasts prevents that, and releasing the
//! last one lets the machine sleep again, so Chanto never keeps it up while
//! it sits at the prompt waiting for you.
//!
//! macOS: `caffeinate -i -w <pid>` (idle sleep only; it also exits if Chanto
//! dies). Linux: `systemd-inhibit --what=idle:sleep`. Windows:
//! `SetThreadExecutionState(ES_SYSTEM_REQUIRED)` on a thread that lives as long
//! as the hold. None of these can stop a laptop lid from forcing sleep.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

/// `keep_awake` in config: while working (default), never, or for the whole session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    Working,
    Never,
    Always,
}

impl Policy {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "working" | "auto" | "true" | "on" => Some(Self::Working),
            "never" | "off" | "false" => Some(Self::Never),
            "always" => Some(Self::Always),
            _ => None,
        }
    }
}

const WORKING: u8 = 0;
const NEVER: u8 = 1;
const ALWAYS: u8 = 2;
static POLICY: AtomicU8 = AtomicU8::new(WORKING);

struct State {
    holders: usize,
    inhibitor: Option<Inhibitor>,
    /// The hold `Always` takes for the whole session.
    session: bool,
}
static STATE: Mutex<State> = Mutex::new(State {
    holders: 0,
    inhibitor: None,
    session: false,
});

/// Set the policy from config; `Always` takes a hold that lasts until exit.
pub fn init(policy: Policy) {
    POLICY.store(
        match policy {
            Policy::Working => WORKING,
            Policy::Never => NEVER,
            Policy::Always => ALWAYS,
        },
        Ordering::Relaxed,
    );
    let mut s = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if policy == Policy::Always && !s.session {
        s.session = true;
        acquire(&mut s);
    }
}

/// Keeps the machine awake until dropped (unless `keep_awake = "never"`).
#[must_use = "the machine may sleep as soon as the guard is dropped"]
pub struct Guard {
    held: bool,
}

pub fn hold() -> Guard {
    if POLICY.load(Ordering::Relaxed) == NEVER {
        return Guard { held: false };
    }
    let mut s = STATE.lock().unwrap_or_else(|e| e.into_inner());
    acquire(&mut s);
    Guard { held: true }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.held {
            let mut s = STATE.lock().unwrap_or_else(|e| e.into_inner());
            release(&mut s);
        }
    }
}

/// Whether Chanto is keeping the machine awake right now (for the status line).
pub fn active() -> bool {
    STATE
        .lock()
        .map(|s| s.inhibitor.as_ref().is_some_and(Inhibitor::alive))
        .unwrap_or(false)
}

fn acquire(s: &mut State) {
    s.holders += 1;
    if s.holders == 1 && !cfg!(test) {
        s.inhibitor = Inhibitor::start();
    }
}

fn release(s: &mut State) {
    s.holders = s.holders.saturating_sub(1);
    if s.holders == 0 {
        s.inhibitor = None;
    }
}

/// The platform's keep-awake mechanism; dropping it lets the machine sleep.
enum Inhibitor {
    #[cfg_attr(windows, allow(dead_code))]
    Child(std::process::Child),
    #[cfg(windows)]
    Thread(std::sync::mpsc::Sender<()>),
}

impl Inhibitor {
    #[cfg(target_os = "macos")]
    fn start() -> Option<Self> {
        let pid = std::process::id().to_string();
        quiet(std::process::Command::new("caffeinate").args(["-i", "-w", &pid])).map(Self::Child)
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn start() -> Option<Self> {
        // `tail --pid` ends the inhibitor if Chanto dies without dropping it.
        let pid = std::process::id().to_string();
        quiet(std::process::Command::new("systemd-inhibit").args([
            "--what=idle:sleep",
            "--who=chanto",
            "--why=chanto is working",
            "--mode=block",
            "tail",
            "--pid",
            &pid,
            "-f",
            "/dev/null",
        ]))
        .map(Self::Child)
    }

    #[cfg(windows)]
    fn start() -> Option<Self> {
        use windows_sys::Win32::System::Power::{
            ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
        };
        // The execution state belongs to the thread that set it, so one thread
        // holds it and clears it when told to stop (or when Chanto exits).
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("keep-awake".into())
            .spawn(move || {
                unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
                let _ = rx.recv();
                unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
            })
            .ok()?;
        Some(Self::Thread(tx))
    }

    fn alive(&self) -> bool {
        match self {
            Self::Child(_) => true,
            #[cfg(windows)]
            Self::Thread(_) => true,
        }
    }
}

#[cfg(unix)]
fn quiet(cmd: &mut std::process::Command) -> Option<std::process::Child> {
    use std::process::Stdio;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        match self {
            Self::Child(c) => {
                let _ = c.kill();
                let _ = c.wait();
            }
            #[cfg(windows)]
            Self::Thread(tx) => {
                let _ = tx.send(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_parses_the_documented_values() {
        assert_eq!(Policy::parse("working"), Some(Policy::Working));
        assert_eq!(Policy::parse(" Never "), Some(Policy::Never));
        assert_eq!(Policy::parse("always"), Some(Policy::Always));
        assert_eq!(Policy::parse("off"), Some(Policy::Never));
        assert_eq!(Policy::parse("sometimes"), None);
    }

    #[test]
    fn holds_nest_and_the_last_release_lets_go() {
        // Tests never start a real inhibitor; this checks the counting.
        let count = || STATE.lock().unwrap().holders;
        let before = count();
        let a = hold();
        let b = hold();
        assert!(count() >= before + 2);
        drop(a);
        drop(b);
        assert_eq!(count(), before);
    }
}
