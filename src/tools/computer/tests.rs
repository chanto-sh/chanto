use super::*;

#[test]
fn parses_and_validates_actions() {
    assert_eq!(
        parse_action(&json!({"action": "screenshot"})),
        Ok(Action::Screenshot)
    );
    assert_eq!(
        parse_action(&json!({"action": "left_click", "coordinate": [10, 20.5]})),
        Ok(Action::Click {
            at: (10.0, 20.5),
            button: Button::Left,
            count: 1
        })
    );
    assert_eq!(
        parse_action(&json!({"action": "double_click", "x": 3, "y": "4"})),
        Ok(Action::Click {
            at: (3.0, 4.0),
            button: Button::Left,
            count: 2
        })
    );
    assert_eq!(
        parse_action(&json!({"action": "right_click", "coordinate": "[7, 8]"})),
        Ok(Action::Click {
            at: (7.0, 8.0),
            button: Button::Right,
            count: 1
        })
    );
    assert_eq!(
        parse_action(
            &json!({"action": "left_click_drag", "start_coordinate": [1, 2], "coordinate": [3, 4]})
        ),
        Ok(Action::Drag {
            from: (1.0, 2.0),
            to: (3.0, 4.0)
        })
    );
    assert_eq!(
        parse_action(&json!({"action": "scroll", "scroll_direction": "up"})),
        Ok(Action::Scroll {
            at: None,
            dir: Dir::Up,
            amount: 3
        })
    );
    assert_eq!(
        parse_action(&json!({"action": "open_app", "app": "Calculator"})),
        Ok(Action::OpenApp {
            app: "Calculator".into()
        })
    );
    assert_eq!(
        parse_action(&json!({"action": "wait"})),
        Ok(Action::Wait { secs: 1.0 })
    );
    for (bad, why) in [
        (json!({}), "missing `action`"),
        (json!({"action": "explode"}), "unknown action"),
        (json!({"action": "left_click"}), "needs `coordinate`"),
        (
            json!({"action": "left_click", "coordinate": [-1, 5]}),
            "non-negative",
        ),
        (
            json!({"action": "left_click", "coordinate": [1]}),
            "must be [x, y]",
        ),
        (
            json!({"action": "left_click_drag", "coordinate": [1, 2]}),
            "start_coordinate",
        ),
        (
            json!({"action": "scroll", "scroll_direction": "sideways"}),
            "unknown scroll_direction",
        ),
        (json!({"action": "scroll", "scroll_amount": 500}), "1-50"),
        (json!({"action": "type"}), "needs `text`"),
        (
            json!({"action": "key", "text": "hyper+q"}),
            "unknown modifier",
        ),
        (json!({"action": "key", "text": "Launch"}), "unknown key"),
        (json!({"action": "open_app"}), "needs `app`"),
        (json!({"action": "wait", "duration": 100}), "0-30"),
    ] {
        let e = parse_action(&bad).unwrap_err();
        assert!(e.contains(why), "{bad}: {e}");
    }
}

#[test]
fn parses_key_combos() {
    assert_eq!(
        parse_combo("cmd+shift+t"),
        Ok(Combo {
            mods: vec![Mod::Cmd, Mod::Shift],
            key: Key::Char('t')
        })
    );
    assert_eq!(
        parse_combo("Return"),
        Ok(Combo {
            mods: vec![],
            key: Key::Named(Named::Return)
        })
    );
    assert_eq!(
        parse_combo("ctrl+Enter"),
        Ok(Combo {
            mods: vec![Mod::Ctrl],
            key: Key::Named(Named::Return)
        })
    );
    assert_eq!(
        parse_combo("option+F5"),
        Ok(Combo {
            mods: vec![Mod::Alt],
            key: Key::Named(Named::F(5))
        })
    );
    assert_eq!(
        parse_combo("cmd++"),
        Ok(Combo {
            mods: vec![Mod::Cmd],
            key: Key::Char('+')
        })
    );
    assert_eq!(
        mac_key_request(&parse_combo("cmd+shift+t").unwrap()),
        json!({"op": "key", "ch": "t", "mods": ["command", "shift"]})
    );
    assert_eq!(
        mac_key_request(&parse_combo("Escape").unwrap()),
        json!({"op": "key", "code": 53, "mods": []})
    );
    assert_eq!(
        x_key(&parse_combo("cmd+PageDown").unwrap()),
        "super+Page_Down"
    );
    assert_eq!(x_key(&parse_combo("ctrl++").unwrap()), "ctrl+plus");
}

