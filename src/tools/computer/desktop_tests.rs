//! The Computer tool against shims for every desktop program, in a child
//! process that points `output` at them, so no real screenshot, mouse movement
//! or key press can happen. macOS and X11 run the same scenario through their
//! own programs; Wayland has its own, on Linux.

use super::*;
use crate::agent::tests::{make_env_with, tmpdir};
use crate::permissions::Mode;
use crate::test_support::{isolated, shim};
use base64::Engine;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

const MAC: bool = cfg!(target_os = "macos");

struct Desktop {
    dir: PathBuf,
}

impl Desktop {
    /// Every shim logs its name and arguments; a file named `fail-<program>`
    /// makes it fail with that file's contents on stderr.
    fn new() -> Desktop {
        let dir = tmpdir("desktop");
        let log = dir.join("log");
        let write = |name: &str, body: &str| {
            let script = format!(
                "[ $# -eq 0 ] && exit 0\necho \"{name} $*\" >> '{log}'\nif [ -f '{dir}/fail-{name}' ]; then cat '{dir}/fail-{name}' >&2; exit 1; fi\nfor a; do last=$a; done\n{body}",
                log = log.display(),
                dir = dir.display(),
            );
            shim(&dir.join(name), &script);
        };
        let writes_last = "printf 'JPEGDATA' > \"$last\"";
        write(
            "osascript",
            "case \"$last\" in\n\
             *probe*) cat '__DIR__/probe' ;;\n\
             *cursor*) echo '{\"x\":1280,\"y\":800}' ;;\n\
             *windows*) echo '[{\"app\":\"Mail\",\"title\":\"Inbox\",\"x\":0,\"y\":0,\"w\":1000,\"h\":800},{\"app\":\"Tiny\",\"title\":\"\",\"x\":0,\"y\":0,\"w\":10,\"h\":10},{\"app\":\"Notes\",\"title\":\"\",\"x\":200,\"y\":200,\"w\":100,\"h\":100}]' ;;\n\
             esac"
                .replace("__DIR__", &dir.display().to_string())
                .as_str(),
        );
        std::fs::write(
            dir.join("probe"),
            r#"{"ax":true,"screen":true,"w":2560,"h":1600}"#,
        )
        .unwrap();
        write("screencapture", writes_last);
        write("sips", writes_last);
        write("open", "");
        write("gtk-launch", "");
        write("import", writes_last);
        write(
            "xdotool",
            "case \"$1\" in\n\
             getdisplaygeometry) cat '__DIR__/geometry' ;;\n\
             getmouselocation) echo 'x:1280 y:800 screen:0 window:7' ;;\n\
             search) echo 'Terminal' ;;\n\
             esac"
                .replace("__DIR__", &dir.display().to_string())
                .as_str(),
        );
        std::fs::write(dir.join("geometry"), "2560 1600\n").unwrap();
        // SAFETY: the test runs alone in its own child process.
        unsafe {
            std::env::set_var("CHANTO_TEST_FAKE_DESKTOP", &dir);
            std::env::set_var("DISPLAY", ":99");
            std::env::remove_var("WAYLAND_DISPLAY");
            std::env::remove_var("XDG_SESSION_TYPE");
            std::env::set_var("PATH", format!("{}:/usr/bin:/bin", dir.display()));
        }
        Desktop { dir }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("log")).unwrap_or_default()
    }

    fn clear_log(&self) {
        let _ = std::fs::remove_file(self.dir.join("log"));
    }

    fn fail(&self, program: &str, stderr: &str) {
        std::fs::write(self.dir.join(format!("fail-{program}")), stderr).unwrap();
    }

    fn heal(&self, program: &str) {
        std::fs::remove_file(self.dir.join(format!("fail-{program}"))).unwrap();
    }

    fn ctx(&self, vision: bool) -> Ctx {
        let env = make_env_with("http://127.0.0.1:9", &self.dir, Mode::Bypass, 32768, |_| {});
        env.set_vision(vision);
        Ctx {
            env,
            cancel: CancellationToken::new(),
            depth: 0,
            call_id: None,
            cwd_override: None,
            reads: None,
        }
    }
}

