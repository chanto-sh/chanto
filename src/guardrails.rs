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

mod policy;
mod select;
pub use policy::{script_command, set_policy};
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
    /// Checks added after `checks` (or the detected ones): the quality
    /// profile's pattern rules (`chanto quality check`).
    pub quality_checks: Vec<String>,
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
        if !o.quality_checks.is_empty() {
            self.quality_checks = o.quality_checks;
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
        let pytest = policy::pytest_python();
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
    // Outside one, it runs as a Bash command does: sandboxed, without secrets.
    let mut lease = None;
    let (mut c, token) = match &container {
        Some(ct) => {
            let (c, t) = ct.command(cmd, cwd, &env);
            (c, Some(t))
        }
        None => match policy::check_command(cmd, cwd, &env) {
            Ok((c, l)) => {
                lease = l;
                (c, None)
            }
            Err(e) => {
                return CheckResult {
                    command: cmd.to_string(),
                    ok: false,
                    output: format!("could not run: {e:#}"),
                    secs: t0.elapsed().as_secs_f32(),
                };
            }
        },
    };
    c.kill_on_drop(true);
    let r = run_detached_output(c, timeout).await;
    drop(lease);
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

/// The agent's command as run from `root`, given the directory `cwd` it ran
/// in: its `cd` steps are resolved and dropped, and every other step must run
/// in `root` itself. `None` when a step runs elsewhere (a subdirectory's run
/// only stands for that part of the project) or a `cd` can't be resolved.
pub fn at_root(agent_cmd: &str, cwd: &Path, root: &Path) -> Option<String> {
    let mut dir = cwd.to_path_buf();
    let mut kept = Vec::new();
    for seg in agent_cmd.split("&&") {
        let words = shell_words::split(seg.trim()).ok()?;
        if words.first().map(String::as_str) == Some("cd") {
            let [_, to] = words.as_slice() else {
                return None;
            };
            if to.starts_with(['~', '-']) || to.contains('$') {
                return None;
            }
            dir = lexical(&dir.join(to));
            continue;
        }
        if !same_dir(&dir, root) {
            return None;
        }
        kept.push(seg.trim());
    }
    (!kept.is_empty()).then(|| kept.join(" && "))
}

/// `p` with `.` and `..` resolved without touching the file system.
fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

fn same_dir(a: &Path, b: &Path) -> bool {
    lexical(a) == lexical(b)
        || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// Does a passing run of `agent_cmd` prove that `check` passes? Only a plain run
/// of the same test runner over at least the same scope counts: no pipes
/// (`| tail` hides the exit status), no `||`, no backgrounding, no filters to
/// part of the suite. A run of more files or packages than `check` names
/// counts; a run of fewer doesn't. A `cd` can't be resolved here, so a command
/// that has one doesn't count: [`at_root`] resolves it against the directory
/// the command ran in first.
pub fn covers(agent_cmd: &str, check: &str) -> bool {
    if agent_cmd.contains('|') || agent_cmd.contains('`') || agent_cmd.contains("$(") {
        return false;
    }
    if agent_cmd
        .split("&&")
        .any(|seg| seg.split_whitespace().next() == Some("cd"))
    {
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
    use crate::procs::State;
    loop {
        if !journal.exists() {
            return false;
        }
        match crate::procs::state(pid) {
            // A killed process stays a zombie until reaped; it's dead all the same.
            Some(State::Gone | State::Zombie) => return !recover_put_back(journal).is_empty(),
            Some(State::Running) => {}
            // Without a process table there is no telling whether `pid` died,
            // and putting files back under a live run would undo its work.
            None => return false,
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
mod tests;
