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
mod tests {
    use super::*;

    #[test]
    fn patterns_match_globs_and_extensions() {
        let p = Path::new("src/app/main.rs");
        assert!(
            matches("rs", p)
                && matches(".rs", p)
                && matches("*.rs", p)
                && matches("src/**/*.rs", p)
                && matches("*.{rs,toml}", p)
        );
        assert!(!matches("py", p) && !matches("tests/*.rs", p) && !matches("", p));
        let cfg = FileCommands {
            commands: BTreeMap::from([
                ("*.rs".into(), "a".into()),
                ("src/app/*.rs".into(), "b".into()),
                ("py".into(), "c".into()),
            ]),
            ..Default::default()
        };
        assert_eq!(
            configured(&cfg, p).as_deref(),
            Some("b"),
            "the most specific pattern wins"
        );
        assert_eq!(configured(&cfg, Path::new("x.py")).as_deref(), Some("c"));
        assert_eq!(configured(&cfg, Path::new("x.go")), None);
    }

    #[test]
    fn command_lines_place_the_file() {
        use crate::shell::Flavor::*;
        assert_eq!(
            command_line("rustfmt", "src/a b.rs", Posix),
            "rustfmt 'src/a b.rs'"
        );
        assert_eq!(
            command_line("fmt {file} --check", "a.rs", Posix),
            "fmt a.rs --check"
        );
        // Nothing in a file name expands, whatever the shell.
        let evil = r"src\$(calc) %PATH% `id`'.rs";
        assert_eq!(
            command_line("fmt", evil, Posix),
            format!("fmt '{}'", evil.replace('\'', r"'\''"))
        );
        assert_eq!(
            command_line("fmt", evil, PowerShell),
            r"fmt 'src\$(calc) %PATH% `id`''.rs'"
        );
        assert_eq!(
            command_line("fmt {file} --check", evil, Cmd),
            r#"fmt "%CHANTO_FILE%" --check"#
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_names_are_not_interpreted_by_the_shell() {
        let d = std::env::temp_dir().join(format!(
            "chanto-fmt-quote-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&d).unwrap();
        let name = "$(touch pwned) `touch pwned2` 'q'.txt";
        std::fs::write(d.join(name), "x").unwrap();
        let cfg = FileCommands {
            commands: BTreeMap::from([(
                "txt".into(),
                "cat {file} && printf %s \"$CHANTO_FILE\"".into(),
            )]),
            ..Default::default()
        };
        let cmd = command_for(&cfg, Stage::Format, &d, &d.join(name)).unwrap();
        let r = run(&cmd, &d, Path::new(name), 10).await;
        assert!(r.ok && r.output == format!("x{name}"), "{r:?}");
        assert!(!d.join("pwned").exists() && !d.join("pwned2").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn detection_needs_the_project_to_use_the_tool() {
        let d = std::env::temp_dir().join(format!(
            "chanto-fmt-detect-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&d).unwrap();
        let all = |_: &str| true;
        let none = |_: &str| false;
        let f = |n: &str| d.join(n);
        assert_eq!(
            detect(Stage::Format, &d, &f("a.rs"), &all),
            None,
            "no Cargo.toml"
        );
        std::fs::write(
            d.join("Cargo.toml"),
            "[package]\nname = \"x\"\nedition = \"2024\"\n",
        )
        .unwrap();
        assert_eq!(
            detect(Stage::Format, &d, &f("a.rs"), &all).as_deref(),
            Some("rustfmt --edition 2024")
        );
        assert_eq!(
            detect(Stage::Format, &d, &f("a.rs"), &none),
            None,
            "rustfmt not installed"
        );
        assert_eq!(
            detect(Stage::Format, &d, &f("a.py"), &all),
            None,
            "no Python tool configured"
        );
        std::fs::write(
            d.join("pyproject.toml"),
            "[tool.black]\nline-length = 100\n",
        )
        .unwrap();
        assert_eq!(
            detect(Stage::Format, &d, &f("a.py"), &all).as_deref(),
            Some("black --quiet")
        );
        std::fs::write(d.join("pyproject.toml"), "[tool.ruff]\n").unwrap();
        assert_eq!(
            detect(Stage::Format, &d, &f("a.py"), &all).as_deref(),
            Some("ruff format --quiet")
        );
        assert_eq!(
            detect(Stage::Lint, &d, &f("a.py"), &all).as_deref(),
            Some("ruff check --quiet")
        );
        assert_eq!(detect(Stage::Format, &d, &f("a.ts"), &all), None);
        std::fs::write(
            d.join("package.json"),
            r#"{"devDependencies": {"prettier": "^3", "eslint": "^9"}}"#,
        )
        .unwrap();
        assert_eq!(
            detect(Stage::Format, &d, &f("a.ts"), &all).as_deref(),
            Some("npx --no-install prettier --write --log-level warn")
        );
        assert_eq!(
            detect(Stage::Lint, &d, &f("a.ts"), &all).as_deref(),
            Some("npx --no-install eslint")
        );
        assert_eq!(detect(Stage::Format, &d, &f("a.go"), &all), None);
        std::fs::write(d.join("go.mod"), "module x\n").unwrap();
        assert_eq!(
            detect(Stage::Format, &d, &f("a.go"), &all).as_deref(),
            Some("gofmt -w")
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edits_are_formatted_and_linted_for_the_model() {
        use crate::agent::tests::{make_env_with, mock_server, tc, tmpdir};
        use crate::agent::{Agent, TurnEnd};
        use serde_json::json;
        let dir = tmpdir("format-lint");
        let (base, seen) = mock_server(vec![
            json!({"tool_calls": tc("Write", json!({"file_path": "a.txt", "content": "hello\n"}))}),
            // The formatter's rewrite doesn't count as an outside change.
            json!({"tool_calls": tc("Edit", json!({"file_path": "a.txt", "old_string": "HELLO", "new_string": "hi"}))}),
            json!({"content": "Done."}),
        ])
        .await;
        let env = make_env_with(
            &base,
            &dir,
            crate::permissions::Mode::AcceptEdits,
            32768,
            |c| {
                c.format.commands.insert(
                    "txt".into(),
                    "tr a-z A-Z < {file} > {file}.tmp && mv {file}.tmp {file}".into(),
                );
                c.lint.commands.insert(
                    "*.txt".into(),
                    "grep -n HI {file} && exit 1 || exit 0".into(),
                );
            },
        );
        let mut agent = Agent::new(env);
        let end = agent
            .run_turn(
                "write it",
                vec![],
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(end, TurnEnd::Done(ref t) if t == "Done."));
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "HI\n");
        let reqs = seen.lock().unwrap();
        let tool_msg = |i: usize| {
            reqs[i]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .find(|m| m["role"] == "tool")
                .unwrap()["content"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let first = tool_msg(1);
        assert!(
            first.contains("a.txt was formatted with") && first.contains("+HELLO"),
            "{first}"
        );
        assert!(!first.contains("<lint"), "clean file: no lint report");
        let second = tool_msg(2);
        assert!(
            second.contains("<lint file=\"a.txt\"") && second.contains("1:HI"),
            "{second}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn layers_merge_commands() {
        let mut a = FileCommands {
            auto: Some(true),
            commands: BTreeMap::from([("rs".into(), "rustfmt".into())]),
            ..Default::default()
        };
        a.merge(FileCommands {
            timeout_secs: Some(5),
            commands: BTreeMap::from([("py".into(), "black".into())]),
            ..Default::default()
        });
        assert_eq!(
            (a.auto, a.timeout_secs, a.commands.len()),
            (Some(true), Some(5), 2)
        );
        assert!(a.on());
        a.merge(FileCommands {
            enabled: Some(false),
            ..Default::default()
        });
        assert!(!a.on());
        assert!(!FileCommands::default().on(), "off unless configured");
    }
}
