//! What the sandbox protects inside the trees it lets commands write.
//!
//! A sandboxed command may write the project, temp dirs and download caches,
//! but some paths in those trees hold code or settings that run later outside
//! the sandbox: git hooks and config (the next `git` the user or Chanto runs),
//! agent and editor settings (hooks, MCP servers, tasks), Chanto's own home
//! (config, the shell snapshot every unsandboxed `!cmd` sources) and the bin
//! and config trees of toolchains. The guards below keep those read-only.
//! They are steps applied in order after the writable grants; the last step
//! that matches a path decides.
//!
//! macOS enforces all of them. Landlock can only grant, never deny, and
//! carving a guarded path out of a tree takes the right to create entries in
//! every directory on the way to it, so Linux enforces only the home guards
//! ([`home_guards`]) and only in trees it can carve without breaking builds;
//! see `sandbox_linux::landlock_write_allow`.

use super::*;
use std::path::{Path, PathBuf};

/// One step of the write policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Guard {
    /// No writes at or below this path.
    Deny(PathBuf),
    /// Writable again below an earlier `Deny` (an isolated agent's worktree
    /// under `.chanto/`). Only takes effect inside a writable tree.
    Reopen(PathBuf),
    /// The entry itself can't be created, removed or renamed (the project's
    /// `.git`, `.claude` and `.vscode` directories); what is inside follows
    /// the other steps. Renaming the directory away and moving a prepared one
    /// into its place used to get around every deny that named a path inside.
    Pin(PathBuf),
    /// A regex for paths that may not exist yet (submodule or worktree git
    /// dirs, nested repositories' hooks, `.claude/settings*.json`).
    Pattern(String),
    /// A terminal device: no reading (keystrokes), writing (fake prompts) or
    /// ioctls (`stty`, input injection) at all.
    Device(PathBuf),
}

/// Home-relative paths that hold code or settings run outside the sandbox.
/// Most are outside the writable caches anyway; listing them still protects
/// them when the project *is* the home directory, or a configured
/// `allow_write` covers them. Dependency caches (extracted crates, Go
/// modules, Maven and Gradle jars, `npx` packages) are not listed: they are
/// writable like the project's own `node_modules` (see `UNIX_CACHES`).
const HOME_DENIES: &[&str] = &[
    // Shell and git startup files.
    ".zshrc",
    ".zshenv",
    ".zprofile",
    ".zlogin",
    ".zlogout",
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".bash_logout",
    ".profile",
    ".gitconfig",
    ".config/git",
    ".config/fish",
    ".config/systemd",
    ".config/autostart",
    ".ssh",
    "Library/LaunchAgents",
    // Agent settings with hooks.
    ".claude/settings.json",
    ".claude/settings.local.json",
    ".claude/hooks",
    ".codex",
    // Toolchains: binaries, settings, extracted sources build scripts run from.
    ".cargo/bin",
    ".cargo/config.toml",
    ".cargo/config",
    ".cargo/env",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".rustup",
    ".pyenv",
    ".local/share/uv",
    ".local/bin",
    ".nvm",
    ".volta",
    ".docker/cli-plugins",
    ".bun/bin",
    ".bun/install/global",
    ".gradle/init.d",
    ".gradle/init.gradle",
    ".gradle/init.gradle.kts",
    ".gradle/gradle.properties",
    ".gradle/wrapper",
    ".gradle/jdks",
    ".m2/settings.xml",
    ".m2/settings-security.xml",
    ".m2/extensions.xml",
    ".m2/wrapper",
    // Environments and browsers in the general caches that later run as is.
    ".cache/pre-commit",
    ".cache/pypoetry/virtualenvs",
    "Library/Caches/pypoetry/virtualenvs",
    ".cache/ms-playwright",
    "Library/Caches/ms-playwright",
    ".cache/puppeteer",
];

