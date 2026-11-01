//! One streaming model call: provider fallbacks, retries, image rejection, the
//! text tool protocol, and usage/cost accounting.

use super::*;

impl Agent {
    pub(super) async fn call_model(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<Option<llm::ChatResponse>> {
        crate::util::timing("model call");
        self.env.settle_model_info().await;
        // The system prompt was written before they connected: it names the
        // servers and carries their instructions.
        if self.env.settle_mcp().await {
            self.refresh_system();
        }
        // Also recover an interrupted turn in the current process, without
        // requiring the user to reload the session from disk.
        crate::session::repair_tool_pairs(&mut self.messages);
        self.publish_conversation();
        if !self.budget_ready(cancel).await {
            return Ok(None);
        }
        let mut tools = self.env.tools.specs(&|n| self.tool_allowed(n));
        // Startup hides the resource tools when no server it connected has
        // resources; one that connected since (in the background, or after a
        // login) may have.
        if self.env.mcp.iter().any(|c| c.has_resources()) {
            tools.extend(self.env.tools.hidden_specs(&|n| {
                crate::mcp::RESOURCE_TOOLS.contains(&n) && self.tool_allowed(n)
            }));
        }
        let mut mcp_tools: Vec<(llm::ToolSpec, bool)> = Vec::new();
        for c in self.env.mcp.iter().chain(&self.sub.mcp) {
            c.refresh_if_stale().await;
            mcp_tools.extend(
                c.spec_entries()
                    .into_iter()
                    .filter(|(t, _)| self.tool_allowed(&t.name)),
            );
        }
        let text_mode = self.env.text_tools.load(Ordering::Relaxed);
        // Tool search: deferred MCP tools are announced by name (appended to the
        // newest message, so nothing already sent changes) and join the end of
        // the tool list once loaded, keeping earlier definitions in place.
        let mut deferrable: Vec<llm::ToolSpec> = mcp_tools
            .iter()
            .filter(|(_, upfront)| !upfront)
            .map(|(t, _)| t.clone())
            .collect();
        let search_allowed = self.tool_allowed(crate::tools::tool_search::NAME);
        let defer_mcp = search_allowed
            && crate::tools::tool_search::Setting::from_config(&self.env.config)
                .defers(&deferrable, self.env.context_max());
        if !defer_mcp {
            deferrable.clear();
        }
        // `optimize.defer_tools`: rarely used built-in tools go the same way.
        if search_allowed && self.env.config.optimize.defer_tools() {
            let upfront = self.upfront_builtins.clone().unwrap_or_default();
            let (later, now) = tools.into_iter().partition(|t| {
                crate::tools::tool_search::DEFERRED_BUILTINS.contains(&t.name.as_str())
                    && !upfront.contains(&t.name)
            });
            tools = now;
            deferrable.extend::<Vec<_>>(later);
        }
        if !deferrable.is_empty() {
            tools.push(crate::tools::tool_search::spec());
            tools.extend(
                mcp_tools
                    .iter()
                    .filter(|(_, upfront)| *upfront || !defer_mcp)
                    .map(|(t, _)| t.clone()),
            );
            let announced = crate::tools::tool_search::announced(&self.messages);
            let new: Vec<&llm::ToolSpec> = deferrable
                .iter()
                .filter(|t| !announced.contains(&t.name))
                .collect();
            if !new.is_empty() {
                let note = crate::tools::tool_search::announcement(&new);
                match self.messages.last_mut() {
                    Some(m) if matches!(m.role, Role::User | Role::Tool) => {
                        std::sync::Arc::make_mut(&mut m.content).push_str("\n\n");
                        std::sync::Arc::make_mut(&mut m.content).push_str(&note);
                    }
                    _ => self.messages.push(Message::user(note)),
                }
            }
            // The text protocol lists tools in the system prompt, so loaded
            // tools stay out of it: their definitions are in the ToolSearch result.
            if !text_mode {
                let loaded = crate::tools::tool_search::loaded(&self.messages, &deferrable);
                // A deferred built-in this project needed goes upfront next session.
                for n in &loaded {
                    if crate::tools::tool_search::DEFERRED_BUILTINS.contains(&n.as_str())
                        && self.recorded_builtins.insert(n.clone())
                    {
                        let cwd = self.cwd_override.clone().unwrap_or_else(|| self.env.cwd());
                        crate::tools::tool_search::record_used(&cwd, n);
                    }
                }
                tools.extend(
                    loaded
                        .iter()
                        .filter_map(|n| deferrable.iter().find(|t| &t.name == n))
                        .cloned(),
                );
            }
        } else {
            tools.extend(mcp_tools.into_iter().map(|(t, _)| t));
        }
        let mut messages = self.messages.clone();
        // A replaced system prompt (`--system-prompt`, or an `--agent`
        // definition's prompt) is the user's own text: the project context
        // appended to it stays in place, in the order it was built in, rather
        // than being rewritten into the first user message. Subagents keep the
        // same shape: their first user message is the task they were handed
        // (a reviewer's request, a subagent's prompt), sent as written.
        if self.env.config.optimize.stable_prefix() && self.system_base.is_none() && self.depth == 0
        {
            move_project_context(&mut messages);
        }
        let (mut messages, tools) = if text_mode {
            (to_text_protocol(&messages, &tools), vec![])
        } else {
            (messages, tools)
        };
        // Nothing secret leaves the machine (or reaches a local model's logs).
        self.message_redactor.redact(&mut messages);
        let fallbacks = self.fallback_chain();
        self.settle_effort();
        let req = ChatRequest {
            model: self
                .turn_fallback
                .and_then(|i| fallbacks.get(i).cloned())
                .unwrap_or_else(|| self.model()),
            messages,
            tools,
            // After thinking ran out the output cap (see `think_less`).
            think: Some(self.env.think() && self.think_less < 2),
            effort: if self.think_less == 1 && self.env.think() {
                Some("low".into())
            } else {
                self.effort()
            },
            temperature: self.env.config.temperature,
            // Cap a single response so a degenerate repetition loop can't run
            // forever. A call already cut off at the cap with nothing usable in
            // it gets double the room (see `output_cap_boost`): a reasoning
            // model can spend the whole budget thinking and emit no text.
            max_tokens: Some(
                (self.env.context_max() / 2).min(16384u64 << self.output_cap_boost.min(2)) as u32,
            ),
            json: false,
        };
        self.stats.before_call(
            &format!("{} {:?} {:?}", req.model, req.effort, req.think),
            &serde_json::to_string(&req.tools).unwrap_or_default(),
            &req.messages,
        );
        self.emit(Event::Busy(Some("Working…".into())));
        self.emit(Event::AssistantStart);
        let env = self.env.clone();
        let quiet = self.depth > 0;
        // A subagent's text goes to the agents panel instead.
        let live_key = if quiet { self.parent.clone() } else { None };
        let t0 = Instant::now();
        let first_token: Mutex<Option<Instant>> = Mutex::new(None);
        // Whatever the model streams before an interrupt. Kept so an `esc` doesn't
        // silently throw away the partial answer the user just watched arrive.
        let streamed: Mutex<String> = Mutex::new(String::new());
        // MessageDisplay hooks see (and may replace) the text before it's shown.
        let mut display = (!quiet && crate::hooks::has(&self.env, "MessageDisplay"))
            .then(|| crate::hooks::message_display(&self.env));
        let display_tx = Mutex::new(display.as_ref().map(|(tx, _)| tx.clone()));
        let bash_allowed = self.tool_allowed("Bash");
        let mut on_delta = |d: Delta| {
            if let Delta::ToolCall(name) = &d {
                // A session's first shell command waits for the shell snapshot
                // (the user's startup files: seconds on a busy setup). Starting
                // it as soon as the model begins a Bash call hides some of that
                // behind the rest of the reply. Sessions that never run Bash
                // never start the login shell.
                if bash_allowed && name == "Bash" && !env.shell_snapshot.initialized() {
                    let env = env.clone();
                    tokio::spawn(async move { crate::shell::snapshot::ensure(&env).await });
                }
                return;
            }
            {
                let mut ft = first_token.lock().unwrap();
                if ft.is_none() {
                    *ft = Some(Instant::now());
                }
            }
            if let Delta::Text(t) = &d {
                streamed.lock().unwrap().push_str(t);
            }
            if quiet {
                if let (Some(k), Delta::Text(t)) = (&live_key, &d) {
                    env.live_agents.text(k, t);
                }
                return;
            }
            match d {
                Delta::Text(t) => match display_tx.lock().unwrap().as_ref() {
                    Some(tx) => {
                        let _ = tx.send(t);
                    }
                    None => env.emit(Event::Text(t)),
                },
                Delta::Reasoning(t) => env.emit(Event::Reasoning(t)),
                Delta::ToolCall(_) => {}
            }
        };
        // Show what the MessageDisplay hooks still hold, before the message ends.
        async fn finish_display(
            tx: &Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>,
            display: &mut Option<(
                tokio::sync::mpsc::UnboundedSender<String>,
                tokio::task::JoinHandle<()>,
            )>,
        ) {
            tx.lock().unwrap().take();
            if let Some((tx, handle)) = display.take() {
                drop(tx);
                let _ = handle.await;
            }
        }
        let mut spec = req.model.clone();
        let (mut provider, model_name) = self.env.resolve(&spec);
        let mut req = ChatRequest {
            model: model_name,
            ..req
        };
        let mut attempt = 0;
        // Whether we've already dropped images after the endpoint refused them.
        let mut images_dropped = false;
        // Times this request waited out a usage limit (a reset that doesn't take mustn't loop forever).
        let mut limit_waits = 0;
        let mut resp = loop {
            // Masked above (`message_redactor`).
            let fut = provider.chat_masked(&req, &mut on_delta);
            let r = tokio::select! {
                r = fut => Some(r),
                _ = cancel.cancelled() => None,
            };
            match r {
                None => {
                    finish_display(&display_tx, &mut display).await;
                    self.emit(Event::AssistantEnd);
                    self.emit(Event::Busy(None));
                    if let Some(keep) = Self::interrupted_keep(&streamed.lock().unwrap()) {
                        self.messages.push(llm::Message::assistant(keep));
                    }
                    return Ok(None);
                }
                Some(Ok(r)) => break r,
                Some(Err(e))
                    if req.think != Some(false)
                        && e.downcast_ref::<llm::ReasoningLoop>().is_some() =>
                {
                    self.emit(Event::Warn(format!(
                        "{e}; retrying this step without thinking."
                    )));
                    req.think = Some(false);
                }
                Some(Err(e)) => {
                    attempt += 1;
                    let msg = format!("{e:#}");
                    // We assume OpenAI-compatible endpoints accept images (they
                    // report no capabilities), so a text-only model rejects the
                    // whole request. Drop the images and retry once rather than
                    // failing the turn, and say so.
                    if !images_dropped && looks_like_image_rejection(&msg) {
                        let dropped = drop_images(&mut req.messages);
                        if dropped > 0 {
                            images_dropped = true;
                            self.emit(Event::Warn(format!(
                                "This endpoint rejected the image(s) ({msg}); resending {dropped} message(s) without them."
                            )));
                            continue;
                        }
                    }
                    let no_tools = msg.contains("does not support tools")
                        || (msg.contains("tool")
                            && (msg.contains("not supported") || msg.contains("unsupported")));
                    if no_tools && !self.env.text_tools.load(Ordering::Relaxed) {
                        self.env.text_tools.store(true, Ordering::Relaxed);
                        self.emit(Event::Warn("This model has no native tool calling; switching to the text tool-call protocol.".into()));
                        finish_display(&display_tx, &mut display).await;
                        self.emit(Event::AssistantEnd);
                        self.emit(Event::Busy(None));
                        return Box::pin(self.call_model(cancel)).await;
                    }
                    // Only retry if nothing was streamed yet (otherwise the UI would show duplicates).
                    // Nothing listening: retrying won't start the server.
                    let refused = llm::connection_refused(&msg);
                    let nothing_streamed = first_token.lock().unwrap().is_none();
                    let transient = nothing_streamed
                        && !refused
                        && (msg.contains("connect")
                            || msg.contains("timed out")
                            || e.downcast_ref::<llm::StreamStalled>().is_some()
                            || msg.contains("503")
                            || msg.contains("502")
                            || msg.contains("error decoding")
                            || msg.contains("overloaded")
                            || msg.contains("rate_limit"));
                    // With a fallback model waiting, one more try is enough
                    // (hosted providers have already retried the request).
                    let next = self.turn_fallback.map_or(0, |i| i + 1);
                    let fallback = (nothing_streamed && llm::warrants_fallback(&msg))
                        .then(|| fallbacks.get(next).cloned())
                        .flatten();
                    let tries = if fallback.is_some() { 1 } else { 3 };
                    if attempt <= tries && transient {
                        self.emit(Event::Warn(format!(
                            "Model request failed ({}); retrying ({attempt}/{tries})…",
                            llm::root_cause(&msg)
                        )));
                        tokio::time::sleep(std::time::Duration::from_millis(1000 * attempt)).await;
                        continue;
                    }
                    if let Some(to) = fallback {
                        self.emit(Event::Warn(format!(
                            "{spec} is unavailable ({}); switching to the fallback model {to} for the rest of this turn.",
                            crate::util::one_line(llm::root_cause(&msg), 120)
                        )));
                        self.turn_fallback = Some(next);
                        // A fallback with a smaller window gets a summary it can hold
                        // (from itself: the model it replaces is the one failing).
                        if self.outgrows(&to).is_some() {
                            finish_display(&display_tx, &mut display).await;
                            self.emit(Event::AssistantEnd);
                            self.emit(Event::Busy(None));
                            self.emit(Event::Info(format!("❖ The conversation is too long for {to}'s context window; compacting it first")));
                            self.compact_with(None, "auto", cancel, None).await?;
                            return Box::pin(self.call_model(cancel)).await;
                        }
                        let (p, m) = self.env.resolve(&to);
                        provider = p;
                        req.model = m;
                        spec = to;
                        attempt = 0;
                        images_dropped = false;
                        continue;
                    }
                    // A plan's usage limit with a known reset: wait for it and carry on.
                    if let Some(at) = nothing_streamed
                        .then(|| self.limit_reset(&e))
                        .flatten()
                        .filter(|_| limit_waits < 3)
                    {
                        limit_waits += 1;
                        match self.wait_for_limit(at, &msg, cancel).await {
                            LimitWait::Resumed => {
                                attempt = 0;
                                continue;
                            }
                            LimitWait::Cancelled => {
                                finish_display(&display_tx, &mut display).await;
                                self.emit(Event::AssistantEnd);
                                self.emit(Event::Busy(None));
                                return Ok(None);
                            }
                            LimitWait::No => {}
                        }
                    }
                    finish_display(&display_tx, &mut display).await;
                    self.emit(Event::AssistantEnd);
                    self.emit(Event::Busy(None));
                    let (p, model) = self.env.resolve(&spec);
                    // A server that isn't running is a setup matter, not a bug.
                    if !refused {
                        crate::feedback::report_error("model_request", &msg);
                    }
                    return Err(
                        match llm::failure_hint(
                            &msg,
                            p.kind,
                            &p.base_url,
                            &model,
                            p.billing_provider.as_deref(),
                        ) {
                            // The hint says what to do; of the HTTP client's error chain, only its last cause adds anything.
                            _ if refused => anyhow::anyhow!(
                                "Can't reach the model server at {} ({}). Is it running? `/model` switches models; `chanto doctor` checks the setup.",
                                p.base_url,
                                llm::root_cause(&msg)
                                    .trim_end_matches(|c: char| c == ')'
                                        || c.is_ascii_digit()
                                        || c == ' ')
                                    .trim_end_matches("(os error")
                                    .trim()
                            ),
                            Some(hint) => e.context(hint),
                            None => e,
                        },
                    );
                }
            }
        };
        self.last_model = Some(spec.clone());
        finish_display(&display_tx, &mut display).await;
        *self.env.hook_state.last_call.lock().unwrap() = Some(Instant::now());
        self.emit(Event::AssistantEnd);
        self.emit(Event::Busy(None));
        let gen_secs = first_token
            .lock()
            .unwrap()
            .map(|f| f.elapsed().as_secs_f64())
            .unwrap_or(t0.elapsed().as_secs_f64());
        let tps = if gen_secs > 0.0 {
            resp.usage.completion_tokens as f64 / gen_secs
        } else {
            0.0
        };
        // Whatever a provider reports: everything downstream adds these up.
        const MOST_TOKENS: u64 = 1 << 40;
        for n in [
            &mut resp.usage.prompt_tokens,
            &mut resp.usage.completion_tokens,
            &mut resp.usage.cached_tokens,
            &mut resp.usage.cache_write_tokens,
        ] {
            *n = (*n).min(MOST_TOKENS);
        }
        if resp.usage.prompt_tokens > 0 {
            self.last_prompt_tokens = resp
                .usage
                .prompt_tokens
                .saturating_add(resp.usage.completion_tokens);
            self.msgs_at_last_usage = self.messages.len() + 1;
        }
        self.total_in = self.total_in.saturating_add(resp.usage.prompt_tokens);
        self.total_out = self.total_out.saturating_add(resp.usage.completion_tokens);
        self.total_cached = self.total_cached.saturating_add(resp.usage.cached_tokens);
        let prefill = first_token
            .lock()
            .unwrap()
            .map(|f| (f - t0).as_secs_f64())
            .unwrap_or(0.0);
        self.stats.after_call(
            resp.usage.prompt_tokens,
            resp.usage.completion_tokens,
            t0.elapsed().as_secs_f64(),
            prefill,
        );
        if let Some(s) = resp.rate_limits.clone() {
            *self.env.rate_limits.lock().unwrap() = Some(s.clone());
            self.env.spend.limits(&s);
            self.emit(Event::Limits(s));
        }
        let cost = crate::spend::price_on(&self.env, &provider, &req.model)(&resp.usage);
        let notes = self.env.spend.model_call(
            &self.spend_key(),
            &spec,
            &resp.usage,
            cost,
            self.env.context_max(),
        );
        crate::spend::notify(&self.env, notes);
        self.emit_usage(tps);
        Ok(Some(resp))
    }
}
