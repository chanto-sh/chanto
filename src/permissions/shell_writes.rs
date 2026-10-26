//! Shell commands that write the files [`is_protected_path`] guards for the
//! file tools: git internals, agent settings and hooks, MCP configs. A
//! `tee .chanto/config.toml` or `cp x .git/hooks/pre-commit` plants code that
//! runs, unsandboxed, the next time git or the agent starts, so it needs a
//! human as an Edit of the same file does.
//!
//! Recognised: redirections and the destinations of the usual file commands,
//! followed through `cd`/`pushd`, wrappers (`env -C`, `sudo`, `timeout`),
//! `sh -c` scripts, command substitutions and brace expansion. A target the
//! shell computes when it runs (`$var`, `$(…)`, `$'…'`) can't be judged, so
//! it counts as protected, as does the source of a link. Archives and
//! patches write the paths their input names, which the command doesn't
//! show: [`Scan::unseen`] reports them. What a script or program writes is
//! left to the sandbox.

use super::*;
use std::path::PathBuf;

/// What a command writes, as far as its text shows.
#[derive(Debug, Default)]
pub(super) struct Scan {
    /// The first protected path it writes, as written.
    pub protected: Option<String>,
    /// It extracts an archive or applies a patch, whose input names the
    /// files written.
    pub unseen: bool,
}

/// The first protected path `cmd` recognisably writes, run from `cwd`.
#[cfg(test)]
pub(super) fn protected_write(cmd: &str, cwd: &Path, posix: bool) -> Option<String> {
    scan(cmd, cwd, posix).protected
}

/// What `cmd`, run from `cwd`, writes. Outside bash (`posix` false) `\` is
/// the path separator, which the bash reading takes for an escape, so the
/// command is also judged with each `\` read as `/`.
pub(super) fn scan(cmd: &str, cwd: &Path, posix: bool) -> Scan {
    let mut out = Scan::default();
    scan_into(cmd, &mut State::at(cwd), 0, &mut out);
    if !posix && out.protected.is_none() {
        scan_into(&cmd.replace('\\', "/"), &mut State::at(cwd), 0, &mut out);
    }
    out
}

/// The working directory as the command moves it. Shared with
/// substitutions: a `cd` in one is assumed to carry on, which can only make
/// more targets protected.
struct State {
    dir: PathBuf,
    stack: Vec<PathBuf>,
}

impl State {
    fn at(dir: &Path) -> State {
        State {
            dir: dir.to_path_buf(),
            stack: Vec::new(),
        }
    }
}

/// Programs whose operands (or some of them) are files they write.
const WRITERS: &[&str] = &[
    "tee", "touch", "truncate", "chmod", "chown", "chgrp", "mkdir", "shred", "cp", "mv", "ln",
    "install", "rsync", "sed", "perl", "ruby", "dd", "curl", "wget", "unzip", "tar",
];

