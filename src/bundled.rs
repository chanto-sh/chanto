//! Bundled skills: prompt-based commands shipped in the
//! binary (`/simplify`, `/batch`, `/debug`, `/code-review`,
//! `/fewer-permission-prompts`). Unlike built-in commands, a user, project or
//! plugin skill or command with the same name replaces one; `/review` is the
//! built-in alias that always runs the bundled `/code-review`. `/statusline`
//! lives here too since it is a prompt.

use crate::agent::{Agent, Env};
use crate::commands::{CommandInfo, Outcome};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const BUNDLED: &[CommandInfo] = &[
    CommandInfo {
        name: "simplify",
        args: "[target]",
        help: "Review the changed code for duplication, needless complexity, waste and misplaced logic, then apply the fixes (bundled skill)",
    },
    CommandInfo {
        name: "batch",
        args: "<instruction>",
        help: "Plan a large change, split it into independent units and run each in its own worktree agent in parallel (bundled skill)",
    },
    CommandInfo {
        name: "debug",
        args: "[description]",
        help: "Troubleshoot a problem: read this session's Chanto log, reproduce, find the root cause (bundled skill)",
    },
    CommandInfo {
        name: "code-review",
        args: "[low|medium|high|xhigh|max] [--fix] [--comment] [--max-findings n|all|default] [pr#|branch|path]",
        help: "Review the current diff, or a PR, branch, range or path, for correctness bugs; --fix applies the findings (bundled skill; alias /review)",
    },
    CommandInfo {
        name: "fewer-permission-prompts",
        args: "",
        help: "Scan this project's sessions for common read-only Bash and MCP calls and add an allowlist for them (bundled skill)",
    },
];

/// Run bundled skill `name`, or `None` if there is no such bundled skill.
pub async fn run(agent: &mut Agent, name: &str, arg: &str) -> Option<Outcome> {
    let env = agent.env.clone();
    Some(match name {
        "simplify" => Outcome::Prompt(simplify_prompt(arg)),
        "batch" => batch(&env, arg),
        "debug" => Outcome::Prompt(debug_prompt(agent, arg)),
        "code-review" => code_review(agent, arg).await,
        "fewer-permission-prompts" => Outcome::Prompt(fewer_prompts_prompt(&env)),
        _ => return None,
    })
}

fn target_or_diff(target: &str) -> String {
    if target.trim().is_empty() {
        "the work this branch has in flight. Find it with `git status --short`, `git diff HEAD` and \
         `git log --oneline @{upstream}..HEAD`; when there's no upstream, compare against `main` (or the \
         parent commit if the branch has neither). Untracked files count. Don't change anything while \
         reviewing"
            .into()
    } else {
        format!(
            "`{}`. Read the argument by its shape: a bare number is a pull request (`gh pr diff <n>`), a \
             branch or revision range is a `git diff` against it, and anything else is a file or directory \
             (read the files there and the commits that recently touched them)",
            target.trim()
        )
    }
}

// ---------------------------------------------------------------- /simplify

fn simplify_prompt(target: &str) -> String {
    format!(
        "# /simplify\n\nLook for cleanup in {}, then carry it out. Keep behaviour exactly as it is: this is a quality pass, not a bug hunt (use `/code-review` for correctness).\n\n\
         1. Read the changed code and enough of its neighbours to learn the helpers, idioms and module boundaries already in use.\n\
         2. Start four reviewers in parallel with the Task tool (subagent_type \"Explore\", one message), handing each the changed files and one lens:\n\
            - **Duplication**: code that repeats an existing helper or another part of the change; name the helper that should replace it.\n\
            - **Complexity**: state that can be derived, conditions that cancel out, dead branches, functions doing too much, and the simpler formulation.\n\
            - **Waste**: repeated computation, copying, I/O or round trips that could be hoisted, cached or skipped.\n\
            - **Placement**: logic living in a layer or module that doesn't own it, or a local patch where the shared mechanism should change.\n\
            Each reviewer must cite file:line and the replacement, and may answer \"nothing\".\n\
         3. Sort the reports: keep changes that clearly pay off; drop churn, guesses and style preferences.\n\
         4. Make the kept edits one small change at a time, in the surrounding style.\n\
         5. Run the project's build, lint and tests; undo or fix anything that changes behaviour.\n\
         6. Close with the files and lines changed, and what you left alone on purpose.",
        target_or_diff(target)
    )
}

