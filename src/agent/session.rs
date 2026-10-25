//! Sessions: checkpoints and rewind, branches and forks, saving and restoring,
//! and refreshing the system prompt when the session changes underneath it.

use super::*;

impl Agent {
    /// Restore files and conversation to before the given checkpoint.
    /// Returns (files restored, prompt of that turn).
    pub fn rewind(&mut self, idx: usize) -> Result<(usize, String)> {
        self.rewind_mode(idx, "both")
    }

    pub fn rewind_undo(&mut self) -> Result<usize> {
        let mut saved = self.env.rewind_undo.lock().unwrap();
        let tree = saved
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No code restore to undo in this session"))?;
        let count = self.env.shadow()?.restore(tree)?;
        *saved = None;
        Ok(count)
    }

    pub fn rewind_mode(&mut self, idx: usize, mode: &str) -> Result<(usize, String)> {
        let mut cps = self.env.checkpoints.lock().unwrap();
        if idx >= cps.len() {
            anyhow::bail!("no such checkpoint");
        }
        if self.env.config.learning.capture() {
            crate::learn::record(
                &self.env.root,
                "rewind",
                &format!("The user rewound to before: {}", cps[idx].prompt),
                "",
            );
        }
        let tree = cps[idx].tree.clone();
        let mut before_tree = None;
        let shadow_count = if mode != "conversation" {
            match tree.as_ref() {
                Some(tree) => {
                    let (count, current) = self.env.shadow()?.restore_saved(tree)?;
                    *self.env.rewind_undo.lock().unwrap() = Some(current.clone());
                    before_tree = Some(current);
                    Some(count)
                }
                None => None,
            }
        } else {
            Some(0)
        };
        let removed: Vec<Checkpoint> = if mode == "code" {
            cps[idx..].to_vec()
        } else {
            cps.drain(idx..).collect()
        };
        drop(cps);
        let mut restored = std::collections::HashSet::new();
        let mut before_files = Vec::new();
        for cp in removed.iter().rev() {
            for (path, orig) in &cp.files {
                // `/rewind conversation` rolls back only the chat; the working
                // tree is left as it is, including files edited outside the
                // project after a `/cd` (which the in-project skip below never
                // covered, so they used to be reverted silently).
                if mode == "conversation" {
                    continue;
                }
                // Shadow snapshots restore the project's disk; edits made in an
                // editor's buffers, or outside the project after `/cd`, are only
                // in these per-file snapshots.
                let normalized = permissions::normalize(path);
                let project = permissions::normalize(&self.env.root);
                let in_project = normalized.starts_with(&project);
                if in_project && shadow_count.is_some() && !self.env.frontend.writes_via_client() {
                    continue;
                }
                let root = if in_project {
                    project
                } else {
                    permissions::normalize(&self.env.cwd())
                };
                let Ok(relative) = normalized.strip_prefix(&root) else {
                    continue;
                };
                if relative.components().any(|c| {
                    matches!(
                        c.as_os_str().to_str(),
                        Some(".git" | "node_modules" | "target" | ".venv")
                    )
                }) {
                    continue;
                }
                let mut ignores = ignore::gitignore::GitignoreBuilder::new(&root);
                let mut dirs: Vec<_> = normalized
                    .parent()
                    .into_iter()
                    .flat_map(|p| p.ancestors())
                    .take_while(|p| p.starts_with(&root))
                    .collect();
                dirs.reverse();
                for dir in dirs {
                    ignores.add(dir.join(".gitignore"));
                }
                if ignores.build().is_ok_and(|g| {
                    g.matched_path_or_any_parents(&normalized, false)
                        .is_ignore()
                }) {
                    continue;
                }
                if orig
                    .as_ref()
                    .is_some_and(|b| b.len() as u64 > self.env.config.history.max_file_bytes)
                {
                    continue;
                }
                if !restored.contains(path) {
                    before_files.push((path.clone(), std::fs::read(path).ok()));
                }
                match orig {
                    Some(bytes) => {
                        if let Some(parent) = path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        std::fs::write(path, bytes)?;
                        if let Ok(text) = std::str::from_utf8(bytes) {
                            self.env.frontend.file_restored(path, text);
                        }
                    }
                    None => {
                        let _ = std::fs::remove_file(path);
                    }
                }
                self.env.mark_read(path);
                restored.insert(path.clone());
            }
        }
        let first = &removed[0];
        let messages = if mode != "code" {
            self.messages
                .split_off(first.msg_index.max(1).min(self.messages.len()))
        } else {
            vec![]
        };
        self.env.redo.lock().unwrap().push(Redo {
            checkpoints: if mode != "code" {
                removed.clone()
            } else {
                vec![]
            },
            messages,
            msg_len: self.messages.len(),
            tree: before_tree,
            files: before_files,
            prompt: first.prompt.clone(),
        });
        self.reads.clear();
        self.last_prompt_tokens = 0;
        self.msgs_at_last_usage = 0;
        Ok((
            shadow_count.map_or(restored.len(), |n| n.max(restored.len())),
            first.prompt.clone(),
        ))
    }

