//! Desktop notifications through the terminal: OSC 9 (iTerm2, WezTerm,
//! Ghostty, Windows Terminal, kitty), OSC 777 (urxvt, foot, VTE terminals) or
//! the bell, wrapped for tmux so they reach the outer terminal. Terminals that
//! can't raise one themselves (Terminal.app) fall back to a system notification
//! from `terminal-notifier` on macOS or `notify-send` on Linux, if installed.

use std::io::Write;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Channel {
    Osc9,
    Osc777,
    Bell,
    /// The OS notification centre (`terminal-notifier` on macOS, `notify-send`
    /// on Linux), plus the bell, for terminals that can't raise one themselves.
    System,
    Off,
}

/// The `notifications` setting; `auto` (or unset, or anything unrecognised)
/// picks from the terminal.
pub fn channel(setting: Option<&str>) -> Channel {
    match setting {
        Some("off") => Channel::Off,
        // A screen reader wants to hear that Chanto needs you, and the bell is
        // the one cue every terminal emits, whatever it can raise on its own.
        // An explicit `off` still mutes it.
        _ if crate::a11y::screen_reader() => Channel::Bell,
        Some("osc9") => Channel::Osc9,
        Some("osc777") => Channel::Osc777,
        Some("bell") => Channel::Bell,
        Some("system") => Channel::System,
        _ => detect(&|k| std::env::var(k).ok().filter(|v| !v.is_empty())),
    }
}

/// Which notification the terminal shows, from what it says about itself.
/// Inside tmux `TERM_PROGRAM` is tmux, but `LC_TERMINAL` (iTerm2) and the
/// terminals' own variables usually survive.
fn detect(var: &dyn Fn(&str) -> Option<String>) -> Channel {
    let program = var("TERM_PROGRAM").unwrap_or_default();
    let term = var("TERM").unwrap_or_default();
    if matches!(program.as_str(), "iTerm.app" | "WezTerm" | "ghostty")
        || var("LC_TERMINAL").as_deref() == Some("iTerm2")
        || var("WT_SESSION").is_some()
        || var("WEZTERM_EXECUTABLE").is_some()
        || var("GHOSTTY_RESOURCES_DIR").is_some()
        || var("KITTY_WINDOW_ID").is_some()
        || term == "xterm-kitty"
        || term == "xterm-ghostty"
    {
        return Channel::Osc9;
    }
    if term.starts_with("foot") || term.starts_with("rxvt") || var("VTE_VERSION").is_some() {
        return Channel::Osc777;
    }
    // Terminal.app only rings the bell; a notifier app can still post one.
    if program == "Apple_Terminal" {
        return Channel::System;
    }
    Channel::Bell
}

/// Keep a message on one line and free of anything the terminal would read as
/// part of the escape sequence (ESC, BEL, ST and other control characters).
fn clean(s: &str, max: usize) -> String {
    let one: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let one = one.split_whitespace().collect::<Vec<_>>().join(" ");
    match one.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &one[..i]),
        None => one,
    }
}

/// The bytes to write, or `None` when there is nothing to send. In tmux an OSC
/// goes through as a DCS passthrough (`allow-passthrough on`) with every ESC
/// doubled; the bell needs no wrapping.
pub fn sequence(ch: Channel, title: &str, body: &str, tmux: bool) -> Option<String> {
    let (title, body) = (clean(title, 60), clean(body, 200));
    let osc = match ch {
        Channel::Off => return None,
        Channel::Bell | Channel::System => return Some("\x07".into()),
        Channel::Osc9 => format!("\x1b]9;{title}: {body}\x07"),
        // `;` separates the title from the body.
        Channel::Osc777 => format!("\x1b]777;notify;{};{body}\x07", title.replace(';', ",")),
    };
    Some(if tmux {
        format!("\x1bPtmux;{}\x1b\\", osc.replace('\x1b', "\x1b\x1b"))
    } else {
        osc
    })
}

/// What the session is doing, for the Program Status Protocol (OSC 7501):
/// terminals that implement it show it on the tab (working, waiting on you,
/// done).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Working,
    /// Waiting on the user: `permission` or `question`.
    Blocked(&'static str),
    Done,
    Error,
}

