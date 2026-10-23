//! Verified mode: the agent can't finish a turn that changed the project
//! until the project's checks pass and existing tests are intact.
//!
//! - Done-gate: run the configured (or auto-detected) check commands when the
//!   model gives its final answer; failures go back to the model.
//! - Test protection: existing test files are snapshotted when the turn
//!   starts; deleting them, removing assertions or adding skip markers fails
//!   the gate ("fix the code, not the tests").
//! - Evidence: the final answer ends with which checks passed.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

mod select;
use select::{related, selection_broken};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GuardrailsConfig {
    /// Turn verified mode on (also `--verify`).
    pub enabled: bool,
    /// Commands that must exit 0 (default: detected from the project).
    pub checks: Vec<String>,
    /// Gate attempts before giving up and reporting failure (default 3).
    pub max_attempts: Option<u32>,
    /// Fail the gate when existing tests are deleted or weakened (default true).
    pub protect_tests: Option<bool>,
    /// Per-check timeout in seconds (default 600).
    pub timeout_secs: Option<u64>,
    /// Which tests the done-gate runs: "related" (default) or "full".
    pub scope: Option<Scope>,
    /// Before the done-gate, have the `oracle` subagent review the turn's
    /// diff once; its findings go back to the model (default false).
    pub oracle: Option<bool>,
}

/// Which tests the done-gate runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// The tests related to the files changed this turn; the full suite when
    /// that can't be worked out safely.
    Related,
    /// The whole suite, every time.
    Full,
}

impl Scope {
    pub fn parse(s: &str) -> Option<Scope> {
        match s.trim() {
            "related" => Some(Scope::Related),
            "full" => Some(Scope::Full),
            _ => None,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Scope::Related => "related",
            Scope::Full => "full",
        }
    }
}

impl GuardrailsConfig {
    pub fn merge(&mut self, o: GuardrailsConfig, raw: &toml::Table) {
        if raw.contains_key("enabled") {
            self.enabled = o.enabled;
        }
        if !o.checks.is_empty() {
            self.checks = o.checks;
        }
        if o.max_attempts.is_some() {
            self.max_attempts = o.max_attempts;
        }
        if o.protect_tests.is_some() {
            self.protect_tests = o.protect_tests;
        }
        if o.timeout_secs.is_some() {
            self.timeout_secs = o.timeout_secs;
        }
        if o.scope.is_some() {
            self.scope = o.scope;
        }
        if o.oracle.is_some() {
            self.oracle = o.oracle;
        }
    }
}

fn has_cmd(c: &str) -> bool {
    crate::shell::command_exists(c)
}

/// Check commands for the project, from its build files.
pub fn detect_checks(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if root.join("Cargo.toml").exists() {
        out.push("cargo test --quiet".to_string());
    }
    if root.join("go.mod").exists() {
        out.push("go vet ./... && go test ./...".to_string());
    }
    if let Ok(pkg) = std::fs::read_to_string(root.join("package.json")) {
        let v: serde_json::Value = serde_json::from_str(&pkg).unwrap_or_default();
        let test = v["scripts"]["test"].as_str().unwrap_or("");
        if !test.is_empty() && !test.contains("no test specified") {
            out.push("npm test --silent".to_string());
        }
        if v["scripts"]["typecheck"].is_string() {
            out.push("npm run typecheck --silent".to_string());
        }
    }
    let py = [
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "pytest.ini",
        "tox.ini",
    ]
    .iter()
    .any(|f| root.join(f).exists())
        || walk(root, 3)
            .iter()
            .any(|p| is_test_file(p) && p.extension().and_then(|e| e.to_str()) == Some("py"));
    if py {
        // The first Python with pytest (Homebrew's `python` may have it when `python3` doesn't).
        let pytest = ["python3", "python"].into_iter().find(|py| {
            std::process::Command::new(py)
                .args(["-c", "import pytest"])
                .output()
                .is_ok_and(|o| o.status.success())
        });
        // No -x: a failure that predates the change mustn't hide the tests after
        // it. -rfE names every failure (older pytest doesn't by default), which
        // is how failures are matched against the baseline.
        out.push(match pytest {
            Some(py) => format!("{py} -m pytest -q -rfE"),
            None => "python3 -m unittest discover -q -p '*test*.py'".into(),
        });
    }
    if out.is_empty()
        && root.join("Makefile").exists()
        && has_cmd("make")
        && std::fs::read_to_string(root.join("Makefile"))
            .unwrap_or_default()
            .lines()
            .any(|l| l.starts_with("test:"))
    {
        out.push("make test".into());
    }
    out
}

/// Heuristic: is this a test file?
pub fn is_test_file(p: &Path) -> bool {
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    // `tests`, but also `testing/` (pytest keeps its suite there) and
    // `test_utils/`, so match a word within the directory name rather than
    // requiring the whole component to be one known spelling.
    let in_test_dir = p.components().any(|c| {
        c.as_os_str().to_str().is_some_and(|part| {
            part.split(['-', '_', '.'])
                .any(|w| w.to_ascii_lowercase().starts_with("test"))
        })
    });
    let code = [
        "py", "rs", "go", "js", "ts", "jsx", "tsx", "mjs", "cjs", "rb", "java", "kt", "php", "cs",
        "swift", "c", "cc", "cpp",
    ]
    .iter()
    .any(|e| name.ends_with(&format!(".{e}")));
    code && (name.starts_with("test_")
        || name.contains("_test.")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.contains("_spec.")
        || name.ends_with("test.java")
        || name.ends_with("tests.cs")
        || in_test_dir)
}

fn walk(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    ignore::WalkBuilder::new(root)
        .max_depth(Some(max_depth))
        .hidden(true)
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
        .filter(|p| {
            !p.components().any(|c| {
                matches!(
                    c.as_os_str().to_str(),
                    Some("node_modules" | "target" | ".chanto")
                )
            })
        })
        .take(5000)
        .collect()
}

/// Test files and their contents at the start of a turn.
pub struct Snapshot {
    files: HashMap<PathBuf, String>,
    /// Files that must not change at all (e.g. approved acceptance criteria).
    locked: HashMap<PathBuf, String>,
}

pub fn snapshot(root: &Path) -> Snapshot {
    let mut files = HashMap::new();
    let mut total = 0usize;
    for p in walk(root, 12) {
        let name_is_test = is_test_file(&p);
        // Inline tests (Rust `#[cfg(test)] mod tests`, and similar) live in
        // ordinary source files, which the name heuristic misses — yet they are
        // most of Chanto's own suite. Read Rust sources and keep the ones that
        // actually contain tests.
        let is_rust = p.extension().is_some_and(|e| e == "rs");
        if !name_is_test && !is_rust {
            continue;
        }
        let Ok(t) = std::fs::read_to_string(&p) else {
            continue;
        };
        let has_inline_tests = is_rust && (t.contains("#[cfg(test)]") || t.contains("mod tests"));
        if !name_is_test && !has_inline_tests {
            continue;
        }
        total += t.len();
        if total > 20_000_000 {
            break;
        }
        files.insert(p, t);
    }
    Snapshot {
        files,
        locked: HashMap::new(),
    }
}

impl Snapshot {
    /// Also lock every file under `dirs` against any change.
    pub fn lock_dirs(&mut self, dirs: &[PathBuf]) {
        for d in dirs {
            for p in walk(d, 12) {
                if let Ok(t) = std::fs::read_to_string(&p) {
                    self.locked.insert(p, t);
                }
            }
        }
    }
}

const ASSERT_MARKERS: &[&str] = &[
    "assert",
    "expect(",
    "expect!",
    ".should",
    "t.error",
    "t.fatal",
    "t.fail",
    "require.",
    "verify(",
    "self.fail",
    "pytest.raises",
    "assertraises",
];
const SKIP_MARKERS: &[&str] = &[
    "@unittest.skip",
    "pytest.mark.skip",
    "pytest.skip(",
    "pytest.mark.xfail",
    ".skip(",
    "xit(",
    "xdescribe(",
    "xtest(",
    "#[ignore]",
    "t.skip(",
    "@disabled",
    "@ignore",
    "skiptest(",
    "return  # skip",
    "test.todo(",
];

/// The test functions in a file and their bodies, normalized. Adding a test
/// is fine; one that was there before and now reads differently means the
/// change was fitted to the tests instead of the other way round.
fn test_functions(text: &str) -> std::collections::BTreeMap<String, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = std::collections::BTreeMap::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if let Some(name) = test_name(trimmed, &lines, i) {
            let (body, end) = indented_body(&lines, i, indent);
            let key = if out.contains_key(&name) {
                (2..)
                    .map(|n| format!("{name} ({n})"))
                    .find(|k| !out.contains_key(k))
                    .unwrap()
            } else {
                name
            };
            out.insert(key, body);
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

fn test_name(line: &str, lines: &[&str], i: usize) -> Option<String> {
    // Python (including unittest methods and async tests).
    if let Some(rest) = line
        .strip_prefix("async def ")
        .or_else(|| line.strip_prefix("def "))
    {
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        return name.starts_with("test").then_some(name);
    }
    // Go.
    if let Some(rest) = line.strip_prefix("func Test") {
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        return (!name.is_empty()).then(|| format!("Test{name}"));
    }
    // Rust, only when annotated as a test.
    if let Some(rest) = line
        .strip_prefix("pub fn ")
        .or_else(|| line.strip_prefix("async fn "))
        .or_else(|| line.strip_prefix("fn "))
    {
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let annotated = lines[i.saturating_sub(3)..i].iter().any(|l| {
            let l = l.trim();
            l.starts_with("#[test]") || l.starts_with("#[tokio::test]")
        });
        return annotated.then_some(name);
    }
    None
}

/// A function body, from the line after its signature until the indentation
/// drops back to the signature's. Blank lines and indentation are dropped so a
/// pure reformat doesn't read as a change.
fn indented_body(lines: &[&str], start: usize, indent: usize) -> (String, usize) {
    let mut body = String::new();
    let mut i = start + 1;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        if line.len() - line.trim_start().len() <= indent {
            break;
        }
        body.push_str(line.trim());
        body.push('\n');
        i += 1;
    }
    (body, i)
}

fn count(text: &str, markers: &[&str]) -> usize {
    text.lines()
        .filter(|l| {
            let l = l.trim().to_ascii_lowercase();
            !l.starts_with('#') && !l.starts_with("//") && markers.iter().any(|m| l.contains(m))
        })
        .count()
}

