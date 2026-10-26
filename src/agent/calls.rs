//! Running authorized tool calls: read-only calls concurrently, the rest in
//! order, and the spend guard that stops a turn before it overspends.

use super::*;

impl Agent {
    /// Returns true if the turn should stop (interrupt / user rejection).
    pub(super) async fn execute_calls(
        &mut self,
        calls: Vec<ToolCall>,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        // A full disk makes every write fail in confusing ways, so refuse
        // space-consuming calls while there is little left.
        let free = self.disk_free();
        // 1. Authorize sequentially (may prompt the user). The last field is
        // hook context to fold into that call's result.
        let mut plan: Vec<PlannedCall> = Vec::new();
        let mut call_seq: HashMap<String, u64> = HashMap::new();
        // Where each approved call's paths pointed when it was approved.
        let mut approved: HashMap<String, Vec<(String, PathBuf)>> = HashMap::new();
        // For approved shell commands: whether the static rules allowed them
        // outright, and whether they touched a secret file, at approval.
        let mut approved_exec: HashMap<String, ExecJudgement> = HashMap::new();
        let mut stop = false;
        for call in calls {
            if stop || cancel.is_cancelled() {
                plan.push((call, None, Err("Interrupted by user".into()), None));
                continue;
            }
            let mut call = call;
            let repaired = call
                .arguments
                .as_object_mut()
                .and_then(|o| o.remove(llm::REPAIRED))
                .is_some();
            let mut call = codex_call(call);
            if repaired
                && self
                    .lookup_tool(&call.name)
                    .is_some_and(|t| t.kind() == ToolKind::Edit)
            {
                // Broken JSON in a file write was most likely cut off: running
                // the repaired version would leave a truncated file.
                plan.push((call.clone(), None, Err(format!(
                    "This {} call was not run: its arguments weren't valid JSON (they look cut off), and writing a repaired guess could leave a truncated file. \
                     Resend it in smaller pieces: e.g. Write a short first part, then add the rest with several Edit calls.",
                    call.name
                )), None));
                continue;
            }
            let tool_cwd = self.cwd_override.clone().filter(|_| {
                self.lookup_tool(&call.name)
                    .is_some_and(|t| t.kind() != ToolKind::External || t.read_only_hint())
            });
            if let Some(dir) = &tool_cwd {
                // Isolated agents: make paths absolute so permission checks,
                // hooks and diagnostics all see the worktree file.
                for k in ["file_path", "path", "notebook_path"] {
                    if let Some(p) = call
                        .arguments
                        .get(k)
                        .and_then(|v| v.as_str())
                        .filter(|p| !p.is_empty())
                    {
                        let mut abs = crate::util::resolve(dir, p);
                        if let Some((repo, wt)) = &self.worktree_map {
                            let n = crate::permissions::normalize(&abs);
                            if !n.starts_with(crate::permissions::normalize(wt))
                                && let Ok(rel) = n.strip_prefix(repo)
                            {
                                abs = wt.join(rel);
                            }
                        }
                        call.arguments[k] = json!(abs.display().to_string());
                    }
                }
            }
            let tool = self.lookup_tool(&call.name);
            let Some(tool) = tool else {
                let avail = self.env.tools.names().join(", ");
                plan.push((
                    call.clone(),
                    None,
                    Err(format!(
                        "Unknown tool `{}`. Available tools: {avail}",
                        call.name
                    )),
                    None,
                ));
                continue;
            };
            let mut call = ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments: normalize_args(&tool.schema(), &call.arguments),
            };
            if let Some(raw) = call.arguments.get("__raw") {
                plan.push((
                    call.clone(),
                    None,
                    Err(format!(
                        "Tool arguments were not valid JSON: {}",
                        crate::util::truncate(&raw.to_string(), 300)
                    )),
                    None,
                ));
                continue;
            }
            self.env
                .frontend
                .tool_call(
                    &call.id,
                    &call.name,
                    &call.arguments,
                    &self.cwd_override.clone().unwrap_or_else(|| self.env.cwd()),
                )
                .await;
            self.cur_cancel = Some(cancel.clone());
            let authorization = self.authorize(tool.as_ref(), &mut call).await;
            let base = self.cwd_override.clone().unwrap_or_else(|| self.env.cwd());
            match &authorization {
                Auth::Allow => {
                    approved.insert(call.id.clone(), path_targets(&call, &base));
                    if tool.kind_for(&call.arguments) == ToolKind::Execute {
                        approved_exec.insert(
                            call.id.clone(),
                            judge_exec(self.mode(), &call, tool.as_ref(), &base),
                        );
                    }
                }
                Auth::AllowWith(c, _) => {
                    approved.insert(c.id.clone(), path_targets(c, &base));
                }
                _ => {}
            }
            self.env.accounting.log("info", "permission_decision", json!({"name": call.name, "decision": match &authorization { Auth::Allow | Auth::AllowWith(..) => "allow", Auth::Deny(_) => "deny", Auth::Stop => "stop" }}));
            match authorization {
                Auth::Allow => {
                    if let Some(free) = free
                        && free < DISK_CRITICAL
                        && writes_to_disk(&call, tool.as_ref())
                        && !frees_space(&call)
                    {
                        plan.push((
                            call,
                            None,
                            Err(format!(
                                "Blocked: only {} MB of disk space is free, so this call was not run. \
                                 Free space first (remove build artifacts or caches), then retry.",
                                free >> 20
                            )),
                            None,
                        ));
                        continue;
                    }
                    match tool.kind_for(&call.arguments) {
                        ToolKind::Edit => {
                            self.turn_mutations += 1;
                            self.edit_seq += 1;
                        }
                        ToolKind::Execute
                            if !permissions::is_safe_command(
                                call.arguments["command"].as_str().unwrap_or(""),
                            ) =>
                        {
                            self.turn_mutations += 1;
                            self.edit_seq += 1;
                            self.turn_untracked_writes = true;
                        }
                        ToolKind::External => self.turn_untracked_writes = true,
                        _ if call.name == "Task" => self.turn_untracked_writes = true,
                        _ => {}
                    }
                    call_seq.insert(call.id.clone(), self.edit_seq);
                    let ctx = self.hook_context.take();
                    plan.push((call, Some(tool), Ok(()), ctx))
                }
                Auth::AllowWith(c, note) => {
                    let t = self.lookup_tool(&c.name);
                    let ok = if t.is_some() {
                        Ok(())
                    } else {
                        Err(format!("Tool {} is not available", c.name))
                    };
                    plan.push((c, t, ok, note));
                }
                Auth::Deny(r) => plan.push((call, Some(tool), Err(r), None)),
                Auth::Stop => {
                    stop = true;
                    plan.push((call, None, Err("The user rejected this action and stopped the turn. Wait for their next instruction.".into()), None));
                }
            }
        }

