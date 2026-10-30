//! Glob, Grep and Ls over an [`Fs`], for file systems other than the disk
//! (the browser's, tests'). The CLI searches the disk faster, in parallel or
//! with ripgrep; these walk the same files, in the same order, with the same
//! output.

use crate::fs::Fs;
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Keeps only the entries needed for the first `limit` output lines, in key
/// order, while counting every line: a broad search can match millions of
/// lines without holding them all.
pub struct Bounded<K: Ord> {
    limit: usize,
    pub entries: std::collections::BTreeMap<K, (usize, String)>,
    kept_lines: usize,
    pub total_lines: usize,
}

impl<K: Ord> Bounded<K> {
    pub fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            entries: Default::default(),
            kept_lines: 0,
            total_lines: 0,
        }
    }
    pub fn push(&mut self, key: K, text: String) {
        let n = text.lines().count().max(1);
        self.total_lines += n;
        if self.kept_lines >= self.limit {
            // Full: only worth keeping if it sorts before the last entry.
            match self.entries.last_key_value() {
                Some((last, _)) if key < *last => {}
                _ => return,
            }
        }
        self.kept_lines += n;
        self.entries.insert(key, (n, text));
        // Drop trailing entries no longer needed for the first `limit` lines.
        while let Some((_, (ln, _))) = self.entries.last_key_value() {
            if self.kept_lines - ln >= self.limit {
                let (_, (ln, _)) = self.entries.pop_last().unwrap();
                self.kept_lines -= ln;
            } else {
                break;
            }
        }
    }
    pub fn into_text(self) -> (String, usize) {
        let mut s = String::new();
        for (_, (_, t)) in self.entries {
            s.push_str(&t);
            if !t.ends_with('\n') {
                s.push('\n');
            }
        }
        (s, self.total_lines)
    }
}

/// One file or directory a walk reached.
pub struct Visit {
    pub path: PathBuf,
    pub is_dir: bool,
    /// Levels below the root: 1 for its own entries.
    pub depth: usize,
}

/// Visit everything under `root`, depth first and in name order: `.git`
/// skipped, `.gitignore` and `.ignore` honoured (each from its own directory
/// down, and those of the root's parents), at most `max_depth` levels down
/// and `budget` entries in all. A file `keep` accepts is visited even when
/// an ignore file excludes it, as ripgrep's `--glob` overrides them. `f`
/// returns false to stop.
pub fn walk(
    fs: &dyn Fs,
    root: &Path,
    max_depth: Option<usize>,
    budget: usize,
    keep: &dyn Fn(&Path) -> bool,
    f: &mut dyn FnMut(&Visit) -> bool,
) {
    let ignores = root
        .ancestors()
        .skip(1)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .filter_map(|d| ignores_in(fs, d))
        .collect();
    let mut w = Walker {
        fs,
        max_depth,
        keep,
        ignores,
        left: budget,
    };
    w.dir(root, 1, f);
}

/// What one walk carries from directory to directory.
struct Walker<'a> {
    fs: &'a dyn Fs,
    max_depth: Option<usize>,
    keep: &'a dyn Fn(&Path) -> bool,
    /// The ignore files in force, outermost first.
    ignores: Vec<Gitignore>,
    /// Entries still to visit.
    left: usize,
}

impl Walker<'_> {
    /// Visit `dir`'s entries; false once the walk should stop.
    fn dir(&mut self, dir: &Path, depth: usize, f: &mut dyn FnMut(&Visit) -> bool) -> bool {
        if self.max_depth.is_some_and(|m| depth > m) {
            return true;
        }
        let Ok(mut entries) = self.fs.read_dir(dir) else {
            return true;
        };
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let own = ignores_in(self.fs, dir);
        let pushed = own.is_some();
        self.ignores.extend(own);
        let mut go_on = true;
        for e in entries {
            if e.name == ".git" {
                continue;
            }
            let path = dir.join(&e.name);
            if ignored(&self.ignores, &path, e.is_dir) && (e.is_dir || !(self.keep)(&path)) {
                continue;
            }
            if self.left == 0 {
                go_on = false;
                break;
            }
            self.left -= 1;
            let visit = Visit {
                path,
                is_dir: e.is_dir,
                depth,
            };
            if !f(&visit) || (e.is_dir && !self.dir(&visit.path, depth + 1, f)) {
                go_on = false;
                break;
            }
        }
        if pushed {
            self.ignores.pop();
        }
        go_on
    }
}

/// The rules in `dir`'s `.gitignore` and `.ignore`, if it has either.
fn ignores_in(fs: &dyn Fs, dir: &Path) -> Option<Gitignore> {
    let mut b = GitignoreBuilder::new(dir);
    let mut any = false;
    for name in [".gitignore", ".ignore"] {
        if let Ok(bytes) = fs.read(&dir.join(name)) {
            any = true;
            for line in String::from_utf8_lossy(&bytes).lines() {
                let _ = b.add_line(None, line);
            }
        }
    }
    if any { b.build().ok() } else { None }
}