impl Snapshot {
    /// How existing tests were weakened since the snapshot.
    pub fn violations(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut locked: Vec<&PathBuf> = self.locked.keys().collect();
        locked.sort();
        for p in locked {
            if std::fs::read_to_string(p).ok().as_ref() != Some(&self.locked[p]) {
                out.push(format!("{}: approved acceptance criteria are locked, but this file was changed or deleted", p.display()));
            }
        }
        let mut paths: Vec<&PathBuf> = self.files.keys().collect();
        paths.sort();
        for p in paths {
            let before = &self.files[p];
            let name = p.display();
            let Ok(after) = std::fs::read_to_string(p) else {
                out.push(format!("{name}: test file deleted"));
                continue;
            };
            if &after == before {
                continue;
            }
            let (a0, a1) = (count(before, ASSERT_MARKERS), count(&after, ASSERT_MARKERS));
            if a1 < a0 {
                out.push(format!("{name}: {} assertion(s) removed", a0 - a1));
            }
            let (s0, s1) = (count(before, SKIP_MARKERS), count(&after, SKIP_MARKERS));
            if s1 > s0 {
                out.push(format!("{name}: {} skip marker(s) added", s1 - s0));
            }
            // Same number of assertions but different ones: expected values changed.
            if a1 == a0 && a0 > 0 {
                let set = |t: &str| -> Vec<String> {
                    let mut v: Vec<String> = t
                        .lines()
                        .map(|l| l.trim().to_string())
                        .filter(|l| {
                            ASSERT_MARKERS
                                .iter()
                                .any(|m| l.to_ascii_lowercase().contains(m))
                        })
                        .collect();
                    v.sort();
                    v
                };
                let (b, a) = (set(before), set(&after));
                let changed = b.iter().filter(|l| !a.contains(l)).count();
                if changed > 0 {
                    out.push(format!("{name}: {changed} existing assertion(s) changed"));
                }
            }
            // A test whose body changed in any other way is the same move one
            // step earlier: the change was fitted to the test, not the code.
            let (b, a) = (test_functions(before), test_functions(&after));
            for (func, body) in &b {
                match a.get(func) {
                    None => out.push(format!("{name}: {func} (existing test) removed")),
                    Some(now) if now != body => {
                        out.push(format!("{name}: {func} (existing test) changed"))
                    }
                    _ => {}
                }
            }
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub command: String,
    pub ok: bool,
    pub output: String,
    pub secs: f32,
}

pub async fn run_check(cmd: &str, cwd: &Path, timeout: Duration) -> CheckResult {
    run_check_env(cmd, cwd, timeout, &[]).await
}

/// `run_check` with extra environment variables (goal criteria).
pub async fn run_check_env(
    cmd: &str,
    cwd: &Path,
    timeout: Duration,
    env: &[(String, String)],
) -> CheckResult {
    let t0 = Instant::now();
    let mut env = env.to_vec();
    // Edits within the same second can leave stale .pyc files that hide the fix.
    env.push(("PYTHONDONTWRITEBYTECODE".into(), "1".into()));
    // A background run's checks run where its commands do: in its container.
    let container = crate::runs::container::for_path(cwd);
    let (mut c, token) = match &container {
        Some(ct) => {
            let (c, t) = ct.command(cmd, cwd, &env);
            (c, Some(t))
        }
        None => {
            let mut c = crate::shell::command(cmd);
            c.current_dir(cwd).stdin(std::process::Stdio::null());
            c.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            (c, None)
        }
    };
    c.kill_on_drop(true);
    let r = run_detached_output(c, timeout).await;
    if let (Err(_), Some(ct), Some(t)) = (&r, &container, &token) {
        ct.kill(t);
    }
    let (ok, output) = match r {
        Ok(Ok((status, out))) => (status.success(), out),
        Ok(Err(e)) => (false, format!("could not run: {e}")),
        Err(_) => (false, format!("timed out after {}s", timeout.as_secs())),
    };
    CheckResult {
        command: cmd.to_string(),
        ok,
        output: crate::tools::bash::strip_ansi(&output),
        secs: t0.elapsed().as_secs_f32(),
    }
}

/// How long to keep reading a check's output after its process exits.
const OUTPUT_GRACE: Duration = Duration::from_millis(300);

/// Run `c` and collect stdout then stderr, as `Command::output` does, except
/// that it stops waiting once the process itself exits: a background daemon
/// it started (a language server, a build watcher) inherits the pipes and
/// would otherwise hold the check open until its time limit. Output still
/// arriving within a short grace period is kept; the pipes are then drained
/// and discarded in the background so the daemon isn't killed by SIGPIPE.
async fn run_detached_output(
    mut c: tokio::process::Command,
    timeout: Duration,
) -> Result<std::io::Result<(std::process::ExitStatus, String)>, tokio::time::error::Elapsed> {
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncReadExt;
    c.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => return Ok(Err(e)),
    };
    let collect = |pipe: Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>>| {
        let buf = Arc::new(Mutex::new(Some(Vec::new())));
        let b = buf.clone();
        let done = tokio::spawn(async move {
            let Some(mut pipe) = pipe else { return };
            let mut chunk = [0u8; 8192];
            while let Ok(n) = pipe.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                // Once detached (`None`), keep draining but drop the bytes.
                if let Some(v) = b.lock().unwrap().as_mut() {
                    v.extend_from_slice(&chunk[..n]);
                }
            }
        });
        (buf, done)
    };
    let (out_buf, out_done) = collect(child.stdout.take().map(|p| Box::new(p) as _));
    let (err_buf, err_done) = collect(child.stderr.take().map(|p| Box::new(p) as _));
    let status = tokio::time::timeout(timeout, child.wait()).await?;
    let status = match status {
        Ok(s) => s,
        Err(e) => return Ok(Err(e)),
    };
    let _ = tokio::time::timeout(OUTPUT_GRACE, async {
        let _ = out_done.await;
        let _ = err_done.await;
    })
    .await;
    let take = |b: &Arc<Mutex<Option<Vec<u8>>>>| b.lock().unwrap().take().unwrap_or_default();
    let (out, err) = (take(&out_buf), take(&err_buf));
    Ok(Ok((
        status,
        format!(
            "{}{}",
            String::from_utf8_lossy(&out),
            String::from_utf8_lossy(&err)
        ),
    )))
}

/// The check was killed at its time limit rather than finishing.
pub fn timed_out(r: &CheckResult) -> bool {
    r.output.starts_with("timed out after")
}

/// Outcome of the done-gate.
pub enum Gate {
    /// Evidence line for the final answer.
    Passed(String),
    /// Feedback for the model, and the failed check (if a check failed).
    Failed(String, Option<CheckResult>),
    /// Nothing failed, but no check ran any tests either: says why.
    NoTests(String),
}

/// Test-runner commands, by the words that name them.
const RUNNERS: &[&[&str]] = &[
    &["cargo", "test"],
    &["cargo", "nextest", "run"],
    &["go", "test"],
    &["go", "vet"],
    &["python3", "-m", "pytest"],
    &["python", "-m", "pytest"],
    &["pytest"],
    &["python3", "-m", "unittest"],
    &["python", "-m", "unittest"],
    &["npm", "test"],
    &["npm", "run", "test"],
    &["pnpm", "test"],
    &["yarn", "test"],
    &["npx", "vitest", "run"],
    &["npx", "vitest", "related"],
    &["npx", "jest"],
    &["npm", "run", "typecheck"],
    &["make", "test"],
    &["make", "check"],
];

/// Flags that run only part of a suite, or nothing at all.
const FILTERS: &[&str] = &[
    "-k",
    "-m",
    "--lf",
    "--last-failed",
    "-run",
    "--run",
    "-p",
    "--package",
    "--lib",
    "--bin",
    "--test",
    "--exact",
    "-t",
    "--testNamePattern",
    "--grep",
    "--deselect",
    "--ignore",
    "--ignore-glob",
    "--collect-only",
    "--co",
    "--setup-only",
    "--setup-plan",
    "--no-run",
    "--list",
    "-list",
    "--listTests",
    "--skip",
    "-skip",
    "-short",
    "--onlyChanged",
    "-o",
    "--doc",
    "--tests",
    "--examples",
    "--benches",
    "--bins",
];

/// Flags that take a value: the value isn't a positional argument.
const VALUE_FLAGS: &[&str] = &[
    "-p",
    "-k",
    "-m",
    "-run",
    "-t",
    "--package",
    "--test",
    "--pattern",
    "-s",
    "--start-directory",
    "--deselect",
    "--ignore",
    "--ignore-glob",
    "-skip",
    "--skip",
    "-o",
    "-c",
    "--rootdir",
    "--confcutdir",
    "--basetemp",
    "--tb",
    "--top-level-directory",
];

fn runner(words: &[String]) -> Option<(usize, String)> {
    RUNNERS
        .iter()
        .filter(|r| words.len() >= r.len() && words.iter().zip(r.iter()).all(|(a, b)| a == b))
        .max_by_key(|r| r.len())
        .map(|r| (r.len(), r.join(" ")))
}

/// Runners that are the same tool under another name.
fn family(name: &str) -> &str {
    match name {
        "python3 -m pytest" | "python -m pytest" | "pytest" => "pytest",
        "python3 -m unittest" | "python -m unittest" => "unittest",
        "npx vitest run" | "npx vitest related" => "vitest",
        n => n,
    }
}

/// A runner invocation's arguments after the runner words.
struct Parsed {
    pos: Vec<String>,
    flags: Vec<String>,
    /// `cargo test -p <name>` packages.
    pkgs: Vec<String>,
}

/// The positional arguments and flags after the runner, with redirections dropped.
fn args(words: &[String]) -> Parsed {
    let mut p = Parsed {
        pos: vec![],
        flags: vec![],
        pkgs: vec![],
    };
    let mut it = words.iter();
    while let Some(w) = it.next() {
        if w.contains('>') || w.contains('<') {
            continue;
        }
        if w.starts_with('-') {
            let (flag, value) = w
                .split_once('=')
                .map(|(f, v)| (f, Some(v.to_string())))
                .unwrap_or((w.as_str(), None));
            p.flags.push(flag.to_string());
            // Flags that take a value: the value isn't a positional argument.
            let value = if value.is_none() && VALUE_FLAGS.contains(&flag) {
                it.next().cloned()
            } else {
                value
            };
            if matches!(flag, "-p" | "--package") {
                p.pkgs.extend(value);
            }
        } else if w != "." && w != "--" {
            p.pos
                .push(w.trim_start_matches("./").trim_end_matches('/').to_string());
        }
    }
    p
}

/// Is test target `want` (a file, directory, package or module) inside `have`?
fn target_covers(fam: &str, have: &str, want: &str) -> bool {
    if have == want {
        return true;
    }
    match fam {
        "go test" | "go vet" => {
            have == "..."
                || have
                    .strip_suffix("/...")
                    .is_some_and(|d| want == d || want.starts_with(&format!("{d}/")))
        }
        // A directory holds the files under it; `file::test` is narrower than the file.
        "pytest" => !have.contains("::") && want.starts_with(&format!("{have}/")),
        _ => false,
    }
}

