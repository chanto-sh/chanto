//! Context compaction: measuring the window, clearing old tool results, and
//! summarizing the conversation into a handoff the next turn can continue from.

use super::*;

impl Agent {
    /// The model the next request goes to: this turn's fallback, or the session's.
    pub(super) fn request_model(&self) -> String {
        let fallbacks = self.fallback_chain();
        self.turn_fallback
            .and_then(|i| fallbacks.get(i).cloned())
            .unwrap_or_else(|| self.model())
    }

    /// A model's context window in tokens (0: unknown).
    pub fn window_of(&self, spec: &str) -> u64 {
        self.env.resolve(spec).0.num_ctx as u64
    }

    /// When the conversation is too big for `spec`'s context window (it has
    /// reached the compaction threshold there): (tokens used, its window).
    pub fn outgrows(&self, spec: &str) -> Option<(u64, u64)> {
        let window = self.window_of(spec);
        let used = self.context_used();
        let threshold = self.env.config.compact_threshold.unwrap_or(0.85) as f64;
        (window > 0 && self.messages.len() >= 3 && used as f64 >= window as f64 * threshold)
            .then_some((used, window))
    }

    /// Idle compaction (`idle_compaction`): whether a session idle this long
    /// should compact now. It needs `idle_compact_minutes` idle (default 54,
    /// before an hour-long prompt cache expires) and a conversation filling
    /// `idle_compact_percent` of the window (default half), or at least
    /// `CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS` tokens when that is set.
    /// `/autocompact off` turns it off too.
    pub fn idle_compact_due(&self, idle: std::time::Duration) -> bool {
        let cfg = &self.env.config;
        if cfg.idle_compaction != Some(true)
            || self.env.autocompact.load(Ordering::Relaxed) == u64::MAX
            || self.messages.len() < 3
        {
            return false;
        }
        let max = self.env.context_max();
        let floor = std::env::var("CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(
                (max as f64 * cfg.idle_compact_percent.unwrap_or(0.5).clamp(0.0, 1.0) as f64)
                    as u64,
            );
        idle >= std::time::Duration::from_secs(
            cfg.idle_compact_minutes.unwrap_or(54).saturating_mul(60),
        ) && max > 0
            && self.context_used() >= floor.max(1)
    }

    pub(super) async fn maybe_compact(&mut self, cancel: &CancellationToken) -> Result<()> {
        // Switched (`/model`, a fallback) to a model whose window can't hold the
        // conversation: the model that last served it summarizes it first, as
        // it can still read all of it.
        let target = self.request_model();
        if let Some((used, window)) = self.outgrows(&target)
            && let Some(prev) = self
                .last_model
                .clone()
                .filter(|p| *p != target && self.window_of(p) > window)
        {
            self.emit(Event::Info(format!("❖ The conversation (~{used} tokens) is too long for {target}'s {window}-token context; {prev} is summarizing it first")));
            match self.compact_with(None, "auto", cancel, Some(&prev)).await {
                Ok(()) if self.outgrows(&target).is_none() || cancel.is_cancelled() => {
                    return Ok(());
                }
                Ok(()) => {}
                Err(e) => self.emit(Event::Warn(format!(
                    "{prev} couldn't summarize the conversation ({e:#}); {target} will."
                ))),
            }
        }
        let max = self.window_of(&target);
        let mut threshold = self.env.config.compact_threshold.unwrap_or(0.85) as f64;
        // `/autocompact`: off, or a token count for this session.
        match self.env.autocompact.load(Ordering::Relaxed) {
            0 => {}
            u64::MAX => return Ok(()),
            tokens if max > 0 => threshold = (tokens as f64 / max as f64).min(threshold.max(0.95)),
            _ => {}
        }
        if (self.context_used() as f64) < max as f64 * threshold || self.messages.len() < 4 {
            return Ok(());
        }
        // Clearing old tool results costs no model call, but it rewrites history
        // and so invalidates the prompt-prefix cache. That is only worth it when
        // it actually gets us back under the trigger — otherwise compaction
        // (which rewrites history anyway) would break the cache a second time for
        // nothing. Decide from an estimate before mutating.
        let reclaim = self.clearable_tool_tokens(6);
        if reclaim > 0
            && (self.context_used().saturating_sub(reclaim) as f64)
                < max as f64 * (threshold - 0.15)
            && self.clear_old_tool_results(6)
        {
            self.emit(Event::Info(
                "❖ Cleared old tool results to free context".into(),
            ));
            self.emit_usage(0.0);
            return Ok(());
        }
        self.compact(None, "auto", cancel).await
    }