fn scan_into(cmd: &str, st: &mut State, depth: usize, out: &mut Scan) {
    if depth > 4 {
        return;
    }
    for part in split_shell(cmd) {
        if out.protected.is_some() {
            return;
        }
        let (words, mut targets, subs) = tokens(&part);
        for s in subs {
            scan_into(&s, st, depth + 1, out);
        }
        let mut words = words.as_slice();
        // `{ tee …; }`, `if …; then tee …`: the grouping and the keywords
        // aren't the command.
        while let Some(w) = words.first()
            && matches!(
                w.as_str(),
                "{" | "}" | "!" | "if" | "then" | "else" | "elif" | "do" | "while" | "until"
            )
        {
            words = &words[1..];
        }
        while words.first().is_some_and(|w| is_assignment(w)) {
            words = &words[1..];
        }
        // Where this command runs: `env -C` and `sudo -D` move only it.
        let mut at = st.dir.clone();
        let mut from_input = false;
        while let Some(w) = words.first() {
            let prog = base(w);
            let (valued, long): (&str, &[&str]) = match prog {
                "env" => ("uCS", &["unset", "chdir", "split-string"]),
                "sudo" | "doas" => (
                    "ugChDprtTUR",
                    &[
                        "user",
                        "group",
                        "close-from",
                        "chdir",
                        "host",
                        "prompt",
                        "role",
                        "type",
                        "command-timeout",
                        "other-user",
                        "chroot",
                    ],
                ),
                "timeout" => ("sk", &["signal", "kill-after"]),
                "nice" => ("n", &["adjustment"]),
                "stdbuf" => ("ioe", &["input", "output", "error"]),
                "time" => ("fo", &["format", "output"]),
                "exec" => ("a", &[]),
                "nohup" | "command" | "builtin" => ("", &[]),
                "xargs" => (
                    "aEdILnPs",
                    &[
                        "arg-file",
                        "delimiter",
                        "eof",
                        "replace",
                        "max-lines",
                        "max-args",
                        "max-procs",
                        "max-chars",
                        "process-slot-var",
                    ],
                ),
                _ => break,
            };
            let (o, n) = parse(&words[1..], valued, long, true);
            for (name, v) in &o.opts {
                match (prog, name.as_str()) {
                    ("env", "C" | "chdir") | ("sudo", "D" | "chdir") => {
                        if v.contains(OPAQUE) {
                            out.protected = Some(plain(v));
                            return;
                        }
                        at = resolve_in(&at, v);
                    }
                    ("env", "S" | "split-string") => {
                        scan_into(v, &mut State::at(&at), depth + 1, out);
                    }
                    ("time", "o" | "output") => targets.push(v.clone()),
                    _ => {}
                }
            }
            words = &words[1 + n..];
            if prog == "env" {
                while words.first().is_some_and(|w| is_assignment(w)) {
                    words = &words[1..];
                }
            }
            if prog == "timeout" && !words.is_empty() {
                // The duration.
                words = &words[1..];
            }
            from_input |= prog == "xargs";
        }
        let Some(first) = words.first() else {
            if let Some(hit) = first_protected(&at, &targets) {
                out.protected = Some(hit);
            }
            continue;
        };
        let args = &words[1..];
        if first.contains(OPAQUE) {
            // A program the shell computes could be any of the writers: each
            // operand may be a target.
            targets.extend(args.iter().cloned());
        }
        let prog = base(first);
        match prog {
            "cd" | "pushd" => {
                let d = args
                    .iter()
                    .find(|w| !w.starts_with('-') || w.as_str() == "-");
                if let Some(d) = d
                    && d.contains(OPAQUE)
                {
                    out.protected = Some(plain(d));
                    return;
                }
                let back = st.stack.last().cloned();
                if prog == "pushd" {
                    st.stack.push(st.dir.clone());
                }
                st.dir = match d.map(String::as_str) {
                    // Back somewhere this command may not show: the last
                    // directory it left, or where it started.
                    Some("-") => back.unwrap_or_else(|| at.clone()),
                    Some(d) => resolve_in(&at, d),
                    // `cd` alone goes home.
                    None => dirs::home_dir().unwrap_or(at.clone()),
                };
                continue;
            }
            "popd" => {
                if let Some(d) = st.stack.pop() {
                    st.dir = d;
                }
                continue;
            }
            "sh" | "bash" | "zsh" | "dash" | "ksh" => {
                let (o, _) = parse(args, "oO", &[], false);
                if o.has(&["c"])
                    && let Some(script) = o.operands.first()
                {
                    scan_into(script, &mut State::at(&at), depth + 1, out);
                }
            }
            "eval" => scan_into(&args.join(" "), &mut State::at(&at), depth + 1, out),
            "tee" | "touch" | "truncate" | "chmod" | "chown" | "chgrp" | "mkdir" | "shred" => {
                let valued = match prog {
                    "touch" => "dtr",
                    "truncate" => "sr",
                    "mkdir" => "m",
                    "shred" => "nsu",
                    _ => "",
                };
                targets.extend(
                    parse(args, valued, &["reference", "mode", "size"], false)
                        .0
                        .operands,
                );
            }
            "cp" | "mv" | "ln" | "install" | "rsync" => copy_targets(prog, args, &mut targets),
            "sed" | "perl" | "ruby" => {
                let valued = if prog == "sed" { "ef" } else { "eEIlMmrx0" };
                let (o, _) = parse(args, valued, &["expression", "file"], false);
                if o.has(&["i", "in-place"]) {
                    targets.extend(o.operands);
                }
            }
            "dd" => targets.extend(
                args.iter()
                    .filter_map(|w| w.strip_prefix("of="))
                    .map(str::to_string),
            ),
            "curl" => {
                const WRITES: &[&str] = &[
                    "output",
                    "output-dir",
                    "dump-header",
                    "cookie-jar",
                    "trace",
                    "trace-ascii",
                    "stderr",
                    "libcurl",
                    "etag-save",
                    "hsts",
                    "alt-svc",
                ];
                let long: Vec<&str> = WRITES
                    .iter()
                    .copied()
                    .chain([
                        "header",
                        "data",
                        "user",
                        "request",
                        "user-agent",
                        "cookie",
                        "config",
                        "form",
                        "referer",
                        "proxy",
                        "write-out",
                        "upload-file",
                    ])
                    .collect();
                let (o, _) = parse(args, "AbcCDdEeFHKmoPQrtTuUwxXyYz", &long, false);
                targets.extend(o.values(&["o", "D", "c"]));
                targets.extend(o.values(WRITES));
            }
            "wget" => {
                const WRITES: &[&str] = &[
                    "o",
                    "a",
                    "O",
                    "P",
                    "output-file",
                    "append-output",
                    "output-document",
                    "directory-prefix",
                ];
                let (o, _) = parse(
                    args,
                    "oaOPeiBtTwQUlARDIX",
                    &[
                        "output-file",
                        "append-output",
                        "output-document",
                        "directory-prefix",
                        "execute",
                        "input-file",
                        "user-agent",
                        "header",
                    ],
                    false,
                );
                targets.extend(o.values(WRITES));
            }
            "unzip" => {
                let (o, _) = parse(args, "dx", &[], false);
                targets.extend(o.values(&["d"]));
                out.unseen |= !o.has(&["l", "t", "v", "Z", "p", "c", "z"]);
            }
            "tar" | "bsdtar" | "gtar" => {
                // Old style: `tar xzf a.tgz` is `tar -xzf a.tgz`.
                let mut a = args.to_vec();
                if let Some(f) = a.first_mut()
                    && !f.starts_with('-')
                    && !f.is_empty()
                    && f.chars().all(|c| c.is_ascii_alphabetic())
                {
                    f.insert(0, '-');
                }
                let (o, _) = parse(
                    &a,
                    "bCfFgHIKLNTVX",
                    &[
                        "directory",
                        "file",
                        "files-from",
                        "exclude-from",
                        "use-compress-program",
                        "to-command",
                        "checkpoint-action",
                        "info-script",
                        "new-volume-script",
                        "rsh-command",
                        "rmt-command",
                        "format",
                        "owner",
                        "group",
                        "mode",
                        "mtime",
                        "newer",
                        "transform",
                        "xform",
                        "label",
                        "exclude",
                        "suffix",
                    ],
                    false,
                );
                targets.extend(o.values(&["C", "directory"]));
                if o.has(&["c", "r", "u", "A", "create", "append", "update", "catenate"]) {
                    targets.extend(o.values(&["f", "file"]));
                }
                out.unseen |= o.has(&["x", "extract", "get"]);
            }
            "patch" => {
                let (o, _) = parse(
                    args,
                    "BDdFgiopVrYz",
                    &["input", "output", "directory"],
                    false,
                );
                out.unseen |= !o.has(&["dry-run"]);
            }
            "go" => {
                if matches!(
                    args.first().map(String::as_str),
                    Some("build" | "test" | "install")
                ) {
                    targets.extend(go_outputs(&args[1..]));
                }
            }
            "git" => {
                let (sub, dirs, git_dir) = git_globals(args);
                for d in &dirs {
                    if d.contains(OPAQUE) {
                        out.protected = Some(plain(d));
                        return;
                    }
                    at = resolve_in(&at, d);
                }
                let rest = &args[sub..];
                match rest.first().map(String::as_str) {
                    Some("apply") => {
                        let (o, _) = parse(
                            &rest[1..],
                            "pC",
                            &["directory", "exclude", "include"],
                            false,
                        );
                        let report = o.has(&["check", "stat", "numstat", "summary"]);
                        out.unseen |= !report || o.has(&["apply"]);
                    }
                    Some("am") => out.unseen = true,
                    _ => {
                        if let Some(file) = git_config_write(rest) {
                            targets.push(file.unwrap_or_else(|| {
                                format!("{}/config", git_dir.as_deref().unwrap_or(".git"))
                            }));
                        }
                    }
                }
            }
            _ => {}
        }
        if from_input && WRITERS.contains(&prog) {
            // `… | xargs tee`: the input names more targets.
            targets.push(OPAQUE.to_string());
        }
        if let Some(hit) = first_protected(&at, &targets) {
            out.protected = Some(hit);
            return;
        }
    }
}

