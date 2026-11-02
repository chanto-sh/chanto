//! Search tools: Glob, Grep (ripgrep when available, built-in parallel
//! fallback), Ls.

use super::{Ctx, Tool, ToolKind, ToolOutput, arg_bool, arg_str, arg_u64};
use crate::util::{rel, resolve};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use chanto_core::tools::{MAX_RESULTS, WALK_BUDGET, grep_options};

use chanto_core::search::{Bounded, GrepOptions};

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> String {
        "Glob".into()
    }
    fn description(&self) -> String {
        chanto_core::tools::GLOB_DESCRIPTION.into()
    }
    fn schema(&self) -> Value {
        chanto_core::tools::glob_schema()
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }
    fn rule_subjects(&self, args: &Value) -> Vec<String> {
        vec![
            args["path"]
                .as_str()
                .filter(|p| !p.is_empty())
                .unwrap_or(".")
                .to_string(),
        ]
    }
    fn label(&self, args: &Value) -> String {
        format!("Glob({})", args["pattern"].as_str().unwrap_or(""))
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let cwd = ctx.cwd();
        let mut root = args["path"]
            .as_str()
            .map(|p| resolve(&cwd, p))
            .unwrap_or(cwd.clone());
        let mut pattern = arg_str(&args, "pattern")?.to_string();
        if pattern.starts_with('/') || pattern.starts_with("~/") {
            // Split an absolute pattern into a literal directory root + relative glob.
            let full = resolve(&cwd, &pattern).display().to_string();
            let meta = full.find(['*', '?', '[', '{']).unwrap_or(full.len());
            let cut = full[..meta].rfind('/').unwrap_or(0);
            root = std::path::PathBuf::from(if cut == 0 { "/" } else { &full[..cut] });
            pattern = full[cut + 1..].to_string();
            if pattern.is_empty() {
                pattern = "**/*".into();
            }
        }
        let r2 = root.clone();
        let fs = ctx.env.fs();
        let (files, total) = tokio::task::spawn_blocking(move || {
            if fs.on_disk() {
                glob_files(&r2, &pattern, MAX_RESULTS, WALK_BUDGET)
            } else {
                Ok(chanto_core::search::glob(
                    fs.as_ref(),
                    &r2,
                    &pattern,
                    MAX_RESULTS,
                    WALK_BUDGET,
                )?)
            }
        })
        .await??;
        let out = chanto_core::tools::glob_output(files.iter().map(|(p, _)| rel(&cwd, p)), total);
        if total == 0 {
            return Ok(ToolOutput::ok(out).display("No files found"));
        }
        Ok(ToolOutput::ok(out).display(format!("Found {}", crate::util::plural(total, "file"))))
    }
}

/// Matches collected newest-first and bounded while the tree is walked.
type NewestFirst = Arc<Mutex<Bounded<(std::cmp::Reverse<SystemTime>, PathBuf)>>>;

/// The `limit` most recently modified matches (newest first), and how many
/// matched in all, stopping after `budget` visited entries.
fn glob_files(
    root: &Path,
    pattern: &str,
    limit: usize,
    budget: usize,
) -> Result<(Vec<(PathBuf, SystemTime)>, usize)> {
    let pat = pattern.trim_start_matches("./");
    let glob = globset::GlobBuilder::new(pat)
        .literal_separator(false)
        .build()?
        .compile_matcher();
    // Also match against basename when the pattern has no separator.
    let basename_only = !pat.contains('/');
    // Newest first, bounded: (Reverse(mtime), path) keeps the `limit` newest.
    let results: NewestFirst = Arc::new(Mutex::new(Bounded::new(limit)));
    // Results are best-effort newest over what was seen before the budget.
    let visited = Arc::new(AtomicUsize::new(0));
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .threads(num_threads())
        .build_parallel();
    walker.run(|| {
        let results = results.clone();
        let glob = glob.clone();
        let root = root.to_path_buf();
        let visited = visited.clone();
        Box::new(move |entry| {
            if visited.fetch_add(1, Ordering::Relaxed) >= budget {
                return ignore::WalkState::Quit;
            }
            if let Ok(e) = entry
                && e.file_type().map(|t| t.is_file()).unwrap_or(false)
            {
                let p = e.path();
                let relp = p.strip_prefix(&root).unwrap_or(p);
                let hit = glob.is_match(relp)
                    || (basename_only
                        && e.file_name()
                            .to_str()
                            .map(|n| glob.is_match(n))
                            .unwrap_or(false));
                if hit {
                    let mt = e
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(SystemTime::UNIX_EPOCH);
                    results
                        .lock()
                        .unwrap()
                        .push((std::cmp::Reverse(mt), p.to_path_buf()), String::new());
                }
            }
            ignore::WalkState::Continue
        })
    });
    let b = Arc::try_unwrap(results)
        .map_err(|_| anyhow::anyhow!("glob walker still running"))?
        .into_inner()
        .unwrap();
    let total = b.total_lines;
    Ok((
        b.entries
            .into_keys()
            .map(|(std::cmp::Reverse(mt), p)| (p, mt))
            .collect(),
        total,
    ))
}