/// The write guards for a sandboxed command in project `root`, in order.
pub(crate) fn write_guards(root: &Path, home: &Path) -> Vec<Guard> {
    let mut out = Vec::new();
    // Chanto's home: config, hooks, tokens and the shell snapshot. A run's
    // worktree lives in there; a command running in one may write it.
    for c in chanto_homes(home) {
        if !root.starts_with(&c) {
            out.push(Guard::Deny(c));
        } else if root != c {
            out.push(Guard::Deny(c));
            out.push(Guard::Reopen(root.to_path_buf()));
        }
    }
    for rel in HOME_DENIES {
        out.push(Guard::Deny(home.join(rel)));
    }
    project_guards(root, &mut out);
    // Repositories nested anywhere in the project (vendored, cloned, test
    // fixtures): their hooks and config run on the next git command in them.
    out.push(Guard::Pattern(format!(
        "^{}/(.+/)?\\.git/(hooks(/.*)?|config)$",
        regex::escape(&norm(root))
    )));
    // Isolated agents' worktrees are writable, but not their own settings
    // and git internals.
    let worktrees = root.join(".chanto/worktrees");
    if let Ok(rd) = std::fs::read_dir(&worktrees) {
        for e in rd.flatten() {
            if e.path().is_dir() {
                project_guards(&e.path(), &mut out);
                out.push(Guard::Deny(e.path().join(".chanto")));
            }
        }
    }
    // The terminal Chanto runs in, and the user's other terminal windows: a
    // command must not read keystrokes from them, type into them or paint
    // fake prompts on them (setsid already takes the controlling terminal
    // away). Ptys the command opens itself (`script`, `expect`) come later
    // and stay usable.
    out.extend(terminals().into_iter().map(Guard::Device));
    out
}

/// Chanto's home wherever it is (`CHANTO_HOME`, XDG) and the default one.
fn chanto_homes(home: &Path) -> Vec<PathBuf> {
    let mut out = vec![crate::config::home_dir()];
    if !out.contains(&home.join(".chanto")) {
        out.push(home.join(".chanto"));
    }
    out
}

/// The guards outside any project: Chanto's home and [`HOME_DENIES`]. These
/// are what Linux enforces (when they fall in a writable tree at all).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn home_guards(home: &Path) -> Vec<PathBuf> {
    let mut out = chanto_homes(home);
    out.extend(HOME_DENIES.iter().map(|rel| home.join(rel)));
    out
}

/// The protected paths of one checkout at `dir`, matching what
/// `permissions::is_protected_path` keeps from the file tools.
fn project_guards(dir: &Path, out: &mut Vec<Guard>) {
    out.push(Guard::Deny(dir.join(".chanto")));
    out.push(Guard::Reopen(dir.join(".chanto/worktrees")));
    out.push(Guard::Reopen(dir.join(".chanto/outcome")));
    for rel in [
        ".codex",
        ".claude/settings.json",
        ".claude/settings.local.json",
        ".claude/hooks",
        ".husky",
        ".githooks",
        ".mcp.json",
        ".vscode/settings.json",
        ".vscode/tasks.json",
    ] {
        out.push(Guard::Deny(dir.join(rel)));
    }
    // Pinned whether or not they exist: renaming a prepared directory into
    // place writes only the directory's own entry, never the paths inside.
    for rel in [".claude", ".vscode"] {
        out.push(Guard::Pin(dir.join(rel)));
    }
    if let Ok(rd) = std::fs::read_dir(dir.join(".claude")) {
        out.extend(
            rd.flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("settings"))
                .map(|e| Guard::Deny(e.path())),
        );
    }
    out.push(Guard::Pattern(format!(
        "^{}/\\.claude/settings[^/]*$",
        regex::escape(&norm(dir))
    )));
    git_guards(dir, out);
}