        if self.depth == 0 {
            self.spend_preflight(&mut plan, cancel).await;
        }

        // 2. Execute in order, running consecutive read-only/network/Task calls concurrently.
        let parallel_safe = |c: &ToolCall, t: &Arc<dyn Tool>| {
            matches!(
                t.kind_for(&c.arguments),
                ToolKind::ReadOnly | ToolKind::Network
            ) || t.name() == "Task"
        };
        let mut results: Vec<Option<ToolOutput>> = vec![None; plan.len()];
        let ctx = Ctx {
            env: self.env.clone(),
            cancel: cancel.clone(),
            depth: self.depth,
            call_id: None,
            cwd_override: self.cwd_override.clone(),
            reads: Some(self.reads.clone()),
        };
        let mut i = 0;
        while i < plan.len() {
            if cancel.is_cancelled() {
                break;
            }
            let runnable = |j: usize| matches!(&plan[j], (_, Some(_), Ok(()), _));
            if !runnable(i) {
                i += 1;
                continue;
            }
            let base = self.cwd_override.clone().unwrap_or_else(|| self.env.cwd());
            let (call, tool, _, _) = &plan[i];
            let tool = tool.as_ref().unwrap();
            if !parallel_safe(call, tool) {
                let why = retargeted(approved.get(&call.id), call, &base).or_else(|| {
                    let then = approved_exec.get(&call.id)?;
                    exec_regressed(then, &judge_exec(self.mode(), call, tool.as_ref(), &base))
                });
                if let Some(why) = why {
                    plan[i].2 = Err(why);
                    i += 1;
                    continue;
                }
                // The verify baseline must see the project before any change.
                let mutating = match tool.kind_for(&call.arguments) {
                    ToolKind::Edit => true,
                    ToolKind::Execute => !permissions::is_safe_command(
                        call.arguments["command"].as_str().unwrap_or(""),
                    ),
                    _ => false,
                };
                // Anything that may write (edits, commands, MCP tools) waits
                // for the background snapshot of the files it could change.
                if mutating
                    || !matches!(
                        tool.kind_for(&call.arguments),
                        ToolKind::ReadOnly | ToolKind::Network
                    )
                {
                    self.env.wait_snapshot().await;
                }
                if mutating && let Some(h) = self.baseline_task.take() {
                    self.emit(Event::Busy(Some(
                        "Recording which checks pass before changes…".into(),
                    )));
                    self.baseline = h.await.ok();
                    self.emit(Event::Busy(None));
                }
                let before = if call.name == "Bash" && mutating {
                    self.env.shadow_snapshot()
                } else {
                    None
                };
                // Show what the command changed, like an edit: only for the
                // main agent in the session's own project (the snapshot
                // covers that), and not for background commands.
                let before = before.filter(|_| {
                    self.depth == 0
                        && self.cwd_override.is_none()
                        && !call.arguments["run_in_background"]
                            .as_bool()
                            .unwrap_or(false)
                        && permissions::normalize(&self.env.cwd())
                            .starts_with(permissions::normalize(&self.env.root))
                        && crate::tools::bash::bash_diff_enabled(&self.env.config)
                });
                if crate::util::timing_on() {
                    crate::util::timing(&format!("tool {} start", call.name));
                }
                let out =
                    run_tool_with(tool.as_ref(), call, &ctx, self.parent.clone(), before).await;
                if crate::util::timing_on() {
                    crate::util::timing(&format!("tool {} done", call.name));
                }
                if self.depth == 0
                    && !out.is_error
                    && tool.kind_for(&call.arguments) == ToolKind::Edit
                {
                    // Remember what we wrote, to notice if the user fixes it up by hand.
                    let paths = match call.name.as_str() {
                        "apply_patch" => crate::tools::patch::paths(
                            call.arguments["input"].as_str().unwrap_or(""),
                        ),
                        _ => ["file_path", "notebook_path"]
                            .iter()
                            .filter_map(|k| call.arguments[*k].as_str())
                            .map(str::to_string)
                            .collect(),
                    };
                    let base = self.cwd_override.clone().unwrap_or_else(|| self.env.cwd());
                    for p in paths {
                        let p = crate::util::resolve(&base, &p);
                        if let Ok(t) = std::fs::read_to_string(&p) {
                            self.agent_wrote.insert(p, t);
                        }
                    }
                }
                results[i] = Some(out);
                i += 1;
                continue;
            }
            // Gather a batch of consecutive parallel-safe calls.
            let mut j = i;
            let mut batch = Vec::new();
            while j < plan.len() {
                match &plan[j] {
                    (c, Some(t), Ok(()), _) if parallel_safe(c, t) => batch.push(j),
                    (_, _, Err(_), _) => {}
                    _ => break,
                }
                j += 1;
            }
            for k in std::mem::take(&mut batch) {
                let call = &plan[k].0;
                match retargeted(approved.get(&call.id), call, &base) {
                    Some(why) => plan[k].2 = Err(why),
                    None => batch.push(k),
                }
            }
            let futs = batch.iter().map(|&k| {
                let (call, tool, _, _) = &plan[k];
                let ctx = &ctx;
                let parent = self.parent.clone();
                async move { run_tool(tool.as_ref().unwrap().as_ref(), call, ctx, parent).await }
            });
            let outs = futures::future::join_all(futs).await;
            for (k, o) in batch.into_iter().zip(outs) {
                results[k] = Some(o);
            }
            i = j;
        }

