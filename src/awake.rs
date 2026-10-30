//! Keeping the machine awake while Chanto works. A long turn, a `/loop` or a
//! background run dies quietly when the laptop idles into sleep; holding a
//! [`Guard`] for as long as the work lasts prevents that, and releasing the
//! last one lets the machine sleep again, so Chanto never keeps it up while
//! it sits at the prompt waiting for you.
//!
//! macOS: an IOKit power assertion against idle sleep, what `caffeinate -i`
//! takes, without starting a process for it (the system drops it if Chanto
//! dies). Linux: `systemd-inhibit --what=idle:sleep`. Windows:
//! `SetThreadExecutionState(ES_SYSTEM_REQUIRED)` on a thread that lives as long
//! as the hold. None of these can stop a laptop lid from forcing sleep.
//!
//! A hold only takes effect once work has gone on for [`DELAY`]: no system
//! idles into sleep sooner (the shortest setting is a minute), so a quick
//! turn, like a one-shot question, never pays for one.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

/// How long work goes on before the machine is kept awake (short in tests).
#[cfg(not(test))]
const DELAY: Duration = Duration::from_secs(20);
#[cfg(test)]
const DELAY: Duration = Duration::from_millis(100);

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
    /// Counts the times holding started from none, so a delayed start meant
    /// for work that has since ended doesn't fire.
    epoch: u64,
    /// Inhibitors started, in tests, which never start a real one.
    #[cfg(test)]
    started: u64,
}
static STATE: Mutex<State> = Mutex::new(State {
    holders: 0,
    inhibitor: None,
    session: false,
    epoch: 0,
    #[cfg(test)]
    started: 0,
});

fn state() -> std::sync::MutexGuard<'static, State> {
    lock(&STATE)
}

fn lock(cell: &'static Mutex<State>) -> std::sync::MutexGuard<'static, State> {
    cell.lock().unwrap_or_else(|e| e.into_inner())
}

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
    let mut s = state();
    if policy == Policy::Always && !s.session {
        s.session = true;
        acquire(&STATE, &mut s, Duration::ZERO);
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
    acquire(&STATE, &mut state(), DELAY);
    Guard { held: true }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.held {
            release(&mut state());
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

/// Take a hold on `cell` (locked as `s`); the first one starts the inhibitor
/// after `delay`, if work is still going on then.
fn acquire(cell: &'static Mutex<State>, s: &mut State, delay: Duration) {
    s.holders += 1;
    if s.holders != 1 {
        return;
    }
    s.epoch += 1;
    if delay.is_zero() {
        start(s);
        return;
    }
    let epoch = s.epoch;
    let _ = std::thread::Builder::new()
        .name("keep-awake".into())
        .spawn(move || {
            std::thread::sleep(delay);
            let mut s = lock(cell);
            if s.holders > 0 && s.epoch == epoch && s.inhibitor.is_none() {
                start(&mut s);
            }
        });
}

fn start(s: &mut State) {
    #[cfg(test)]
    {
        s.started += 1;
    }
    if !cfg!(test) {
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
    #[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
    Child(std::process::Child),
    #[cfg(windows)]
    Thread(std::sync::mpsc::Sender<()>),
    #[cfg(target_os = "macos")]
    Assertion(u32),
}

#[cfg(target_os = "macos")]
mod iokit {
    use std::ffi::c_void;
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        pub fn IOPMAssertionCreateWithName(
            kind: *const c_void,
            level: u32,
            name: *const c_void,
            id: *mut u32,
        ) -> i32;
        pub fn IOPMAssertionRelease(id: u32) -> i32;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithBytes(
            alloc: *const c_void,
            bytes: *const u8,
            len: isize,
            encoding: u32,
            external: u8,
        ) -> *const c_void;
        pub fn CFRelease(cf: *const c_void);
    }
    /// A CFString the caller releases.
    pub fn cfstr(s: &str) -> *const c_void {
        const UTF8: u32 = 0x0800_0100;
        unsafe { CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as isize, UTF8, 0) }
    }
}

impl Inhibitor {
    #[cfg(target_os = "macos")]
    fn start() -> Option<Self> {
        // `kIOPMAssertionTypePreventUserIdleSystemSleep` at `kIOPMAssertionLevelOn`.
        let kind = iokit::cfstr("PreventUserIdleSystemSleep");
        let name = iokit::cfstr("chanto is working");
        if kind.is_null() || name.is_null() {
            return None;
        }
        let mut id = 0u32;
        let ok = unsafe { iokit::IOPMAssertionCreateWithName(kind, 255, name, &mut id) } == 0;
        unsafe {
            iokit::CFRelease(kind);
            iokit::CFRelease(name);
        }
        ok.then_some(Self::Assertion(id))
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
            #[cfg(target_os = "macos")]
            Self::Assertion(_) => true,
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
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
            #[cfg(target_os = "macos")]
            Self::Assertion(id) => unsafe {
                iokit::IOPMAssertionRelease(*id);
            },
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

    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_assertion_holds_until_dropped() {
        let listed = || {
            std::process::Command::new("pmset")
                .args(["-g", "assertions"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains("chanto is working"))
                .unwrap_or(false)
        };
        let i = Inhibitor::start().expect("power assertion");
        assert!(matches!(i, Inhibitor::Assertion(_)));
        assert!(listed(), "pmset lists the assertion while it's held");
        drop(i);
        assert!(!listed(), "released on drop");
    }

    #[test]
    fn a_short_hold_never_starts_the_inhibitor() {
        // Its own state: turns in other tests take holds on the shared one.
        static CELL: Mutex<State> = Mutex::new(State {
            holders: 0,
            inhibitor: None,
            session: false,
            epoch: 0,
            started: 0,
        });
        let take = || acquire(&CELL, &mut lock(&CELL), DELAY);
        let drop_one = || release(&mut lock(&CELL));
        let started = || lock(&CELL).started;
        take();
        drop_one();
        std::thread::sleep(DELAY * 3);
        assert_eq!(started(), 0, "a hold dropped before the delay started one");
        // One that lasts past the delay does, once.
        take();
        take();
        std::thread::sleep(DELAY * 3);
        assert_eq!(started(), 1);
        drop_one();
        drop_one();
        // A later hold starts its own after the delay, not at once.
        take();
        assert_eq!(started(), 1);
        std::thread::sleep(DELAY * 3);
        assert_eq!(started(), 2);
        drop_one();
        // `keep_awake = "always"` starts at once.
        acquire(&CELL, &mut lock(&CELL), Duration::ZERO);
        assert_eq!(started(), 3);
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