/// The checkout's `.git` (a directory, or a worktree's `gitdir:` file) and
/// the git directories it leads to: hooks and config run code on the next
/// git command; `commondir` would point a worktree at another repository.
/// A missing `.git` is pinned too, so a command can't create one (with
/// hooks) for the user's next `git` to run.
fn git_guards(dir: &Path, out: &mut Vec<Guard>) {
    let dotgit = dir.join(".git");
    let gitdir = match dotgit.symlink_metadata() {
        Ok(md) if !md.is_dir() => {
            out.push(Guard::Deny(dotgit.clone()));
            match read_pointer(&dotgit, "gitdir:", dir) {
                Some(g) => g,
                None => return,
            }
        }
        _ => {
            out.push(Guard::Pin(dotgit.clone()));
            dotgit
        }
    };
    let mut dirs = vec![gitdir.clone()];
    let commondir = gitdir.join("commondir");
    if commondir.is_file() {
        out.push(Guard::Deny(commondir.clone()));
        if let Some(c) = read_pointer(&commondir, "", &gitdir) {
            dirs.push(c);
        }
    }
    for g in dirs {
        for rel in ["hooks", "config", "config.worktree", "commondir"] {
            out.push(Guard::Deny(g.join(rel)));
        }
        if let Some(h) = hooks_path(&g.join("config"), dir) {
            out.push(Guard::Deny(h));
        }
        // Submodules' and worktrees' own git dirs: the existing ones here,
        // new ones by pattern. The directories themselves are pinned, so one
        // can't be swapped for a prepared copy with its own hooks.
        for sub in ["modules", "worktrees"] {
            out.push(Guard::Pin(g.join(sub)));
            if let Ok(rd) = std::fs::read_dir(g.join(sub)) {
                for e in rd.flatten() {
                    for rel in ["hooks", "config", "config.worktree", "commondir"] {
                        out.push(Guard::Deny(e.path().join(rel)));
                    }
                }
            }
        }
        let g = regex::escape(&norm(&g));
        out.push(Guard::Pattern(format!(
            "^{g}/(modules/.+|worktrees/[^/]+)/(hooks(/.*)?|config|config\\.worktree|commondir)$"
        )));
        out.push(Guard::Pattern(format!(
            "^{g}/(modules/([^/]+/modules/)*[^/]+|worktrees/[^/]+)$"
        )));
    }
}

/// The path a git pointer file names (`gitdir: <path>` in a worktree's
/// `.git`, the bare path in `commondir`), relative to `base`. Read here, not
/// with `git`: a hostile repository's config must not run anything.
fn read_pointer(file: &Path, prefix: &str, base: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(file).ok()?;
    let line = text.lines().next()?.trim();
    let p = line.strip_prefix(prefix)?.trim();
    (!p.is_empty()).then(|| base.join(p))
}

/// `core.hooksPath` from a git config file, when it names a directory
/// (relative paths are relative to the checkout).
fn hooks_path(config: &Path, dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(config).ok()?;
    text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        if !k.trim().eq_ignore_ascii_case("hookspath") {
            return None;
        }
        let v = v.trim().trim_matches('"');
        let v = v
            .strip_prefix("~/")
            .and_then(|r| dirs::home_dir().map(|h| h.join(r)))
            .unwrap_or_else(|| PathBuf::from(v));
        (!v.as_os_str().is_empty()).then(|| dir.join(v))
    })
}

/// The terminals to keep commands away from: Chanto's own, and on macOS
/// every other terminal window's tty that exists when the command starts.
/// Only Seatbelt enforces these: Landlock has no device rules that wouldn't
/// also take away the ptys a command opens itself.
fn terminals() -> Vec<PathBuf> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let mut out = own_terminals();
    if let Ok(rd) = std::fs::read_dir("/dev") {
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            let n = name.strip_prefix("ttys").unwrap_or("");
            if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && !out.contains(&e.path()) {
                out.push(e.path());
            }
        }
    }
    out
}

/// A path as Seatbelt compares it: symlinks resolved, also for the parts that
/// don't exist yet.
pub(super) fn norm(p: &Path) -> String {
    crate::permissions::normalize(p).display().to_string()
}

/// The paths the guards deny, in order (for tests).
#[cfg(test)]
pub(crate) fn write_deny_paths(cwd: &Path, home: &Path) -> Vec<PathBuf> {
    write_guards(cwd, home)
        .into_iter()
        .filter_map(|g| match g {
            Guard::Deny(p) => Some(p),
            _ => None,
        })
        .collect()
}