fn num_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(12)
}

pub struct Grep;

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> String {
        "Grep".into()
    }
    fn description(&self) -> String {
        chanto_core::tools::GREP_DESCRIPTION.into()
    }
    fn schema(&self) -> Value {
        chanto_core::tools::grep_schema()
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }
    fn rule_subjects(&self, args: &Value) -> Vec<String> {
        vec![
            args["path"]
                .as_str()
                .filter(|p| !p.is_empty())
                .unwrap_or(".")
                .to_string(),
        ]
    }
    fn label(&self, args: &Value) -> String {
        let mut s = format!("Grep(\"{}\"", args["pattern"].as_str().unwrap_or(""));
        if let Some(p) = args["path"].as_str() {
            s.push_str(&format!(", {p}"));
        }
        if let Some(g) = args["glob"].as_str() {
            s.push_str(&format!(", {g}"));
        }
        s.push(')');
        s
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let cwd = ctx.cwd();
        let pattern = arg_str(&args, "pattern")?.to_string();
        let path = args["path"]
            .as_str()
            .map(|p| resolve(&cwd, p))
            .unwrap_or(cwd.clone());
        let mode = args["output_mode"]
            .as_str()
            .unwrap_or("files_with_matches")
            .to_string();
        let head = arg_u64(&args, "head_limit")
            .map(|n| n as usize)
            .unwrap_or(if mode == "content" { 400 } else { MAX_RESULTS });
        let fs = ctx.env.fs();
        // Every path keeps only what's needed for the first `head` lines.
        let (out, total, warn) = if fs.on_disk() && which_rg().is_some() {
            run_rg(&args, &pattern, &path, &mode, &cwd, head).await?
        } else {
            let opts = grep_options(&args, &mode, head);
            let c2 = cwd.clone();
            let (o, t) = tokio::task::spawn_blocking(move || {
                if fs.on_disk() {
                    builtin_grep(&opts, &pattern, &path, &c2)
                } else {
                    chanto_core::search::grep(fs.as_ref(), &path, &pattern, &opts, &|p| rel(&c2, p))
                }
            })
            .await??;
            (o, t, None)
        };
        let s = chanto_core::tools::grep_output(&out, total, head, warn.as_deref());
        if total == 0 {
            return Ok(ToolOutput::ok(s).display("No matches"));
        }
        Ok(ToolOutput::ok(s).display(format!(
            "Found {}",
            crate::util::plural(
                total,
                if args["output_mode"] == "content" {
                    "line"
                } else {
                    "result"
                }
            )
        )))
    }
}

fn which_rg() -> Option<&'static PathBuf> {
    use std::sync::OnceLock;
    static RG: OnceLock<Option<PathBuf>> = OnceLock::new();
    RG.get_or_init(|| {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join("rg"))
                .find(|p| p.is_file())
        })
    })
    .as_ref()
}

