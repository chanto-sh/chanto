//! `chanto quality check`: the profile's pattern rules. A rule with
//! `new_code_only` (the default) looks only at lines the working tree adds
//! relative to `HEAD`, plus untracked files, so existing code that breaks it
//! doesn't fail every change. Exits 1 when a rule is broken.

use super::*;
use std::collections::BTreeMap;

/// Most violations listed; the rest are counted.
const MAX_LISTED: usize = 50;
/// Files larger than this are skipped.
const MAX_FILE: u64 = 1024 * 1024;

pub struct Outcome {
    pub text: String,
    pub failed: bool,
}

pub fn cli(args: &[String]) -> Result<Outcome> {
    let mut all = false;
    for a in args {
        match a.as_str() {
            "--all" => all = true,
            "-h" | "--help" | "help" => {
                return Ok(Outcome {
                    text: "Usage: chanto quality check [--all]\n\nCheck the rules in .chanto/quality.toml against the lines this change adds (--all: whole files).".into(),
                    failed: false,
                });
            }
            other => {
                anyhow::bail!("unknown argument {other:?} (usage: chanto quality check [--all])")
            }
        }
    }
    let root = std::env::current_dir()?;
    let Some(profile) = load(&root)? else {
        anyhow::bail!(
            "No quality profile here ({}). `chanto prime` writes one.",
            path(&root).display()
        );
    };
    let changed = if all { None } else { added_lines(&root) };
    let violations = check(&profile, &root, changed.as_ref())?;
    Ok(report(&profile, &violations))
}

#[derive(Debug, PartialEq)]
pub struct Violation {
    pub rule: String,
    pub file: String,
    pub line: usize,
    pub text: String,
}

/// Lines added per file (path relative to `root`) since `HEAD`, untracked
/// files whole. None outside a git repository or before the first commit:
/// then everything counts as new.
pub fn added_lines(root: &Path) -> Option<BTreeMap<String, Vec<(usize, String)>>> {
    crate::hostgit::read(root, &["rev-parse", "--verify", "-q", "HEAD"])?;
    let mut args = vec![
        "-c",
        "core.quotePath=false",
        "diff",
        "--no-color",
        "--unified=0",
        "--relative",
    ];
    args.extend(crate::hostgit::DIFF_ARGS);
    args.extend(["HEAD", "--"]);
    let diff = crate::hostgit::read(root, &args)?;
    let mut out = parse_diff(&diff);
    let untracked =
        crate::hostgit::read(root, &["ls-files", "--others", "--exclude-standard", "-z"])
            .unwrap_or_default();
    for f in untracked.split('\0').filter(|f| !f.is_empty()) {
        if let Some(text) = read_small(&root.join(f)) {
            out.insert(
                f.to_string(),
                text.lines()
                    .enumerate()
                    .map(|(i, l)| (i + 1, l.to_string()))
                    .collect(),
            );
        }
    }
    Some(out)
}

/// Added lines from a `git diff --unified=0`.
pub fn parse_diff(diff: &str) -> BTreeMap<String, Vec<(usize, String)>> {
    let mut out: BTreeMap<String, Vec<(usize, String)>> = BTreeMap::new();
    let mut file: Option<String> = None;
    let mut next = 0usize;
    for line in diff.lines() {
        if let Some(p) = line.strip_prefix("+++ ") {
            file = p.strip_prefix("b/").map(str::to_string);
        } else if line.starts_with("@@") {
            // @@ -a,b +c,d @@
            next = line
                .split_whitespace()
                .find_map(|w| w.strip_prefix('+'))
                .and_then(|w| w.split(',').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
        } else if let (Some(f), Some(added)) = (&file, line.strip_prefix('+')) {
            out.entry(f.clone())
                .or_default()
                .push((next, added.to_string()));
            next += 1;
        }
    }
    out
}

fn read_small(p: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(p).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        return None;
    }
    std::fs::read_to_string(p).ok()
}