/// One Seatbelt rule: `(action op… (filter "arg"))`.
pub(crate) type Rule = (&'static str, &'static [&'static str], &'static str, String);

/// The Seatbelt rules for the guards, given the (resolved) writable trees.
pub(crate) fn guard_rules(guards: &[Guard], writable: &[String]) -> Vec<Rule> {
    const WRITE: &[&str] = &["file-write*"];
    guards
        .iter()
        .filter_map(|g| match g {
            Guard::Deny(p) => Some(("deny", WRITE, "subpath", norm(p))),
            Guard::Reopen(p) => {
                let p = norm(p);
                writable
                    .iter()
                    .any(|w| Path::new(&p).starts_with(w))
                    .then_some(("allow", WRITE, "subpath", p))
            }
            Guard::Pin(p) => Some(("deny", WRITE, "literal", norm(p))),
            Guard::Pattern(re) => Some(("deny", WRITE, "regex", re.clone())),
            Guard::Device(p) => Some((
                "deny",
                &["file-read*", "file-write*", "file-ioctl"][..],
                "literal",
                norm(p),
            )),
        })
        .collect()
}

/// Seatbelt rules for the guards, given the (resolved) writable trees.
pub(super) fn seatbelt_guards(guards: &[Guard], writable: &[String]) -> String {
    let mut prof = String::new();
    for (action, ops, filter, arg) in guard_rules(guards, writable) {
        prof.push_str(&format!(
            "({action} {} ({filter} \"{}\"))\n",
            ops.join(" "),
            sbpl_quote(&arg)
        ));
    }
    prof
}

#[cfg(test)]
mod guard_tests {
    use super::*;

    #[test]
    fn deny_paths_cover_persistence_and_toolchains() {
        let cwd = std::path::Path::new("/tmp/proj");
        let home = std::path::Path::new("/home/u");
        let denied = write_deny_paths(cwd, home);
        let has = |p: &str| denied.iter().any(|d| d == std::path::Path::new(p));
        for p in [
            "/tmp/proj/.chanto",
            "/tmp/proj/.claude/settings.json",
            "/tmp/proj/.mcp.json",
            "/tmp/proj/.husky",
            "/tmp/proj/.githooks",
            "/tmp/proj/.vscode/tasks.json",
            "/home/u/.cargo/bin",
            "/home/u/.rustup",
            "/home/u/.pyenv",
            "/home/u/.gradle/init.d",
        ] {
            assert!(has(p), "missing deny for {p}");
        }
    }

    #[test]
    fn git_directory_is_pinned() {
        let dir = std::env::temp_dir().join(format!("chanto-gitguard-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join(".git/hooks")).unwrap();
        let guards = write_guards(&dir, std::path::Path::new("/home/u"));
        assert!(guards.contains(&Guard::Pin(dir.join(".git"))), "{guards:?}");
        assert!(
            guards
                .iter()
                .any(|g| matches!(g, Guard::Deny(p) if p == &dir.join(".git/config")))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A worktree's `.git` is a file (`gitdir: …`); its target dir's hooks and
    /// config are protected too, and the `.git` file itself is denied.
    #[test]
    fn worktree_gitfile_and_its_gitdir_are_protected() {
        let base = std::env::temp_dir().join(format!("chanto-wtguard-{}", uuid::Uuid::new_v4()));
        let wt = base.join("wt");
        let gd = base.join("realgit");
        std::fs::create_dir_all(gd.join("hooks")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gd.display())).unwrap();
        let guards = write_guards(&wt, std::path::Path::new("/home/u"));
        assert!(
            guards
                .iter()
                .any(|g| matches!(g, Guard::Deny(p) if p == &wt.join(".git")))
        );
        assert!(
            guards
                .iter()
                .any(|g| matches!(g, Guard::Deny(p) if p == &gd.join("hooks")))
        );
        assert!(
            guards
                .iter()
                .any(|g| matches!(g, Guard::Deny(p) if p == &gd.join("config")))
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