/// Does running `a` exercise everything `c` does, within one runner family?
fn superset(fam: &str, a: &Parsed, c: &Parsed, a_name: &str, c_name: &str) -> bool {
    let all = |have: &[String], want: &[String]| {
        want.iter()
            .all(|w| have.iter().any(|h| target_covers(fam, h, w)))
    };
    match fam {
        // No paths: the whole suite. Paths: each wanted file under one of them.
        "pytest" => {
            if c.pos.is_empty() {
                a.pos.is_empty()
            } else {
                a.pos.is_empty() || all(&a.pos, &c.pos)
            }
        }
        // `--findRelatedTests` / `vitest related` list source files: more files, more tests.
        "npx jest" | "vitest" => {
            let related = |p: &Parsed, name: &str| {
                p.flags.iter().any(|f| f == "--findRelatedTests") || name == "npx vitest related"
            };
            match (related(a, a_name), related(c, c_name)) {
                (false, _) if a.pos.is_empty() => true,
                (true, true) => all(&a.pos, &c.pos),
                _ => a.pos == c.pos && related(a, a_name) == related(c, c_name),
            }
        }
        "go test" | "go vet" => !c.pos.is_empty() && all(&a.pos, &c.pos),
        // Positional arguments filter test names; `-p` picks packages.
        "cargo test" | "cargo nextest run" => {
            let workspace = a.flags.iter().any(|f| f == "--workspace" || f == "--all");
            a.pos == c.pos
                && if c.pkgs.is_empty() {
                    a.pkgs.is_empty()
                } else {
                    workspace || all(&a.pkgs, &c.pkgs)
                }
        }
        // Module names: a run of more modules covers fewer.
        "unittest" => {
            a.pos == c.pos
                || (!c.pos.is_empty()
                    && !c.pos.iter().any(|p| p == "discover")
                    && all(&a.pos, &c.pos))
        }
        _ => a.pos == c.pos,
    }
}

/// Does a passing run of `agent_cmd` prove that `check` passes? Only a plain run
/// of the same test runner over at least the same scope counts: no pipes
/// (`| tail` hides the exit status), no `||`, no backgrounding, no filters to
/// part of the suite. A run of more files or packages than `check` names
/// counts; a run of fewer doesn't.
pub fn covers(agent_cmd: &str, check: &str) -> bool {
    if agent_cmd.contains('|') || agent_cmd.contains('`') || agent_cmd.contains("$(") {
        return false;
    }
    let segments = |c: &str| -> Option<Vec<Vec<String>>> {
        c.split("&&")
            .map(|seg| {
                shell_words::split(seg.trim())
                    .ok()
                    .filter(|w| !w.is_empty())
            })
            .collect()
    };
    let (Some(agent), Some(want)) = (segments(agent_cmd), segments(check)) else {
        return false;
    };
    if agent.iter().any(|w| w.iter().any(|x| x == ";" || x == "&")) || agent_cmd.contains(';') {
        return false;
    }
    want.iter().all(|cw| {
        let Some((n, name)) = runner(cw) else {
            return false;
        };
        let fam = family(&name);
        let c = args(&cw[n..]);
        agent.iter().any(|aw| {
            let Some((m, aname)) = runner(aw) else {
                return false;
            };
            if family(&aname) != fam {
                return false;
            }
            let a = args(&aw[m..]);
            // unittest's -p is the file pattern and cargo's picks packages: not filters.
            let filter = |f: &String| {
                FILTERS.contains(&f.as_str())
                    && !(fam == "unittest" && f == "-p")
                    && !(fam.starts_with("cargo") && (f == "-p" || f == "--package"))
                    // `go test -n` prints the commands without running them.
                    || (fam.starts_with("go") && f == "-n")
            };
            superset(fam, &a, &c, &aname, &name) && !a.flags.iter().any(filter)
        })
    })
}

/// Names of failing tests in a test runner's output (pytest, unittest, cargo,
/// go, jest/vitest), or None when the output doesn't name any.
pub fn failing_tests(output: &str) -> Option<std::collections::BTreeSet<String>> {
    static RE: std::sync::OnceLock<Vec<regex::Regex>> = std::sync::OnceLock::new();
    let res = RE.get_or_init(|| {
        [
            r"(?m)^(?:FAILED|ERROR) (\S+::\S+)",
            // pytest's section headers ("____ test_name[param] ____"), which every
            // version prints, with or without a short summary.
            r"(?m)^_{3,} (.+?) _{3,}\s*$",
            r"(?m)^(?:FAIL|ERROR): (\S+ \([^)]+\))",
            r"(?m)^test (\S+) \.\.\. FAILED",
            r"(?m)^\s*--- FAIL: (\S+)",
            r"(?m)^\s*[✕×] (.+?)(?: \(\d+ ?ms\))?$",
        ]
        .iter()
        .map(|r| regex::Regex::new(r).unwrap())
        .collect()
    });
    let set: std::collections::BTreeSet<String> = res
        .iter()
        .flat_map(|r| r.captures_iter(output).map(|c| c[1].trim().to_string()))
        .collect();
    (!set.is_empty()).then_some(set)
}

/// Split a failed check into failures that are new and ones that were already
/// failing before the agent started. None: can't tell (no baseline, or the
/// output names no tests), so treat everything as new.
pub fn pre_existing(
    now: &CheckResult,
    baseline: &[CheckResult],
) -> Option<(Vec<String>, Vec<String>)> {
    let base = baseline.iter().find(|b| b.command == now.command)?;
    if base.ok {
        return None;
    }
    match (failing_tests(&now.output), failing_tests(&base.output)) {
        (Some(n), Some(b)) => Some((
            n.difference(&b).cloned().collect(),
            n.intersection(&b).cloned().collect(),
        )),
        // Neither names tests (e.g. it doesn't build) and it failed before too.
        (None, None) => Some((vec![], vec![format!("`{}`", now.command)])),
        _ => None,
    }
}

/// A test runner that found nothing to run (unittest/pytest exit with an error for that).
fn ran_no_tests(output: &str) -> bool {
    let o = output.to_lowercase();
    let says_none = o.contains("no tests ran")
        || o.contains("collected 0 items")
        || o.contains("ran 0 tests")
        || o.contains("no test files") && !o.contains("fail");
    // A pytest *collection error* also prints "no tests ran" but exits non-zero
    // and is a genuine failure (broken import, syntax error, bad conftest), so
    // only treat it as benign when nothing looks like an error.
    says_none
        && !o.contains("error")
        && !o.contains("traceback")
        && !o.contains("exception")
        && !o.contains("failed")
        && !o.contains("cannot")
}

/// More changed files than this: verify with the full suite.
const MAX_CHANGED: usize = 20;
/// More related test files than this: the full suite isn't much slower.
const MAX_RELATED_FILES: usize = 50;
/// A project with at most this many test files just runs everything.
const SMALL_PROJECT_TESTS: usize = 20;
/// A full suite measured at most this long in this session just runs.
pub const FAST_FULL_SECS: f32 = 30.0;

/// One command the done-gate runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// The check as configured or detected: the full suite.
    pub full: String,
    /// What actually runs: `full`, or the related selection.
    pub command: String,
    /// The selection's targets (empty: the full suite), each with the file it
    /// names, when it names a file.
    targets: Vec<(String, Option<PathBuf>)>,
    /// Runner commands the targets are appended to (`go vet`, `go test`).
    prefixes: Vec<String>,
    /// Targets are rendered as `-p <crate>`.
    packages: bool,
}

impl Run {
    fn full(check: &str) -> Run {
        Run {
            full: check.to_string(),
            command: check.to_string(),
            targets: vec![],
            prefixes: vec![],
            packages: false,
        }
    }

    fn scoped(
        full: &str,
        prefixes: Vec<String>,
        targets: Vec<(String, Option<PathBuf>)>,
        packages: bool,
    ) -> Run {
        let mut r = Run {
            full: full.to_string(),
            command: String::new(),
            targets,
            prefixes,
            packages,
        };
        r.command = r.render(&r.targets.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>());
        r
    }

    fn render(&self, targets: &[String]) -> String {
        let args = targets
            .iter()
            .map(|t| {
                if self.packages {
                    format!("-p {}", shell_words::quote(t))
                } else {
                    shell_words::quote(t).into_owned()
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        self.prefixes
            .iter()
            .map(|p| format!("{p} {args}"))
            .collect::<Vec<_>>()
            .join(" && ")
    }

    /// Runs only the tests related to the change, not the whole suite.
    pub fn is_scoped(&self) -> bool {
        !self.targets.is_empty()
    }

    /// The selection without the test files that didn't exist before this
    /// turn (for the baseline), or None when nothing would be left.
    pub fn without_new_files(
        &self,
        originals: &HashMap<PathBuf, Option<Vec<u8>>>,
        root: &Path,
    ) -> Option<String> {
        if !self.is_scoped() {
            return Some(self.command.clone());
        }
        let kept: Vec<String> = self
            .targets
            .iter()
            .filter(|(_, f)| {
                f.as_ref().is_none_or(|f| {
                    !matches!(
                        originals.get(&crate::permissions::normalize(&root.join(f))),
                        Some(None)
                    )
                })
            })
            .map(|(t, _)| t.clone())
            .collect();
        (!kept.is_empty()).then(|| self.render(&kept))
    }
}

/// What the done-gate runs, and why it runs the full suite when it does.
#[derive(Debug, Clone)]
pub struct Plan {
    pub runs: Vec<Run>,
    /// Why the full suite runs although the scope is "related".
    pub note: Option<String>,
}

/// A file changed this turn; `new`: it didn't exist when the turn started.
#[derive(Debug, Clone)]
pub struct Changed {
    pub path: PathBuf,
    pub new: bool,
}

/// Shared test or build setup: a change here can affect any test.
fn is_infra(rel: &Path) -> bool {
    let name = rel
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let s = rel.to_string_lossy().replace('\\', "/");
    matches!(
        name.as_str(),
        "conftest.py" | "pytest.ini" | "tox.ini" | "setup.cfg" | "setup.py" | "pyproject.toml" | "noxfile.py" | "pipfile" | "pipfile.lock" | "poetry.lock"
            | "uv.lock" | "cargo.toml" | "cargo.lock" | "build.rs" | ".cargo/config.toml" | "go.mod" | "go.sum" | "go.work" | "package.json"
            | "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml" | "bun.lockb" | ".babelrc" | "makefile" | "gnumakefile" | ".coveragerc"
            | ".gitlab-ci.yml" | ".travis.yml" | "azure-pipelines.yml" | "jenkinsfile"
    ) || (name.starts_with("requirements") && name.ends_with(".txt"))
        || ["jest.config", "jest.setup", "vitest.config", "vitest.setup", "vite.config", "babel.config", "karma.conf", "tsconfig", "setuptests"]
            .iter()
            .any(|p| name.starts_with(p))
        || s.starts_with(".github/") || s.starts_with(".circleci/") || s.starts_with(".cargo/")
        // A test package's __init__ changes how its tests are collected.
        || (name == "__init__.py" && is_test_file(rel))
}

/// Documentation: no test exercises it.
fn is_doc(rel: &Path) -> bool {
    let ext = rel
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let first = rel
        .components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .unwrap_or("");
    let in_tests = rel.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some("tests" | "test" | "__tests__" | "testdata" | "fixtures")
        )
    });
    matches!(ext.as_str(), "md" | "rst" | "adoc")
        || matches!(first, "doc" | "docs" | "changelog" | "changelog.d")
        || (rel.components().count() == 1 && (ext.is_empty() || ext == "txt") && !in_tests)
}