/// The files `cp`, `mv`, `ln`, `install` or `rsync` writes: the destination,
/// each source's name under it, and the sources of a link, which writes
/// through to them later (`ln -s .chanto l && tee l/config.toml`).
fn copy_targets(prog: &str, args: &[String], targets: &mut Vec<String>) {
    let (valued, long): (&str, &[&str]) = match prog {
        "install" => (
            "gmoSt",
            &[
                "group",
                "mode",
                "owner",
                "suffix",
                "target-directory",
                "strip-program",
            ],
        ),
        "rsync" => (
            "efBTM",
            &[
                "rsh",
                "rsync-path",
                "filter",
                "exclude",
                "include",
                "exclude-from",
                "include-from",
                "files-from",
                "temp-dir",
                "partial-dir",
                "backup-dir",
                "suffix",
                "compare-dest",
                "copy-dest",
                "link-dest",
                "log-file",
                "password-file",
                "chmod",
                "chown",
                "usermap",
                "groupmap",
                "out-format",
                "timeout",
                "bwlimit",
                "max-size",
                "min-size",
                "port",
                "address",
                "remote-option",
            ],
        ),
        _ => ("St", &["suffix", "target-directory"]),
    };
    let (o, _) = parse(args, valued, long, false);
    // rsync's `-t` keeps times; the others' names a directory.
    let named = if prog == "rsync" {
        None
    } else {
        o.value(&["t", "target-directory"])
    };
    let ops = &o.operands;
    if prog == "install" && o.has(&["d", "directory"]) {
        targets.extend(ops.iter().cloned());
    }
    let (dest, sources) = match named {
        Some(d) => (Some(d), &ops[..]),
        None if ops.len() >= 2 => (ops.last().cloned(), &ops[..ops.len() - 1]),
        None => (None, &ops[..0]),
    };
    if prog == "ln" || (prog == "cp" && o.has(&["s", "l", "symbolic-link", "link"])) {
        targets.extend(sources.iter().cloned());
    }
    if let Some(dest) = dest {
        // Into a directory, each source lands under its own name.
        for s in sources {
            let name = s.trim_end_matches('/').rsplit('/').next().unwrap_or(s);
            if !name.is_empty() && name != "." && name != ".." {
                targets.push(format!("{}/{name}", dest.trim_end_matches('/')));
            }
        }
        targets.push(dest);
    }
}

