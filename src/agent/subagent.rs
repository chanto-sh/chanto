//! Subagents: their definitions, model and permission overrides, MCP
//! connections, and the runner behind the Task tool.

use super::*;

/// A subagent running in the background (Task with `run_in_background`).
pub struct BgAgent {
    pub id: String,
    pub description: String,
    pub kind: String,
    /// running | completed | failed | stopped
    pub status: String,
    pub result: Option<String>,
    pub started: Instant,
    pub cancel: CancellationToken,
    pub worktree: Option<crate::worktree::Worktree>,
    /// Kept so the agent can be resumed with more instructions.
    pub agent: Arc<tokio::sync::Mutex<Agent>>,
    /// The agent's [`Agent::followups`], reachable while it runs.
    pub followups: Arc<Mutex<Vec<String>>>,
}

/// Options for launching a subagent.
#[derive(Default)]
pub struct SubagentOpts {
    pub background: bool,
    pub worktree: bool,
    pub description: String,
    /// A fork: start from this conversation instead of a blank one.
    pub history: Option<Vec<Message>>,
    /// A forked skill's `model` and `effort`, over the agent type's.
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// ANSI color for a subagent definition's `color`.
pub(crate) fn agent_color(c: &str) -> Option<&'static str> {
    Some(match c {
        "red" => "31",
        "green" => "32",
        "yellow" => "33",
        "blue" => "34",
        "purple" => "35",
        "cyan" => "36",
        "orange" => "38;5;208",
        "pink" => "38;5;213",
        _ => return None,
    })
}

/// A subagent's `memory` directory: `user` in `~/.claude/agent-memory/`,
/// `project` in `.claude/agent-memory/`, `local` in `.claude/agent-memory-local/`.
pub fn agent_memory_dir(root: &Path, scope: &str, name: &str) -> Option<PathBuf> {
    let name = name.replace([':', '/', '\\'], "-");
    Some(match scope {
        "user" => crate::compat::claude_dir().join("agent-memory").join(name),
        "project" => root.join(".claude").join("agent-memory").join(name),
        "local" => root.join(".claude").join("agent-memory-local").join(name),
        _ => return None,
    })
}

impl Agent {
    /// Configure this agent from a definition's frontmatter: tools, model,
    /// permission mode, turn limit, effort, hooks, memory, preloaded skills.
    /// `subagent` is false when the definition runs as the main session
    /// (then `Stop` hooks stay `Stop`).
    pub fn apply_agent_def(&mut self, def: &AgentDef, subagent: bool) {
        self.tool_filter = def.tools.clone();
        self.tool_deny = def.disallowed_tools.clone();
        if let Some(m) = &def.model {
            let m = subagent_model(&self.env, &self.model(), m);
            self.model_override = Some(m);
        }
        let mut extra = self.adopt_frontmatter(def, subagent);
        if subagent {
            extra.push_str("\n\nYou are running as a subagent. Your final message is the only thing returned to the caller: make it a complete report.");
        }
        self.system_extra = Some(extra);
    }

