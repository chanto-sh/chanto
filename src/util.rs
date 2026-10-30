//! Small shared helpers.

use std::path::{Path, PathBuf};

/// Uppercase the first character, leaving the rest alone.
/// `CHANTO_TIMING=1`: print elapsed time since process start at a phase
/// boundary, to stderr. For diagnosing slow starts and turns.
/// `fs::canonicalize` without the Windows `\\?\` verbatim prefix when the
/// path can be written without it. Git, TOML files and people don't
/// understand verbatim paths; elsewhere this is plain `canonicalize`.
pub fn canonical(p: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    dunce::canonicalize(p)
}

/// Method form of [`canonical`], used instead of `Path::canonicalize`.
pub trait Canonical {
    fn canonical(&self) -> std::io::Result<PathBuf>;
}
impl Canonical for Path {
    fn canonical(&self) -> std::io::Result<PathBuf> {
        canonical(self)
    }
}

pub fn timing(label: &str) {
    let (on, t0) = timing_state();
    if *on {
        eprintln!(
            "[timing] {:>7.1} ms  {label}",
            t0.elapsed().as_secs_f64() * 1000.0
        );
    }
}

/// Is `CHANTO_TIMING` on? Guards labels that cost something to build.
pub fn timing_on() -> bool {
    timing_state().0
}

fn timing_state() -> &'static (bool, std::time::Instant) {
    static START: std::sync::OnceLock<(bool, std::time::Instant)> = std::sync::OnceLock::new();
    START.get_or_init(|| {
        (
            std::env::var_os("CHANTO_TIMING").is_some(),
            std::time::Instant::now(),
        )
    })
}

