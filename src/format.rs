//! Format and lint after edits (`[format]`, `[lint]`).
//!
//! After Write, Edit, MultiEdit or apply_patch succeeds, each changed file is
//! run through its formatter (written back in place; the model is told what
//! changed, so its view of the file stays right) and then its linter (errors
//! for that file go back to the model). Commands come from the config, by
//! file glob or extension; with `auto = true`, the project's own tools are
//! detected (rustfmt, prettier, ruff/black, gofmt, clang-format; ruff and
//! eslint for lint). These are user-configured commands, treated like hooks:
//! a project can only set them once trusted (see `config::RISKY_KEYS`).

use crate::agent::Env;
use crate::tools::ToolOutput;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct FileCommands {
    /// Off switch for everything below (default on).
    pub enabled: Option<bool>,
    /// Detect the project's own tools when no command matches (default off).
    pub auto: Option<bool>,
    /// Per command time limit in seconds (default 15 to format, 60 to lint).
    pub timeout_secs: Option<u64>,
    /// File pattern → command. A pattern is a glob (`*.rs`, `src/**/*.ts`,
    /// `*.{js,ts}`) or a bare extension (`rs`, `.py`). `{file}` in the command
    /// is the file's path (quoted); without it the path is appended.
    pub commands: BTreeMap<String, String>,
}

impl FileCommands {
    pub fn merge(&mut self, o: FileCommands) {
        if o.enabled.is_some() {
            self.enabled = o.enabled;
        }
        if o.auto.is_some() {
            self.auto = o.auto;
        }
        if o.timeout_secs.is_some() {
            self.timeout_secs = o.timeout_secs;
        }
        self.commands.extend(o.commands);
    }

    fn on(&self) -> bool {
        self.enabled != Some(false) && (self.auto == Some(true) || !self.commands.is_empty())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    Format,
    Lint,
}

/// Does `pattern` (a glob or an extension) match `rel` (the path relative to the project)?
fn matches(pattern: &str, rel: &Path) -> bool {
    let p = pattern.trim();
    if p.is_empty() {
        return false;
    }
    let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if !p.contains(['*', '?', '[', '{', '/']) {
        let ext = p.trim_start_matches('.');
        return rel
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case(ext))
            || name == p;
    }
    let Ok(g) = globset::GlobBuilder::new(p)
        .literal_separator(false)
        .build()
    else {
        return false;
    };
    let m = g.compile_matcher();
    m.is_match(rel) || (!p.contains('/') && m.is_match(name))
}

/// The configured command for this file, if any (the most specific
/// pattern wins: the longest).
fn configured(cfg: &FileCommands, rel: &Path) -> Option<String> {
    cfg.commands
        .iter()
        .filter(|(pat, cmd)| !cmd.trim().is_empty() && matches(pat, rel))
        .max_by_key(|(pat, _)| pat.len())
        .map(|(_, c)| c.clone())
}

fn ext(p: &Path) -> String {
    p.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn package_json(root: &Path) -> Value {
    std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn has_node_dep(root: &Path, name: &str) -> bool {
    let v = package_json(root);
    ["devDependencies", "dependencies"]
        .iter()
        .any(|k| v[*k].get(name).is_some())
}

/// `node_modules/.bin/<tool>` when installed, else `npx --no-install <tool>`.
fn node_tool(root: &Path, tool: &str) -> String {
    if root.join("node_modules").join(".bin").join(tool).exists() {
        format!("node_modules/.bin/{tool}")
    } else {
        format!("npx --no-install {tool}")
    }
}

fn pyproject_has(root: &Path, table: &str) -> bool {
    std::fs::read_to_string(root.join("pyproject.toml"))
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok())
        .is_some_and(|t| t.get("tool").and_then(|t| t.get(table)).is_some())
}

fn ruff_configured(root: &Path) -> bool {
    root.join("ruff.toml").exists()
        || root.join(".ruff.toml").exists()
        || pyproject_has(root, "ruff")
}

/// The Rust edition from Cargo.toml (rustfmt on a single file needs it).
fn rust_edition(root: &Path) -> String {
    let t = std::fs::read_to_string(root.join("Cargo.toml"))
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok());
    let t = t.as_ref();
    let edition = |p: Option<&toml::Value>| {
        p.and_then(|p| p.get("edition"))
            .and_then(|e| e.as_str())
            .map(str::to_string)
    };
    edition(t.and_then(|t| t.get("package")))
        .or_else(|| {
            edition(
                t.and_then(|t| t.get("workspace"))
                    .and_then(|w| w.get("package")),
            )
        })
        .unwrap_or_else(|| "2021".into())
}

