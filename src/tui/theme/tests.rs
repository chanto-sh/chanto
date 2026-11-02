use super::*;

#[test]
fn every_builtin_sets_every_key() {
    for (name, text) in BUILTIN {
        let p = parse(name, text, true).unwrap_or_else(|e| panic!("{e}"));
        assert_ne!(p, MONO, "{name}");
    }
    // The defaults keep the built-in dark theme's colors.
    let dark = builtin("dark").unwrap();
    assert_eq!(dark.accent, Color::Rgb(0x34, 0xd3, 0x99));
    assert_eq!(dark.error, Color::Red);
    assert!(!dark.light && builtin("light").unwrap().light);
}

#[test]
fn the_colorblind_theme_separates_the_hard_pairs() {
    let p = builtin("colorblind").unwrap();
    // Diff add/remove split by lightness as well as hue — the Okabe–Ito blue
    // and orange — so a diff is still readable in grayscale or with red-green
    // color blindness, on top of the `+`/`-` markers.
    assert_eq!(p.diff_add, Color::Rgb(0x56, 0xb4, 0xe9));
    assert_eq!(p.diff_remove, Color::Rgb(0xe6, 0x9f, 0x00));
    assert_ne!(p.success, p.error, "success and error are different colors");
}

#[test]
fn menus_stand_out_from_common_terminal_backgrounds() {
    for (name, _) in BUILTIN {
        let p = builtin(name).unwrap();
        assert_ne!(p.selection_bg, p.panel_bg, "{name}");
        if !p.light {
            assert!(
                ![Color::Rgb(0, 0, 0), Color::Rgb(0x1e, 0x1e, 0x1e)].contains(&p.panel_bg),
                "{name}"
            );
        }
    }
}

#[test]
fn strict_parsing_names_what_is_missing() {
    let e = parse(
        "half",
        "background = \"dark\"\naccent = \"#ffffff\"\n",
        true,
    )
    .unwrap_err();
    assert!(
        e.contains("missing") && e.contains("dim") && !e.contains("accent,"),
        "{e}"
    );
}

#[test]
fn a_user_theme_overrides_its_base() {
    let p = parse(
        "mine",
        "base = \"nord\"\naccent = \"#abc\"\nerror = \"light-red\"\n",
        false,
    )
    .unwrap();
    assert_eq!(p.accent, Color::Rgb(0xaa, 0xbb, 0xcc));
    assert_eq!(p.error, Color::LightRed);
    assert_eq!(p.success, builtin("nord").unwrap().success);
    // No base: the dark theme.
    let p = parse("mine", "background = \"light\"\nlink = \"#010203\"", false).unwrap();
    assert!(p.light);
    assert_eq!(p.link, Color::Rgb(1, 2, 3));
    assert_eq!(p.dim, builtin("dark").unwrap().dim);
}

#[test]
fn bad_theme_files_say_what_is_wrong() {
    let e = parse("t", "acent = \"#ffffff\"", false).unwrap_err();
    assert!(
        e.contains("unknown key `acent`") && e.contains("accent"),
        "{e}"
    );
    let e = parse("t", "accent = \"#ggg\"", false).unwrap_err();
    assert!(e.contains("`accent` = \"#ggg\" is not a color"), "{e}");
    let e = parse("t", "accent = 5", false).unwrap_err();
    assert!(e.contains("must be a string"), "{e}");
    let e = parse("t", "base = \"nope\"", false).unwrap_err();
    assert!(e.contains("not a built-in"), "{e}");
    let e = parse("t", "background = \"blue\"", false).unwrap_err();
    assert!(e.contains("\"dark\" or \"light\""), "{e}");
    assert!(
        parse("t", "accent = ", false)
            .unwrap_err()
            .starts_with("theme `t`:")
    );
    // A built-in can't lean on a base.
    assert!(
        parse("t", "base = \"dark\"", true)
            .unwrap_err()
            .contains("unknown key `base`")
    );
}

#[test]
fn colors_parse_from_hex_and_names() {
    assert_eq!(parse_color("#FF8000"), Some(Color::Rgb(255, 128, 0)));
    assert_eq!(parse_color("#f80"), Some(Color::Rgb(255, 136, 0)));
    assert_eq!(parse_color("Dark Gray"), Some(Color::DarkGray));
    assert_eq!(parse_color("default"), Some(Color::Reset));
    for bad in ["#12345", "#+12345", "#ééé", "orange", ""] {
        assert_eq!(parse_color(bad), None, "{bad}");
    }
}

#[test]
fn user_themes_load_from_the_themes_dir() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("chanto-theme-{}", uuid::Uuid::new_v4()));
    let previous = std::env::var_os("CHANTO_HOME");
    // SAFETY: test-only, under TEST_HOME_LOCK.
    unsafe { std::env::set_var("CHANTO_HOME", &home) };
    std::fs::create_dir_all(dir()).unwrap();
    std::fs::write(
        dir().join("mine.toml"),
        "base = \"gruvbox\"\naccent = \"#123456\"\n",
    )
    .unwrap();
    std::fs::write(dir().join("broken.toml"), "accent = \"nope\"\n").unwrap();
    let (name, p) = load("mine").unwrap();
    assert_eq!(
        (name.as_str(), p.accent),
        ("mine", Color::Rgb(0x12, 0x34, 0x56))
    );
    assert!(load("broken").unwrap_err().contains("not a color"));
    assert!(
        load("missing")
            .unwrap_err()
            .contains("No theme named `missing`")
    );
    assert!(load("../mine").is_err());
    let names: Vec<String> = available()
        .into_iter()
        .filter(|(_, user)| *user)
        .map(|(n, _)| n)
        .collect();
    assert_eq!(names, vec!["broken".to_string(), "mine".to_string()]);
    unsafe {
        match previous {
            Some(p) => std::env::set_var("CHANTO_HOME", p),
            None => std::env::remove_var("CHANTO_HOME"),
        }
    }
    let _ = std::fs::remove_dir_all(&home);
}
