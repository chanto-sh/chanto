//! The turn loop: what the model sees, streaming its reply, running the tool
//! calls it asks for, and handling interrupts, limits and re-planning.

use super::*;

impl Agent {
    /// Run one user turn to completion (model ↔ tools loop).
    pub async fn run_turn(
        &mut self,
        input: &str,
        images: Vec<String>,
        cancel: CancellationToken,
    ) -> Result<TurnEnd> {
        // No idle sleep mid-turn (`keep_awake`); released once the turn ends.
        let _awake = crate::awake::hold();
        if let Some(acp) = &mut self.acp {
            return acp.run_turn(&self.env, input, images, cancel).await;
        }
        let seq = self.edit_seq;
        if self.upfront_builtins.is_none() && self.env.config.optimize.defer_tools() {
            let cwd = self.cwd_override.clone().unwrap_or_else(|| self.env.cwd());
            let used = crate::tools::tool_search::used_builtins(&cwd);
            self.upfront_builtins = Some(crate::tools::tool_search::upfront_builtins(
                &cwd, input, &used,
            ));
            self.recorded_builtins = used.into_iter().collect();
        }
        if self.depth == 0 {
            // An invoked skill's grants last until the user's next message.
            self.env.skill_turn.lock().unwrap().begin_turn();
        }
        let end = match self.sub.hooks.clone() {
            // A subagent's own hooks run only while it does.
            Some(hooks) => {
                crate::hooks::SCOPE
                    .scope(
                        crate::hooks::Scope {
                            hooks: Some(hooks),
                            off: false,
                        },
                        self.run_turn_inner(input, images, cancel),
                    )
                    .await
            }
            None => self.run_turn_inner(input, images, cancel).await,
        };
        // The checkpoint snapshot runs alongside the turn. Anything that changes
        // files waits for it before running, so if the turn changed something
        // it's already done; make sure the saved session records it. A turn
        // that only read needs nothing from it (the next checkpoint describes
        // the same state), so a quick answer doesn't wait on a cold snapshot.
        if self.edit_seq != seq {
            self.env.wait_snapshot().await;
        }
        // An interrupted turn (esc, ctrl+c, SIGTERM) keeps the user's message:
        // save it now, as the process may be about to exit.
        if self.depth == 0 && matches!(end, Ok(TurnEnd::Interrupted)) {
            self.save_session();
        }
        end
    }

    /// An error when `input` alone can't fit in the model's context window
    /// beside the system prompt (counted up to half the window).
    pub(super) fn oversized_input(&self, input: &str) -> Option<anyhow::Error> {
        let max = self.env.context_max();
        let tokens = llm::estimate_tokens(&[Message::user(input)]);
        let system = self
            .messages
            .first()
            .filter(|m| m.role == Role::System)
            .map(|m| llm::estimate_tokens(std::slice::from_ref(m)))
            .unwrap_or(0);
        if max == 0 || tokens + system.min(max / 2) <= max {
            return None;
        }
        Some(anyhow::anyhow!(
            "The input is too large to send: ~{tokens} tokens, and the model's context is {max} tokens. Attach it as a file instead (save it and mention the path, so the model reads only what it needs) or trim it."
        ))
    }