/// `go build`/`go test`/`go install` outputs: `-o PATH` (one dash or two,
/// spaced or with `=`).
fn go_outputs(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(w) = it.next() {
        if !w.starts_with('-') {
            continue;
        }
        let name = w.trim_start_matches('-');
        if name == "o" {
            out.extend(it.next().cloned());
        } else if let Some(v) = name.strip_prefix("o=") {
            out.push(v.to_string());
        }
    }
    out
}

/// `git`'s own options before the subcommand: where the subcommand starts,
/// the `-C` directories in order, and a `--git-dir`.
fn git_globals(args: &[String]) -> (usize, Vec<String>, Option<String>) {
    let (mut dirs, mut git_dir) = (Vec::new(), None);
    let mut i = 0;
    while let Some(w) = args.get(i) {
        match w.as_str() {
            "-C" => {
                dirs.extend(args.get(i + 1).cloned());
                i += 2;
            }
            "-c" | "--config-env" | "--namespace" | "--super-prefix" | "--work-tree" => i += 2,
            "--git-dir" => {
                git_dir = args.get(i + 1).cloned();
                i += 2;
            }
            w if w.starts_with("--git-dir=") => {
                git_dir = Some(w["--git-dir=".len()..].to_string());
                i += 1;
            }
            w if w.starts_with('-') => i += 1,
            _ => break,
        }
    }
    (i.min(args.len()), dirs, git_dir)
}

