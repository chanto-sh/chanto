//! Outcome mode: review what was built, not the code.
//!
//! 1. Spec: the agent explores (read-only) and writes executable acceptance
//!    criteria to `.chanto/outcome/<id>/criteria.json` (plus check scripts).
//! 2. Baseline: every criterion runs once before any work, so one that
//!    already passes is flagged (it proves nothing about the change).
//! 3. Approval: the user approves the criteria, not code.
//! 4. Build: verified mode with the criteria as checks; the criteria files
//!    are locked (editing them fails the gate).
//! 5. Report: each criterion with its evidence, plus risk flags for changes a
//!    human should still read (auth, migrations, dependencies, secrets, ...).

use crate::agent::{Agent, TurnEnd};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Criterion {
    pub id: String,
    /// What the user cares about, in product terms.
    pub title: String,
    /// Shell command that exits 0 when the criterion holds.
    pub check: String,
}

pub fn outcome_dir(root: &Path, id: &str) -> PathBuf {
    root.join(".chanto").join("outcome").join(id)
}

pub(crate) fn spec_prompt(intent: &str, dir: &Path) -> String {
    format!(
        r#"Outcome mode, step 1 of 2: write acceptance criteria. Do NOT implement anything yet.

The user wants: {intent}

Explore the codebase (read-only) to learn how it's built, run and tested. Then define how we will KNOW the result is right, as executable acceptance criteria the user can approve without reading code:
- Write them to {dir}/criteria.json as a JSON array: [{{"id": "c1", "title": "<user-visible behavior, plain language>", "check": "<shell command run from the project root; exit 0 = criterion met>"}}].
- Put any test or helper scripts the checks need under {dir}/ (e.g. {dir}/checks/test_signup.py, or a script that starts the server and curls it). Only write files inside {dir}/ in this step.
- Cover the main behavior, the important edge cases and error handling, and anything the user explicitly asked for. 3-10 criteria; each check fast and deterministic (no network beyond localhost).
- Checks for new behavior must FAIL right now (nothing is built yet) and pass once it is correct. Don't write criteria that pass trivially.

Finish with a short summary of the criteria."#,
        dir = dir.display()
    )
}

fn build_prompt(intent: &str, dir: &Path, criteria: &[Criterion]) -> String {
    let list: String = criteria
        .iter()
        .map(|c| format!("- {} {}: `{}`\n", c.id, c.title, c.check))
        .collect();
    format!(
        "Outcome mode, step 2 of 2: build it.\n\nThe user wants: {intent}\n\nThe user approved these acceptance criteria (in {}):\n{list}\n\
         Implement the change so every criterion passes, following the codebase's conventions, and keep the project's existing tests passing. \
         The criteria and their scripts are locked: don't edit anything under {}. Run the checks yourself before finishing.",
        dir.display(),
        dir.display()
    )
}

pub fn load(dir: &Path) -> Result<Vec<Criterion>> {
    let text = std::fs::read_to_string(dir.join("criteria.json"))
        .map_err(|_| anyhow::anyhow!("the agent didn't write {}/criteria.json", dir.display()))?;
    let v: Vec<Criterion> = serde_json::from_str(crate::llm::repair_json(&text).trim())?;
    if v.is_empty() {
        bail!("criteria.json is empty");
    }
    Ok(v)
}

pub struct CheckRun {
    pub criterion: Criterion,
    pub ok: bool,
    pub output: String,
    pub secs: f32,
}

pub async fn run_all(criteria: &[Criterion], root: &Path) -> Vec<CheckRun> {
    let mut out = Vec::new();
    for c in criteria {
        let r = crate::guardrails::run_check(&c.check, root, Duration::from_secs(300)).await;
        out.push(CheckRun {
            criterion: c.clone(),
            ok: r.ok,
            output: r.output,
            secs: r.secs,
        });
    }
    out
}

/// A changed file a human should still read, and why.
#[derive(Debug, PartialEq)]
pub struct Risk {
    pub file: String,
    pub reason: &'static str,
}

