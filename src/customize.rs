//! Customization guidance, read-only extension checks, and live reload.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::config::HOOK_EVENTS;

pub fn extension_guide() -> String {
    // Real locations: the user's config home (CHANTO_HOME aware) and this
    // binary, which may differ from the `chanto` on PATH.
    let home = crate::config::home_dir().display().to_string();
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "chanto".into());
    guide_text()
        .replace("~/.chanto/", &format!("{home}/"))
        .replace(
            "`chanto check-extensions",
            &format!("`{exe} check-extensions"),
        )
}

fn guide_text() -> String {
    let schema = include_str!("config.rs")
        .split("pub struct Config {")
        .nth(1)
        .unwrap()
        .split("\n}")
        .next()
        .unwrap();
    let keys = schema
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("pub ")
                .and_then(|s| s.split_once(':').map(|(key, _)| key))
        })
        .collect::<Vec<_>>()
        .join(", ");
    // Keep examples sourced from the config schema used by the running binary.
    format!(
        r#"# chanto extension guide
Implement the user's request with the smallest extension: instructions, a command or skill before a subagent or hook. Ask user vs project scope when it matters and isn't specified. Preserve existing files and settings. Writes to protected settings still require normal approval; never bypass it. If extension points cannot express the request, explain `chanto customize --source`.

Markdown front matter uses simple key: value lines between opening and closing --- lines, followed by a nonempty instruction body.
User scope: ~/.chanto/; project scope: .chanto/. Project files override user files. Config: config.toml, plus project config.local.toml (local overrides). Risky project config (hooks, MCP, permissions, endpoints) requires folder trust (`chanto --trust`); do not trust a folder on the user's behalf.
- Commands: commands/<name>.md; optional `---` front matter with `description: ...`, then the prompt body. Invoke /<name>; placeholders $ARGUMENTS, $1..$9, $NAME. Inline !`command` executes shell code and needs project trust.
- Skills: skills/<name>/SKILL.md; optional `---` front matter with `name: ...` and `description: ...`, then instructions. The Skill tool loads them; supporting files can live beside SKILL.md. Skills and commands also take `argument-hint` (shown in completion), `allowed-tools` (permission rules pre-approved for the turn that invokes it, e.g. `Bash(npm test *) Read`), `model`, `effort`, `shell` (bash|powershell for !`cmd`), `hooks` (same layout as settings, registered when invoked; `once: true` removes one after its first success), and with `context: fork` + `agent`, `background: false` to wait for the result.
- Subagents: agents/<name>.md; optional front matter `name`, `description`, `tools` (comma-separated tool names), `disallowedTools`, `model` (provider/model or inherit), `permissionMode` (default|acceptEdits|auto|dontAsk|plan), `maxTurns`, `skills` (preloaded), `mcpServers` (configured names or inline name-to-config maps), `hooks` (active while it runs; Stop runs as SubagentStop), `memory` (user|project|local), `background`, `effort`, `isolation: worktree`, `initialPrompt` (for --agent), `color`, `omitClaudeMd`; body is the subagent prompt. Task selects the agent by name.
- Output styles: output-styles/<name>.md with optional name/description front matter and instruction body. Select /output-style <name> or output_style = "<name>" in config.
- Instructions: AGENTS.md in the project (also CHANTO.md/CLAUDE.md; AGENTS.override.md overrides AGENTS.md). User instructions: ~/.chanto/AGENTS.md.
- Profiles: [profiles.<name>] in config, containing config overrides; select --profile <name> or profile = "<name>". Profiles apply last.
- MCP servers: [mcp_servers.<name>], command/args/env for stdio or url/headers for HTTP. Prefer `chanto mcp add <name> -s user|project|local -- <command> [args]`, or a URL instead of -- command. Restart for server changes.
- Plugins: compatible enabled Claude plugins contribute commands, skills, agents, hooks, output styles and MCP; settings enabledPlugins controls them. /plugins lists enabled plugins. Prefer a native extension for a single behavior.
- Hooks: [[hooks.<Event>]] with command (optional args = [...] for exec form without a shell, shell = "powershell", async = true to run in the background, asyncRewake = true to wake the model on exit 2, onFailure = "block" to block when the hook fails), or type = "http" plus url (optional headers, allowed_env_vars, onFailure), type = "mcp_tool" plus server, tool and input (strings take ${{tool_input.file_path}}-style paths), or type = "prompt"/"agent" plus prompt ($ARGUMENTS is the input JSON; optional model; the model answers {{"ok": true}} or {{"ok": false, "reason": "..."}}; they run on tool, prompt, Stop/SubagentStop, task and TeammateIdle events, agent hooks not on PermissionRequest; prompt hooks take continueOnBlock), optional matcher (empty or * = all; names separated by | or , match exactly; anything else is an unanchored regex), optional if (one permission rule such as Bash(git *); tool events only), timeout_secs (default 60). Events: {}.
  JSON stdin: hook_event_name, session_id, transcript_path, cwd, permission_mode, plus event fields (tool_name/tool_input for tool events, tool_response object after success (Bash: stdout, stderr, interrupted) plus tool_output text, error after failure; prompt for UserPromptSubmit; last_assistant_message/stop_hook_active for Stop/SubagentStop; agent_type for SubagentStart; trigger/custom_instructions for PreCompact; trigger/compact_summary for PostCompact; source for SessionStart; reason for SessionEnd; message/notification_type for Notification; the other events carry the fields Claude Code documents, e.g. tool_calls for PostToolBatch, task_id/task_subject for TaskCreated/TaskCompleted (TodoWrite items), old_cwd/new_cwd for CwdChanged, from_model/to_model for PreModelSwitch/PostModelSwitch; trigger (init|maintenance) for Setup, which fires only for --init-only or -p with --init / --maintenance and can't block). Environment: CLAUDE_PROJECT_DIR and CHANTO_PROJECT_DIR; SessionStart, Setup, CwdChanged and FileChanged hooks also get CLAUDE_ENV_FILE (also CHANTO_ENV_FILE): `export NAME=value` lines appended there run before every later Bash command in the session.
  Exit 2 blocks with stderr feedback; exit 0 may print JSON: {{"hookSpecificOutput":{{"permissionDecision":"allow|deny|ask","permissionDecisionReason":"reason","updatedInput":{{}},"additionalContext":"text"}}}}. PreToolUse/PermissionRequest use permissionDecision; PreToolUse uses updatedInput. Stop can return {{"decision":"block","reason":"keep going"}}. PostToolUse can return hookSpecificOutput.updatedToolOutput shaped like tool_response to replace what the model sees. `disable_all_hooks = true` turns hooks off. {{"continue":false,"stopReason":"reason"}} ends the turn; systemMessage shows text to the user. Plain stdout adds context for SessionStart/UserPromptSubmit.

After writing, run `chanto check-extensions -C <project>` through Bash, inspect its errors and loaded names (commands, skills, agents, styles, hooks), and fix failures. Then suggest /reload to activate Markdown extensions, instructions, styles and hooks; other config, providers, MCP and LSP need a restart. Do not claim activation until validated.

Top-level config keys from the running schema: {}. Config is TOML; use the example formats below and `chanto --example-config`. For less common keys, read src/config.rs and the referenced config structs in the source checkout rather than inventing formats.
Config example from the running binary:
{}
"#,
        HOOK_EVENTS.join(", "),
        keys,
        crate::config::EXAMPLE_CONFIG
    )
}

