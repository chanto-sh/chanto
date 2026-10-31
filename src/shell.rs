//! Choosing a shell and running a one-liner through it, on any platform.
//!
//! The shell tool speaks POSIX, so on Unix we use bash. On Windows that shell
//! usually isn't there, so we prefer a bash that is (Git Bash, not WSL), then
//! PowerShell, then cmd.exe. `CHANTO_SHELL` overrides the choice; everything
//! that needs to run a command goes through here so there is one place to get
//! it right.
//!
//! The one exception is on Windows with the sandbox on: Git Bash can't start
//! under its restricted token (its runtime creates objects only the user's own
//! SID may write, which is what the token must not have), so the shell tool
//! uses PowerShell there; see [`tool_shell`]. Hooks and other commands the
//! user wrote run unsandboxed and keep [`shell`].

use std::path::{Path, PathBuf};

pub mod snapshot;

/// Whether a path is an executable file: on Unix, has an execute bit; on
/// Windows, a file is enough (PATHEXT decides what runs).
pub fn is_exec(p: &Path) -> bool {
    let Ok(md) = p.metadata() else {
        return false;
    };
    if !md.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        true
    }
}

/// Find an executable on PATH without shelling out, so it works on Windows.
/// On Windows an extension may be implied: `bash` also finds `bash.exe`.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) && Path::new(name).extension().is_none() {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| e.to_lowercase())
            .collect()
    } else {
        Vec::new()
    };
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if is_exec(&cand) {
            return Some(cand);
        }
        for ext in &exts {
            let cand = dir.join(format!("{name}{ext}"));
            if is_exec(&cand) {
                return Some(cand);
            }
        }
    }
    None
}

/// Whether a command is available to run.
pub fn command_exists(name: &str) -> bool {
    // An explicit path is not on PATH, but is still runnable.
    let p = Path::new(name);
    if p.is_absolute() || name.contains('/') || name.contains('\\') {
        return is_exec(p);
    }
    find_on_path(name).is_some()
}

/// The shell program and the arguments it takes to run a one-liner.
pub fn shell() -> (String, Vec<&'static str>) {
    if let Some(s) = std::env::var("CHANTO_SHELL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        let base = Path::new(&s)
            .file_name()
            .and_then(|b| b.to_str())
            .unwrap_or(&s)
            .to_ascii_lowercase();
        let flag = if base == "cmd" || base == "cmd.exe" {
            "/C"
        } else {
            "-c"
        };
        return (s, vec![flag]);
    }
    #[cfg(unix)]
    {
        for s in ["/bin/bash", "/usr/bin/bash", "/opt/homebrew/bin/bash"] {
            if Path::new(s).exists() {
                return (s.into(), vec!["-c"]);
            }
        }
        ("sh".into(), vec!["-c"])
    }
    #[cfg(windows)]
    {
        // The tool speaks POSIX: a real bash (Git Bash) is the best match.
        if let Some(p) = windows_bash() {
            return (p.display().to_string(), vec!["-c"]);
        }
        windows_native_shell()
    }
}

#[cfg(windows)]
fn windows_native_shell() -> (String, Vec<&'static str>) {
    if let Some(p) = find_on_path("pwsh.exe") {
        return (p.display().to_string(), vec!["-NoProfile", "-Command"]);
    }
    if let Some(p) = find_on_path("powershell.exe") {
        return (p.display().to_string(), vec!["-NoProfile", "-Command"]);
    }
    (
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()),
        vec!["/C"],
    )
}