/// Flag changes that verification can't vouch for.
pub fn risks(changed: &[(String, char)], diff: &str) -> Vec<Risk> {
    let path_rules: &[(&str, &'static str)] = &[
        (
            r"(?i)(auth|login|session|token|passw|secret|crypt|permission|acl|rbac|oauth|jwt|security)",
            "auth / security code",
        ),
        (
            r"(?i)(migrat|schema\.|\.sql$|alembic|prisma/)",
            "database schema or migration",
        ),
        (
            r"(?i)(^|/)(package\.json|package-lock\.json|pnpm-lock\.yaml|yarn\.lock|cargo\.toml|cargo\.lock|go\.mod|go\.sum|requirements[^/]*\.txt|pyproject\.toml|poetry\.lock|gemfile)$",
            "dependencies changed",
        ),
        (
            r"(?i)(^|/)\.env|credentials|\.pem$|\.key$",
            "secrets / credentials",
        ),
        (
            r"(?i)(^|/)(\.github/workflows|\.gitlab-ci|dockerfile|docker-compose|terraform|\.tf$|k8s|helm)",
            "CI / infrastructure",
        ),
        (
            r"(?i)(^|/)(api|routes?|handlers?|controllers?|openapi|swagger|proto)(/|\.|$)|\.proto$",
            "public API surface",
        ),
    ];
    let path_re: Vec<(regex::Regex, &'static str)> = path_rules
        .iter()
        .map(|(re, why)| (regex::Regex::new(re).unwrap(), *why))
        .collect();
    let mut out = Vec::new();
    for (file, status) in changed {
        if file.starts_with(".chanto/") {
            continue;
        }
        if *status == 'D' {
            out.push(Risk {
                file: file.clone(),
                reason: "file deleted",
            });
        }
        for (re, why) in &path_re {
            if re.is_match(file) {
                out.push(Risk {
                    file: file.clone(),
                    reason: why,
                });
            }
        }
    }
    // Risky constructs in added lines, attributed to their file.
    let content_rules: &[(&str, &'static str)] = &[
        (
            r"(?i)\b(eval\(|exec\(|os\.system|subprocess|shell\s*=\s*true|child_process|Command::new)",
            "runs commands / dynamic code",
        ),
        (
            r"(?i)\b(drop\s+table|delete\s+from|truncate\s+table|rm\s+-rf|remove_dir_all|shutil\.rmtree)",
            "destructive data operations",
        ),
        (
            r"(?i)\b(unsafe\s*\{|Mutex|RwLock|thread::spawn|goroutine|go func|asyncio\.gather|threading\.|Promise\.all)",
            "concurrency",
        ),
        (
            r#"(?i)(innerHTML|dangerouslySetInnerHTML|format!\(.*select|f"select|\+\s*"select|execute\(\s*f)"#,
            "injection-prone code",
        ),
    ];
    let content_re: Vec<(regex::Regex, &'static str)> = content_rules
        .iter()
        .map(|(re, why)| (regex::Regex::new(re).unwrap(), *why))
        .collect();
    let mut file = String::new();
    for line in diff.lines() {
        if let Some(f) = line.strip_prefix("+++ b/") {
            file = f.to_string();
            continue;
        }
        if !line.starts_with('+') || line.starts_with("+++") || file.starts_with(".chanto/") {
            continue;
        }
        for (re, why) in &content_re {
            if re.is_match(line)
                && !out
                    .iter()
                    .any(|r: &Risk| r.file == file && r.reason == *why)
            {
                out.push(Risk {
                    file: file.clone(),
                    reason: why,
                });
            }
        }
    }
    out
}

fn git(root: &Path, args: &[&str]) -> String {
    std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

/// Files changed since `base` (committed or not, plus untracked), with status letters.
fn changed_files(root: &Path, base: &str) -> (Vec<(String, char)>, String) {
    let mut files: Vec<(String, char)> = git(root, &["diff", "--name-status", base])
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let st = it.next()?.chars().next()?;
            Some((it.last()?.to_string(), st))
        })
        .collect();
    for f in git(root, &["ls-files", "--others", "--exclude-standard"]).lines() {
        files.push((f.to_string(), 'A'));
    }
    let mut diff = git(root, &["diff", base]);
    // Untracked files' content counts as added lines.
    for (f, st) in &files {
        if *st == 'A'
            && !diff.contains(&format!("+++ b/{f}"))
            && let Ok(t) = std::fs::read_to_string(root.join(f))
        {
            diff.push_str(&format!("\n+++ b/{f}\n"));
            for l in t.lines().take(2000) {
                diff.push_str(&format!("+{l}\n"));
            }
        }
    }
    (files, diff)
}

fn report(
    intent: &str,
    runs: &[CheckRun],
    baseline: &[CheckRun],
    risks: &[Risk],
    files: &[(String, char)],
    verdict: &str,
) -> String {
    let passed = runs.iter().filter(|r| r.ok).count();
    let mut s = format!(
        "# Outcome report\n\n**Asked for:** {intent}\n\n**Result:** {passed}/{} acceptance criteria pass. {verdict}\n\n## Acceptance criteria\n",
        runs.len()
    );
    for r in runs {
        let pre = baseline
            .iter()
            .find(|b| b.criterion.id == r.criterion.id)
            .map(|b| b.ok)
            .unwrap_or(false);
        s.push_str(&format!(
            "\n### {} {} {}\n`{}` ({:.1}s){}\n```\n{}\n```\n",
            if r.ok { "✅" } else { "❌" },
            r.criterion.id,
            r.criterion.title,
            r.criterion.check,
            r.secs,
            if pre {
                " · ⚠ also passed before the change, so it doesn't prove this work"
            } else {
                ""
            },
            crate::util::truncate_middle(r.output.trim(), 1200)
        ));
    }
    s.push_str("\n## Review these by hand\n");
    if risks.is_empty() {
        s.push_str("Nothing flagged: no auth, schema, dependency, secret, infrastructure or public-API changes, and no risky constructs found.\n");
    } else {
        s.push_str("Passing checks can't vouch for these:\n");
        for r in risks {
            s.push_str(&format!("- `{}`: {}\n", r.file, r.reason));
        }
    }
    let n = files
        .iter()
        .filter(|(f, _)| !f.starts_with(".chanto/"))
        .count();
    s.push_str(&format!(
        "\n{n} file(s) changed (`/diff` to see the code).\n"
    ));
    s
}

/// Run the whole flow. Returns the report (also saved as report.md).
pub async fn run(agent: &mut Agent, intent: &str, cancel: CancellationToken) -> Result<String> {
    let id = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    run_with_id(agent, intent, &id, cancel).await
}

pub async fn run_with_id(
    agent: &mut Agent,
    intent: &str,
    id: &str,
    cancel: CancellationToken,
) -> Result<String> {
    let env = agent.env.clone();
    let root = env.root.clone();
    let dir = outcome_dir(&root, id);
    std::fs::create_dir_all(&dir)?;
    let base = git(&root, &["rev-parse", "HEAD"]).trim().to_string();
    let base = if base.is_empty() {
        "HEAD".to_string()
    } else {
        base
    };

    // 1. Spec (plan mode: read-only except the outcome directory).
    let prev_mode = env.mode();
    env.set_mode(crate::permissions::Mode::Plan);
    let end = agent
        .run_turn(&spec_prompt(intent, &dir), vec![], cancel.clone())
        .await;
    env.set_mode(prev_mode);
    if matches!(end?, TurnEnd::Interrupted) {
        bail!("interrupted while writing criteria");
    }
    let criteria = load(&dir)?;

    // 2. Baseline.
    env.emit(crate::agent::Event::Busy(Some(
        "Running criteria before any change…".into(),
    )));
    let baseline = run_all(&criteria, &root).await;
    env.emit(crate::agent::Event::Busy(None));

    // 3. Approval.
    let mut q = format!("**Acceptance criteria** for: {intent}\n\n");
    for b in &baseline {
        q.push_str(&format!(
            "- **{}** {}\n  `{}`{}\n",
            b.criterion.id,
            b.criterion.title,
            b.criterion.check,
            if b.ok { "  ⚠ already passes" } else { "" }
        ));
    }
    q.push_str("\nApprove these? The agent will build until all of them pass; you review this list and the evidence, not the code.");
    let opts = vec!["Approve and build".to_string(), "Cancel".to_string()];
    let approved = if env.interactive {
        matches!(
            env.frontend.choose(&q, &opts).await.as_deref(),
            Some("Approve and build")
        )
    } else {
        env.emit(crate::agent::Event::Info(format!(
            "{q}\n(non-interactive: approved automatically)"
        )));
        true
    };
    if !approved {
        return Ok(format!(
            "Cancelled. The criteria are in `{}`; edit them and run `/outcome` again, or build from them yourself.",
            dir.display()
        ));
    }

    // 4. Build under verified mode, with the criteria as checks and locked.
    let checks: Vec<String> = criteria.iter().map(|c| c.check.clone()).collect();
    let end = agent
        .run_turn_verified(
            &build_prompt(intent, &dir, &criteria),
            checks,
            vec![dir.clone()],
            cancel.clone(),
        )
        .await?;
    let verdict = match &end {
        TurnEnd::Done(t) if t.contains("Not verified") => {
            "⚠ The agent stopped before every check passed."
        }
        TurnEnd::Done(_) => "",
        TurnEnd::Interrupted => "⚠ Interrupted before finishing.",
    };

    // 5. Evidence + risks.
    let runs = run_all(&criteria, &root).await;
    let (files, diff) = changed_files(&root, &base);
    let risks = risks(&files, &diff);
    let text = report(intent, &runs, &baseline, &risks, &files, verdict);
    std::fs::write(dir.join("report.md"), &text)?;
    Ok(format!(
        "{text}\n_Saved to `{}`._",
        dir.join("report.md").display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_risky_changes() {
        let files = vec![
            ("src/auth/login.py".to_string(), 'M'),
            ("migrations/0003_users.sql".to_string(), 'A'),
            ("package.json".to_string(), 'M'),
            ("old.py".to_string(), 'D'),
            ("src/util.py".to_string(), 'M'),
            (".chanto/outcome/x/criteria.json".to_string(), 'A'),
        ];
        let diff = "+++ b/src/util.py\n+import subprocess\n+subprocess.run(cmd, shell=True)\n+++ b/src/db.py\n+cur.execute(f\"SELECT * FROM t WHERE id={x}\")\n";
        let r = risks(&files, diff);
        let has = |f: &str, why: &str| r.iter().any(|x| x.file == f && x.reason == why);
        assert!(has("src/auth/login.py", "auth / security code"));
        assert!(has(
            "migrations/0003_users.sql",
            "database schema or migration"
        ));
        assert!(has("package.json", "dependencies changed"));
        assert!(has("old.py", "file deleted"));
        assert!(has("src/util.py", "runs commands / dynamic code"));
        assert!(has("src/db.py", "injection-prone code"));
        assert!(!r.iter().any(|x| x.file.starts_with(".chanto/")));
    }
}