/// For `git config` that sets something, the file it writes: `Some(None)`
/// for the repository's own config, `Some(Some(path))` for a `--file`. Also
/// `git remote` that adds or changes a remote (the repository's config).
fn git_config_write(args: &[String]) -> Option<Option<String>> {
    let sub = args.first()?;
    let rest = &args[1..];
    match sub.as_str() {
        "config" => {
            let (o, _) = parse(
                rest,
                "f",
                &["file", "blob", "type", "default", "comment", "value"],
                false,
            );
            // Another user's or the machine's file isn't the repository's.
            if o.has(&["global", "system", "blob"]) {
                return None;
            }
            let ops = &o.operands;
            let verb = ops.first().map(String::as_str);
            let section_ops = o.has(&["rename-section", "remove-section"])
                || matches!(verb, Some("rename-section" | "remove-section"));
            let edit = o.has(&["edit", "e"]) || verb == Some("edit");
            let writes = section_ops
                || edit
                || o.has(&["add", "replace-all", "unset", "unset-all"])
                || matches!(verb, Some("set" | "unset"));
            let reads = o
                .opts
                .iter()
                .any(|(n, _)| n.starts_with("get") || n == "l" || n == "list")
                || matches!(verb, Some("get" | "list"));
            if !writes && (reads || ops.len() < 2) {
                return None;
            }
            // Who you are and how output looks run nothing.
            let key = if matches!(verb, Some("set" | "unset")) {
                ops.get(1)
            } else {
                ops.first()
            };
            if !section_ops
                && !edit
                && key.is_some_and(|k| {
                    let k = k.to_ascii_lowercase();
                    k.starts_with("user.") || k.starts_with("color.")
                })
            {
                return None;
            }
            Some(o.value(&["f", "file"]))
        }
        "remote" => rest
            .first()
            .is_some_and(|w| {
                matches!(
                    w.as_str(),
                    "add" | "set-url" | "rename" | "set-head" | "set-branches"
                )
            })
            .then_some(None),
        _ => None,
    }
}

/// One command's arguments read the getopt way.
#[derive(Debug, Default)]
struct Opts {
    /// Each option by name (a short letter, or a long name spelled out in
    /// full when it abbreviates a known one), with its value if it takes one.
    opts: Vec<(String, String)>,
    operands: Vec<String>,
}

impl Opts {
    fn has(&self, names: &[&str]) -> bool {
        self.opts.iter().any(|(n, _)| names.contains(&n.as_str()))
    }

    fn values(&self, names: &[&str]) -> Vec<String> {
        self.opts
            .iter()
            .filter(|(n, _)| names.contains(&n.as_str()))
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn value(&self, names: &[&str]) -> Option<String> {
        self.values(names).pop()
    }
}

/// Read `args` as options and operands: a short cluster (`-xzf a.tgz`) is
/// one option per letter, and a letter in `valued` takes the rest of the
/// cluster or the next word as its value; `--name=v`, or `--name v` for a
/// name in `long` (or an abbreviation of one), takes `v`. With
/// `stop_at_operand`, reading ends at the first operand (a wrapper's
/// command); the count of words read is returned.
fn parse(args: &[String], valued: &str, long: &[&str], stop_at_operand: bool) -> (Opts, usize) {
    let mut o = Opts::default();
    let mut i = 0;
    while i < args.len() {
        let w = &args[i];
        i += 1;
        if w == "--" {
            if !stop_at_operand {
                o.operands.extend(args[i..].iter().cloned());
                i = args.len();
            }
            break;
        }
        if let Some(l) = w.strip_prefix("--") {
            let (name, v) = match l.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (l, None),
            };
            let full = long
                .iter()
                .find(|f| **f == name || (name.len() >= 3 && f.starts_with(name)));
            let v = match (v, full) {
                (Some(v), _) => v,
                (None, Some(_)) => {
                    let v = args.get(i).cloned().unwrap_or_default();
                    i += 1;
                    v
                }
                (None, None) => String::new(),
            };
            o.opts.push((full.map_or(name, |f| *f).to_string(), v));
        } else if w.len() > 1 && w.starts_with('-') {
            let cs: Vec<char> = w[1..].chars().collect();
            for (k, c) in cs.iter().enumerate() {
                if valued.contains(*c) {
                    let rest: String = cs[k + 1..].iter().collect();
                    let v = if rest.is_empty() {
                        let v = args.get(i).cloned().unwrap_or_default();
                        i += 1;
                        v
                    } else {
                        rest
                    };
                    o.opts.push((c.to_string(), v));
                    break;
                }
                o.opts.push((c.to_string(), String::new()));
            }
        } else if stop_at_operand {
            return (o, i - 1);
        } else {
            o.operands.push(w.clone());
        }
    }
    (o, i.min(args.len()))
}