pub fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Extensions we attach as images (the model's MIME is sniffed from the bytes).
pub const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// Whether a path names an image by extension.
pub fn is_image_path(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Undo the shell-style `\ ` escaping terminals put in drag-and-dropped paths,
/// so `Screenshot\ 2026.png` resolves to the real `Screenshot 2026.png`.
pub fn unescape_path(s: &str) -> String {
    s.replace("\\ ", " ")
}

use chanto_core::fs::temp_sibling;

/// Atomic write readable only by the owner (0600). For anything derived from a
/// conversation — exports, histories — which can contain secrets.
pub fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = temp_sibling(path);
    let res = (|| {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Create `dir` (and its parents) and make it readable only by the owner
/// (0700), tightening one that already exists.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(dir)?.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// Open a file to append to, creating it mode 0600 and tightening an existing
/// one. Used for the prompt history and learned signals, which would otherwise
/// be world-readable.
pub fn append_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let f = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    Ok(f)
}

pub use chanto_core::text::{changed_region, truncate};

/// Keep the head and tail of long output (errors usually live at the end).
pub fn truncate_middle(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let half = max / 2;
    let mut a = half;
    while !s.is_char_boundary(a) {
        a -= 1;
    }
    let mut b = s.len() - half;
    while !s.is_char_boundary(b) {
        b += 1;
    }
    let omitted = s[a..b].lines().count();
    format!(
        "{}\n\n… [{} lines / {} chars omitted] …\n\n{}",
        &s[..a],
        omitted,
        b - a,
        &s[b..]
    )
}

pub fn resolve(cwd: &Path, p: &str) -> PathBuf {
    let p = if let Some(rest) = p.strip_prefix("~/") {
        dirs::home_dir().unwrap_or_default().join(rest)
    } else if p == "~" {
        dirs::home_dir().unwrap_or_default()
    } else {
        PathBuf::from(p)
    };
    if p.is_absolute() { p } else { cwd.join(p) }
}

/// Free space on the filesystem holding `path`, in bytes.
#[cfg(unix)]
pub fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` only writes into the struct passed to it.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0)
        .then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(not(unix))]
pub fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Display a path relative to cwd when possible.
pub fn rel(cwd: &Path, p: &Path) -> String {
    p.strip_prefix(cwd)
        .map(|r| r.display().to_string())
        .unwrap_or_else(|_| p.display().to_string())
}

pub fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!(
            "{}…",
            flat.chars().take(max.saturating_sub(1)).collect::<String>()
        )
    }
}

/// Find an item by name (case-insensitive), falling back to a unique
/// plugin-namespaced match: `commit` finds `commit-commands:commit`.
pub fn find_named<'a, T>(items: &'a [T], name: &str, get: impl Fn(&T) -> &String) -> Option<&'a T> {
    if let Some(x) = items.iter().find(|x| get(x).eq_ignore_ascii_case(name)) {
        return Some(x);
    }
    let suffix = format!(":{}", name.to_ascii_lowercase());
    let mut hits = items
        .iter()
        .filter(|x| get(x).to_ascii_lowercase().ends_with(&suffix));
    match (hits.next(), hits.next()) {
        (Some(x), None) => Some(x),
        _ => None,
    }
}

/// Abbreviate a count for compact display: `942`, `1.2k`, `13.6k`, `1.5M`.
/// Token counters change on every token, so a fixed-width form keeps the status
/// line from shifting as the number grows.
pub fn short_num(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 999_500 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// A count with its noun in the right number: `1 line`, `2 lines`,
/// `3 entries`.
pub fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        return format!("1 {word}");
    }
    match word.strip_suffix('y') {
        Some(stem) if !stem.ends_with(['a', 'e', 'i', 'o', 'u']) => format!("{n} {stem}ies"),
        _ => format!("{n} {word}s"),
    }
}

/// Human-readable elapsed time for status lines and tool timings, following
/// the convention: bare seconds under a minute, then `1m 5s`, then
/// `1h 2m`. Long runs stop reading as an ever-growing `1069s`.
pub fn duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// The exit code (128 + the signal's number) once SIGTERM or SIGHUP arrived; 0 before.
pub static TERMINATED: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Call `f` with the exit code for each SIGTERM (143) or SIGHUP (129), after
/// recording it in [`TERMINATED`]. Once registered, the signals no longer
/// kill the process by themselves, so `f` must end it (after saving).
/// Nothing on other platforms.
#[cfg(unix)]
pub fn on_termination(f: impl Fn(i32) + Send + Sync + 'static) {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut hup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    ) else {
        return;
    };
    tokio::spawn(async move {
        loop {
            let code = tokio::select! {
                Some(()) = term.recv() => 143,
                Some(()) = hup.recv() => 129,
                else => return,
            };
            TERMINATED.store(code, std::sync::atomic::Ordering::SeqCst);
            f(code);
        }
    });
}

#[cfg(not(unix))]
pub fn on_termination(_f: impl Fn(i32) + Send + Sync + 'static) {}

/// The exit code a SIGTERM or SIGHUP asked for, if one arrived.
pub fn terminated() -> Option<i32> {
    Some(TERMINATED.load(std::sync::atomic::Ordering::SeqCst)).filter(|c| *c != 0)
}

/// Record the exit code a handled signal asks for (130 for SIGINT).
pub fn set_terminated(code: i32) {
    TERMINATED.store(code, std::sync::atomic::Ordering::SeqCst);
}

/// Model text made safe to print to a terminal: escape sequences (CSI, OSC,
/// DCS and the rest) and other control characters are removed, keeping
/// newlines and tabs. A model's reply could otherwise retitle the window,
/// rewrite the screen or plant a clipboard write (OSC 52).
pub fn strip_terminal_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\n' | '\t' => out.push(c),
            '\r' if it.peek() == Some(&'\n') => {}
            '\x1b' => match it.next() {
                // CSI: parameters and intermediates, then a final byte.
                Some('[') => {
                    for d in it.by_ref() {
                        if ('@'..='~').contains(&d) {
                            break;
                        }
                    }
                }
                // String sequences, ended by BEL or ESC \.
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    while let Some(d) = it.next() {
                        if d == '\x07' || d == '\u{9c}' {
                            break;
                        }
                        if d == '\x1b' {
                            it.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                // Two- or three-byte sequences (ESC ( B, ESC 7, …).
                Some(' '..='/') => {
                    it.next();
                }
                _ => {}
            },
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::{append_private, free_bytes, write_private};
    use super::{
        duration, is_image_path, plural, short_num, strip_terminal_escapes, unescape_path,
    };
    use std::path::Path;

    #[cfg(unix)]
    #[test]
    fn reports_free_space_for_an_existing_directory() {
        let free = free_bytes(&std::env::temp_dir()).expect("statvfs on a real directory");
        assert!(free > 0);
        assert_eq!(free_bytes(Path::new("/definitely/not/here")), None);
    }

    #[test]
    fn counts_agree_with_their_nouns() {
        assert_eq!(plural(1, "line"), "1 line");
        assert_eq!(plural(0, "line"), "0 lines");
        assert_eq!(plural(2, "line"), "2 lines");
        assert_eq!(plural(1, "entry"), "1 entry");
        assert_eq!(plural(3, "entry"), "3 entries");
        assert_eq!(plural(2, "day"), "2 days");
    }

    #[test]
    fn terminal_escapes_are_stripped_from_model_text() {
        let raw = "\x1b]0;pwned\x07Hi \x1b[31mred\x1b[0m\r\nclip\x1b]52;c;ZXZpbA==\x1b\\ \x1bPdcs\x1b\\ok\x1b(B\x08\x07\u{9b}\tend\n";
        assert_eq!(strip_terminal_escapes(raw), "Hi red\nclip ok\tend\n");
        assert_eq!(strip_terminal_escapes("plain ünïcode ✓"), "plain ünïcode ✓");
    }

    #[test]
    fn durations_read_in_minutes_and_hours() {
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(59), "59s");
        assert_eq!(duration(60), "1m 0s");
        assert_eq!(duration(1069), "17m 49s");
        assert_eq!(duration(3599), "59m 59s");
        assert_eq!(duration(3600), "1h 0m");
        assert_eq!(duration(7_500), "2h 5m");
    }

    #[cfg(unix)]
    #[test]
    fn conversation_files_are_readable_only_by_the_owner() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("chanto-private-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("export.md");
        write_private(&f, b"secret\n").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "secret\n");
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Rewriting an existing, world-readable file tightens its mode.
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&f, b"again\n").unwrap();
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Append-only files are created 0600 too, and an existing loose one is fixed.
        let h = dir.join("history.jsonl");
        std::fs::write(&h, "old\n").unwrap();
        std::fs::set_permissions(&h, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut fh = append_private(&h).unwrap();
        writeln!(fh, "new").unwrap();
        assert_eq!(
            std::fs::metadata(&h).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(std::fs::read_to_string(&h).unwrap().contains("new"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_num_abbreviates_consistently() {
        assert_eq!(short_num(0), "0");
        assert_eq!(short_num(999), "999");
        assert_eq!(short_num(1_000), "1.0k");
        assert_eq!(short_num(13_648), "13.6k");
        assert_eq!(short_num(999_499), "999.5k");
        // Rounds up into the next unit rather than printing 1000.0k.
        assert_eq!(short_num(999_600), "1.0M");
        assert_eq!(short_num(1_500_000), "1.5M");
    }

    #[test]
    fn image_paths_are_recognized_by_extension_case_insensitively() {
        assert!(is_image_path(Path::new("/tmp/Screen.PNG")));
        assert!(is_image_path(Path::new("a/b/c.webp")));
        assert!(!is_image_path(Path::new("/tmp/notes.txt")));
        assert!(!is_image_path(Path::new("/tmp/noext")));
    }

    #[test]
    fn shell_escaped_spaces_in_dropped_paths_are_unescaped() {
        // This is what a terminal inserts when a file with spaces is dragged in.
        assert_eq!(
            unescape_path("/var/folders/x/Screenshot\\ 2026-10-12\\ at\\ 7.46.25\\ PM.png"),
            "/var/folders/x/Screenshot 2026-10-12 at 7.46.25 PM.png"
        );
    }
}

/// `path` with the home directory written as `~`, for display.
pub fn tilde(path: &std::path::Path) -> String {
    let p = path.display().to_string();
    let Some(home) = std::env::var_os("HOME")
        .map(|h| h.to_string_lossy().trim_end_matches('/').to_string())
        .filter(|h| !h.is_empty())
    else {
        return p;
    };
    if p == home {
        "~".into()
    } else if let Some(rest) = p.strip_prefix(&format!("{home}/")) {
        format!("~/{rest}")
    } else {
        p
    }
}

/// A USD amount the same way everywhere: cents from $1 up, four decimals
/// below (cheap models cost fractions of a cent), plain `$0.00` for nothing.
pub fn usd(amount: f64) -> String {
    if amount == 0.0 || amount >= 1.0 {
        format!("${amount:.2}")
    } else {
        format!("${amount:.4}")
    }
}

/// `s` cut to its last `max` characters, with a leading `…` when shortened.
pub fn tail_chars(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else {
        format!(
            "…{}",
            s.chars().skip(n + 1 - max.max(1)).collect::<String>()
        )
    }
}

/// The name nearest to `typed` (a typo or two away), if any is close enough.
pub fn closest(typed: &str, names: impl Iterator<Item = String>) -> Option<String> {
    // Edit distance where swapping two neighbouring letters counts as one edit.
    fn distance(a: &str, b: &str) -> usize {
        let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for (i, row) in d.iter_mut().enumerate() {
            row[0] = i;
        }
        for (j, cell) in d[0].iter_mut().enumerate() {
            *cell = j;
        }
        for i in 1..=a.len() {
            for j in 1..=b.len() {
                let cost = usize::from(a[i - 1] != b[j - 1]);
                d[i][j] = (d[i - 1][j] + 1)
                    .min(d[i][j - 1] + 1)
                    .min(d[i - 1][j - 1] + cost);
                if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                    d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
                }
            }
        }
        d[a.len()][b.len()]
    }
    let typed = typed.to_lowercase();
    let len = typed.chars().count();
    let limit = if len <= 4 { 1 } else { 2 };
    // Ties go to the name closest in length (`modle` → `model`, not `mode`).
    names
        .map(|n| {
            (
                distance(&typed, &n.to_lowercase()),
                n.chars().count().abs_diff(len),
                n,
            )
        })
        .filter(|(d, _, _)| *d <= limit)
        .min_by_key(|(d, l, _)| (*d, *l))
        .map(|(_, _, n)| n)
}

/// Common emoji shortcodes (`:tada:`, `:+1:`…) replaced with the emoji.
/// Unknown names are left as typed, so `a:b:c` and code survive.
pub fn emoji_shortcodes(text: &str) -> String {
    const CODES: &[(&str, &str)] = &[
        ("+1", "👍"),
        ("thumbsup", "👍"),
        ("-1", "👎"),
        ("thumbsdown", "👎"),
        ("tada", "🎉"),
        ("rocket", "🚀"),
        ("fire", "🔥"),
        ("bug", "🐛"),
        ("sparkles", "✨"),
        ("heart", "❤️"),
        ("smile", "😄"),
        ("joy", "😂"),
        ("wink", "😉"),
        ("thinking", "🤔"),
        ("eyes", "👀"),
        ("pray", "🙏"),
        ("clap", "👏"),
        ("wave", "👋"),
        ("ok_hand", "👌"),
        ("100", "💯"),
        ("warning", "⚠️"),
        ("x", "❌"),
        ("white_check_mark", "✅"),
        ("check", "✔️"),
        ("heavy_check_mark", "✔️"),
        ("question", "❓"),
        ("exclamation", "❗"),
        ("bulb", "💡"),
        ("memo", "📝"),
        ("lock", "🔒"),
        ("key", "🔑"),
        ("hammer", "🔨"),
        ("wrench", "🔧"),
        ("gear", "⚙️"),
        ("zap", "⚡"),
        ("boom", "💥"),
        ("star", "⭐"),
        ("rotating_light", "🚨"),
        ("construction", "🚧"),
        ("recycle", "♻️"),
        ("art", "🎨"),
        ("lipstick", "💄"),
        ("package", "📦"),
        ("chart_with_upwards_trend", "📈"),
        ("coffee", "☕"),
        ("sweat_smile", "😅"),
        ("cry", "😢"),
        ("sob", "😭"),
        ("rage", "😡"),
        ("shrug", "🤷"),
        ("facepalm", "🤦"),
    ];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(':') {
        let after = &rest[start + 1..];
        let name_len = after.find(':').filter(|&n| {
            n > 0
                && after[..n]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-'))
        });
        match name_len.and_then(|n| {
            CODES
                .iter()
                .find(|(k, _)| *k == &after[..n])
                .map(|(_, e)| (n, *e))
        }) {
            Some((n, emoji)) => {
                out.push_str(&rest[..start]);
                out.push_str(emoji);
                rest = &after[n + 1..];
            }
            None => {
                out.push_str(&rest[..=start]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod emoji_tests {
    #[test]
    fn replaces_known_shortcodes_only() {
        use super::emoji_shortcodes as e;
        assert_eq!(e("shipped it :tada: :+1:"), "shipped it 🎉 👍");
        assert_eq!(e("std::io::Write and a:b:c"), "std::io::Write and a:b:c");
        assert_eq!(e(":notanemoji: stays"), ":notanemoji: stays");
        assert_eq!(e("time 10:30:45"), "time 10:30:45");
        assert_eq!(e(":fire::rocket:"), "🔥🚀");
    }
}