    /// Build this turn's user message: the prompt plus the context the model
    /// needs (reminders, memory, hooks) and a checkpoint for `/rewind`, merged
    /// into a waiting user message so strict chat templates keep alternating.
    pub(super) async fn prepare_turn(
        &mut self,
        input: &str,
        images: Vec<String>,
    ) -> Result<Option<TurnEnd>> {
        crate::util::timing("run_turn start");
        self.turn_evidence.clear();
        // A fallback model serves one turn; each turn tries the session's model first.
        self.turn_fallback = None;
        // `/model` is handled immediately even mid-turn (it only needs the env),
        // so the system prompt it affects is rebuilt here before it is used.
        if self.system_for_model.as_deref() != Some(self.env.model().as_str())
            || self.system_quiet != self.env.quiet()
        {
            self.refresh_system();
        }
        // Compaction can't shrink the user's own message: one bigger than the
        // context window would fail at the server or be cut silently.
        if let Some(e) = self.oversized_input(input) {
            return Err(e);
        }
        let mut images = images;
        if !images.is_empty() && !self.env.vision() {
            // A text-only model would reject these at the provider; drop them with
            // an actionable message rather than failing the whole turn.
            self.emit(Event::Warn(format!(
                "The current model ({}) does not accept images, so {} attached image(s) were dropped. Switch to a vision model (for example `/model chatgpt/gpt-5`) to use them.",
                self.env.model(),
                images.len()
            )));
            images.clear();
        }
        let mut content = input.to_string();
        if self.depth == 0 {
            crate::util::timing("before secrets refresh");
            crate::secrets::refresh(&self.env.root);
            crate::util::timing("secrets refreshed");
            let (_, found) = crate::secrets::redact_with_labels(input);
            if !found.is_empty() {
                self.emit(Event::Warn(format!(
                    "Your message contains what looks like a secret ({}). It's masked before anything is sent to the model or saved.",
                    found.join(", ")
                )));
            }
        }
        if self.depth == 0 && self.env.config.learning.capture() {
            if crate::learn::is_correction(input) {
                let last = self
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == Role::Assistant && !m.content.trim().is_empty())
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                crate::learn::record(
                    &self.env.root,
                    "correction",
                    input,
                    &crate::util::truncate(&last, 400),
                );
            }
            // Files the user changed by hand after the agent wrote them.
            for (path, written) in std::mem::take(&mut self.agent_wrote) {
                if let Ok(now) = std::fs::read_to_string(&path)
                    && now != written
                {
                    let d = crate::learn::small_diff(&written, &now);
                    crate::learn::record(
                        &self.env.root,
                        "user_edit",
                        &format!(
                            "The user edited {} after the agent wrote it",
                            path.display()
                        ),
                        &d,
                    );
                }
            }
        }
        if self.depth == 0 {
            // A halt left over from an interrupted turn must not end this one.
            self.env.halt.lock().unwrap().take();
            *self.env.hook_state.turn_id.lock().unwrap() = uuid::Uuid::new_v4().to_string();
            if let Some(s) = &self.session {
                self.env.accounting.set_session(&s.id);
                *self.env.session_info.lock().unwrap() = Some((s.id.clone(), s.path()));
            }
            let r = crate::hooks::user_prompt_submit(&self.env, input).await;
            if let Some(reason) = r.block.or(r.halt) {
                self.emit(Event::Warn(format!("Prompt blocked by hook: {reason}")));
                return Ok(Some(TurnEnd::Done(String::new())));
            }
            let mut extra: Vec<String> =
                self.env.pending_context.lock().unwrap().drain(..).collect();
            // Parallel sessions in other terminals: say what this one is doing,
            // and tell the agent what they're doing.
            crate::presence::set_task(input);
            extra.extend(crate::presence::context());
            extra.extend(r.context);
            // `--bare` skips auto memory.
            if !self.messages.iter().any(|m| m.role == Role::User)
                && (!cfg!(test) || std::env::var_os("CHANTO_HOME").is_some())
                && !crate::launch::get().no_customizations()
            {
                if self.env.config.learning.lessons_enabled() {
                    match crate::learn::migrate_memories(&self.env.root) {
                        Ok(changes) => {
                            for change in changes {
                                self.emit(Event::Info(format!("\x1b[2m{change}\x1b[0m")));
                            }
                        }
                        Err(e) => self.emit(Event::Warn(format!("Memory migration: {e}"))),
                    }
                }
                match crate::memory::first_turn_context(
                    &crate::memory::Store::new(&self.env.root),
                    &self.messages,
                ) {
                    Ok(Some(index)) => {
                        let index = if self.env.config.learning.lessons_enabled() {
                            index
                        } else {
                            index
                                .lines()
                                .filter(|line| !line.contains("- lesson-"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        };
                        // Empty when no memories survive the filter: an empty
                        // index is a heading with nothing under it.
                        if !index.trim().is_empty() {
                            extra.push(index);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => self.emit(Event::Warn(format!("Memory index: {e}"))),
                }
            }
            // First turn: what's in the project, and the files the request names.
            crate::util::timing("before primer");
            if self.env.config.primer != Some(false)
                && !self.messages.iter().any(|m| m.role == Role::User)
            {
                let root = self.cwd_override.clone().unwrap_or_else(|| self.env.cwd());
                if let Some(listing) = crate::primer::project_listing(&root) {
                    extra.push(listing);
                }
                for (path, text) in crate::primer::mentioned_files(&root, input) {
                    self.env.mark_read(&path);
                    let shown = path
                        .strip_prefix(&root)
                        .unwrap_or(&path)
                        .display()
                        .to_string();
                    extra.push(format!("Contents of {shown} (named in the request; already read, so you can edit it directly):\n{}", crate::primer::numbered(&text)));
                }
            }
            extra.extend(crate::ide::turn_context(&self.env).await);
            for ctx in extra {
                content.push_str(&format!("\n\n<system-reminder>\n{ctx}\n</system-reminder>"));
            }
        }
        if let Some(r) = crate::context::mode_reminder(self.mode())
            && self.depth == 0
        {
            content.push_str("\n\n");
            content.push_str(r);
        }
        if self.depth == 0 {
            // Nothing changes files before the model's first reply, so the
            // snapshot runs alongside the model call; file-changing tools wait.
            self.env.wait_snapshot().await;
            let id = uuid::Uuid::new_v4().to_string();
            // A new prompt ends what /redo can bring back.
            self.env.redo.lock().unwrap().clear();
            self.env.checkpoints.lock().unwrap().push(Checkpoint {
                id: id.clone(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                tree: None,
                msg_index: self.messages.len(),
                prompt: input.lines().next().unwrap_or("").to_string(),
                files: HashMap::new(),
            });
            let env = self.env.clone();
            *self.env.pending_snapshot.lock().unwrap() =
                Some(tokio::task::spawn_blocking(move || {
                    crate::util::timing("snapshot start");
                    // After `/cd` out of the project, a snapshot of the old project
                    // says nothing about this turn's files (and restoring it would
                    // revert the user's own changes there): rewind uses the
                    // turn's per-file copies instead.
                    let moved = !permissions::normalize(&env.cwd())
                        .starts_with(permissions::normalize(&env.root));
                    let tree = if moved { None } else { env.shadow_snapshot() };
                    crate::util::timing("snapshot done");
                    if let Some(cp) = env
                        .checkpoints
                        .lock()
                        .unwrap()
                        .iter_mut()
                        .find(|c| c.id == id)
                    {
                        cp.tree = tree;
                    }
                }));
        }
        // Merge into a trailing user message (e.g. `!cmd` output, an interrupted turn)
        // so roles keep alternating for strict chat templates.
        let n_msgs = self.messages.len();
        match self.messages.last_mut() {
            Some(last) if last.role == Role::User && n_msgs > 1 => {
                last.content.push_str("\n\n");
                last.content.push_str(&content);
                last.images.extend(images);
                if self.depth == 0
                    && let Some(cp) = self.env.checkpoints.lock().unwrap().last_mut()
                {
                    cp.msg_index = cp.msg_index.min(n_msgs - 1);
                }
            }
            _ => {
                let mut m = Message::user(content);
                m.images = images;
                self.messages.push(m);
            }
        }
        Ok(None)
    }

    pub(super) async fn run_turn_inner(
        &mut self,
        input: &str,
        images: Vec<String>,
        cancel: CancellationToken,
    ) -> Result<TurnEnd> {
        if let Some(end) = self.prepare_turn(input, images).await? {
            return Ok(end);
        }
        let guard = self.env.config.guardrails.clone();
        let forced = self.forced_verify.take();
        let verify = self.depth == 0 && (guard.enabled || forced.is_some());
        let verify_root = self
            .cwd_override
            .clone()
            .unwrap_or_else(|| self.env.root.clone());
        let mut verify_checks: Vec<String> = if !verify {
            vec![]
        } else if guard.checks.is_empty() {
            crate::guardrails::detect_checks(&verify_root)
        } else {
            guard.checks.clone()
        };
        if let Some((extra, _)) = &forced {
            for c in extra {
                if !verify_checks.contains(c) {
                    verify_checks.push(c.clone());
                }
            }
        }
        if verify && self.env.config.learning.lessons_enabled() {
            for c in crate::learn::checks(&self.env.root) {
                if !verify_checks.contains(&c) {
                    verify_checks.push(c);
                }
            }
        }
        crate::util::timing("before test snapshot");
        // Existing tests are protected in every mode, not only `--verify`: a
        // turn that "finishes" by deleting or skipping a test has not done the
        // work. A weaker snapshot is cheap next to the model calls.
        let mut test_snapshot = (self.depth == 0 && guard.protect_tests != Some(false))
            .then(|| crate::guardrails::snapshot(&verify_root));
        if let (Some(s), Some((_, dirs))) = (test_snapshot.as_mut(), &forced) {
            s.lock_dirs(dirs);
        }
        let mut gate_attempts = 0u32;
        // How many times a finish was bounced for weakening existing tests
        // outside `--verify` (where the gate handles it instead).
        let mut test_bounces = 0u32;
        // How many times a finish was bounced because the last check the agent
        // ran itself had failed.
        let mut check_bounces = 0u32;
        // How many times a finish was bounced because the failure it saw was
        // only ever re-run at a narrower scope.
        let mut stale_bounces = 0u32;
        // Existing tests changed mid-turn (reported as soon as it happens, and
        // remembered so the same change isn't named twice).
        let mut test_edit_notices = 0u32;
        let mut test_edits_seen: Vec<String> = Vec::new();
        // A finish that never ran the tests covering the files it changed.
        let mut related_nudges = 0u32;
        // A turn that ends without edits is still checked once: claiming
        // "done" without doing the work is the commonest false finish.
        let mut gate_ran = false;
        let mut oracle_ran = false;
        let mut verdict: Option<String> = None;
        self.last_verify = None;
        let scope = self
            .env
            .verify_scope
            .read()
            .unwrap()
            .or(guard.scope)
            .unwrap_or(crate::guardrails::Scope::Related);
        // Running everything is the simple choice when it's cheap: a small
        // project, or a full suite this session already timed as fast.
        let fast_full = verify
            && scope == crate::guardrails::Scope::Related
            && (verify_checks.iter().all(|c| {
                self.full_secs
                    .get(c)
                    .is_some_and(|s| *s <= crate::guardrails::FAST_FULL_SECS)
            }) || crate::guardrails::small_project(&verify_root));
        self.check_runs.clear();
        self.check_timeouts.clear();
        if verify {
            self.turn_mutations = 0;
            self.turn_untracked_writes = false;
            self.turn_started = Some(SystemTime::now());
            self.baseline = None;
            // Which checks fail before any change, run alongside the first
            // model call; edits wait for it (see execute_calls). With related
            // scope on a larger project the gate runs only some tests, so
            // their baseline is taken later, only if they fail.
            self.baseline_task = (!verify_checks.is_empty()
                && (scope == crate::guardrails::Scope::Full || fast_full))
                .then(|| {
                    let t = std::time::Duration::from_secs(guard.timeout_secs.unwrap_or(600));
                    tokio::spawn(crate::guardrails::baseline(
                        verify_checks.clone(),
                        verify_root.clone(),
                        t,
                    ))
                });
            let checks = if verify_checks.is_empty() {
                "no checks detected".to_string()
            } else {
                let list = verify_checks
                    .iter()
                    .map(|c| format!("`{c}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                if scope == crate::guardrails::Scope::Related && !fast_full {
                    format!(
                        "the tests related to the files you change (from {list}; the full suite when shared test setup changes)"
                    )
                } else {
                    list
                }
            };
            if let Some(last) = self.messages.last_mut().filter(|m| m.role == Role::User) {
                last.content.push_str(&format!(
                    "\n\n<system-reminder>Verified mode: when you finish, chanto runs {checks} and checks that existing tests were not deleted or weakened. \
                     Run them yourself before your final answer, and fix the code, not the tests. The user's instructions come first: \
                     if a check fails in code they told you to leave alone, leave it and say so in your answer.</system-reminder>"
                ));
            }
        }
        let max_turns = self
            .sub
            .max_turns
            .unwrap_or_else(|| self.env.config.max_turns.unwrap_or(500));
        let mut final_text = String::new();
        let mut empty_retries = 0;
        self.output_cap_boost = 0;
        self.think_less = 0;
        // The last batch's calls and results, and how many batches in a row
        // matched them exactly. Only that trailing run counts toward a loop,
        // so earlier batches (with all their output) aren't kept.
        let mut recent_calls: Option<((String, String), usize)> = None;
        let mut watch = crate::loopwatch::LoopWatch::default();
        let mut loop_strikes = 0;
        let mut exhausted = true;
        let mut stop_hook_active = false;
        // The provenance check sends unsupported claims back at most once a turn.
        let mut provenance_bounced = false;
        // A finish with items still open on the model's own todo list. Bounded
        // so a list it has decided not to finish cannot loop the turn.
        let mut todo_bounces = 0u32;
        for _ in 0..max_turns {
            if cancel.is_cancelled() {
                return Ok(TurnEnd::Interrupted);
            }
            if !self.budget_ready(&cancel).await {
                return Ok(TurnEnd::Done(
                    self.env.accounting.check().unwrap_err().to_string(),
                ));
            }
            self.maybe_compact(&cancel).await?;
            let called = self.call_model(&cancel).await;
            if let (Err(e), 0) = (&called, self.depth) {
                crate::hooks::stop_failure(&self.env, &format!("{e:#}")).await;
            }
            let resp = match called? {
                Some(r) => r,
                None => {
                    if let Err(e) = self.env.accounting.check() {
                        return Ok(TurnEnd::Done(e.to_string()));
                    }
                    return Ok(TurnEnd::Interrupted);
                }
            };
            if !self.budget_ready(&cancel).await {
                return Ok(TurnEnd::Done(
                    self.env.accounting.check().unwrap_err().to_string(),
                ));
            }
            let mut text = resp.text.clone();
            let mut calls = resp.tool_calls.clone();
            if calls.is_empty()
                && (text.contains("<tool_call>")
                    || text.contains("<function_call>")
                    || text.trim_start().starts_with('{'))
            {
                let names: Vec<String> = self
                    .env
                    .tools
                    .names()
                    .into_iter()
                    .chain(self.env.mcp.iter().flat_map(|c| c.tool_names()))
                    .chain([crate::tools::tool_search::NAME.to_string()])
                    .collect();
                let (rest, parsed) = llm::extract_text_tool_calls(&text, &names);
                // With native tool calling, a model that falls back to writing
                // the call as text sends nothing else; a call inside prose is a
                // quotation (of a file or page it read), not an action.
                let native = !self
                    .env
                    .text_tools
                    .load(std::sync::atomic::Ordering::Relaxed);
                if !parsed.is_empty() && !(native && !rest.trim().is_empty()) {
                    text = rest;
                    calls = parsed;
                }
            }
            if !calls.is_empty() && resp.finish_reason.as_deref() == Some("length") {
                // Truncated mid tool call: arguments may be incomplete; don't execute.
                self.messages.push(Message::assistant(text.clone()));
                self.stats.turns += 1;
                self.messages.push(Message::user("<system-reminder>Your previous response was cut off at the output token limit before the tool call was complete, so it was not executed. Break the work into smaller steps (e.g. write a large file in several Edit calls).</system-reminder>"));
                self.emit(Event::Warn("Response truncated at the output limit; asking the model to use smaller steps.".into()));
                continue;
            }
            let mut msg = Message::assistant(text.clone());
            // The repair marker is for execute_calls only; the transcript keeps the call as sent.
            msg.tool_calls = calls.iter().map(without_repair_marker).collect();
            msg.provider_items = resp.provider_items.clone();
            if text.trim().is_empty() && calls.is_empty() {
                // Empty response (often after a long thinking run; common with
                // small models): nudge. Only consecutive empties count, and
                // giving up finishes the turn normally, so the verify gate runs.
                empty_retries += 1;
                if empty_retries <= 3 {
                    // Cut off at the output cap with nothing in it: the budget
                    // went to thinking, so retrying at the same cap just repeats
                    // it, and a bigger cap only buys a longer think (one
                    // Terminal-Bench task ran 208k characters of reasoning
                    // through three retries and ended with an empty answer).
                    // Give the next call more room and less thinking: low
                    // effort, then none, so it has to write or call a tool.
                    let truncated = resp.finish_reason.as_deref() == Some("length");
                    if truncated {
                        self.output_cap_boost = (self.output_cap_boost + 1).min(2);
                        self.think_less = (self.think_less + 1).min(2);
                    }
                    let note = if truncated {
                        "Your last response spent the whole output budget thinking and produced no text or tool call. \
                         Stop deliberating: call one tool now to make progress, or write the answer in pieces."
                    } else {
                        "Your last response was empty. Continue: either call a tool or give your final answer."
                    };
                    self.messages.push(Message::user(format!(
                        "<system-reminder>{note}</system-reminder>"
                    )));
                    continue;
                }
            } else {
                empty_retries = 0;
                self.output_cap_boost = 0;
                self.think_less = 0;
            }
            // Warn before recording the tool calls: a user message between the
            // tool_calls message and the tool results answering it makes
            // OpenAI-style providers reject the whole conversation.
            if !calls.is_empty() {
                self.check_disk();
            }
            self.messages.push(msg);
            self.stats.turns += 1;
            self.stats.tool_calls += calls.len() as u64;
            if calls.is_empty() && self.depth == 0 {
                // Reports from background agents that finished meanwhile; in
                // non-interactive runs, wait for the ones still running.
                // Monitor watches count too: their lines are what the run waits for.
                let waiting =
                    |env: &Env| background_running(env) + crate::tools::monitor::active(env);
                if self.env.notifications.lock().unwrap().is_empty()
                    && !self.env.interactive
                    && waiting(&self.env) > 0
                {
                    self.emit(Event::Busy(Some(format!(
                        "Waiting for {} background task(s)…",
                        waiting(&self.env)
                    ))));
                    let notified = self.env.bg_done.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if waiting(&self.env) > 0 && self.env.notifications.lock().unwrap().is_empty() {
                        tokio::select! {
                            _ = notified => {}
                            _ = cancel.cancelled() => return Ok(TurnEnd::Interrupted),
                        }
                    }
                    self.emit(Event::Busy(None));
                }
                let pending: Vec<String> =
                    self.env.notifications.lock().unwrap().drain(..).collect();
                if !pending.is_empty() {
                    for n in pending {
                        self.messages.push(Message::user(n));
                    }
                    continue;
                }
            }
            // A second opinion on the diff, once, before the checks run.
            if calls.is_empty()
                && verify
                && guard.oracle == Some(true)
                && !oracle_ran
                && self.turn_mutations > 0
            {
                oracle_ran = true;
                if let Some(findings) = self.oracle_review(input, &verify_root, &cancel).await {
                    self.messages.push(Message::user(format!(
                        "<system-reminder>Before verification, a second reviewer (the oracle) read this turn's diff and reported:\n\n{findings}\n\n\
                         Fix the problems that are real (check each against the code), say briefly why you disagree with any you don't fix, \
                         then give your final answer.</system-reminder>"
                    )));
                    continue;
                }
            }
            if calls.is_empty()
                && verify
                && (self.turn_mutations > 0 || (!gate_ran && !verify_checks.is_empty()))
            {
                let unchanged = self.turn_mutations == 0;
                gate_ran = true;
                let timeout = std::time::Duration::from_secs(guard.timeout_secs.unwrap_or(600));
                self.emit(Event::Busy(Some("Verifying…".into())));
                self.env.wait_snapshot().await;
                let changes = self.turn_changes(&verify_root);
                let changed = changes.as_ref().map(|c| c.files.clone());
                let plan = crate::guardrails::plan(
                    &verify_checks,
                    &verify_root,
                    changed.as_deref(),
                    scope,
                    fast_full,
                );
                let vouched: Vec<String> = plan
                    .runs
                    .iter()
                    .filter(|r| {
                        self.check_runs.iter().any(|(cmd, seq, ok, _)| {
                            *ok && *seq == self.edit_seq
                                && (crate::guardrails::covers(cmd, &r.command)
                                    || crate::guardrails::covers(cmd, &r.full))
                        })
                    })
                    .map(|r| r.command.clone())
                    .collect();
                // A check the agent ran itself this turn that failed: reuse its
                // output rather than repeating a suite that can take minutes.
                // Stale once anything was edited since, so those are dropped.
                let mut reuse: Vec<crate::guardrails::CheckResult> = Vec::new();
                for r in &plan.runs {
                    for (cmd, seq, ok, out) in &self.check_runs {
                        if !*ok
                            && *seq == self.edit_seq
                            && (crate::guardrails::covers(cmd, &r.command)
                                || crate::guardrails::covers(cmd, &r.full))
                        {
                            reuse.push(crate::guardrails::CheckResult {
                                command: cmd.clone(),
                                ok: false,
                                output: out.clone(),
                                secs: 0.0,
                            });
                        }
                    }
                }
                // The agent's own run of these tests hung, with no edits since:
                // running them again would most likely hang too, so don't spend
                // another time limit finding that out.
                let hung = plan.runs.iter().find_map(|r| {
                    self.check_timeouts
                        .iter()
                        .find(|(cmd, seq, secs)| {
                            *seq == self.edit_seq
                                && *secs >= timeout.as_secs().min(300)
                                && (crate::guardrails::covers(&r.command, cmd)
                                    || crate::guardrails::covers(cmd, &r.command))
                        })
                        .map(|(_, _, secs)| crate::guardrails::CheckResult {
                            command: r.command.clone(),
                            ok: false,
                            output: format!("timed out after {secs}s (your own run)"),
                            secs: *secs as f32,
                        })
                });
                let (res, ran) = match hung {
                    Some(r) => (
                        crate::guardrails::Gate::Failed(String::new(), Some(r.clone())),
                        vec![r],
                    ),
                    None => {
                        crate::guardrails::gate(
                            &plan,
                            &vouched,
                            &reuse,
                            test_snapshot.as_ref(),
                            &verify_root,
                            timeout,
                        )
                        .await
                    }
                };
                let slow = ran
                    .iter()
                    .find(|r| crate::guardrails::timed_out(r))
                    .cloned();
                // A failing run may have stopped early (`-x`): it only says the suite is slow.
                for r in ran.iter().filter(|r| {
                    verify_checks.contains(&r.command)
                        && (r.ok || r.secs > crate::guardrails::FAST_FULL_SECS)
                }) {
                    self.full_secs.insert(r.command.clone(), r.secs);
                }
                // Failures that were already there before the agent started.
                let split = match &res {
                    // Comparing against the original files would hang the same way.
                    _ if slow.is_some() => None,
                    crate::guardrails::Gate::Failed(_, Some(f)) => {
                        if let Some(h) = self.baseline_task.take() {
                            self.baseline = h.await.ok();
                        }
                        if !self
                            .baseline
                            .iter()
                            .flatten()
                            .any(|b| b.command == f.command)
                            && let Some(b) = self
                                .lazy_baseline(&plan, f, changes.as_ref(), &verify_root, timeout)
                                .await
                        {
                            self.baseline.get_or_insert_with(Vec::new).push(b);
                        }
                        self.baseline
                            .as_ref()
                            .and_then(|b| crate::guardrails::pre_existing(f, b))
                    }
                    _ => None,
                };
                self.emit(Event::Busy(None));
                match res {
                    // Tests that don't finish can't verify anything, and another
                    // round would cost the same time limit again: stop here.
                    crate::guardrails::Gate::Failed(..) if slow.is_some() => {
                        let why = slow
                            .as_ref()
                            .map(|r| {
                                format!(
                                    "`{}` didn't finish ({})",
                                    r.command,
                                    r.output.lines().next().unwrap_or("timed out")
                                )
                            })
                            .unwrap_or_default();
                        self.emit(Event::Warn(format!("Not verified: {why}")));
                        verdict = Some(format!(
                            "\n\n**⚠ Not verified**: {why}. Run the tests yourself, or set a faster test command {}.",
                            checks_hint()
                        ));
                        self.last_verify = Some((false, why));
                        self.turn_mutations = 0;
                    }
                    crate::guardrails::Gate::Passed(evidence) => {
                        self.emit(Event::Info(format!("✓ Verified: {evidence}")));
                        verdict = Some(format!("\n\n_Verified: {evidence}._"));
                        self.last_verify = Some((true, evidence.clone()));
                        self.turn_mutations = 0;
                    }
                    crate::guardrails::Gate::NoTests(why) => {
                        self.emit(Event::Warn(format!("Not verified: {why}")));
                        verdict = Some(format!(
                            "\n\n**⚠ Not verified**: {why}. Set the project's test command {}.",
                            checks_hint()
                        ));
                        self.last_verify = Some((false, why));
                        self.turn_mutations = 0;
                    }
                    // Only failures that predate this change remain, and the model
                    // has been told once: finish honestly instead of looping.
                    crate::guardrails::Gate::Failed(feedback, _)
                        if gate_attempts >= 1
                            && split.as_ref().is_some_and(|(new, _)| new.is_empty()) =>
                    {
                        let old = split.map(|(_, old)| old).unwrap_or_default();
                        self.emit(Event::Warn(
                            "Only failures that predate this change remain; stopping.".into(),
                        ));
                        verdict = Some(format!(
                            "\n\n**⚠ Not verified**: these were failing before this change and still fail: {}.\n\n{feedback}",
                            old.join(", ")
                        ));
                        self.last_verify = Some((
                            false,
                            format!("still failing from before this change: {}", old.join(", ")),
                        ));
                        self.turn_mutations = 0;
                    }
                    crate::guardrails::Gate::Failed(feedback, _)
                        if gate_attempts < guard.max_attempts.unwrap_or(3) =>
                    {
                        let feedback = match &split {
                            Some((new, old)) if !old.is_empty() => format!(
                                "{feedback}\n\nAlready failing before you started (fix them if they're part of the task, unless the user told you to leave that code alone): {}.\nNewly failing: {}.",
                                old.join(", "),
                                if new.is_empty() {
                                    "none".to_string()
                                } else {
                                    new.join(", ")
                                }
                            ),
                            _ => feedback,
                        };
                        gate_attempts += 1;
                        if self.env.config.learning.capture() {
                            crate::learn::record(
                                &self.env.root,
                                "gate_fail",
                                feedback.lines().next().unwrap_or(""),
                                &crate::util::truncate(&feedback, 600),
                            );
                        }
                        self.emit(Event::Warn(format!("Verification failed (attempt {gate_attempts}); sending the failures back to the model.")));
                        self.last_verify = Some((
                            false,
                            feedback
                                .lines()
                                .next()
                                .unwrap_or("checks failed")
                                .to_string(),
                        ));
                        let msg = if unchanged {
                            // If it answers again without edits, say so.
                            verdict = Some(format!(
                                "\n\n**⚠ Not verified**: no files were changed and the checks fail:\n\n{feedback}"
                            ));
                            format!(
                                "<system-reminder>You ended your turn without changing any files, and the project's checks fail.\n\n{feedback}\n\n\
                                 If the request needs code changes, you are not done: use your tools to make them and run the checks. \
                                 If it was only a question, give your final answer again.</system-reminder>"
                            )
                        } else {
                            format!(
                                "<system-reminder>You are not done: verification failed.\n\n{feedback}\n\nFix the problems, run the checks yourself, then give your final answer.</system-reminder>"
                            )
                        };
                        self.messages.push(Message::user(msg));
                        continue;
                    }
                    crate::guardrails::Gate::Failed(feedback, _) => {
                        self.emit(Event::Warn("Verification still failing; stopping.".into()));
                        verdict = Some(format!(
                            "\n\n**⚠ Not verified**: checks still fail after {gate_attempts} attempts:\n\n{feedback}"
                        ));
                        self.last_verify = Some((
                            false,
                            feedback
                                .lines()
                                .next()
                                .unwrap_or("checks failed")
                                .to_string(),
                        ));
                        self.turn_mutations = 0;
                    }
                }
            }
            if calls.is_empty() {
                // Outside `--verify` there is no gate, but a finish that stands
                // on a deleted or skipped test still isn't the work: send it
                // back a couple of times, then finish saying so. `--verify`
                // handles this in the gate above.
                let weakened = test_snapshot
                    .as_ref()
                    .map(crate::guardrails::Snapshot::violations)
                    .unwrap_or_default();
                if !verify && !weakened.is_empty() && test_bounces < 2 {
                    test_bounces += 1;
                    let list = weakened.join("; ");
                    self.emit(Event::Warn(format!("Existing tests were changed: {list}")));
                    self.messages.push(Message::user(format!(
                        "<system-reminder>You changed or removed existing tests: {list}. That doesn't count as progress: \
                         restore them (for example `git checkout -- <file>`) and fix the code they exercise, or add your \
                         own tests alongside them. If the new behaviour genuinely conflicts with an existing test, say so \
                         in your answer instead of editing the test.</system-reminder>"
                    )));
                    continue;
                }
                // A finish that stands on a check the agent watched fail is not
                // the work either. Its own run is already in hand, so this
                // costs no extra command; after a couple of nudges, finish but
                // say the check was still failing.
                if !verify
                    && check_bounces < 2
                    && let Some((cmd, out)) = last_failed_check(&self.check_runs, self.edit_seq)
                {
                    check_bounces += 1;
                    self.emit(Event::Warn(format!("Your last check failed: {cmd}")));
                    self.messages.push(Message::user(format!(
                        "<system-reminder>You are not done: the last check you ran, `{cmd}`, failed, and nothing has changed since.\n\n\
                         {}\n\nFix the failure. If these are existing tests, their expectations are the contract: prefer an \
                         implementation that leaves the default case unchanged (annotate only the new or non-default case) \
                         rather than assuming they are stale. If the failure is unrelated to the request or expected, say so \
                         explicitly and explain why before giving your final answer.</system-reminder>",
                        crate::util::truncate(out, 4_000)
                    )));
                    continue;
                }
                // A run that went green on a narrower scope never showed the
                // original failure was fixed. One nudge, then finish saying so.
                if !verify
                    && stale_bounces == 0
                    && last_failed_check(&self.check_runs, self.edit_seq).is_none()
                    && let Some((cmd, out)) = unverified_failure(&self.check_runs)
                {
                    stale_bounces += 1;
                    self.emit(Event::Warn(format!("`{cmd}` failed and was never re-run")));
                    self.messages.push(Message::user(format!(
                        "<system-reminder>You are not done: `{cmd}` failed, and nothing you have run since covers the same tests, so the failure is still unverified.\n\n\
                         {}\n\nRe-run `{cmd}` (or a run that includes those tests). If the failure is unrelated to the request or expected, say so explicitly and explain why before giving your final answer.</system-reminder>",
                        crate::util::truncate(out, 4_000)
                    )));
                    continue;
                }
                // Outside `--verify` nothing forces the tests covering the
                // change to run. A change exercised only by the agent's own new
                // test is how unrelated output gets broken, so ask once for the
                // related tests when the turn never ran them.
                if !verify
                    && related_nudges == 0
                    && self.turn_mutations > 0
                    && let Some(changes) = self.turn_changes(&verify_root)
                {
                    // `verify_checks` is only filled in verified mode; detect
                    // the project's own checks here so plain turns get the
                    // same "run the related tests" nudge.
                    let checks = if verify_checks.is_empty() {
                        crate::guardrails::detect_checks(&verify_root)
                    } else {
                        verify_checks.clone()
                    };
                    let plan = crate::guardrails::plan(
                        &checks,
                        &verify_root,
                        Some(&changes.files),
                        crate::guardrails::Scope::Related,
                        false,
                    );
                    // Only nudge when the selection is narrower than the suite:
                    // a fallback to the full suite is not worth imposing.
                    let pending: Vec<&str> = plan
                        .runs
                        .iter()
                        .filter(|r| r.command != r.full)
                        .map(|r| r.command.as_str())
                        .filter(|cmd| {
                            !self.check_runs.iter().any(|(run, seq, ok, _)| {
                                *ok && *seq == self.edit_seq
                                    && (crate::guardrails::covers(run, cmd)
                                        || !scoped_to_tests(run))
                            })
                        })
                        .collect();
                    if !pending.is_empty() {
                        related_nudges += 1;
                        let cmds = pending
                            .iter()
                            .take(2)
                            .map(|c| format!("`{c}`"))
                            .collect::<Vec<_>>()
                            .join(" or ");
                        self.emit(Event::Warn(format!(
                            "Tests covering the change were not run: {cmds}"
                        )));
                        self.messages.push(Message::user(format!(
                            "<system-reminder>You changed source files but have not run the existing tests that cover them. \
                             Run {cmds} before giving your final answer: running only your own new test doesn't show that \
                             existing behaviour still passes. If those tests genuinely can't run here, say why in your \
                             answer.</system-reminder>"
                        )));
                        continue;
                    }
                }
                // The todo list is the model's own plan, and it is shown to the
                // user: finishing while items are open (often because a
                // compaction froze the list mid-task) is a false finish. Send
                // it back to reconcile the list, bounded like the other gates.
                if self.depth == 0 && todo_bounces < 2 {
                    let open: Vec<String> = self
                        .env
                        .todos
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|t| t.status != "completed")
                        .map(|t| format!("- [{}] {}", t.status, t.content))
                        .collect();
                    if !open.is_empty() {
                        todo_bounces += 1;
                        let list = open.join("\n");
                        self.emit(Event::Warn(format!(
                            "Todo list still has {} unfinished item(s)",
                            open.len()
                        )));
                        self.messages.push(Message::user(format!(
                            "<system-reminder>You are not done: your todo list still has unfinished items:\n\n{list}\n\n\
                             Finish the remaining work and mark each item `completed` with TodoWrite, or remove the items \
                             you no longer intend to do. Only mark an item completed when it is actually done. If an item \
                             is blocked or the user's request changed, say so in your answer and remove it from the list. \
                             Update the list before giving your final answer.</system-reminder>"
                        )));
                        continue;
                    }
                }
                final_text = format!("{text}{}", verdict.take().unwrap_or_default());
                if !verify && !weakened.is_empty() {
                    final_text.push_str(&format!(
                        "\n\n**⚠ Existing tests were changed, so this may not be the real fix**: {}.",
                        weakened.join("; ")
                    ));
                }
                if !verify
                    && let Some((cmd, _)) = last_failed_check(&self.check_runs, self.edit_seq)
                {
                    final_text.push_str(&format!(
                        "\n\n**⚠ Not verified**: the last check you ran, `{cmd}`, was failing when you finished."
                    ));
                } else if !verify && let Some((cmd, _)) = unverified_failure(&self.check_runs) {
                    final_text.push_str(&format!(
                        "\n\n**⚠ Not verified**: `{cmd}` failed and was never re-run at a scope that covers the same tests."
                    ));
                }
                let open_todos: Vec<String> = self
                    .env
                    .todos
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|t| t.status != "completed")
                    .map(|t| t.content.clone())
                    .collect();
                if !open_todos.is_empty() {
                    final_text.push_str(&format!(
                        "\n\n**⚠ Todo list not finished**: {}. The work above may be incomplete.",
                        open_todos.join("; ")
                    ));
                }
                if resp.finish_reason.as_deref() == Some("length") {
                    self.emit(Event::Warn("Response hit the output token limit.".into()));
                }
                let event = if self.depth == 0 {
                    "Stop"
                } else {
                    "SubagentStop"
                };
                let r = crate::hooks::stop(&self.env, event, &final_text, stop_hook_active).await;
                if let (Some(reason), None) = (r.block, r.halt) {
                    // A Stop hook asked the agent to keep going.
                    stop_hook_active = true;
                    self.messages.push(Message::user(format!(
                        "<system-reminder>{event} hook feedback: {reason}</system-reminder>"
                    )));
                    continue;
                }
                if self.env.config.optimize.provenance() && !self.turn_evidence.is_empty() {
                    let unsupported = crate::provenance::unsupported(&text, &self.turn_evidence);
                    if !unsupported.is_empty() {
                        let claims = unsupported
                            .iter()
                            .take(5)
                            .map(|s| format!("`{s}`"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let more = if unsupported.len() > 5 {
                            format!(" and {} more", unsupported.len() - 5)
                        } else {
                            String::new()
                        };
                        self.emit(Event::Warn(format!("⚠ Provenance check: this turn's tool outputs do not support {claims}{more}.")));
                        if !provenance_bounced {
                            // One bounded bounce: send the claims back so the model can
                            // re-check them against the code before it finishes. If it
                            // still cannot support them, the warning above is the only
                            // record and the answer stands as written.
                            provenance_bounced = true;
                            self.messages.push(Message::user(format!(
                                "<system-reminder>Before you finish: these claims in your answer are not backed by anything your tools \
                                 returned this turn: {claims}{more}. Re-check each against the code or the tool output, fix or remove the \
                                 ones that are wrong, then give your final answer again.</system-reminder>"
                            )));
                            continue;
                        }
                    }
                }
                exhausted = false;
                break;
            }
            // Doom-loop guard: identical calls repeated 3 times in a row. A
            // call whose output changed in between (polling a background
            // shell, a command that increments something) is making progress
            // even though its arguments are the same, so the result counts too.
            let sig = calls
                .iter()
                .map(|c| format!("{}{}", c.name, c.arguments))
                .collect::<Vec<_>>()
                .join("|");
            let t_tools = Instant::now();
            let edits_before = self.edit_seq;
            let batch: Vec<ToolCall> = calls.iter().map(without_repair_marker).collect();
            let first_result = self.messages.len();
            // Note the batch before it touches anything, so a crash or a kill
            // mid-batch shows up on the next resume instead of diverging silently.
            let running: Vec<String> = calls
                .iter()
                .map(|c| format!("{}: {}", c.name, self.label_for(c)))
                .collect();
            let files: Vec<String> = self
                .env
                .checkpoints
                .lock()
                .unwrap()
                .last()
                .map(|cp| cp.files.keys().map(|p| p.display().to_string()).collect())
                .unwrap_or_default();
            if let Some(s) = &self.session {
                s.mark_in_flight(running, files);
            }
            let stop = self.execute_calls(calls, &cancel).await?;
            self.stats.tool_secs += t_tools.elapsed().as_secs_f64();
            self.save_session_soon();
            if stop {
                return Ok(TurnEnd::Interrupted);
            }
            // Editing an existing test is nearly always the code being fitted
            // to the test, so say so when it happens instead of waiting for the
            // finish, by which point the answer is already built around it.
            if !verify
                && test_edit_notices < 2
                && self.edit_seq != edits_before
                && let Some(snap) = test_snapshot.as_ref()
            {
                let changed: Vec<String> = snap
                    .violations()
                    .into_iter()
                    .filter(|v| !test_edits_seen.contains(v))
                    .collect();
                if !changed.is_empty() {
                    test_edit_notices += 1;
                    test_edits_seen.extend(changed.iter().cloned());
                    let list = changed.join("; ");
                    self.emit(Event::Warn(format!("Existing tests were changed: {list}")));
                    self.messages.push(Message::user(format!(
                        "<system-reminder>You changed an existing test: {list}. Existing tests are the contract: \
                         restore them (for example `git checkout -- <file>`) and fix the code they exercise, or add your \
                         own tests alongside them. If the requested behaviour genuinely conflicts with an existing test, \
                         keep the code change, leave the test as it was, and explain the conflict in your answer.\
                         </system-reminder>"
                    )));
                }
            }
            // Subagents leave the flag set so the parent turn stops too.
            let halt = if self.depth == 0 {
                self.env.halt.lock().unwrap().take()
            } else {
                self.env.halt.lock().unwrap().clone()
            };
            if let Some(reason) = halt {
                self.emit(Event::Warn(format!("Stopped by hook: {reason}")));
                return Ok(TurnEnd::Done(String::new()));
            }
            if crate::hooks::has(&self.env, "PostToolBatch") {
                let tool_calls: Vec<Value> = batch
                    .iter()
                    .map(|c| {
                        let result = self.messages[first_result..].iter().find(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some(c.id.as_str()));
                        json!({"tool_name": c.name, "tool_input": c.arguments, "tool_use_id": c.id, "tool_response": result.map(|m| m.content.clone()).unwrap_or_default()})
                    })
                    .collect();
                let r = crate::hooks::post_tool_batch(&self.env, tool_calls).await;
                if let Some(reason) = r.block.clone().or(r.halt.clone()) {
                    // It stays in the conversation, so the model sees it if the user continues.
                    self.messages.push(Message::user(format!("<system-reminder>PostToolBatch hook stopped the turn: {reason}</system-reminder>")));
                    self.emit(Event::Warn(format!(
                        "Stopped by PostToolBatch hook: {reason}"
                    )));
                    return Ok(TurnEnd::Done(String::new()));
                }
                if let Some(c) = r.context_text()
                    && let Some(m) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| m.role == Role::Tool)
                {
                    m.content
                        .push_str(&format!("\n\n<system-reminder>\n{c}\n</system-reminder>"));
                }
            }
            // Loop guard: identical calls in a row, the same failure again and
            // again, long failure runs, or edits undoing earlier edits. The
            // first time the model is told to change approach; the second
            // time the turn stops instead of burning the budget.
            let results: Vec<(String, bool, String)> = self.messages[first_result..]
                .iter()
                .filter(|m| m.role == Role::Tool)
                .map(|m| {
                    (
                        m.tool_call_id.clone().unwrap_or_default(),
                        m.content.starts_with("Error") || m.content.starts_with("Blocked"),
                        m.content.clone(),
                    )
                })
                .collect();
            let cwd = self.cwd_override.clone().unwrap_or_else(|| self.env.cwd());
            // Identical calls that also returned the same thing: with the
            // result in hand, this distinguishes a real loop from polling.
            let result_sig = results
                .iter()
                .map(|(_, _, content)| content.as_str())
                .collect::<Vec<_>>()
                .join("|");
            let repeats = match &mut recent_calls {
                Some((last, n)) if last.0 == sig && last.1 == result_sig => {
                    *n += 1;
                    *n
                }
                _ => {
                    recent_calls = Some(((sig.clone(), result_sig), 1));
                    1
                }
            };
            let looping = watch
                .observe(&batch, &results, &cwd)
                .or_else(|| (repeats >= 3).then(|| format!("the same tool call has been made {repeats} times in a row with identical arguments and output")));
            if let Some(reason) = looping {
                loop_strikes += 1;
                recent_calls = None;
                if loop_strikes >= 2 {
                    self.emit(Event::Warn(format!(
                        "Stopped: still going in circles ({reason})."
                    )));
                    return Ok(TurnEnd::Done(format!(
                        "**Stopped:** I kept going in circles ({reason}) even after changing approach, so I stopped rather than keep \
                         spending tokens. The problem probably needs a different plan or information I don't have."
                    )));
                }
                self.messages.push(Message::user(format!(
                    "<system-reminder>You're going in circles: {reason}. Repeating the same approach won't produce a different result. \
                     Step back: re-read the error, check your assumptions against the code or docs, and try a different approach, \
                     or explain what's blocking you.</system-reminder>"
                )));
                self.emit(Event::Warn(format!(
                    "Detected a loop ({reason}); asking the model to change approach."
                )));
            }
            if self.depth == 0 {
                for n in self.env.notifications.lock().unwrap().drain(..) {
                    self.messages.push(Message::user(n));
                }
                let pending: Vec<Queued> = self.env.inbox.lock().unwrap().drain(..).collect();
                for msg in pending {
                    let (text, images) = crate::commands::expand_mentions(self, &msg.text).await;
                    let mut m = Message::user(format!(
                        "<system-reminder>The user sent this message while you were working. Take it into account:</system-reminder>\n{text}"
                    ));
                    m.images = images;
                    self.messages.push(m);
                }
            } else {
                let pending: Vec<String> = self.followups.lock().unwrap().drain(..).collect();
                for msg in pending {
                    self.messages.push(Message::user(format!(
                        "<system-reminder>The user sent you this message while you were working. Take it into account:</system-reminder>\n{msg}"
                    )));
                }
            }
        }
        if exhausted {
            self.emit(Event::Warn(format!(
                "Stopped after {max_turns} model calls (max_turns)."
            )));
            if self.depth > 0 && self.sub.max_turns.is_some() {
                // `maxTurns`: hand back what it has, marked partial.
                let last = self
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == Role::Assistant && !m.content.trim().is_empty())
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                final_text = format!(
                    "{last}\n\n[Partial result: this subagent reached its maxTurns limit ({max_turns}) before finishing. Resume it to continue.]"
                );
            }
        }
        self.save_session();
        Ok(TurnEnd::Done(final_text))
    }

    /// The part of an interrupted response worth keeping in the transcript. Anything
    /// from a tool-call block onward is dropped: a half-written call can't be
    /// executed and would only confuse the next turn. Returns `None` when nothing
    /// useful was streamed.
    pub(super) fn interrupted_keep(partial: &str) -> Option<String> {
        let keep = partial[..crate::llm::tool_call_start(partial)].trim();
        (!keep.is_empty()).then(|| keep.to_string())
    }

    /// A side question about `messages`: one tool-less call to `model` whose
    /// question and answer stay out of the conversation. Needs only the
    /// environment, so it can run while a turn holds the agent.
    pub async fn side_question_on(
        env: &Env,
        model: &str,
        messages: &[Message],
        question: &str,
    ) -> Result<String> {
        let (provider, resolved) = env.resolve(model);
        let req = ChatRequest {
            model: resolved,
            messages: Self::side_question_messages(
                messages,
                &env.btw.lock().unwrap().clone(),
                question,
            ),
            tools: vec![],
            think: Some(false),
            effort: None,
            temperature: env.config.temperature,
            max_tokens: Some(2048),
            json: false,
        };
        provider.chat(&req, &mut |_| {}).await.map(|r| r.text)
    }

    /// The messages for a `/btw` side question: the conversation so far (redacted),
    /// then the question marked as a side question. `messages` is left untouched —
    /// this is the whole point of `/btw`, that asking changes nothing.
    pub(super) fn side_question_messages(
        messages: &[Message],
        earlier: &[(String, String)],
        question: &str,
    ) -> Vec<Message> {
        let mut msgs = messages.to_vec();
        // Earlier side questions make a thread: the newest 20.
        for (q, a) in &earlier[earlier.len().saturating_sub(20)..] {
            msgs.push(Message::user(format!("<system-reminder>An earlier side question (not part of the task).</system-reminder>\n\n{q}")));
            msgs.push(Message::new(Role::Assistant, a.clone()));
        }
        crate::secrets::redact_messages(&mut msgs);
        msgs.push(Message::user(format!(
        "<system-reminder>This is a side question, not part of the task. Answer it directly and briefly from the conversation so far. \
         You have no tools for it; if it needs one, say what you would run instead.</system-reminder>\n\n{question}"
    )));
        msgs
    }

    pub(super) async fn budget_ready(&self, cancel: &CancellationToken) -> bool {
        if let Err(e) = self.env.accounting.check() {
            let msg = e.to_string();
            self.emit(Event::Warn(msg.clone()));
            if self.env.interactive {
                let question = format!("{msg} Continue beyond the budget?");
                let options = ["Stop".into(), "Continue".into()];
                let reply = tokio::select! {
                    _ = cancel.cancelled() => None,
                    reply = self.env.frontend.choose(&question, &options) => reply,
                };
                if reply.as_deref() == Some("Continue") {
                    self.env.accounting.continue_budget();
                    return true;
                }
            }
            return false;
        }
        if let Some(msg) = self.env.accounting.warning() {
            self.emit(Event::Warn(msg));
        }
        true
    }
}