/// The first of `targets` (relative to `dir`) that is protected, as written.
/// One the shell computes when it runs could be anything, so it counts.
fn first_protected(dir: &Path, targets: &[String]) -> Option<String> {
    targets.iter().find_map(|t| {
        if t.contains(OPAQUE) {
            return Some(plain(t));
        }
        if t.is_empty()
            || t == "/dev/null"
            || t.starts_with("/dev/std")
            || t.starts_with("/dev/fd/")
        {
            return None;
        }
        expand(&resolve_in(dir, t))
            .iter()
            .any(|p| protected_target(p))
            .then(|| t.clone())
    })
}

/// A file a write would make protected, or a directory one would write into
/// protected files of (`cp -r hooks .git/`, `mv x .husky`).
fn protected_target(p: &Path) -> bool {
    is_protected_path(p)
        || ["x", "settings.json", "tasks.json", "hooks/x"]
            .iter()
            .any(|c| is_protected_path(&p.join(c)))
}

/// Names a glob in a write target could stand for, among the protected ones.
const PROTECTED_NAMES: &[&str] = &[
    ".git",
    ".chanto",
    ".claude",
    ".codex",
    ".husky",
    ".vscode",
    ".mcp.json",
    "hooks",
    "config",
    "config.toml",
    "settings.json",
    "settings.local.json",
    "tasks.json",
];

/// `p` with each globbed component replaced by the protected names it
/// matches (as bash matches: a pattern without a leading dot skips dot
/// names), so `.chant?/config.toml` is judged as `.chanto/config.toml`.
fn expand(p: &Path) -> Vec<PathBuf> {
    let mut out = vec![PathBuf::new()];
    for c in p.components() {
        let s = c.as_os_str().to_string_lossy();
        if !s.contains(['*', '?', '[']) {
            out.iter_mut().for_each(|o| o.push(c));
            continue;
        }
        let Ok(m) = globset::Glob::new(&s).map(|g| g.compile_matcher()) else {
            out.iter_mut().for_each(|o| o.push(c));
            continue;
        };
        let names: Vec<&str> = PROTECTED_NAMES
            .iter()
            .copied()
            .filter(|n| (s.starts_with('.') || !n.starts_with('.')) && m.is_match(n))
            .collect();
        let mut next = Vec::new();
        for o in &out {
            next.push(o.join(c));
            for n in &names {
                next.push(o.join(n));
            }
        }
        next.truncate(64);
        out = next;
    }
    out
}

/// Starts a word whose value the shell computes when it runs (`$var`,
/// `$(…)`, backticks, `$'…'`).
const OPAQUE: char = '\u{0}';
/// Unquoted brace-expansion characters, told apart from quoted ones.
const B_OPEN: char = '\u{E000}';
const B_COMMA: char = '\u{E001}';
const B_CLOSE: char = '\u{E002}';

/// `w` without the marks [`tokens`] adds.
fn plain(w: &str) -> String {
    w.chars().filter(|c| *c != OPAQUE).map(literal).collect()
}