/// The innermost rule that speaks about `path` decides.
fn ignored(ignores: &[Gitignore], path: &Path, is_dir: bool) -> bool {
    for g in ignores.iter().rev() {
        match g.matched(path, is_dir) {
            Match::Ignore(_) => return true,
            Match::Whitelist(_) => return false,
            Match::None => {}
        }
    }
    false
}

/// The `limit` most recently modified files under `root` matching `pattern`
/// (newest first), and how many matched in all. A pattern without a `/` also
/// matches file names anywhere.
pub fn glob(
    fs: &dyn Fs,
    root: &Path,
    pattern: &str,
    limit: usize,
    budget: usize,
) -> Result<(Vec<(PathBuf, SystemTime)>, usize), globset::Error> {
    let pat = pattern.trim_start_matches("./");
    let glob = globset::GlobBuilder::new(pat)
        .literal_separator(false)
        .build()?
        .compile_matcher();
    let basename_only = !pat.contains('/');
    let mut found: Bounded<(std::cmp::Reverse<SystemTime>, PathBuf)> = Bounded::new(limit);
    walk(fs, root, None, budget, &|_| false, &mut |v| {
        if v.is_dir {
            return true;
        }
        let relp = v.path.strip_prefix(root).unwrap_or(&v.path);
        let hit = glob.is_match(relp)
            || (basename_only
                && v.path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| glob.is_match(n)));
        if hit {
            let mt = fs
                .metadata(&v.path)
                .ok()
                .and_then(|m| m.modified)
                .unwrap_or(SystemTime::UNIX_EPOCH);
            found.push((std::cmp::Reverse(mt), v.path.clone()), String::new());
        }
        true
    });
    let total = found.total_lines;
    Ok((
        found
            .entries
            .into_keys()
            .map(|(std::cmp::Reverse(mt), p)| (p, mt))
            .collect(),
        total,
    ))
}

/// The extensions Grep's `type` filter means.
pub fn type_extensions(t: &str) -> Vec<String> {
    let exts: &[&str] = match t {
        "rust" | "rs" => &["rs"],
        "py" | "python" => &["py", "pyi"],
        "js" => &["js", "jsx", "mjs", "cjs"],
        "ts" => &["ts", "tsx", "mts", "cts"],
        "go" => &["go"],
        "java" => &["java"],
        "c" => &["c", "h"],
        "cpp" => &["cpp", "cc", "cxx", "hpp", "hh", "h"],
        "md" | "markdown" => &["md", "markdown"],
        other => return vec![other.to_string()],
    };
    exts.iter().map(|e| e.to_string()).collect()
}

/// What Grep asks for, besides the pattern.
pub struct GrepOptions {
    /// `content`, `count`, or `files_with_matches`.
    pub mode: String,
    pub case_insensitive: bool,
    pub multiline: bool,
    pub glob: Option<String>,
    pub file_type: Option<String>,
    pub before: usize,
    pub after: usize,
    /// Output lines to keep; the total is still counted.
    pub head: usize,
}

impl GrepOptions {
    pub fn regex(&self, pattern: &str) -> Result<regex::bytes::Regex, regex::Error> {
        regex::bytes::RegexBuilder::new(pattern)
            .case_insensitive(self.case_insensitive)
            .multi_line(true)
            .dot_matches_new_line(self.multiline)
            .build()
    }
}

/// One file's lines of Grep output, as `shown` names it, or None when it has
/// no match (or is binary). Searching one file names it nowhere, as ripgrep
/// prints it: `shown` is then None, except for `files_with_matches`.
pub fn grep_file(
    bytes: &[u8],
    shown: Option<&str>,
    re: &regex::bytes::Regex,
    opts: &GrepOptions,
) -> Option<String> {
    if bytes.iter().take(8000).any(|&b| b == 0) || !re.is_match(bytes) {
        return None;
    }
    Some(match opts.mode.as_str() {
        "content" => {
            // A final newline ends the last line rather than starting another.
            let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
            let lines: Vec<&[u8]> = body.split(|&b| b == b'\n').collect();
            let mut keep = vec![false; lines.len()];
            let mut hit = vec![false; lines.len()];
            for (i, l) in lines.iter().enumerate() {
                if re.is_match(l) {
                    hit[i] = true;
                    for k in keep
                        .iter_mut()
                        .take(
                            i.saturating_add(opts.after)
                                .saturating_add(1)
                                .min(lines.len()),
                        )
                        .skip(i.saturating_sub(opts.before))
                    {
                        *k = true;
                    }
                }
            }
            let mut s = String::new();
            let mut last = None;
            for i in 0..lines.len() {
                if keep[i] {
                    // Before each group of context, as ripgrep separates them
                    // (in this file and from the file before; `finish` drops
                    // the first); plain matches get none.
                    if opts.before + opts.after > 0 && last.is_none_or(|l| i > l + 1) {
                        s.push_str("--\n");
                    }
                    let sep = if hit[i] { ':' } else { '-' };
                    let text = String::from_utf8_lossy(lines[i]);
                    if let Some(shown) = shown {
                        s.push_str(shown);
                        s.push(sep);
                    }
                    s.push_str(&format!(
                        "{}{sep}{}\n",
                        i + 1,
                        crate::text::truncate(&text, 500)
                    ));
                    last = Some(i);
                }
            }
            s
        }
        "count" => match shown {
            Some(shown) => format!("{shown}:{}\n", re.find_iter(bytes).count()),
            None => format!("{}\n", re.find_iter(bytes).count()),
        },
        _ => format!("{}\n", shown.unwrap_or_default()),
    })
}

