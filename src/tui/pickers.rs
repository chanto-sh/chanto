//! Pickers: `/model`, `/resume`, `/mcp` and settings share one filtered list,
//! plus the file listing and fuzzy matching they search over.
use super::*;

/// Filter-as-you-type over a picker's rows (`/model`, `/resume`).
pub(super) struct PickerSearch {
    pub(super) query: String,
    /// Every row's label.
    pub(super) all: Vec<String>,
    /// What each row is searched by (the label, or more: `/resume` adds the first prompt).
    pub(super) hay: Vec<String>,
    /// Rows shown, as indices into `all`.
    pub(super) indices: Vec<usize>,
    /// `/resume`: every word must appear (rather than a fuzzy model match),
    /// and ctrl+d / ctrl+a / tab delete, archive and show archived.
    pub(super) sessions: bool,
}

impl PickerSearch {
    pub(super) fn new(labels: &[String], hay: Vec<String>, sessions: bool) -> Self {
        PickerSearch {
            query: String::new(),
            all: labels.to_vec(),
            hay,
            indices: (0..labels.len()).collect(),
            sessions,
        }
    }
    /// Re-filter after the query changed.
    pub(super) fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        if self.sessions {
            self.indices = (0..self.all.len())
                .filter(|&i| {
                    q.split_whitespace()
                        .all(|w| self.hay[i].to_lowercase().contains(w))
                })
                .collect();
        } else {
            let mut scores: Vec<_> = self
                .hay
                .iter()
                .enumerate()
                .filter_map(|(i, m)| fuzzy_score(&m.to_lowercase(), &q).map(|s| (s, i)))
                .collect();
            scores.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
            self.indices = scores.iter().map(|(_, i)| *i).collect();
        }
    }
    /// The dialog's heading for the current query.
    pub(super) fn heading(&self) -> String {
        if self.sessions {
            let head = "**Resume a session · type to search · ctrl+d delete · ctrl+a archive/unarchive · tab show/hide archived**";
            match (self.query.is_empty(), self.indices.is_empty()) {
                (true, _) => head.to_string(),
                (false, false) => format!("{head}\nSearch: {}", self.query),
                (false, true) => format!(
                    "{head}\nSearch: {}\n\nNo session's title or first prompt matches.",
                    self.query
                ),
            }
        } else if self.indices.is_empty() && !self.query.is_empty() {
            format!(
                "**Switch model · Enter saves as default · Ctrl+S this session only**\nSearch: {}\n\nNo listed model matches. Enter uses `{}` as the model name (as `provider/model`).",
                self.query, self.query
            )
        } else {
            format!(
                "**Switch model · Enter saves as default · Ctrl+S this session only**\nSearch: {}",
                self.query
            )
        }
    }
}

impl App {
    /// `/resume`'s picker: type to filter by title and first prompt; ctrl+d
    /// deletes (after asking), ctrl+a archives or unarchives, tab shows or
    /// hides archived sessions. Each action reopens the picker.
    pub(super) fn open_resume_picker(&mut self, archived: bool) {
        let rows = crate::session::Session::browse(&self.env.cwd(), "", archived);
        if rows.is_empty() {
            let note = if archived {
                "No saved sessions for this directory."
            } else {
                "No saved sessions for this directory (archived ones: `/resume --all`)."
            };
            self.note(note, pal().dim);
            return;
        }
        let labels: Vec<String> = rows
            .iter()
            .map(|l| {
                let s = &l.session;
                let title = if s.title.is_empty() {
                    l.first_prompt.as_str()
                } else {
                    s.title.as_str()
                };
                format!(
                    "{}{} · {}{}",
                    "  ".repeat(l.depth),
                    s.updated.get(..16).unwrap_or("").replace('T', " "),
                    crate::util::one_line(title, 70),
                    if s.archived { " [archived]" } else { "" }
                )
            })
            .collect();
        let hay = rows
            .iter()
            .map(|l| format!("{}\n{}\n{}", l.session.title, l.first_prompt, l.session.id))
            .collect();
        let ids: Vec<(String, bool)> = rows
            .iter()
            .map(|l| (l.session.id.clone(), l.session.archived))
            .collect();
        let (tx, rx) = oneshot::channel::<Option<(usize, String)>>();
        let app_tx = self.tx.clone();
        tokio::spawn(async move {
            let Ok(Some((i, action))) = rx.await else {
                return;
            };
            let reopen = if archived { "/resume --all" } else { "/resume" };
            if action == "toggle-archived" {
                let _ = app_tx.send(AppMsg::Submit(if archived {
                    "/resume".into()
                } else {
                    "/resume --all".into()
                }));
                return;
            }
            let Some((id, is_archived)) = ids.get(i) else {
                return;
            };
            match action.as_str() {
                "delete" => {
                    let _ = app_tx.send(AppMsg::Submit(format!("/sessions delete {id}")));
                    let _ = app_tx.send(AppMsg::Submit(reopen.into()));
                }
                "archive" => {
                    let _ = app_tx.send(AppMsg::Submit(format!(
                        "/sessions {} {id}",
                        if *is_archived { "unarchive" } else { "archive" }
                    )));
                    let _ = app_tx.send(AppMsg::Submit(reopen.into()));
                }
                _ => {
                    let _ = app_tx.send(AppMsg::Submit(format!("/resume {id}")));
                }
            }
        });
        let search = PickerSearch::new(&labels, hay, true);
        let q = search.heading();
        self.open_dialog(Dialog::Question {
            q,
            options: labels,
            sel: 0,
            tx: Some(tx),
            typing: None,
            scroll: 0,
            free_text: false,
            search: Some(search),
            action: None,
        });
    }