/// Run ripgrep, streaming its output: keeps the first `head` lines and counts
/// the rest. Returns (kept lines, total lines, warning about unsearchable paths).
async fn run_rg(
    args: &Value,
    pattern: &str,
    path: &Path,
    mode: &str,
    cwd: &Path,
    head: usize,
) -> Result<(String, usize, Option<String>)> {
    let mut cmd = tokio::process::Command::new(which_rg().unwrap());
    cmd.current_dir(cwd);
    cmd.args([
        "--no-config",
        "--color=never",
        "--max-columns=500",
        "--max-columns-preview",
        "--hidden",
        "--glob=!.git",
    ]);
    match mode {
        "content" => {
            if args
                .get("-n")
                .map(|v| v.as_bool() != Some(false))
                .unwrap_or(true)
            {
                cmd.arg("-n");
            }
            for k in ["-A", "-B", "-C"] {
                if let Some(n) = arg_u64(args, k) {
                    cmd.arg(format!("{k}{n}"));
                }
            }
        }
        "count" => {
            cmd.arg("--count");
        }
        // Unsorted: ripgrep's own --sortr makes it search on one thread. The
        // newest-first order is applied to the paths below instead.
        _ => {
            cmd.arg("--files-with-matches");
        }
    }
    if arg_bool(args, "-i") {
        cmd.arg("-i");
    }
    if arg_bool(args, "multiline") {
        cmd.args(["-U", "--multiline-dotall"]);
    }
    if let Some(g) = args["glob"].as_str() {
        cmd.arg(format!("--glob={g}"));
    }
    if let Some(t) = args["type"].as_str() {
        cmd.arg(format!("--type={t}"));
    }
    cmd.arg("-e").arg(pattern);
    let shown = if path == cwd {
        PathBuf::from(".")
    } else {
        path.strip_prefix(cwd)
            .map(|p| p.to_path_buf())
            .unwrap_or(path.to_path_buf())
    };
    cmd.arg(shown);
    cmd.kill_on_drop(true);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null());
    let mut child = cmd.spawn()?;
    let mut stderr = child.stderr.take().unwrap();
    let err_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut buf).await;
        buf
    });
    use tokio::io::AsyncBufReadExt;
    let mut reader = tokio::io::BufReader::new(child.stdout.take().unwrap());
    let (mut out, mut kept, mut total) = (String::new(), 0usize, 0usize);
    let paths_only = mode != "content" && mode != "count";
    let mut paths: Vec<String> = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).await? == 0 {
            break;
        }
        total += 1;
        let l = String::from_utf8_lossy(&line);
        let l = l.trim_end_matches(['\n', '\r']);
        let l = l.strip_prefix("./").unwrap_or(l);
        if paths_only {
            paths.push(l.to_string());
        } else if kept < head && out.len() < 120_000 {
            out.push_str(l);
            out.push('\n');
            kept += 1;
        }
    }
    if paths_only {
        out = newest_first(cwd, paths, head).await;
    }
    let status = child.wait().await?;
    let err = err_task.await.unwrap_or_default();
    if !status.success() && status.code() != Some(1) && total == 0 {
        anyhow::bail!("ripgrep error: {}", String::from_utf8_lossy(&err).trim());
    }
    // Partial results (e.g. unreadable directories): keep matches, note the warnings.
    let warn = (status.code() == Some(2) && !err.is_empty()).then(|| {
        format!(
            "[some paths could not be searched: {}]",
            crate::util::truncate(String::from_utf8_lossy(&err).trim(), 500)
        )
    });
    Ok((out, total, warn))
}

/// The first `head` of `paths` (relative to `cwd`), most recently modified
/// first, as `--sortr=modified` would list them.
async fn newest_first(cwd: &Path, paths: Vec<String>, head: usize) -> String {
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut dated: Vec<(std::time::SystemTime, String)> = paths
            .into_iter()
            .map(|p| {
                let t = std::fs::metadata(cwd.join(&p))
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                (t, p)
            })
            .collect();
        dated.sort_by_key(|a| std::cmp::Reverse(a.0));
        let mut out = String::new();
        for (_, p) in dated.into_iter().take(head) {
            if out.len() >= 120_000 {
                break;
            }
            out.push_str(&p);
            out.push('\n');
        }
        out
    })
    .await
    .unwrap_or_default()
}

