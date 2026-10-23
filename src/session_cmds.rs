//! Session commands: `/background`, `/cd`, `/plan`,
//! `/recap` and `/autocompact`.

use crate::agent::{Agent, Env};
use crate::commands::Outcome;
use crate::llm::{Message, Role};
use crate::permissions::Mode;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

/// Printed when the frontend exits instead of the usual resume hint
/// (`/background` moved the session elsewhere).
static EXIT_NOTE: Mutex<Option<String>> = Mutex::new(None);

pub fn take_exit_note() -> Option<String> {
    EXIT_NOTE.lock().unwrap().take()
}

/// `/recap` and the away recap are capped at this many characters.
pub const RECAP_MAX: usize = 400;

/// Whether the TUI shows a recap when you come back after a while away:
/// `CLAUDE_CODE_ENABLE_AWAY_SUMMARY` (or `CHANTO_AWAY_SUMMARY`) wins over
/// `away_summary` in the config; on by default.
pub fn away_summary_enabled(env: &Env) -> bool {
    for var in ["CHANTO_AWAY_SUMMARY", "CLAUDE_CODE_ENABLE_AWAY_SUMMARY"] {
        match std::env::var(var).as_deref() {
            Ok("0") | Ok("false") => return false,
            Ok("1") | Ok("true") => return true,
            _ => {}
        }
    }
    env.config.away_summary.unwrap_or(true)
}

/// User prompts in the conversation (not reminders or tool plumbing).
pub fn user_turns(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|m| {
            m.role == Role::User && !m.content.trim_start().starts_with("<system-reminder>")
        })
        .count()
}

/// A one-line summary of the session so far, from the session's model with
/// no tools; nothing is added to the conversation.
pub async fn recap(env: &Env, messages: &[Message]) -> anyhow::Result<String> {
    if user_turns(messages) == 0 {
        anyhow::bail!("Nothing to recap yet.");
    }
    let q = format!(
        "Recap this session so far in one line of at most {RECAP_MAX} characters: what is being worked on, where it stands, \
         and the next step if one is pending. Reply with the recap only."
    );
    let text = Agent::side_question_on(env, &env.model(), messages, &q).await?;
    let one = crate::util::one_line(text.trim().trim_start_matches("Recap:").trim(), RECAP_MAX);
    if one.is_empty() {
        anyhow::bail!("The model returned an empty recap.");
    }
    Ok(one)
}

/// `/plan [task]`: plan mode, and start planning the task if one is given.
pub fn plan(agent: &Agent, arg: &str) -> Outcome {
    agent.env.set_mode(Mode::Plan);
    if arg.trim().is_empty() {
        Outcome::Show("Plan mode: the model researches and proposes a plan before changing anything. shift+tab or `/mode` leaves it.".into())
    } else {
        Outcome::Prompt(arg.trim().to_string())
    }
}

/// Parse `/autocompact` sizes: `500k`, `1.5m`, `200000`.
fn parse_tokens(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase().replace(['_', ','], "");
    let (num, mul) = match s.chars().last()? {
        'k' => (&s[..s.len() - 1], 1_000.0),
        'm' => (&s[..s.len() - 1], 1_000_000.0),
        _ => (&s[..], 1.0),
    };
    let v: f64 = num.parse().ok()?;
    (v > 0.0).then_some((v * mul) as u64)
}