// ---------------------------------------------------------------- /batch

fn in_git_repo(dir: &Path) -> bool {
    std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(dir)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn batch(env: &Arc<Env>, instruction: &str) -> Outcome {
    if instruction.trim().is_empty() {
        return Outcome::Show("Usage: `/batch <instruction>`, e.g. `/batch migrate src/ from JavaScript to TypeScript`. It plans the change, splits it into independent units, and runs one background agent per unit in its own git worktree.".into());
    }
    let has_hook = env.config.hooks.contains_key("WorktreeCreate");
    if !in_git_repo(&env.cwd()) && !has_hook {
        return Outcome::Show("/batch needs a git repository (each unit runs in its own worktree), or a WorktreeCreate hook that creates worktrees.".into());
    }
    Outcome::Prompt(batch_prompt(
        instruction.trim(),
        crate::resources::max_background_agents(),
    ))
}

fn batch_prompt(instruction: &str, limit: usize) -> String {
    format!(
        "# /batch\n\nCarry out this change across the codebase with parallel workers:\n\n> {instruction}\n\n\
         ## Map the work\nUnderstand what the change reaches before splitting it: send Explore subagents (Task, subagent_type \"Explore\", several at once) to find every affected file, the conventions to match, and how the project builds and tests.\n\n\
         ## Split it\nChoose the smallest set of units that can proceed independently — a handful for most changes, and only for a very large refactor as many as about 30. A unit owns files no other unit touches, can be verified by itself, and doesn't need another unit's output. If the work can't be split that way, say so and propose doing it directly in this session instead.\n\n\
         ## Get sign-off\nWrite the plan as a numbered list: each unit's files, the exact change and how it will be verified. Wait for approval before starting anything (in plan mode, present it with ExitPlanMode). Rework it if asked.\n\n\
         ## Launch the workers\nAfter approval, send one Task per unit in a single message with `run_in_background: true` and `isolation: \"worktree\"` (subagent_type \"general-purpose\"). Keep at most {limit} running and add the next as reports arrive. A worker can't see this session, so its prompt must stand alone: restate the goal, its unit's scope and files, the project's conventions, and instruct it to\n\
         1. change only its own files;\n\
         2. build and test its scope, fixing what it broke;\n\
         3. commit on its worktree branch with a clear message;\n\
         4. push and open a PR with `gh pr create` if the repo has a GitHub remote and `gh auth status` succeeds, otherwise leave the commit on the branch;\n\
         5. report status (done / failed / partial), branch, PR URL, files changed and test results.\n\n\
         ## Bring it together\nWait for the reports rather than polling. Keep a unit → status → branch / PR table current as they land. Once all are in, summarize it and name the failures (offer to retry). If PRs were opened, list them and stop. Otherwise ask before merging the branches into the current one, one at a time (`git merge --no-ff <branch>`), testing after each and resolving conflicts."
    )
}

// ---------------------------------------------------------------- /debug

fn debug_prompt(agent: &Agent, description: &str) -> String {
    let log = crate::config::home_dir().join("logs").join(format!(
        "chanto-{}.log",
        chrono::Local::now().format("%Y-%m-%d")
    ));
    let session = agent
        .session
        .as_ref()
        .map(|s| s.id.clone())
        .unwrap_or_default();
    let filter = if session.is_empty() {
        String::new()
    } else {
        format!(" Lines for this session have `\"session_id\":\"{session}\"`.")
    };
    let issue = if description.trim().is_empty() {
        "The user didn't describe the problem: look for errors, failed tool calls, failed or slow model calls and warnings, and ask what went wrong if nothing stands out.".to_string()
    } else {
        format!("The problem: {}", description.trim())
    };
    format!(
        "# /debug\n\nTroubleshoot a problem in this session.\n\n{issue}\n\n\
         Chanto writes structured diagnostics (JSON lines: model calls with provider, status, latency and tokens; tool calls with name, duration and success; errors and warnings) to `{}`.{filter} Earlier days are in the same directory. `chanto doctor` checks the setup (server, credentials, tools).\n\n\
         Work systematically:\n\
         1. **Gather evidence**: read the end of the log (filter by session; don't dump the whole file), the relevant part of this conversation, and the configuration involved (`/config` lists the files).\n\
         2. **Reproduce**: find the smallest way to make the problem happen again (rerun the failing command or request) before changing anything. If it is a bug in the user's code rather than in the session, reproduce it with a test or a minimal script first.\n\
         3. **Hypothesize and test**: list the likely causes, then rule them in or out one at a time with evidence (logs, added output, a narrower reproduction). Don't guess-fix.\n\
         4. **Fix the root cause**, not the symptom, with the smallest change; then rerun the reproduction to confirm it is gone and run the related tests.\n\
         5. **Report**: what was wrong, the evidence, what you changed (or what the user should change, for configuration and environment problems), and how you verified it.",
        crate::util::tilde(&log)
    )
}

// ---------------------------------------------------------------- /code-review

const LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

#[derive(Debug, Default, PartialEq)]
pub struct ReviewArgs {
    pub level: Option<String>,
    pub ultra: bool,
    pub fix: bool,
    pub comment: bool,
    /// `n`, `all`, or `default` (forget a remembered limit).
    pub max_findings: Option<String>,
    pub target: String,
}

/// `[level] [--fix] [--comment] [--max-findings n|all|default] [target]`: the
/// level and flags lead; everything after them is the target.
pub fn parse_review(arg: &str) -> Result<ReviewArgs, String> {
    let mut a = ReviewArgs::default();
    let mut rest = arg.trim();
    loop {
        let (tok, after) = rest
            .split_once(char::is_whitespace)
            .map(|(t, r)| (t, r.trim_start()))
            .unwrap_or((rest, ""));
        match tok {
            "" => break,
            l if LEVELS.contains(&l) && a.level.is_none() && !a.ultra => {
                a.level = Some(l.to_string())
            }
            "ultra" if a.level.is_none() => a.ultra = true,
            "--fix" => a.fix = true,
            "--comment" => a.comment = true,
            "--post" | "--no-post" => {}
            "--max-findings" => {
                let (v, after2) = after
                    .split_once(char::is_whitespace)
                    .map(|(t, r)| (t, r.trim_start()))
                    .unwrap_or((after, ""));
                if !(v == "all" || v == "default" || v.parse::<u32>().is_ok_and(|n| n > 0)) {
                    return Err(format!(
                        "--max-findings takes a number, `all` or `default`, not {v:?}"
                    ));
                }
                a.max_findings = Some(v.to_string());
                rest = after2;
                continue;
            }
            _ => break,
        }
        rest = after;
    }
    a.target = rest.to_string();
    Ok(a)
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct ReviewPrefs {
    level: Option<String>,
    max_findings: Option<String>,
}

fn prefs_path() -> PathBuf {
    crate::config::home_dir().join("code-review.json")
}

fn load_prefs() -> ReviewPrefs {
    std::fs::read(prefs_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_prefs(p: &ReviewPrefs) {
    let _ = std::fs::create_dir_all(crate::config::home_dir());
    let _ = std::fs::write(
        prefs_path(),
        serde_json::to_vec_pretty(p).unwrap_or_default(),
    );
}

fn default_limit(level: &str) -> u32 {
    match level {
        "low" => 5,
        "medium" => 10,
        "high" => 15,
        "xhigh" => 25,
        _ => 40,
    }
}

pub fn review_prompt(a: &ReviewArgs, level: &str, max_findings: Option<&str>) -> String {
    let scope = match level {
        "low" => {
            "Only flag defects you would bet on. A false alarm costs more than a missed minor issue."
        }
        "medium" => {
            "Hunt for defects that would change behaviour: wrong logic, missed edge cases, broken error paths, bad assumptions about inputs or APIs, races, leaks, and security holes. Report only what you'd defend in review."
        }
        "high" => {
            "Cover correctness as above, then include worthwhile tidy-ups: removing duplication, untangling complexity and cutting wasted work. Label which is which."
        }
        "xhigh" => {
            "Be broad and deep: every class of correctness problem, then duplication, complexity and waste. Lower-confidence findings are welcome if you name your uncertainty."
        }
        _ => {
            "Leave nothing unexamined: walk every changed path, its callers and callees, and the error and edge cases around them; then do the same for cleanup. Include uncertain findings with a confidence note."
        }
    };
    let limit = match max_findings {
        Some("all") => "Report every finding.".to_string(),
        Some(n) => format!("Report at most {n} findings, most important first."),
        None => format!(
            "Report at most {} findings, most important first.",
            default_limit(level)
        ),
    };
    let mut s = format!(
        "# /code-review ({level} effort)\n\nReview {} for bugs.\n\n{scope} {limit}\n\n\
         Read the surrounding code, not just the diff, and follow the project's instructions files (AGENTS.md, CLAUDE.md). Verify each finding against the code before reporting it; drop anything you can't substantiate.\n\n\
         Report each finding as: **severity** (critical / high / medium / low) · `file:line` · a one-sentence summary · why it is wrong (the input or path that triggers it) · the concrete fix. Order by severity. If you find nothing worth reporting, say so plainly.",
        target_or_diff(&a.target)
    );
    if a.fix {
        s.push_str("\n\nThen apply the fixes for your findings to the working tree, keeping each change minimal, and run the relevant tests. Finish by listing each finding as fixed, skipped (with why) or no change needed.");
    } else {
        s.push_str("\n\nDo not modify any files.");
    }
    if a.comment {
        s.push_str(
            "\n\nThen post the findings on the pull request or merge request: on GitHub, as inline review comments at each file:line with `gh api repos/{owner}/{repo}/pulls/<number>/reviews` (one review, event COMMENT); on GitLab (a `!123` reference or a gitlab URL), as a single note with `glab mr note`. If the target isn't a PR/MR, or `gh`/`glab` isn't available or signed in, say so and just print the findings.",
        );
    }
    s
}

pub async fn code_review(agent: &mut Agent, arg: &str) -> Outcome {
    let env = agent.env.clone();
    let mut a = match parse_review(arg) {
        Ok(a) => a,
        Err(e) => return Outcome::Show(format!("/code-review: {e}")),
    };
    let mut notes = Vec::new();
    if a.ultra {
        notes.push("Ultrareview (the cloud review) isn't available in chanto; running a max-effort review here instead.".to_string());
        a.level = Some("max".into());
    }
    let mut prefs = load_prefs();
    let level = match (&a.level, a.ultra) {
        (Some(l), false) => {
            if env.interactive {
                prefs.level = Some(l.clone());
            }
            l.clone()
        }
        (Some(l), true) => l.clone(),
        (None, _) => match prefs.level.clone().filter(|l| LEVELS.contains(&l.as_str())) {
            Some(l) => {
                notes.push(format!(
                    "Reusing {l} effort, the level you typed last time."
                ));
                l
            }
            None => env
                .effort()
                .filter(|e| LEVELS.contains(&e.as_str()))
                .unwrap_or_else(|| "medium".into()),
        },
    };
    let max = match a.max_findings.as_deref() {
        Some("default") => {
            prefs.max_findings = None;
            None
        }
        Some(v) => {
            if env.interactive {
                prefs.max_findings = Some(v.to_string());
            }
            Some(v.to_string())
        }
        None => prefs.max_findings.clone(),
    };
    if env.interactive {
        save_prefs(&prefs);
    }
    let prompt = review_prompt(&a, &level, max.as_deref());
    // Interactive: a background agent with its own context;
    // the findings arrive in the conversation when it's done. Otherwise (print
    // mode, or inside a subagent) the review runs in the foreground.
    if !env.interactive || agent.depth > 0 {
        return Outcome::Prompt(prompt);
    }
    let ctx = crate::tools::Ctx {
        env: env.clone(),
        cancel: tokio_util::sync::CancellationToken::new(),
        depth: agent.depth,
        call_id: None,
        cwd_override: agent.cwd_override.clone(),
        reads: None,
    };
    let opts = crate::agent::SubagentOpts {
        background: true,
        description: format!("/code-review {level}"),
        ..Default::default()
    };
    let started = match crate::agent::run_subagent(&ctx, "general-purpose", &prompt, opts).await {
        Ok(out) if !out.is_error => format!(
            "Started a {level}-effort code review in the background; its findings arrive here when it finishes. {}",
            out.display.as_deref().unwrap_or("")
        ),
        Ok(_) | Err(_) => return Outcome::Prompt(prompt),
    };
    notes.push(started);
    Outcome::Show(notes.join("\n"))
}

// ---------------------------------------------------------------- /fewer-permission-prompts

/// Rule-shaped keys for the Bash commands and MCP tools in `messages`, with counts.
pub fn tally(
    messages: &[crate::llm::Message],
) -> (BTreeMap<String, usize>, BTreeMap<String, usize>) {
    let (mut bash, mut mcp) = (BTreeMap::new(), BTreeMap::new());
    for c in messages.iter().flat_map(|m| &m.tool_calls) {
        if c.name.starts_with("mcp__") {
            *mcp.entry(c.name.clone()).or_insert(0) += 1;
        } else if c.name == "Bash" {
            let cmd = c
                .arguments
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            for seg in cmd
                .split(['|', ';', '\n'])
                .flat_map(|s| s.split("&&"))
                .flat_map(|s| s.split("||"))
            {
                if let Some(k) = command_key(seg) {
                    *bash.entry(k).or_insert(0) += 1;
                }
            }
        }
    }
    (bash, mcp)
}

/// `git status --short` → `git status`; `ls -la` → `ls`: the prefix a
/// `Bash(<prefix>:*)` rule would name. Env assignments and `cd` are skipped.
fn command_key(seg: &str) -> Option<String> {
    let words: Vec<&str> = seg
        .split_whitespace()
        .skip_while(|w| w.contains('=') && !w.starts_with('-'))
        .collect();
    let first = *words.first()?;
    if matches!(
        first,
        "cd" | "echo" | "true" | "false" | "sleep" | "(" | "{" | "}" | ")"
    ) || first.starts_with(['#', '$', '(', '"', '\''])
    {
        return None;
    }
    let sub = words.get(1).filter(|w| {
        w.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && w.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    });
    Some(match sub {
        Some(s)
            if matches!(
                first,
                "git"
                    | "cargo"
                    | "npm"
                    | "pnpm"
                    | "yarn"
                    | "go"
                    | "gh"
                    | "docker"
                    | "kubectl"
                    | "pip"
                    | "uv"
                    | "poetry"
                    | "make"
                    | "bun"
                    | "dotnet"
                    | "mvn"
                    | "gradle"
                    | "brew"
                    | "rustup"
                    | "terraform"
            ) =>
        {
            format!("{first} {s}")
        }
        _ => first.to_string(),
    })
}

fn fewer_prompts_prompt(env: &Arc<Env>) -> String {
    let cwd = env.root.clone();
    let mut sessions = crate::session::Session::list(&cwd);
    sessions.sort_by(|a, b| b.updated.cmp(&a.updated));
    let mut all = Vec::new();
    let mut used = 0;
    for s in sessions.iter().take(40) {
        if let Ok((_, msgs)) = crate::session::Session::load(&cwd, &s.id) {
            all.extend(msgs);
            used += 1;
        }
    }
    all.extend(env.conversation.read().unwrap().iter().cloned());
    let (bash, mcp) = tally(&all);
    let top = |m: &BTreeMap<String, usize>, n: usize| {
        let mut v: Vec<_> = m.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        v.into_iter()
            .take(n)
            .map(|(k, c)| format!("- `{k}` ×{c}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let allow = &env.config.permissions.allow;
    format!(
        "# /fewer-permission-prompts\n\nReduce permission prompts in this project with an allowlist of safe, read-only calls.\n\n\
         From {used} saved session(s) in this project plus this one, the most frequent Bash commands (by prefix) and MCP tools were:\n\n**Bash**\n{}\n\n**MCP tools**\n{}\n\n\
         Allow rules already in effect: {}\n\n\
         1. Pick the ones that are **read-only** and safe to run without asking: commands that only inspect (e.g. `git status`, `git diff`, `git log`, `ls`, `cat`, `rg`, `cargo check`-style queries) and MCP tools whose names say they read, list, get or search. Never include anything that writes, deletes, installs, pushes, deploys, runs arbitrary code (`python`, `node`, `bash`, `sh`, `npx`, `make`, test runners that execute project code) or reaches the network with side effects, nor anything already allowed.\n\
         2. Prioritize by how often they were used, and show the proposed rules as a short table (rule, uses, why it is safe), using Chanto's rule syntax: `Bash(git status:*)` for a command prefix, the full tool name (`mcp__server__tool`) for an MCP tool.\n\
         3. Add them to the project's `.chanto/config.toml` under `[permissions]` `allow = [...]` (create the file if needed, keep everything else in it, and merge with any existing list). Chanto reads project allow rules only in a trusted folder; mention that if this folder isn't trusted.\n\
         4. Report what you added and how to undo it.",
        if bash.is_empty() {
            "(none)".into()
        } else {
            top(&bash, 40)
        },
        if mcp.is_empty() {
            "(none)".into()
        } else {
            top(&mcp, 25)
        },
        if allow.is_empty() {
            "none".to_string()
        } else {
            allow
                .iter()
                .map(|r| format!("`{r}`"))
                .collect::<Vec<_>>()
                .join(", ")
        },
    )
}

// ---------------------------------------------------------------- /statusline

/// Where `/statusline` saves the script, and the command that runs it.
fn statusline_script() -> (PathBuf, String) {
    let dir = crate::config::home_dir();
    if cfg!(windows) {
        let p = dir.join("statusline.ps1");
        let cmd = format!(
            "powershell -NoProfile -ExecutionPolicy Bypass -File \"{}\"",
            p.display()
        );
        (p, cmd)
    } else {
        let p = dir.join("statusline.sh");
        let cmd = shell_words::quote(&p.display().to_string()).into_owned();
        (p, cmd)
    }
}

/// Set top-level `status_line` in a Chanto config file, keeping the rest.
pub fn set_status_line(path: &Path, command: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .context("can't edit the chanto config: it isn't valid TOML; fix it first")?;
    doc["status_line"] = toml_edit::value(command);
    std::fs::create_dir_all(path.parent().context("config needs a parent directory")?)?;
    std::fs::write(path, doc.to_string())?;
    Ok(())
}

pub fn statusline(env: &Arc<Env>, description: &str) -> Outcome {
    let (script, cmd) = statusline_script();
    let config = crate::config::home_dir().join("config.toml");
    let previous = env.config.status_line.clone().filter(|p| *p != cmd);
    if let Err(e) = set_status_line(&config, &cmd) {
        return Outcome::Show(format!("/statusline: {e:#}"));
    }
    let what = if description.trim().is_empty() {
        "The user gave no description: base it on their shell prompt. Read PS1 / PROMPT from their shell's startup files (`~/.zshrc`, `~/.bashrc`, `~/.bash_profile`, `~/.profile`, a sourced theme such as oh-my-zsh's or `~/.config/starship.toml`). Reproduce its parts and colors (user, host, directory, git branch, ...), translating prompt escapes (`\\u` `\\h` `\\w` `%n` `%m` `%~` and so on) into commands, using `cwd` from the JSON for the directory; drop the trailing `$`/`%`/`>`. Then add the model name. If no prompt is configured, use `dir · git branch · model · context %`.".to_string()
    } else {
        format!("What the user wants: {}", description.trim())
    };
    let prev = previous
        .map(|p| format!("\n\nTheir previous status line command was `{p}`; reuse what fits."))
        .unwrap_or_default();
    Outcome::Prompt(format!(
        "# /statusline\n\nSet up Chanto's status line: a command whose output shows below the prompt.\n\n{what}{prev}\n\n\
         How Chanto runs it: after each turn it runs `{cmd}` with this JSON on stdin and shows the first line of stdout (ANSI colors are fine); it must finish within 5 seconds:\n\
         ```json\n{{\"cwd\": \"/path/to/project\", \"model\": \"provider/model-name\", \"mode\": \"default\", \"context_used\": 12345, \"context_max\": 200000}}\n```\n\n\
         1. Write the script to `{}` ({}). Parse the JSON with `jq` if it is installed, otherwise without it. Keep it fast: no network calls; for git use `git -C \"$cwd\" ... 2>/dev/null` with quick commands.\n\
         2. Make it executable and test it: pipe the sample JSON above into `{cmd}` and check the output; fix any error.\n\
         3. chanto's config (`{}`) already has `status_line` set to that command; tell the user it applies to new sessions (restart chanto), and that they can change the script any time.",
        script.display(),
        if cfg!(windows) {
            "PowerShell: read stdin with `$input | Out-String | ConvertFrom-Json`"
        } else {
            "a POSIX `sh` script starting with `#!/bin/sh`"
        },
        crate::util::tilde(&config),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_args() {
        let a = parse_review("high --fix --max-findings 7 main...feature").unwrap();
        assert_eq!(a.level.as_deref(), Some("high"));
        assert!(a.fix && !a.comment);
        assert_eq!(a.max_findings.as_deref(), Some("7"));
        assert_eq!(a.target, "main...feature");
        let a = parse_review("/fix-issue 123").unwrap();
        assert_eq!(
            a.target, "/fix-issue 123",
            "the rest is the target, even a command"
        );
        assert!(a.level.is_none());
        let a = parse_review("--comment 1234").unwrap();
        assert!(a.comment && a.target == "1234");
        assert!(parse_review("ultra").unwrap().ultra);
        assert_eq!(parse_review("low src/high").unwrap().target, "src/high");
        assert_eq!(parse_review("").unwrap(), ReviewArgs::default());
        assert!(parse_review("--max-findings lots").is_err());
        assert_eq!(
            parse_review("--max-findings all")
                .unwrap()
                .max_findings
                .as_deref(),
            Some("all")
        );
    }

    #[test]
    fn review_prompts() {
        let a = parse_review("--fix").unwrap();
        let p = review_prompt(&a, "low", None);
        assert!(
            p.contains("at most 5")
                && p.contains("apply the fixes")
                && !p.contains("Do not modify")
        );
        let p = review_prompt(&parse_review("1234").unwrap(), "max", Some("all"));
        assert!(p.contains("`1234`") && p.contains("every finding") && p.contains("Do not modify"));
        assert!(
            review_prompt(&parse_review("--comment").unwrap(), "medium", Some("3"))
                .contains("gh api")
        );
    }

    #[test]
    fn tallies_commands_and_mcp_tools() {
        use crate::llm::{Message, Role, ToolCall};
        let call = |name: &str, args: serde_json::Value| ToolCall {
            id: "1".into(),
            name: name.into(),
            arguments: args,
        };
        let mut m = Message::new(Role::Assistant, String::new());
        m.tool_calls = vec![
            call(
                "Bash",
                serde_json::json!({"command": "git status --short && git diff | head -5"}),
            ),
            call(
                "Bash",
                serde_json::json!({"command": "FOO=1 cargo test -p x; ls -la"}),
            ),
            call(
                "Bash",
                serde_json::json!({"command": "cd src && git status"}),
            ),
            call("mcp__gh__list_issues", serde_json::json!({})),
            call("Read", serde_json::json!({"file_path": "x"})),
        ];
        let (bash, mcp) = tally(&[m]);
        assert_eq!(bash["git status"], 2);
        assert_eq!(bash["git diff"], 1);
        assert_eq!(bash["head"], 1);
        assert_eq!(bash["cargo test"], 1);
        assert_eq!(bash["ls"], 1);
        assert!(!bash.contains_key("cd"));
        assert_eq!(mcp["mcp__gh__list_issues"], 1);
    }

    #[test]
    fn status_line_is_saved_keeping_the_rest() {
        let dir = std::env::temp_dir().join(format!("chanto-sl-{}", uuid::Uuid::new_v4().simple()));
        let p = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &p,
            "# mine\nmodel = \"m\"\nstatus_line = \"old\"\n\n[permissions]\nallow = [\"Read\"]\n",
        )
        .unwrap();
        set_status_line(&p, "/home/me/.chanto/statusline.sh").unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        let t: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(
            t["status_line"].as_str(),
            Some("/home/me/.chanto/statusline.sh")
        );
        assert_eq!(t["permissions"]["allow"][0].as_str(), Some("Read"));
        assert!(text.starts_with("# mine"));
        set_status_line(&dir.join("new/config.toml"), "x").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bundled_names_are_distinct_from_builtins() {
        for b in BUNDLED {
            assert!(
                !crate::commands::BUILTIN.iter().any(|c| c.name == b.name),
                "/{} is both built-in and bundled",
                b.name
            );
        }
        assert!(batch_prompt("do it", 4).contains("isolation: \"worktree\""));
        assert!(simplify_prompt("").contains("four reviewers"));
    }
}