    /// Put back the most recent /undo or /rewind: its files and the turns it
    /// removed. Returns (files restored, prompt of the first turn brought back).
    pub fn redo(&mut self) -> Result<(usize, String)> {
        let mut stack = self.env.redo.lock().unwrap();
        let r = stack.last().ok_or_else(|| {
            anyhow::anyhow!("Nothing to redo (a new prompt clears what /undo took back)")
        })?;
        if self.messages.len() != r.msg_len {
            anyhow::bail!("The conversation changed since the undo, so it can't be redone");
        }
        let r = stack.pop().expect("checked above");
        drop(stack);
        let mut count = 0;
        if let Some(tree) = &r.tree {
            count = self.env.shadow()?.restore(tree)?;
            *self.env.rewind_undo.lock().unwrap() = None;
        }
        for (path, bytes) in &r.files {
            match bytes {
                Some(b) => {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    std::fs::write(path, b)?;
                    if let Ok(text) = std::str::from_utf8(b) {
                        self.env.frontend.file_restored(path, text);
                    }
                }
                None => {
                    let _ = std::fs::remove_file(path);
                }
            }
            self.env.mark_read(path);
        }
        self.messages.extend(r.messages);
        self.env.checkpoints.lock().unwrap().extend(r.checkpoints);
        self.reads.clear();
        self.last_prompt_tokens = 0;
        self.msgs_at_last_usage = 0;
        Ok((count.max(r.files.len()), r.prompt))
    }

    /// Redirect absolute paths into `repo` to the worktree `wt` (a
    /// conversation that continues in a worktree).
    pub fn map_worktree(&mut self, repo: PathBuf, wt: PathBuf) {
        self.worktree_map = Some((repo, wt));
    }

    /// Rebuild the system message (after an output style or similar change).
    pub fn refresh_system_prompt(&mut self) {
        self.refresh_system();
    }

    /// Switch to an independent session without restoring working files.
    /// Editing an earlier prompt (`/edit-prompt`): a new branch of the
    /// session from just before checkpoint `idx`, as `/fork` makes one, so the
    /// original conversation stays intact under its own id. With `code`, the
    /// project's files go back to that point too, as `/rewind` restores them
    /// (and `/rewind undo` in the branch brings them back). Returns the new
    /// session's id, the original prompt, and how many files were restored.
    pub fn branch_at(&mut self, idx: usize, code: bool) -> Result<(String, String, usize)> {
        let prompt = self
            .env
            .checkpoints
            .lock()
            .unwrap()
            .get(idx)
            .map(|c| c.prompt.clone())
            .ok_or_else(|| anyhow::anyhow!("no such checkpoint"))?;
        if self.session.is_none() {
            anyhow::bail!("this session isn't saved, so it can't branch");
        }
        let restored = if code {
            self.rewind_mode(idx, "code")?.0
        } else {
            0
        };
        let undo = self.env.rewind_undo.lock().unwrap().clone();
        let id = self.fork_session(None, Some(idx))?;
        // The files' state before the restore belongs to this branch's history too.
        if code && undo.is_some() {
            *self.env.rewind_undo.lock().unwrap() = undo;
            self.save_session();
        }
        Ok((id, prompt, restored))
    }

    pub fn fork_session(&mut self, name: Option<&str>, point: Option<usize>) -> Result<String> {
        self.save_session();
        let parent = self
            .session
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no current session"))?;
        let (child, messages) = parent.fork(&self.messages, name, point)?;
        let id = child.id.clone();
        self.messages = messages;
        self.reads.clear();
        self.stats = child.stats.clone();
        self.session = Some(child);
        self.env.todos.lock().unwrap().clear();
        self.restore_todos();
        self.save_session();
        Ok(id)
    }

    /// Save now, superseding any background save still in flight.
    pub fn save_session(&mut self) {
        if self.depth > 0 {
            return;
        }
        let Some((meta, msgs)) = self.session_snapshot() else {
            return;
        };
        let generation = self.save_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let mut written = self.save_written.lock().unwrap();
        if let Err(e) = meta.write(msgs) {
            self.env
                .emit(Event::Warn(format!("Could not save session: {e}")));
        } else {
            // The transcript on disk now matches the files, so no batch is
            // outstanding any more.
            meta.clear_in_flight();
        }
        let _ = crate::history::maintain(&self.env.root, &self.env.config.history);
        *written = generation;
        self.last_save = Some(Instant::now());
    }