    /// Tokens `clear_old_tool_results(keep)` would free, without mutating history.
    pub(super) fn clearable_tool_tokens(&self, keep: usize) -> u64 {
        let tool_idx: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::Tool)
            .map(|(i, _)| i)
            .collect();
        if tool_idx.len() <= keep {
            return 0;
        }
        tool_idx[..tool_idx.len() - keep]
            .iter()
            .filter(|&&i| {
                let m = &self.messages[i];
                m.content.len() > 300 && !m.content.starts_with("[old tool result cleared")
            })
            .map(|&i| crate::llm::estimate_tokens(std::slice::from_ref(&self.messages[i])))
            .sum()
    }

    /// Replace the content of all but the last `keep` tool results with a stub.
    /// Returns true if anything was cleared.
    pub(super) fn clear_old_tool_results(&mut self, keep: usize) -> bool {
        let tool_idx: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::Tool)
            .map(|(i, _)| i)
            .collect();
        if tool_idx.len() <= keep {
            return false;
        }
        let mut cleared = false;
        for &i in &tool_idx[..tool_idx.len() - keep] {
            let m = &mut self.messages[i];
            if m.content.len() > 300 && !m.content.starts_with("[old tool result cleared") {
                let first = m
                    .content
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(120)
                    .collect::<String>();
                m.set_content(format!(
                    "[old tool result cleared to save context; first line was: {first}]"
                ));
                cleared = true;
                self.reads.clear();
            }
        }
        // Old images too.
        let last_user = self
            .messages
            .iter()
            .rposition(|m| m.role == Role::User)
            .unwrap_or(0);
        for m in self.messages[..last_user].iter_mut() {
            if !m.images.is_empty() {
                m.images.clear();
                std::sync::Arc::make_mut(&mut m.content)
                    .push_str(" [image removed to save context]");
                cleared = true;
            }
        }
        if cleared {
            self.stats.expect_break("old tool results cleared");
            // Usage numbers are stale now; fall back to estimation.
            self.last_prompt_tokens = 0;
            self.msgs_at_last_usage = 0;
        }
        cleared
    }

    /// Summarize the conversation and replace it with the summary.
    pub async fn compact(
        &mut self,
        instructions: Option<&str>,
        trigger: &str,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.compact_with(instructions, trigger, cancel, None).await
    }

    /// [`Self::compact`] with the summary written by `model` (default: the
    /// model the next request goes to).
    pub(super) async fn compact_with(
        &mut self,
        instructions: Option<&str>,
        trigger: &str,
        cancel: &CancellationToken,
        model: Option<&str>,
    ) -> Result<()> {
        let model = model
            .map(str::to_string)
            .unwrap_or_else(|| self.request_model());
        let before = self.context_used();
        let fields = serde_json::json!({"trigger": trigger, "custom_instructions": instructions});
        if let Some(why) = crate::hooks::notify(&self.env, "PreCompact", Some(trigger), fields)
            .await
            .block
        {
            // Blocked: the conversation carries on uncompacted.
            self.emit(Event::Warn(format!(
                "Compaction blocked by PreCompact hook{}",
                if why.is_empty() {
                    String::new()
                } else {
                    format!(": {why}")
                }
            )));
            return Ok(());
        }
        self.emit(Event::Busy(Some("Compacting conversation…".into())));
        // Shrink bulky tool outputs so the summarization request itself fits.
        let budget_chars = (self.window_of(&model).max(1024) as usize).saturating_mul(3) * 7 / 10;
        // A prompt the user just sent (compaction before the turn's first call)
        // goes after the summary as written, not only through the summary: a
        // long request or paste used to reach the model as a few summary lines.
        let pending = match self.messages.last() {
            Some(m) if m.role == Role::User && self.messages.len() > 2 => {
                let mut m = m.clone();
                if m.content.len() > budget_chars / 2 {
                    m.set_content(crate::util::truncate_middle(&m.content, budget_chars / 2));
                }
                Some(m)
            }
            _ => None,
        };
        let mut msgs: Vec<Message> =
            self.messages[..self.messages.len() - pending.is_some() as usize].to_vec();
        let mut per_tool = 4000;
        loop {
            let total: usize = msgs
                .iter()
                .map(|m| {
                    m.content.len()
                        + m.tool_calls
                            .iter()
                            .map(|c| c.arguments.to_string().len())
                            .sum::<usize>()
                })
                .sum();
            if total <= budget_chars || per_tool < 200 {
                break;
            }
            for m in msgs.iter_mut().skip(1) {
                m.images.clear();
                for c in m.tool_calls.iter_mut() {
                    let s = c.arguments.to_string();
                    if s.len() > per_tool {
                        c.arguments = serde_json::json!({ "_truncated": crate::util::truncate(&s, per_tool) });
                    }
                }
                if (m.role == Role::Tool || m.content.len() > per_tool * 2)
                    && m.content.len() > per_tool
                {
                    m.set_content(crate::util::truncate_middle(&m.content, per_tool));
                }
            }
            per_tool /= 2;
        }
        let mut prompt = String::from(COMPACT_PROMPT);
        if let Some(i) = instructions {
            prompt.push_str(&format!("\n\nAdditional instructions for the summary: {i}"));
        }
        msgs.push(Message::user(prompt));
        let req = ChatRequest {
            model,
            messages: msgs,
            tools: vec![],
            think: Some(false),
            effort: None,
            temperature: Some(0.2),
            max_tokens: Some(4096),
            json: false,
        };
        let (provider, model_name) = self.env.resolve(&req.model);
        let req = ChatRequest {
            model: model_name,
            ..req
        };
        let mut sink = |_: Delta| {};
        let r = tokio::select! {
            r = provider.chat(&req, &mut sink) => r?,
            _ = cancel.cancelled() => { self.emit(Event::Busy(None)); return Ok(()); }
        };
        self.emit(Event::Busy(None));
        let summary = r.text.trim().to_string();
        let todos = self.env.todos.lock().unwrap().clone();
        let mut cont = format!(
            "This session is being continued from a previous conversation that ran out of context. Summary of the conversation so far:\n\n{summary}"
        );
        if !todos.is_empty() {
            cont.push_str(&format!(
                "\n\nCurrent todo list:\n{}",
                serde_json::to_string_pretty(&todos).unwrap_or_default()
            ));
        }
        if let Some(goal) = self.goal.as_deref().and_then(crate::goal::reminder) {
            cont.push_str(&format!("\n\n{goal}"));
        }
        self.messages = vec![Message::system(self.system_prompt())];
        match pending {
            Some(mut p) => {
                // Fold the continuation note into the user's pending message
                // rather than adding a second user message: two consecutive
                // user messages break the strict chat templates some local
                // servers enforce, and the rest of the loop keeps roles
                // alternating for exactly that reason. The pending message's
                // images (and any other fields) are preserved.
                cont.push_str("\n\nThe user's latest message follows.\n\n");
                cont.push_str(&p.content);
                p.set_content(cont);
                self.messages.push(p);
            }
            None => {
                cont.push_str("\n\nContinue from where you left off without asking the user further questions; pick up the last task you were working on.");
                self.messages.push(Message::user(cont));
            }
        }
        self.reads.clear();
        self.stats.compactions += 1;
        self.stats.expect_break("compaction");
        // Earlier checkpoints can still restore files, but the conversation can
        // only go back as far as the summary.
        for cp in self.env.checkpoints.lock().unwrap().iter_mut() {
            cp.msg_index = cp.msg_index.min(self.messages.len());
        }
        self.last_prompt_tokens = 0;
        self.msgs_at_last_usage = 0;
        let after = self.context_used();
        self.emit(Event::Compacted { before, after });
        self.emit_usage(0.0);
        crate::hooks::post_compact(&self.env, trigger, &summary).await;
        if self.depth == 0 {
            self.env.session_start("compact").await;
        }
        Ok(())
    }
}