/// Count test files, stopping once there are more than `stop_after`.
fn count_test_files(root: &Path, stop_after: usize) -> usize {
    let mut n = 0;
    for e in ignore::WalkBuilder::new(root)
        .max_depth(Some(12))
        .hidden(true)
        .filter_entry(|e| !skip_dir(e.path()))
        .build()
        .flatten()
    {
        if e.file_type().is_some_and(|t| t.is_file()) && is_test_file(e.path()) {
            n += 1;
            if n > stop_after {
                break;
            }
        }
    }
    n
}

/// Directories that hold dependencies or build output, not the project's code.
fn skip_dir(p: &Path) -> bool {
    matches!(
        p.file_name().and_then(|n| n.to_str()),
        Some(
            "node_modules"
                | "target"
                | ".chanto"
                | "__pycache__"
                | ".venv"
                | "venv"
                | "site-packages"
                | ".tox"
                | "dist"
                | "build"
                | "vendor"
        )
    )
}

/// Few enough tests that running all of them is the simple, cheap choice.
pub fn small_project(root: &Path) -> bool {
    count_test_files(root, SMALL_PROJECT_TESTS) <= SMALL_PROJECT_TESTS
}

/// Files under `root` modified at or after `since`, or None when there are
/// too many files to look through.
pub fn modified_since(root: &Path, since: std::time::SystemTime) -> Option<Vec<PathBuf>> {
    let mut out = Vec::new();
    for (i, e) in ignore::WalkBuilder::new(root)
        .hidden(true)
        .filter_entry(|e| !skip_dir(e.path()))
        .build()
        .flatten()
        .enumerate()
    {
        if i > 200_000 {
            return None;
        }
        if !e.file_type().is_some_and(|t| t.is_file())
            || e.path().extension().is_some_and(|x| x == "pyc")
        {
            continue;
        }
        if e.metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .is_some_and(|m| m >= since)
        {
            out.push(e.into_path());
        }
    }
    Some(out)
}

/// `path` relative to `root`, when it is inside it.
fn relative(root: &Path, path: &Path) -> Option<PathBuf> {
    if path.is_relative() {
        return Some(path.to_path_buf());
    }
    if let Ok(r) = path.strip_prefix(root) {
        return Some(r.to_path_buf());
    }
    let (root, path) = (
        crate::permissions::normalize(root),
        crate::permissions::normalize(path),
    );
    path.strip_prefix(&root).ok().map(Path::to_path_buf)
}

/// Decide what the done-gate runs. `changed`: the files changed this turn
/// (None: unknown). `fast_full`: the full suite is cheap enough to just run.
pub fn plan(
    checks: &[String],
    root: &Path,
    changed: Option<&[Changed]>,
    scope: Scope,
    fast_full: bool,
) -> Plan {
    let full = |note: Option<String>| Plan {
        runs: checks.iter().map(|c| Run::full(c)).collect(),
        note,
    };
    if scope == Scope::Full || checks.is_empty() {
        return full(None);
    }
    let Some(changed) = changed else {
        return full(Some("couldn't tell which files changed".into()));
    };
    let mut rel: Vec<Changed> = changed
        .iter()
        .filter_map(|c| relative(root, &c.path).map(|path| Changed { path, new: c.new }))
        .collect();
    rel.sort_by(|a, b| a.path.cmp(&b.path));
    rel.dedup_by(|a, b| a.path == b.path);
    // Nothing changed: the check of a turn that claims to be done without edits.
    if rel.is_empty() || fast_full {
        return full(None);
    }
    if rel.len() > MAX_CHANGED {
        return full(Some(format!("{} files changed", rel.len())));
    }
    if let Some(c) = rel.iter().find(|c| is_infra(&c.path)) {
        return full(Some(format!("`{}` changed", c.path.display())));
    }
    let code: Vec<Changed> = rel.into_iter().filter(|c| !is_doc(&c.path)).collect();
    if code.is_empty() {
        return full(Some("only documentation changed".into()));
    }
    let mut note = None;
    let runs = checks
        .iter()
        .map(|c| match related(c, root, &code) {
            Ok(Some(r)) => r,
            Ok(None) => Run::full(c),
            Err(why) => {
                note.get_or_insert(why);
                Run::full(c)
            }
        })
        .collect();
    Plan { runs, note }
}

/// Where the current versions are kept while `run_on_originals` has the
/// originals on disk, one file per project under `dir`.
pub fn put_back_journal(dir: &Path, root: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let h: String = Sha256::digest(root.to_string_lossy().as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    dir.join("putback").join(format!("{h}.json"))
}

#[derive(Serialize, Deserialize)]
struct Journal {
    root: PathBuf,
    /// Path and base64 contents (None: the file didn't exist).
    files: Vec<(PathBuf, Option<String>)>,
}

fn put_back(files: &[(PathBuf, Option<Vec<u8>>)]) {
    for (p, now) in files {
        let _ = match now {
            Some(b) => std::fs::write(p, b),
            None => std::fs::remove_file(p),
        };
    }
}

/// Put back the changes left aside by `run_on_originals` runs under `root`
/// that never finished (Chanto was killed mid-run), from journals in `dir`.
/// Returns the files restored.
pub fn recover_put_backs(dir: &Path, root: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir.join("putback")) else {
        return Vec::new();
    };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter(|p| {
            std::fs::read(p)
                .ok()
                .and_then(|b| serde_json::from_slice::<Journal>(&b).ok())
                .is_some_and(|j| j.root.starts_with(root))
        })
        .flat_map(|p| recover_put_back(&p))
        .collect()
}

/// Put back the current versions saved in one journal. Returns the files.
pub fn recover_put_back(journal: &Path) -> Vec<PathBuf> {
    use base64::Engine;
    let Some(j) = std::fs::read(journal)
        .ok()
        .and_then(|b| serde_json::from_slice::<Journal>(&b).ok())
    else {
        return Vec::new();
    };
    let files: Vec<(PathBuf, Option<Vec<u8>>)> = j
        .files
        .into_iter()
        .filter_map(|(p, b)| match b {
            Some(b) => base64::engine::general_purpose::STANDARD
                .decode(b)
                .ok()
                .map(|b| (p, Some(b))),
            None => Some((p, None)),
        })
        .collect();
    put_back(&files);
    let _ = std::fs::remove_file(journal);
    files.into_iter().map(|(p, _)| p).collect()
}

/// Run `cmd` with the files in `originals` back as they were before this turn
/// (None: the file didn't exist), then put the current versions back, also
/// if this is cancelled. The current versions are saved to `journal` first,
/// so `recover_put_back` can restore them if Chanto is killed meanwhile.
/// None when the journal or an original couldn't be written.
pub async fn run_on_originals(
    cmd: &str,
    cwd: &Path,
    timeout: Duration,
    originals: &HashMap<PathBuf, Option<Vec<u8>>>,
    journal: &Path,
) -> Option<CheckResult> {
    use base64::Engine;
    struct PutBack(
        Vec<(PathBuf, Option<Vec<u8>>)>,
        PathBuf,
        Option<std::process::Child>,
    );
    impl Drop for PutBack {
        fn drop(&mut self) {
            put_back(&self.0);
            let _ = std::fs::remove_file(&self.1);
            // The guard exits once the journal is gone.
            if let Some(mut g) = self.2.take() {
                let _ = g.wait();
            }
        }
    }
    let now: Vec<(PathBuf, Option<Vec<u8>>)> = originals
        .keys()
        .map(|p| (p.clone(), std::fs::read(p).ok()))
        .collect();
    let j = Journal {
        root: cwd.to_path_buf(),
        files: now
            .iter()
            .map(|(p, b)| {
                (
                    p.clone(),
                    b.as_ref()
                        .map(|b| base64::engine::general_purpose::STANDARD.encode(b)),
                )
            })
            .collect(),
    };
    std::fs::create_dir_all(journal.parent()?).ok()?;
    let tmp = journal.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(&j).ok()?).ok()?;
    std::fs::rename(&tmp, journal).ok()?;
    // A kill that can't be caught (SIGKILL, a closed terminal, a CI timeout)
    // would otherwise leave the originals on disk until Chanto next starts here.
    let guard = spawn_guard(journal);
    let _put_back = PutBack(now, journal.to_path_buf(), guard);
    for (p, orig) in originals {
        match orig {
            Some(b) => {
                if let Some(d) = p.parent() {
                    std::fs::create_dir_all(d).ok()?;
                }
                std::fs::write(p, b).ok()?;
            }
            None => {
                if p.exists() {
                    std::fs::remove_file(p).ok()?;
                }
            }
        }
    }
    Some(run_check(cmd, cwd, timeout).await)
}

/// argv[1] of the process that watches a put-back journal (see [`guard`]).
pub const GUARD_FLAG: &str = "--internal-put-back-guard";

/// Start [`guard`] for `journal` in its own process group, so killing
/// Chanto's group doesn't take it along.
fn spawn_guard(journal: &Path) -> Option<std::process::Child> {
    let exe = std::env::current_exe().ok()?;
    // Under `cargo test` the current exe is the test runner.
    if cfg!(test) {
        return None;
    }
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(GUARD_FLAG)
        .arg(journal)
        .arg(std::process::id().to_string());
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB
        cmd.creation_flags(0x0000_0200 | 0x0000_0008 | 0x0100_0000);
    }
    cmd.spawn().ok()
}