fn check_markdown(path: &Path) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if let Some(rest) = text.strip_prefix("---") {
        let end = rest
            .find("\n---")
            .with_context(|| format!("{}: unclosed front matter", path.display()))?;
        // Indented lines and `- ` items continue the key above (YAML lists, block text).
        for line in rest[..end]
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with([' ', '\t', '-', '#']))
        {
            if !line.contains(':') {
                bail!("{}: expected key: value in front matter", path.display());
            }
        }
    }
    if crate::context::strip_frontmatter(&text).trim().is_empty() {
        bail!("{}: empty instruction body", path.display());
    }
    Ok(())
}

fn check_hooks(configured: &crate::config::HooksConfig) -> Result<()> {
    for (event, hooks) in configured {
        if !HOOK_EVENTS.contains(&event.as_str()) {
            bail!("unknown hook event {event}");
        }
        for h in hooks {
            if h.hooks.is_empty() && !crate::config::runnable_hook(h) {
                bail!(
                    "{event}: empty hook command (or unsupported hook type `{}`)",
                    h.kind
                );
            }
            if h.hooks
                .iter()
                .any(|c| c.kind.is_empty() || !crate::config::runnable_hook(c))
            {
                bail!(
                    "{event}: invalid nested hook (each needs a `type`: `command` with command, `http` with url, `mcp_tool` with server and tool, or `prompt`/`agent` with prompt)"
                );
            }
            for cond in h
                .condition
                .iter()
                .chain(h.hooks.iter().filter_map(|c| c.condition.as_ref()))
            {
                if !crate::hooks::condition_is_valid(cond) {
                    bail!(
                        "{event}: invalid hook `if` condition {cond:?} (one permission rule, e.g. `Bash(git *)`)"
                    );
                }
            }
            regex::Regex::new(&format!("^(?:{})$", h.matcher))
                .with_context(|| format!("{event}: invalid hook matcher"))?;
        }
    }
    Ok(())
}

