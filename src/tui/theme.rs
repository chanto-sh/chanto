//! Color themes: the palette every TUI color comes from. The built-ins are
//! TOML like the user's own (`~/.chanto/themes/<name>.toml`), so one parser
//! checks both. `NO_COLOR` turns colors off whatever the theme.

use ratatui::style::Color;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, RwLock};

macro_rules! palette {
    ($($(#[$doc:meta])* $key:ident),* $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq)]
        pub struct Palette {
            /// A light background: picks the syntax-highlighting theme.
            pub light: bool,
            $($(#[$doc])* pub $key: Color,)*
        }

        /// Every color a theme file sets.
        pub const KEYS: &[&str] = &[$(stringify!($key)),*];

        /// No colors at all (`NO_COLOR`); also where strict parsing starts.
        const MONO: Palette = Palette { light: false, $($key: Color::Reset,)* };

        impl Palette {
            fn slot(&mut self, key: &str) -> Option<&mut Color> {
                match key {
                    $(stringify!($key) => Some(&mut self.$key),)*
                    _ => None,
                }
            }
        }
    };
}

palette! {
    /// Spinner, list selections, the welcome box.
    accent,
    /// Secondary text: tool output, hints, the footer.
    dim,
    /// Barely-there hints ("ctrl+o to expand").
    faint,
    /// The prompt's frame in the default mode.
    border,
    error,
    success,
    warning,
    /// Plan mode, diff hunk headers.
    info,
    /// Accept-edits mode.
    accept_edits,
    /// Auto mode.
    auto_mode,
    /// Inline `code`.
    code,
    /// Code blocks without syntax highlighting; the shell (`!`) prompt.
    code_block,
    link,
    /// Background of the slash and @ menus; set apart from common terminal
    /// backgrounds (#000000, VS Code's #1e1e1e).
    panel_bg,
    /// Selected text, search hits, and the highlighted row of a menu.
    selection_bg,
    selection_fg,
    /// `+` and `-` lines in ```diff blocks.
    diff_add,
    diff_remove,
    /// Added and removed lines in edit previews.
    diff_add_bg,
    diff_remove_bg,
}

const DARK: &str = r##"
background = "dark"
accent = "#34d399"
dim = "#808080"
faint = "#5a5a5a"
border = "#646464"
error = "red"
success = "green"
warning = "yellow"
info = "cyan"
accept_edits = "#c8a0ff"
auto_mode = "#f5a623"
code = "#d28246"
code_block = "#6ea06e"
link = "#508cdc"
panel_bg = "#2f343e"
selection_bg = "#264f78"
selection_fg = "#f0f4fa"
diff_add = "green"
diff_remove = "red"
diff_add_bg = "#143c14"
diff_remove_bg = "#501414"
"##;

const LIGHT: &str = r##"
background = "light"
accent = "#10b981"
dim = "#808080"
faint = "#969696"
border = "#646464"
error = "red"
success = "green"
warning = "#a06400"
info = "#008296"
accept_edits = "#c8a0ff"
auto_mode = "#b06800"
code = "#d28246"
code_block = "#6ea06e"
link = "#508cdc"
panel_bg = "#e8e8e8"
selection_bg = "#005f9e"
selection_fg = "#f0f4fa"
diff_add = "green"
diff_remove = "red"
diff_add_bg = "#d2f0d2"
diff_remove_bg = "#fad7d7"
"##;

/// https://draculatheme.com/contribute (the official palette).
const DRACULA: &str = r##"
background = "dark"
accent = "#ff79c6"
dim = "#6272a4"
faint = "#4d5a82"
border = "#6272a4"
error = "#ff5555"
success = "#50fa7b"
warning = "#f1fa8c"
info = "#8be9fd"
accept_edits = "#bd93f9"
auto_mode = "#ffb86c"
code = "#ffb86c"
code_block = "#50fa7b"
link = "#8be9fd"
panel_bg = "#21222c"
selection_bg = "#44475a"
selection_fg = "#f8f8f2"
diff_add = "#50fa7b"
diff_remove = "#ff5555"
diff_add_bg = "#234a2f"
diff_remove_bg = "#5a2a33"
"##;

/// https://www.nordtheme.com/docs/colors-and-palettes
const NORD: &str = r##"
background = "dark"
accent = "#88c0d0"
dim = "#7b88a1"
faint = "#616e88"
border = "#4c566a"
error = "#bf616a"
success = "#a3be8c"
warning = "#ebcb8b"
info = "#8fbcbb"
accept_edits = "#b48ead"
auto_mode = "#d08770"
code = "#d08770"
code_block = "#a3be8c"
link = "#81a1c1"
panel_bg = "#3b4252"
selection_bg = "#434c5e"
selection_fg = "#eceff4"
diff_add = "#a3be8c"
diff_remove = "#bf616a"
diff_add_bg = "#3a4a3c"
diff_remove_bg = "#4f3539"
"##;

/// https://ethanschoonover.com/solarized/
const SOLARIZED_DARK: &str = r##"
background = "dark"
accent = "#268bd2"
dim = "#839496"
faint = "#586e75"
border = "#586e75"
error = "#dc322f"
success = "#859900"
warning = "#b58900"
info = "#2aa198"
accept_edits = "#6c71c4"
auto_mode = "#cb4b16"
code = "#cb4b16"
code_block = "#859900"
link = "#268bd2"
panel_bg = "#073642"
selection_bg = "#586e75"
selection_fg = "#fdf6e3"
diff_add = "#859900"
diff_remove = "#dc322f"
diff_add_bg = "#0f3a2a"
diff_remove_bg = "#3b1f22"
"##;

/// https://github.com/morhetz/gruvbox (dark, medium contrast).
const GRUVBOX: &str = r##"
background = "dark"
accent = "#fe8019"
dim = "#928374"
faint = "#665c54"
border = "#665c54"
error = "#fb4934"
success = "#b8bb26"
warning = "#fabd2f"
info = "#8ec07c"
accept_edits = "#d3869b"
auto_mode = "#d79921"
code = "#fe8019"
code_block = "#b8bb26"
link = "#83a598"
panel_bg = "#3c3836"
selection_bg = "#504945"
selection_fg = "#fbf1c7"
diff_add = "#b8bb26"
diff_remove = "#fb4934"
diff_add_bg = "#32361a"
diff_remove_bg = "#3c1f1e"
"##;

/// Okabe–Ito (https://jfly.uni-koeln.de/color/): eight colors that stay distinct
/// under the common forms of color blindness, chosen here for a dark terminal.
/// `+`/`-` diffs use the blue/orange pair, the safest two, on top of the `+`/`-`
/// markers, so the diff still reads in grayscale.
const COLORBLIND: &str = r##"
background = "dark"
accent = "#56b4e9"
dim = "#9a9a9a"
faint = "#6f6f6f"
border = "#8a8a8a"
error = "#d55e00"
success = "#009e73"
warning = "#f0e442"
info = "#0072b2"
accept_edits = "#cc79a7"
auto_mode = "#e69f00"
code = "#e69f00"
code_block = "#009e73"
link = "#56b4e9"
panel_bg = "#2b2b2b"
selection_bg = "#005a87"
selection_fg = "#ffffff"
diff_add = "#56b4e9"
diff_remove = "#e69f00"
diff_add_bg = "#12384f"
diff_remove_bg = "#4a3200"
"##;

const BUILTIN: &[(&str, &str)] = &[
    ("dark", DARK),
    ("light", LIGHT),
    ("dracula", DRACULA),
    ("nord", NORD),
    ("solarized-dark", SOLARIZED_DARK),
    ("gruvbox", GRUVBOX),
    ("colorblind", COLORBLIND),
];

static PALETTE: LazyLock<RwLock<Palette>> =
    LazyLock::new(|| RwLock::new(builtin("dark").expect("the dark theme parses")));
static NAME: RwLock<String> = RwLock::new(String::new());
static NO_COLOR: AtomicBool = AtomicBool::new(false);
/// Bumped on every switch, so cached renders know to redraw.
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// The current palette.
pub fn pal() -> Palette {
    *PALETTE.read().unwrap_or_else(|e| e.into_inner())
}

pub fn epoch() -> u64 {
    EPOCH.load(Ordering::Relaxed)
}

/// The current theme's name.
pub fn current() -> String {
    NAME.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// `NO_COLOR` is set: no theme applies.
pub fn no_color() -> bool {
    NO_COLOR.load(Ordering::Relaxed)
}

/// Tests only. `NO_COLOR` is a process-wide latch, so a single test that builds
/// the app while `NO_COLOR` is set turns colors off for every other test in the
/// binary. This clears the latch and restores the default colored theme.
#[cfg(test)]
pub fn enable_colors_for_test() {
    NO_COLOR.store(false, Ordering::Relaxed);
    if let Err(e) = switch("dark") {
        panic!("test theme did not load: {e}");
    }
}

fn set(name: &str, p: Palette) {
    *PALETTE.write().unwrap_or_else(|e| e.into_inner()) = p;
    *NAME.write().unwrap_or_else(|e| e.into_inner()) = name.to_string();
    super::markdown::set_light(p.light);
    EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// Startup: apply the configured theme, unless `NO_COLOR` is set. Returns a
/// warning when the configured theme can't be used (the default applies).
pub fn init(setting: Option<&str>) -> Option<String> {
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        NO_COLOR.store(true, Ordering::Relaxed);
        set(
            "none (NO_COLOR)",
            Palette {
                light: super::markdown::detect_light(),
                ..MONO
            },
        );
        return None;
    }
    match switch(setting.unwrap_or("auto")) {
        Ok(_) => None,
        Err(e) => {
            let _ = switch("auto");
            Some(format!("{e}; using the default theme"))
        }
    }
}

/// Switch to a theme by name; returns the name that applied (`auto` resolves
/// to `dark` or `light`).
pub fn switch(name: &str) -> Result<String, String> {
    if no_color() {
        return Err("NO_COLOR is set, so colors stay off".into());
    }
    let (name, p) = load(name)?;
    set(&name, p);
    Ok(name)
}

/// Resolve a theme: `auto`, a built-in, or `~/.chanto/themes/<name>.toml`.
pub fn load(name: &str) -> Result<(String, Palette), String> {
    let name = name.trim();
    if name.is_empty() || name == "auto" {
        let n = if super::markdown::detect_light() {
            "light"
        } else {
            "dark"
        };
        return Ok((n.into(), builtin(n).expect("built-in")));
    }
    if let Some(p) = builtin(name) {
        return Ok((name.into(), p));
    }
    if name.contains(['/', '\\']) || name.starts_with('.') {
        return Err(format!("`{name}` is not a theme name"));
    }
    let path = dir().join(format!("{name}.toml"));
    let text = std::fs::read_to_string(&path).map_err(|_| {
        format!(
            "No theme named `{name}` (built-in: {}; or create {})",
            BUILTIN.iter().map(|b| b.0).collect::<Vec<_>>().join(", "),
            path.display()
        )
    })?;
    parse(name, &text, false).map(|p| (name.to_string(), p))
}

/// Where user themes live.
pub fn dir() -> std::path::PathBuf {
    crate::config::home_dir().join("themes")
}

/// Theme names: the built-ins, then the user's (marked `true`).
pub fn available() -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = BUILTIN.iter().map(|b| (b.0.to_string(), false)).collect();
    let mut user: Vec<String> = std::fs::read_dir(dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("toml"))
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
        .filter(|n| builtin(n).is_none())
        .collect();
    user.sort();
    out.extend(user.into_iter().map(|n| (n, true)));
    out
}

fn builtin(name: &str) -> Option<Palette> {
    let (n, text) = BUILTIN.iter().find(|b| b.0 == name)?;
    parse(n, text, true).ok()
}

/// Parse a theme file. Strict (the built-ins): every key must be set. Otherwise
/// `base` names a built-in to start from (default `dark`) and the file sets
/// only what it changes. Unknown keys and bad colors are errors either way.
pub fn parse(name: &str, text: &str, strict: bool) -> Result<Palette, String> {
    let table: toml::Table =
        toml::from_str(text).map_err(|e| format!("theme `{name}`: {}", e.to_string().trim()))?;
    let mut p = if strict {
        MONO
    } else {
        match table.get("base") {
            None => builtin("dark").expect("built-in"),
            Some(toml::Value::String(b)) => builtin(b).ok_or_else(|| {
                format!(
                    "theme `{name}`: base `{b}` is not a built-in theme ({})",
                    BUILTIN.iter().map(|b| b.0).collect::<Vec<_>>().join(", ")
                )
            })?,
            Some(_) => return Err(format!("theme `{name}`: `base` must be a string")),
        }
    };
    let mut missing: Vec<&str> = KEYS.to_vec();
    let mut background = false;
    for (k, v) in &table {
        match k.as_str() {
            "base" if !strict => continue,
            "background" => {
                p.light = match v.as_str() {
                    Some("light") => true,
                    Some("dark") => false,
                    _ => {
                        return Err(format!(
                            "theme `{name}`: `background` must be \"dark\" or \"light\""
                        ));
                    }
                };
                background = true;
                continue;
            }
            _ => {}
        }
        let Some(slot) = p.slot(k) else {
            return Err(format!(
                "theme `{name}`: unknown key `{k}` (keys: background, base, {})",
                KEYS.join(", ")
            ));
        };
        let Some(s) = v.as_str() else {
            return Err(format!(
                "theme `{name}`: `{k}` must be a string such as \"#rrggbb\""
            ));
        };
        *slot = parse_color(s).ok_or_else(|| format!("theme `{name}`: `{k}` = \"{s}\" is not a color (use #rrggbb, #rgb or a name such as red)"))?;
        missing.retain(|m| *m != k.as_str());
    }
    if strict && (!missing.is_empty() || !background) {
        let mut all = missing;
        if !background {
            all.insert(0, "background");
        }
        return Err(format!("theme `{name}` is missing {}", all.join(", ")));
    }
    Ok(p)
}

/// `#rrggbb`, `#rgb`, an ANSI color name, or `default`.
pub fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        let byte = |h: &str| {
            if h.chars().all(|c| c.is_ascii_hexdigit()) {
                u8::from_str_radix(h, 16).ok()
            } else {
                None
            }
        };
        if !hex.is_ascii() {
            return None;
        }
        return match hex.len() {
            6 => Some(Color::Rgb(
                byte(&hex[0..2])?,
                byte(&hex[2..4])?,
                byte(&hex[4..6])?,
            )),
            3 => Some(Color::Rgb(
                byte(&hex[0..1])? * 17,
                byte(&hex[1..2])? * 17,
                byte(&hex[2..3])? * 17,
            )),
            _ => None,
        };
    }
    Some(
        match s.to_ascii_lowercase().replace(['-', '_', ' '], "").as_str() {
            "default" | "reset" => Color::Reset,
            "black" => Color::Black,
            "red" => Color::Red,
            "green" => Color::Green,
            "yellow" => Color::Yellow,
            "blue" => Color::Blue,
            "magenta" => Color::Magenta,
            "cyan" => Color::Cyan,
            "gray" | "grey" => Color::Gray,
            "darkgray" | "darkgrey" => Color::DarkGray,
            "lightred" => Color::LightRed,
            "lightgreen" => Color::LightGreen,
            "lightyellow" => Color::LightYellow,
            "lightblue" => Color::LightBlue,
            "lightmagenta" => Color::LightMagenta,
            "lightcyan" => Color::LightCyan,
            "white" => Color::White,
            _ => return None,
        },
    )
}

/// Edit previews come from the tools as ANSI with fixed backgrounds (see
/// `ui::diff`); paint those in the theme's diff colors instead.
pub fn ansi_bg(c: Color) -> Color {
    match c {
        Color::Rgb(20, 60, 20) => pal().diff_add_bg,
        Color::Rgb(80, 20, 20) => pal().diff_remove_bg,
        c => c,
    }
}

#[cfg(test)]
mod tests {
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
}