fn tool() -> Computer {
    Computer::new(&ComputerConfig::default())
}

async fn act(ctx: &Ctx, args: Value) -> ToolOutput {
    tool().run(args, ctx).await.unwrap()
}

#[tokio::test]
async fn screenshots_are_scaled_and_coordinates_map_back_to_the_screen() {
    if isolated(
        "tools::computer::desktop_tests::screenshots_are_scaled_and_coordinates_map_back_to_the_screen",
    ) {
        return;
    }
    let desktop = Desktop::new();
    let ctx = desktop.ctx(true);
    assert!(support().is_ok());

    let out = act(&ctx, json!({"action": "screenshot"})).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("1280x800"), "{}", out.content);
    assert!(out.content.contains("untrusted data"));
    assert!(!out.content.contains("Screen Recording"));
    assert_eq!(
        out.images,
        [base64::engine::general_purpose::STANDARD.encode("JPEGDATA")]
    );
    let log = desktop.log();
    if MAC {
        assert!(log.contains("screencapture -x -m -C -t png"), "{log}");
        assert!(log.contains("sips -z 800 1280"), "{log}");
    } else {
        assert!(
            log.contains("import -window root -resize 1280x800!"),
            "{log}"
        );
    }

    // A screenshot point is doubled on a screen twice its size.
    desktop.clear_log();
    let out = act(
        &ctx,
        json!({"action": "double_click", "coordinate": [100, 50]}),
    )
    .await;
    assert!(
        out.content.starts_with("2x Left click at (100, 50)."),
        "{}",
        out.content
    );
    let log = desktop.log();
    if MAC {
        assert!(
            log.contains(r#""count":2"#) && log.contains(r#""x":200.0"#),
            "{log}"
        );
    } else {
        assert!(
            log.contains("mousemove --sync 200 100 click --repeat 2 1"),
            "{log}"
        );
    }

    // A point outside the screenshot never reaches the desktop.
    desktop.clear_log();
    let out = act(
        &ctx,
        json!({"action": "left_click", "coordinate": [1280, 0]}),
    )
    .await;
    assert!(out.is_error);
    assert!(
        out.content.contains("outside the 1280x800"),
        "{}",
        out.content
    );
    assert_eq!(desktop.log(), "");

    let out = act(&ctx, json!({"action": "cursor_position"})).await;
    assert_eq!(
        out.content,
        "The cursor is at (640, 400) in screenshot coordinates."
    );

    let out = act(&ctx, json!({"action": "list_windows"})).await;
    if MAC {
        assert!(
            out.content
                .contains("- Mail \"Inbox\" at (0, 0)-(500, 400)"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("- Notes at (100, 100)-(150, 150)"),
            "{}",
            out.content
        );
        assert!(!out.content.contains("Tiny"), "{}", out.content);
    } else {
        assert_eq!(out.content, "Visible windows:\nTerminal");
    }
}

#[tokio::test]
async fn every_input_action_reaches_the_desktop_program() {
    if isolated("tools::computer::desktop_tests::every_input_action_reaches_the_desktop_program") {
        return;
    }
    let desktop = Desktop::new();
    let ctx = desktop.ctx(true);
    for (args, done, mac, x11) in [
        (
            json!({"action": "mouse_move", "coordinate": [1, 2]}),
            "Moved the mouse to (1, 2).",
            r#""op":"move""#,
            "mousemove --sync 2 4",
        ),
        (
            json!({"action": "left_click_drag", "start_coordinate": [1, 1], "coordinate": [2, 2]}),
            "Dragged from (1, 1) to (2, 2).",
            r#""op":"drag""#,
            "mousedown 1 mousemove --sync 4 4 mouseup 1",
        ),
        (
            json!({"action": "scroll", "scroll_direction": "left", "scroll_amount": 2, "coordinate": [1, 1]}),
            "scrolled left 2.",
            r#""dx":1"#,
            "click --repeat 2 6",
        ),
        (
            json!({"action": "scroll", "scroll_direction": "up"}),
            "scrolled up 3.",
            r#""at":false"#,
            "click --repeat 3 4",
        ),
        (
            json!({"action": "type", "text": "héllo"}),
            "Typed 5 characters.",
            r#""text":"héllo""#,
            "type --delay 12 -- héllo",
        ),
        (
            json!({"action": "key", "text": "ctrl+a"}),
            "Pressed the key.",
            r#""op":"#,
            "key -- ctrl+a",
        ),
        (
            json!({"action": "focus_app", "app": "Mail"}),
            "Focused Mail.",
            r#""op":"focus""#,
            "--class Mail windowactivate",
        ),
    ] {
        desktop.clear_log();
        let out = act(&ctx, args.clone()).await;
        assert!(!out.is_error, "{args}: {}", out.content);
        assert!(out.content.starts_with(done), "{args}: {}", out.content);
        assert!(
            out.content
                .ends_with("Take a screenshot to check the result.")
        );
        let log = desktop.log();
        let want = if MAC { mac } else { x11 };
        assert!(log.contains(want), "{args}: wanted {want} in {log}");
    }

    desktop.clear_log();
    let out = act(&ctx, json!({"action": "open_app", "app": "Calculator"})).await;
    assert_eq!(
        out.content,
        "Opened Calculator. Take a screenshot to see it."
    );
    let log = desktop.log();
    let want = if MAC {
        "open -a Calculator"
    } else {
        "gtk-launch Calculator"
    };
    assert!(log.contains(want), "{log}");

    let out = act(&ctx, json!({"action": "wait", "duration": 0.01})).await;
    assert_eq!(out.content, "Waited 0.01s.");
    let out = act(&ctx, json!({"action": "nope"})).await;
    assert!(out.is_error);
    assert!(LAST_ACTION.load(Ordering::Relaxed) > 0);
}

#[tokio::test]
async fn failures_name_the_program_and_what_went_wrong() {
    if isolated("tools::computer::desktop_tests::failures_name_the_program_and_what_went_wrong") {
        return;
    }
    let desktop = Desktop::new();
    let ctx = desktop.ctx(true);

    // A blind model is told why, before anything is captured.
    let blind = desktop.ctx(false);
    let out = act(&blind, json!({"action": "screenshot"})).await;
    assert!(out.is_error);
    assert!(out.content.contains("can't see images"), "{}", out.content);
    assert!(
        !desktop
            .log()
            .lines()
            .any(|l| l.starts_with("screencapture ") || l.starts_with("import ")),
        "{}",
        desktop.log()
    );

    let opener = if MAC { "open" } else { "gtk-launch" };
    desktop.fail(opener, "no such app\n");
    let out = act(&ctx, json!({"action": "open_app", "app": "Ghost"})).await;
    assert_eq!(out.content, "couldn't open Ghost: no such app");
    desktop.heal(opener);

    let shooter = if MAC { "screencapture" } else { "import" };
    desktop.fail(shooter, "capture broke\n");
    let out = act(&ctx, json!({"action": "screenshot"})).await;
    assert!(out.is_error);
    assert!(out.content.contains("capture broke"), "{}", out.content);
    desktop.heal(shooter);

    if MAC {
        desktop.fail("sips", "bad image\n");
        let out = act(&ctx, json!({"action": "screenshot"})).await;
        assert_eq!(out.content, "resizing the screenshot failed: bad image");
        desktop.heal("sips");

        // A missing permission becomes instructions, not a raw error.
        desktop.fail("screencapture", "could not create image from display\n");
        let out = act(&ctx, json!({"action": "screenshot"})).await;
        assert!(
            out.content.contains("Screen & System Audio Recording"),
            "{}",
            out.content
        );
        desktop.heal("screencapture");

        desktop.fail("osascript", "chanto-accessibility\n");
        let out = act(&ctx, json!({"action": "type", "text": "x"})).await;
        assert!(out.content.contains("Accessibility"), "{}", out.content);
        desktop.fail("osascript", "error -1743\n");
        let out = act(&ctx, json!({"action": "type", "text": "x"})).await;
        assert!(out.content.contains("Automation"), "{}", out.content);
        desktop.fail("osascript", "something else\n");
        let out = act(&ctx, json!({"action": "type", "text": "x"})).await;
        assert_eq!(out.content, "the desktop action failed: something else");
        desktop.heal("osascript");

        // The probe's answers decide what's possible before anything is sent.
        std::fs::write(
            desktop.dir.join("probe"),
            r#"{"ax":false,"screen":false,"w":2560,"h":1600}"#,
        )
        .unwrap();
        let out = act(&ctx, json!({"action": "screenshot"})).await;
        assert!(
            out.content
                .contains("Screen Recording permission is missing"),
            "{}",
            out.content
        );
        std::fs::write(desktop.dir.join("probe"), "not json").unwrap();
        let out = act(&ctx, json!({"action": "screenshot"})).await;
        assert!(
            out.content.contains("reading the screen size"),
            "{}",
            out.content
        );
        std::fs::write(
            desktop.dir.join("probe"),
            r#"{"ax":true,"screen":true,"w":0,"h":0}"#,
        )
        .unwrap();
        let out = act(&ctx, json!({"action": "screenshot"})).await;
        assert_eq!(out.content, "couldn't read the screen size");
    } else {
        desktop.fail("xdotool", "cannot open display\n");
        let out = act(&ctx, json!({"action": "screenshot"})).await;
        assert!(
            out.content.contains("cannot open display"),
            "{}",
            out.content
        );
        desktop.heal("xdotool");

        act(&ctx, json!({"action": "screenshot"})).await;
        desktop.fail("xdotool", "bad window\n");
        let out = act(&ctx, json!({"action": "type", "text": "x"})).await;
        assert_eq!(out.content, "xdotool failed: bad window");
        desktop.heal("xdotool");

        std::fs::write(desktop.dir.join("geometry"), "garbage\n").unwrap();
        let out = act(&ctx, json!({"action": "screenshot"})).await;
        assert!(
            out.content.contains("xdotool getdisplaygeometry"),
            "{}",
            out.content
        );
    }
}

#[cfg(target_os = "linux")]
mod wayland {
    use super::*;

    /// A fake Sway session: a PNG header reports the screen size.
    fn session(desktop: &Desktop) {
        let png =
            "\\211PNG\\r\\n\\032\\n\\000\\000\\000\\rIHDR\\000\\000\\007\\200\\000\\000\\004\\070";
        shim(
            &desktop.dir.join("grim"),
            &format!(
                "[ $# -eq 0 ] && exit 0\necho \"grim $*\" >> '{log}'\nfor a; do last=$a; done\nif [ -f '{dir}/no-jpeg' ] && [ \"$4\" = jpeg ]; then echo 'jpeg support disabled' >&2; exit 1; fi\nprintf '{png}' > \"$last\"",
                log = desktop.dir.join("log").display(),
                dir = desktop.dir.display(),
            ),
        );
        shim(
            &desktop.dir.join("ydotool"),
            &format!(
                "if [ $# -eq 0 ]; then echo 'Usage: ydotool cmd'; exit 0; fi\necho \"ydotool $*\" >> '{}'",
                desktop.dir.join("log").display()
            ),
        );
        shim(
            &desktop.dir.join("swaymsg"),
            &format!(
                "[ $# -eq 0 ] && exit 0\necho \"swaymsg $*\" >> '{}'\necho '{{\"nodes\":[{{\"pid\":1,\"name\":\"Term\"}}],\"floating_nodes\":[{{\"pid\":2,\"name\":\"Float\"}}]}}'",
                desktop.dir.join("log").display()
            ),
        );
        std::fs::write(desktop.dir.join("ydotool.sock"), "").unwrap();
        // SAFETY: the test runs alone in its own child process.
        unsafe {
            std::env::set_var("WAYLAND_DISPLAY", "wayland-1");
            std::env::set_var("XDG_CURRENT_DESKTOP", "sway");
            std::env::set_var("SWAYSOCK", desktop.dir.join("sway.sock"));
            std::env::set_var("YDOTOOL_SOCKET", desktop.dir.join("ydotool.sock"));
        }
    }

    #[tokio::test]
    async fn a_sway_session_uses_grim_ydotool_and_swaymsg() {
        if isolated(
            "tools::computer::desktop_tests::wayland::a_sway_session_uses_grim_ydotool_and_swaymsg",
        ) {
            return;
        }
        let desktop = Desktop::new();
        session(&desktop);
        let ctx = desktop.ctx(true);
        assert_eq!(support(), Ok(()));

        let out = act(&ctx, json!({"action": "screenshot"})).await;
        assert!(!out.is_error, "{}", out.content);
        // 1920x1080 from the PNG header, fit inside 1280x800.
        assert!(out.content.contains("1280x720"), "{}", out.content);
        assert!(
            desktop.log().contains("grim -s 0.6667 -t jpeg -q 70"),
            "{}",
            desktop.log()
        );

        // Builds without JPEG fall back to a scaled PNG.
        std::fs::write(desktop.dir.join("no-jpeg"), "").unwrap();
        let out = act(&ctx, json!({"action": "screenshot"})).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            desktop.log().contains("grim -s 0.6667 -t png"),
            "{}",
            desktop.log()
        );

        let out = act(&ctx, json!({"action": "cursor_position"})).await;
        assert!(
            out.content.contains("doesn't let apps read the cursor"),
            "{}",
            out.content
        );
        let out = act(&ctx, json!({"action": "list_windows"})).await;
        assert!(
            out.content.contains("Term") && out.content.contains("Float"),
            "{}",
            out.content
        );
        let out = act(&ctx, json!({"action": "focus_app", "app": "foot"})).await;
        assert!(out.content.starts_with("Focused foot."), "{}", out.content);
        assert!(
            desktop.log().contains("[app_id=\"(?i)foot\"] focus"),
            "{}",
            desktop.log()
        );

        desktop.clear_log();
        let out = act(&ctx, json!({"action": "type", "text": "hi"})).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(desktop.log().contains("ydotool type"), "{}", desktop.log());
    }

    #[test]
    fn a_wayland_desktop_reports_what_it_is_missing() {
        if isolated(
            "tools::computer::desktop_tests::wayland::a_wayland_desktop_reports_what_it_is_missing",
        ) {
            return;
        }
        let desktop = Desktop::new();
        // SAFETY: the test runs alone in its own child process.
        unsafe {
            std::env::set_var("WAYLAND_DISPLAY", "wayland-1");
            std::env::set_var("XDG_CURRENT_DESKTOP", "GNOME");
            std::env::set_var("PATH", desktop.dir.display().to_string());
        }
        assert!(support().unwrap_err().contains("screenshot tool"));
        shim(&desktop.dir.join("gnome-screenshot"), "exit 0");
        assert!(support().unwrap_err().contains("needs ImageMagick"));
        assert_eq!(wayland_shooter(), Some("gnome-screenshot"));
        shim(&desktop.dir.join("convert"), "exit 0");
        assert_eq!(magick(), Some("convert"));
        assert!(support().unwrap_err().contains("needs ydotool"));
    }
}

