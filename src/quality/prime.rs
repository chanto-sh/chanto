//! `chanto prime`: find the project's checks, run each once as a baseline,
//! and write `.chanto/quality.toml` so verified mode runs them every turn.
//! Checks that fail now are written turned off (with the reason) rather than
//! left to fail every turn; checks slower than [`SLOW`] are kept out of the
//! gate for scheduled CI.

use super::*;
use std::io::IsTerminal as _;
use std::time::Duration;

/// Slower than this, a check is written `when = "nightly"`.
const SLOW: f32 = 300.0;
const TIMEOUT: Duration = Duration::from_secs(600);

const USAGE: &str = "Usage: chanto prime [--yes] [--dry-run] [--force] | --hook

Find this project's checks (tests, lint, typecheck, format, secrets), run
each once, and write .chanto/quality.toml so verified mode runs them on
every change.

  --yes      write without asking
  --dry-run  print the profile instead of writing it
  --force    replace an existing .chanto/quality.toml
  --hook     only install a git pre-push hook that runs the profile's checks";

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub name: String,
    pub run: String,
    pub kind: &'static str,
}

pub async fn cli(args: &[String]) -> Result<String> {
    let (mut yes, mut dry, mut force) = (false, false, false);
    for a in args {
        match a.as_str() {
            "--yes" | "-y" => yes = true,
            "--dry-run" => dry = true,
            "--force" => force = true,
            "--hook" => return super::hook::install(&std::env::current_dir()?),
            "-h" | "--help" | "help" => return Ok(USAGE.into()),
            other => anyhow::bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }
    let root = std::env::current_dir()?;
    if !crate::config::is_trusted(&root) {
        anyhow::bail!(
            "This folder isn't trusted yet, and priming runs its checks. Start `chanto` here once and trust the folder, then run `chanto prime` again."
        );
    }
    let target = path(&root);
    if target.exists() && !force && !dry {
        anyhow::bail!(
            "{} already exists. `chanto prime --force` replaces it.",
            target.display()
        );
    }
    // Checks run as the gate runs them: under this project's sandbox and
    // secrets settings.
    let (cfg, _) = crate::config::Config::load(&root, true)?;
    crate::guardrails::set_policy(&cfg.sandbox, &cfg.secrets);

    let candidates = discover(&root);
    let mut runs = Vec::new();
    for c in &candidates {
        eprintln!("Running {} …", c.run);
        let r = crate::guardrails::run_check(&c.run, &root, TIMEOUT).await;
        eprintln!(
            "  {} in {:.1}s",
            if r.ok {
                "passed"
            } else if crate::guardrails::timed_out(&r) {
                "timed out"
            } else {
                "failed"
            },
            r.secs
        );
        runs.push((r.ok, r.secs));
    }
    let text = render(&candidates, &runs);
    if dry {
        return Ok(text);
    }
    if !yes {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return Ok(format!(
                "{text}\nNot written: run `chanto prime --yes` to write {}.",
                target.display()
            ));
        }
        println!("{text}");
        print!("Write {}? [Y/n] ", target.display());
        use std::io::Write as _;
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if matches!(answer.trim().to_ascii_lowercase().as_str(), "n" | "no") {
            return Ok("Not written.".into());
        }
    }
    std::fs::create_dir_all(target.parent().unwrap_or(&root))?;
    std::fs::write(&target, &text)?;
    Ok(summary(&candidates, &runs, &target))
}

/// Check commands this project declares, most useful first.
pub fn discover(root: &Path) -> Vec<Candidate> {
    let has = crate::shell::command_exists;
    let mut out: Vec<Candidate> = crate::guardrails::detect_checks(root)
        .into_iter()
        .map(|run| Candidate {
            name: name_for(&run),
            kind: if run.contains("typecheck") {
                "typecheck"
            } else {
                "test"
            },
            run,
        })
        .collect();
    let mut add = |name: &str, run: &str, kind: &'static str| {
        if !out.iter().any(|c| c.run == run) {
            out.push(Candidate {
                name: name.into(),
                run: run.into(),
                kind,
            });
        }
    };
    if root.join("Cargo.toml").exists() {
        if has("cargo-clippy") {
            add(
                "clippy",
                "cargo clippy --workspace --all-targets --quiet -- -D warnings",
                "lint",
            );
        }
        if has("cargo-fmt") {
            add("rustfmt", "cargo fmt --all --check", "format");
        }
    }
    if let Ok(pkg) = std::fs::read_to_string(root.join("package.json")) {
        let v: serde_json::Value = serde_json::from_str(&pkg).unwrap_or_default();
        if v["scripts"]["lint"].is_string() {
            add("lint", "npm run lint --silent", "lint");
        }
    }
    let pyproject = std::fs::read_to_string(root.join("pyproject.toml")).unwrap_or_default();
    if has("ruff")
        && (pyproject.contains("[tool.ruff")
            || root.join("ruff.toml").exists()
            || root.join(".ruff.toml").exists())
    {
        add("ruff", "ruff check .", "lint");
    }
    if has("mypy") && (pyproject.contains("[tool.mypy") || root.join("mypy.ini").exists()) {
        add("mypy", "mypy .", "typecheck");
    }
    if has("gitleaks") {
        add(
            "gitleaks",
            "gitleaks git --pre-commit --redact --no-banner .",
            "secrets",
        );
    }
    if root.join("go.mod").exists() && has("gofmt") {
        add("gofmt", "test -z \"$(gofmt -l .)\"", "format");
    }
    out
}