fn literal(c: char) -> char {
    match c {
        B_OPEN => '{',
        B_COMMA => ',',
        B_CLOSE => '}',
        c => c,
    }
}

/// Split one simple command into its words, the files it redirects output
/// to, and the commands it substitutes (`$(…)`, backticks, `<(…)`, `>(…)`),
/// resolving quotes and brace expansion as the shell does. A word the shell
/// computes starts with [`OPAQUE`]. The command comes from [`split_shell`],
/// so escapes are already resolved (or made inert).
fn tokens(part: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
    let cs: Vec<char> = part.chars().collect();
    let (mut words, mut targets, mut subs) = (Vec::new(), Vec::new(), Vec::new());
    let mut i = 0;
    // The text up to the `close` that ends what opened just before `*i`
    // (nesting counted), leaving `*i` after it.
    let enclosed = |i: &mut usize, open: char, close: char| -> String {
        let mut depth = 0;
        let start = *i;
        while let Some(&c) = cs.get(*i) {
            *i += 1;
            if c == close {
                if depth == 0 {
                    return cs[start..*i - 1].iter().collect();
                }
                depth -= 1;
            } else if c == open {
                depth += 1;
            }
        }
        cs[start..].iter().collect()
    };
    // Read one word from `i`: quotes removed, up to an unquoted blank or operator.
    let word = |i: &mut usize, subs: &mut Vec<String>| -> (String, bool) {
        let (mut w, mut any, mut opaque) = (String::new(), false, false);
        let (mut sq, mut dq) = (false, false);
        while let Some(&c) = cs.get(*i) {
            let next = cs.get(*i + 1).copied();
            match c {
                '\'' if !dq => sq = !sq,
                '"' if !sq => dq = !dq,
                '$' if !sq && next == Some('$') => {
                    // `$$`, the shell's PID: never the start of `$'…'`.
                    w.push_str("$$");
                    opaque = true;
                    any = true;
                    *i += 2;
                    continue;
                }
                '$' if !sq && next == Some('(') => {
                    *i += 2;
                    subs.push(enclosed(i, '(', ')'));
                    opaque = true;
                    any = true;
                    continue;
                }
                '`' if !sq => {
                    *i += 1;
                    subs.push(enclosed(i, '`', '`'));
                    opaque = true;
                    any = true;
                    continue;
                }
                '$' if !sq && !dq && next == Some('\'') => {
                    // `$'…'`: escapes the checks can't follow.
                    *i += 2;
                    w.push_str(&enclosed(i, '\'', '\''));
                    opaque = true;
                    any = true;
                    continue;
                }
                '$' if !sq => {
                    opaque = true;
                    w.push(c);
                }
                '{' if !sq && !dq => w.push(B_OPEN),
                ',' if !sq && !dq => w.push(B_COMMA),
                '}' if !sq && !dq => w.push(B_CLOSE),
                c if !sq
                    && !dq
                    && (c.is_whitespace()
                        || matches!(c, '<' | '>' | '|' | ';' | '&' | '(' | ')')) =>
                {
                    break;
                }
                c => w.push(c),
            }
            any = true;
            *i += 1;
        }
        if opaque {
            w.insert(0, OPAQUE);
        }
        (w, any)
    };
    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() || c == '(' || c == ')' {
            i += 1;
            continue;
        }
        if (c == '<' || c == '>') && cs.get(i + 1) == Some(&'(') {
            // `<(…)`, `>(…)`: a command of its own.
            i += 2;
            subs.push(enclosed(&mut i, '(', ')'));
            continue;
        }
        if c == '>' || c == '<' {
            let output = c == '>' || cs.get(i + 1) == Some(&'>');
            i += 1;
            while matches!(cs.get(i), Some('>' | '|' | '<')) {
                i += 1;
            }
            if cs.get(i) == Some(&'&') {
                i += 1;
                // `>&2`, `2>&-`: a descriptor, not a file.
                if matches!(cs.get(i), Some(d) if d.is_ascii_digit() || *d == '-') {
                    while matches!(cs.get(i), Some(d) if d.is_ascii_digit() || *d == '-') {
                        i += 1;
                    }
                    continue;
                }
            }
            while cs.get(i).is_some_and(|c| c.is_whitespace()) {
                i += 1;
            }
            let (t, _) = word(&mut i, &mut subs);
            if output {
                targets.extend(braces(&t));
            }
            continue;
        }
        if c == '&' {
            // `&>file`: the `>` comes next.
            i += 1;
            continue;
        }
        let (w, any) = word(&mut i, &mut subs);
        // `2>`: the digits name a descriptor.
        let fd = cs.get(i) == Some(&'>') && !w.is_empty() && w.chars().all(|c| c.is_ascii_digit());
        if any && !fd {
            words.extend(braces(&w));
        }
        if !any {
            i += 1;
        }
    }
    (words, targets, subs)
}