/// Whether to report [`Status`]: the `terminal_status` setting (`auto`, `on`,
/// `off`); `CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1` turns it off as in Claude
/// Code. `auto` sends it only to terminals known to implement OSC 7501
/// (Ghostty), and not inside tmux or screen, which don't pass it through.
pub fn status_enabled(setting: Option<&str>, var: &dyn Fn(&str) -> Option<String>) -> bool {
    if matches!(
        var("CLAUDE_CODE_DISABLE_TERMINAL_TITLE").as_deref(),
        Some("1" | "true")
    ) {
        return false;
    }
    match setting {
        Some("on" | "true" | "always") => true,
        Some("off" | "false" | "never") => false,
        _ => {
            if var("TMUX").is_some() || var("STY").is_some() {
                return false;
            }
            var("TERM_PROGRAM").is_some_and(|p| p.eq_ignore_ascii_case("ghostty"))
                || var("TERM").as_deref() == Some("xterm-ghostty")
                || var("GHOSTTY_RESOURCES_DIR").is_some()
        }
    }
}

/// The OSC 7501 report for `status`, with an optional one-line message
/// (base64, as the protocol requires, and within its 2048-byte limit).
pub fn status_sequence(status: Status, msg: &str) -> String {
    use base64::Engine;
    let mut pairs = match status {
        Status::Working => "state=working".to_string(),
        Status::Blocked(kind) => format!("state=blocked:kind={kind}"),
        Status::Done => "state=done".into(),
        Status::Error => "state=error".into(),
    };
    pairs.push_str(":app=chanto");
    let msg = clean(msg, 200);
    if !msg.is_empty() {
        pairs.push_str(":msg=");
        pairs.push_str(&base64::engine::general_purpose::STANDARD.encode(msg.as_bytes()));
    }
    format!("\x1b]7501;{pairs}\x1b\\")
}

/// Write an OSC 7501 report now.
pub fn send_status(status: Status, msg: &str) {
    let mut out = std::io::stdout();
    let _ = out.write_all(status_sequence(status, msg).as_bytes());
    let _ = out.flush();
}

/// terminal-notifier reads option values through `NSUserDefaults`, which tries
/// to parse a value whose first character is `[`, `(`, `{`, `"` or `<` as a
/// property list and then drops the notification. A leading backslash escapes it.
fn notifier_value(s: &str) -> String {
    match s.chars().next() {
        Some(c) if "[({<\"".contains(c) => format!("\\{s}"),
        _ => s.to_string(),
    }
}

/// `terminal-notifier` is an app bundle, so macOS can activate something on
/// click: the terminal that runs the session, or the URL a run notification
/// links to.
fn terminal_notifier_command(
    title: &str,
    body: &str,
    open: Option<&str>,
    var: &dyn Fn(&str) -> Option<String>,
) -> std::process::Command {
    let mut c = std::process::Command::new("terminal-notifier");
    c.arg("-title")
        .arg(notifier_value(title))
        .arg("-message")
        .arg(notifier_value(body));
    if let Some(url) = open {
        c.arg("-open").arg(url);
    }
    if let Some(bundle) = terminal_bundle(var) {
        c.arg("-activate").arg(bundle);
    }
    c
}

/// Bundle identifier of the terminal, for `terminal-notifier -activate`.
/// `LC_TERMINAL` (iTerm2) survives tmux, where `TERM_PROGRAM` is tmux.
fn terminal_bundle(var: &dyn Fn(&str) -> Option<String>) -> Option<&'static str> {
    let program = var("TERM_PROGRAM").unwrap_or_default();
    let lc = var("LC_TERMINAL").unwrap_or_default();
    Some(match (program.as_str(), lc.as_str()) {
        ("iTerm.app", _) | (_, "iTerm2") => "com.googlecode.iterm2",
        ("Apple_Terminal", _) => "com.apple.Terminal",
        ("WezTerm", _) => "com.github.wez.wezterm",
        ("ghostty", _) => "com.mitchellh.ghostty",
        ("Hyper", _) => "co.zeit.hyper",
        ("vscode", _) => "com.microsoft.VSCode",
        ("WarpTerminal", _) => "dev.warp.Warp-Stable",
        ("kitty", _) => "net.kovidgoyal.kitty",
        _ => return None,
    })
}

