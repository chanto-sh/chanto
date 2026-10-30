//! The user's shell setup for Bash commands (a "shell snapshot").
//!
//! Commands run under a plain `bash -c`, which reads no rc files, so PATH
//! changes and aliases and functions from `~/.zshrc` / `~/.bashrc` (nvm,
//! pyenv, asdf, Homebrew's shellenv, the user's own aliases) would be
//! missing. Once per session, on the first Bash command, Chanto runs the
//! user's `$SHELL` (zsh or bash) as an interactive login shell, with no
//! terminal, stdin from /dev/null, its output discarded and a time limit,
//! and has it dump what its startup files set up. Chanto turns that into a
//! private file of bash code that every command sources before it runs:
//!
//! - exported variables the startup files added or changed (PATH merged, so
//!   entries only Chanto's own environment has, like an activated venv,
//!   stay in front), minus secret-named ones and secret-looking values,
//!   terminal and prompt state, and what Chanto sets itself;
//! - functions (not completion, prompt or hook functions), each defined
//!   through its own `eval` so one bash can't parse (zsh syntax) is skipped;
//! - aliases, with `expand_aliases` on.
//!
//! Commands still run under bash: the tool's quoting, `pwd -P` bookkeeping
//! and sandbox wrapping stay the same for everyone, and what a zsh startup
//! file sets up mostly carries over (PATH and variables always; functions
//! when bash can parse them). The file is sourced inside the command's
//! process, so the sandbox applies to it like to the command.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The session's snapshot, taken on first use: the file to source, or None
/// (off, unsupported shell, or the capture failed).
pub type Slot = tokio::sync::OnceCell<Option<PathBuf>>;

/// How long the startup files may take.
pub const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FUNCTION: usize = 64 * 1024;
const MAX_TOTAL: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Bash,
    Zsh,
}

/// The shells whose setup can be captured.
pub fn kind_of(shell: &Path) -> Option<Kind> {
    let base = shell
        .file_name()?
        .to_str()?
        .trim_start_matches('-')
        .to_ascii_lowercase();
    match base.as_str() {
        "bash" => Some(Kind::Bash),
        "zsh" => Some(Kind::Zsh),
        _ => None,
    }
}

#[derive(Debug)]
pub enum Error {
    TimedOut,
    Unsupported(String),
    Failed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::TimedOut => write!(
                f,
                "the shell's startup files took longer than {} s",
                TIMEOUT.as_secs()
            ),
            Error::Unsupported(s) => write!(f, "unsupported shell {s}"),
            Error::Failed(s) => write!(f, "{s}"),
        }
    }
}

/// What the startup files set up, as the shell reported it.
#[derive(Debug, Default)]
pub struct Captured {
    pub vars: Vec<(String, String)>,
    /// (name, value)
    pub aliases: Vec<(String, String)>,
    /// (name, `alias name='value'` as bash prints it)
    pub alias_defs: Vec<(String, String)>,
    /// (name, definition)
    pub functions: Vec<(String, String)>,
}

fn truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}
fn falsy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// Whether Bash commands get the snapshot: `CHANTO_SHELL_SNAPSHOT` wins,
/// then Claude Code's `CLAUDE_BASH_NO_LOGIN` (set: off), then
/// `shell_snapshot` in the config (default on). Never on Windows, nor when
/// commands run in PowerShell or cmd.
pub fn enabled(config: &crate::config::Config) -> bool {
    if cfg!(windows) || crate::shell::flavor() != crate::shell::Flavor::Posix {
        return false;
    }
    if let Ok(v) = std::env::var("CHANTO_SHELL_SNAPSHOT") {
        if falsy(&v) {
            return false;
        }
        if truthy(&v) {
            return true;
        }
    }
    if std::env::var("CLAUDE_BASH_NO_LOGIN").is_ok_and(|v| truthy(&v)) {
        return false;
    }
    config.shell_snapshot.unwrap_or(true)
}

/// The user's shell, when it's one whose setup can be captured.
fn user_shell() -> Option<PathBuf> {
    let s = std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && crate::shell::is_exec(p))?;
    kind_of(&s).map(|_| s)
}

/// Take the session's snapshot if this is the first command that needs it.
/// Waits for a capture already under way.
pub async fn ensure(env: &crate::agent::Env) {
    env.shell_snapshot.get_or_init(|| init(env)).await;
}

