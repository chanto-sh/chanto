//! Deterministic full-frame text and cell-style snapshots, plus ANSI preview frames.
use super::app_tests::*;
use super::*;
use ratatui::{backend::TestBackend, buffer::Buffer};
use std::fmt::Write as _;

const CASES: &[&str] = &[
    "welcome",
    "conversation",
    "streaming",
    "background_waiting",
    "background_success",
    "background_failure",
    "quota_warning",
    "quota_critical",
    "permission",
    "question",
    "completion",
    "scrolled",
];

type Replies = Vec<Box<dyn std::any::Any>>;

fn fixture(case: &str, now: Instant, unix: i64) -> (TestApp, Replies) {
    let mut replies: Replies = Vec::new();
    let mut t = test_app();
    t.blocks.clear();
    t.branch = Some("main".into());
    t.busy_since = now - Duration::from_secs(65);
    t.busy_label = Some("Responding".into());
    match case {
        "welcome" => {
            t.push(Kind::Welcome {
                model: "mock".into(),
                cwd: "/workspace/demo".into(),
            });
        }
        "conversation" => {
            t.push(Kind::User("Add a greeting function and test it.".into()));
            t.push(Kind::Assistant("Implemented `greet(name)` and its tests.\n\n```rust\nfn greet(name: &str) -> String {\n    format!(\"Hello, {name}!\")\n}\n```\n\n- Tests passed\n- Handles Unicode names".into()));
        }
        "streaming" => {
            t.push(Kind::User("Explain the change.".into()));
            t.busy = true;
            t.on_agent(Event::Text("The parser now handles streamed data in small pieces.\n\n```rust\nlet mut buffer = Vec::new();\n".into()));
        }
        "background_waiting" | "background_success" | "background_failure" => {
            t.push(Kind::User("Build the release binary.".into()));
            let running = case == "background_waiting";
            t.busy = running;
            let (display, progress, error) = match case {
                "background_waiting" => (None, Some("Waiting for new output (up to 30s)".into()), false),
                "background_success" => (Some("Completed · exit 0 · 2 new lines\nCompiling chanto\nFinished release build".into()), None, false),
                _ => (Some("Failed · exit 1 · 1 new line\nerror: could not compile the project".into()), None, true),
            };
            t.on_agent(Event::ToolStart {
                id: "poll-1".into(),
                label: "BashOutput(Build release binary)".into(),
                parent: None,
            });
            if let Some(progress) = progress {
                t.on_agent(Event::ToolProgress {
                    id: "poll-1".into(),
                    text: progress,
                });
            }
            if let Some(display) = display {
                t.on_agent(Event::ToolEnd {
                    id: "poll-1".into(),
                    display,
                    content: String::new(),
                    is_error: error,
                    images: vec![],
                    parent: None,
                });
            }
            let tool = t.find_tool("poll-1").unwrap();
            tool.started = now - Duration::from_secs(5);
            tool.elapsed = (!running).then_some(Duration::from_secs(5));
        }
        "quota_warning" | "quota_critical" => {
            t.push(Kind::User("Check the remaining allowance.".into()));
            t.push(Kind::Assistant(
                "The remaining allowance and reset time are in the footer.".into(),
            ));
            t.on_agent(Event::Limits(crate::llm::limits::Snapshot {
                primary: Some(crate::llm::limits::Window {
                    used_percent: if case == "quota_warning" { 75.0 } else { 90.0 },
                    window_minutes: Some(300),
                    resets_at: Some(unix + 3600),
                }),
                secondary: None,
                plan_type: Some("pro".into()),
            }));
        }
        "permission" => {
            t.push(Kind::User("Clean the build output.".into()));
            let (tx, rx) = oneshot::channel();
            replies.push(Box::new(rx));
            t.busy = true;
            t.on_msg(AppMsg::Permission(permission(None), tx));
        }
        "question" => {
            let (tx, rx) = oneshot::channel();
            replies.push(Box::new(rx));
            t.busy = true;
            t.on_msg(AppMsg::Question(
                "Which database should we use?".into(),
                vec!["SQLite".into(), "PostgreSQL".into()],
                true,
                tx,
            ));
        }
        "completion" => type_str(&mut t, "/m"),
        "scrolled" => {
            for i in 0..30 {
                t.push(Kind::Markdown(format!(
                    "Step {i}: checked the project and ran the tests."
                )));
            }
            t.scroll = 5;
        }
        _ => panic!("unknown visual fixture: {case}"),
    }
    (t, replies)
}

