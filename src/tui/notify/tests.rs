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