/// Shell code that sources the session's snapshot, to run before a command
/// (empty without one). Call [`ensure`] first.
pub fn prelude(env: &crate::agent::Env) -> String {
    match env.shell_snapshot.get() {
        Some(Some(p)) => source_line(p),
        _ => String::new(),
    }
}

pub fn source_line(p: &Path) -> String {
    format!(". {} >/dev/null 2>&1\n", sq(&p.display().to_string()))
}

async fn init(env: &crate::agent::Env) -> Option<PathBuf> {
    // Tests set the slot themselves; never run the developer's own rc files.
    if cfg!(test) || !enabled(&env.config) {
        return None;
    }
    let Some(shell) = user_shell() else {
        env.accounting.log("debug", "shell_snapshot", serde_json::json!({"message": "no supported $SHELL (zsh or bash); commands run with chanto's environment"}));
        return None;
    };
    let config = env.config.clone();
    let t0 = Instant::now();
    let sh = shell.clone();
    let result = tokio::task::spawn_blocking(move || create(&sh, None, TIMEOUT, &config))
        .await
        .ok()?;
    match result {
        Ok(p) => {
            env.accounting.log(
                "info",
                "shell_snapshot",
                serde_json::json!({"shell": shell, "ms": t0.elapsed().as_millis() as u64}),
            );
            Some(p)
        }
        Err(e) => {
            env.accounting.log(
                "warn",
                "shell_snapshot",
                serde_json::json!({"shell": shell, "error": e.to_string()}),
            );
            if matches!(e, Error::TimedOut) {
                env.emit(crate::agent::Event::Warn(format!(
                    "Your shell's startup files ({}) took longer than {} s, so Bash commands run without them: PATH changes, aliases and functions from them are missing. \
                     Speed them up (they run with CHANTO_SHELL_SNAPSHOT=1 set) or set `shell_snapshot = false`.",
                    match kind_of(&shell) { Some(Kind::Zsh) => "~/.zshrc and friends", _ => "~/.bashrc and friends" },
                    TIMEOUT.as_secs()
                )));
            }
            None
        }
    }
}

/// Capture `shell`'s setup and write the snapshot file. `home` replaces
/// `$HOME` for the shell (tests).
pub fn create(
    shell: &Path,
    home: Option<&Path>,
    timeout: Duration,
    config: &crate::config::Config,
) -> Result<PathBuf, Error> {
    let dir = private_dir()
        .map_err(|e| Error::Failed(format!("can't create the snapshot directory: {e}")))?;
    let result = capture_in(&dir, shell, home, timeout).and_then(|c| {
        let functions_path = dir.join("functions.sh");
        let r = render(
            &c,
            &|k| std::env::var(k).ok(),
            &config.secrets,
            &|k| config.env.contains_key(k),
            &functions_path,
        );
        // The functions first: a stub that ran before they were written
        // would define nothing.
        write_private(&functions_path, r.functions.as_bytes())
            .map_err(|e| Error::Failed(format!("can't write the snapshot: {e}")))?;
        let p = dir.join("snapshot.sh");
        write_private(&p, r.snapshot.as_bytes())
            .map_err(|e| Error::Failed(format!("can't write the snapshot: {e}")))?;
        Ok(p)
    });
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    result
}

/// Run `shell` as an interactive login shell and collect what it set up.
#[cfg(test)]
pub fn capture(shell: &Path, home: Option<&Path>, timeout: Duration) -> Result<Captured, Error> {
    let dir = private_dir().map_err(|e| Error::Failed(e.to_string()))?;
    let r = capture_in(&dir, shell, home, timeout);
    let _ = std::fs::remove_dir_all(&dir);
    r
}

