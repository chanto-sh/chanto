//! Verified turns: guardrail checks after edits, the oracle's second opinion,
//! and waiting out provider rate limits.

use super::*;

impl Agent {
    /// Run a turn in verified mode with extra checks, locking `locked` dirs.
    pub async fn run_turn_verified(
        &mut self,
        input: &str,
        checks: Vec<String>,
        locked: Vec<PathBuf>,
        cancel: CancellationToken,
    ) -> Result<TurnEnd> {
        self.forced_verify = Some((checks, locked));
        self.run_turn(input, vec![], cancel).await
    }

    /// Files changed during this turn, with their contents when it started.
    /// None when that can't be worked out.
    pub(super) fn turn_changes(&self, root: &Path) -> Option<TurnChanges> {
        let cp = self.env.checkpoints.lock().unwrap().last().cloned()?;
        let mut originals: HashMap<PathBuf, Option<Vec<u8>>> = HashMap::new();
        for (p, orig) in cp.files {
            if std::fs::read(&p).ok() != orig {
                originals.insert(p, orig);
            }
        }
        let mut unknown = Vec::new();
        if self.turn_untracked_writes {
            // Shell commands can change anything: compare with the snapshot
            // taken when the turn started, or else look at modification times.
            let shadow = cp
                .tree
                .as_ref()
                .filter(|_| self.cwd_override.is_none())
                .and_then(|t| self.env.shadow().ok()?.changed_since(t).ok());
            match shadow {
                Some(list) => {
                    for (rel, orig) in list {
                        originals
                            .entry(permissions::normalize(&self.env.root.join(rel)))
                            .or_insert(orig);
                    }
                }
                None => {
                    for p in crate::guardrails::modified_since(root, self.turn_started?)? {
                        let p = permissions::normalize(&p);
                        if !originals.contains_key(&p) {
                            unknown.push(p);
                        }
                    }
                }
            }
        }
        let mut files: Vec<crate::guardrails::Changed> = originals
            .iter()
            .map(|(p, o)| crate::guardrails::Changed {
                path: p.clone(),
                new: o.is_none(),
            })
            .collect();
        files.extend(unknown.iter().map(|p| crate::guardrails::Changed {
            path: p.clone(),
            new: false,
        }));
        Some(TurnChanges {
            files,
            originals,
            complete: unknown.is_empty(),
        })
    }

    /// The result `f`'s command had before this turn's changes: run with the
    /// changed files temporarily put back as they were. None when that can't
    /// be done safely, so every failure counts as new.
    pub(super) async fn lazy_baseline(
        &self,
        plan: &crate::guardrails::Plan,
        f: &crate::guardrails::CheckResult,
        changes: Option<&TurnChanges>,
        root: &Path,
        timeout: Duration,
    ) -> Option<crate::guardrails::CheckResult> {
        let changes = changes.filter(|c| c.complete)?;
        if changes.originals.is_empty() {
            // Nothing changed this turn: the failures were all there before.
            return Some(f.clone());
        }
        // Background agents may be writing to these files.
        if background_running(&self.env) > 0 {
            return None;
        }
        let run = plan
            .runs
            .iter()
            .find(|r| r.command == f.command || r.full == f.command)?;
        let cmd = if run.command == f.command {
            run.without_new_files(&changes.originals, root)?
        } else {
            f.command.clone()
        };
        self.emit(Event::Busy(Some(
            "Checking which failures predate your changes…".into(),
        )));
        let mut b = crate::guardrails::run_on_originals(
            &cmd,
            root,
            timeout,
            &changes.originals,
            &crate::guardrails::put_back_journal(&self.env.history_dir, root),
        )
        .await;
        self.emit(Event::Busy(Some("Verifying…".into())));
        if let Some(b) = b.as_mut() {
            b.command = f.command.clone();
        }
        b
    }

    /// When the usage limit behind this error resets (Unix seconds): from the
    /// error, else from the account's last reported limits.
    pub(super) fn limit_reset(&self, e: &anyhow::Error) -> Option<i64> {
        let q = e
            .chain()
            .find_map(|c| c.downcast_ref::<llm::limits::QuotaExhausted>())?;
        if let Some(at) = q.resets_at() {
            return Some(at);
        }
        if !q.to_string().contains("usage limit reached") {
            return None;
        }
        let snap = self.env.rate_limits.lock().unwrap().clone()?;
        [snap.primary, snap.secondary]
            .into_iter()
            .flatten()
            .filter(|w| w.used_percent >= 100.0)
            .filter_map(|w| w.resets_at)
            .max()
    }