    /// `/model`, `/resume`, `/rewind` without arguments open a picker.
    pub(super) fn open_picker(&mut self, cmd: &str) -> bool {
        match cmd {
            "/rewind" | "/fork" => {
                let cps: Vec<String> = self
                    .env
                    .checkpoints
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|c| c.prompt.clone())
                    .collect();
                let mut labels = Vec::new();
                let mut values = Vec::new();
                if cmd == "/fork" {
                    labels.push("Current conversation".into());
                    values.push("/fork --at-current".into());
                }
                for (i, p) in cps.iter().enumerate().rev() {
                    labels.push(crate::util::one_line(p, 90));
                    values.push(if cmd == "/fork" {
                        format!("/fork --at {i}")
                    } else {
                        format!("/rewind {i}")
                    });
                }
                let _ = self.tx.send(AppMsg::Picker {
                    title: if cmd == "/fork" {
                        "Fork before a prompt".into()
                    } else {
                        "Rewind: restore files and conversation to before…".into()
                    },
                    labels,
                    values,
                });
                true
            }
            // `/rewind <n>` asks what to restore; `/rewind undo` runs as a command.
            cmd if cmd.starts_with("/rewind ")
                && cmd.split_whitespace().count() == 2
                && cmd
                    .split_whitespace()
                    .nth(1)
                    .is_some_and(|s| s.parse::<usize>().is_ok()) =>
            {
                let idx = cmd
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse::<usize>().ok());
                let tree = idx.and_then(|i| {
                    self.env
                        .checkpoints
                        .lock()
                        .unwrap()
                        .get(i)
                        .and_then(|c| c.tree.clone())
                });
                let summary = tree
                    .and_then(|t| {
                        crate::history::Shadow::new(&self.env.root, &self.env.config.history)
                            .and_then(|s| s.changes(&t))
                            .ok()
                    })
                    .map(|(n, m, k)| {
                        format!("{n} changed, {m} created, {k} deleted since checkpoint")
                    })
                    .unwrap_or_else(|| "File-edit fallback".into());
                let _ = self.tx.send(AppMsg::Picker {
                    title: format!("Rewind: {summary}"),
                    labels: vec![
                        "Restore code and conversation".into(),
                        "Conversation only".into(),
                        "Code only".into(),
                        "Edit this prompt in a new branch (restore code too)".into(),
                        "Edit this prompt in a new branch (keep files as they are)".into(),
                    ],
                    values: vec![
                        format!("{cmd} both"),
                        format!("{cmd} conversation"),
                        format!("{cmd} code"),
                        format!("/edit-prompt {} both --edit", idx.unwrap_or(0)),
                        format!("/edit-prompt {} conversation --edit", idx.unwrap_or(0)),
                    ],
                });
                true
            }
            // From the rewind picker: put that prompt in the input to edit; Enter
            // branches the session there and sends it (see `submit`).
            cmd if cmd.starts_with("/edit-prompt ") && cmd.ends_with(" --edit") => {
                let mut words = cmd.split_whitespace().skip(1);
                let idx = words.next().and_then(|n| n.parse::<usize>().ok());
                let mode = if words.next() == Some("conversation") {
                    "conversation"
                } else {
                    "both"
                };
                let prompt = idx.and_then(|i| {
                    self.env
                        .checkpoints
                        .lock()
                        .unwrap()
                        .get(i)
                        .map(|c| c.prompt.clone())
                });
                match (idx, prompt) {
                    (Some(i), Some(p)) => {
                        self.input
                            .set(p.split("<system-reminder>").next().unwrap_or(&p).trim_end());
                        self.editing_prompt = Some((i, mode));
                        let files = if mode == "both" {
                            "files go back to that point too"
                        } else {
                            "files stay as they are"
                        };
                        self.hint = Some((
                            format!(
                                "Editing an earlier prompt: Enter starts a new branch from there with it ({files}; the original stays in /resume) · esc cancels"
                            ),
                            Instant::now(),
                        ));
                    }
                    _ => self.note(
                        "That prompt is no longer in this session's history.",
                        pal().dim,
                    ),
                }
                true
            }
            "/theme" => {
                let cur = theme::current();
                let themes = theme::available();
                // `auto` first: it follows the terminal's light or dark background.
                let labels = std::iter::once("auto (match the terminal)".to_string())
                    .chain(themes.iter().map(|(n, user)| {
                        format!(
                            "{n}{}{}",
                            if *user { " (yours)" } else { "" },
                            if *n == cur { " (current)" } else { "" }
                        )
                    }))
                    .collect();
                let values = std::iter::once("/theme auto".to_string())
                    .chain(themes.iter().map(|(n, _)| format!("/theme {n}")))
                    .collect();
                let _ = self.tx.send(AppMsg::Picker {
                    title: "Theme".into(),
                    labels,
                    values,
                });
                true
            }
            "/resume" | "/resume --all" => {
                self.open_resume_picker(cmd == "/resume --all");
                true
            }
            "/model" => {
                let env = self.env.clone();
                let tx = self.tx.clone();
                let picker = move |models: Vec<String>, env: &crate::agent::Env| {
                    let cur = env.model();
                    let labels = models
                        .iter()
                        .map(|m| {
                            if *m == cur {
                                format!("{m} (current)")
                            } else {
                                m.clone()
                            }
                        })
                        .collect();
                    let values = models.iter().map(|m| format!("/model {m}")).collect();
                    let _ = tx.send(AppMsg::Picker { title: "Switch model · type to search · Enter saves as default · Ctrl+S this session only".into(), labels, values });
                };
                // Open at once with the remembered list and refresh it for next
                // time; only the very first time waits on the providers.
                let cached = crate::onboarding::cached_models(&env);
                tokio::spawn(async move {
                    match cached {
                        Some((models, _)) => {
                            picker(models, &env);
                            crate::onboarding::refresh_models(&env, true).await;
                        }
                        None => {
                            let models = crate::onboarding::refresh_models(&env, false).await;
                            picker(models, &env);
                        }
                    }
                });
                true
            }
            _ => false,
        }
    }

    /// Interactive editors for `/config`, `/permissions`, `/hooks` and `/agents`:
    /// each row shows current state and its value is the command that changes or
    /// inspects it. Returns false when there is nothing to choose (the caller
    /// then falls back to the textual listing).
    pub(super) fn open_settings_picker(&mut self, cmd: &str) -> bool {
        let mut labels = Vec::new();
        let mut values = Vec::new();
        let title = match cmd {
            "/config" => {
                let mut row = |label: String, value: String| {
                    labels.push(label);
                    values.push(value);
                };
                let mode = self.env.mode();
                row(format!("theme: {}", theme::current()), "/theme".into());
                row(
                    format!("mode: {}", mode.label()),
                    format!("/mode {}", mode_arg(mode.next())),
                );
                row(
                    format!(
                        "sandbox: {}",
                        if crate::tools::bash::sandbox_on() {
                            "on"
                        } else {
                            "off"
                        }
                    ),
                    format!(
                        "/sandbox {}",
                        if crate::tools::bash::sandbox_on() {
                            "off"
                        } else {
                            "on"
                        }
                    ),
                );
                row(
                    format!("vim keys: {}", if self.vim { "on" } else { "off" }),
                    "/vim".into(),
                );
                row("model".into(), "/model".into());
                row("permissions".into(), "/permissions".into());
                row("hooks".into(), "/hooks".into());
                row("subagents".into(), "/agents".into());
                "Settings (select to change)"
            }
            "/permissions" => {
                let r = self.env.rules.read().unwrap();
                for (set, list) in [("allow", &r.allow), ("ask", &r.ask), ("deny", &r.deny)] {
                    for rule in list.iter() {
                        labels.push(format!("{set} · {rule}  (select to remove)"));
                        values.push(format!("/un{set} {rule}"));
                    }
                }
                if labels.is_empty() {
                    return false;
                }
                "Permission rules (select one to remove)"
            }
            "/hooks" => {
                let hooks = self.env.hooks.read().unwrap().clone();
                for (ev, list) in &hooks {
                    for (i, h) in list.iter().enumerate() {
                        labels.push(format!(
                            "{ev} · {}",
                            crate::util::one_line(&h.describe(), 60)
                        ));
                        values.push(format!("/hooks test {ev} {i}"));
                    }
                }
                if labels.is_empty() {
                    return false;
                }
                "Hooks (select to test-run)"
            }
            "/agents" => {
                let agents = self.env.agents.read().unwrap().clone();
                for a in &agents {
                    labels.push(format!(
                        "{} — {}",
                        a.name,
                        crate::util::one_line(&a.description, 60)
                    ));
                    values.push(format!("/agents {}", a.name));
                }
                if labels.is_empty() {
                    return false;
                }
                "Subagents (select to view)"
            }
            _ => return false,
        };
        let _ = self.tx.send(AppMsg::Picker {
            title: title.into(),
            labels,
            values,
        });
        true
    }
}