/// `/autocompact [<tokens>|auto|off]`: when this session compacts on its own.
pub fn autocompact(env: &Env, arg: &str) -> Outcome {
    let max = env.context_max();
    let threshold = env.config.compact_threshold.unwrap_or(0.85);
    let configured = format!(
        "{:.0}% of the {max}-token context (about {} tokens)",
        threshold * 100.0,
        (max as f64 * threshold as f64) as u64
    );
    let arg = arg.trim();
    if arg.is_empty() {
        let now = match env.autocompact.load(Ordering::Relaxed) {
            0 => format!("at {configured}, from `compact_threshold`"),
            u64::MAX => "off for this session".into(),
            t => format!("at {t} tokens for this session"),
        };
        return Outcome::Show(format!(
            "Auto-compaction is {now}. `/autocompact <tokens>` (e.g. 120k) sets it for this session, `off` turns it off, `auto` goes back to the configured threshold."
        ));
    }
    match arg {
        "auto" | "default" | "reset" => {
            env.autocompact.store(0, Ordering::Relaxed);
            Outcome::Show(format!("Auto-compaction back to {configured}."))
        }
        "off" | "never" | "0" => {
            env.autocompact.store(u64::MAX, Ordering::Relaxed);
            Outcome::Show("Auto-compaction is off for this session; `/compact` still compacts on demand. A conversation that outgrows the context fails instead.".into())
        }
        _ => match parse_tokens(arg) {
            Some(t) if t < 1_000 => Outcome::Show(
                "That's too small to hold a conversation; use at least 1k tokens.".into(),
            ),
            Some(t) => {
                env.autocompact.store(t, Ordering::Relaxed);
                let capped = if max > 0 && t as f64 > max as f64 * 0.95 {
                    format!(
                        " (above 95% of this model's {max}-token context, so compaction happens at 95%)"
                    )
                } else {
                    String::new()
                };
                Outcome::Show(format!(
                    "This session compacts once the context reaches {t} tokens{capped}."
                ))
            }
            None => Outcome::Show(
                "Usage: `/autocompact [<tokens>|auto|off]`, e.g. `/autocompact 120k`.".into(),
            ),
        },
    }
}