/// A tool the project clearly uses, for this file.
fn detect(stage: Stage, root: &Path, file: &Path, has: &dyn Fn(&str) -> bool) -> Option<String> {
    let e = ext(file);
    const PRETTIER: &[&str] = &[
        "js", "jsx", "mjs", "cjs", "ts", "tsx", "css", "scss", "less", "json", "md", "html", "vue",
        "yaml", "yml", "graphql",
    ];
    const ESLINT: &[&str] = &["js", "jsx", "mjs", "cjs", "ts", "tsx", "vue"];
    match stage {
        Stage::Format => match e.as_str() {
            "rs" if root.join("Cargo.toml").exists() && has("rustfmt") => {
                Some(format!("rustfmt --edition {}", rust_edition(root)))
            }
            "go" if root.join("go.mod").exists() && has("gofmt") => Some("gofmt -w".into()),
            "py" | "pyi" if ruff_configured(root) && has("ruff") => {
                Some("ruff format --quiet".into())
            }
            "py" | "pyi" if pyproject_has(root, "black") && has("black") => {
                Some("black --quiet".into())
            }
            "c" | "h" | "cc" | "cpp" | "hpp" | "cxx"
                if root.join(".clang-format").exists() && has("clang-format") =>
            {
                Some("clang-format -i".into())
            }
            e if PRETTIER.contains(&e) && has_node_dep(root, "prettier") => Some(format!(
                "{} --write --log-level warn",
                node_tool(root, "prettier")
            )),
            _ => None,
        },
        Stage::Lint => match e.as_str() {
            "py" | "pyi" if ruff_configured(root) && has("ruff") => {
                Some("ruff check --quiet".into())
            }
            e if ESLINT.contains(&e) && has_node_dep(root, "eslint") => {
                Some(node_tool(root, "eslint"))
            }
            _ => None,
        },
    }
}

/// The environment variable that also carries the file's path to the command.
const FILE_VAR: &str = "CHANTO_FILE";

/// The file's path as one literal word for the shell that runs the command,
/// so `$(…)`, backticks or `%VAR%` in a file name never expand. cmd.exe has
/// no quoting that stops `%` expansion, so there the path is passed in
/// `CHANTO_FILE` and referenced (expansion doesn't recurse into the value,
/// and Windows file names can't contain `"`).
fn quote(p: &str, flavor: crate::shell::Flavor) -> String {
    use crate::shell::Flavor;
    match flavor {
        Flavor::Posix => shell_words::quote(p).into_owned(),
        Flavor::PowerShell => crate::shell::powershell_quote(p),
        Flavor::Cmd => format!("\"%{FILE_VAR}%\""),
    }
}

/// The command line for one file: `{file}` replaced, or the path appended.
fn command_line(cmd: &str, rel: &str, flavor: crate::shell::Flavor) -> String {
    if cmd.contains("{file}") {
        cmd.replace("{file}", &quote(rel, flavor))
    } else {
        format!("{cmd} {}", quote(rel, flavor))
    }
}

/// The command to run for `file` at this stage.
fn command_for(cfg: &FileCommands, stage: Stage, root: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(root).ok()?;
    let cmd = configured(cfg, rel).or_else(|| {
        (cfg.auto == Some(true))
            .then(|| detect(stage, root, file, &|c| crate::shell::command_exists(c)))
            .flatten()
    })?;
    Some(command_line(
        &cmd,
        &rel.to_string_lossy(),
        crate::shell::flavor(),
    ))
}

/// Run a format or lint command with the file's path in `CHANTO_FILE`.
async fn run(cmd: &str, root: &Path, rel_path: &Path, secs: u64) -> crate::guardrails::CheckResult {
    let env = [(
        FILE_VAR.to_string(),
        rel_path.to_string_lossy().into_owned(),
    )];
    crate::guardrails::run_check_env(cmd, root, Duration::from_secs(secs), &env).await
}