fn snapshot(buf: &Buffer) -> String {
    let mut text = String::new();
    let mut styles = String::new();
    for y in 0..buf.area.height {
        let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        writeln!(text, "{}", row.trim_end()).unwrap();
        let mut start = 0;
        while start < buf.area.width {
            let style = buf[(start, y)].style();
            let mut end = start + 1;
            while end < buf.area.width && buf[(end, y)].style() == style {
                end += 1;
            }
            writeln!(styles, "row {y:02} cols {start:03}..{end:03}: {style:?}").unwrap();
            start = end;
        }
    }
    format!(
        "{}x{}\n--- screen ---\n{text}--- cell styles ---\n{styles}",
        buf.area.width, buf.area.height
    )
}

fn ansi(buf: &Buffer) -> String {
    let mut out = String::from("\x1b[2J\x1b[H\x1b[?25l");
    for y in 0..buf.area.height {
        write!(out, "\x1b[{};1H", y + 1).unwrap();
        let mut x = 0;
        while x < buf.area.width {
            let cell = &buf[(x, y)];
            out.push_str("\x1b[0m");
            for (c, fg) in [(cell.fg, true), (cell.bg, false)] {
                match c {
                    Color::Rgb(r, g, b) => {
                        write!(out, "\x1b[{};2;{r};{g};{b}m", if fg { 38 } else { 48 }).unwrap()
                    }
                    Color::Indexed(n) => {
                        write!(out, "\x1b[{};5;{n}m", if fg { 38 } else { 48 }).unwrap()
                    }
                    Color::Reset => {}
                    c => {
                        let n = match c {
                            Color::Black => 0,
                            Color::Red => 1,
                            Color::Green => 2,
                            Color::Yellow => 3,
                            Color::Blue => 4,
                            Color::Magenta => 5,
                            Color::Cyan => 6,
                            Color::Gray => 7,
                            Color::DarkGray => 8,
                            Color::LightRed => 9,
                            Color::LightGreen => 10,
                            Color::LightYellow => 11,
                            Color::LightBlue => 12,
                            Color::LightMagenta => 13,
                            Color::LightCyan => 14,
                            Color::White => 15,
                            _ => unreachable!(),
                        };
                        write!(out, "\x1b[{};5;{n}m", if fg { 38 } else { 48 }).unwrap();
                    }
                }
            }
            for (m, n) in [
                (Modifier::BOLD, 1),
                (Modifier::DIM, 2),
                (Modifier::ITALIC, 3),
                (Modifier::UNDERLINED, 4),
                (Modifier::REVERSED, 7),
                (Modifier::CROSSED_OUT, 9),
            ] {
                if cell.modifier.contains(m) {
                    write!(out, "\x1b[{n}m").unwrap();
                }
            }
            out.push_str(cell.symbol());
            x += unicode_width::UnicodeWidthStr::width(cell.symbol()).max(1) as u16;
        }
    }
    out.push_str("\x1b[0m");
    out
}

#[tokio::test]
#[ignore = "isolated visual suite: cargo test --lib visual_snapshots -- --ignored --test-threads=1"]
async fn visual_snapshots() {
    let now = Instant::now();
    let unix = chrono::DateTime::parse_from_rfc3339("2030-01-15T21:00:00Z")
        .unwrap()
        .timestamp();
    let _clock = view_time::Frozen::new(now, unix);
    let mut settings = insta::Settings::clone_current();
    settings.set_snapshot_path("snapshots/visual");
    settings.set_prepend_module_to_snapshot(false);
    let _settings = settings.bind_to_scope();
    let output = std::env::var_os("CHANTO_VISUAL_OUT").map(std::path::PathBuf::from);
    if let Some(dir) = &output {
        std::fs::create_dir_all(dir).unwrap();
    }
    let mut failures = Vec::new();
    for theme in ["dark", "light"] {
        for &(w, h) in &[(40, 20), (80, 24), (120, 30)] {
            for case in CASES {
                let (mut t, _replies) = fixture(case, now, unix);
                theme::enable_colors_for_test();
                theme::switch(theme).unwrap();
                let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
                terminal.draw(|f| t.app.draw(f)).unwrap();
                let buffer = terminal.backend().buffer();
                let name = format!("{theme}_{w}x{h}_{case}");
                if let Some(dir) = &output {
                    std::fs::write(dir.join(format!("{name}.ansi")), ansi(buffer)).unwrap();
                    std::fs::write(dir.join(format!("{name}.txt")), snapshot(buffer)).unwrap();
                }
                if std::panic::catch_unwind(|| {
                    insta::assert_snapshot!(name.clone(), snapshot(buffer))
                })
                .is_err()
                {
                    failures.push(name);
                }
            }
        }
    }
    theme::enable_colors_for_test();
    assert!(
        failures.is_empty(),
        "visual snapshots changed: {}",
        failures.join(", ")
    );
}
