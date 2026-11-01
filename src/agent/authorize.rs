//! Permission decisions for one tool call: sandbox and read-only policy, allow
//! rules, the auto-mode classifier, and asking the user when none of them decide.

use super::*;

impl Agent {
    /// What the auto-mode classifier knows of the user's intent: what the
    /// human typed this session (shared with subagents), and this agent's
    /// task when someone else wrote it.
    pub(super) fn intent(&self) -> permissions::Intent {
        permissions::Intent {
            requests: self.env.user_requests(),
            task: self.task_prompt.clone(),
        }
    }

    /// Note who wrote a prompt this agent is about to work on: what a
    /// frontend marked typed becomes the session's requests, anything else
    /// is the agent's task.
    pub(super) fn note_input(&mut self, input: &str) {
        if std::mem::take(&mut self.typed_input) && self.depth == 0 {
            self.env.record_user_request(input);
            self.task_prompt = None;
        } else {
            self.task_prompt = Some(input.to_string());
        }
    }

    pub(super) async fn authorize(&mut self, tool: &dyn Tool, call: &mut ToolCall) -> Auth {
        let env = self.env.clone();
        let name = tool.name();
        // An explicit read-only policy outranks everything below: hooks, allow
        // rules, mode changes and plan approval cannot enable writes.
        if let Some(reason) = env.config.sandbox.tool_denial(tool, &call.arguments) {
            return Auth::Deny(reason);
        }
        if let Some(reason) = permissions::restricted_denial(&name, &call.arguments, &env.cwd()) {
            return Auth::Deny(reason);
        }
        // Hooks first: they can rewrite the input, block, approve or force a prompt.
        let hook = crate::hooks::pre_tool_use(&env, &name, &call.arguments).await;
        self.hook_context = hook.context_text();
        if let Some(input) = hook.updated_input {
            call.arguments = input;
        }
        if let Some(r) = hook.halt {
            *env.halt.lock().unwrap() = Some(r.clone());
            return Auth::Deny(format!("Stopped by PreToolUse hook: {r}"));
        }
        // Memory decides by scope, so make the configured default explicit.
        if name == "Memory"
            && let Some(o) = call.arguments.as_object_mut()
            && !o.get("scope").is_some_and(|v| v.is_string())
        {
            o.insert("scope".into(), json!(env.config.memory.scope));
        }
        let call = &*call;
        let args = &call.arguments;
        let mut hook_ask = None;
        match hook.decision {
            Some(crate::hooks::PermDecision::Deny(r)) => {
                return Auth::Deny(format!("Blocked by PreToolUse hook: {r}"));
            }
            Some(crate::hooks::PermDecision::Allow) => return Auth::Allow,
            Some(crate::hooks::PermDecision::Ask(r)) => hook_ask = Some(r),
            None => {}
        }
        let kind = tool.kind_for(args);
        let mode = self.mode();
        let subjects = tool.rule_subjects(args);
        let mut rule = env.rules.read().unwrap().check(&name, &subjects);
        // An invoked skill's `allowed-tools` pre-approve for its turn; deny
        // and ask rules still win.
        if rule.is_none() {
            let grants = env.skill_turn.lock().unwrap().allow.clone();
            if !grants.is_empty()
                && Rules::new(&grants, &[], &[])
                    .check(&name, &subjects)
                    .is_some()
            {
                rule = Some(Decision::Allow);
            }
        }
        let explicit_ask = matches!(rule, Some(Decision::Ask));
        // Set when the auto-mode classifier (rather than a static rule) refused.
        let mut classified_block = false;
        let mut decision = match rule {
            Some(Decision::Deny(r)) => return Auth::Deny(format!("Permission denied: {r}")),
            _ if hook_ask.is_some() && mode != Mode::Bypass => Decision::Ask,
            // Computer use asks for every action in these modes, whatever rules say.
            Some(Decision::Allow)
                if name == "Computer" && matches!(mode, Mode::Default | Mode::AcceptEdits) =>
            {
                Decision::Ask
            }
            Some(Decision::Allow)
                if mode != Mode::Plan || kind == ToolKind::ReadOnly || tool.read_only_hint() =>
            {
                Decision::Allow
            }
            Some(Decision::Ask) if mode != Mode::Bypass => Decision::Ask,
            // A tool that says it only reads may help a plan, but it's the
            // tool's word: the user decides.
            _ if mode == Mode::Plan && tool.read_only_hint() => Decision::Ask,
            // What came in from outside can steer the model, and the classifier
            // doesn't see it, so sending data out takes a person from here on.
            _ if mode == Mode::Auto
                && kind == ToolKind::Execute
                && env.read_outside.load(std::sync::atomic::Ordering::Relaxed)
                && permissions::sends_data_out(args["command"].as_str().unwrap_or("")) =>
            {
                Decision::Ask
            }
            _ => match permissions::static_decision(mode, kind, &name, args, &env.cwd()) {
                Some(d) => d,
                // Auto mode, fast path (opt-in): routine shell commands skip the
                // model classifier. It only ever allows, and steps aside when the
                // user wrote their own auto-mode rules (only the model can read those).
                None if kind == ToolKind::Execute
                    && {
                        let a = env.auto_mode.read().unwrap();
                        a.1 && a.0.is_empty()
                    }
                    && matches!(
                        crate::risk::assess(args["command"].as_str().unwrap_or("")),
                        crate::risk::Verdict::Allow { .. }
                    ) =>
                {
                    Decision::Allow
                }
                None => {
                    // Auto mode: consult the classifier.
                    self.emit(Event::Busy(Some("Checking action safety…".into())));
                    let model = env
                        .config
                        .classifier_model
                        .clone()
                        .unwrap_or_else(|| self.model());
                    let (cp, cm) = env.resolve(&model);
                    let rules = env.auto_mode.read().unwrap().0.clone();
                    let d = permissions::classify(
                        &cp,
                        &cm,
                        &self.intent(),
                        &name,
                        args,
                        &rules,
                        env.config.optimize.token_classifier(),
                    )
                    .await;
                    self.emit(Event::Busy(None));
                    classified_block = matches!(d, Decision::Deny(_));
                    d
                }
            },
        };
        // Why a prompt is being shown, when it wasn't a static rule that asked.
        let mut ask_because: Option<String> = None;
        if mode == Mode::Auto {
            if let Decision::Deny(r) = &decision {
                self.consecutive_blocks += 1;
                // The classifier is advisory: "block" means "a human should
                // decide", not "refuse forever". So in auto mode a blocked
                // action becomes a permission prompt carrying the classifier's
                // reasoning, and only a refusal by the human stops it. Without
                // this a cautious classifier can silently strand a turn the
                // user wanted.
                // A static dangerous-pattern block still holds for the first
                // few attempts; repeated attempts fall through to the human.
                if env.interactive && (classified_block || self.consecutive_blocks >= 3) {
                    if !classified_block {
                        self.emit(Event::Warn(format!(
                            "Auto mode blocked {} actions in a row; asking you.",
                            self.consecutive_blocks
                        )));
                    }
                    ask_because = Some(r.clone());
                    decision = Decision::Ask;
                } else {
                    self.emit(Event::Warn(r.clone()));
                }
            } else {
                self.consecutive_blocks = 0;
            }
        }
        match decision {
            Decision::Allow => Auth::Allow,
            Decision::Deny(r) if mode == Mode::Auto => {
                let retry = crate::hooks::permission_denied(&env, &name, args, &call.id, &r).await;
                // Without a verdict (the classifier failed) a retry would only fail again.
                let no_verdict = r.starts_with("auto-mode classifier unavailable")
                    || r.starts_with("auto-mode classifier timed out");
                if retry && !no_verdict {
                    return Auth::Deny(format!(
                        "{r}\n\nA PermissionDenied hook says you may retry this tool call."
                    ));
                }
                Auth::Deny(r)
            }
            Decision::Deny(r) => Auth::Deny(r),
            Decision::Ask if self.sub.dont_ask => Auth::Deny(format!(
                "Permission to use {name} was denied: this subagent runs in dontAsk mode, which denies anything not allowed by a rule."
            )),
            Decision::Ask => {
                // Automation gets its say before we decide that no human is
                // available: a PermissionRequest hook can approve or deny.
                let fields = serde_json::json!({"tool_name": name, "tool_input": args});
                let pr = crate::hooks::notify(&env, "PermissionRequest", Some(&name), fields).await;
                if let Some(r) = pr.halt {
                    *env.halt.lock().unwrap() = Some(r.clone());
                    return Auth::Deny(format!("Stopped by PermissionRequest hook: {r}"));
                }
                if let Some(b) = pr.block {
                    return Auth::Deny(format!("Blocked by PermissionRequest hook: {b}"));
                }
                match pr.decision {
                    Some(crate::hooks::PermDecision::Allow) => return Auth::Allow,
                    Some(crate::hooks::PermDecision::Deny(r)) => {
                        return Auth::Deny(format!("Denied by PermissionRequest hook: {r}"));
                    }
                    _ => {}
                }
                // `--permission-prompt-tool`: an MCP tool answers for the absent user.
                if let Some(answer) = permission_prompt_tool(&env, &name, call).await {
                    return answer;
                }
                if !env.interactive {
                    // Leaving the sandbox is the one case auto mode can't cover
                    // for you, so say how to authorise it (a blanket "try auto"
                    // would be wrong here).
                    let rules = deny_hint_rules(&name, &subjects);
                    let how = if args["dangerouslyDisableSandbox"].as_bool() == Some(true) {
                        format!(
                            "Run with --dangerously-skip-permissions, or add an allow rule for the command (e.g. {rules})."
                        )
                    } else if mode == Mode::AcceptEdits {
                        // acceptEdits only covers file edits; suggesting it again wouldn't help.
                        format!(
                            "Run with --mode auto, or add an allow rule for the command (e.g. {rules})."
                        )
                    } else if mode == Mode::Auto {
                        format!("Add an allow rule for the command (e.g. {rules}).")
                    } else {
                        format!(
                            "Run with --mode auto/acceptEdits or add an allow rule (e.g. {rules})."
                        )
                    };
                    return Auth::Deny(format!(
                        "Permission to use {name} requires user approval, which is unavailable in non-interactive mode. ({how})"
                    ));
                }
                let mut req = self.permission_request(tool, call, kind);
                req.reason = Some(
                    ask_because
                        .or_else(|| hook_ask.clone())
                        .filter(|reason| !reason.trim().is_empty())
                        .unwrap_or_else(|| {
                            if explicit_ask {
                                "An explicit permission rule requires approval for this action."
                                    .into()
                            } else {
                                permissions::ask_reason(mode, kind, &name, args, &env.cwd())
                            }
                        }),
                );
                let msg = format!("chanto needs your permission to use {name}");
                crate::hooks::notify(
                    &env,
                    "Notification",
                    Some("permission_prompt"),
                    serde_json::json!({"message": msg, "notification_type": "permission_prompt"}),
                )
                .await;
                // With an IDE connected, an edit is also shown there as a diff;
                // whichever answers first wins.
                let ide_diff = match (kind, env.ide()) {
                    (ToolKind::Edit, Some(ide)) => {
                        crate::ide::proposed_change(&env.cwd(), &name, args)
                            .map(|(p, c)| (ide, p, c))
                    }
                    _ => None,
                };
                // An interrupted turn stops waiting for the answer.
                let cancel = self.cur_cancel.clone().unwrap_or_default();
                // Auto mode usually runs unattended, so a permission prompt must
                // not hang the turn forever: if no one answers, deny it with a
                // note the model can act on, and let it try another way.
                let auto_deny = auto_deny_after(mode, env.config.permissions.auto_deny_after_secs);
                req.auto_deny = auto_deny;
                let ask = |req| {
                    let (env, cancel) = (env.clone(), cancel.clone());
                    async move {
                        let wait = async {
                            tokio::select! {
                                r = env.frontend.permission(req) => r,
                                _ = cancel.cancelled() => PermissionReply::Deny(None),
                            }
                        };
                        match auto_deny {
                            None => wait.await,
                            Some(d) => match tokio::time::timeout(d, wait).await {
                                Ok(r) => r,
                                Err(_) => {
                                    env.frontend.emit(Event::Warn(format!(
                                        "Auto mode: no answer in {}s, so this was denied to keep the turn moving.",
                                        d.as_secs()
                                    )));
                                    PermissionReply::AutoDenied(format!(
                                        "Auto mode denied this action because no one answered the permission prompt within {}s — the user did not deny it. The turn continued without it. Try a different approach, or say what you need.",
                                        d.as_secs()
                                    ))
                                }
                            },
                        }
                    }
                };
                let reply = match ide_diff {
                    None => ask(req).await,
                    Some((ide, path, proposed)) => {
                        let prompt = ask(req);
                        tokio::pin!(prompt);
                        tokio::select! {
                            r = &mut prompt => {
                                ide.close_diffs(&path);
                                r
                            }
                            d = ide.open_diff(&path, &proposed) => match d {
                                Ok(d) if d.accepted => {
                                    self.consecutive_blocks = 0;
                                    self.emit(Event::Info(format!("Change accepted in {}", ide.name)));
                                    return match d.contents.filter(|c| *c != proposed) {
                                        Some(c) => Auth::AllowWith(ToolCall { id: call.id.clone(), name: "Write".into(), arguments: json!({"file_path": path, "content": c}) }, None),
                                        None => Auth::Allow,
                                    };
                                }
                                Ok(_) => {
                                    self.emit(Event::Info(format!("Change rejected in {}", ide.name)));
                                    return Auth::Stop;
                                }
                                // The IDE went away: keep waiting for the terminal.
                                Err(_) => prompt.await,
                            }
                        }
                    }
                };
                match reply {
                    PermissionReply::Allow => {
                        self.consecutive_blocks = 0;
                        Auth::Allow
                    }
                    PermissionReply::AllowAlways => {
                        self.consecutive_blocks = 0;
                        if kind == ToolKind::Edit {
                            env.grant_accept_edits();
                        } else if name == "Computer" {
                            // Never remembered: the next action asks again.
                        } else {
                            for r in always_rules(&name, &subjects) {
                                env.rules.write().unwrap().allow.push(Rule::parse(&r));
                                if let Err(e) = crate::config::persist_allow_rule(&env.cwd(), &r) {
                                    self.emit(Event::Warn(format!(
                                        "Could not save permission rule: {e}"
                                    )));
                                }
                            }
                        }
                        Auth::Allow
                    }
                    PermissionReply::AllowEdited(content) => {
                        self.consecutive_blocks = 0;
                        match crate::ide::proposed_change(&env.cwd(), &name, args) {
                            Some((path, proposed)) if proposed != content => {
                                let note = format!(
                                    "The user edited your proposed change to {} before approving it, so the file now has their version, not yours. Read it before changing it again.",
                                    path.display()
                                );
                                let write = ToolCall {
                                    id: call.id.clone(),
                                    name: "Write".into(),
                                    arguments: json!({"file_path": path, "content": content}),
                                };
                                Auth::AllowWith(write, Some(note))
                            }
                            _ => Auth::Allow,
                        }
                    }
                    PermissionReply::Deny(_) | PermissionReply::AutoDenied(_)
                        if kind == ToolKind::Execute && !cfg!(test) =>
                    {
                        // Never fast-allow this command's shape again.
                        crate::risk::record_denial(args["command"].as_str().unwrap_or(""));
                        match reply {
                            PermissionReply::Deny(Some(fb)) => {
                                self.denied_with(&env, tool, call, fb, true)
                            }
                            PermissionReply::AutoDenied(note) => {
                                self.denied_with(&env, tool, call, note, false)
                            }
                            _ => Auth::Stop,
                        }
                    }
                    PermissionReply::Deny(Some(fb)) => self.denied_with(&env, tool, call, fb, true),
                    PermissionReply::AutoDenied(note) => {
                        self.denied_with(&env, tool, call, note, false)
                    }
                    PermissionReply::Deny(None) => Auth::Stop,
                }
            }
        }
    }