#[test]
fn scales_between_screenshot_and_screen_points() {
    // A 14" MacBook Pro: 1728x1117 points (3456x2234 pixels on Retina).
    let s = Scale::fit((1728.0, 1117.0), (1280, 800));
    assert_eq!(s.shot, (1238, 800));
    let (x, y) = s.to_screen((619.0, 400.0)).unwrap();
    assert!(
        (x - 864.0).abs() < 1.0 && (y - 558.5).abs() < 1.0,
        "{x},{y}"
    );
    assert_eq!(s.to_shot((864.0, 558.5)), (619, 400));
    assert_eq!(s.to_screen((0.0, 0.0)).unwrap(), (0.0, 0.0));
    assert!(
        s.to_screen((1238.0, 10.0))
            .unwrap_err()
            .contains("outside the 1238x800 screenshot")
    );
    assert!(s.to_screen((10.0, 800.0)).is_err());
    // Never enlarged: a small screen keeps its size and maps 1:1.
    let s = Scale::fit((1024.0, 768.0), (1280, 800));
    assert_eq!(s.shot, (1024, 768));
    assert_eq!(s.to_screen((100.0, 200.0)).unwrap(), (100.0, 200.0));
    // Round trips stay within a pixel.
    let s = Scale::fit((2560.0, 1440.0), (1280, 800));
    assert_eq!(s.shot, (1280, 720));
    for p in [(0.0, 0.0), (640.0, 360.0), (1279.0, 719.0)] {
        let (sx, sy) = s.to_shot(s.to_screen(p).unwrap());
        assert!((sx as f64 - p.0).abs() <= 1.0 && (sy as f64 - p.1).abs() <= 1.0);
    }
}

#[test]
fn maps_permission_errors() {
    assert_eq!(
        permission_from_error("could not create image from display\n"),
        Some(Permission::ScreenRecording)
    );
    assert_eq!(
        permission_from_error(
            "execution error: System Events got an error: osascript is not allowed assistive access. (-1719)"
        ),
        Some(Permission::Accessibility)
    );
    assert_eq!(
        permission_from_error(
            "execution error: Not authorized to send Apple events to System Events. (-1743)"
        ),
        Some(Permission::Automation)
    );
    assert_eq!(
        permission_from_error(
            "execution error: Error: chanto-accessibility: not trusted for accessibility"
        ),
        Some(Permission::Accessibility)
    );
    assert_eq!(permission_from_error("syntax error"), None);
    let m = permission_message(Permission::ScreenRecording, "iTerm");
    assert!(
        m.contains("Privacy & Security → Screen & System Audio Recording")
            && m.contains("turn it on for iTerm")
            && m.contains("reopen iTerm"),
        "{m}"
    );
    assert!(permission_message(Permission::Accessibility, "Terminal").contains("Accessibility"));
    assert!(permission_message(Permission::Automation, "Terminal").contains("System Events"));
}

