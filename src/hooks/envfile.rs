//! Env files and the bash prelude: hooks on SessionStart / Setup / cwd or
//! file changes can export shell variables that later Bash tool calls pick
//! up. This writes the per-event env file, sources the right ones into the
//! Bash prelude, and parses plain `KEY=value` assignments for the model's
//! environment.

use super::*;

/// The events whose command hooks get `CLAUDE_ENV_FILE`.
const ENV_FILE_EVENTS: &[&str] = &["SessionStart", "Setup", "CwdChanged", "FileChanged"];

/// The session's `CLAUDE_ENV_FILE`s: one for SessionStart and Setup hooks,
/// kept for the whole session, and one for CwdChanged and FileChanged hooks,
/// cleared at each CwdChanged (as in Claude Code). None without a session.
pub(super) fn env_files(env: &Env) -> Option<(PathBuf, PathBuf)> {
    let id = env.session_id();
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    let dir = env
        .hook_state
        .env_dir
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| crate::config::home_dir().join("session-env"));
    Some((
        dir.join(format!("{id}.sh")),
        dir.join(format!("{id}.cwd.sh")),
    ))
}

/// Create `p` (empty) if it's missing; whether it exists now.
fn touch(p: &std::path::Path) -> bool {
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
        .is_ok()
}

/// The `CLAUDE_ENV_FILE` for `event`'s hooks, if it gets one.
pub(super) fn env_file_for(env: &Env, event: &str) -> Option<PathBuf> {
    // Hooks run by PowerShell or cmd don't get it, as in Claude Code.
    if !ENV_FILE_EVENTS.contains(&event) || !posix_shell() {
        return None;
    }
    let (session, cwd) = env_files(env)?;
    let p = if matches!(event, "SessionStart" | "Setup") {
        session
    } else {
        cwd
    };
    touch(&p).then_some(p)
}

/// At session start: create the session's env file (kept on resume) when a
/// hook could write to it.
pub fn prepare_env_file(env: &Env) {
    if ENV_FILE_EVENTS.iter().any(|e| has(env, e))
        && let Some((session, _)) = env_files(env)
    {
        touch(&session);
    }
}

/// Whether commands run in a POSIX shell (bash, Git Bash), not PowerShell or cmd.
fn posix_shell() -> bool {
    let (prog, _) = crate::shell::shell();
    let base = std::path::Path::new(&prog)
        .file_stem()
        .and_then(|b| b.to_str())
        .unwrap_or(&prog)
        .to_ascii_lowercase();
    !matches!(base.as_str(), "cmd" | "powershell" | "pwsh")
}

/// Every env file the session's commands get, in order: one named by
/// `CLAUDE_ENV_FILE` (or `CHANTO_ENV_FILE`) when Chanto started, then the
/// session's own two. Only non-empty files.
fn env_sources(env: &Env) -> Vec<PathBuf> {
    static LAUNCH: OnceLock<Option<PathBuf>> = OnceLock::new();
    let launch = LAUNCH.get_or_init(|| {
        std::env::var_os("CLAUDE_ENV_FILE")
            .or_else(|| std::env::var_os("CHANTO_ENV_FILE"))
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    });
    let mut out: Vec<PathBuf> = launch.iter().cloned().collect();
    if let Some((session, cwd)) = env_files(env) {
        out.extend([session, cwd]);
    }
    out.retain(|f| std::fs::metadata(f).is_ok_and(|m| m.is_file() && m.len() > 0));
    out
}

/// Shell code that runs the session's non-empty env files, to put before
/// every Bash tool command. Empty when there's nothing to run, or when the
/// shell isn't a POSIX one (PowerShell, cmd): those get [`plain_env_vars`].
pub fn bash_prelude(env: &Env) -> String {
    if !posix_shell() {
        return String::new();
    }
    let mut out = String::new();
    for f in env_sources(env) {
        // Git Bash reads `C:/...` paths; forward slashes need no escaping.
        let path = if cfg!(windows) {
            f.display().to_string().replace('\\', "/")
        } else {
            f.display().to_string()
        };
        out.push_str(&format!(
            ". '{}' >/dev/null 2>&1\n",
            path.replace('\'', r"'\''")
        ));
    }
    out
}

/// For commands run by PowerShell or cmd, which can't run the env files: the
/// variables they assign, when every line of every file is a plain assignment
/// (see [`plain_assignments`]); otherwise none. On Windows, values with `/` or
/// `\` are left out (Git Bash and Windows write paths differently).
pub fn plain_env_vars(env: &Env) -> Vec<(String, String)> {
    if posix_shell() {
        return vec![];
    }
    let mut vars = vec![];
    for f in env_sources(env) {
        let text = std::fs::read_to_string(&f).unwrap_or_default();
        let Some(v) = plain_assignments(&text) else {
            env.accounting.log(
                "debug",
                "session_env",
                json!({"message": "Session environment is not all plain assignments", "file": f}),
            );
            return vec![];
        };
        vars.extend(v);
    }
    if cfg!(windows) {
        vars.retain(|(_, v)| !v.contains('/') && !v.contains('\\'));
    }
    vars
}

/// Parse an env file made only of blank lines, `#` comments and assignments
/// (`export NAME=value`, `declare -x NAME=value` or `NAME=value`) whose value
/// Bash would use as written: unquoted `[A-Za-z0-9_@%+=:,./-]` text, '...'
/// text, and "..." text with every `$`, backtick and `"` backslash-escaped.
/// None if any line is something else. Assignments come in file order.
pub fn plain_assignments(text: &str) -> Option<Vec<(String, String)>> {
    let mut out = vec![];
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let rest = if let Some(r) = line.strip_prefix("export ") {
            r.trim_start()
        } else if let Some(r) = line
            .strip_prefix("declare ")
            .map(str::trim_start)
            .and_then(|r| r.strip_prefix("-x "))
        {
            r.trim_start()
        } else {
            line
        };
        let (name, value) = rest.split_once('=')?;
        let mut nc = name.chars();
        if !nc
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            || !nc.all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return None;
        }
        out.push((name.to_string(), plain_value(value)?));
    }
    Some(out)
}

fn plain_value(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => loop {
                match chars.next()? {
                    '\'' => break,
                    c => out.push(c),
                }
            },
            '"' => loop {
                match chars.next()? {
                    '"' => break,
                    '$' | '`' => return None,
                    '\\' => match chars.next()? {
                        // Bash drops the backslash only before these.
                        c @ ('$' | '`' | '"' | '\\') => out.push(c),
                        c => {
                            out.push('\\');
                            out.push(c);
                        }
                    },
                    c => out.push(c),
                }
            },
            c if c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c) => out.push(c),
            _ => return None,
        }
    }
    Some(out)
}