pub fn check_extensions(cwd: &Path) -> Result<(crate::config::Config, String)> {
    // Validate native config even when trust would strip its risky fields.
    for path in [
        crate::config::home_dir().join("config.toml"),
        cwd.join(".chanto/config.toml"),
        cwd.join(".chanto/config.local.toml"),
    ] {
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            let mut raw: toml::Table =
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
            if let Some(profiles) = raw.remove("profiles") {
                let profiles = profiles.as_table().context("profiles must be a table")?;
                for (name, value) in profiles {
                    let profile: crate::config::Config = value
                        .clone()
                        .try_into()
                        .with_context(|| format!("{} profile {name}", path.display()))?;
                    check_hooks(&profile.hooks)?;
                }
            }
            let native: crate::config::Config = toml::Value::Table(raw)
                .try_into()
                .with_context(|| format!("parsing {}", path.display()))?;
            check_hooks(&native.hooks).with_context(|| format!("{}", path.display()))?;
        }
    }
    let trusted = crate::config::is_trusted(cwd);
    let (cfg, skipped) = crate::config::Config::load(cwd, trusted)?;
    // From the CLI nothing has loaded plugins yet; their commands and skills count too.
    crate::compat::set_active_plugins(crate::compat::plugins_with_compat(
        cwd,
        trusted,
        cfg.compat.claude,
    ));
    for kind in ["commands", "skills", "agents", "output-styles"] {
        for (dir, _, _) in crate::context::component_dirs(cwd, kind) {
            if !dir.exists() {
                continue;
            }
            for entry in std::fs::read_dir(&dir)? {
                let path = entry?.path();
                if kind == "skills" && path.is_dir() {
                    // Compatibility folders can contain grouping directories (e.g.
                    // Codex's .system or Claude's synced) that loaders ignore.
                    let native = dir == crate::config::home_dir().join("skills")
                        || dir == cwd.join(".chanto/skills");
                    if !native && !path.join("SKILL.md").exists() {
                        continue;
                    }
                    check_markdown(&path.join("SKILL.md"))?;
                } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    check_markdown(&path)?;
                }
            }
        }
    }
    check_hooks(&cfg.hooks)?;
    let mut report = String::from("Extensions valid\n");
    let lists = [
        (
            "commands",
            crate::context::load_commands(cwd)
                .into_iter()
                .map(|c| c.name)
                .collect::<Vec<_>>(),
        ),
        (
            "skills",
            crate::context::load_skills(cwd)
                .into_iter()
                .map(|c| c.name)
                .collect(),
        ),
        (
            "agents",
            crate::context::load_agents(cwd)
                .into_iter()
                .map(|c| c.name)
                .collect(),
        ),
        (
            "output styles",
            crate::context::load_output_styles(cwd)
                .into_iter()
                .map(|c| c.name)
                .collect(),
        ),
        (
            "hooks",
            cfg.hooks
                .iter()
                .map(|(event, h)| format!("{event} ({})", h.len()))
                .collect(),
        ),
    ];
    for (kind, mut names) in lists {
        names.sort();
        report.push_str(&format!(
            "{kind}: {}\n",
            if names.is_empty() {
                "none".into()
            } else {
                names.join(", ")
            }
        ));
    }
    if !skipped.is_empty() {
        report.push_str(&format!(
            "Ignored untrusted settings: {}\n",
            skipped.join(", ")
        ));
    }
    report.push_str("Validation does not execute hooks or connect MCP servers.");
    Ok((cfg, report))
}

/// `/reload`, after ConfigChange hooks had their say on the settings files
/// that changed.
pub async fn reload_checked(env: &crate::agent::Env) -> Result<String> {
    if let Err(why) = crate::hooks::config_change(env).await {
        bail!(why);
    }
    reload(env)
}