    /// The frontmatter settings beyond tools and model: permission mode, turn
    /// limit, effort, hooks, named MCP servers, memory and preloaded skills.
    /// Returns the definition's prompt with its memory and skills added.
    pub(crate) fn adopt_frontmatter(&mut self, def: &AgentDef, subagent: bool) -> String {
        let env = self.env.clone();
        // A project's agent files run hooks only in a trusted folder, and
        // can tighten its permission mode there but not loosen it.
        let trusted = !def.project || def.path.parent().is_some_and(crate::config::is_trusted);
        let loosens = |pm: &str| {
            matches!(
                Mode::parse(pm),
                Some(Mode::AcceptEdits | Mode::Auto | Mode::Bypass)
            )
        };
        if let Some(pm) = def
            .permission_mode
            .as_deref()
            .filter(|pm| !trusted && loosens(pm))
        {
            env.emit(Event::Warn(format!("Subagent {} is defined in a folder you haven't trusted; its permissionMode `{pm}` was ignored (chanto --trust to allow).", def.name)));
        } else if let Some(pm) = &def.permission_mode {
            match pm.to_ascii_lowercase().as_str() {
                // Denies prompts unless the main conversation's mode takes over.
                "dontask" => self.sub.dont_ask = !matches!(env.mode(), Mode::Bypass | Mode::AcceptEdits | Mode::Auto),
                "manual" => self.sub.mode = Some(Mode::Default),
                other => match Mode::parse(other) {
                    Some(m) => self.sub.mode = Some(m),
                    None => env.emit(Event::Warn(format!("Subagent {}: unknown permissionMode `{pm}`; using the main conversation's.", def.name))),
                },
            }
        }
        self.sub.max_turns = def.max_turns;
        self.sub.effort = def
            .effort
            .clone()
            .filter(|e| crate::llm::EFFORTS.contains(&e.as_str()));
        self.sub.omit_instructions = def.omit_claude_md;
        if let Some(h) = &def.hooks {
            if trusted {
                let mut h = h.clone();
                if subagent && let Some(stop) = h.remove("Stop") {
                    h.entry("SubagentStop".into()).or_default().extend(stop);
                }
                self.sub.hooks = Some(Arc::new(h));
            } else {
                env.emit(Event::Warn(format!("Subagent {} is defined in a folder you haven't trusted; its hooks were not loaded (chanto --trust to allow).", def.name)));
            }
        }
        // Configured servers it names stay reachable through a `tools` list.
        if let Some(f) = &mut self.tool_filter {
            for m in &def.mcp_servers {
                if let crate::context::AgentMcp::Named(n) = m
                    && let Some(c) = env.mcp.iter().find(|c| &c.name == n)
                {
                    f.push(format!("{}*", c.full_name("")));
                }
            }
        }
        let mut extra = def.prompt.clone();
        let memory = def.memory.as_deref().filter(|_| env.config.memory.auto);
        if let Some(dir) = memory.and_then(|scope| agent_memory_dir(&env.root, scope, &def.name)) {
            let _ = std::fs::create_dir_all(&dir);
            let index = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap_or_default();
            let mut shown: String = index.lines().take(200).collect::<Vec<_>>().join("\n");
            if shown.len() > 25_000 {
                shown.truncate(shown.floor_char_boundary(25_000));
            }
            extra.push_str(&format!(
                "\n\n# Agent memory\nYou have a persistent memory directory at {} that survives across conversations. \
                 Consult it before starting, and save what you learn (codebase patterns, debugging insights, decisions) as \
                 markdown files there with Write and Edit, indexed in MEMORY.md. Keep MEMORY.md under 200 lines: curate it \
                 when it grows past that.\n",
                dir.display()
            ));
            if !shown.trim().is_empty() {
                extra.push_str(&format!(
                    "\n<memory-index path=\"{}\">\n{shown}\n</memory-index>\n",
                    dir.join("MEMORY.md").display()
                ));
            }
            if let Some(f) = &mut self.tool_filter {
                for t in ["Read", "Write", "Edit"] {
                    if !f.iter().any(|x| x == t) {
                        f.push(t.into());
                    }
                }
            }
        }
        // `skills`: their full content, up to 32 names; ones only the user may run can't be preloaded.
        let skills = env.skills.read().unwrap().clone();
        let mut seen: Vec<&str> = Vec::new();
        for name in def
            .skills
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            if seen.contains(&name) || seen.len() >= 32 {
                continue;
            }
            seen.push(name);
            match crate::util::find_named(&skills, name, |s| &s.name).filter(|s| s.model_invocable)
            {
                Some(sk) => {
                    let text = std::fs::read_to_string(&sk.path).unwrap_or_default();
                    let dir = sk
                        .path
                        .parent()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    let body = crate::context::strip_frontmatter(&text)
                        .replace("${CLAUDE_SKILL_DIR}", &dir);
                    extra.push_str(&format!(
                        "\n\n<skill name=\"{}\" base_directory=\"{dir}\">\n{}\n</skill>",
                        sk.name,
                        body.trim()
                    ));
                }
                None => crate::util::timing(&format!(
                    "subagent {}: skill {name} is missing or can't be preloaded",
                    def.name
                )),
            }
        }
        extra
    }
}