        // 3. Record results (every call must get a tool message).
        let mut images: Vec<(String, Vec<String>)> = Vec::new();
        for (i, (call, tool, auth, hook_ctx)) in plan.into_iter().enumerate() {
            let out = match (results[i].take(), auth) {
                (Some(o), _) => o,
                (None, Err(reason)) => {
                    if tool.is_some() || !reason.starts_with("Interrupted") {
                        self.emit(Event::ToolStart {
                            id: call.id.clone(),
                            label: self.label_for(&call),
                            parent: self.parent.clone(),
                        });
                        self.emit(Event::ToolEnd {
                            id: call.id.clone(),
                            display: reason.clone(),
                            content: reason.clone(),
                            is_error: true,
                            images: Vec::new(),
                            parent: self.parent.clone(),
                        });
                    }
                    ToolOutput::err(reason)
                }
                (None, Ok(())) => ToolOutput::err("Interrupted by user"),
            };
            if call.name == "Bash" {
                // `gh pr create` and friends: link the pull request to this session.
                if let Some(s) = self.session.as_mut() {
                    for url in crate::session::pull_request_urls(&out.content) {
                        if !s.pull_requests.contains(&url) {
                            s.pull_requests.push(url);
                        }
                    }
                }
                let cmd = call.arguments["command"].as_str().unwrap_or("");
                let safe = permissions::is_safe_command(cmd);
                if let Some(seq) = call_seq.get(&call.id) {
                    if out.is_error
                        && safe
                        && let Some(secs) = timed_out_after(&out.content)
                    {
                        self.check_timeouts.push((cmd.to_string(), *seq, secs));
                    } else if crate::guardrails::failing_tests(&out.content).is_some() {
                        // A run that named failing tests is evidence it failed,
                        // even when a pipeline (`pytest ... | tail`) reported
                        // the exit status of its last stage instead.
                        self.check_runs.push((
                            cmd.to_string(),
                            *seq,
                            false,
                            crate::util::truncate_middle(&out.content, 8_000),
                        ));
                    } else if !out.is_error && safe {
                        self.check_runs
                            .push((cmd.to_string(), *seq, true, String::new()));
                    }
                }
            } else if call.name == "BashOutput" {
                // A test suite run in the background reports its failures
                // when polled, not when it was launched. Attribute the output
                // to the command the shell is running so the finish guard
                // sees it just like a foreground run.
                if let Some(id) = call.arguments["bash_id"].as_str()
                    && let Some(sh) = self.env.shells.lock().unwrap().get(id).cloned()
                    && let Some(seq) = call_seq.get(&call.id)
                    && crate::guardrails::failing_tests(&out.content).is_some()
                {
                    self.check_runs.push((
                        sh.command.clone(),
                        *seq,
                        false,
                        crate::util::truncate_middle(&out.content, 8_000),
                    ));
                }
            }
            if !out.images.is_empty() {
                images.push((call.name.clone(), out.images.clone()));
            }
            // At most half the context window (≈3.5 characters a token), so one
            // result can't overflow a small model's window on its own.
            let cap = (self.env.context_max() as usize * 7 / 4).clamp(4_000, 60_000);
            let mut content = crate::util::truncate_middle(&out.content, cap);
            if out.is_error && !content.starts_with("Error") && !content.starts_with("Blocked") {
                content = format!("Error: {content}");
            }
            // PreToolUse/PermissionRequest hook context rides along with the
            // result, so the model sees it on this turn rather than the next prompt.
            if let Some(c) = hook_ctx.filter(|c| !c.trim().is_empty()) {
                content.push_str(&format!("\n\n<system-reminder>\n{c}\n</system-reminder>"));
            }
            if self.depth == 0
                && let Some(hint) =
                    self.stats
                        .live_hint(&call.name, &content, self.env.context_max())
            {
                self.emit(Event::Info(hint));
            }
            let notes = self
                .env
                .spend
                .tool_call(&self.spend_key(), &call, &content, out.is_error);
            crate::spend::notify(&self.env, notes);
            if self.env.config.optimize.provenance() {
                let remaining = 2_000_000usize.saturating_sub(self.turn_evidence.len());
                if remaining > 0 {
                    if !self.turn_evidence.is_empty() {
                        self.turn_evidence.push('\n');
                    }
                    let mut end = content.len().min(remaining);
                    while !content.is_char_boundary(end) {
                        end -= 1;
                    }
                    self.turn_evidence.push_str(&content[..end]);
                }
            }
            self.messages.push(Message::tool(&call, content));
        }
        for (name, imgs) in images {
            let mut m = Message::user(format!("[image output from {name}]"));
            m.images = imgs;
            self.messages.push(m);
        }
        Ok(stop || cancel.is_cancelled())
    }

    /// This agent in the spend ledger: the main agent, or the Task call that started it.
    pub(super) fn spend_key(&self) -> String {
        match (&self.parent, self.depth) {
            (_, 0) => crate::spend::MAIN.to_string(),
            (Some(p), _) => p.clone(),
            (None, _) => "subagent".into(),
        }
    }

    /// Spend guard: before several subagents start (in this batch, or next
    /// to ones already running), estimate them and, over the threshold, ask
    /// or follow the non-interactive policy. Refused calls get the reason.
    pub(super) async fn spend_preflight(
        &mut self,
        plan: &mut [PlannedCall],
        cancel: &CancellationToken,
    ) {
        let new: Vec<usize> = plan
            .iter()
            .enumerate()
            .filter(|(_, (c, t, ok, _))| {
                t.is_some()
                    && ok.is_ok()
                    && c.name == "Task"
                    && c.arguments["resume"].as_str().is_none_or(str::is_empty)
            })
            .map(|(i, _)| i)
            .collect();
        let running = self.env.live_agents.running_count();
        if new.is_empty() || new.len() + running < 2 {
            return;
        }
        let kind = plan[new[0]].0.arguments["subagent_type"]
            .as_str()
            .unwrap_or("general-purpose")
            .to_string();
        // A subagent starts with a system prompt and tools like ours, plus its task.
        let prompt = new
            .iter()
            .map(|&i| plan[i].0.arguments["prompt"].as_str().unwrap_or("").len() as u64 / 4)
            .max()
            .unwrap_or(0);
        let tools = serde_json::to_string(&self.env.tools.specs(&|n| self.tool_allowed(n)))
            .map(|s| s.len() as u64 / 4)
            .unwrap_or(0);
        let start =
            llm::estimate_tokens(&self.messages[..self.messages.len().min(1)]) + tools + prompt;
        if let Err(reason) =
            crate::spend::preflight_agents(&self.env, &kind, new.len(), running, start, cancel)
                .await
        {
            for i in new {
                plan[i].2 = Err(reason.clone());
            }
            return;
        }
        if let Some(n) = self.env.spend.parallel(running + new.len()) {
            crate::spend::notify(&self.env, vec![n]);
        }
    }

    /// `fallback_model` specs (at most three), without the session's own model.
    pub(super) fn fallback_chain(&self) -> Vec<String> {
        let primary = self.model();
        self.env
            .config
            .fallback_model
            .as_ref()
            .map(|f| f.specs())
            .unwrap_or_default()
            .into_iter()
            .filter(|s| *s != primary)
            .collect()
    }

    pub(super) fn lookup_tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if !self.tool_allowed(name) {
            return None;
        }
        self.env
            .tools
            .get(name)
            .or_else(|| {
                self.env
                    .mcp
                    .iter()
                    .chain(&self.sub.mcp)
                    .find_map(|c| c.tool(name))
            })
            .or_else(|| {
                (name == crate::tools::tool_search::NAME)
                    .then(|| Arc::new(crate::tools::tool_search::ToolSearch) as Arc<dyn Tool>)
            })
    }

    pub(super) fn label_for(&self, call: &ToolCall) -> String {
        self.lookup_tool(&call.name)
            .map(|t| t.label(&call.arguments))
            .unwrap_or_else(|| call.name.clone())
    }
}