pub fn reload(env: &crate::agent::Env) -> Result<String> {
    let cwd = &env.root;
    let previous = crate::compat::active_plugins();
    crate::compat::set_active_plugins(crate::compat::plugins_with_compat(
        cwd,
        crate::config::is_trusted(cwd),
        env.config.compat.claude,
    ));
    let (cfg, report) = match check_extensions(cwd) {
        Ok(result) => result,
        Err(e) => {
            crate::compat::set_active_plugins(previous);
            return Err(e);
        }
    };
    *env.commands.write().unwrap() = crate::context::load_commands(cwd);
    *env.skills.write().unwrap() = crate::context::load_skills(cwd);
    *env.agents.write().unwrap() = crate::context::load_agents(cwd);
    *env.output_styles.write().unwrap() = crate::context::load_output_styles(cwd);
    *env.hooks.write().unwrap() = cfg.hooks;
    *env.output_style.write().unwrap() = cfg
        .output_style
        .filter(|s| !s.eq_ignore_ascii_case("default"));
    Ok(format!(
        "{report}\nReloaded commands, skills, agents, output styles, instructions and hooks. Other configuration, provider/tool settings, MCP and LSP servers require a restart."
    ))
}

pub fn source_checkout(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path.join("Cargo.toml")) else {
        return false;
    };
    let Ok(manifest) = toml::from_str::<toml::Value>(&text) else {
        return false;
    };
    manifest
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        == Some("chanto")
        && path.join(".git").exists()
        && std::process::Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(path)
            .output()
            .is_ok_and(|o| o.status.success() && o.stdout == b"true\n")
}

fn slug(request: &str) -> String {
    let words: Vec<_> = request
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .take(5)
        .collect();
    let slug = words.join("-").to_ascii_lowercase();
    if slug.is_empty() {
        "change".into()
    } else {
        slug.chars().take(48).collect()
    }
}

async fn command(cwd: &Path, program: &str, args: &[&str]) -> Result<()> {
    let status = tokio::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .status()
        .await?;
    if !status.success() {
        bail!("{program} {} failed: {status}", args.join(" "));
    }
    Ok(())
}