/// Connect a subagent's inline `mcpServers` (closed when it's dropped). A
/// project agent's need a trusted folder.
pub async fn connect_agent_mcp(agent: &mut Agent, def: &AgentDef) {
    let inline: Vec<(String, crate::config::McpServerConfig)> = def
        .mcp_servers
        .iter()
        .filter_map(|m| {
            if let crate::context::AgentMcp::Inline(n, c) = m {
                Some((n.clone(), c.as_ref().clone()))
            } else {
                None
            }
        })
        .collect();
    if inline.is_empty() {
        return;
    }
    if def.project && !def.path.parent().is_some_and(crate::config::is_trusted) {
        agent.env.emit(Event::Warn(format!("Subagent {} is defined in a folder you haven't trusted; its MCP servers were not started (chanto --trust to allow).", def.name)));
        return;
    }
    let servers: std::collections::BTreeMap<String, crate::config::McpServerConfig> =
        inline.into_iter().collect();
    let (clients, errs) = crate::mcp::connect_all(&servers).await;
    for e in errs {
        agent
            .env
            .emit(Event::Warn(format!("Subagent {}: {e}", def.name)));
    }
    if let Some(f) = &mut agent.tool_filter {
        f.extend(clients.iter().map(|c| format!("{}*", c.full_name(""))));
    }
    agent.sub.mcp = clients;
    agent.refresh_system();
}