/// The output of every file, as one text and its line count: ripgrep
/// separates groups of context with `--`, but doesn't start with one.
pub fn finish(found: Bounded<impl Ord>) -> (String, usize) {
    let (text, total) = found.into_text();
    match text.strip_prefix("--\n") {
        Some(rest) => (rest.to_string(), total - 1),
        None => (text, total),
    }
}

/// Whether `glob` and the `type` filter let `path` through.
pub fn grep_filters(
    path: &Path,
    glob: Option<&globset::GlobMatcher>,
    exts: Option<&[String]>,
) -> bool {
    if let Some(g) = glob
        && !path.file_name().is_some_and(|n| g.is_match(n))
        && !g.is_match(path)
    {
        return false;
    }
    if let Some(exts) = exts {
        let ext = path.extension().and_then(|x| x.to_str()).unwrap_or("");
        if !exts.iter().any(|e| e == ext) {
            return false;
        }
    }
    true
}

/// Search `root` (a directory or one file). Returns the output, keeping only
/// what the first `opts.head` lines need (newest files first for paths, path
/// order otherwise), and the total line count. `show` names a file in it.
pub fn grep(
    fs: &dyn Fs,
    root: &Path,
    pattern: &str,
    opts: &GrepOptions,
    show: &dyn Fn(&Path) -> String,
) -> Result<(String, usize), anyhow::Error> {
    let re = opts.regex(pattern)?;
    let glob = match &opts.glob {
        Some(g) => Some(globset::Glob::new(g)?.compile_matcher()),
        None => None,
    };
    let exts = opts.file_type.as_deref().map(type_extensions);
    type Key = (Option<std::cmp::Reverse<SystemTime>>, PathBuf);
    let mut found: Bounded<Key> = Bounded::new(opts.head);
    let single = fs.metadata(root).is_ok_and(|m| m.is_file());
    let mut search = |path: &Path| {
        if !grep_filters(path, glob.as_ref(), exts.as_deref()) {
            return;
        }
        let Ok(bytes) = fs.read(path) else { return };
        let shown = show(path);
        let shown = (!single || opts.mode == "files_with_matches").then_some(shown.as_str());
        let Some(out) = grep_file(&bytes, shown, &re, opts) else {
            return;
        };
        let key = if opts.mode == "files_with_matches" {
            let mt = fs
                .metadata(path)
                .ok()
                .and_then(|m| m.modified)
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (Some(std::cmp::Reverse(mt)), path.to_path_buf())
        } else {
            (None, path.to_path_buf())
        };
        found.push(key, out);
    };
    if single {
        search(root);
    } else {
        // A `glob` overrides the ignore files, as in ripgrep.
        let forced = |p: &Path| {
            glob.as_ref()
                .is_some_and(|g| grep_filters(p, Some(g), None))
        };
        walk(fs, root, None, usize::MAX, &forced, &mut |v| {
            if !v.is_dir {
                search(&v.path);
            }
            true
        });
    }
    Ok(finish(found))
}

/// `root` as a tree `depth` levels deep, as Ls shows it: at most 500 lines.
pub fn tree(fs: &dyn Fs, root: &Path, depth: usize) -> String {
    let mut lines = vec![format!("{}/", root.display())];
    walk(fs, root, Some(depth), usize::MAX, &|_| false, &mut |v| {
        if lines.len() > 500 {
            lines.push("… (truncated)".into());
            return false;
        }
        let name = v
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        lines.push(format!(
            "{}{}{}",
            "  ".repeat(v.depth),
            name,
            if v.is_dir { "/" } else { "" }
        ));
        true
    });
    lines.join("\n")
}

#[cfg(test)]
mod search_tests;