pub async fn run_source(mut cli: crate::Cli) -> Result<()> {
    use crate::agent::{Frontend, PermissionReply, PermissionRequest, TurnEnd};
    let request = cli.prompt.join(" ");
    if request.trim().is_empty() {
        bail!("Usage: chanto customize --source <request>");
    }
    let fe = std::sync::Arc::new(crate::plain::PlainFrontend::new(
        crate::plain::OutputFormat::Live,
        true,
    ));
    let built_from = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let checkout = if source_checkout(&built_from) {
        built_from
    } else {
        let dest = dirs::home_dir()
            .context("no home directory")?
            .join(".local/share/chanto/src");
        if !source_checkout(&dest) {
            let manifest: toml::Value = toml::from_str(include_str!("../Cargo.toml"))?;
            let url = manifest["package"]["repository"]
                .as_str()
                .context("no repository URL in Cargo.toml")?;
            let answer = fe
                .choose(
                    &format!(
                        "Source checkout unavailable. Clone {url} into {}?",
                        dest.display()
                    ),
                    &["Yes".into(), "No".into()],
                )
                .await;
            if !answer.is_some_and(|s| s.eq_ignore_ascii_case("yes")) {
                bail!("source clone declined");
            }
            if dest.exists() {
                bail!("{} exists but is not a chanto checkout", dest.display());
            }
            std::fs::create_dir_all(dest.parent().unwrap())?;
            command(
                dest.parent().unwrap(),
                "git",
                &[
                    "clone",
                    "--",
                    url,
                    dest.to_str().context("non-UTF8 checkout path")?,
                ],
            )
            .await?;
            if !source_checkout(&dest) {
                bail!("cloned repository is not a chanto checkout");
            }
        }
        dest
    };
    let dirty = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&checkout)
        .output()?;
    if !dirty.status.success() || !dirty.stdout.is_empty() {
        bail!(
            "source checkout has uncommitted changes; commit or stash them first: {}",
            checkout.display()
        );
    }
    let previous = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&checkout)
        .output()?;
    let previous = String::from_utf8_lossy(&previous.stdout).trim().to_string();
    let branch = format!(
        "custom/{}-{}",
        slug(&request),
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    command(&checkout, "git", &["switch", "-c", &branch]).await?;
    println!(
        "Source: {} · branch {branch}\nUndo later: git switch {previous} && ./install.sh --from-source (your customization stays on {branch}).",
        checkout.display()
    );
    cli.cwd = Some(checkout.clone());
    cli.add_dir.clear();
    cli.verify = true;
    cli.no_trust_prompt = true;
    let crate::Setup { env, mut agent, .. } = crate::setup(&cli, fe.clone(), true).await?;
    let cancel = tokio_util::sync::CancellationToken::new();
    let signal = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal.cancel();
    });
    let result = agent.run_turn_verified(
        &format!("Modify chanto's own source for this request: {request}. Work only in this checkout. Do not install or build release yourself; the caller tests, builds and installs afterward. Preserve existing tests. Report changed files and limitations."),
        vec!["nice -n 19 cargo test -j 2 --quiet".into()], vec![], cancel,
    ).await;
    env.shutdown().await;
    let TurnEnd::Done(text) = result? else {
        bail!("customization interrupted; source branch retained");
    };
    println!("{text}");
    // Verify independently: the done gate may report failure after exhausting attempts.
    command(
        &checkout,
        "nice",
        &["-n", "19", "cargo", "test", "-j", "2", "--quiet"],
    )
    .await?;
    // Commit on the customization branch, so undoing is a clean `git switch` back.
    command(&checkout, "git", &["add", "-A"]).await?;
    let msg = format!("Customize: {}", crate::util::one_line(&request, 72));
    if command(&checkout, "git", &["commit", "-q", "-m", &msg])
        .await
        .is_err()
    {
        println!("(nothing to commit: the agent made no source changes)");
    }
    let dest = std::env::var_os("PREFIX")
        .map(PathBuf::from)
        .unwrap_or(
            dirs::home_dir()
                .context("no home directory")?
                .join(".local"),
        )
        .join("bin/chanto");
    let reply = fe
        .permission(PermissionRequest {
            tool: "Bash".into(),
            label: "Build and reinstall chanto".into(),
            detail: format!(
                "cargo build --release; remove then copy target/release/chanto to {}",
                dest.display()
            ),
            always_label: "Approve this install".into(),
            reason: Some("Replace the installed executable with the tested customization".into()),
            proposed: None,
            auto_deny: None,
        })
        .await;
    if !matches!(reply, PermissionReply::Allow | PermissionReply::AllowAlways) {
        bail!("installation declined; tested source changes retained on {branch}");
    }
    command(&checkout, "cargo", &["build", "--release"]).await?;
    std::fs::create_dir_all(dest.parent().unwrap())?;
    match std::fs::remove_file(&dest) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    std::fs::copy(checkout.join("target/release/chanto"), &dest)?;
    println!(
        "Installed {} from {branch}. Restart chanto to use it.",
        dest.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("chanto-customize-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    #[test]
    fn guide_covers_extension_points() {
        // The guide and the expected home path both read CHANTO_HOME, which
        // other tests change under this lock; hold it so they agree.
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let guide = extension_guide();
        for word in [
            "config.toml",
            "commands/",
            "SKILL.md",
            "agents/",
            "output-styles/",
            "profiles",
            "MCP",
            "Plugins",
            "AGENTS.md",
            &crate::config::home_dir().display().to_string(),
            ".chanto/",
            "check-extensions",
            "/reload",
            "max_turns",
            "guardrails",
        ] {
            assert!(guide.contains(word), "missing {word}");
        }
        for event in HOOK_EVENTS {
            assert!(guide.contains(event));
        }
    }
    #[test]
    fn check_detects_broken_extensions() {
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let d = temp();
        let root = d.join(".chanto");
        std::fs::create_dir_all(root.join("commands")).unwrap();
        std::fs::write(root.join("config.toml"), "model = [").unwrap();
        assert!(
            check_extensions(&d)
                .unwrap_err()
                .to_string()
                .contains("config.toml")
        );
        std::fs::remove_file(root.join("config.toml")).unwrap();
        std::fs::write(root.join("commands/bad.md"), "---\ndescription: broken").unwrap();
        assert!(
            check_extensions(&d)
                .unwrap_err()
                .to_string()
                .contains("bad.md")
        );
        std::fs::remove_file(root.join("commands/bad.md")).unwrap();
        std::fs::create_dir_all(root.join("skills/bad")).unwrap();
        std::fs::write(root.join("skills/bad/SKILL.md"), "").unwrap();
        assert!(
            check_extensions(&d)
                .unwrap_err()
                .to_string()
                .contains("SKILL.md")
        );
        std::fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn detects_source_checkout() {
        let d = temp();
        std::fs::write(d.join("Cargo.toml"), "[package]\nname = 'chanto'\n").unwrap();
        assert!(!source_checkout(&d));
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&d)
                .status()
                .unwrap()
                .success()
        );
        assert!(source_checkout(&d));
        std::fs::write(d.join("Cargo.toml"), "[package]\nname = 'other'\n").unwrap();
        assert!(!source_checkout(&d));
        std::fs::remove_dir_all(d).unwrap();
    }
}
