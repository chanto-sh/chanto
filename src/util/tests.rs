#[cfg(unix)]
use super::{append_private, free_bytes, write_private};
use super::{duration, is_image_path, plural, short_num, strip_terminal_escapes, unescape_path};
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