fn capture_in(
    dir: &Path,
    shell: &Path,
    home: Option<&Path>,
    timeout: Duration,
) -> Result<Captured, Error> {
    let kind = kind_of(shell).ok_or_else(|| Error::Unsupported(shell.display().to_string()))?;
    let out = dir.join("capture");
    write_private(&out, b"").map_err(|e| Error::Failed(e.to_string()))?;
    let home_dir = home.map(Path::to_path_buf).or_else(dirs::home_dir);
    let mut cmd = std::process::Command::new(shell);
    cmd.args(["-i", "-l", "-c", &capture_script(kind, home_dir.as_deref())])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .env("CHANTO_SNAPSHOT_OUT", &out)
        .env("CHANTO_SHELL_SNAPSHOT", "1")
        // Fancy prompts and terminal integrations stand down for a dumb terminal.
        .env("TERM", "dumb");
    if let Some(h) = &home_dir {
        cmd.current_dir(h);
    }
    if let Some(h) = home {
        cmd.env("HOME", h).env_remove("ZDOTDIR");
    }
    // Its own session: no controlling terminal, so an interactive shell
    // can't take over Chanto's terminal, and nothing can wait on /dev/tty.
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Failed(format!("can't start {}: {e}", shell.display())))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                #[cfg(unix)]
                unsafe {
                    // The whole session: the shell and anything its rc files started.
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::TimedOut);
            }
        }
    };
    let bytes = std::fs::read(&out).unwrap_or_default();
    // It holds every exported variable, secrets included, which the
    // snapshot itself withholds: don't leave it beside the snapshot.
    let _ = std::fs::remove_file(&out);
    parse(&bytes).ok_or_else(|| {
        Error::Failed(format!(
            "{} exited ({status}) without reporting its setup",
            shell.display()
        ))
    })
}

/// The command the shell runs once its startup files are done: write each
/// exported variable, alias and function as NUL-separated (kind, name,
/// value) records, then an end marker.
fn capture_script(kind: Kind, home: Option<&Path>) -> String {
    let dump = match kind {
        Kind::Bash => {
            r#"IFS=$' \t\n'
for __hx_n in $(compgen -e); do printf 'E\0%s\0%s\0' "$__hx_n" "${!__hx_n}"; done
for __hx_n in $(compgen -a); do printf 'D\0%s\0%s\0' "$__hx_n" "$(alias "$__hx_n")"; done
for __hx_n in $(compgen -A function); do printf 'F\0%s\0%s\0' "$__hx_n" "$(declare -f "$__hx_n")"; done"#
        }
        Kind::Zsh => {
            r#"IFS=$' \t\n'
for __hx_n in ${(k)parameters[(R)*export*]}; do printf 'E\0%s\0%s\0' "$__hx_n" "${(P)__hx_n}"; done
for __hx_n in ${(k)aliases}; do printf 'A\0%s\0%s\0' "$__hx_n" "${aliases[$__hx_n]}"; done
for __hx_n in ${(k)functions}; do printf 'F\0%s\0%s\0' "$__hx_n" "$(functions -- "$__hx_n")"; done"#
        }
    };
    // An interactive login bash reads only the first of its profile files;
    // most of them source ~/.bashrc, and when it doesn't, do it here.
    let bashrc = match (kind, home) {
        (Kind::Bash, Some(h)) if !profile_sources_bashrc(h) && h.join(".bashrc").is_file() => {
            ". ~/.bashrc </dev/null\n"
        }
        _ => "",
    };
    format!(
        "{bashrc}__hx_o=$CHANTO_SNAPSHOT_OUT\nunset CHANTO_SNAPSHOT_OUT CHANTO_SHELL_SNAPSHOT\n{{\n{dump}\nprintf 'Z\\0\\0\\0'\n}} >| \"$__hx_o\" 2>/dev/null\n"
    )
}

/// Whether the profile file a login bash reads mentions `.bashrc`.
fn profile_sources_bashrc(home: &Path) -> bool {
    [".bash_profile", ".bash_login", ".profile"]
        .iter()
        .map(|f| home.join(f))
        .find(|p| p.is_file())
        .is_some_and(|p| {
            std::fs::read_to_string(p)
                .unwrap_or_default()
                .contains("bashrc")
        })
}

fn parse(bytes: &[u8]) -> Option<Captured> {
    let fields: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
    let mut c = Captured::default();
    let mut done = false;
    for rec in fields.chunks(3) {
        let [kind, name, value] = rec else { break };
        let (name, value) = (
            String::from_utf8_lossy(name).into_owned(),
            String::from_utf8_lossy(value).into_owned(),
        );
        match *kind {
            b"E" => c.vars.push((name, value)),
            b"A" => c.aliases.push((name, value)),
            b"D" => c.alias_defs.push((name, value)),
            b"F" => c.functions.push((name, value)),
            b"Z" => {
                done = true;
                break;
            }
            _ => break,
        }
    }
    done.then_some(c)
}