/// Run by CI's wayland job, in a headless Sway session with grim and ydotoold.
#[tokio::test]
#[ignore = "needs a Wayland session with grim and ydotoold"]
async fn wayland_end_to_end() {
    // SAFETY: it runs alone, on request, in CI's headless session.
    unsafe { std::env::set_var("CHANTO_TEST_REAL_DESKTOP", "1") };
    support().unwrap();
    let (size, _) = screen_size(true).await.unwrap();
    assert!(size.0 >= 320.0 && size.1 >= 200.0, "{size:?}");
    let scale = Scale::fit(size, (1280, 800));
    let img = capture(&scale).await.unwrap();
    assert!(img.len() > 1000, "screenshot is {} bytes", img.len());
    for a in [
        Action::Click {
            at: (10.0, 10.0),
            button: Button::Left,
            count: 1,
        },
        Action::Type { text: "ok".into() },
        Action::Key {
            combo: Combo {
                mods: vec![Mod::Ctrl],
                key: Key::Char('a'),
            },
        },
    ] {
        let cmds = if ydotool_legacy() {
            ydotool_legacy_cmds(&a, &scale)
        } else {
            ydotool_cmds(&a, &scale)
        };
        for args in cmds.unwrap().unwrap() {
            let o = output("ydotool", &args).await.unwrap();
            assert!(
                o.status.success(),
                "ydotool {args:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        }
    }
}

#[test]
fn wayland_input_and_png_size() {
    let s = Scale {
        screen: (2000.0, 1000.0),
        shot: (1000, 500),
    };
    let cmds = |a: Action| {
        ydotool_cmds(&a, &s)
            .unwrap()
            .unwrap()
            .iter()
            .map(|c| c.join(" "))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        cmds(Action::Click {
            at: (10.0, 20.0),
            button: Button::Left,
            count: 2
        }),
        ["mousemove --absolute -x 20 -y 40", "click --repeat 2 0xC0"]
    );
    assert_eq!(
        cmds(Action::Scroll {
            at: None,
            dir: Dir::Down,
            amount: 3
        }),
        ["mousemove --wheel -x 0 -y -3"]
    );
    assert_eq!(
        cmds(Action::Drag {
            from: (0.0, 0.0),
            to: (5.0, 5.0)
        })[1],
        "click 0x40"
    );
    let key = |mods: Vec<Mod>, key: Key| ydotool_key(&Combo { mods, key }).unwrap().join(" ");
    assert_eq!(key(vec![Mod::Ctrl], Key::Char('c')), "29:1 46:1 46:0 29:0");
    assert_eq!(key(vec![], Key::Char('A')), "42:1 30:1 30:0 42:0");
    assert_eq!(
        key(vec![Mod::Cmd, Mod::Shift], Key::Named(Named::F(5))),
        "125:1 42:1 63:1 63:0 42:0 125:0"
    );
    assert_eq!(ev_code(Key::Char('/')), Some(53));
    assert!(
        ydotool_key(&Combo {
            mods: vec![],
            key: Key::Char('é')
        })
        .is_err()
    );
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    png.extend(1920u32.to_be_bytes());
    png.extend(1080u32.to_be_bytes());
    assert_eq!(png_size(&png), Some((1920.0, 1080.0)));
    assert_eq!(png_size(b"GIF89a"), None);
    // ydotool 0.1.x: absolute moves, numbered clicks, named combos, no scroll or drag.
    let old = |a: Action| {
        ydotool_legacy_cmds(&a, &s)
            .map(|c| c.unwrap().iter().map(|c| c.join(" ")).collect::<Vec<_>>())
    };
    assert_eq!(
        old(Action::Click {
            at: (10.0, 20.0),
            button: Button::Right,
            count: 2
        })
        .unwrap(),
        ["mousemove 20 40", "click 2", "click 2"]
    );
    assert_eq!(
        old(Action::Key {
            combo: Combo {
                mods: vec![Mod::Ctrl],
                key: Key::Char('T')
            }
        })
        .unwrap(),
        ["key ctrl+shift+t"]
    );
    assert!(
        old(Action::Scroll {
            at: None,
            dir: Dir::Down,
            amount: 1
        })
        .unwrap_err()
        .contains("1.0")
    );
    let sway = json!({"nodes": [{"nodes": [{"pid": 1, "name": "Terminal"}, {"name": "workspace"}]}], "floating_nodes": [{"pid": 2, "name": "Calc"}]});
    let mut t = window_titles(&sway);
    t.sort();
    assert_eq!(t, ["Calc", "Terminal"]);
    assert_eq!(
        window_titles(&json!([{"title": "Firefox"}, {"title": ""}])),
        ["Firefox"]
    );
}

#[test]
fn xdotool_commands_use_screen_points() {
    let s = Scale::fit((2560.0, 1440.0), (1280, 800));
    let args = |a: Action| xdotool_args(&a, &s).unwrap().unwrap().join(" ");
    assert_eq!(
        args(Action::Click {
            at: (100.0, 50.0),
            button: Button::Right,
            count: 1
        }),
        "mousemove --sync 200 100 click --repeat 1 3"
    );
    assert_eq!(
        args(Action::Drag {
            from: (0.0, 0.0),
            to: (10.0, 10.0)
        }),
        "mousemove --sync 0 0 mousedown 1 mousemove --sync 20 20 mouseup 1"
    );
    assert_eq!(
        args(Action::Scroll {
            at: None,
            dir: Dir::Down,
            amount: 2
        }),
        "click --repeat 2 5"
    );
    assert_eq!(
        args(Action::Type { text: "-rf".into() }),
        "type --delay 12 -- -rf"
    );
    assert!(
        xdotool_args(
            &Action::Click {
                at: (5000.0, 1.0),
                button: Button::Left,
                count: 1
            },
            &s
        )
        .is_err()
    );
    assert!(xdotool_args(&Action::Screenshot, &s).unwrap().is_none());
}

/// Takes a real screenshot: CHANTO_TEST_SCREENSHOT=1 on a Mac whose
/// terminal has the Screen Recording permission. Clicks nothing.
#[tokio::test]
async fn screenshot_on_macos() {
    if !cfg!(target_os = "macos") || std::env::var_os("CHANTO_TEST_SCREENSHOT").is_none() {
        return;
    }
    let c = Computer::new(&ComputerConfig::default());
    let out = c.act(&Action::Screenshot, true).await.unwrap();
    assert_eq!(out.images.len(), 1);
    let bytes = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(&out.images[0])
            .unwrap()
    };
    assert!(bytes.starts_with(&[0xff, 0xd8]), "a JPEG");
    assert!(out.content.contains("Screenshot of the main display"));
    let cursor = c.act(&Action::Cursor, true).await.unwrap();
    assert!(cursor.content.contains("screenshot coordinates"));
}