    pub(super) fn denied_with(
        &self,
        env: &Env,
        tool: &dyn crate::tools::Tool,
        call: &ToolCall,
        fb: String,
        by_user: bool,
    ) -> Auth {
        if env.config.learning.capture() {
            crate::learn::record(&env.root, "denied", &fb, &tool.label(&call.arguments));
        }
        if by_user {
            Auth::Deny(format!("The user denied this action and said: {fb}"))
        } else {
            Auth::Deny(fb)
        }
    }

    pub(super) fn permission_request(
        &self,
        tool: &dyn Tool,
        call: &ToolCall,
        kind: ToolKind,
    ) -> PermissionRequest {
        let name = tool.name();
        let args = &call.arguments;
        let detail = match name.as_str() {
            "Bash" => format!(
                "{}\n{}",
                args["command"].as_str().unwrap_or(""),
                args["description"]
                    .as_str()
                    .map(|d| format!("\n{d}"))
                    .unwrap_or_default()
            ),
            "Edit" | "MultiEdit" | "Write" => edit_preview(&self.env.cwd(), &name, args),
            "Memory" => memory_preview(args),
            _ => serde_json::to_string_pretty(args).unwrap_or_default(),
        };
        let subjects = tool.rule_subjects(args);
        let always_label = if kind == ToolKind::Edit && self.mode() == Mode::Auto {
            "Allow once (auto mode already allows workspace edits)".to_string()
        } else if kind == ToolKind::Edit {
            "Allow all edits for this session".to_string()
        } else if name == "Computer" {
            "Allow once (each computer action asks again)".to_string()
        } else {
            format!(
                "Always allow {} in this project",
                always_rules(&name, &subjects).join(", ")
            )
        };
        let proposed = if kind == ToolKind::Edit {
            crate::ide::proposed_change(&self.env.cwd(), &name, args).map(|(_, c)| c)
        } else {
            None
        };
        PermissionRequest {
            tool: name,
            label: tool.label(args),
            detail,
            always_label,
            reason: None,
            proposed,
            auto_deny: None,
        }
    }
}

/// What a Memory save would store, for its permission prompt.
pub(super) fn memory_preview(args: &Value) -> String {
    let scope = args["scope"].as_str().unwrap_or("user");
    let where_ = if scope == "user" {
        "user memory (loaded in every project)"
    } else {
        "project memory"
    };
    let action = args["action"].as_str().unwrap_or("save");
    let name = args["name"].as_str().unwrap_or("");
    if action == "delete" {
        return format!("Delete {name} from {where_}");
    }
    format!(
        "{} {name} in {where_}\nDescription: {}\n\n{}",
        if action == "update" { "Update" } else { "Save" },
        args["description"].as_str().unwrap_or(""),
        args["body"].as_str().unwrap_or("")
    )
}