/// `s` as one single-quoted shell word.
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn valid_var_name(n: &str) -> bool {
    let mut cs = n.chars();
    cs.next().is_some_and(|c| c.is_ascii_alphabetic())
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Variables that belong to the terminal, the prompt or one shell process,
/// or that Chanto sets for commands itself.
fn skip_var(n: &str) -> bool {
    const EXACT: &[&str] = &[
        "PWD",
        "OLDPWD",
        "SHLVL",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "TERM",
        "COLORTERM",
        "COLUMNS",
        "LINES",
        "TMPDIR",
        "TMP",
        "TEMP",
        "PAGER",
        "GIT_PAGER",
        "GIT_EDITOR",
        "EDITOR",
        "VISUAL",
        "NO_COLOR",
        "CLICOLOR",
        "CLICOLOR_FORCE",
        "FORCE_COLOR",
        "PS1",
        "PS2",
        "PS3",
        "PS4",
        "PROMPT",
        "RPROMPT",
        "RPS1",
        "PROMPT_COMMAND",
        "HISTFILE",
        "HISTSIZE",
        "HISTFILESIZE",
        "SAVEHIST",
        "HISTCONTROL",
        "HISTIGNORE",
        "SSH_AUTH_SOCK",
        "SSH_AGENT_PID",
        "SSH_TTY",
        "SSH_CONNECTION",
        "SSH_CLIENT",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "DBUS_SESSION_BUS_ADDRESS",
        "XDG_RUNTIME_DIR",
        "GPG_TTY",
        "TTY",
        "TMUX",
        "TMUX_PANE",
        "STY",
        "WINDOWID",
        "ZDOTDIR",
        "LC_TERMINAL",
        "LC_TERMINAL_VERSION",
        "MAIL",
        "MAILCHECK",
        "OPTIND",
        "BASH_ENV",
        "ENV",
        "IFS",
    ];
    const PREFIX: &[&str] = &[
        "CHANTO",
        "CLAUDE",
        "TERM_",
        "ITERM",
        "VSCODE_",
        "KITTY_",
        "WEZTERM_",
        "ALACRITTY_",
        "GHOSTTY_",
        "WT_",
        "SHELL_SESSION",
        "BASH_FUNC_",
        "P9K",
        "POWERLEVEL9K",
        "STARSHIP_",
        "ZSH_AUTOSUGGEST",
        "ZSH_HIGHLIGHT",
    ];
    let upper = n.to_ascii_uppercase();
    EXACT.contains(&n)
        || PREFIX.iter().any(|p| n.starts_with(p))
        || upper.ends_with("_PROXY")
        || upper == "NO_PROXY"
}

/// Functions that are completion, prompt or hook machinery, or that would
/// change the builtins Chanto's own command wrapper relies on.
fn skip_function(n: &str) -> bool {
    const EXACT: &[&str] = &[
        "cd",
        "pwd",
        "exit",
        "source",
        "eval",
        "export",
        "alias",
        "unalias",
        "builtin",
        "command",
        "exec",
        "printf",
        "echo",
        "test",
        "set",
        "unset",
        "shopt",
        "trap",
        "precmd",
        "preexec",
        "chpwd",
        "periodic",
        "zshexit",
        "zshaddhistory",
        "command_not_found_handle",
        "command_not_found_handler",
        "compdef",
        "compinit",
        "bashcompinit",
    ];
    const PREFIX: &[&str] = &[
        "_",
        "-",
        "+",
        ".",
        "TRAP",
        "prompt_",
        "p10k",
        "powerlevel",
        "starship_",
        "omz_",
        "iterm2_",
        "vte_",
        "zle-",
        "__",
    ];
    !n.chars()
        .all(|c| c.is_ascii_alphanumeric() || "_-.:+@".contains(c))
        || EXACT.contains(&n)
        || PREFIX.iter().any(|p| n.starts_with(p))
}

fn skip_alias(n: &str) -> bool {
    const EXACT: &[&str] = &[
        "cd",
        "pwd",
        "exit",
        "source",
        "eval",
        "export",
        "alias",
        "builtin",
        "command",
        "exec",
        "printf",
        "echo",
        "test",
        "set",
        "unset",
        "shopt",
        "trap",
        "run-help",
        "which-command",
    ];
    n.is_empty()
        || n.starts_with('-')
        || !n
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-.:+@,%".contains(c))
        || EXACT.contains(&n)
}

/// The inherited PATH keeps its order: it is what Chanto was started with,
/// an activated venv first. The login shell can't decide that order: on macOS
/// its `path_helper` puts the system directories first and moves the
/// inherited entries after them, so a venv's `python3` would lose to
/// `/usr/local/bin`'s. An entry only the login shell has (a PATH the startup
/// files extend, when Chanto was launched without them) goes right after the
/// inherited entry it follows there, or, before any, ahead of the first one
/// it precedes. No repeats.
fn merge_path(inherited: Option<&str>, captured: &str) -> String {
    let inherited: Vec<&str> = inherited
        .unwrap_or("")
        .split(':')
        .filter(|s| !s.is_empty())
        .collect();
    let mut leading: Vec<&str> = Vec::new();
    let mut first: Option<&str> = None;
    let mut after: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut last: Option<&str> = None;
    for e in captured.split(':').filter(|s| !s.is_empty()) {
        if inherited.contains(&e) {
            first = first.or(Some(e));
            last = Some(e);
        } else if let Some(anchor) = last {
            match after.iter_mut().find(|(a, _)| *a == anchor) {
                Some((_, extra)) => extra.push(e),
                None => after.push((anchor, vec![e])),
            }
        } else {
            leading.push(e);
        }
    }
    let mut order: Vec<&str> = Vec::new();
    for e in &inherited {
        if first == Some(*e) {
            order.append(&mut leading);
        }
        order.push(e);
        if let Some((_, extra)) = after.iter().find(|(a, _)| a == e) {
            order.extend(extra);
        }
    }
    // Nothing in common (or no inherited PATH): the captured one goes after.
    order.append(&mut leading);
    let mut out: Vec<&str> = Vec::new();
    for e in order {
        if !out.contains(&e) {
            out.push(e);
        }
    }
    out.join(":")
}

/// The snapshot: bash code that recreates what the startup files set up,
/// and the definitions of their functions. Variables already as `inherited`
/// has them, and `pinned` ones (the config's `env`, which Chanto set itself),
/// are left alone.
///
/// The snapshot is sourced before every command, and defining every function
/// each time cost more than the command itself (~20 ms for nvm's alone). So
/// it defines a one-line stub per function instead; the first call to any of
/// them sources `functions` (written to `functions_path`), which replaces
/// all the stubs with the real definitions and runs the call.
pub fn render(
    c: &Captured,
    inherited: &dyn Fn(&str) -> Option<String>,
    secrets: &crate::secrets::SecretsConfig,
    pinned: &dyn Fn(&str) -> bool,
    functions_path: &Path,
) -> Rendered {
    let mut out = String::from(
        "# chanto shell snapshot: your shell's setup, sourced before each Bash command\n",
    );
    for (name, value) in &c.vars {
        if !valid_var_name(name)
            || skip_var(name)
            || pinned(name)
            || crate::secrets::withhold_env(name, secrets)
        {
            continue;
        }
        let old = inherited(name);
        let value = if name == "PATH" {
            merge_path(old.as_deref(), value)
        } else {
            value.clone()
        };
        if old.as_deref() == Some(value.as_str())
            || crate::secrets::contains_secret(&value)
            || crate::secrets::contains_secret(&format!("{name}={value}"))
        {
            continue;
        }
        out.push_str(&format!("export {name}={}\n", sq(&value)));
    }
    // Functions before aliases, so no alias rewrites a function as it's
    // defined (the loader turns alias expansion off while it defines them).
    let mut lazy: Vec<&str> = Vec::new();
    let mut defs = String::new();
    for (name, def) in &c.functions {
        if skip_function(name)
            || def.len() > MAX_FUNCTION
            || def.contains("builtin autoload -X")
            || crate::secrets::contains_secret(def)
        {
            continue;
        }
        let line = format!("eval {} 2>/dev/null\n", sq(def));
        if out.len() + defs.len() + line.len() > MAX_TOTAL {
            break;
        }
        if stub_name(name) {
            lazy.push(name);
            defs.push_str(&line);
        } else {
            out.push_str(&line);
        }
    }
    let mut functions = String::new();
    if !lazy.is_empty() {
        let loader = sq(&functions_path.display().to_string());
        for name in &lazy {
            out.push_str(&format!(
                "{name} () {{ . {loader} 2>/dev/null; {name} \"$@\"; }}\n"
            ));
        }
        functions.push_str(&format!("unset -f {} 2>/dev/null\n", lazy.join(" ")));
        functions.push_str("__chanto_aliases=$(shopt -p expand_aliases 2>/dev/null)\n");
        functions.push_str("shopt -u expand_aliases 2>/dev/null\n");
        functions.push_str(&defs);
        functions.push_str("eval \"$__chanto_aliases\" 2>/dev/null; unset __chanto_aliases\n");
    }
    let mut aliases = String::new();
    for (name, value) in &c.aliases {
        if !skip_alias(name) && !crate::secrets::contains_secret(value) {
            aliases.push_str(&format!("alias {}={} 2>/dev/null\n", sq(name), sq(value)));
        }
    }
    for (name, def) in &c.alias_defs {
        if !skip_alias(name) && def.starts_with("alias ") && !crate::secrets::contains_secret(def) {
            aliases.push_str(&format!("eval {} 2>/dev/null\n", sq(def)));
        }
    }
    if !aliases.is_empty() && out.len() + aliases.len() <= MAX_TOTAL {
        out.push_str("shopt -s expand_aliases 2>/dev/null\n");
        out.push_str(&aliases);
    }
    Rendered {
        snapshot: out,
        functions,
    }
}

/// The two files of a snapshot: what's sourced before each command, and the
/// function definitions its stubs load on first use (empty without any).
pub struct Rendered {
    pub snapshot: String,
    pub functions: String,
}

/// A function name that can be written as a plain stub definition: anything
/// else is defined in the snapshot itself, as before.
fn stub_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '.'))
}

