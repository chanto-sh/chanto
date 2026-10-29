//! Slash commands end to end: each one against a real `Env`/`Agent` in a temp
//! project, with a mock OpenAI server where a command calls the model.

use super::*;
use crate::agent::tests::{mock_server, remove_dir_eventually, tmpdir};
use crate::agent::{EnvInit, Frontend, PermissionReply, PermissionRequest};
use crate::config::Config;
use crate::llm::{Message, Provider, ProviderKind, Role, ToolCall};
use serde_json::json;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Records what a command tells the user, and answers its questions from a script.
#[derive(Default)]
struct Recorder {
    notes: Mutex<Vec<String>>,
    answers: Mutex<VecDeque<String>>,
}

impl Recorder {
    fn notes(&self) -> Vec<String> {
        self.notes.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Frontend for Recorder {
    fn emit(&self, ev: Event) {
        match ev {
            Event::Warn(s) | Event::Info(s) | Event::Error(s) => self.notes.lock().unwrap().push(s),
            _ => {}
        }
    }
    async fn permission(&self, _: PermissionRequest) -> PermissionReply {
        PermissionReply::Deny(None)
    }
    async fn question(&self, _: &str, _: &[String]) -> Option<String> {
        self.answers.lock().unwrap().pop_front()
    }
}

struct Fixture {
    dir: PathBuf,
    env: Arc<Env>,
    fe: Arc<Recorder>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        remove_dir_eventually(&self.dir);
    }
}

fn fixture_with(
    name: &str,
    base: &str,
    interactive: bool,
    setup: impl FnOnce(&Path),
    f: impl FnOnce(&mut Config),
) -> Fixture {
    let dir = tmpdir(name);
    setup(&dir);
    let mut config = Config::default();
    config.provider = ProviderKind::Openai;
    f(&mut config);
    let fe = Arc::new(Recorder::default());
    let env = Env::new(EnvInit {
        config,
        provider: Provider::new(ProviderKind::Openai, base, None, 32768),
        model: "mock".into(),
        mode: Mode::Default,
        vision: false,
        cwd: dir.clone(),
        tools: crate::tools::Registry::builtin(),
        mcp: vec![],
        frontend: fe.clone(),
        interactive,
    });
    Fixture { dir, env, fe }
}

/// A project with no model server behind it (commands that never call one).
fn fixture(name: &str) -> Fixture {
    fixture_with(name, "http://127.0.0.1:1/v1", false, |_| {}, |_| {})
}

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn kind(o: &Outcome) -> String {
    match o {
        Outcome::Show(s) => format!("Show({s:?})"),
        Outcome::Prompt(s) => format!("Prompt({s:?})"),
        Outcome::Run(r) => format!("Run({})", r.label()),
        Outcome::Exit => "Exit".into(),
        Outcome::Reset(s) => format!("Reset({s:?})"),
        Outcome::Nothing => "Nothing".into(),
    }
}

fn shown(o: Outcome) -> String {
    match o {
        Outcome::Show(s) => s,
        o => panic!("expected Show, got {}", kind(&o)),
    }
}

fn prompt(o: Outcome) -> String {
    match o {
        Outcome::Prompt(s) => s,
        o => panic!("expected Prompt, got {}", kind(&o)),
    }
}

async fn env_show(env: &Arc<Env>, line: &str) -> String {
    shown(
        handle_env(env, line)
            .await
            .unwrap_or_else(|| panic!("{line} is not an env command")),
    )
}

async fn run(agent: &mut Agent, line: &str) -> String {
    shown(handle(agent, line).await)
}

/// Sessions, shares and approvals live in the home directory, which a test
/// holding `Home` redirects for the whole process: hold the lock while using it.
fn home_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Point `CHANTO_HOME` at a fresh dir for the rest of the test (under
/// `TEST_HOME_LOCK`), restoring the previous value on drop.
struct Home {
    dir: PathBuf,
    previous: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Home {
    fn new(name: &str) -> Home {
        let lock = home_lock();
        let dir = tmpdir(name);
        let previous = std::env::var_os("CHANTO_HOME");
        // SAFETY: test-only, under TEST_HOME_LOCK.
        unsafe { std::env::set_var("CHANTO_HOME", &dir) };
        Home {
            dir,
            previous,
            _lock: lock,
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        // SAFETY: test-only, under TEST_HOME_LOCK.
        unsafe {
            match &self.previous {
                Some(p) => std::env::set_var("CHANTO_HOME", p),
                None => std::env::remove_var("CHANTO_HOME"),
            }
        }
        remove_dir_eventually(&self.dir);
    }
}

// ------------------------------------------------------------ handle_env

#[tokio::test]
async fn quiet_switches_on_off_and_toggles() {
    let fx = fixture("cmd-quiet");
    let env = &fx.env;
    assert!(
        env_show(env, "/quiet on")
            .await
            .contains("Quiet mode **on**")
    );
    assert!(env.quiet());
    assert_eq!(env_show(env, "/quiet off").await, "Quiet mode **off**.");
    assert!(!env.quiet());
    env_show(env, "/quiet").await;
    assert!(env.quiet(), "a bare /quiet toggles");
    assert_eq!(
        env_show(env, "/quiet loud").await,
        "Usage: `/quiet [on|off]`"
    );
    assert!(env.quiet(), "a bad argument changes nothing");
}

#[tokio::test]
async fn keybindings_creates_the_file_once() {
    let home = Home::new("cmd-keys-home");
    let fx = fixture("cmd-keys");
    let first = env_show(&fx.env, "/keybindings").await;
    assert!(first.starts_with("Created `"), "{first}");
    let path = home.dir.join("keybindings.json");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        crate::tui::keybindings::TEMPLATE
    );
    let again = env_show(&fx.env, "/keybindings").await;
    assert!(again.starts_with("Your keybindings:"), "{again}");
}

#[tokio::test]
async fn help_lists_commands_skills_and_shortcuts() {
    let fx = fixture_with(
        "cmd-help",
        "http://127.0.0.1:1/v1",
        false,
        |d| {
            write(
                d,
                ".chanto/commands/deploy.md",
                "---\ndescription: Ship it\nargument-hint: <env>\n---\nDeploy $1",
            );
            write(
                d,
                ".chanto/skills/tidy/SKILL.md",
                "---\nname: tidy\ndescription: Tidy up\n---\nTidy",
            );
        },
        |_| {},
    );
    let s = env_show(&fx.env, "/help").await;
    for part in [
        "## Commands",
        "- `/help` —",
        "## Bundled skills",
        "## Custom commands",
        "- `/deploy <env>` — Ship it",
        "## Skills",
        "- `/tidy` — Tidy up",
        "## Shortcuts",
    ] {
        assert!(s.contains(part), "missing {part:?} in\n{s}");
    }
    assert!(!s.contains("## MCP prompts"), "no MCP servers");
    // `/?` is the same listing.
    assert_eq!(env_show(&fx.env, "/?").await, s);
}

#[tokio::test]
async fn exit_and_unknown_env_commands() {
    let fx = fixture("cmd-exit");
    for c in ["/exit", "/quit", "/q"] {
        assert!(matches!(handle_env(&fx.env, c).await, Some(Outcome::Exit)));
    }
    assert!(handle_env(&fx.env, "/compact").await.is_none());
    assert_eq!(
        env_show(&fx.env, "/goal --bg").await,
        "Usage: `/goal --bg <objective>`"
    );
}

#[tokio::test]
async fn mode_shows_sets_and_rejects() {
    let fx = fixture("cmd-mode");
    assert_eq!(env_show(&fx.env, "/mode").await, "Mode: **default**");
    assert!(matches!(
        handle_env(&fx.env, "/mode plan").await,
        Some(Outcome::Nothing)
    ));
    assert_eq!(fx.env.mode(), Mode::Plan);
    assert!(
        env_show(&fx.env, "/mode sideways")
            .await
            .starts_with("Unknown mode.")
    );
    assert_eq!(fx.env.mode(), Mode::Plan);
}

#[tokio::test]
async fn model_shows_and_switches_for_the_session() {
    let (base, _) = mock_server(vec![]).await;
    let fx = fixture_with("cmd-model", &base, false, |_| {}, |_| {});
    let s = env_show(&fx.env, "/model").await;
    assert!(
        s.starts_with("Model: **mock** (context 32768 tokens, vision off)"),
        "{s}"
    );
    assert_eq!(
        env_show(&fx.env, "/model --session other-model").await,
        "Switched model to **other-model** for this session"
    );
    assert_eq!(fx.env.model(), "other-model");
    // OpenAI-compatible servers report no capabilities, so vision is assumed.
    assert!(fx.env.vision());
}

#[tokio::test]
async fn model_without_a_flag_is_saved_as_the_default() {
    let home = Home::new("cmd-model-save-home");
    let (base, _) = mock_server(vec![]).await;
    let fx = fixture_with("cmd-model-save", &base, false, |_| {}, |_| {});
    let s = env_show(&fx.env, "/model --save saved-model").await;
    assert!(s.contains("saved as the default"), "{s}");
    let table: toml::Table =
        toml::from_str(&std::fs::read_to_string(home.dir.join("config.toml")).unwrap()).unwrap();
    assert_eq!(table["model"].as_str(), Some("saved-model"));
    assert_eq!(fx.env.model(), "saved-model");
}

#[tokio::test]
async fn model_needing_a_login_is_refused() {
    let _home = Home::new("cmd-model-login-home");
    let fx = fixture_with(
        "cmd-model-login",
        "http://127.0.0.1:1/v1",
        false,
        |_| {},
        // Built-in hosted providers are only offered without a custom server.
        |c| c.provider = ProviderKind::Ollama,
    );
    let s = env_show(&fx.env, "/model chanto/some-model").await;
    if std::env::var_os("CHANTO_CLOUD_TOKEN").is_none() {
        assert!(s.starts_with("No "), "{s}");
        assert_eq!(fx.env.model(), "mock", "the model is kept");
    }
}

#[tokio::test]
async fn verify_reports_and_sets_the_scope() {
    let fx = fixture("cmd-verify");
    let s = env_show(&fx.env, "/verify").await;
    assert!(
        s.starts_with("Verified mode: off (start with `--verify`).") && s.contains("related"),
        "{s}"
    );
    let s = env_show(&fx.env, "/verify full").await;
    assert!(s.contains("the full test suite"), "{s}");
    assert_eq!(
        *fx.env.verify_scope.read().unwrap(),
        Some(crate::guardrails::Scope::Full)
    );
    assert_eq!(
        env_show(&fx.env, "/verify most").await,
        "Usage: `/verify [full|related]`"
    );
    let on = fixture_with(
        "cmd-verify-on",
        "http://127.0.0.1:1/v1",
        false,
        |_| {},
        |c| c.guardrails.enabled = true,
    );
    assert!(
        env_show(&on.env, "/verify")
            .await
            .starts_with("Verified mode: on.")
    );
}

#[tokio::test]
async fn think_and_effort() {
    let fx = fixture("cmd-think");
    let env = &fx.env;
    assert_eq!(
        env_show(env, "/think high").await,
        "Thinking enabled, effort high"
    );
    assert!(env.think());
    assert_eq!(env.effort().as_deref(), Some("high"));
    assert_eq!(
        env_show(env, "/effort").await,
        "Effort: high (set with /think low|medium|high|xhigh|max)"
    );
    assert_eq!(env_show(env, "/think off").await, "Thinking disabled");
    assert!(!env.think());
    assert_eq!(
        env_show(env, "/think").await,
        "Thinking enabled, effort high",
        "a bare /think toggles and keeps the effort"
    );
    assert!(
        env_show(env, "/think hard")
            .await
            .starts_with("Usage: /think [on|off|")
    );
    assert_eq!(env_show(env, "/effort false").await, "Thinking disabled");
}

#[tokio::test]
async fn theme_lists_and_rejects_unknown_names() {
    let fx = fixture("cmd-theme");
    let s = env_show(&fx.env, "/theme").await;
    assert!(
        s.starts_with("Theme: **") && s.contains("- `dark`") && s.contains("- `light`"),
        "{s}"
    );
    let s = env_show(&fx.env, "/theme no-such-theme-xyz").await;
    assert!(s.contains("no-such-theme-xyz"), "{s}");
}

#[tokio::test]
async fn vision_switches() {
    let fx = fixture("cmd-vision");
    assert_eq!(env_show(&fx.env, "/vision on").await, "Vision enabled");
    assert!(fx.env.vision());
    assert_eq!(env_show(&fx.env, "/vision").await, "Vision disabled");
    assert_eq!(env_show(&fx.env, "/vision off").await, "Vision disabled");
    assert!(!fx.env.vision());
}

#[tokio::test]
async fn permission_rules_are_added_listed_and_removed() {
    let fx = fixture("cmd-rules");
    let mut agent = Agent::new(fx.env.clone());
    assert_eq!(
        run(&mut agent, "/allow").await,
        "Usage: /allow <rule>, e.g. Bash(npm test:*)"
    );
    assert_eq!(
        run(&mut agent, "/allow Bash(npm test:*)").await,
        "Added allow rule `Bash(npm test:*)` for this session"
    );
    run(&mut agent, "/ask Bash(git push:*)").await;
    run(&mut agent, "/deny Bash(rm:*)").await;
    let s = run(&mut agent, "/permissions").await;
    assert!(
        s.contains("- allow: `Bash(npm test:*)`")
            && s.contains("- ask: `Bash(git push:*)`")
            && s.contains("- deny: `Bash(rm:*)`"),
        "{s}"
    );
    assert_eq!(
        run(&mut agent, "/unallow Bash(npm test:*)").await,
        "Removed allow rule `Bash(npm test:*)`"
    );
    assert_eq!(
        run(&mut agent, "/unask Bash(nope:*)").await,
        "No ask rule matching `Bash(nope:*)`"
    );
    assert_eq!(
        run(&mut agent, "/undeny Bash(rm:*)").await,
        "Removed deny rule `Bash(rm:*)`"
    );
    assert_eq!(run(&mut agent, "/undeny").await, "Usage: /undeny <rule>");
    assert!(
        run(&mut agent, "/permissions")
            .await
            .contains("- allow: (none)")
    );
}

#[tokio::test]
async fn tools_skills_agents_todos_and_shells() {
    let fx = fixture_with(
        "cmd-lists",
        "http://127.0.0.1:1/v1",
        false,
        |d| {
            write(
                d,
                ".chanto/skills/lore/SKILL.md",
                "---\ndescription: Background\nuser-invocable: false\n---\nlore",
            );
            write(
                d,
                ".chanto/skills/manual/SKILL.md",
                "---\ndescription: By hand\ndisable-model-invocation: true\n---\nmanual",
            );
            write(
                d,
                ".chanto/agents/reviewer.md",
                "---\nname: reviewer\ndescription: Reviews diffs\ntools: Read, Grep\ndisallowedTools: Bash\n---\nBe picky.",
            );
        },
        |_| {},
    );
    let env = &fx.env;
    let tools = env_show(env, "/tools").await;
    assert!(
        tools.starts_with("## Tools\n") && tools.contains("- **Read** —"),
        "{tools}"
    );
    let skills = env_show(env, "/skills").await;
    assert!(
        skills.contains("- **lore** (model only) — Background")
            && skills.contains("- **manual** (only as /name) — By hand"),
        "{skills}"
    );
    let agents = env_show(env, "/agents").await;
    assert!(
        agents.contains("- **Explore**") && agents.contains("- **reviewer** — Reviews diffs"),
        "{agents}"
    );
    let def = env_show(env, "/agents reviewer").await;
    assert!(
        def.starts_with("## reviewer\nReviews diffs")
            && def.contains("**Allowed tools:** Read, Grep except Bash")
            && def.ends_with("Be picky."),
        "{def}"
    );
    assert_eq!(
        env_show(env, "/agents nobody").await,
        "No subagent named `nobody`."
    );
    assert_eq!(env_show(env, "/todos").await, "No todos.");
    *env.todos.lock().unwrap() = ["completed", "in_progress", "pending"]
        .iter()
        .map(|s| crate::tools::misc::Todo {
            content: format!("task {s}"),
            status: s.to_string(),
            active_form: String::new(),
        })
        .collect();
    assert_eq!(
        env_show(env, "/todos").await,
        "- [x] task completed\n- [~] task in_progress\n- [ ] task pending"
    );
    assert_eq!(env_show(env, "/bashes").await, "No background shells.");

    let empty = fixture("cmd-lists-empty");
    assert!(
        env_show(&empty.env, "/skills")
            .await
            .starts_with("No skills found")
    );
}

#[tokio::test]
async fn mcp_without_servers() {
    let fx = fixture("cmd-mcp");
    let env = &fx.env;
    assert!(
        env_show(env, "/mcp")
            .await
            .starts_with("No MCP servers configured.")
    );
    assert_eq!(
        env_show(env, "/mcp login").await,
        "Usage: /mcp login <server> (remote servers: none)"
    );
    assert_eq!(
        env_show(env, "/mcp logout ghost").await,
        "No MCP server named `ghost`."
    );
    assert_eq!(
        env_show(env, "/mcp reconnect").await,
        "Usage: `/mcp reconnect <server>|all`"
    );
    assert_eq!(
        env_show(env, "/mcp reconnect all").await,
        "No MCP servers to reconnect."
    );
    assert_eq!(
        env_show(env, "/mcp disable ghost").await,
        "No MCP server named `ghost`."
    );
    assert_eq!(mcp_summary(env), (0, 0, false));
}

#[tokio::test]
async fn mcp_rows_cover_configured_disabled_and_pending_servers() {
    let _lock = home_lock();
    let fx = fixture_with(
        "cmd-mcp-rows",
        "http://127.0.0.1:1/v1",
        false,
        |d| {
            write(
                d,
                ".mcp.json",
                r#"{"mcpServers":{"fromjson":{"command":"true"}}}"#,
            );
        },
        |c| {
            c.mcp_servers.insert(
                "off".into(),
                crate::config::McpServerConfig {
                    command: "true".into(),
                    disabled: true,
                    ..Default::default()
                },
            );
            c.mcp_servers.insert(
                "down".into(),
                crate::config::McpServerConfig {
                    url: Some("http://127.0.0.1:1/mcp".into()),
                    ..Default::default()
                },
            );
            c.origins
                .insert("mcp_servers.off".into(), PathBuf::from("/etc/chanto.toml"));
        },
    );
    let rows = mcp_rows(&fx.env);
    let row = |n: &str| rows.iter().find(|r| r.name == n).unwrap();
    assert!(row("off").state == McpState::Off);
    assert_eq!(row("off").detail, "`disabled = true` in `/etc/chanto.toml`");
    assert!(row("down").state == McpState::Failed && row("down").http);
    assert_eq!(row("down").detail, "not connected");
    assert!(
        row("fromjson").state == McpState::Pending,
        "untrusted .mcp.json"
    );
    // A disabled server stays out of the footer's count.
    assert_eq!(mcp_summary(&fx.env), (0, 1, false));
    let listing = env_show(&fx.env, "/mcp").await;
    assert!(
        listing.starts_with("## MCP servers\n") && listing.contains("- **fromjson** ⏸"),
        "{listing}"
    );
    assert_eq!(
        env_show(&fx.env, "/mcp login down").await,
        "`down` didn't connect at startup (unreachable or misconfigured), so there is nothing to log in to. Check `/mcp`."
    );
    // `.mcp.json` servers waiting for approval are approved or rejected here.
    let s = env_show(&fx.env, "/mcp disable fromjson").await;
    assert_eq!(s, "Rejected `fromjson` from .mcp.json for this project.");
    let rows = mcp_rows(&fx.env);
    assert!(
        rows.iter()
            .any(|r| r.name == "fromjson" && r.state == McpState::Rejected)
    );
    let s = env_show(&fx.env, "/mcp enable fromjson").await;
    assert!(s.starts_with("Approved `fromjson`"), "{s}");
    let status = status(&Agent::new(fx.env.clone()));
    assert!(status.contains("off ⊘ disabled"), "{status}");
}

#[tokio::test]
async fn side_questions_answer_without_changing_the_history() {
    let (base, seen) = mock_server(vec![json!({"content": "  forty-two  "})]).await;
    let fx = fixture_with("cmd-btw", &base, false, |_| {}, |_| {});
    let mut agent = Agent::new(fx.env.clone());
    assert!(
        run(&mut agent, "/btw")
            .await
            .starts_with("Ask a side question")
    );
    assert_eq!(
        env_show(&fx.env, "/btw fork").await,
        "No side question to fork yet: ask one with `/btw <question>`."
    );
    let before = agent.messages.len();
    assert_eq!(
        run(&mut agent, "/btw what is the answer?").await,
        "**/btw what is the answer?**\n\nforty-two"
    );
    assert_eq!(agent.messages.len(), before);
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(
        run(&mut agent, "/btw").await,
        "**what is the answer?**\n\nforty-two"
    );
    // A failing server is reported, not panicked on.
    let down = fixture("cmd-btw-down");
    assert!(
        env_show(&down.env, "/btw anything")
            .await
            .starts_with("Side question failed:")
    );
}

// ------------------------------------------------------------ handle

#[tokio::test]
async fn agent_commands_need_an_external_agent() {
    let fx = fixture("cmd-acp");
    let mut agent = Agent::new(fx.env.clone());
    for c in [
        "/agent-mode",
        "/agent-config x y",
        "/agent-login",
        "/agent-logout",
    ] {
        assert_eq!(
            run(&mut agent, c).await,
            "This session is not driving an external ACP agent.",
            "{c}"
        );
    }
    // Only an ACP session offers them for completion.
    assert!(
        !completions(&agent)
            .iter()
            .any(|(n, _)| n.starts_with("/agent-"))
    );
}

#[tokio::test]
async fn prompts_and_long_runs() {
    let fx = fixture("cmd-prompts");
    let mut agent = Agent::new(fx.env.clone());
    assert_eq!(
        run(&mut agent, "/customize").await,
        "Usage: `/customize <request>`"
    );
    assert_eq!(
        prompt(handle(&mut agent, "/customize add a /hello command").await),
        "Customize chanto: add a /hello command"
    );
    assert!(
        run(&mut agent, "/outcome")
            .await
            .starts_with("Usage: `/outcome")
    );
    let label = |o: Outcome| match o {
        Outcome::Run(r) => r.label(),
        o => panic!("expected Run, got {}", kind(&o)),
    };
    assert_eq!(
        label(handle(&mut agent, "/build a CLI").await),
        "Outcome mode"
    );
    assert_eq!(label(handle(&mut agent, "/goal ship it").await), "Goal");
    assert_eq!(label(handle(&mut agent, "/team split it").await), "Team");
    match handle(&mut agent, "/goal resume abc").await {
        Outcome::Run(LongRun::GoalResume(Some(id))) => assert_eq!(id, "abc"),
        o => panic!("expected a goal resume, got {}", kind(&o)),
    }
    assert!(matches!(
        handle(&mut agent, "/goal resume").await,
        Outcome::Run(LongRun::GoalResume(None))
    ));
    // "resumes" is an objective, not the resume subcommand.
    assert!(matches!(
        handle(&mut agent, "/goal resumes work").await,
        Outcome::Run(LongRun::Goal(o)) if o == "resumes work"
    ));
    assert_eq!(
        prompt(handle(&mut agent, "/init").await),
        crate::context::INIT_PROMPT
    );
    let review = prompt(handle(&mut agent, "/security-review auth only").await);
    assert!(review.contains("this branch (auth only)"), "{review}");
    assert!(
        prompt(handle(&mut agent, "/pr-comments").await)
            .contains("the current branch's pull request")
    );
    assert!(prompt(handle(&mut agent, "/pr-comments 12").await).contains("pull request 12"));
}

#[tokio::test]
async fn learned_lessons_round_trip() {
    let fx = fixture("cmd-learn");
    let mut agent = Agent::new(fx.env.clone());
    assert!(
        run(&mut agent, "/learned")
            .await
            .starts_with("No lessons yet (0 signals recorded).")
    );
    let added = run(&mut agent, "/learned add Always run cargo fmt").await;
    assert!(added.starts_with("Added lesson `"), "{added}");
    let list = run(&mut agent, "/learn").await;
    assert!(
        list.starts_with("## Lessons\n")
            && list.contains("[active] Always run cargo fmt")
            && list.contains("0 signals recorded."),
        "{list}"
    );
    let off = run(&mut agent, "/learned disable all").await;
    assert!(off.starts_with("1 lesson(s) now disabled."), "{off}");
    assert!(run(&mut agent, "/learned").await.contains("[disabled]"));
    let on = run(&mut agent, "/learned enable all").await;
    assert!(on.starts_with("1 lesson(s) now active."), "{on}");
    assert_eq!(
        run(&mut agent, "/learned accept nope").await,
        "No matching lesson for `nope`."
    );
    assert!(
        run(&mut agent, "/learned undo")
            .await
            .starts_with("Undid the last change")
    );
    assert!(
        run(&mut agent, "/learned signals")
            .await
            .starts_with("## Recorded signals (0 total, newest last)")
    );
}

#[tokio::test]
async fn tasks_hooks_and_plugins() {
    let fx = fixture("cmd-tasks");
    let mut agent = Agent::new(fx.env.clone());
    assert!(
        run(&mut agent, "/tasks")
            .await
            .starts_with("No background agents.")
    );
    assert_eq!(
        run(&mut agent, "/tasks stop t-1").await,
        "No running agent `t-1`"
    );
    assert!(
        run(&mut agent, "/hooks")
            .await
            .starts_with("No hooks configured.")
    );
    assert_eq!(
        run(&mut agent, "/plugin install \"unterminated").await,
        "missing closing quote"
    );
}

#[tokio::test]
async fn hooks_are_listed_and_test_run() {
    let fx = fixture_with(
        "cmd-hooks",
        "http://127.0.0.1:1/v1",
        false,
        |_| {},
        |c| {
            c.hooks.insert(
                "Stop".into(),
                vec![
                    crate::config::HookConfig {
                        command: "echo from-the-hook".into(),
                        ..Default::default()
                    },
                    crate::config::HookConfig {
                        kind: "http".into(),
                        url: "http://127.0.0.1:1/hook".into(),
                        matcher: "Bash".into(),
                        ..Default::default()
                    },
                ],
            );
        },
    );
    let mut agent = Agent::new(fx.env.clone());
    let list = run(&mut agent, "/hooks").await;
    assert!(
        list.contains("- **Stop**: `echo from-the-hook`")
            && list.contains("- **Stop** `Bash`: `POST http://127.0.0.1:1/hook`"),
        "{list}"
    );
    let out = run(&mut agent, "/hooks test stop 0").await;
    assert!(
        out.starts_with("## stop → `echo from-the-hook`\n\nexit: 0")
            && out.contains("**stdout**\n```\nfrom-the-hook"),
        "{out}"
    );
    assert!(
        run(&mut agent, "/hooks test Stop 1")
            .await
            .contains("is an http hook")
    );
    assert_eq!(
        run(&mut agent, "/hooks test Stop 7").await,
        "`Stop` has 2 hook(s)."
    );
    assert_eq!(
        run(&mut agent, "/hooks test Nope 0").await,
        "No hooks for event `Nope`."
    );
}

#[tokio::test]
async fn clear_then_rewind_restores_the_conversation() {
    let _lock = home_lock();
    let fx = fixture("cmd-clear");
    let mut agent = Agent::new(fx.env.clone());
    agent.session = Some(Session::new(&fx.env.cwd(), "mock"));
    // Nothing said yet: nothing to undo.
    assert!(matches!(
        handle(&mut agent, "/clear").await,
        Outcome::Reset(None)
    ));
    agent
        .messages
        .push(Message::user("remember the word kumquat"));
    agent.messages.push(Message::assistant("kumquat noted"));
    let first = agent.session.as_ref().unwrap().id.clone();
    match handle(&mut agent, "/new").await {
        Outcome::Reset(Some(note)) => {
            assert_eq!(note, "Conversation cleared — `/rewind` restores it.")
        }
        o => panic!("expected Reset, got {}", kind(&o)),
    }
    assert!(!agent.messages.iter().any(|m| m.content.contains("kumquat")));
    assert_ne!(agent.session.as_ref().unwrap().id, first);
    match handle(&mut agent, "/rewind").await {
        Outcome::Reset(Some(note)) => assert!(
            note.starts_with("↩ Restored the conversation you cleared"),
            "{note}"
        ),
        o => panic!("expected Reset, got {}", kind(&o)),
    }
    assert_eq!(agent.session.as_ref().unwrap().id, first);
    assert!(agent.messages.iter().any(|m| m.content.contains("kumquat")));
    // Restored once; there is nothing more to rewind.
    assert_eq!(run(&mut agent, "/rewind").await, "Nothing to rewind.");
}

#[tokio::test]
async fn rewind_undo_redo_and_branching_need_history() {
    let _lock = home_lock();
    let fx = fixture("cmd-rewind");
    let mut agent = Agent::new(fx.env.clone());
    assert_eq!(run(&mut agent, "/undo").await, "Nothing to rewind.");
    assert!(
        run(&mut agent, "/rewind undo")
            .await
            .starts_with("Undo failed")
    );
    assert!(matches!(
        handle(&mut agent, "/redo").await,
        Outcome::Show(_)
    ));
    assert!(
        run(&mut agent, "/edit-prompt")
            .await
            .starts_with("Usage: `/edit-prompt <checkpoint>")
    );
    assert!(
        run(&mut agent, "/edit-prompt 3 both again")
            .await
            .starts_with("Couldn't branch:")
    );
    assert_eq!(
        run(&mut agent, "/fork x --at many").await,
        "Usage: /fork [name] --at <checkpoint index>"
    );
}

#[tokio::test]
async fn rewind_lists_checkpoints_and_validates_arguments() {
    let _lock = home_lock();
    let fx = fixture("cmd-rewind-list");
    let mut agent = Agent::new(fx.env.clone());
    agent.session = Some(Session::new(&fx.env.cwd(), "mock"));
    agent.messages.push(Message::system("sys"));
    agent.messages.push(Message::user("first prompt"));
    fx.env
        .checkpoints
        .lock()
        .unwrap()
        .push(crate::agent::Checkpoint {
            id: "c0".into(),
            timestamp: String::new(),
            tree: None,
            msg_index: 1,
            prompt: "first prompt".into(),
            files: Default::default(),
        });
    assert_eq!(
        run(&mut agent, "/rewind").await,
        "## Checkpoints (rewind with `/rewind <n>`)\n0. first prompt\n"
    );
    assert_eq!(run(&mut agent, "/rewind x").await, "Usage: /rewind <n>");
    assert_eq!(
        run(&mut agent, "/rewind 0 sideways").await,
        "Usage: /rewind <n> [both|conversation|code]"
    );
    match handle(&mut agent, "/rewind 0 conversation").await {
        Outcome::Reset(Some(note)) => assert!(
            note.starts_with("⏪ Rewound to before \"first prompt\""),
            "{note}"
        ),
        o => panic!("expected Reset, got {}", kind(&o)),
    }
    assert!(!agent.messages.iter().any(|m| *m.content == "first prompt"));
}

#[tokio::test]
async fn sessions_are_listed_renamed_and_resumed() {
    let _lock = home_lock();
    let fx = fixture("cmd-sessions");
    let mut agent = Agent::new(fx.env.clone());
    assert_eq!(
        run(&mut agent, "/rename").await,
        "This session isn't saved, so it has no title."
    );
    assert_eq!(
        run(&mut agent, "/resume").await,
        "No saved sessions for this directory."
    );
    agent.session = Some(Session::new(&fx.env.cwd(), "mock"));
    agent.messages.push(Message::system("sys"));
    agent.messages.push(Message::user("plan the migration"));
    agent.save_session();
    let id = agent.session.as_ref().unwrap().id.clone();
    assert!(
        run(&mut agent, "/rename")
            .await
            .starts_with("Session title: **plan the migration**")
    );
    assert_eq!(
        run(&mut agent, "/title Migration plan").await,
        "Session renamed to **Migration plan**"
    );
    let list = run(&mut agent, "/sessions").await;
    assert!(
        list.starts_with("## Sessions") && list.contains(&format!("`{id}`")),
        "{list}"
    );
    assert!(list.contains("Migration plan"), "{list}");
    assert!(
        run(&mut agent, "/resume --search Migration")
            .await
            .contains(&id)
    );
    assert_eq!(
        run(&mut agent, "/resume -s zebra").await,
        "No saved sessions match `zebra`."
    );
    assert!(
        run(&mut agent, "/resume no-such-id")
            .await
            .starts_with("Could not resume:")
    );
    handle(&mut agent, "/clear").await;
    match handle(&mut agent, &format!("/resume {id}")).await {
        Outcome::Reset(Some(note)) => assert_eq!(note, "Resumed session: Migration plan"),
        o => panic!("expected Reset, got {}", kind(&o)),
    }
    assert_eq!(agent.session.as_ref().unwrap().id, id);
    assert!(
        agent
            .messages
            .iter()
            .any(|m| *m.content == "plan the migration")
    );
    match handle(&mut agent, "/fork side-quest").await {
        Outcome::Reset(Some(note)) => assert!(note.starts_with("Forked session: "), "{note}"),
        o => panic!("expected Reset, got {}", kind(&o)),
    }
    assert_ne!(agent.session.as_ref().unwrap().id, id);
}

#[tokio::test]
async fn share_and_export_write_files() {
    let _lock = home_lock();
    let fx = fixture("cmd-export");
    let mut agent = Agent::new(fx.env.clone());
    assert_eq!(
        run(&mut agent, "/share --everything").await,
        "Usage: /share [--no-tool-output]"
    );
    assert_eq!(
        run(&mut agent, "/share").await,
        "Share failed: no current session"
    );
    agent.session = Some(Session::new(&fx.env.cwd(), "mock"));
    agent.messages = vec![
        Message::system("sys"),
        Message::user("hello<system-reminder>hidden</system-reminder>"),
        Message::assistant("hi there"),
    ];
    let s = run(&mut agent, "/share --no-tool-output").await;
    let path = s.strip_prefix("Shared to `").unwrap().trim_end_matches('`');
    assert!(Path::new(path).exists(), "{s}");
    let s = run(&mut agent, "/export notes/out.md").await;
    let out = fx.dir.join("notes/out.md");
    assert_eq!(s, format!("Exported to `{}`", out.display()));
    let md = std::fs::read_to_string(out).unwrap();
    assert_eq!(
        md,
        "# Conversation\n\n## User\n\nhello\n\n## Assistant\n\nhi there\n\n"
    );
    let s = run(&mut agent, "/export").await;
    assert!(s.contains("chanto-conversation-"), "{s}");
}

#[test]
fn export_markdown_shows_tool_calls_and_masks_secrets() {
    let fx = fixture_sync("cmd-export-md");
    let mut agent = Agent::new(fx.env.clone());
    let key = format!("sk-ant-api03-{}", "Q".repeat(40));
    let mut call = Message::assistant("");
    call.tool_calls = vec![ToolCall {
        id: "c1".into(),
        name: "Bash".into(),
        arguments: json!({"command": "ls"}),
    }];
    let mut result = Message::new(Role::Tool, format!("a.txt\nkey {key}"));
    result.tool_call_id = Some("c1".into());
    agent.messages = vec![Message::system("sys"), Message::user("list"), call, result];
    let md = export_markdown(&agent);
    assert!(
        md.contains("**Tool call:** `Bash` `{\"command\":\"ls\"}`"),
        "{md}"
    );
    assert!(md.contains("<details><summary>Result</summary>"), "{md}");
    assert!(!md.contains(&key), "{md}");
    // The plain REPL's transcript: each call with its result's first line.
    assert_eq!(last_turn_steps(&agent), "## Last turn\n- `Bash(ls)`: a.txt");
}

/// `fixture` for synchronous tests: `Env::new` needs a runtime.
fn fixture_sync(name: &str) -> Fixture {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _g = rt.enter();
    fixture(name)
}

#[tokio::test]
async fn transcript_and_copy_with_nothing_to_show() {
    let fx = fixture("cmd-transcript");
    let mut agent = Agent::new(fx.env.clone());
    assert_eq!(
        run(&mut agent, "/transcript").await,
        "No steps in the last turn."
    );
    assert_eq!(run(&mut agent, "/copy").await, "Nothing to copy.");
}

#[tokio::test]
async fn memory_to_agents_md() {
    let fx = fixture("cmd-memory");
    let mut agent = Agent::new(fx.env.clone());
    std::fs::write(fx.dir.join("AGENTS.md"), "# Notes").unwrap();
    let s = run(&mut agent, "/memory --agents use tabs").await;
    assert!(s.starts_with("Saved to `"), "{s}");
    assert_eq!(
        std::fs::read_to_string(fx.dir.join("AGENTS.md")).unwrap(),
        "# Notes\n- use tabs\n"
    );
    let key = format!("sk-ant-api03-{}", "Q".repeat(40));
    assert_eq!(
        run(&mut agent, &format!("/remember --agents the key is {key}")).await,
        "Refusing to store a secret"
    );
    // Without `--agents` it goes through the memory store.
    assert!(matches!(
        handle(&mut agent, "/memory").await,
        Outcome::Show(_)
    ));
}

#[tokio::test]
async fn config_shows_files_and_sets_session_settings() {
    let fx = fixture_with(
        "cmd-config",
        "http://127.0.0.1:1/v1",
        false,
        |d| write(d, ".chanto/config.toml", "mode = \"plan\"\n"),
        |_| {},
    );
    let mut agent = Agent::new(fx.env.clone());
    let s = run(&mut agent, "/config").await;
    assert!(
        s.starts_with("## Config files (lowest → highest precedence)\n")
            && s.contains(".chanto/config.toml`")
            && s.contains("- provider: OpenAI-compatible at `http://127.0.0.1:1/v1`"),
        "{s}"
    );
    assert!(
        run(&mut agent, "/config colour red")
            .await
            .starts_with("No session setting `colour`.")
    );
    assert_eq!(
        run(&mut agent, "/config output_style").await,
        "Usage: `/config output-style <value>`"
    );
    assert!(matches!(
        handle(&mut agent, "/config mode acceptEdits").await,
        Outcome::Nothing
    ));
    assert_eq!(fx.env.mode(), Mode::AcceptEdits);
    assert_eq!(run(&mut agent, "/config vision on").await, "Vision enabled");
}

#[tokio::test]
async fn output_styles_switch() {
    let fx = fixture("cmd-style");
    let mut agent = Agent::new(fx.env.clone());
    let s = run(&mut agent, "/output-style").await;
    assert!(
        s.starts_with("Output style: **Default**.") && s.contains("- Explanatory:"),
        "{s}"
    );
    assert_eq!(
        run(&mut agent, "/output-style explanatory").await,
        "Output style: **Explanatory** (set `output_style = \"Explanatory\"` in config to keep it)"
    );
    assert_eq!(
        fx.env.output_style.read().unwrap().as_deref(),
        Some("Explanatory")
    );
    assert!(
        run(&mut agent, "/output-style shouty")
            .await
            .starts_with("No output style named `shouty`.")
    );
    run(&mut agent, "/output-style default").await;
    assert_eq!(*fx.env.output_style.read().unwrap(), None);
}

#[tokio::test]
async fn reports_status_context_cost_and_insights() {
    let fx = fixture("cmd-reports");
    let mut agent = Agent::new(fx.env.clone());
    let s = run(&mut agent, "/status").await;
    assert!(
        s.starts_with(&format!(
            "## Status\n- chanto **v{}**",
            env!("CARGO_PKG_VERSION")
        )) && s.contains("- model: **mock**")
            && s.contains("- session: not saved")
            && s.contains("- MCP: none configured"),
        "{s}"
    );
    let s = run(&mut agent, "/context").await;
    assert!(
        s.starts_with("## Context\n`") && s.contains("- model: mock · mode: default"),
        "{s}"
    );
    assert!(matches!(
        handle(&mut agent, "/context skills").await,
        Outcome::Show(_)
    ));
    assert!(matches!(
        handle(&mut agent, "/skill-doctor").await,
        Outcome::Show(_)
    ));
    let s = run(&mut agent, "/cost").await;
    assert!(
        s.starts_with("Tokens this session: **0** in · **0** out")
            && s.contains("_No account limits reported by this provider._"),
        "{s}"
    );
    assert!(matches!(
        handle(&mut agent, "/usage today").await,
        Outcome::Show(_)
    ));
    assert!(matches!(
        handle(&mut agent, "/insights").await,
        Outcome::Show(_)
    ));
    let s = run(&mut agent, "/privacy-settings").await;
    assert!(
        s.starts_with("## Privacy")
            && s.contains("- Nothing: the model runs at `http://127.0.0.1:1/v1`"),
        "{s}"
    );
    let s = run(&mut agent, "/release-notes").await;
    assert!(
        s.contains(env!("CARGO_PKG_VERSION")) && s.contains("chanto upgrade --check"),
        "{s}"
    );
}

#[tokio::test]
async fn privacy_names_a_remote_server() {
    let fx = fixture_with(
        "cmd-privacy",
        "https://models.example.com/v1",
        false,
        |_| {},
        |c| c.telemetry.otlp_endpoint = Some("http://collector:4318".into()),
    );
    let s = privacy_settings(&fx.env);
    assert!(
        s.contains("Opt-in OTLP export is enabled")
            && s.contains("- Your prompts, attached files and tool results go to `https://models.example.com/v1`")
            && s.contains("Web search/fetch"),
        "{s}"
    );
}

#[tokio::test]
async fn login_logout_and_sandbox_without_side_effects() {
    let _home = Home::new("cmd-login-home");
    let fx = fixture("cmd-login");
    let mut agent = Agent::new(fx.env.clone());
    assert!(run(&mut agent, "/login").await.starts_with("```\n"));
    assert_eq!(
        run(&mut agent, "/login openai --as bad!name").await,
        "Account names are letters, digits, `.`, `_` and `-`."
    );
    assert!(
        run(&mut agent, "/login groq")
            .await
            .starts_with("To save an API key for `groq`")
    );
    assert_eq!(
        run(&mut agent, "/logout groq").await,
        "Not logged in to `groq`. Usage: /logout <provider>"
    );
    assert_eq!(
        run(&mut agent, "/sandbox maybe").await,
        "Usage: /sandbox [on|off]"
    );
    let s = run(&mut agent, "/sandbox").await;
    assert!(
        s.starts_with("Sandbox for shell commands: ") && s.contains("- Can write: the project"),
        "{s}"
    );
}

#[tokio::test]
async fn tui_only_commands_explain_themselves() {
    let fx = fixture("cmd-tui-only");
    let mut agent = Agent::new(fx.env.clone());
    assert!(
        run(&mut agent, "/loop 5m x")
            .await
            .contains("interactive TUI")
    );
    assert!(run(&mut agent, "/vim").await.contains("Vim keybindings"));
    assert!(run(&mut agent, "/voice").await.starts_with("Voice input"));
    assert!(matches!(
        handle(&mut agent, "/diagnostics").await,
        Outcome::Show(_)
    ));
    assert_eq!(
        run(&mut agent, "/add-dir does/not/exist").await,
        format!(
            "Not a directory: `{}`",
            fx.dir.join("does/not/exist").display()
        )
    );
}

#[tokio::test]
async fn terminal_setup_per_terminal() {
    let _lock = home_lock();
    let compat = tmpdir("cmd-term-compat");
    let saved = (
        std::env::var_os("TERM_PROGRAM"),
        std::env::var_os("CHANTO_COMPAT_HOME"),
    );
    let set = |k: &str, v: Option<&std::ffi::OsStr>| unsafe {
        // SAFETY: test-only, under TEST_HOME_LOCK.
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    };
    set("CHANTO_COMPAT_HOME", Some(compat.as_os_str()));
    set("TERM_PROGRAM", Some("iTerm.app".as_ref()));
    assert!(shown(terminal_setup()).contains("iTerm2 → Settings"));
    set("TERM_PROGRAM", Some("WezTerm".as_ref()));
    assert!(shown(terminal_setup()).contains("WezTerm already sends"));
    set("TERM_PROGRAM", None);
    assert!(shown(terminal_setup()).starts_with("**Terminal setup (unknown)"));
    // VS Code: the binding is written to its keybindings file, once.
    set("TERM_PROGRAM", Some("vscode".as_ref()));
    let kb = compat.join("Library/Application Support/Code/User/keybindings.json");
    std::fs::create_dir_all(kb.parent().unwrap()).unwrap();
    std::fs::write(&kb, "// mine\n[{\"key\": \"ctrl+k\"}]").unwrap();
    assert!(shown(terminal_setup()).starts_with("Added the shift+enter binding"));
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&kb).unwrap()).unwrap();
    assert_eq!(v[0]["key"], "ctrl+k");
    assert_eq!(v[1]["key"], "shift+enter");
    assert_eq!(
        shown(terminal_setup()),
        "shift+enter is already bound in VS Code."
    );
    set("TERM_PROGRAM", saved.0.as_deref());
    set("CHANTO_COMPAT_HOME", saved.1.as_deref());
    remove_dir_eventually(&compat);
}

#[tokio::test]
async fn diff_reports_repository_state() {
    let fx = fixture("cmd-diff");
    let mut agent = Agent::new(fx.env.clone());
    assert_eq!(run(&mut agent, "/diff").await, "Not a git repository.");
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(&fx.dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    std::fs::write(fx.dir.join("a.txt"), "one\n").unwrap();
    git(&["add", "a.txt"]);
    git(&["commit", "-q", "-m", "init"]);
    assert_eq!(run(&mut agent, "/diff").await, "No changes.");
    std::fs::write(fx.dir.join("a.txt"), "two\n").unwrap();
    std::fs::write(fx.dir.join("new.txt"), "x\n").unwrap();
    let s = run(&mut agent, "/diff").await;
    assert!(
        s.contains("a.txt | 2 +-")
            && s.contains("Untracked:\n```\nnew.txt\n```")
            && s.contains("```diff\n")
            && s.contains("+two"),
        "{s}"
    );
}

#[tokio::test]
async fn unknown_commands_suggest_the_closest() {
    let fx = fixture("cmd-unknown");
    let mut agent = Agent::new(fx.env.clone());
    assert!(matches!(
        handle(&mut agent, "/comapct").await,
        Outcome::Nothing
    ));
    assert!(matches!(
        handle(&mut agent, "/xyzzyplugh").await,
        Outcome::Nothing
    ));
    assert_eq!(
        fx.fe.notes(),
        [
            "Unknown command /comapct. Did you mean /compact?",
            "Unknown command /xyzzyplugh — try /help"
        ]
    );
}

#[tokio::test]
async fn custom_commands_expand_and_resolve_by_suffix() {
    let fx = fixture_with(
        "cmd-custom",
        "http://127.0.0.1:1/v1",
        false,
        |d| {
            write(d, ".chanto/commands/tools/lint.md", "Lint $ARGUMENTS");
            write(d, ".chanto/commands/greet.md", "Hi !`echo should-not-run`");
        },
        |_| {},
    );
    let mut agent = Agent::new(fx.env.clone());
    let names: Vec<String> = completions(&agent).into_iter().map(|(n, _)| n).collect();
    assert!(names.iter().any(|n| n == "/tools:lint"), "{names:?}");
    assert_eq!(
        prompt(handle(&mut agent, "/lint src").await),
        "Lint src",
        "the bare name works when unambiguous"
    );
    assert_eq!(
        prompt(handle(&mut agent, "/prompts:tools:lint x").await),
        "Lint x"
    );
    // A project command's !`cmd` blocks need a trusted folder.
    assert_eq!(
        prompt(handle(&mut agent, "/greet").await),
        "Hi !`echo should-not-run`"
    );
    assert!(
        fx.fe.notes()[0].contains("isn't trusted; its !`command` blocks were not run"),
        "{:?}",
        fx.fe.notes()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn command_arguments_cannot_add_inline_commands() {
    let _home = Home::new("cmd-args-inline-home");
    let fx = fixture_with(
        "cmd-args-inline",
        "http://127.0.0.1:1/v1",
        false,
        |d| {
            write(
                d,
                ".chanto/commands/show.md",
                "Args: !`printf '[%s]' \"$ARGUMENTS\"` $ARGUMENTS",
            )
        },
        |_| {},
    );
    crate::config::trust(&fx.dir).unwrap();
    let mut agent = Agent::new(fx.env.clone());
    let out = prompt(handle(&mut agent, "/show a !`touch pwned`").await);
    assert_eq!(out, "Args: [a !`touch pwned`] a !`touch pwned`");
    assert!(!fx.dir.join("pwned").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn command_blocks_get_positional_and_named_arguments_as_data() {
    let dir = tmpdir("cmd-inline-args");
    let body = "!`printf '<%s|%s|%s>' \"$1\" \"$2\" \"$TICKET\"`";
    let out = expand_inline_bash(body, &dir, None, "'a;b' \"$(touch x)\" TICKET=ABC-1", true).await;
    assert_eq!(out, "<a;b|$(touch x)|ABC-1>");
    assert!(!dir.join("x").exists());
    // An argument can't replace a variable the environment already has.
    let out = expand_inline_bash("!`echo \"$HOME\"`", &dir, None, "HOME=/evil", true).await;
    assert_ne!(out, "/evil");
    remove_dir_eventually(&dir);
}

#[tokio::test]
async fn inline_commands_run_and_report_their_output() {
    let dir = tmpdir("cmd-inline");
    let out = expand_inline_bash("A !`echo one`\n!`echo two 1>&2` B", &dir, None, "", false).await;
    assert_eq!(out, "A one\ntwo B");
    // A `!` glued to a word isn't a command.
    assert_eq!(
        expand_inline_bash("x!`echo no`", &dir, None, "", false).await,
        "x!`echo no`"
    );
    remove_dir_eventually(&dir);
}

#[tokio::test]
async fn forked_skills_run_in_a_subagent() {
    let (base, _) = mock_server(vec![json!({"content": "subagent report"})]).await;
    let fx = fixture_with(
        "cmd-fork-skill",
        &base,
        false,
        |d| {
            write(
                d,
                ".chanto/skills/survey/SKILL.md",
                "---\ndescription: Survey\ncontext: fork\nagent: Explore\n---\nSurvey $ARGUMENTS",
            );
        },
        |_| {},
    );
    let mut agent = Agent::new(fx.env.clone());
    let p = prompt(handle(&mut agent, "/survey the repo").await);
    assert!(
        p.starts_with("The /survey skill ran in a Explore subagent. Its report:")
            && p.contains("subagent report"),
        "{p}"
    );
}

#[tokio::test]
async fn skill_frontmatter_applies_to_the_turn() {
    let fx = fixture("cmd-activate");
    let meta = crate::context::SkillMeta {
        allowed_tools: vec!["Bash(ls:*)".into()],
        model: Some("inherit".into()),
        effort: Some("high".into()),
        ..Default::default()
    };
    activate_skill(&fx.env, "user-skill", &meta, false, true, false);
    {
        let turn = fx.env.skill_turn.lock().unwrap();
        assert_eq!(turn.allow, ["Bash(ls:*)"]);
        assert_eq!(turn.effort.as_deref(), Some("high"));
        assert!(turn.model.is_some());
    }
    // A typed `/name` arms the turn about to start; the turn after clears it.
    fx.env.skill_turn.lock().unwrap().begin_turn();
    assert_eq!(fx.env.skill_turn.lock().unwrap().allow, ["Bash(ls:*)"]);
    fx.env.skill_turn.lock().unwrap().begin_turn();
    assert!(fx.env.skill_turn.lock().unwrap().allow.is_empty());
    // An untrusted project's skill grants nothing, and a bad effort is ignored.
    let meta = crate::context::SkillMeta {
        allowed_tools: vec!["Bash".into()],
        effort: Some("ludicrous".into()),
        ..Default::default()
    };
    activate_skill(&fx.env, "proj-skill", &meta, true, false, false);
    assert!(fx.env.skill_turn.lock().unwrap().allow.is_empty());
    assert_eq!(fx.env.skill_turn.lock().unwrap().effort, None);
    let notes = fx.fe.notes();
    assert!(
        notes[0].contains("its allowed-tools were not pre-approved")
            && notes[1].starts_with("/proj-skill: ignoring effort `ludicrous`"),
        "{notes:?}"
    );
}

#[tokio::test]
async fn mentions_attach_files_directories_and_images() {
    let fx = fixture("cmd-mentions");
    std::fs::write(fx.dir.join("notes.txt"), "the notes").unwrap();
    std::fs::create_dir(fx.dir.join("sub")).unwrap();
    std::fs::write(fx.dir.join("sub/inner.rs"), "").unwrap();
    std::fs::write(fx.dir.join("shot.png"), [137u8, 80, 78, 71]).unwrap();
    std::fs::write(fx.dir.join("my file.txt"), "spaced").unwrap();
    let agent = Agent::new(fx.env.clone());
    let (text, images) = expand_mentions(
        &agent,
        "see @notes.txt, @sub and shot.png notes.txt @my\\ file.txt @gone.txt",
    )
    .await;
    assert!(
        text.contains(&format!(
            "<file path=\"{}\">\nthe notes\n</file>",
            fx.dir.join("notes.txt").display()
        )),
        "{text}"
    );
    assert!(
        text.contains("<directory path=") && text.contains("inner.rs"),
        "{text}"
    );
    assert!(text.contains("spaced"), "{text}");
    // A bare text path in prose is not attached; only once, via the `@`.
    assert_eq!(text.matches("the notes").count(), 1, "{text}");
    assert_eq!(images, ["iVBORw=="]);
    assert_eq!(fx.fe.notes(), ["Could not attach gone.txt: no such file"]);
}

#[tokio::test]
async fn loops_start_on_the_scheduler() {
    let fx = fixture("cmd-loop");
    let s = start_loop(
        &fx.env,
        Some(std::time::Duration::from_secs(300)),
        Some("check CI".into()),
    );
    assert!(
        s.starts_with("Looping every 5m") && s.contains(": check CI · `/loop stop` or esc ends it"),
        "{s}"
    );
    assert!(fx.env.scheduler.loop_active());
    assert!(fx.env.scheduler.stop_loop());
    let s = start_loop(&fx.env, None, None);
    assert!(
        s.starts_with("Self-paced loop: the default loop prompt (loop.md, or maintenance)"),
        "{s}"
    );
    assert!(fx.env.scheduler.stop_loop());
}

#[test]
fn json_comments_are_stripped_for_vs_code() {
    assert_eq!(strip_json_comments("// head\n[1, // one\n2]"), "\n[1, \n2]");
}

#[test]
fn mcp_states_have_labels_and_glyphs() {
    for (s, label, glyph) in [
        (McpState::Connected, "connected", "✓"),
        (McpState::NeedsLogin, "needs login", "!"),
        (McpState::Failed, "failed", "✗"),
        (McpState::Off, "disabled", "⊘"),
        (McpState::Pending, "pending approval", "⏸"),
        (McpState::Rejected, "rejected", "✘"),
    ] {
        assert_eq!((s.label(), s.glyph()), (label, glyph));
    }
    assert_eq!(
        failed_detail("srv", "MCP server `srv` is not running"),
        "the process isn't running"
    );
}

#[tokio::test]
async fn transcript_skips_reminders_and_unknown_tools() {
    let fx = fixture("cmd-steps");
    let mut agent = Agent::new(fx.env.clone());
    let mut call = Message::assistant("Looking.");
    call.tool_calls = vec![ToolCall {
        id: "x".into(),
        name: "Mystery".into(),
        arguments: json!({}),
    }];
    agent.messages = vec![
        Message::system("sys"),
        Message::user("go"),
        call,
        Message::user("<system-reminder>note</system-reminder>"),
        Message::assistant("Done."),
    ];
    assert_eq!(
        last_turn_steps(&agent),
        "## Last turn\nLooking.\n\n- `Mystery`: \n\nDone."
    );
}

#[tokio::test]
async fn status_reports_for_long_runs_and_sessions() {
    let (base, _) = mock_server(vec![json!({"content": "We fixed the parser."})]).await;
    let fx = fixture_with("cmd-misc", &base, false, |_| {}, |_| {});
    let mut agent = Agent::new(fx.env.clone());
    for c in [
        "/goal",
        "/goal list",
        "/team",
        "/team list",
        "/stats",
        "/peers",
        "/plugins",
        "/schedule",
        "/plan",
        "/autocompact",
    ] {
        assert!(
            matches!(handle(&mut agent, c).await, Outcome::Show(_)),
            "{c}"
        );
    }
    assert_eq!(
        run(&mut agent, "/team stop").await,
        "No team is running in this session."
    );
    assert!(matches!(
        handle(&mut agent, "/cd no/such/dir").await,
        Outcome::Show(_)
    ));
    assert_eq!(fx.env.cwd(), fx.dir, "a bad /cd stays put");
    agent.messages = vec![
        Message::system("sys"),
        Message::user("fix the parser"),
        Message::assistant("Done: the parser handles empty input."),
    ];
    let recap = run(&mut agent, "/recap").await;
    assert!(recap.starts_with("※ "), "{recap}");
}

#[tokio::test]
async fn statusline_saves_a_script_to_the_config() {
    let home = Home::new("cmd-statusline-home");
    let fx = fixture("cmd-statusline");
    let mut agent = Agent::new(fx.env.clone());
    let p = prompt(handle(&mut agent, "/statusline show the branch").await);
    assert!(p.contains("What the user wants: show the branch"), "{p}");
    let cfg = std::fs::read_to_string(home.dir.join("config.toml")).unwrap();
    assert!(cfg.contains("status_line"), "{cfg}");
}