pub(super) fn list_files(cwd: &std::path::Path) -> Vec<String> {
    let mut v = Vec::new();
    for e in ignore::WalkBuilder::new(cwd)
        .hidden(true)
        .max_depth(Some(12))
        .build()
        .flatten()
    {
        if v.len() >= 20_000 {
            break;
        }
        if e.depth() == 0 {
            continue;
        }
        if let Ok(r) = e.path().strip_prefix(cwd) {
            let mut s = r.display().to_string();
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                s.push('/');
            }
            v.push(s);
        }
    }
    v
}

/// Files `list_files` leaves out because they're ignored (gitignore,
/// .ignore), for `@` completion. Hidden files stay out as they do there, and
/// never anything inside `.git`; the walk is
/// capped so a huge `node_modules` or `target` can't stall the input.
pub(super) fn list_ignored_files(cwd: &std::path::Path, known: &[String]) -> Vec<String> {
    let known: std::collections::HashSet<&str> = known.iter().map(String::as_str).collect();
    let mut v = Vec::new();
    let walk = ignore::WalkBuilder::new(cwd)
        .standard_filters(false)
        .hidden(true)
        .max_depth(Some(8))
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    for e in walk.flatten().take(30_000) {
        if e.depth() == 0 || !e.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if let Ok(r) = e.path().strip_prefix(cwd) {
            let s = r.display().to_string();
            if !known.contains(s.as_str()) {
                v.push(s);
            }
        }
    }
    v
}

/// Subsequence fuzzy match; higher is better.
pub(super) fn fuzzy_score(hay: &str, needle: &str) -> Option<i64> {
    if needle.is_empty() {
        return Some(0);
    }
    if let Some(i) = hay.find(needle) {
        let base = hay
            .rfind('/')
            .map(|j| if i > j { 50 } else { 0 })
            .unwrap_or(50);
        return Some(1000 - i as i64 + base);
    }
    let mut score = 0i64;
    let mut it = hay.chars().enumerate();
    let mut last = None;
    for nc in needle.chars() {
        loop {
            let (i, hc) = it.next()?;
            if hc == nc {
                if let Some(l) = last
                    && i == l + 1
                {
                    score += 5;
                }
                last = Some(i);
                score += 1;
                break;
            }
        }
    }
    Some(score)
}