/// A new directory only this user can enter, for one snapshot. Also clears
/// out snapshots older than two days that earlier sessions left behind.
///
/// It lives under Chanto's home, not the temp dir: the snapshot is sourced
/// unsandboxed (every `!cmd` and every approved `dangerouslyDisableSandbox`
/// command), so a sandboxed command appending a payload to it would run that
/// payload the next time. The temp dir is sandbox-writable; Chanto's home is
/// on the sandbox's write-deny list (`tools::bash::guards`), so a sandboxed
/// command can't reach the file here.
fn private_dir() -> std::io::Result<PathBuf> {
    let base = crate::config::home_dir().join("shell-snapshots");
    std::fs::create_dir_all(&base)?;
    if let Ok(rd) = std::fs::read_dir(&base) {
        let now = std::time::SystemTime::now();
        let old = now - Duration::from_secs(2 * 86400);
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if !m.is_dir() || !owned_by_me(&m) {
                continue;
            }
            if m.modified().is_ok_and(|t| t < old) {
                let _ = std::fs::remove_dir_all(e.path());
                continue;
            }
            // A raw capture a crashed or older Chanto left behind; one still
            // being written is younger than any capture's timeout.
            let capture = e.path().join("capture");
            if std::fs::metadata(&capture)
                .and_then(|m| m.modified())
                .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age > 10 * TIMEOUT))
            {
                let _ = std::fs::remove_file(capture);
            }
        }
    }
    let dir = base.join(uuid::Uuid::new_v4().simple().to_string());
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(&dir)?;
    Ok(dir)
}