/// `/cd <path>`: move the session to another working directory, keeping the
/// conversation. The new directory's instructions, commands, skills, agents,
/// hooks, permission rules and env apply at once; the session file moves
/// with it so `--resume` finds it there. CwdChanged hooks run.
pub async fn cd(agent: &mut Agent, arg: &str) -> Outcome {
    let env = agent.env.clone();
    let old = env.cwd();
    let arg = arg.trim();
    if arg.is_empty() {
        return Outcome::Show(format!(
            "Working directory: `{}`. Usage: `/cd <path>`",
            crate::util::tilde(&old)
        ));
    }
    let target = crate::util::resolve(&old, arg);
    let Ok(new) = crate::util::canonical(&target) else {
        return Outcome::Show(format!("No such directory: `{}`", target.display()));
    };
    if !new.is_dir() {
        return Outcome::Show(format!("Not a directory: `{}`", new.display()));
    }
    if new == old {
        return Outcome::Show(format!("Already in `{}`.", crate::util::tilde(&new)));
    }
    // `Cd(...)` permission rules restrict where the session can move.
    let subject = [new.display().to_string()];
    if let Some(crate::permissions::Decision::Deny(r)) =
        env.rules.read().unwrap().check("Cd", &subject)
    {
        return Outcome::Show(format!("Can't move there: {r}."));
    }
    let mut trusted = crate::config::is_trusted(&new);
    if !trusted && env.interactive {
        let q = format!(
            "Trust {}? Its project settings (hooks, permission rules, env), instructions, commands and skills will apply to this session.",
            crate::util::tilde(&new)
        );
        let options = vec![
            "Yes, trust it and move".to_string(),
            "No, stay here".to_string(),
        ];
        match env.frontend.choose(&q, &options).await {
            Some(a) if a == options[0] => {
                if let Err(e) = crate::config::trust(&new) {
                    return Outcome::Show(format!("Couldn't record the trust decision: {e:#}"));
                }
                trusted = true;
            }
            _ => return Outcome::Show(format!("Stayed in `{}`.", crate::util::tilde(&old))),
        }
    }
    env.set_cwd(new.clone());
    env.set_project(&new);
    // Project extensions and settings from the new directory.
    *env.commands.write().unwrap() = crate::context::load_commands(&new);
    *env.skills.write().unwrap() = crate::context::load_skills(&new);
    *env.agents.write().unwrap() = crate::context::load_agents(&new);
    *env.output_styles.write().unwrap() = crate::context::load_output_styles(&new);
    let mut applied = vec!["instructions", "commands", "skills", "agents"];
    match crate::config::Config::load(&new, trusted) {
        Ok((cfg, _)) => {
            *env.hooks.write().unwrap() = cfg.hooks.clone();
            switch_rules(&env, &cfg);
            for (k, v) in &cfg.env {
                // SAFETY: as at startup; settings' env applies on top of the previous directory's.
                unsafe { std::env::set_var(k, v) };
            }
            applied.extend(["hooks", "permission rules", "env"]);
        }
        Err(e) => {
            // Without the new settings, at least don't carry the old project's grants over.
            switch_rules(&env, &crate::config::Config::default());
            env.emit(crate::agent::Event::Warn(format!(
                "Couldn't read {}'s settings: {e:#}",
                crate::util::tilde(&new)
            )))
        }
    }
    // Accept-edits mode that came from an "always allow" on an edit was a
    // grant for the old project; a mode the user chose stays.
    let mode_note = env
        .revoke_accept_edits_grant()
        .map(|m| format!(" Permission mode reset to {} for the new project (accept edits came from an \"always allow\" in the old one).", m.label()))
        .unwrap_or_default();
    agent.refresh_system_prompt();
    // The session file lives with its project, so `--resume` there finds it.
    agent.save_session();
    if let Some(s) = agent.session.as_mut() {
        let before = s.path();
        s.cwd = new.clone();
        let after = s.path();
        if before.exists() && before != after {
            if let Some(dir) = after.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if std::fs::rename(&before, &after).is_err() && std::fs::copy(&before, &after).is_ok() {
                let _ = std::fs::remove_file(&before);
            }
        }
        *env.session_info.lock().unwrap() = Some((s.id.clone(), after));
    }
    agent.save_session();
    if crate::hooks::has(&env, "InstructionsLoaded") {
        let files = crate::context::instruction_files(&new)
            .into_iter()
            .map(|(p, _)| (p, "session_start"))
            .collect();
        crate::hooks::instructions_loaded(&env, files, None);
    }
    crate::hooks::cwd_changed(&env, &old, &new).await;
    let instr: Vec<String> = crate::context::instruction_files(&new)
        .iter()
        .map(|(p, _)| crate::util::rel(&new, p))
        .collect();
    let loaded = if instr.is_empty() {
        "no instruction files".to_string()
    } else {
        instr.join(", ")
    };
    agent.messages.push(Message::user(format!(
        "<system-reminder>The user moved this session's working directory from {} to {} (/cd). Relative paths and shell commands now start there.</system-reminder>",
        old.display(),
        new.display()
    )));
    Outcome::Show(format!(
        "Moved to `{}` ({loaded}). Applied its {}. MCP servers from its `.mcp.json` start when you run Chanto there.{mode_note}",
        crate::util::tilde(&new),
        applied.join(", ")
    ))
}

/// `/cd`'s permission switch. Allow rules and extra directories are grants
/// tied to the old project (its settings, its saved "always allow" answers,
/// `/allow` and `/add-dir` this session), so they're replaced by `cfg`'s: the
/// user's own settings, plus the new project's when it's trusted (`cfg` was
/// loaded that way). `--add-dir` directories stay. Deny and ask rules only
/// tighten, so the session's are kept alongside the new ones.
fn switch_rules(env: &Env, cfg: &crate::config::Config) {
    let fresh = crate::permissions::Rules::new(
        &cfg.permissions.allow,
        &cfg.permissions.deny,
        &cfg.permissions.ask,
    );
    {
        let mut guard = env.rules.write().unwrap();
        let rules = &mut *guard;
        rules.allow = fresh.allow;
        for (have, add) in [(&mut rules.deny, fresh.deny), (&mut rules.ask, fresh.ask)] {
            for r in add {
                if !have.iter().any(|h| h.to_string() == r.to_string()) {
                    have.push(r);
                }
            }
        }
    }
    crate::permissions::set_extra_dirs(&extra_dirs_after_cd(env, cfg), &env.cwd());
    // The auto-mode classifier's rules are the project's too.
    *env.auto_mode.write().unwrap() = (
        cfg.permissions.auto_mode_rules.clone(),
        cfg.permissions.fast_path,
    );
}