    /// Wait until `at` and resume, if `resume_after_limit` says so (or, when
    /// it's unset, the user does), with a countdown and a notification.
    pub(super) async fn wait_for_limit(
        &self,
        at: i64,
        msg: &str,
        cancel: &CancellationToken,
    ) -> LimitWait {
        let now = chrono::Utc::now().timestamp();
        // A reset more than a week and a day away isn't worth waiting for here.
        if at - now > 8 * 86_400 {
            return LimitWait::No;
        }
        let when = reset_clock(at, now);
        let wait = match self.env.config.resume_after_limit {
            Some(b) => b,
            None if self.env.interactive && self.depth == 0 => {
                self.emit(Event::Busy(None));
                let question = format!(
                    "{}\n\nWait and continue at {when}?",
                    crate::util::one_line(llm::root_cause(msg), 200)
                );
                let options = ["Wait and continue".to_string(), "Stop".to_string()];
                let reply = tokio::select! {
                    _ = cancel.cancelled() => return LimitWait::Cancelled,
                    r = self.env.frontend.choose(&question, &options) => r,
                };
                reply.as_deref() == Some("Wait and continue")
            }
            None => false,
        };
        if !wait {
            return LimitWait::No;
        }
        self.emit(Event::Warn(format!(
            "Usage limit reached; this turn continues at {when} when it resets (esc stops waiting)."
        )));
        self.emit(Event::Status {
            key: "limit".into(),
            lines: vec![format!("⏸ usage limit · resuming at {when}")],
        });
        loop {
            let left = at - chrono::Utc::now().timestamp();
            if left < 0 {
                break;
            }
            self.emit(Event::Busy(Some(format!(
                "Usage limit: resuming at {when} (in {})",
                crate::util::duration(left.max(0) as u64)
            ))));
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(1000)) => {}
                _ = cancel.cancelled() => {
                    self.emit(Event::Status { key: "limit".into(), lines: vec![] });
                    return LimitWait::Cancelled;
                }
            }
        }
        self.emit(Event::Status {
            key: "limit".into(),
            lines: vec![],
        });
        self.emit(Event::Busy(Some("Thinking…".into())));
        let note = "Usage limit reset; continuing the turn.";
        self.emit(Event::Info(note.into()));
        crate::hooks::notify(
            &self.env,
            "Notification",
            Some("limit_resumed"),
            json!({"message": note, "notification_type": "limit_resumed"}),
        )
        .await;
        if self.env.interactive {
            crate::tui::notify::send(
                crate::tui::notify::channel(self.env.config.notifications.as_deref()),
                "chanto",
                note,
            );
        }
        LimitWait::Resumed
    }

    /// Verified mode with `guardrails.oracle`: the oracle subagent reviews
    /// this turn's diff. Its findings, or None when it has none (or the
    /// diff can't be worked out, or the review fails).
    pub(super) async fn oracle_review(
        &mut self,
        request: &str,
        root: &Path,
        cancel: &CancellationToken,
    ) -> Option<String> {
        self.env.wait_snapshot().await;
        let changes = self.turn_changes(root)?;
        let diff = turn_diff(root, &changes);
        if diff.trim().is_empty() {
            return None;
        }
        self.emit(Event::Busy(Some("Oracle reviewing the changes…".into())));
        let mut oracle = build_subagent(&self.env, self.depth + 1, "oracle");
        oracle.cwd_override = self.cwd_override.clone();
        oracle.reset();
        let prompt = format!(
            "Review this change before it is declared done.\n\nThe user's request:\n{}\n\nThe diff of this turn's changes:\n```diff\n{}\n```\n\n\
             Read the surrounding code as needed. Report real problems only (bugs, missed requirements, broken callers, unsafe behavior); \
             if there are none, answer exactly LGTM.",
            crate::util::truncate(request, 4000),
            crate::util::truncate(&diff, 30_000)
        );
        let end = Box::pin(oracle.run_turn(&prompt, vec![], cancel.clone())).await;
        self.emit(Event::Busy(None));
        let text = match end {
            Ok(TurnEnd::Done(t)) => t.trim().to_string(),
            Ok(TurnEnd::Interrupted) => return None,
            Err(e) => {
                self.emit(Event::Warn(format!(
                    "The oracle's review failed ({}); verifying without it.",
                    crate::util::one_line(&format!("{e:#}"), 160)
                )));
                return None;
            }
        };
        if text.is_empty() || oracle_approves(&text) {
            self.emit(Event::Info("Oracle: no problems found in the diff.".into()));
            return None;
        }
        self.emit(Event::Info(format!(
            "Oracle found problems; sending them to the model:\n{}",
            crate::util::truncate(&text, 1200)
        )));
        Some(text)
    }
}