/// Set at startup from `[sandbox] enabled`. Not changed by `/sandbox`, so the
/// shell the model was told about stays the shell for the whole session.
static SANDBOXED_TOOL_SHELL: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn set_sandboxed_tool_shell(on: bool) {
    SANDBOXED_TOOL_SHELL.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// The shell the model's commands run in.
pub fn tool_shell() -> (String, Vec<&'static str>) {
    #[cfg(windows)]
    if SANDBOXED_TOOL_SHELL.load(std::sync::atomic::Ordering::Relaxed)
        && std::env::var("CHANTO_SHELL").map_or(true, |s| s.trim().is_empty())
    {
        return windows_native_shell();
    }
    shell()
}

/// The quoting rules of [`tool_shell`].
pub fn tool_flavor() -> Flavor {
    flavor_of(&tool_shell().0)
}

/// A bash that runs Windows programs in Windows paths: Git Bash (or MSYS2,
/// Cygwin) from PATH, else Git's default install. `System32\bash.exe` is the
/// WSL launcher, which runs commands inside Linux where the project's
/// `C:\...` paths and Windows tools don't exist, so it is skipped.
#[cfg(windows)]
fn windows_bash() -> Option<PathBuf> {
    let system = std::env::var_os("SystemRoot").map(PathBuf::from);
    let path = std::env::var_os("PATH").unwrap_or_default();
    let from_path = std::env::split_paths(&path)
        .filter(|dir| {
            !system
                .as_ref()
                .is_some_and(|s| starts_with_ignore_case(dir, s))
        })
        .map(|dir| dir.join("bash.exe"))
        .find(|p| is_exec(p));
    from_path.or_else(|| {
        ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
            .iter()
            .filter_map(std::env::var_os)
            .map(|d| PathBuf::from(d).join("Git").join("bin").join("bash.exe"))
            .find(|p| is_exec(p))
    })
}

#[cfg(windows)]
fn starts_with_ignore_case(p: &Path, prefix: &Path) -> bool {
    let (p, prefix) = (
        p.to_string_lossy().to_lowercase(),
        prefix.to_string_lossy().to_lowercase(),
    );
    let prefix = prefix.trim_end_matches('\\');
    p == prefix || p.starts_with(&format!("{prefix}\\"))
}

/// A bash to run POSIX scripts with: plain `bash` on Unix; on Windows Git Bash
/// (or MSYS2, Cygwin), never `System32\\bash.exe`, the WSL launcher. None
/// when Windows has no such bash.
pub fn bash() -> Option<String> {
    #[cfg(windows)]
    {
        windows_bash().map(|p| p.display().to_string())
    }
    #[cfg(not(windows))]
    {
        Some("bash".into())
    }
}

/// The quoting rules of the shell [`command`] runs scripts with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    /// bash or sh, including Git Bash on Windows.
    Posix,
    PowerShell,
    Cmd,
}

pub fn flavor() -> Flavor {
    flavor_of(&shell().0)
}

fn flavor_of(program: &str) -> Flavor {
    let base = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    let base = base.strip_suffix(".exe").unwrap_or(&base);
    match base {
        "pwsh" | "powershell" => Flavor::PowerShell,
        "cmd" => Flavor::Cmd,
        _ => Flavor::Posix,
    }
}

/// `s` as one literal word for a PowerShell command line: single quotes, in
/// which nothing expands. PowerShell also treats the typographic single
/// quotes as quotes, so those are doubled too.
pub fn powershell_quote(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// A `Command` that runs `script` through the shell.
pub fn command(script: &str) -> tokio::process::Command {
    let (prog, flags) = shell();
    let mut c = tokio::process::Command::new(prog);
    c.args(&flags).arg(script);
    c
}

/// As [`command`], for synchronous use.
pub fn std_command(script: &str) -> std::process::Command {
    let (prog, flags) = shell();
    let mut c = std::process::Command::new(prog);
    c.args(&flags).arg(script);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flavors_and_powershell_quoting() {
        assert_eq!(flavor_of("/bin/bash"), Flavor::Posix);
        assert_eq!(
            flavor_of(r"C:\Program Files\Git\bin\bash.exe"),
            Flavor::Posix
        );
        assert_eq!(
            flavor_of(r"C:\Program Files\PowerShell\7\pwsh.exe"),
            Flavor::PowerShell
        );
        assert_eq!(flavor_of("powershell.exe"), Flavor::PowerShell);
        assert_eq!(flavor_of(r"C:\Windows\system32\CMD.EXE"), Flavor::Cmd);
        assert_eq!(powershell_quote("a $(b) %c%.rs"), "'a $(b) %c%.rs'");
        assert_eq!(
            powershell_quote("it's\u{2019}.rs"),
            "'it''s\u{2019}\u{2019}.rs'"
        );
    }
}