/// Files a successful edit tool changed.
fn edited_paths(cwd: &Path, tool: &str, args: &Value) -> Vec<PathBuf> {
    let list: Vec<String> = match tool {
        "Write" | "Edit" | "MultiEdit" => args["file_path"]
            .as_str()
            .map(|p| vec![p.to_string()])
            .unwrap_or_default(),
        "apply_patch" => crate::tools::patch::paths(
            ["input", "patch"]
                .iter()
                .find_map(|k| args[*k].as_str())
                .unwrap_or(""),
        ),
        _ => vec![],
    };
    let mut out: Vec<PathBuf> = Vec::new();
    for p in list {
        let p = crate::util::resolve(cwd, &p);
        if p.is_file() && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// A short unified diff of what the formatter changed.
fn format_diff(before: &str, after: &str, max_lines: usize) -> Option<String> {
    let d = similar::TextDiff::from_lines(before, after);
    let text = d.unified_diff().context_radius(1).to_string();
    let lines: Vec<&str> = text.lines().collect();
    (lines.len() <= max_lines).then_some(text)
}

/// Lint output worth showing the model: trimmed and bounded.
fn lint_report(output: &str) -> String {
    let t = output.trim();
    crate::util::truncate(t, 4000)
}

/// Run after every tool: format, then lint, the files a successful edit changed.
pub async fn after_tool(
    env: &Env,
    root: &Path,
    cwd: &Path,
    tool: &str,
    args: &Value,
    out: &mut ToolOutput,
) {
    let (fmt, lint) = (&env.config.format, &env.config.lint);
    if out.is_error || !(fmt.on() || lint.on()) {
        return;
    }
    // Commands can't run where only reads are allowed, and an editor that
    // holds the file (ACP) owns its contents.
    if env.config.sandbox.read_only || env.frontend.writes_via_client() {
        return;
    }
    let paths = edited_paths(cwd, tool, args);
    if paths.is_empty() {
        return;
    }
    let root = crate::permissions::normalize(root);
    let mut notes: Vec<String> = Vec::new();
    for path in paths {
        let path = crate::permissions::normalize(&path);
        if !path.starts_with(&root) {
            continue;
        }
        let rel = crate::util::rel(&root, &path);
        if fmt.on()
            && let Some(cmd) = command_for(fmt, Stage::Format, &root, &path)
        {
            let before = std::fs::read_to_string(&path).unwrap_or_default();
            let r = run(
                &cmd,
                &root,
                path.strip_prefix(&root).unwrap_or(&path),
                fmt.timeout_secs.unwrap_or(15),
            )
            .await;
            let after = std::fs::read_to_string(&path).unwrap_or_default();
            if !r.ok {
                out.content.push_str(&format!("\n\n<system-reminder>The formatter `{cmd}` failed on {rel}, so the file was left as you wrote it:\n{}\n</system-reminder>", lint_report(&r.output)));
                notes.push(format!("\x1b[33mformatter failed on {rel}\x1b[0m"));
            } else if after != before {
                // The formatter's write is the agent's own: keep later edits allowed.
                env.mark_read(&path);
                let shown = match format_diff(&before, &after, 60) {
                        Some(d) => format!("Formatting changed it:\n```diff\n{}\n```", d.trim_end()),
                        None => "Formatting changed many lines: Read it again before editing the reformatted parts.".into(),
                    };
                out.content.push_str(&format!("\n\n<system-reminder>{rel} was formatted with `{cmd}` after your edit. {shown}\n</system-reminder>"));
                notes.push(format!("formatted {rel}"));
            }
        }
        if lint.on()
            && let Some(cmd) = command_for(lint, Stage::Lint, &root, &path)
        {
            let r = run(
                &cmd,
                &root,
                path.strip_prefix(&root).unwrap_or(&path),
                lint.timeout_secs.unwrap_or(60),
            )
            .await;
            if !r.ok {
                let report = lint_report(&r.output);
                out.content.push_str(&format!(
                    "\n\n<lint file=\"{rel}\" command=\"{cmd}\">\n{report}\n</lint>"
                ));
                let n = report.lines().filter(|l| !l.trim().is_empty()).count();
                notes.push(format!(
                    "\x1b[31mlint: {n} line{} of findings in {rel}\x1b[0m",
                    if n == 1 { "" } else { "s" }
                ));
            }
        }
    }
    if !notes.is_empty() {
        let note = notes.join("\n");
        out.display = Some(match out.display.take() {
            Some(d) => format!("{d}\n{note}"),
            None => note,
        });
    }
}

#[cfg(test)]
mod tests;
