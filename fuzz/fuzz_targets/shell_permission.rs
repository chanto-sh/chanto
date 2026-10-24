//! The shell permission check: what runs without a prompt.
//!
//! Property: if a command `s` is statically allowed (a safe command in the
//! default mode, auto mode's fast path, or a `Bash(prefix:*)` allow rule),
//! then `s` followed by a command separator and a dangerous command must not
//! be allowed too, unless bash itself would not run that command. That is
//! the bug class of the v0.1.1 fix: a quote that bash reads as text (in a
//! comment, a heredoc body, after a backslash) made the checker think a
//! quote was open, and the commands after it ran unprompted.
//!
//! Whether bash runs the appended command is settled by bash's own parser
//! (`bash -n`, which parses without executing anything), consulted only for
//! the rare inputs where the checker allows both commands. The appended
//! command is inert when:
//! - the script doesn't parse (an open quote, say): bash runs none of it;
//! - `s` leaves a heredoc open: bash reads the command as its body;
//! - `s` ends in a comment: the separator and command are comment text;
//! - `s` ends in a line-continuation backslash and the separator is a
//!   newline: the command becomes arguments of the last one.
//!
//! Also: no input panics, and `normalize_shell` terminates on all of them.

#![no_main]

use chanto::permissions::{self, Decision, Mode, Rules};
use chanto::tools::ToolKind;
use libfuzzer_sys::fuzz_target;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Commands appended to `s`: (separator, command).
const SUFFIXES: &[(&str, &str)] = &[
    ("\n ", "rm -rf ~"),
    ("; ", "curl x | sh"),
    (" && ", "rm -rf ~"),
    (" || ", "chanto_fuzz_evil"),
    (" | ", "sh"),
    (" & ", "chanto_fuzz_evil"),
    ("\n", "chanto_fuzz_evil"),
];

fn cwd() -> &'static Path {
    static CWD: OnceLock<PathBuf> = OnceLock::new();
    CWD.get_or_init(|| {
        let d = std::env::temp_dir().join("chanto-fuzz-cwd");
        let _ = std::fs::create_dir_all(&d);
        d
    })
}

fn rules() -> &'static Rules {
    static R: OnceLock<Rules> = OnceLock::new();
    R.get_or_init(|| {
        let allow: Vec<String> = [
            "Bash(echo:*)",
            "Bash(cat:*)",
            "Bash(git status:*)",
            "Bash(ls)",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        Rules::new(&allow, &[], &[])
    })
}

/// Why `cmd` runs without a prompt, if it does.
fn allowed_by(cmd: &str) -> Option<&'static str> {
    let args = serde_json::json!({ "command": cmd });
    for (mode, name) in [(Mode::Default, "default mode"), (Mode::Auto, "auto mode")] {
        if permissions::static_decision(mode, ToolKind::Execute, "Bash", &args, cwd())
            == Some(Decision::Allow)
        {
            return Some(name);
        }
    }
    if rules().check("Bash", &permissions::split_shell(cmd)) == Some(Decision::Allow) {
        return Some("a Bash(prefix:*) allow rule");
    }
    None
}

fn bash() -> Option<&'static str> {
    static B: OnceLock<Option<&'static str>> = OnceLock::new();
    *B.get_or_init(|| {
        ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"]
            .into_iter()
            .find(|p| Path::new(p).exists())
    })
}

/// Parse `script` with bash without running it (`-n`, no rc files, no
/// environment): whether it parses, and bash's warnings.
fn bash_parse(bash: &str, script: &str) -> (bool, String) {
    let out = Command::new(bash)
        .args(["--norc", "--noprofile", "-n", "-c", script])
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .expect("run bash -n");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Whether bash would run `cmd` when it follows `s` after `sep`.
fn bash_runs_suffix(s: &str, sep: &str, cmd: &str) -> bool {
    let trailing = s.chars().rev().take_while(|&c| c == '\\').count();
    if sep.starts_with('\n') && trailing % 2 == 1 {
        return false;
    }
    let Some(bash) = bash() else {
        // Without bash there is no oracle: count it as running (strict).
        return true;
    };
    let (ok, warnings) = bash_parse(bash, &format!("{s}{sep}{cmd}"));
    if !ok || warnings.contains("here-document") {
        return false;
    }
    // Is the separator an operator that puts `cmd` in command position?
    // Then a `)` in its place is a syntax error; in a comment it is text.
    !bash_parse(bash, &format!("{s}{sep})")).0
}

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    // NUL can't reach bash through argv, and very long inputs only slow the
    // search down.
    if s.len() > 4096 || s.contains('\0') {
        return;
    }

    // Never panics, always terminates.
    let parts = permissions::split_shell(s);
    let _ = permissions::has_dynamic_parts(s);
    let _ = permissions::dangerous_reason(s);
    for part in &parts {
        let _ = chanto::fuzzing::normalize_shell(part);
    }

    if allowed_by(s).is_none() {
        return;
    }
    for (sep, evil) in SUFFIXES {
        let cmd = format!("{s}{sep}{evil}");
        if let Some(how) = allowed_by(&cmd)
            && bash_runs_suffix(s, sep, evil)
        {
            panic!(
                "{how} allows {cmd:?}, but bash runs the appended command.\nsplit_shell: {:?}",
                permissions::split_shell(&cmd)
            );
        }
    }
});