fn name_for(run: &str) -> String {
    let w: Vec<&str> = run.split_whitespace().collect();
    match w.as_slice() {
        ["cargo", "test", ..] => "tests".into(),
        ["go", "vet", ..] => "go vet and tests".into(),
        ["npm", "test", ..] => "tests".into(),
        ["npm", "run", script, ..] => (*script).into(),
        ["make", target, ..] => format!("make {target}"),
        _ if run.contains("pytest") || run.contains("unittest") => "tests".into(),
        _ => w.first().copied().unwrap_or("check").into(),
    }
}

fn toml_str(s: &str) -> String {
    toml::Value::String(s.into()).to_string()
}

/// The profile text: the baseline results as comments and `when` values.
pub fn render(candidates: &[Candidate], runs: &[(bool, f32)]) -> String {
    let mut s = String::from(concat!(
        "# Chanto quality profile, written by `chanto prime`. Commit it: verified mode
# runs the `when = \"turn\"` checks before the agent may finish a change.
# Docs: ",
        site_url!(),
        "/docs/reference#quality-profile
version = 1

[gate]
enabled = true          # verified mode on in this project
scope = \"related\"       # tests related to the change; \"full\" for the whole suite
protect_tests = true
oracle = false          # true: a reviewer model checks each diff before the gate
",
    ));
    if candidates.is_empty() {
        s.push_str(
            "\n# No checks found. Add one per command that must pass, for example:\n# [[check]]\n# name = \"tests\"\n# run = \"make test\"\n",
        );
    }
    for (c, (ok, secs)) in candidates.iter().zip(runs) {
        let when = if !ok {
            "off"
        } else if *secs > SLOW {
            "nightly"
        } else {
            "turn"
        };
        s.push('\n');
        if !ok {
            s.push_str("# Failed when primed, so it's off: fix it, then set when = \"turn\".\n");
        } else if when == "nightly" {
            s.push_str("# Too slow for every turn; run it in scheduled CI.\n");
        }
        s.push_str(&format!(
            "[[check]]\nname = {}\nrun = {}\nkind = \"{}\"\nwhen = \"{when}\"\nbaseline = \"{}\"\nsecs = {:.1}\nsource = \"detected\"\n",
            toml_str(&c.name),
            toml_str(&c.run),
            c.kind,
            if *ok { "passing" } else { "failing" },
            secs
        ));
    }
    s.push_str(
        "
# Pattern rules: `chanto quality check` (also run by the gate) fails when a line
# the change adds matches `regex`. Example:
# [[rule]]
# id = \"no-debug-print\"
# title = \"No debug prints in library code\"
# regex = '\\bdbg!\\('
# paths = [\"src/**\"]
# exclude = [\"**/*_tests.rs\"]
# new_code_only = true   # false: every line of every matching file

# [review]
# no_touch = [\"vendor/**\"]   # the agent may not edit these paths

# `chanto quality check` also scans the lines a change adds for keys, tokens
# and passwords. Keep test fixtures with fake keys out of it, or turn it off:
# [secrets]
# exclude = [\"tests/fixtures/**\"]
# scan = false
",
    );
    s
}

fn summary(candidates: &[Candidate], runs: &[(bool, f32)], target: &Path) -> String {
    let on = runs
        .iter()
        .filter(|(ok, secs)| *ok && *secs <= SLOW)
        .count();
    let failing: Vec<&str> = candidates
        .iter()
        .zip(runs)
        .filter(|(_, (ok, _))| !ok)
        .map(|(c, _)| c.run.as_str())
        .collect();
    let mut s = format!(
        "Wrote {}: {on} of {} checks run on every change; verified mode is on in this project.",
        target.display(),
        candidates.len()
    );
    if !failing.is_empty() {
        s.push_str(&format!(
            "\nFailing now, so turned off: {}. Fix them, then set when = \"turn\".",
            failing.join(", ")
        ));
    }
    s.push_str(
        "\nCommit it so everyone (and CI) gets the same checks: git add .chanto/quality.toml",
    );
    s.push_str("\n`chanto prime --hook` adds a pre-push hook that runs them before each push.");
    s
}