/// The extra directories after `/cd`: `cfg`'s (relative to the new directory)
/// and `--add-dir`'s (relative to where the session started).
pub(crate) fn extra_dirs_after_cd(env: &Env, cfg: &crate::config::Config) -> Vec<String> {
    let launch = crate::launch::get()
        .add_dirs
        .iter()
        .map(|d| crate::util::resolve(&env.root, d).display().to_string());
    cfg.permissions
        .additional_directories
        .iter()
        .cloned()
        .chain(launch)
        .collect()
}

/// `/background [prompt]`: hand this conversation to a background run (its
/// own git worktree, `chanto runs` / `chanto attach`) and free the terminal.
pub async fn background(agent: &mut Agent, arg: &str) -> Outcome {
    let env = agent.env.clone();
    if user_turns(&agent.messages) == 0 && arg.trim().is_empty() {
        return Outcome::Show("Nothing to move to the background yet. `/background <prompt>` starts one with that instruction (or `/bg <task>` for a fresh run).".into());
    }
    agent.save_session();
    let Some(path) = agent
        .session
        .as_ref()
        .map(|s| s.path())
        .filter(|p| p.exists())
    else {
        return Outcome::Show(
            "This session hasn't been saved, so it can't move to the background.".into(),
        );
    };
    let task = if arg.trim().is_empty() {
        "Continue the work in progress from the conversation above until it's done."
    } else {
        arg.trim()
    };
    let mut body = crate::runs::cli::new_run_body(&env.cwd(), task, None);
    body["model"] = serde_json::json!(env.model());
    body["from_session"] = serde_json::json!(path);
    match crate::runs::cli::start(body).await {
        Ok(run) => {
            *EXIT_NOTE.lock().unwrap() = Some(format!(
                "Session moved to background run {} (branch {}{}). Follow it with: chanto attach {}  ·  list runs: chanto runs",
                run.id,
                run.branch,
                if run.source.snapshot.is_some() {
                    ", with your uncommitted changes"
                } else {
                    ""
                },
                run.id
            ));
            Outcome::Exit
        }
        Err(e) => Outcome::Show(format!(
            "Couldn't move the session to the background: {e:#}"
        )),
    }
}

pub const SESSIONS_USAGE: &str = "Usage: chanto sessions [--search <text>] [--all]\n       chanto sessions delete <id>\n       chanto sessions archive <id>\n       chanto sessions unarchive <id>\n\n\
List this directory's saved sessions (forks indented under their parent); resume one with `chanto -r <id>`.\n\
--search keeps sessions whose title, first prompt or id contains every word; --all includes archived ones.\n\
delete removes the transcript and its rewind snapshots (never project files); archive hides a session from lists and `-c`.";

/// One listing line: indent, id, date, title (and first prompt when it says more).
fn listing_line(l: &crate::session::Listed) -> String {
    let s = &l.session;
    let title = if s.title.is_empty() {
        "(untitled)"
    } else {
        s.title.as_str()
    };
    let prompt = crate::util::one_line(&l.first_prompt, 70);
    let prompt = if prompt.is_empty() || title.starts_with(prompt.trim_end_matches('…')) {
        String::new()
    } else {
        format!(" — {prompt}")
    };
    format!(
        "{}{}  {}  {}{}{}",
        "  ".repeat(l.depth),
        s.id,
        s.updated.get(..16).unwrap_or(&s.updated).replace('T', " "),
        title,
        if s.archived { "  [archived]" } else { "" },
        prompt
    )
}