/// The command that posts a system notification, if this OS has one. `open` is
/// a URL for the notification to open when clicked (background-run links).
///
/// macOS goes through `terminal-notifier` only. A bare `osascript` has no app
/// bundle, so macOS posts its notification under Script Editor and opens Script
/// Editor on a click; with no `terminal-notifier` there is simply no system
/// notification and the bell is all you get.
pub fn system_command(
    title: &str,
    body: &str,
    open: Option<&str>,
) -> Option<std::process::Command> {
    let (title, body) = (clean(title, 60), clean(body, 200));
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if cfg!(target_os = "macos") {
        crate::shell::command_exists("terminal-notifier")
            .then(|| terminal_notifier_command(&title, &body, open, &var))
    } else if cfg!(target_os = "linux") && crate::shell::command_exists("notify-send") {
        let mut c = std::process::Command::new("notify-send");
        c.arg("--app-name=chanto").arg(title).arg(body);
        Some(c)
    } else {
        None
    }
}

/// Send a notification now.
pub fn send(ch: Channel, title: &str, body: &str) {
    if ch == Channel::System
        && let Some(mut c) = system_command(title, body, None)
    {
        let _ = c
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    let Some(seq) = sequence(ch, title, body, std::env::var_os("TMUX").is_some()) else {
        return;
    };
    // Flushed: the sequence has no newline and would otherwise sit in the
    // buffer until the next frame.
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// The file the `notify_sound` setting names: a path, or a system sound
/// name (`Glass` on macOS, `complete` on Linux, `chimes` on Windows;
/// `default` picks one). `None` when off or not found.
fn sound_file(
    setting: &str,
    os: &str,
    exists: &dyn Fn(&std::path::Path) -> bool,
) -> Option<std::path::PathBuf> {
    let s = setting.trim();
    if s.is_empty() || matches!(s, "off" | "false" | "none") {
        return None;
    }
    let path = match s.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => std::path::PathBuf::from(s),
    };
    if exists(&path) {
        return Some(path);
    }
    let candidates: Vec<String> = match os {
        "macos" => {
            let name = if s == "default" { "Glass" } else { s };
            vec![
                format!("/System/Library/Sounds/{name}.aiff"),
                format!(
                    "{}/Library/Sounds/{name}.aiff",
                    crate::compat::home().display()
                ),
            ]
        }
        "windows" => {
            let name = if s == "default" {
                "Windows Notify System Generic"
            } else {
                s
            };
            let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
            vec![format!(r"{root}\Media\{name}.wav")]
        }
        _ => {
            let name = if s == "default" { "complete" } else { s };
            ["oga", "ogg", "wav"]
                .iter()
                .map(|x| format!("/usr/share/sounds/freedesktop/stereo/{name}.{x}"))
                .collect()
        }
    };
    candidates
        .into_iter()
        .map(std::path::PathBuf::from)
        .find(|p| exists(p))
}

/// Play the `notify_sound` setting in the background (afplay on macOS,
/// paplay or aplay on Linux, PowerShell's SoundPlayer on Windows).
pub fn play_sound(setting: Option<&str>) {
    let Some(file) = setting.and_then(|s| sound_file(s, std::env::consts::OS, &|p| p.is_file()))
    else {
        return;
    };
    let mut c = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("afplay");
        c.arg(&file);
        c
    } else if cfg!(windows) {
        let lit = file.display().to_string().replace('\'', "''");
        let mut c = std::process::Command::new("powershell");
        c.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("(New-Object Media.SoundPlayer '{lit}').PlaySync()"),
        ]);
        c
    } else {
        let player = if crate::shell::command_exists("paplay") {
            "paplay"
        } else {
            "aplay"
        };
        let mut c = std::process::Command::new(player);
        c.arg(&file);
        c
    };
    let _ = c
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sound_setting_resolves_names_and_paths() {
        let only = |want: &'static str| move |p: &std::path::Path| p == std::path::Path::new(want);
        assert_eq!(sound_file("off", "macos", &|_| true), None);
        assert_eq!(sound_file("", "linux", &|_| true), None);
        assert_eq!(
            sound_file(
                "default",
                "macos",
                &only("/System/Library/Sounds/Glass.aiff")
            )
            .unwrap(),
            std::path::Path::new("/System/Library/Sounds/Glass.aiff")
        );
        assert_eq!(
            sound_file("Ping", "macos", &only("/System/Library/Sounds/Ping.aiff")).unwrap(),
            std::path::Path::new("/System/Library/Sounds/Ping.aiff")
        );
        assert_eq!(
            sound_file(
                "bell",
                "linux",
                &only("/usr/share/sounds/freedesktop/stereo/bell.oga")
            )
            .unwrap(),
            std::path::Path::new("/usr/share/sounds/freedesktop/stereo/bell.oga")
        );
        assert_eq!(
            sound_file("/tmp/done.wav", "linux", &only("/tmp/done.wav")).unwrap(),
            std::path::Path::new("/tmp/done.wav")
        );
        assert_eq!(sound_file("Nope", "macos", &|_| false), None);
    }

    #[test]
    fn sequences_for_each_channel() {
        assert_eq!(
            sequence(Channel::Osc9, "chanto", "Done", false).unwrap(),
            "\x1b]9;chanto: Done\x07"
        );
        assert_eq!(
            sequence(Channel::Osc777, "chanto; x", "Needs you; now", false).unwrap(),
            "\x1b]777;notify;chanto, x;Needs you; now\x07"
        );
        assert_eq!(sequence(Channel::Bell, "t", "b", true).unwrap(), "\x07");
        assert_eq!(sequence(Channel::Off, "t", "b", false), None);
    }

    #[test]
    fn system_channel_rings_the_bell_too() {
        assert_eq!(channel(Some("system")), Channel::System);
        assert_eq!(
            sequence(Channel::System, "t", "b", false).unwrap(),
            "\x07",
            "the bell rings too"
        );
    }

    #[test]
    fn terminal_notifier_focuses_the_terminal_and_opens_links() {
        let with = |vars: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                vars.iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        // A click focuses the terminal the session runs in.
        let t = terminal_notifier_command(
            "chanto",
            "Done",
            None,
            &with(&[("TERM_PROGRAM", "Apple_Terminal")]),
        );
        let args: Vec<String> = t
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-title",
                "chanto",
                "-message",
                "Done",
                "-activate",
                "com.apple.Terminal"
            ]
        );
        // A run notification opens its web link.
        let t = terminal_notifier_command(
            "chanto",
            "run done",
            Some("https://h/#run=1"),
            &with(&[("TERM_PROGRAM", "WezTerm")]),
        );
        let args: Vec<String> = t
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-title",
                "chanto",
                "-message",
                "run done",
                "-open",
                "https://h/#run=1",
                "-activate",
                "com.github.wez.wezterm"
            ]
        );
        // A message starting with a plist character is escaped for NSUserDefaults.
        let t = terminal_notifier_command("chanto", "(3 files changed)", None, &with(&[]));
        let args: Vec<String> = t
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            ["-title", "chanto", "-message", r"\(3 files changed)"]
        );
        // Unknown terminals get no -activate.
        let t = terminal_notifier_command("t", "b", None, &with(&[("TERM_PROGRAM", "Unknown")]));
        assert!(!t.get_args().any(|a| a == "-activate"));
        // iTerm2 is found through LC_TERMINAL inside tmux.
        assert_eq!(
            terminal_bundle(&with(&[
                ("TERM_PROGRAM", "tmux"),
                ("LC_TERMINAL", "iTerm2")
            ])),
            Some("com.googlecode.iterm2")
        );
    }

    #[test]
    fn tmux_gets_a_passthrough_with_escapes_doubled() {
        let s = sequence(Channel::Osc9, "chanto", "Done", true).unwrap();
        assert_eq!(s, "\x1bPtmux;\x1b\x1b]9;chanto: Done\x07\x1b\\");
    }

    #[test]
    fn control_characters_cannot_end_the_sequence_early() {
        let s = sequence(
            Channel::Osc9,
            "har\x07ness",
            "line one\nline\x1b]0;evil\x07 two\u{9c}",
            false,
        )
        .unwrap();
        assert_eq!(s, "\x1b]9;har ness: line one line ]0;evil two\x07");
        assert_eq!(s.matches('\x1b').count(), 1);
        assert_eq!(s.matches('\x07').count(), 1);
        let long = sequence(Channel::Osc9, "t", &"x".repeat(500), false).unwrap();
        assert!(long.chars().count() < 220 && long.contains('…'));
    }

    #[test]
    fn program_status_reports_follow_osc_7501() {
        assert_eq!(
            status_sequence(Status::Working, ""),
            "\x1b]7501;state=working:app=chanto\x1b\\"
        );
        assert_eq!(
            status_sequence(Status::Done, ""),
            "\x1b]7501;state=done:app=chanto\x1b\\"
        );
        assert_eq!(
            status_sequence(Status::Error, ""),
            "\x1b]7501;state=error:app=chanto\x1b\\"
        );
        // The message is base64, so nothing in it can end the sequence.
        let s = status_sequence(Status::Blocked("permission"), "Allow \x1b\\Bash?\x07");
        assert_eq!(
            s,
            "\x1b]7501;state=blocked:kind=permission:app=chanto:msg=QWxsb3cgXEJhc2g/\x1b\\"
        );
        assert_eq!(s.matches('\x1b').count(), 2);
        assert!(!s.contains('\x07'));
    }

    #[test]
    fn program_status_only_where_supported() {
        let with = |vars: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                vars.iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(status_enabled(None, &with(&[("TERM_PROGRAM", "ghostty")])));
        assert!(status_enabled(
            Some("auto"),
            &with(&[("TERM", "xterm-ghostty")])
        ));
        // Unknown terminals never get raw sequences they might print.
        assert!(!status_enabled(
            None,
            &with(&[("TERM_PROGRAM", "Apple_Terminal")])
        ));
        assert!(!status_enabled(None, &with(&[])));
        // tmux and screen don't pass it through.
        assert!(!status_enabled(
            None,
            &with(&[("TERM_PROGRAM", "ghostty"), ("TMUX", "/tmp/tmux")])
        ));
        assert!(status_enabled(Some("on"), &with(&[])));
        assert!(!status_enabled(
            Some("off"),
            &with(&[("TERM_PROGRAM", "ghostty")])
        ));
        assert!(!status_enabled(
            Some("on"),
            &with(&[("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1")])
        ));
    }

    #[test]
    fn detection_follows_the_terminal() {
        let with = |vars: &'static [(&'static str, &'static str)]| {
            detect(&move |k| {
                vars.iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            })
        };
        assert_eq!(with(&[("TERM_PROGRAM", "iTerm.app")]), Channel::Osc9);
        assert_eq!(
            with(&[("TERM_PROGRAM", "tmux"), ("LC_TERMINAL", "iTerm2")]),
            Channel::Osc9
        );
        assert_eq!(with(&[("TERM", "xterm-ghostty")]), Channel::Osc9);
        assert_eq!(with(&[("WT_SESSION", "1")]), Channel::Osc9);
        assert_eq!(with(&[("TERM", "foot")]), Channel::Osc777);
        assert_eq!(
            with(&[("TERM", "xterm-256color"), ("VTE_VERSION", "7600")]),
            Channel::Osc777
        );
        assert_eq!(with(&[("TERM_PROGRAM", "Apple_Terminal")]), Channel::System);
        assert_eq!(with(&[]), Channel::Bell);
        assert_eq!(channel(Some("off")), Channel::Off);
        assert_eq!(channel(Some("osc777")), Channel::Osc777);
    }
}