/// Run an agent hook's verifier: a subagent with read-only tools and no
/// hooks of its own, up to 50 model calls. Returns its final message.
/// Boxed (and `Send` by declaration) because the verifier's turn goes back
/// through the hook code that called it.
pub fn run_hook_agent(
    env: Arc<Env>,
    model: String,
    prompt: String,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send>> {
    Box::pin(crate::hooks::SCOPE
        .scope(crate::hooks::Scope { hooks: None, off: true }, async move {
            let mut agent = Agent::new(env);
            agent.depth = 1;
            agent.tool_filter = Some(["Read", "Grep", "Glob", "Ls"].iter().map(|s| s.to_string()).collect());
            agent.model_override = Some(model);
            agent.sub.max_turns = Some(50);
            agent.sub.dont_ask = true;
            agent.system_extra = Some(
                "You are verifying a condition for a hook. Investigate with the read-only tools as needed, then answer with only \
                 the JSON verdict you were asked for."
                    .into(),
            );
            agent.reset();
            match agent.run_turn(&prompt, vec![], CancellationToken::new()).await? {
                TurnEnd::Done(text) => Ok(text),
                TurnEnd::Interrupted => anyhow::bail!("interrupted"),
            }
        }))
}

/// Claude Code's subagent model aliases and the newest model of each family.
const MODEL_ALIASES: &[(&str, &str)] = &[
    ("sonnet", "claude-sonnet-5-5"),
    ("opus", "claude-opus-5-5"),
    ("haiku", "claude-haiku-5-5"),
    ("fable", "claude-fable-5-1"),
];

/// A subagent definition's `model:` as a model spec. `inherit` is the main
/// conversation's model. An alias (`sonnet`, …) becomes that Claude model on
/// `base`'s provider when it speaks the Anthropic API; elsewhere (other
/// providers, or clouds with their own model ids) the subagent keeps `base`,
/// with a warning once per alias. Anything else is a spec, used as is.
pub(crate) fn subagent_model(env: &Env, base: &str, m: &str) -> String {
    if m.eq_ignore_ascii_case("inherit") {
        return env.model();
    }
    let Some((alias, id)) = MODEL_ALIASES
        .iter()
        .find(|(a, _)| m.eq_ignore_ascii_case(a))
    else {
        return m.to_string();
    };
    let (p, model) = env.resolve(base);
    if p.kind == llm::ProviderKind::Anthropic {
        // Keep base's provider prefix (`anthropic/`, a `[providers.x]` name).
        return format!("{}{id}", base.strip_suffix(model.as_str()).unwrap_or(""));
    }
    static WARNED: std::sync::Mutex<Vec<&str>> = std::sync::Mutex::new(Vec::new());
    let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
    if !warned.contains(alias) {
        warned.push(*alias);
        env.emit(Event::Warn(format!(
            "Subagent model `{alias}` needs an Anthropic provider; using {base} instead."
        )));
    }
    base.to_string()
}

/// `--permission-prompt-tool mcp__server__tool`: ask that MCP tool instead of
/// the user (print mode). It gets `{tool_name, input, tool_use_id}` and answers
/// with JSON text: `{"behavior": "allow", "updatedInput": {...}}` or
/// `{"behavior": "deny", "message": "..."}`. None when no tool is configured
/// (or the session is interactive).
pub(crate) async fn permission_prompt_tool(
    env: &Arc<Env>,
    name: &str,
    call: &ToolCall,
) -> Option<Auth> {
    let tool = crate::launch::get()
        .permission_prompt_tool
        .as_deref()
        .filter(|_| !env.interactive)?;
    let Some(client) = env.mcp.iter().find(|c| c.tool(tool).is_some()) else {
        return Some(Auth::Deny(format!(
            "Permission to use {name} needs approval, and the permission prompt tool {tool} isn't available."
        )));
    };
    let raw = client
        .tools()
        .into_iter()
        .find(|t| client.full_name(&t.name) == tool)
        .map(|t| t.name.clone())?;
    let args = json!({"tool_name": name, "input": call.arguments, "tool_use_id": call.id});
    Some(match client.call(&raw, args).await {
        Ok(out) => parse_permission_answer(&out.content, call),
        Err(e) => Auth::Deny(format!("The permission prompt tool {tool} failed: {e:#}")),
    })
}

pub(crate) fn parse_permission_answer(text: &str, call: &ToolCall) -> Auth {
    let v: Value = serde_json::from_str(text.trim()).unwrap_or_default();
    match v["behavior"].as_str() {
        Some("allow") => match v.get("updatedInput").filter(|i| i.is_object()) {
            Some(input) if *input != call.arguments => Auth::AllowWith(
                ToolCall {
                    arguments: input.clone(),
                    ..call.clone()
                },
                None,
            ),
            _ => Auth::Allow,
        },
        Some("deny") => Auth::Deny(
            v["message"]
                .as_str()
                .filter(|m| !m.is_empty())
                .unwrap_or("Denied by the permission prompt tool.")
                .to_string(),
        ),
        _ => Auth::Deny(format!(
            "The permission prompt tool gave no decision: {}",
            crate::util::truncate(text, 300)
        )),
    }
}

/// A custom subagent definition by name.
pub fn find_agent_def(env: &Env, kind: &str) -> Option<AgentDef> {
    crate::util::find_named(&env.agents.read().unwrap(), kind, |a| &a.name).cloned()
}

pub(crate) fn build_subagent(env: &Arc<Env>, depth: usize, kind: &str) -> Agent {
    let mut agent = Agent::new(env.clone());
    agent.depth = depth;
    agent.parent = TASK_PARENT.try_with(|p| p.clone()).ok().flatten();
    if let Some(m) = &env.config.subagent_model {
        agent.model_override = Some(m.clone());
    }
    let lower = kind.to_ascii_lowercase();
    if lower == "oracle"
        && crate::util::find_named(&env.agents.read().unwrap(), kind, |a| &a.name).is_none()
    {
        agent.tool_filter = Some(EXPLORE_TOOLS.iter().map(|s| s.to_string()).collect());
        // Its own model when set (a stronger or different one is the point), else the main model.
        agent.model_override = env.config.oracle_model.clone();
        agent.system_extra = Some(ORACLE_PROMPT.into());
    } else if lower == "explore" {
        agent.tool_filter = Some(EXPLORE_TOOLS.iter().map(|s| s.to_string()).collect());
        agent.system_extra = Some(
            "You are a fast, read-only exploration subagent. Search and read the codebase to answer the request \
             (RepoMap first for an unfamiliar tree, SemanticSearch for a concept you cannot name). \
             Do not modify anything; use Bash only for read-only commands. Search broadly with parallel tool calls. \
             Finish with a concise, well-organized report with file paths and line numbers — it is the only thing the caller sees."
                .into(),
        );
    } else if let Some(def) = find_agent_def(env, kind) {
        agent.apply_agent_def(&def, true);
    } else {
        agent.system_extra = Some(
            "You are a subagent handling a delegated task autonomously. Complete it fully, then finish with a concise report \
             of what you did and found (with file paths) — your final message is the only thing returned to the caller."
                .into(),
        );
    }
    agent
}

pub(crate) fn report(agent: &Agent, end: Result<TurnEnd>, secs: u64) -> (bool, String, String) {
    let tool_uses = agent
        .messages
        .iter()
        .map(|m| m.tool_calls.len())
        .sum::<usize>();
    let stats = format!(
        "{tool_uses} tool uses · {} tokens · {}",
        agent.total_out,
        crate::util::duration(secs)
    );
    match end {
        Ok(TurnEnd::Done(text)) => {
            let text = if text.trim().is_empty() {
                "(subagent returned no report)".to_string()
            } else {
                text
            };
            (true, text, stats)
        }
        Ok(TurnEnd::Interrupted) => (false, "Subagent interrupted".into(), stats),
        Err(e) => (false, format!("Subagent failed: {e:#}"), stats),
    }
}

pub async fn run_subagent(
    ctx: &Ctx,
    kind: &str,
    prompt: &str,
    opts: SubagentOpts,
) -> Result<ToolOutput> {
    let env = ctx.env.clone();
    crate::hooks::notify(
        &env,
        "SubagentStart",
        Some(kind),
        serde_json::json!({"agent_type": kind}),
    )
    .await;
    let mut agent = build_subagent(&env, ctx.depth + 1, kind);
    let def = find_agent_def(&env, kind);
    let mut opts = opts;
    // A background run outlives its parent's turn, so only the main agent starts them.
    let nested_call = ctx.depth > 0;
    if let Some(d) = &def {
        // `background: true` and `isolation: worktree` hold whatever the call asked.
        opts.background |= d.background;
        opts.worktree |= d.isolation.as_deref() == Some("worktree");
        connect_agent_mcp(&mut agent, d).await;
    }
    if nested_call {
        opts.background = false;
    }
    if let Some(m) = opts.model.take() {
        let m = subagent_model(&env, &agent.model(), &m);
        agent.model_override = Some(m);
    }
    if let Some(e) = opts
        .effort
        .take()
        .filter(|e| crate::llm::EFFORTS.contains(&e.as_str()))
    {
        agent.sub.effort = Some(e);
    }
    // Spend guard: "use a cheaper model for the agents" wins for the session.
    if let Some(m) = env.spend.subagent_model() {
        agent.model_override = Some(m);
    }
    // `color`: the subagent's name in its color, in the result line.
    let badge = def
        .as_ref()
        .and_then(|d| d.color.as_deref().and_then(agent_color))
        .map(|c| format!("\x1b[{c}m●\x1b[0m {kind} · "))
        .unwrap_or_default();
    let id = format!("agent-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]);
    let hook_worktree = match opts.worktree {
        true => crate::hooks::worktree_create(&env, &id).await,
        false => None,
    };
    let worktree = if let Some(made) = hook_worktree {
        // A WorktreeCreate hook replaces git: the agent works in the path it
        // returned, which is kept afterwards.
        let path = match made {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(format!("Could not create a worktree: {e}"))),
        };
        let repo = crate::permissions::normalize(&ctx.cwd());
        agent.cwd_override = Some(path.clone());
        agent.worktree_map = Some((repo, path.clone()));
        agent.system_extra = Some(format!(
            "{}\n\nYou are working in an isolated working copy at {}. Make all changes there (relative paths resolve to it).",
            agent.system_extra.clone().unwrap_or_default(),
            path.display()
        ));
        None
    } else if opts.worktree {
        match crate::worktree::create(&ctx.cwd(), &id) {
            Ok(w) => {
                // Keep the caller's subdirectory: repo/sub → worktree/sub.
                let repo = crate::permissions::normalize(&w.repo);
                let rel = crate::permissions::normalize(&ctx.cwd())
                    .strip_prefix(&repo)
                    .map(|r| r.to_path_buf())
                    .unwrap_or_default();
                agent.cwd_override = Some(w.path.join(rel));
                agent.worktree_map = Some((repo, w.path.clone()));
                agent.system_extra = Some(format!(
                    "{}\n\nYou are working in an isolated git worktree at {} on branch `{}`. Make all changes there (relative paths resolve to it). Commit your work on that branch when done.",
                    agent.system_extra.clone().unwrap_or_default(),
                    w.path.display(),
                    w.branch
                ));
                Some(w)
            }
            Err(e) => {
                return Ok(ToolOutput::err(format!(
                    "Could not create a worktree: {e:#}"
                )));
            }
        }
    } else {
        None
    };
    agent.reset();
    if let Some(history) = opts.history {
        // A fork continues the conversation it came from, with the same tools.
        agent
            .messages
            .extend(history.into_iter().filter(|m| m.role != Role::System));
        agent.system_extra = Some(
            "You are a fork of the main conversation, running in the background with full tool access. \
             Pursue the request below; the main session carries on without you. Your final message is \
             returned to it, so make it a complete, concise report."
                .into(),
        );
        agent.refresh_system();
    }
    if opts.background {
        let live = env
            .bg_agents
            .lock()
            .unwrap()
            .iter()
            .filter(|b| b.status == "running")
            .count();
        if live >= crate::resources::max_background_agents() {
            return Ok(ToolOutput::err(format!(
                "{live} background agents are already running (limit {}). Wait for their reports before starting more.",
                crate::resources::max_background_agents()
            )));
        }
    }
    if let Err(e) =
        crate::resources::wait_for_memory("another subagent", std::time::Duration::from_secs(60))
            .await
    {
        return Ok(ToolOutput::err(e));
    }
    if let Some(key) = &agent.parent {
        env.spend
            .agent_started(key, kind, &opts.description, &agent.model());
    }
    if !opts.background {
        let t0 = Instant::now();
        // "Limit parallelism" from the spend guard, then the resource limit;
        // a nested agent has its own (see `resources::nested_agent`).
        let nested = ctx.depth > 0;
        let _nested = if nested {
            let Some(room) = crate::resources::nested_agent() else {
                return Ok(ToolOutput::err(
                    "Too many nested subagents are running; do this one yourself.",
                ));
            };
            Some(room)
        } else {
            None
        };
        let _spend_slot = if nested {
            None
        } else {
            let Some(slot) = env.spend.slot(&ctx.cancel).await else {
                return Ok(ToolOutput::err("Interrupted by user"));
            };
            Some(slot)
        };
        let _slot = if nested {
            None
        } else {
            Some(crate::resources::agent_slot().await)
        };
        if let Some(key) = &agent.parent {
            env.live_agents
                .start(key, &id, kind, &opts.description, prompt, false);
        }
        let end = agent.run_turn(prompt, vec![], ctx.cancel.clone()).await;
        let (ok, mut text, stats) = report(&agent, end, t0.elapsed().as_secs());
        if let Some(key) = &agent.parent {
            env.live_agents.finish(key, ok);
            env.spend.agent_finished(key);
        }
        if let Some(w) = &worktree {
            text.push_str(&worktree_note(w));
        }
        return Ok(if ok {
            ToolOutput::ok(text).display(format!("{badge}Done ({stats})"))
        } else {
            ToolOutput::err(text)
        });
    }
    // Background: register, run detached, and report through a notification.
    let cancel = CancellationToken::new();
    let followups = agent.followups.clone();
    let handle = Arc::new(tokio::sync::Mutex::new(agent));
    env.bg_agents.lock().unwrap().push(BgAgent {
        id: id.clone(),
        description: opts.description.clone(),
        kind: kind.to_string(),
        status: "running".into(),
        result: None,
        started: Instant::now(),
        cancel: cancel.clone(),
        worktree: worktree.clone(),
        agent: handle.clone(),
        followups,
    });
    spawn_background(env.clone(), id.clone(), handle, prompt.to_string(), cancel);
    let wt = worktree
        .map(|w| format!(" in worktree {} (branch `{}`)", w.path.display(), w.branch))
        .unwrap_or_default();
    Ok(ToolOutput::ok(format!(
        "Started background agent {id}{wt}. You'll get its report automatically when it finishes; continue with other work meanwhile \
         (don't poll or wait for it). To give it follow-up instructions later, call Task with resume=\"{id}\"."
    ))
    .display(format!("{badge}Running in background as {id}")))
}