/// Where a call's path arguments point with symlinks resolved, recorded when
/// it's approved and checked again right before it runs: approval covers the
/// file the path named then, and an earlier call in the batch (or anything
/// else running) may have swapped a link in since.
fn path_targets(call: &ToolCall, base: &Path) -> Vec<(String, PathBuf)> {
    let args = &call.arguments;
    let mut paths: Vec<String> = ["file_path", "notebook_path", "path"]
        .iter()
        .filter_map(|k| args[*k].as_str())
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    if call.name == "apply_patch" {
        paths.extend(crate::tools::patch::paths(
            args["input"]
                .as_str()
                .or(args["patch"].as_str())
                .unwrap_or(""),
        ));
    }
    paths
        .into_iter()
        .map(|p| {
            let target = permissions::normalize(&crate::util::resolve(base, &p));
            (p, target)
        })
        .collect()
}

/// Why a call must not run because a path now resolves somewhere other than
/// where it did at approval, if it does.
fn retargeted(
    approved: Option<&Vec<(String, PathBuf)>>,
    call: &ToolCall,
    base: &Path,
) -> Option<String> {
    let approved = approved?;
    let now = path_targets(call, base);
    if now.len() != approved.len() {
        return Some("Blocked: this call's paths changed after it was approved, so it was not run. Call it again.".into());
    }
    approved
        .iter()
        .zip(&now)
        .find(|(a, b)| a.1 != b.1)
        .map(|(a, b)| {
            format!(
                "Blocked: {} now resolves to {} but pointed to {} when this call was approved (a symlink or the working directory changed in between), so it was not run. Check the path, then call again if it's still what you mean.",
                a.0,
                b.1.display(),
                a.1.display()
            )
        })
}