#[test]
fn x11_support_names_the_missing_tools() {
    if !cfg!(target_os = "linux") {
        return;
    }
    if isolated("tools::computer::desktop_tests::x11_support_names_the_missing_tools") {
        return;
    }
    let dir = tmpdir("desktop-x11");
    // SAFETY: the test runs alone in its own child process.
    unsafe {
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::remove_var("XDG_SESSION_TYPE");
        std::env::remove_var("DISPLAY");
        std::env::set_var("PATH", dir.display().to_string());
    }
    assert!(support().unwrap_err().contains("graphical session"));
    // SAFETY: as above.
    unsafe { std::env::set_var("DISPLAY", ":1") };
    let e = support().unwrap_err();
    assert!(e.contains("xdotool and import"), "{e}");
    shim(&dir.join("xdotool"), "exit 0");
    let e = support().unwrap_err();
    assert!(e.contains("needs import"), "{e}");
    shim(&dir.join("import"), "exit 0");
    assert_eq!(support(), Ok(()));
}

#[test]
fn indicator_follows_recent_use() {
    if isolated("tools::computer::desktop_tests::indicator_follows_recent_use") {
        return;
    }
    assert_eq!(indicator(), None);
    ENABLED.store(true, Ordering::Relaxed);
    LAST_ACTION.store(0, Ordering::Relaxed);
    assert_eq!(indicator(), Some("◉ computer use on"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    LAST_ACTION.store(now, Ordering::Relaxed);
    assert_eq!(indicator(), Some("◉ computer use active"));
}

#[test]
fn the_permission_prompt_names_the_app_chanto_runs_in() {
    if isolated(
        "tools::computer::desktop_tests::the_permission_prompt_names_the_app_chanto_runs_in",
    ) {
        return;
    }
    for (term, name) in [
        ("Apple_Terminal", "Terminal"),
        ("iTerm.app", "iTerm"),
        ("WezTerm", "WezTerm"),
        ("ghostty", "Ghostty"),
        ("WarpTerminal", "Warp"),
        ("Hyper", "Hyper"),
        ("tmux", "the terminal app running tmux"),
        ("", "the terminal app you run Chanto in"),
        ("Other", "Other"),
    ] {
        // SAFETY: the test runs alone in its own child process.
        unsafe { std::env::set_var("TERM_PROGRAM", term) };
        assert_eq!(host_app(), name);
    }
}

#[test]
fn config_merge_only_replaces_what_is_set_and_limits_are_clamped() {
    let mut c = ComputerConfig {
        enabled: Some(true),
        max_width: Some(100),
        max_height: Some(9000),
        ..Default::default()
    };
    assert_eq!(c.max(), (320, 4096));
    c.merge(ComputerConfig {
        max_width: Some(1000),
        ..Default::default()
    });
    assert_eq!(c.enabled, Some(true), "unset keeps the old value");
    assert_eq!(c.max(), (1000, 4096));
    assert_eq!(ComputerConfig::default().max(), (1280, 800));
}

#[test]
fn labels_describe_the_action() {
    let t = tool();
    let label = |v: Value| t.label(&v);
    assert_eq!(
        label(json!({"action": "screenshot"})),
        "Computer(screenshot)"
    );
    assert_eq!(
        label(json!({"action": "left_click", "coordinate": [1, 2]})),
        "Computer(left_click (1, 2))"
    );
    assert_eq!(
        label(
            json!({"action": "left_click_drag", "start_coordinate": [1, 2], "coordinate": [3, 4]})
        ),
        "Computer(left_click_drag (1, 2) → (3, 4))"
    );
    assert_eq!(
        label(json!({"action": "type", "text": "hi"})),
        "Computer(type \"hi\")"
    );
    assert_eq!(
        label(json!({"action": "key", "text": "cmd+c"})),
        "Computer(key cmd+c)"
    );
    assert_eq!(
        label(json!({"action": "open_app", "app": "Mail"})),
        "Computer(open_app Mail)"
    );
    assert_eq!(
        label(json!({"action": "scroll", "scroll_direction": "up"})),
        "Computer(scroll up 3)"
    );
    // Invalid arguments still make a label rather than an error.
    assert_eq!(
        label(json!({"action": "left_click"})),
        "Computer(left_click)"
    );
    assert_eq!(label(json!({})), "Computer(?)");
    assert_eq!(t.kind(), ToolKind::Browser);
    assert_eq!(t.rule_subjects(&json!({"action": "type"})), ["type"]);
    let schema = t.schema();
    for a in schema["properties"]["action"]["enum"].as_array().unwrap() {
        let a = a.as_str().unwrap();
        // Each advertised action parses or says what it's missing; none is unknown.
        let err = parse_action(&json!({"action": a}))
            .err()
            .unwrap_or_default();
        assert!(!err.contains("unknown"), "{a}: {err}");
        assert!(t.description().contains(a), "{a} is undocumented");
    }
}

/// The legacy ydotool has no scrolling or dragging: the model gets told what to
/// use instead rather than a silent no-op.
#[test]
fn legacy_ydotool_commands_and_refusals() {
    let s = Scale {
        screen: (2000.0, 1000.0),
        shot: (1000, 500),
    };
    let cmds = ydotool_legacy_cmds(
        &Action::Click {
            at: (10.0, 10.0),
            button: Button::Right,
            count: 2,
        },
        &s,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        cmds,
        [
            vec!["mousemove", "20", "20"],
            vec!["click", "2"],
            vec!["click", "2"]
        ]
    );
    let e = ydotool_legacy_cmds(
        &Action::Drag {
            from: (0.0, 0.0),
            to: (1.0, 1.0),
        },
        &s,
    )
    .unwrap_err();
    assert!(e.contains("dragging needs ydotool 1.0"), "{e}");
    let e = ydotool_legacy_cmds(
        &Action::Scroll {
            at: None,
            dir: Dir::Up,
            amount: 1,
        },
        &s,
    )
    .unwrap_err();
    assert!(e.contains("press Page_Down"), "{e}");
    let key = |text: &str| {
        let combo = parse_combo(text).unwrap();
        ydotool_legacy_cmds(&Action::Key { combo }, &s)
            .unwrap()
            .unwrap()
    };
    assert_eq!(key("Return"), [vec!["key", "enter"]]);
    assert_eq!(key("ctrl+alt+Delete"), [vec!["key", "ctrl+alt+delete"]]);
    // An uppercase letter needs shift; the key itself is named in lowercase.
    assert_eq!(key("A"), [vec!["key", "shift+a"]]);
    assert_eq!(key("F5"), [vec!["key", "f5"]]);
    for (name, want) in [
        ("Tab", "tab"),
        ("space", "space"),
        ("Backspace", "backspace"),
        ("Escape", "esc"),
        ("Up", "up"),
        ("Down", "down"),
        ("Left", "left"),
        ("Right", "right"),
        ("Home", "home"),
        ("End", "end"),
        ("Page_Up", "pageup"),
        ("Page_Down", "pagedown"),
    ] {
        assert_eq!(key(name), [vec!["key", want]], "{name}");
    }
    assert_eq!(
        ydotool_legacy_cmds(&Action::Type { text: "x".into() }, &s)
            .unwrap()
            .unwrap(),
        [vec!["type", "x"]]
    );
}

#[test]
fn window_titles_read_both_compositors_shapes() {
    let hypr = json!([{"title": "a"}, {"title": ""}, {"other": 1}]);
    assert_eq!(window_titles(&hypr), ["a"]);
    let sway = json!({"nodes": [
        {"pid": 1, "name": "shown"},
        {"name": "workspace, no pid", "nodes": [{"pid": 2, "name": "nested"}]}
    ], "floating_nodes": [{"pid": 3, "name": "float"}]});
    let mut titles = window_titles(&sway);
    titles.sort();
    assert_eq!(titles, ["float", "nested", "shown"]);
    assert!(window_titles(&json!("nonsense")).is_empty());
}