/// Search the disk without ripgrep, in parallel. Keeps only the entries
/// needed for the first `head` lines (newest files first for paths; path
/// order otherwise). Returns (text, total lines).
fn builtin_grep(
    opts: &GrepOptions,
    pattern: &str,
    root: &Path,
    cwd: &Path,
) -> Result<(String, usize)> {
    let re = opts.regex(pattern)?;
    let glob = match &opts.glob {
        Some(g) => Some(globset::Glob::new(g)?.compile_matcher()),
        None => None,
    };
    let exts = opts
        .file_type
        .as_deref()
        .map(chanto_core::search::type_extensions);
    type Key = (Option<std::cmp::Reverse<SystemTime>>, PathBuf);
    let results: Arc<Mutex<Bounded<Key>>> = Arc::new(Mutex::new(Bounded::new(opts.head)));
    let single = root.is_file();
    let mut walker = ignore::WalkBuilder::new(root);
    walker
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .threads(num_threads());
    // A `glob` overrides the ignore files, as in ripgrep.
    if let Some(g) = &opts.glob {
        let mut o = ignore::overrides::OverrideBuilder::new(root);
        o.add(g)?;
        walker.overrides(o.build()?);
    }
    walker.build_parallel().run(|| {
        let results = results.clone();
        let re = re.clone();
        let glob = glob.clone();
        let exts = exts.clone();
        let cwd = cwd.to_path_buf();
        Box::new(move |entry| {
            let Ok(e) = entry else {
                return ignore::WalkState::Continue;
            };
            if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                return ignore::WalkState::Continue;
            }
            let p = e.path();
            if !chanto_core::search::grep_filters(p, glob.as_ref(), exts.as_deref()) {
                return ignore::WalkState::Continue;
            }
            let Ok(bytes) = std::fs::read(p) else {
                return ignore::WalkState::Continue;
            };
            let shown = rel(&cwd, p);
            let shown = (!single || opts.mode == "files_with_matches").then_some(shown.as_str());
            let Some(out) = chanto_core::search::grep_file(&bytes, shown, &re, opts) else {
                return ignore::WalkState::Continue;
            };
            // files_with_matches: newest first; otherwise path order.
            let key = if opts.mode == "files_with_matches" {
                let mt = e
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                (Some(std::cmp::Reverse(mt)), p.to_path_buf())
            } else {
                (None, p.to_path_buf())
            };
            results.lock().unwrap().push(key, out);
            ignore::WalkState::Continue
        })
    });
    let b = Arc::try_unwrap(results)
        .map_err(|_| anyhow::anyhow!("grep walker still running"))?
        .into_inner()
        .unwrap();
    Ok(chanto_core::search::finish(b))
}

pub struct Ls;

#[async_trait]
impl Tool for Ls {
    fn name(&self) -> String {
        "Ls".into()
    }
    fn description(&self) -> String {
        chanto_core::tools::LS_DESCRIPTION.into()
    }
    fn schema(&self) -> Value {
        chanto_core::tools::ls_schema()
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }
    fn rule_subjects(&self, args: &Value) -> Vec<String> {
        vec![
            args["path"]
                .as_str()
                .filter(|p| !p.is_empty())
                .unwrap_or(".")
                .to_string(),
        ]
    }
    fn label(&self, args: &Value) -> String {
        format!("Ls({})", args["path"].as_str().unwrap_or("."))
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let cwd = ctx.cwd();
        let root = args["path"]
            .as_str()
            .map(|p| resolve(&cwd, p))
            .unwrap_or(cwd.clone());
        let depth = arg_u64(&args, "depth").unwrap_or(2) as usize;
        let r2 = root.clone();
        let fs = ctx.env.fs();
        let out = tokio::task::spawn_blocking(move || {
            if !fs.on_disk() {
                return chanto_core::search::tree(fs.as_ref(), &r2, depth);
            }
            let mut lines = vec![format!("{}/", r2.display())];
            let walker = ignore::WalkBuilder::new(&r2)
                .hidden(false)
                .max_depth(Some(depth))
                .filter_entry(|e| e.file_name() != ".git")
                .sort_by_file_name(|a, b| a.cmp(b))
                .build();
            for e in walker.flatten().skip(1) {
                if lines.len() > 500 {
                    lines.push("… (truncated)".into());
                    break;
                }
                let d = e.depth();
                let name = e.file_name().to_string_lossy();
                let suffix = if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    "/"
                } else {
                    ""
                };
                lines.push(format!("{}{}{}", "  ".repeat(d), name, suffix));
            }
            lines.join("\n")
        })
        .await?;
        let n = out.lines().count() - 1;
        Ok(ToolOutput::ok(out).display(format!("Listed {}", crate::util::plural(n, "entry"))))
    }
}

#[cfg(test)]
mod tests;
