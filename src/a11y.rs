//! Accessibility switches read process-wide: a screen-reader mode that falls
//! back to the line-based frontend, and reduced motion (no animation) that the
//! TUI and the spinner honor. Both can be set by config, a flag or an env var,
//! and reduced motion also follows the system's "Reduce motion" setting.

use std::sync::atomic::{AtomicBool, Ordering};

static SCREEN_READER: AtomicBool = AtomicBool::new(false);
static REDUCE_MOTION: AtomicBool = AtomicBool::new(false);

/// Line-based output, no in-place redraws, and an audible cue: for screen
/// readers and anyone who needs strictly linear output.
pub fn screen_reader() -> bool {
    SCREEN_READER.load(Ordering::Relaxed)
}

/// Skip animation (a steady spinner, no ticking clock or live counters): easier
/// on screen readers, slow links, and eyes that dislike motion.
pub fn reduce_motion() -> bool {
    REDUCE_MOTION.load(Ordering::Relaxed)
}

/// Resolve once at startup, before the frontend is chosen or any spinner starts.
/// `screen_reader`/`animations` are the config (or flag) values; a set
/// `CHANTO_AX_SCREEN_READER` / `CLAUDE_AX_SCREEN_READER` / `CHANTO_SCREEN_READER`
/// or `CHANTO_REDUCE_MOTION` also turns them on.
pub fn init(screen_reader: Option<bool>, animations: Option<bool>) {
    let env_on = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty() && v != "0");
    // The system check spawns `defaults`, so it is lazy: an explicit config,
    // flag or env var answers the question without it.
    let (sr, motion_off) = resolve(screen_reader, animations, env_on, system_reduce_motion);
    SCREEN_READER.store(sr, Ordering::Relaxed);
    REDUCE_MOTION.store(motion_off, Ordering::Relaxed);
    if sr {
        // Subagents and hooks are child processes (some written for Claude Code,
        // which names this variable): let both inherit the mode.
        // SAFETY: called during startup, before threads read the environment.
        unsafe {
            std::env::set_var("CHANTO_SCREEN_READER", "1");
            std::env::set_var("CLAUDE_AX_SCREEN_READER", "1");
        }
    }
}

/// The pure decision, split out so it can be tested without touching the
/// process-wide flags or the real environment.
fn resolve(
    screen_reader: Option<bool>,
    animations: Option<bool>,
    env_on: impl Fn(&str) -> bool,
    system_reduce_motion: impl FnOnce() -> bool,
) -> (bool, bool) {
    let sr = screen_reader.unwrap_or(false)
        || env_on("CHANTO_AX_SCREEN_READER")
        || env_on("CLAUDE_AX_SCREEN_READER")
        || env_on("CHANTO_SCREEN_READER");
    // A screen reader forces motion off; otherwise an explicit config/flag wins,
    // then the env var, then the system setting.
    let motion_off = sr
        || env_on("CHANTO_REDUCE_MOTION")
        || match animations {
            Some(on) => !on,
            None => system_reduce_motion(),
        };
    (sr, motion_off)
}

/// The macOS "Reduce motion" preference (System Settings → Accessibility →
/// Display → Reduce motion). False elsewhere, or if it can't be read.
fn system_reduce_motion() -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    std::process::Command::new("defaults")
        .args(["read", "com.apple.universalaccess", "reduceMotion"])
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "1")
}

#[cfg(test)]
mod tests {
    use super::resolve;

    fn none(_: &str) -> bool {
        false
    }

    #[test]
    fn screen_reader_comes_from_config_or_env() {
        assert!(resolve(Some(true), None, none, || false).0);
        assert!(resolve(None, None, |k| k == "CLAUDE_AX_SCREEN_READER", || false).0);
        assert!(resolve(None, None, |k| k == "CHANTO_AX_SCREEN_READER", || false).0);
        assert!(!resolve(Some(false), None, none, || false).0);
    }

    #[test]
    fn reduced_motion_follows_config_env_and_the_system() {
        // An explicit config value wins; `animations = true` overrides the system.
        assert!(
            !resolve(None, Some(true), none, || true).1,
            "explicit on beats the system setting"
        );
        assert!(resolve(None, Some(false), none, || false).1);
        assert!(resolve(None, None, |k| k == "CHANTO_REDUCE_MOTION", || false).1);
        assert!(
            resolve(None, None, none, || true).1,
            "the system Reduce motion setting is honored"
        );
        assert!(!resolve(None, None, none, || false).1);
    }

    #[test]
    fn a_screen_reader_forces_motion_off() {
        // Even with animations explicitly on, a screen reader turns motion off.
        assert!(resolve(Some(true), Some(true), none, || false).1);
    }
}