/// How the static rules judged a shell command: allowed outright, and
/// whether it touches a secret file (which asks in every mode but bypass).
#[derive(Debug, Clone, Copy, PartialEq)]
struct ExecJudgement {
    allowed: bool,
    secret: bool,
}

fn judge_exec(mode: Mode, call: &ToolCall, tool: &dyn Tool, base: &Path) -> ExecJudgement {
    let kind = tool.kind_for(&call.arguments);
    ExecJudgement {
        allowed: permissions::static_decision(mode, kind, &call.name, &call.arguments, base)
            == Some(Decision::Allow),
        secret: mode != Mode::Bypass
            && permissions::secret_file_target(kind, &call.name, &call.arguments, base).is_some(),
    }
}

/// Why an approved shell command must not run now: an earlier call in the
/// batch made it reach a secret file, or made the rules that allowed it
/// outright stop doing so (a path in it now leads somewhere else).
fn exec_regressed(then: &ExecJudgement, now: &ExecJudgement) -> Option<String> {
    ((then.allowed && !now.allowed) || (!then.secret && now.secret)).then(|| {
        "Blocked: since this command was approved, an earlier call changed what it would touch (it now reaches a file that needs approval, such as a secret or a path outside the project), so it was not run. Call it again to be asked.".to_string()
    })
}