/// Watch `journal` while process `pid` has the originals on disk: when the
/// journal goes away the run finished normally; when `pid` dies first, put
/// the agent's changes back. Returns whether it put anything back.
pub fn guard(journal: &Path, pid: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};
    let p = Pid::from_u32(pid);
    let mut sys = System::new();
    loop {
        if !journal.exists() {
            return false;
        }
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[p]),
            true,
            ProcessRefreshKind::nothing(),
        );
        // A killed process stays a zombie until reaped; it's dead all the same.
        if sys
            .process(p)
            .is_none_or(|x| x.status() == ProcessStatus::Zombie)
        {
            return !recover_put_back(journal).is_empty();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Run every check once, before the agent changes anything.
pub async fn baseline(checks: Vec<String>, cwd: PathBuf, timeout: Duration) -> Vec<CheckResult> {
    let mut out = Vec::new();
    for c in checks {
        out.push(run_check(&c, &cwd, timeout).await);
    }
    out
}

/// Run the done-gate. Runs whose command is in `vouched` already passed in the
/// agent's own run with no edits since, so they aren't run again (the test
/// lock still is). A run covered by one of `reuse` — the agent's own run this
/// turn that *failed* — is used as-is instead of repeated, then judged by the
/// same pass/fail and pre-existing-failure logic as a fresh run. Also returns
/// every check that ran, for timing.
pub async fn gate(
    plan: &Plan,
    vouched: &[String],
    reuse: &[CheckResult],
    snap: Option<&Snapshot>,
    cwd: &Path,
    timeout: Duration,
) -> (Gate, Vec<CheckResult>) {
    let mut problems = Vec::new();
    if let Some(s) = snap {
        let v = s.violations();
        if !v.is_empty() {
            problems.push(format!(
                "Existing tests were weakened:\n{}\nRestore them: fix the code so the original tests pass instead of changing the tests. \
                 (If the user explicitly asked you to change these tests, say so in your final answer.)",
                v.iter().map(|l| format!("- {l}")).collect::<Vec<_>>().join("\n")
            ));
        }
    }
    let mut passed = Vec::new();
    let mut failed = None;
    let mut ran = Vec::new();
    let mut scoped = false;
    let mut note = plan.note.clone();
    for run in &plan.runs {
        if vouched.contains(&run.command) {
            passed.push(format!("`{}` (your run, no edits since)", run.command));
            scoped |= run.is_scoped();
            continue;
        }
        let mut r = match reuse
            .iter()
            .find(|a| covers(&a.command, &run.command) || covers(&a.command, &run.full))
        {
            Some(a) => a.clone(),
            None => run_check(&run.command, cwd, timeout).await,
        };
        ran.push(r.clone());
        // A selection that hung says nothing about whether the full suite works,
        // and running the full suite would only take longer.
        if run.is_scoped()
            && !timed_out(&r)
            && (ran_no_tests(&r.output) || (!r.ok && selection_broken(&r.output, run)))
        {
            // The selection itself didn't work: verify with everything.
            note.get_or_insert_with(|| "the related tests couldn't be run on their own".into());
            r = run_check(&run.full, cwd, timeout).await;
            ran.push(r.clone());
        } else {
            scoped |= run.is_scoped();
        }
        if r.ok {
            passed.push(format!("`{}` ({:.0}s)", r.command, r.secs));
        } else if ran_no_tests(&r.output) {
            // Says nothing about the code: the check found no tests to run.
            passed.push(format!("`{}` (found no tests; not counted)", r.command));
        } else {
            problems.push(format!(
                "`{}` failed:\n```\n{}\n```",
                r.command,
                crate::util::truncate_middle(r.output.trim(), 4000)
            ));
            failed = Some(r);
            break;
        }
    }
    let gate = if problems.is_empty() && plan.runs.is_empty() {
        // Nothing ran: intact tests alone say nothing about the change.
        Gate::NoTests("no test command was found in this project".into())
    } else if problems.is_empty()
        && !passed.is_empty()
        && passed
            .iter()
            .all(|p| p.ends_with("(found no tests; not counted)"))
    {
        Gate::NoTests(
            passed
                .iter()
                .map(|p| p.trim_end_matches(" (found no tests; not counted)"))
                .collect::<Vec<_>>()
                .join(", ")
                + " found no tests to run",
        )
    } else if problems.is_empty() {
        Gate::Passed(if scoped {
            passed.join(", ") + " passed — related to your changes; full suite not run"
        } else if let Some(n) = note {
            format!("{} passed (full suite: {n})", passed.join(", "))
        } else {
            passed.join(", ") + " passed"
        })
    } else {
        Gate::Failed(problems.join("\n\n"), failed)
    };
    (gate, ran)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn checks_do_not_wait_for_daemons_holding_their_output() {
        let d = std::env::temp_dir();
        // The background `sleep` inherits stdout and stderr and keeps them open.
        let t0 = Instant::now();
        let r = run_check(
            "echo started; sleep 30 & echo to-stderr >&2; exit 3",
            &d,
            Duration::from_secs(20),
        )
        .await;
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "waited for the daemon: {:?}",
            t0.elapsed()
        );
        assert!(!r.ok && !timed_out(&r), "{r:?}");
        assert!(
            r.output.contains("started") && r.output.contains("to-stderr"),
            "{r:?}"
        );
        // Output written just after exit by a straggler still arrives within the grace period.
        let r = run_check(
            "(sleep 0.05; echo late) & exit 0",
            &d,
            Duration::from_secs(20),
        )
        .await;
        assert!(r.ok && r.output.contains("late"), "{r:?}");
        // A main process that outlives the limit is still timed out.
        let r = run_check("sleep 30", &d, Duration::from_millis(300)).await;
        assert!(timed_out(&r), "{r:?}");
    }

    #[test]
    fn covers_only_plain_full_runs_of_the_same_runner() {
        assert!(covers("cargo test", "cargo test --quiet"));
        assert!(covers(
            "cd /x/proj && cargo test --quiet 2>&1",
            "cargo test --quiet"
        ));
        assert!(covers("python3 -m pytest -q", "python3 -m pytest -q -rfE"));
        assert!(covers(
            "go vet ./... && go test ./...",
            "go vet ./... && go test ./..."
        ));
        assert!(
            !covers("cargo test 2>&1 | tail -5", "cargo test --quiet"),
            "a pipe hides the exit status"
        );
        assert!(
            !covers("cargo test parse_", "cargo test --quiet"),
            "filtered to some tests"
        );
        assert!(!covers(
            "python3 -m pytest -k login",
            "python3 -m pytest -q -rfE"
        ));
        assert!(
            !covers("go test ./pkg/...", "go vet ./... && go test ./..."),
            "different scope, and go vet not run"
        );
        assert!(!covers("cargo test || true", "cargo test"));
        assert!(!covers("npm test", "cargo test"));
        assert!(covers(
            "python3 -m unittest discover -q -p '*test*.py'",
            "python3 -m unittest discover -q -p '*test*.py'"
        ));
        assert!(
            !covers("cargo test -p core", "cargo test"),
            "one package only"
        );
    }

    #[test]
    fn names_failing_tests_and_splits_pre_existing() {
        let py =
            "FAILED tests/test_a.py::test_one - assert 1 == 2\nFAILED tests/test_b.py::test_two\n";
        assert_eq!(failing_tests(py).unwrap().len(), 2);
        assert!(
            failing_tests("test parse::empty ... FAILED\n")
                .unwrap()
                .contains("parse::empty")
        );
        assert!(
            failing_tests("    --- FAIL: TestLRU (0.00s)\n")
                .unwrap()
                .contains("TestLRU")
        );
        assert!(
            failing_tests("FAIL: test_add (test_calc.T)\n")
                .unwrap()
                .contains("test_add (test_calc.T)")
        );
        assert!(failing_tests("all good").is_none());
        assert!(
            ran_no_tests("----\nRan 0 tests in 0.000s\n\nNO TESTS RAN\n")
                && ran_no_tests("collected 0 items")
                && !ran_no_tests("FAILED (failures=1)")
        );
        let r = |out: &str, ok: bool| CheckResult {
            command: "pytest".into(),
            ok,
            output: out.into(),
            secs: 0.0,
        };
        let before = vec![r("FAILED t.py::old\n", false)];
        let (new, old) =
            pre_existing(&r("FAILED t.py::old\nFAILED t.py::mine\n", false), &before).unwrap();
        assert_eq!(
            (new, old),
            (
                vec!["t.py::mine".to_string()],
                vec!["t.py::old".to_string()]
            )
        );
        assert!(
            pre_existing(&r("FAILED t.py::x\n", false), &[r("", true)]).is_none(),
            "passed before: all new"
        );
        // Output that names no tests: the check itself is what was already failing.
        assert_eq!(
            pre_existing(&r("build broke", false), &[r("build broke", false)]),
            Some((vec![], vec!["`pytest`".to_string()]))
        );
    }

    #[test]
    fn detects_test_tampering() {
        let d =
            std::env::temp_dir().join(format!("chanto-guard-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(d.join("tests")).unwrap();
        let t = d.join("test_math.py");
        std::fs::write(
            &t,
            "def test_a():\n    assert add(1, 2) == 3\n    assert add(0, 0) == 0\n",
        )
        .unwrap();
        std::fs::write(
            d.join("tests/b_test.go"),
            "func TestB(t *testing.T) { if f() != 2 { t.Fatal(\"x\") } }\n",
        )
        .unwrap();
        std::fs::write(d.join("math.py"), "x").unwrap();
        let s = snapshot(&d);
        assert_eq!(s.files.len(), 2);
        assert!(s.violations().is_empty());
        std::fs::write(
            &t,
            "import pytest\n@pytest.mark.skip\ndef test_a():\n    assert add(1, 2) == 3\n",
        )
        .unwrap();
        let v = s.violations().join("\n");
        assert!(
            v.contains("1 assertion(s) removed") && v.contains("skip marker"),
            "{v}"
        );
        std::fs::write(
            &t,
            "def test_a():\n    assert add(1, 2) == 4\n    assert add(0, 0) == 0\n",
        )
        .unwrap();
        assert!(s.violations().join("\n").contains("changed"));
        std::fs::remove_file(d.join("tests/b_test.go")).unwrap();
        assert!(s.violations().join("\n").contains("deleted"));
        // Adding a new test file or new tests is fine.
        std::fs::write(&t, "def test_a():\n    assert add(1, 2) == 3\n    assert add(0, 0) == 0\n\ndef test_c():\n    assert add(2, 2) == 4\n").unwrap();
        std::fs::write(
            d.join("tests/b_test.go"),
            "func TestB(t *testing.T) { if f() != 2 { t.Fatal(\"x\") } }\n",
        )
        .unwrap();
        std::fs::write(d.join("test_new.py"), "def test_n():\n    assert True\n").unwrap();
        assert!(s.violations().is_empty(), "{:?}", s.violations());
    }

    #[test]
    fn changing_an_existing_test_body_is_a_violation() {
        let d = std::env::temp_dir().join(format!(
            "chanto-guard-body-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&d).unwrap();
        let t = d.join("test_mod.py");
        let before = "def test_a():\n    assert f(1) == 2\n\n\ndef test_b():\n    data = load(\"a b\")\n    assert parse(data) == [\"a\", \"b\"]\n";
        std::fs::write(&t, before).unwrap();
        let s = snapshot(&d);
        // The same assertion lines, fed different input: the old checks can't
        // see it, but the test no longer exercises what it used to.
        std::fs::write(
            &t,
            "def test_a():\n    assert f(1) == 2\n\n\ndef test_b():\n    data = load(\"a-b\")\n    assert parse(data) == [\"a\", \"b\"]\n",
        )
        .unwrap();
        let v = s.violations().join("\n");
        assert!(v.contains("test_b (existing test) changed"), "{v}");
        // A pure reformat is not a change.
        std::fs::write(
            &t,
            "def test_a():\n\n    assert f(1) == 2\n\n\ndef test_b():\n    data = load(\"a b\")\n    assert parse(data) == [\"a\", \"b\"]\n",
        )
        .unwrap();
        assert!(s.violations().is_empty(), "{:?}", s.violations());
        // Adding tests alongside, and removing one outright.
        std::fs::write(
            &t,
            "def test_a():\n    assert f(1) == 2\n\n\ndef test_b():\n    data = load(\"a b\")\n    assert parse(data) == [\"a\", \"b\"]\n\n\ndef test_c():\n    assert True\n",
        )
        .unwrap();
        assert!(s.violations().is_empty(), "{:?}", s.violations());
        std::fs::write(&t, "def test_c():\n    assert True\n").unwrap();
        let v = s.violations().join("\n");
        assert!(
            v.contains("test_a (existing test) removed")
                && v.contains("test_b (existing test) removed"),
            "{v}"
        );
    }

    #[test]
    fn collection_errors_still_fail_the_gate() {
        // pytest prints "no tests ran" when collection itself breaks: a genuine
        // failure, not a benign empty run, so it must not pass the gate.
        let err = "ERROR collecting test_x.py\nImportError: no module named y\n\
                   !!!!!!!! Interrupted: 1 error during collection !!!!!!!!\nno tests ran in 0.01s\n";
        assert!(!ran_no_tests(err), "a collection error is a real failure");
        assert!(!ran_no_tests(
            "Traceback (most recent call last):\n  ...\ncollected 0 items in 0.01s\n"
        ));
        assert!(!ran_no_tests(
            "cannot import name 'x'\nno tests ran in 0.02s\n"
        ));
        // A genuinely empty run (nothing to collect) is benign.
        assert!(ran_no_tests("collected 0 items in 0.01s"));
        assert!(ran_no_tests("Ran 0 tests in 0.000s\n\nNO TESTS RAN\n"));
    }

    #[test]
    fn protects_inline_rust_tests_in_source_files() {
        let d = std::env::temp_dir().join(format!(
            "chanto-guard-inline-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(d.join("src")).unwrap();
        let f = d.join("src/foo.rs");
        let good = "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn \
                    adds() {\n        assert_eq!(super::add(1, 2), 3);\n        assert_eq!(super::add(0, 0), 0);\n    }\n}\n";
        std::fs::write(&f, good).unwrap();
        let s = snapshot(&d);
        assert!(
            s.files.contains_key(&f),
            "inline tests in src/*.rs must be snapshotted"
        );
        assert!(s.violations().is_empty());
        // Weakening an inline test is caught just like a separate test file.
        let weak = good.replace("        assert_eq!(super::add(0, 0), 0);\n", "");
        std::fs::write(&f, weak).unwrap();
        let v = s.violations().join("\n");
        assert!(v.contains("assertion(s) removed"), "{v}");
        // A plain source file with no tests stays out of the snapshot.
        let plain = d.join("src/plain.rs");
        std::fs::write(&plain, "pub const X: i32 = 1;\n").unwrap();
        assert!(!snapshot(&d).files.contains_key(&plain));
    }

    fn repo(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "chanto-scope-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        for (p, t) in files {
            let p = d.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, t).unwrap();
        }
        d
    }

    fn changed(paths: &[&str]) -> Vec<Changed> {
        paths
            .iter()
            .map(|p| Changed {
                path: PathBuf::from(p),
                new: false,
            })
            .collect()
    }

    fn commands(p: &Plan) -> Vec<String> {
        p.runs.iter().map(|r| r.command.clone()).collect()
    }

    const PYTEST: &str = "python3 -m pytest -q -rfE";

    fn python_repo() -> PathBuf {
        repo(
            "py",
            &[
                ("pkg/__init__.py", ""),
                ("pkg/mod.py", "def f():\n    return 1\n"),
                ("pkg/other.py", "from pkg.mod import f\n"),
                ("pkg/lonely.py", "X = 1\n"),
                ("tests/test_mod.py", "def test_f():\n    assert True\n"),
                (
                    "tests/test_uses.py",
                    "from pkg import (\n    other,\n    mod,\n)\n",
                ),
                ("tests/test_dotted.py", "import pkg.mod as m\n"),
                ("tests/test_unrelated.py", "from pkg.other import f\n"),
                ("tests/conftest.py", ""),
                ("README.md", "# x\n"),
            ],
        )
    }

    #[test]
    fn python_related_tests_by_name_and_import() {
        let d = python_repo();
        let p = plan(
            &[PYTEST.into()],
            &d,
            Some(&changed(&["pkg/mod.py"])),
            Scope::Related,
            false,
        );
        assert_eq!(
            commands(&p),
            vec![format!(
                "{PYTEST} tests/test_dotted.py tests/test_mod.py tests/test_uses.py"
            )]
        );
        assert!(p.note.is_none() && p.runs[0].is_scoped());
        // A changed test file runs itself; docs don't count; absolute paths work.
        let abs = Changed {
            path: d.join("tests/test_unrelated.py"),
            new: false,
        };
        let p = plan(
            &[PYTEST.into()],
            &d,
            Some(&[
                abs,
                Changed {
                    path: "README.md".into(),
                    new: false,
                },
            ]),
            Scope::Related,
            false,
        );
        assert_eq!(
            commands(&p),
            vec![format!("{PYTEST} tests/test_unrelated.py")]
        );
        // A new script nothing imports (a reproduction) doesn't widen the selection.
        let mut c = changed(&["pkg/mod.py"]);
        c.push(Changed {
            path: "repro.py".into(),
            new: true,
        });
        assert_eq!(
            commands(&plan(&[PYTEST.into()], &d, Some(&c), Scope::Related, false)).len(),
            1
        );
        assert!(plan(&[PYTEST.into()], &d, Some(&c), Scope::Related, false).runs[0].is_scoped());
        // An existing module no test mentions: unsure, so the full suite.
        let p = plan(
            &[PYTEST.into()],
            &d,
            Some(&changed(&["pkg/lonely.py"])),
            Scope::Related,
            false,
        );
        assert_eq!(commands(&p), vec![PYTEST.to_string()]);
        assert!(
            p.note
                .as_deref()
                .unwrap()
                .contains("no tests found for `pkg/lonely.py`"),
            "{:?}",
            p.note
        );
        // A check that isn't a plain test run (or already narrowed) stays as is.
        let p = plan(
            &[
                "make test".into(),
                "python3 -m pytest tests/test_mod.py".into(),
            ],
            &d,
            Some(&changed(&["pkg/mod.py"])),
            Scope::Related,
            false,
        );
        assert_eq!(
            commands(&p),
            vec![
                "make test".to_string(),
                "python3 -m pytest tests/test_mod.py".to_string()
            ]
        );
    }

    #[test]
    fn python_related_falls_back_to_tests_named_after_the_package() {
        // pytest's own layout: `src/_pytest/mark/evaluate.py` is imported by
        // nobody in the test suite by name, but `testing/test_mark.py` covers
        // it. Without the fallback the gate runs all 2,800 tests.
        let d = repo(
            "py-pytest",
            &[
                ("src/_pytest/__init__.py", ""),
                (
                    "src/_pytest/mark/__init__.py",
                    "from .evaluate import _istrue\n",
                ),
                (
                    "src/_pytest/mark/evaluate.py",
                    "def _istrue():\n    return True\n",
                ),
                ("src/_pytest/skipping.py", "X = 1\n"),
                ("testing/test_mark.py", "from _pytest.mark import _istrue\n"),
                (
                    "testing/test_skipping.py",
                    "from _pytest.mark import _istrue\n",
                ),
                ("testing/test_terminal.py", "import pytest\n"),
            ],
        );
        let p = plan(
            &[PYTEST.into()],
            &d,
            Some(&changed(&["src/_pytest/mark/evaluate.py"])),
            Scope::Related,
            false,
        );
        // Named after the package (`test_mark.py`) and importing the package that
        // re-exports it (`test_skipping.py`) — but not the unrelated test.
        assert_eq!(
            commands(&p),
            vec![format!(
                "{PYTEST} testing/test_mark.py testing/test_skipping.py"
            )]
        );
        assert!(p.runs[0].is_scoped(), "{:?}", p.note);
        // A module whose name matches no test and no re-exporting package still
        // escalates to the full suite.
        let p = plan(
            &[PYTEST.into()],
            &d,
            Some(&changed(&["src/_pytest/never_imported.py"])),
            Scope::Related,
            false,
        );
        assert_eq!(commands(&p), vec![PYTEST.to_string()]);
    }

    #[test]
    fn shared_setup_and_wide_changes_escalate_to_the_full_suite() {
        let d = python_repo();
        for (files, why) in [
            (
                vec!["tests/conftest.py", "pkg/mod.py"],
                "`tests/conftest.py` changed",
            ),
            (vec!["pyproject.toml"], "`pyproject.toml` changed"),
            (vec![".github/workflows/ci.yml"], "ci.yml` changed"),
            (vec!["Cargo.toml"], "`Cargo.toml` changed"),
            (vec!["package.json"], "`package.json` changed"),
            (vec!["go.mod"], "`go.mod` changed"),
            (vec!["pkg/data.json"], "isn't Python"),
        ] {
            let p = plan(
                &[PYTEST.into()],
                &d,
                Some(&changed(&files)),
                Scope::Related,
                false,
            );
            assert_eq!(commands(&p), vec![PYTEST.to_string()], "{files:?}");
            assert!(
                p.note.as_deref().is_some_and(|n| n.contains(why)),
                "{files:?}: {:?}",
                p.note
            );
        }
        let many: Vec<String> = (0..21).map(|i| format!("pkg/m{i}.py")).collect();
        let p = plan(
            &[PYTEST.into()],
            &d,
            Some(&changed(
                &many.iter().map(String::as_str).collect::<Vec<_>>(),
            )),
            Scope::Related,
            false,
        );
        assert_eq!(
            (commands(&p), p.note.as_deref()),
            (vec![PYTEST.to_string()], Some("21 files changed"))
        );
        let p = plan(&[PYTEST.into()], &d, None, Scope::Related, false);
        assert_eq!(
            (commands(&p), p.note.as_deref()),
            (
                vec![PYTEST.to_string()],
                Some("couldn't tell which files changed")
            )
        );
        // Forced full, a fast full suite, or no changes: everything, nothing to explain.
        for (scope, fast, files) in [
            (Scope::Full, false, vec!["pkg/mod.py"]),
            (Scope::Related, true, vec!["pkg/mod.py"]),
            (Scope::Related, false, vec![]),
        ] {
            let p = plan(&[PYTEST.into()], &d, Some(&changed(&files)), scope, fast);
            assert_eq!((commands(&p), p.note), (vec![PYTEST.to_string()], None));
        }
        assert!(small_project(&d));
        let big: Vec<(String, &str)> = (0..=SMALL_PROJECT_TESTS)
            .map(|i| (format!("tests/test_{i}.py"), ""))
            .collect();
        assert!(!small_project(&repo(
            "big",
            &big.iter()
                .map(|(p, t)| (p.as_str(), *t))
                .collect::<Vec<_>>()
        )));
    }

    #[test]
    fn scope_config() {
        let raw: toml::Table = "enabled = true\nscope = \"full\"\n".parse().unwrap();
        let mut c = GuardrailsConfig::default();
        c.merge(raw.clone().try_into().unwrap(), &raw);
        assert_eq!(c.scope, Some(Scope::Full));
        let raw: toml::Table = "checks = [\"make test\"]\n".parse().unwrap();
        c.merge(raw.clone().try_into().unwrap(), &raw);
        assert_eq!(c.scope, Some(Scope::Full), "a layer without scope keeps it");
        assert!(
            "scope = \"partial\""
                .parse::<toml::Table>()
                .unwrap()
                .try_into::<GuardrailsConfig>()
                .is_err()
        );
        assert_eq!(
            (
                Scope::parse("related"),
                Scope::parse("full"),
                Scope::parse("x")
            ),
            (Some(Scope::Related), Some(Scope::Full), None)
        );
    }

    #[test]
    fn unittest_selection_names_modules() {
        let d = python_repo();
        let check = "python3 -m unittest discover -q -p '*test*.py'";
        let p = plan(
            &[check.into()],
            &d,
            Some(&changed(&["tests/test_mod.py", "pkg/other.py"])),
            Scope::Related,
            false,
        );
        assert_eq!(
            commands(&p),
            vec![
                "python3 -m unittest -q tests.test_mod tests.test_unrelated tests.test_uses"
                    .to_string()
            ]
        );
        // A start directory changes how modules are named: leave it alone.
        let p = plan(
            &["python3 -m unittest discover -s tests".into()],
            &d,
            Some(&changed(&["pkg/mod.py"])),
            Scope::Related,
            false,
        );
        assert!(!p.runs[0].is_scoped());
    }

    #[test]
    fn cargo_workspace_selects_changed_crates_and_dependents() {
        let d = repo(
            "rs",
            &[
                ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
                ("crates/core/Cargo.toml", "[package]\nname = \"core-lib\"\n"),
                ("crates/core/src/lib.rs", ""),
                (
                    "crates/app/Cargo.toml",
                    "[package]\nname = \"app\"\n[dependencies]\ncore-lib = { path = \"../core\" }\n",
                ),
                ("crates/app/src/main.rs", ""),
                ("crates/other/Cargo.toml", "[package]\nname = \"other\"\n"),
                ("crates/other/src/lib.rs", ""),
            ],
        );
        let p = plan(
            &["cargo test --quiet".into()],
            &d,
            Some(&changed(&["crates/core/src/lib.rs"])),
            Scope::Related,
            false,
        );
        assert_eq!(
            commands(&p),
            vec!["cargo test --quiet -p app -p core-lib".to_string()]
        );
        let p = plan(
            &["cargo test --quiet".into()],
            &d,
            Some(&changed(&["crates/other/src/lib.rs"])),
            Scope::Related,
            false,
        );
        assert_eq!(
            commands(&p),
            vec!["cargo test --quiet -p other".to_string()]
        );
        // A single crate: its whole suite is the related one.
        let single = repo(
            "rs1",
            &[
                ("Cargo.toml", "[package]\nname = \"one\"\n"),
                ("src/lib.rs", ""),
            ],
        );
        let p = plan(
            &["cargo test --quiet".into()],
            &single,
            Some(&changed(&["src/lib.rs"])),
            Scope::Related,
            false,
        );
        assert_eq!(
            (commands(&p), p.note),
            (vec!["cargo test --quiet".to_string()], None)
        );
    }

    #[test]
    fn go_selects_changed_packages_and_importers() {
        let d = repo(
            "go",
            &[
                ("go.mod", "module example.com/m\n\ngo 1.22\n"),
                ("pkg/x/x.go", "package x\n"),
                ("pkg/x/sub/s.go", "package sub\n"),
                (
                    "pkg/y/y.go",
                    "package y\n\nimport \"example.com/m/pkg/x\"\n",
                ),
                ("cmd/z/z.go", "package main\n"),
            ],
        );
        let check = "go vet ./... && go test ./...";
        let p = plan(
            &[check.into()],
            &d,
            Some(&changed(&["pkg/x/x.go"])),
            Scope::Related,
            false,
        );
        assert_eq!(
            commands(&p),
            vec!["go vet ./pkg/x/... ./pkg/y && go test ./pkg/x/... ./pkg/y".to_string()]
        );
        // The root package: `./...` is the related set already.
        let p = plan(
            &[check.into()],
            &d,
            Some(&changed(&["main.go"])),
            Scope::Related,
            false,
        );
        assert_eq!((commands(&p), p.note), (vec![check.to_string()], None));
    }

    #[test]
    fn jest_and_vitest_use_their_related_modes() {
        let jest = repo(
            "jest",
            &[
                ("package.json", r#"{"scripts": {"test": "jest --ci"}}"#),
                ("src/a.js", ""),
                ("src/a.test.js", ""),
            ],
        );
        let p = plan(
            &["npm test --silent".into()],
            &jest,
            Some(&changed(&["src/a.js"])),
            Scope::Related,
            false,
        );
        assert_eq!(
            commands(&p),
            vec!["npx jest --findRelatedTests src/a.js".to_string()]
        );
        let vitest = repo(
            "vitest",
            &[
                ("package.json", r#"{"scripts": {"test": "vitest run"}}"#),
                ("src/a.ts", ""),
            ],
        );
        let p = plan(
            &[
                "npm test --silent".into(),
                "npm run typecheck --silent".into(),
            ],
            &vitest,
            Some(&changed(&["src/a.ts"])),
            Scope::Related,
            false,
        );
        assert_eq!(
            commands(&p),
            vec![
                "npx vitest related --run src/a.ts".to_string(),
                "npm run typecheck --silent".to_string()
            ]
        );
        let mocha = repo(
            "mocha",
            &[
                ("package.json", r#"{"scripts": {"test": "mocha"}}"#),
                ("src/a.js", ""),
            ],
        );
        assert!(
            !plan(
                &["npm test --silent".into()],
                &mocha,
                Some(&changed(&["src/a.js"])),
                Scope::Related,
                false
            )
            .runs[0]
                .is_scoped()
        );
    }

    #[test]
    fn covers_counts_supersets_of_a_selection_not_subsets() {
        let sel = "python3 -m pytest -q -rfE tests/test_a.py tests/lint/test_b.py";
        assert!(covers(
            "python -m pytest tests/test_a.py tests/lint/test_b.py tests/test_c.py",
            sel
        ));
        assert!(covers("pytest tests", sel), "a directory holds its files");
        assert!(covers("python3 -m pytest", sel), "the whole suite");
        assert!(
            !covers("python3 -m pytest tests/test_a.py", sel),
            "only part of the selection"
        );
        assert!(
            !covers(
                "python3 -m pytest tests/test_a.py::test_x tests/lint/test_b.py",
                sel
            ),
            "one test of a file"
        );
        assert!(!covers("python3 -m pytest tests -k slow", sel));
        assert!(
            !covers("python3 -m pytest --collect-only tests", sel),
            "collecting isn't running"
        );
        assert!(
            !covers(
                "python3 -m pytest tests/test_a.py",
                "python3 -m pytest -q -rfE"
            ),
            "a selection doesn't cover the full suite"
        );
        assert!(covers(
            "cargo test -p a -p b -p c",
            "cargo test --quiet -p a -p b"
        ));
        assert!(covers("cargo test --workspace", "cargo test --quiet -p a"));
        assert!(!covers("cargo test -p a", "cargo test --quiet -p a -p b"));
        assert!(
            !covers("cargo test -p a -p b parse", "cargo test --quiet -p a -p b"),
            "filtered by name"
        );
        assert!(
            !covers("cargo test --no-run", "cargo test"),
            "compiling isn't testing"
        );
        assert!(covers(
            "go vet ./... && go test ./...",
            "go vet ./pkg/x/... ./pkg/y && go test ./pkg/x/... ./pkg/y"
        ));
        assert!(covers(
            "go vet ./pkg/... && go test ./pkg/...",
            "go vet ./pkg/x/... ./pkg/y && go test ./pkg/x/... ./pkg/y"
        ));
        assert!(
            !covers(
                "go test ./pkg/x/...",
                "go vet ./pkg/x/... && go test ./pkg/x/..."
            ),
            "go vet not run"
        );
        assert!(!covers(
            "go vet ./pkg/x/... && go test ./pkg/x/...",
            "go vet ./pkg/x/... ./pkg/y && go test ./pkg/x/... ./pkg/y"
        ));
        assert!(covers(
            "python3 -m unittest tests.test_a tests.test_b",
            "python3 -m unittest -q tests.test_a"
        ));
        assert!(!covers(
            "python3 -m unittest tests.test_a.T.test_x",
            "python3 -m unittest -q tests.test_a"
        ));
        assert!(covers("npx jest", "npx jest --findRelatedTests src/a.js"));
        assert!(covers(
            "npx jest --findRelatedTests src/a.js src/b.js",
            "npx jest --findRelatedTests src/a.js"
        ));
        assert!(!covers(
            "npx jest src/b.test.js",
            "npx jest --findRelatedTests src/a.js"
        ));
        assert!(covers(
            "npx vitest run",
            "npx vitest related --run src/a.ts"
        ));
        assert!(!covers(
            "npx vitest related --run src/a.ts",
            "npx vitest run"
        ));
    }

    #[test]
    fn the_guard_puts_changes_back_when_chanto_dies_mid_run() {
        use base64::Engine;
        let d = repo("guard", &[("a.txt", "original\n")]);
        let journal = put_back_journal(&d.join(".journal"), &d);
        let edited = base64::engine::general_purpose::STANDARD.encode("edited\n");
        let write_journal = || {
            std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
            std::fs::write(
                &journal,
                serde_json::to_vec(&Journal {
                    root: d.clone(),
                    files: vec![(d.join("a.txt"), Some(edited.clone()))],
                })
                .unwrap(),
            )
            .unwrap();
        };
        // The owner dies (killed, reaped) with the originals on disk.
        write_journal();
        let mut owner = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = owner.id();
        owner.kill().unwrap();
        owner.wait().unwrap();
        assert!(guard(&journal, pid));
        assert_eq!(
            std::fs::read_to_string(d.join("a.txt")).unwrap(),
            "edited\n"
        );
        assert!(!journal.exists());
        // The owner finishes normally: it removes the journal and the guard leaves the files alone.
        std::fs::write(d.join("a.txt"), "kept\n").unwrap();
        write_journal();
        let j = journal.clone();
        let done = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            std::fs::remove_file(j).unwrap();
        });
        assert!(!guard(&journal, std::process::id()));
        done.join().unwrap();
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "kept\n");
    }

    #[test]
    fn old_pytest_failures_are_named_and_matched_to_the_baseline() {
        // Older pytest prints no "FAILED x::y" summary, only section headers.
        let out = |names: &[&str]| {
            format!(
                "....F.F\n=== FAILURES ===\n{}\n2 failed, 5 passed",
                names
                    .iter()
                    .map(|n| format!("______ {n} ______\nassert 0"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        let before = CheckResult {
            command: "python3 -m pytest -q -rfE".into(),
            ok: false,
            output: out(&["test_code_highlight[with markup]"]),
            secs: 1.0,
        };
        let now = CheckResult {
            output: out(&["test_code_highlight[with markup]", "test_new_thing"]),
            ..before.clone()
        };
        let (new, old) = pre_existing(&now, &[before]).unwrap();
        assert_eq!(new, ["test_new_thing"]);
        assert_eq!(old, ["test_code_highlight[with markup]"]);
    }

    #[tokio::test]
    async fn baseline_runs_on_the_original_files_and_puts_changes_back() {
        let d = repo("orig", &[("a.txt", "changed\n"), ("new.txt", "new\n")]);
        let mut originals = HashMap::new();
        originals.insert(d.join("a.txt"), Some(b"original\n".to_vec()));
        originals.insert(d.join("new.txt"), None);
        originals.insert(d.join("gone.txt"), Some(b"was here\n".to_vec()));
        let journal = put_back_journal(&d.join(".journal"), &d);
        let r = run_on_originals(
            "cat a.txt gone.txt; test ! -e new.txt",
            &d,
            Duration::from_secs(30),
            &originals,
            &journal,
        )
        .await
        .unwrap();
        assert!(
            r.ok && r.output.contains("original") && r.output.contains("was here"),
            "{}",
            r.output
        );
        assert_eq!(
            std::fs::read_to_string(d.join("a.txt")).unwrap(),
            "changed\n"
        );
        assert_eq!(std::fs::read_to_string(d.join("new.txt")).unwrap(), "new\n");
        assert!(!d.join("gone.txt").exists());
        assert!(
            !journal.exists(),
            "journal removed once the changes are back"
        );
        // Killed mid-run: the journal is still there and puts the changes back.
        let run = tokio::spawn({
            let (d, originals, journal) = (d.clone(), originals.clone(), journal.clone());
            async move {
                run_on_originals(
                    "sleep 30",
                    &d,
                    Duration::from_secs(60),
                    &originals,
                    &journal,
                )
                .await
            }
        });
        for _ in 0..100 {
            if std::fs::read_to_string(d.join("a.txt")).unwrap() == "original\n" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            std::fs::read_to_string(d.join("a.txt")).unwrap(),
            "original\n"
        );
        let saved = std::fs::read(&journal).unwrap();
        run.abort();
        let _ = run.await;
        // Simulate a kill: originals back on disk, journal left behind.
        std::fs::write(d.join("a.txt"), "original\n").unwrap();
        std::fs::remove_file(d.join("new.txt")).unwrap();
        std::fs::write(d.join("gone.txt"), "was here\n").unwrap();
        std::fs::write(&journal, saved).unwrap();
        assert!(
            recover_put_backs(&d.join(".journal"), &d.join("elsewhere")).is_empty(),
            "other projects' journals are left alone"
        );
        assert_eq!(recover_put_backs(&d.join(".journal"), &d).len(), 3);
        assert_eq!(
            std::fs::read_to_string(d.join("a.txt")).unwrap(),
            "changed\n"
        );
        assert_eq!(std::fs::read_to_string(d.join("new.txt")).unwrap(), "new\n");
        assert!(!d.join("gone.txt").exists() && !journal.exists());
        assert!(recover_put_back(&journal).is_empty());
        // The baseline selection leaves out test files that are new this turn.
        let py = python_repo();
        let p = plan(
            &[PYTEST.into()],
            &py,
            Some(&changed(&["tests/test_mod.py", "tests/test_unrelated.py"])),
            Scope::Related,
            false,
        );
        let mut originals = HashMap::new();
        originals.insert(
            crate::permissions::normalize(&py.join("tests/test_unrelated.py")),
            None,
        );
        assert_eq!(
            p.runs[0].without_new_files(&originals, &py).unwrap(),
            format!("{PYTEST} tests/test_mod.py")
        );
    }

    #[tokio::test]
    async fn gate_reports_the_scope_it_verified() {
        let d = repo(
            "gate",
            &[
                ("calc.py", "def add(a, b):\n    return a + b\n"),
                (
                    "tests/test_calc.py",
                    "import unittest\nfrom calc import add\nclass T(unittest.TestCase):\n    def test_add(self):\n        self.assertEqual(add(2, 3), 5)\n",
                ),
                (
                    "tests/test_other.py",
                    "import unittest\nclass T(unittest.TestCase):\n    def test_x(self):\n        self.assertTrue(True)\n",
                ),
                // A package, so `unittest discover` from the root finds the tests.
                ("tests/__init__.py", ""),
            ],
        );
        let check = "python3 -m unittest discover -q -p 'test_*.py'".to_string();
        let t = Duration::from_secs(60);
        let p = plan(
            std::slice::from_ref(&check),
            &d,
            Some(&changed(&["calc.py"])),
            Scope::Related,
            false,
        );
        assert_eq!(
            commands(&p),
            vec!["python3 -m unittest -q tests.test_calc".to_string()]
        );
        let (Gate::Passed(e), ran) = gate(&p, &[], &[], None, &d, t).await else {
            panic!("gate failed")
        };
        assert!(
            e.starts_with("`python3 -m unittest -q tests.test_calc` (")
                && e.ends_with("passed — related to your changes; full suite not run"),
            "{e}"
        );
        assert_eq!(ran.len(), 1);
        let (Gate::Passed(e), _) = gate(&p, &[p.runs[0].command.clone()], &[], None, &d, t).await
        else {
            panic!()
        };
        assert!(
            e.contains("(your run, no edits since)") && e.contains("full suite not run"),
            "{e}"
        );
        // Escalated: says why the full suite ran.
        let p = plan(
            std::slice::from_ref(&check),
            &d,
            Some(&changed(&["tox.ini"])),
            Scope::Related,
            false,
        );
        let (Gate::Passed(e), _) = gate(&p, &[], &[], None, &d, t).await else {
            panic!()
        };
        assert!(
            e.contains(&format!("`{check}`"))
                && e.ends_with("passed (full suite: `tox.ini` changed)"),
            "{e}"
        );
        // A selection that can't run on its own falls back to the full suite.
        let mut p = plan(
            std::slice::from_ref(&check),
            &d,
            Some(&changed(&["calc.py"])),
            Scope::Related,
            false,
        );
        p.runs[0] = Run::scoped(
            &check,
            vec!["python3 -m unittest -q".into()],
            vec![(
                "tests.test_missing".into(),
                Some("tests/test_missing.py".into()),
            )],
            false,
        );
        let (Gate::Passed(e), ran) = gate(&p, &[], &[], None, &d, t).await else {
            panic!()
        };
        assert_eq!(ran.len(), 2);
        assert!(
            e.contains(&format!("`{check}`")) && e.contains("couldn't be run on their own"),
            "{e}"
        );
        // A check that runs no tests at all verifies nothing.
        let empty = repo(
            "gate-empty",
            &[("calc.py", "def add(a, b):\n    return a + b\n")],
        );
        let p = plan(
            std::slice::from_ref(&check),
            &empty,
            Some(&changed(&["calc.py"])),
            Scope::Full,
            false,
        );
        let (Gate::NoTests(why), _) = gate(&p, &[], &[], None, &empty, t).await else {
            panic!("a run of no tests passed the gate")
        };
        assert_eq!(why, format!("`{check}` found no tests to run"));
    }

    #[tokio::test]
    async fn gate_reuses_the_agent_own_failing_run() {
        let d = repo(
            "gate-reuse",
            &[
                (
                    "tests/test_x.py",
                    "import unittest\nclass T(unittest.TestCase):\n    def test_ok(self):\n        self.assertTrue(True)\n",
                ),
                ("tests/__init__.py", ""),
            ],
        );
        let check = "python3 -m unittest discover -q -p 'test_*.py'".to_string();
        let t = Duration::from_secs(60);
        let p = plan(
            std::slice::from_ref(&check),
            &d,
            Some(&changed(&["x.py"])),
            Scope::Full,
            false,
        );
        // The agent already ran this check and it failed with this output. The
        // gate must judge that run, not repeat the suite (here the suite would
        // pass, so a repeated run is observable as a Passed gate).
        let mine = CheckResult {
            command: check.clone(),
            ok: false,
            output: "FAILED tests/test_x.py::test_broke - AssertionError\n".into(),
            secs: 0.0,
        };
        let (g, ran) = gate(&p, &[], std::slice::from_ref(&mine), None, &d, t).await;
        let Gate::Failed(feedback, failed) = g else {
            panic!("the reused failure didn't fail the gate")
        };
        assert!(feedback.contains("test_broke"), "{feedback}");
        assert_eq!(failed.map(|f| f.command), Some(check.clone()));
        assert_eq!(ran.len(), 1);
        assert_eq!(ran[0].command, check);
        // With nothing to reuse the gate runs the check itself, and it passes.
        let (g, ran) = gate(&p, &[], &[], None, &d, t).await;
        assert!(matches!(g, Gate::Passed(_)), "fresh run should pass");
        assert_eq!(ran.len(), 1);
    }

    #[test]
    fn test_file_patterns() {
        for p in [
            "test_x.py",
            "x_test.go",
            "a.test.ts",
            "b.spec.js",
            "tests/integration.rs",
            "src/__tests__/a.js",
            "FooTest.java",
            // pytest's suite lives under `testing/`, not `tests/`.
            "testing/python/fixtures.py",
            "test_utils/helpers.rb",
        ] {
            assert!(is_test_file(Path::new(p)), "{p}");
        }
        for p in [
            "src/main.rs",
            "testing.md",
            "contest.py",
            "latest.go",
            "src/attestation.py",
        ] {
            assert!(!is_test_file(Path::new(p)), "{p}");
        }
    }
}