/// `chanto sessions …` for the directory `cwd`.
pub fn sessions_cli(cwd: &std::path::Path, args: &[String]) -> anyhow::Result<String> {
    use crate::session::Session;
    match args.first().map(String::as_str) {
        Some("--help" | "-h" | "help") => Ok(SESSIONS_USAGE.into()),
        Some(verb @ ("delete" | "rm" | "archive" | "unarchive")) => {
            let [id] = &args[1..] else {
                anyhow::bail!("usage: chanto sessions {verb} <id>")
            };
            Ok(match verb {
                "archive" | "unarchive" => {
                    let s = Session::set_archived(cwd, id, verb == "archive")?;
                    format!(
                        "{} session {} ({}).",
                        if verb == "archive" {
                            "Archived"
                        } else {
                            "Unarchived"
                        },
                        s.id,
                        if s.title.is_empty() {
                            "untitled"
                        } else {
                            &s.title
                        }
                    )
                }
                _ => {
                    let s = Session::delete(cwd, id)?;
                    format!(
                        "Deleted session {} and its rewind snapshots; project files are untouched.",
                        s.id
                    )
                }
            })
        }
        _ => {
            let (mut query, mut all) = (String::new(), false);
            let mut it = args.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--all" | "-a" => all = true,
                    "--search" | "-s" => {
                        query = it
                            .next()
                            .cloned()
                            .ok_or_else(|| anyhow::anyhow!("--search needs some text"))?
                    }
                    s if s.starts_with("--search=") => query = s["--search=".len()..].to_string(),
                    other => anyhow::bail!("unknown argument `{other}`\n\n{SESSIONS_USAGE}"),
                }
            }
            let list = Session::browse(cwd, &query, all);
            if list.is_empty() {
                return Ok(if !query.is_empty() {
                    format!(
                        "No saved sessions here match `{query}`{}.",
                        if all {
                            ""
                        } else {
                            " (--all includes archived ones)"
                        }
                    )
                } else {
                    "No saved sessions in this directory yet.".into()
                });
            }
            Ok(list.iter().map(listing_line).collect::<Vec<_>>().join("\n"))
        }
    }
}