fn globs(patterns: &[String]) -> Result<Option<globset::GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut b = globset::GlobSetBuilder::new();
    for p in patterns {
        b.add(globset::Glob::new(p).with_context(|| format!("bad path pattern {p:?}"))?);
    }
    Ok(Some(b.build()?))
}

/// Every violation of the profile's pattern rules. `changed`: the lines a
/// change added (None: treat every line as new).
pub fn check(
    profile: &Profile,
    root: &Path,
    changed: Option<&BTreeMap<String, Vec<(usize, String)>>>,
) -> Result<Vec<Violation>> {
    let mut out = Vec::new();
    let mut files: Option<Vec<String>> = None;
    for rule in &profile.rules {
        let Some(re) = &rule.regex else { continue };
        let re = regex::Regex::new(re)
            .with_context(|| format!("rule {:?}: bad regex {re:?}", rule.id))?;
        let include = globs(&rule.paths).with_context(|| format!("rule {:?}", rule.id))?;
        let exclude = globs(&rule.exclude).with_context(|| format!("rule {:?}", rule.id))?;
        // The profile itself, and Chanto's other project files, aren't code.
        let applies = |f: &str| {
            !f.starts_with(".chanto/")
                && include.as_ref().is_none_or(|g| g.is_match(f))
                && !exclude.as_ref().is_some_and(|g| g.is_match(f))
        };
        let mut hit = |file: &str, line: usize, text: &str| {
            if re.is_match(text) {
                out.push(Violation {
                    rule: rule.id.clone(),
                    file: file.to_string(),
                    line,
                    text: text.trim().chars().take(160).collect(),
                });
            }
        };
        match changed {
            Some(changed) if rule.new_code_only => {
                for (f, lines) in changed.iter().filter(|(f, _)| applies(f)) {
                    for (n, l) in lines {
                        hit(f, *n, l);
                    }
                }
            }
            _ => {
                let files = files.get_or_insert_with(|| project_files(root));
                for f in files.iter().filter(|f| applies(f)) {
                    if let Some(text) = read_small(&root.join(f)) {
                        for (i, l) in text.lines().enumerate() {
                            hit(f, i + 1, l);
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Files in the project, respecting .gitignore, relative with `/`.
fn project_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for e in ignore::WalkBuilder::new(root)
        .hidden(true)
        .build()
        .flatten()
    {
        if e.file_type().is_some_and(|t| t.is_file())
            && let Ok(rel) = e.path().strip_prefix(root)
        {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    out.sort();
    out
}

pub fn report(profile: &Profile, violations: &[Violation]) -> Outcome {
    let rules = profile.rules.iter().filter(|r| r.regex.is_some()).count();
    if violations.is_empty() {
        return Outcome {
            text: format!(
                "Quality rules: {rules} checked, none broken.{}",
                if rules == 0 {
                    " (The profile has no rules with a regex.)"
                } else {
                    ""
                }
            ),
            failed: false,
        };
    }
    let mut text = String::new();
    let mut last = "";
    for v in violations.iter().take(MAX_LISTED) {
        if v.rule != last {
            let title = profile
                .rules
                .iter()
                .find(|r| r.id == v.rule)
                .map(|r| r.title.as_str())
                .filter(|t| !t.is_empty())
                .unwrap_or(&v.rule);
            text.push_str(&format!("✗ {} ({title})\n", v.rule));
            last = &v.rule;
        }
        text.push_str(&format!("  {}:{}: {}\n", v.file, v.line, v.text));
    }
    if violations.len() > MAX_LISTED {
        text.push_str(&format!("  … {} more\n", violations.len() - MAX_LISTED));
    }
    let broken = {
        let mut ids: Vec<&str> = violations.iter().map(|v| v.rule.as_str()).collect();
        ids.dedup();
        ids.len()
    };
    text.push_str(&format!(
        "{} {} of {broken} {} in .chanto/quality.toml. Fix the code; the rules are the project's.",
        violations.len(),
        if violations.len() == 1 {
            "violation"
        } else {
            "violations"
        },
        if broken == 1 { "rule" } else { "rules" },
    ));
    Outcome { text, failed: true }
}