    /// Save between tool batches: at most every few seconds, written off the
    /// async worker. The turn always ends with a `save_session`.
    pub(super) fn save_session_soon(&mut self) {
        if self.depth > 0
            || self
                .last_save
                .is_some_and(|t| t.elapsed() < Duration::from_secs(5))
        {
            return;
        }
        let Some((meta, msgs)) = self.session_snapshot() else {
            return;
        };
        let generation = self.save_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let written = self.save_written.clone();
        self.last_save = Some(Instant::now());
        let env = self.env.clone();
        tokio::task::spawn_blocking(move || {
            let mut w = written.lock().unwrap();
            // A newer save already landed: this snapshot is stale.
            if *w > generation {
                return;
            }
            if let Err(e) = meta.write(msgs) {
                env.emit(Event::Warn(format!("Could not save session: {e}")));
            }
            *w = generation;
        });
    }

    /// The session's metadata and one redacted copy of the history, or None
    /// when there's no session or nothing to save yet.
    fn session_snapshot(&mut self) -> Option<(crate::session::Session, Vec<Message>)> {
        let s = self.session.as_mut()?;
        self.env.accounting.set_session(&s.id);
        *self.env.session_info.lock().unwrap() = Some((s.id.clone(), s.path()));
        s.model = self.env.model();
        self.stats.total_cost_usd = self.env.accounting.total();
        self.stats.price_unknown = self.env.accounting.unknown();
        s.stats = self.stats.clone();
        s.settings = serde_json::json!({
            "mode": self.env.mode(), "think": self.env.think(),
            "tool_policy": self.env.session_tool_policy.read().unwrap().clone(),
            "output_style": self.env.output_style.read().unwrap().clone(),
        });
        // The external agent's session, so resuming this Chanto session
        // restores the agent's side of the conversation too.
        if let Some(id) = self.acp.as_ref().and_then(|acp| acp.session_id()) {
            s.settings["acp_session"] = serde_json::json!(id);
        }
        s.checkpoints = self.env.checkpoints.lock().unwrap().clone();
        s.rewind_undo = self.env.rewind_undo.lock().unwrap().clone();
        if !s.touch(&self.messages) {
            // A brand-new session with no user message isn't worth a file. But a
            // transcript that already exists must be overwritten: /rewind can
            // clear the conversation, and leaving the old file behind would
            // resurrect the discarded history on resume.
            if !s.path().exists() {
                return None;
            }
        }
        let mut msgs = self.messages.clone();
        crate::secrets::redact_messages(&mut msgs);
        Some((s.clone(), msgs))
    }

    /// Restore the todo list from the last TodoWrite call (after resuming).
    pub fn restore_todos(&mut self) {
        let mut acp_session = None;
        if let Some(session) = &self.session {
            // Bypass is never restored from a session file: it has to be
            // chosen again (`--dangerously-skip-permissions`, shift+tab) in
            // the session that uses it. A managed policy mode outranks the
            // saved one, as it does at startup.
            if let Some(mode) = self.env.config.policy_mode.or_else(|| {
                session.settings["mode"]
                    .as_str()
                    .and_then(Mode::parse)
                    .filter(|m| *m != Mode::Bypass)
            }) {
                self.env.set_mode(mode);
            }
            if let Some(think) = session.settings["think"].as_bool() {
                self.env.set_think(think);
            }
            if let Ok(policy) = serde_json::from_value(session.settings["tool_policy"].clone()) {
                *self.env.session_tool_policy.write().unwrap() = policy;
            }
            if let Some(style) = session.settings.get("output_style")
                && let Ok(style) = serde_json::from_value(style.clone())
            {
                *self.env.output_style.write().unwrap() = style;
            }
            acp_session = session.settings["acp_session"].as_str().map(str::to_string);
            *self.env.checkpoints.lock().unwrap() = session.checkpoints.clone();
            *self.env.rewind_undo.lock().unwrap() = session.rewind_undo.clone();
        }
        // A resume continues the external agent's conversation, not a new one.
        if let Some(acp) = self.acp.as_mut() {
            acp.adopt(acp_session.as_deref());
        }
        let last = self
            .messages
            .iter()
            .rev()
            .flat_map(|m| m.tool_calls.iter().rev())
            .find(|c| c.name == "TodoWrite");
        if let Some(c) = last
            && let Ok(t) = serde_json::from_value::<Vec<Todo>>(c.arguments["todos"].clone())
        {
            *self.env.todos.lock().unwrap() = t.clone();
            self.env.emit(Event::Todos(t));
        }
    }

    /// Re-render the system prompt in place (after /model, cwd changes...).
    pub fn refresh_system(&mut self) {
        let sys = self.system_prompt();
        match self.messages.first_mut() {
            Some(m) if m.role == Role::System => m.content = sys,
            _ => self.messages.insert(0, Message::system(sys)),
        }
        self.system_for_model = Some(self.env.model());
        self.system_quiet = self.env.quiet();
    }
}