/// Brace expansion, as bash does it before anything else: `a{b,c}` is the
/// two words `ab` and `ac`, `{1..3}` three. Only unquoted braces (the marks
/// [`tokens`] leaves) count; at most 64 words come out.
fn braces(w: &str) -> Vec<String> {
    let mut out = Vec::new();
    expand_braces(w, &mut out);
    out
}

fn expand_braces(w: &str, out: &mut Vec<String>) {
    if out.len() >= 64 {
        return;
    }
    let cs: Vec<char> = w.chars().collect();
    for (i, &c) in cs.iter().enumerate() {
        if c != B_OPEN {
            continue;
        }
        let (mut depth, mut commas, mut close) = (0, Vec::new(), None);
        for (j, &d) in cs.iter().enumerate().skip(i + 1) {
            match d {
                B_OPEN => depth += 1,
                B_CLOSE if depth == 0 => {
                    close = Some(j);
                    break;
                }
                B_CLOSE => depth -= 1,
                B_COMMA if depth == 0 => commas.push(j),
                _ => {}
            }
        }
        let Some(close) = close else {
            continue;
        };
        let alts: Vec<String> = if commas.is_empty() {
            match range(&cs[i + 1..close].iter().collect::<String>()) {
                Some(r) => r,
                None => continue,
            }
        } else {
            let mut bounds = vec![i];
            bounds.extend(&commas);
            bounds.push(close);
            bounds
                .windows(2)
                .map(|b| cs[b[0] + 1..b[1]].iter().collect())
                .collect()
        };
        let pre: String = cs[..i].iter().collect();
        let post: String = cs[close + 1..].iter().collect();
        for a in alts {
            expand_braces(&format!("{pre}{a}{post}"), out);
        }
        return;
    }
    out.push(w.chars().map(literal).collect());
}

/// `{a..e}` or `{1..10}` (with an optional step): the items, up to 64.
fn range(body: &str) -> Option<Vec<String>> {
    let parts: Vec<&str> = body.split("..").collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let step = match parts.get(2) {
        Some(s) => s.parse::<i64>().ok()?.unsigned_abs().max(1) as usize,
        None => 1,
    };
    if let (Ok(a), Ok(b)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
        let v: Vec<i64> = if a <= b {
            (a..=b).step_by(step).take(64).collect()
        } else {
            (b..=a).rev().step_by(step).take(64).collect()
        };
        return Some(v.iter().map(i64::to_string).collect());
    }
    let (mut a, mut b) = (parts[0].chars(), parts[1].chars());
    if let (Some(x), None, Some(y), None) = (a.next(), a.next(), b.next(), b.next()) {
        let (lo, hi) = ((x as u32).min(y as u32), (x as u32).max(y as u32));
        return Some(
            (lo..=hi)
                .step_by(step)
                .take(64)
                .filter_map(char::from_u32)
                .map(String::from)
                .collect(),
        );
    }
    None
}

fn is_assignment(w: &str) -> bool {
    w.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

fn base(w: &str) -> &str {
    w.rsplit('/').next().unwrap_or(w)
}

/// `t` as seen from `dir`, with a leading `~` as the home directory.
fn resolve_in(dir: &Path, t: &str) -> PathBuf {
    let t = match t.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            return dirs::home_dir()
                .unwrap_or_default()
                .join(rest.trim_start_matches('/'));
        }
        _ => t,
    };
    dir.join(t)
}