#[cfg(unix)]
fn owned_by_me(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    m.uid() == unsafe { libc::getuid() }
}
#[cfg(not(unix))]
fn owned_by_me(_: &std::fs::Metadata) -> bool {
    true
}

fn write_private(p: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(p)?.write_all(bytes)
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn home() -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("chanto-snaphome-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(d.join("mybin")).unwrap();
        let tool = d.join("mybin/hx-only-tool");
        std::fs::write(&tool, "#!/bin/sh\necho tool-ran\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        d
    }

    pub(crate) const RC: &str = "echo noisy rc output\necho more noise >&2\nexport PATH=\"$HOME/mybin:$PATH\"\nexport HX_SNAP_VAR=from-rc\nexport GITHUB_TOKEN=ghp_shouldnotleak1234567890abcdefABCDEF\nalias hxgreet='echo hello-from-alias'\nhxfn() { echo \"fn-got-$1\"; }\n_hx_completion() { :; }\nfalse\n";

    fn run_with(snapshot: &Path, cmd: &str) -> String {
        let script = format!("{}{cmd}", source_line(snapshot));
        let o = std::process::Command::new("/bin/bash")
            .args(["-c", &script])
            .current_dir(std::env::temp_dir())
            .output()
            .unwrap();
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        )
    }

    fn check(shell: &str, rc: &str) {
        if !Path::new(shell).exists() {
            return;
        }
        // The snapshot dir is under Chanto's home, which other tests point
        // elsewhere (and delete) while holding this lock.
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let h = home();
        std::fs::write(h.join(rc), RC).unwrap();
        let p = create(
            Path::new(shell),
            Some(&h),
            Duration::from_secs(20),
            &Default::default(),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let functions = p.with_file_name("functions.sh");
        assert_eq!(
            std::fs::metadata(&functions).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let text =
            std::fs::read_to_string(&p).unwrap() + &std::fs::read_to_string(&functions).unwrap();
        assert!(
            !text.contains("ghp_"),
            "secrets stay out of the snapshot: {text}"
        );
        assert!(
            !text.contains("_hx_completion") && !text.contains("noisy"),
            "{text}"
        );
        // Nothing else stays beside the snapshot, the raw capture included.
        let mut left: Vec<_> = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["functions.sh", "snapshot.sh"]);
        let out = run_with(
            &p,
            "hx-only-tool && hxgreet && hxfn x && echo \"$HX_SNAP_VAR\" && echo \"tok=${GITHUB_TOKEN:-unset}\"",
        );
        assert!(
            out.contains("tool-ran"),
            "{shell}: PATH from the rc file: {out}\n{text}"
        );
        assert!(
            out.contains("hello-from-alias"),
            "{shell}: alias: {out}\n{text}"
        );
        assert!(out.contains("fn-got-x"), "{shell}: function: {out}\n{text}");
        assert!(out.contains("from-rc"), "{out}");
        assert!(!out.contains("noisy"), "{out}");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn snapshot_lives_under_chanto_home_not_temp() {
        // The sandbox can write the temp dir but not Chanto's home, so the
        // snapshot a later unsandboxed command sources must sit under home.
        // The snapshot dir is under Chanto's home, which other tests point
        // elsewhere (and delete) while holding this lock.
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = private_dir().unwrap();
        assert!(
            dir.starts_with(crate::config::home_dir()),
            "snapshot dir {dir:?} is not under CHANTO_HOME"
        );
        assert!(
            !dir.starts_with(std::env::temp_dir())
                || crate::config::home_dir().starts_with(std::env::temp_dir())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_capture_is_removed_and_a_fresh_one_kept() {
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let base = crate::config::home_dir().join("shell-snapshots");
        let (stale, fresh) = (
            base.join(format!("stale-{}", uuid::Uuid::new_v4().simple())),
            base.join(format!("fresh-{}", uuid::Uuid::new_v4().simple())),
        );
        for d in [&stale, &fresh] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("capture"), "E\0GITHUB_TOKEN\0ghp_x\0").unwrap();
            std::fs::write(d.join("snapshot.sh"), "").unwrap();
        }
        // Left by a crash an hour ago; the other is a capture under way.
        std::fs::File::options()
            .write(true)
            .open(stale.join("capture"))
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
        let dir = private_dir().unwrap();
        assert!(!stale.join("capture").exists(), "the stale capture stayed");
        assert!(stale.join("snapshot.sh").exists(), "only the capture goes");
        assert!(
            fresh.join("capture").exists(),
            "a capture under way was removed"
        );
        for d in [&stale, &fresh, &dir] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn functions_load_on_first_call_without_aliases_rewriting_them() {
        let dir = std::env::temp_dir().join(format!("chanto-lazy-fn-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = Captured {
            vars: vec![],
            aliases: vec![("ls".into(), "echo ALIASED".into())],
            alias_defs: vec![],
            functions: vec![
                ("listit".into(), "listit () { ls -d /; }".into()),
                ("twice".into(), "twice () { listit; listit; }".into()),
            ],
        };
        let functions = dir.join("functions.sh");
        let r = render(&c, &|_| None, &Default::default(), &|_| false, &functions);
        std::fs::write(&functions, &r.functions).unwrap();
        let snap = dir.join("snapshot.sh");
        std::fs::write(&snap, &r.snapshot).unwrap();
        // Defining the functions costs nothing until one is called; then the
        // real ones replace the stubs, and the alias defined before them
        // doesn't leak into their bodies.
        let out = std::process::Command::new("/bin/bash")
            .arg("-c")
            .arg(format!(
                // The last line is parsed after the snapshot ran, as a later
                // command would be: aliases still apply there.
                ". '{}'; twice; declare -f listit | grep -c 'functions.sh'\nls -d /",
                snap.display()
            ))
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&out.stdout);
        assert_eq!(out, "/\n/\n0\nALIASED -d /\n", "{out}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn zsh_rc_path_aliases_and_functions_reach_bash() {
        check("/bin/zsh", ".zshrc");
    }

    #[test]
    fn bash_rc_path_aliases_and_functions_reach_bash() {
        check("/bin/bash", ".bashrc");
    }

    #[test]
    fn a_slow_rc_file_times_out() {
        let h = home();
        std::fs::write(h.join(".bashrc"), "sleep 30\nexport HX_SNAP_VAR=late\n").unwrap();
        let t0 = Instant::now();
        let r = create(
            Path::new("/bin/bash"),
            Some(&h),
            Duration::from_millis(500),
            &Default::default(),
        );
        assert!(matches!(r, Err(Error::TimedOut)), "{r:?}");
        assert!(t0.elapsed() < Duration::from_secs(5));
        // An rc file that waits for input gets /dev/null and moves on.
        std::fs::write(
            h.join(".bashrc"),
            "read -r answer\nexport HX_SNAP_VAR=after-read\n",
        )
        .unwrap();
        let c = capture(Path::new("/bin/bash"), Some(&h), Duration::from_secs(20)).unwrap();
        assert!(
            c.vars
                .iter()
                .any(|(k, v)| k == "HX_SNAP_VAR" && v == "after-read")
        );
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn render_filters_and_merges_path() {
        let c = Captured {
            vars: vec![
                ("PATH".into(), "/opt/new/bin:/usr/bin:/bin".into()),
                ("SAME".into(), "x".into()),
                ("PINNED".into(), "from-rc".into()),
                ("NEW".into(), "it's".into()),
                ("PS1".into(), "$ ".into()),
                ("HTTPS_PROXY".into(), "http://x".into()),
                ("MY_API_KEY".into(), "abc".into()),
                ("bad-name".into(), "x".into()),
            ],
            aliases: vec![
                ("ll".into(), "ls -l".into()),
                ("cd".into(), "echo no".into()),
            ],
            alias_defs: vec![],
            functions: vec![
                ("_comp".into(), "_comp () { :; }".into()),
                ("prompt_x".into(), "prompt_x () { :; }".into()),
                ("ok".into(), "ok () { echo 'q'; }".into()),
            ],
        };
        let inherited = |k: &str| match k {
            "PATH" => Some("/venv/bin:/usr/bin:/bin".to_string()),
            "SAME" => Some("x".to_string()),
            _ => None,
        };
        let r = render(
            &c,
            &inherited,
            &Default::default(),
            &|k| k == "PINNED",
            Path::new("/snap/functions.sh"),
        );
        let s = format!("{}{}", r.snapshot, r.functions);
        assert!(
            s.contains("export PATH='/venv/bin:/opt/new/bin:/usr/bin:/bin'"),
            "{s}"
        );
        assert!(s.contains("export NEW='it'\\''s'"), "{s}");
        for bad in [
            "SAME",
            "PINNED",
            "PS1",
            "PROXY",
            "MY_API_KEY",
            "bad-name",
            "_comp",
            "prompt_x",
            "echo no",
        ] {
            assert!(!s.contains(bad), "{bad}: {s}");
        }
        assert!(r.snapshot.contains("alias 'll'='ls -l'"), "{s}");
        // A stub in the snapshot; the definition in the file it loads.
        assert!(
            r.snapshot
                .contains("ok () { . '/snap/functions.sh' 2>/dev/null; ok \"$@\"; }"),
            "{s}"
        );
        assert!(r.functions.starts_with("unset -f ok "), "{s}");
        assert!(
            r.functions.contains("eval 'ok () { echo '\\''q'\\''; }'"),
            "{s}"
        );
        assert_eq!(merge_path(None, "/a::/b:/a"), "/a:/b");
    }

    #[test]
    fn an_activated_venv_stays_first_after_path_helper() {
        // macOS's login shell runs path_helper: system directories first, the
        // inherited entries (the venv) moved after them.
        assert_eq!(
            merge_path(
                Some("/venv/bin:/usr/bin:/bin"),
                "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/venv/bin"
            ),
            "/venv/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin"
        );
    }

    #[test]
    fn startup_files_extend_a_bare_launch_path() {
        // Launched from a GUI with a bare PATH: what the startup files add
        // comes before the system directories, as they put it.
        assert_eq!(
            merge_path(
                Some("/usr/bin:/bin"),
                "/opt/homebrew/bin:/Users/u/.cargo/bin:/usr/bin:/bin"
            ),
            "/opt/homebrew/bin:/Users/u/.cargo/bin:/usr/bin:/bin"
        );
        assert_eq!(merge_path(Some("/usr/bin"), "/usr/bin:/x"), "/usr/bin:/x");
    }
}