/// `/sessions delete|archive|unarchive <id>` (also under `/resume`). Delete
/// asks first when interactive; the open session can't be deleted.
pub async fn manage(agent: &mut Agent, verb: &str, id: &str) -> Outcome {
    use crate::session::Session;
    let env = agent.env.clone();
    let cwd = env.cwd();
    let id = id.trim();
    if id.is_empty() {
        return Outcome::Show(format!("Usage: `/sessions {verb} <id>`"));
    }
    let current = agent.session.as_ref().map(|s| s.id.clone());
    let Ok((target, _)) = Session::load(&cwd, id) else {
        return Outcome::Show(format!("No session `{id}` for this directory."));
    };
    let is_current = current.as_deref() == Some(target.id.as_str());
    let title = if target.title.is_empty() {
        "untitled".to_string()
    } else {
        target.title.clone()
    };
    match verb {
        "archive" | "unarchive" => {
            let archive = verb == "archive";
            if is_current {
                // The open session saves itself; the flag rides along.
                if let Some(s) = agent.session.as_mut() {
                    s.archived = archive;
                }
                agent.save_session();
            } else if let Err(e) = Session::set_archived(&cwd, &target.id, archive) {
                return Outcome::Show(format!("Couldn't {verb} {}: {e:#}", target.id));
            }
            Outcome::Show(if archive {
                format!(
                    "Archived **{title}** ({}). `/resume` hides it; Tab there (or `chanto sessions --all`) shows archived sessions.",
                    target.id
                )
            } else {
                format!("Unarchived **{title}** ({}).", target.id)
            })
        }
        _ => {
            if is_current {
                return Outcome::Show("That's the open session; switch to another (`/resume`, `/clear`) before deleting it.".into());
            }
            if env.interactive {
                let q = format!(
                    "Delete session \"{title}\" ({})? Its transcript and rewind snapshots are removed; project files are not touched.",
                    target.id
                );
                let options = vec!["Delete".to_string(), "Cancel".to_string()];
                if env.frontend.choose(&q, &options).await.as_deref() != Some("Delete") {
                    return Outcome::Show("Kept the session.".into());
                }
            }
            match Session::delete(&cwd, &target.id) {
                Ok(_) => Outcome::Show(format!("Deleted session **{title}** ({}).", target.id)),
                Err(e) => Outcome::Show(format!("Couldn't delete {}: {e:#}", target.id)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_cli_searches_archives_and_deletes() {
        use crate::session::Session;
        let _g = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home =
            std::env::temp_dir().join(format!("chanto-sessions-cli-{}", uuid::Uuid::new_v4()));
        let previous = std::env::var_os("CHANTO_HOME");
        // SAFETY: test-only, under TEST_HOME_LOCK.
        unsafe { std::env::set_var("CHANTO_HOME", &home) };
        let cwd = home.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("keep.txt"), "project file").unwrap();
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut auth = Session::new(&cwd, "m");
        auth.title = "Auth refactor".into();
        auth.save(vec![
            Message::system("s"),
            Message::user("move login to OAuth tokens"),
        ])
        .unwrap();
        let mut docs = Session::new(&cwd, "m");
        docs.save(vec![
            Message::system("s"),
            Message::user("write the README"),
        ])
        .unwrap();
        let out = sessions_cli(&cwd, &[]).unwrap();
        assert!(out.contains(&auth.id) && out.contains(&docs.id), "{out}");
        // Search covers the title and the first prompt, all words, any case.
        let out = sessions_cli(&cwd, &args(&["--search", "oauth LOGIN"])).unwrap();
        assert!(out.contains(&auth.id) && !out.contains(&docs.id), "{out}");
        assert!(
            sessions_cli(&cwd, &args(&["--search", "auth"]))
                .unwrap()
                .contains(&auth.id)
        );
        assert!(
            sessions_cli(&cwd, &args(&["--search", "nothing-like-it"]))
                .unwrap()
                .starts_with("No saved sessions here match")
        );
        // Archived: hidden by default and from -c, listed with --all.
        sessions_cli(&cwd, &args(&["archive", &auth.id])).unwrap();
        let out = sessions_cli(&cwd, &[]).unwrap();
        assert!(!out.contains(&auth.id), "{out}");
        let out = sessions_cli(&cwd, &args(&["--all"])).unwrap();
        assert!(
            out.contains(&auth.id) && out.contains("[archived]"),
            "{out}"
        );
        assert_eq!(Session::latest(&cwd).0.map(|s| s.id), Some(docs.id.clone()));
        assert!(Session::load(&cwd, &auth.id).unwrap().0.archived);
        sessions_cli(&cwd, &args(&["unarchive", &auth.id])).unwrap();
        assert!(sessions_cli(&cwd, &[]).unwrap().contains(&auth.id));
        // Delete: the transcript goes, project files stay; an ambiguous prefix is refused.
        assert!(sessions_cli(&cwd, &args(&["delete", &auth.id[..4]])).is_err());
        let out = sessions_cli(&cwd, &args(&["delete", &auth.id])).unwrap();
        assert!(out.starts_with("Deleted session"), "{out}");
        assert!(!auth.path().exists() && docs.path().exists());
        assert_eq!(
            std::fs::read_to_string(cwd.join("keep.txt")).unwrap(),
            "project file"
        );
        assert!(sessions_cli(&cwd, &args(&["delete", &auth.id])).is_err());
        assert!(sessions_cli(&cwd, &args(&["--bogus"])).is_err());
        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("CHANTO_HOME", previous);
            } else {
                std::env::remove_var("CHANTO_HOME");
            }
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn autocompact_sizes() {
        assert_eq!(parse_tokens("500k"), Some(500_000));
        assert_eq!(parse_tokens("1.5M"), Some(1_500_000));
        assert_eq!(parse_tokens("120_000"), Some(120_000));
        assert_eq!(parse_tokens("lots"), None);
        assert_eq!(parse_tokens("-5k"), None);
    }

    #[test]
    fn user_turns_skip_reminders() {
        let m = vec![
            Message::user("fix it"),
            Message::user("<system-reminder>x</system-reminder>"),
            Message::assistant("ok"),
            Message::user("thanks"),
        ];
        assert_eq!(user_turns(&m), 2);
    }
}
