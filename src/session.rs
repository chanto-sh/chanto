//! Conversation persistence under ~/.chanto/sessions/<project>/<id>.json.

use crate::config::home_dir;
use crate::llm::{Message, Role};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub fork_point: Option<usize>,
    #[serde(default)]
    pub settings: serde_json::Value,
    pub cwd: PathBuf,
    pub model: String,
    pub created: String,
    pub updated: String,
    pub title: String,
    /// Usage counters for `chanto insights`.
    #[serde(default)]
    pub stats: crate::insights::Stats,
    #[serde(default)]
    pub checkpoints: Vec<crate::agent::Checkpoint>,
    #[serde(default)]
    pub rewind_undo: Option<String>,
    /// Pull requests this session's commands opened or printed, by URL
    /// (`chanto --from-pr` finds the session by one).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pull_requests: Vec<String>,
    /// Hidden from `/resume` and `chanto sessions` unless asked for
    /// (`--all`, Tab in the picker); `-c` skips it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
    /// `--no-session-persistence`: never written to disk.
    #[serde(skip)]
    pub ephemeral: bool,
}

/// A turn that was running tools when the process died (crash, kill, power
/// loss). Kept in `<id>.inflight` beside the session file rather than in it:
/// the marker has to be written before every tool batch and removed right
/// after, so it must be one small write, not a rewrite of the whole transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InFlight {
    /// When the batch began (RFC 3339, local time).
    pub at: String,
    /// The tool batch being run, as "Tool: label", one entry per call.
    pub running: Vec<String>,
    /// Files the turn had already changed before this batch.
    pub files: Vec<String>,
}

/// What to tell the user about an interrupted turn. Resuming leaves the
/// transcript as it is, so the point is to say what
/// happened, name what may have changed, and let the next prompt drive.
pub fn interrupted_notice(f: &InFlight) -> String {
    let mut s = format!(
        "The last turn was interrupted before it finished (a tool batch was running at {}).",
        f.at
    );
    if !f.running.is_empty() {
        s.push_str("\nIt was running:");
        for call in &f.running {
            // One per line: a batch of long commands is unreadable run together.
            s.push_str("\n- ");
            s.push_str(call);
        }
    }
    if f.files.is_empty() {
        s.push_str("\nAnything it changed is on disk, so check `git status` before continuing.");
    } else {
        s.push_str(&format!(
            "\nIt had already changed {}; their current contents are on disk, so check `git status` (or `/diff`) before continuing.",
            f.files.join(", ")
        ));
    }
    s
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SessionFile {
    #[serde(flatten)]
    pub meta: Session,
    pub messages: Vec<Message>,
}

pub(crate) fn project_dir(cwd: &Path) -> PathBuf {
    home_dir().join("sessions").join(project_key(cwd))
}

/// A directory name for a project path: a readable slug plus a hash.
pub fn project_key(cwd: &Path) -> String {
    let full = cwd.display().to_string();
    let slug: String = full
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    // Hash suffix keeps distinct paths with the same slug apart (FNV-1a).
    let mut h: u64 = 0xcbf29ce484222325;
    for b in full.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    let tail: String = slug
        .chars()
        .rev()
        .take(80)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{tail}-{:08x}", h as u32)
}

impl Session {
    pub fn new(cwd: &Path, model: &str) -> Session {
        let now = chrono::Local::now().to_rfc3339();
        Session {
            id: format!(
                "{}-{}",
                chrono::Local::now().format("%Y%m%d-%H%M%S"),
                &uuid::Uuid::new_v4().simple().to_string()[..6]
            ),
            parent_id: None,
            fork_point: None,
            settings: serde_json::Value::Null,
            cwd: cwd.to_path_buf(),
            model: model.into(),
            created: now.clone(),
            updated: now,
            title: String::new(),
            stats: Default::default(),
            checkpoints: Vec::new(),
            rewind_undo: None,
            pull_requests: Vec::new(),
            archived: false,
            ephemeral: false,
        }
    }

    /// A session to resume by id or, failing that, by its name (`--name`,
    /// `/rename`): the newest one here with that title.
    pub fn id_for(cwd: &Path, id_or_name: &str) -> String {
        if Self::load(cwd, id_or_name).is_ok() {
            return id_or_name.to_string();
        }
        Self::list(cwd)
            .into_iter()
            .filter(|s| s.title == id_or_name)
            .max_by(|a, b| a.updated.cmp(&b.updated))
            .map(|s| s.id)
            .unwrap_or_else(|| id_or_name.to_string())
    }

    /// The newest session linked to a pull request: `pr` is its number or URL.
    /// This project's sessions first, then any project's.
    pub fn for_pr(cwd: &Path, pr: &str) -> Option<Session> {
        let pr = pr.trim().trim_start_matches('#');
        let matches = |s: &Session| {
            s.pull_requests
                .iter()
                .any(|u| u == pr || u.trim_end_matches('/').rsplit('/').next() == Some(pr))
        };
        let here = Self::list(cwd)
            .into_iter()
            .filter(matches)
            .max_by(|a, b| a.updated.cmp(&b.updated));
        here.or_else(|| {
            Self::summaries()
                .into_iter()
                .map(|(s, _)| s)
                .filter(matches)
                .max_by(|a, b| a.updated.cmp(&b.updated))
        })
    }

    /// Fork before a checkpoint prompt, or at the end of the conversation.
    pub fn fork(
        &self,
        messages: &[Message],
        name: Option<&str>,
        point: Option<usize>,
    ) -> Result<(Session, Vec<Message>)> {
        let end = match point {
            Some(i) => {
                self.checkpoints
                    .get(i)
                    .filter(|c| c.msg_index > 0 && c.msg_index <= messages.len())
                    .ok_or_else(|| anyhow::anyhow!("checkpoint is unavailable in this transcript"))?
                    .msg_index
            }
            None => messages.len(),
        };
        let mut child = self.clone();
        let fresh = Session::new(&self.cwd, &self.model);
        child.id = fresh.id;
        child.created = fresh.created;
        child.parent_id = Some(self.id.clone());
        child.fork_point = Some(end);
        child.rewind_undo = None;
        if let Some(i) = point {
            child.checkpoints.truncate(i);
        }
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            child.title = name.into();
        }
        let messages = messages[..end].to_vec();
        child.write(messages.clone())?;
        Ok((child, messages))
    }

    pub fn tree(cwd: &Path) -> Vec<(Session, usize)> {
        Self::tree_of(Self::list(cwd))
    }

    /// Sessions (newest first) in tree order: forks under their parent.
    fn tree_of(all: Vec<Session>) -> Vec<(Session, usize)> {
        fn visit(s: &Session, depth: usize, all: &[Session], out: &mut Vec<(Session, usize)>) {
            if out.iter().any(|(x, _)| x.id == s.id) {
                return;
            }
            out.push((s.clone(), depth));
            for child in all.iter().filter(|c| c.parent_id.as_deref() == Some(&s.id)) {
                visit(child, depth + 1, all, out);
            }
        }
        let mut out = Vec::new();
        for s in all
            .iter()
            .filter(|s| !all.iter().any(|p| Some(&p.id) == s.parent_id.as_ref()))
        {
            visit(s, 0, &all, &mut out);
        }
        for s in &all {
            if !out.iter().any(|(x, _)| x.id == s.id) {
                visit(s, 0, &all, &mut out);
            }
        }
        out
    }

    pub fn path(&self) -> PathBuf {
        project_dir(&self.cwd).join(format!("{}.json", self.id))
    }

    /// What to tell the user when this session was interrupted mid-turn, or
    /// `None` when its last turn finished cleanly.
    pub fn interrupted(&self) -> Option<String> {
        self.in_flight().map(|f| interrupted_notice(&f))
    }

    /// The marker beside this session's file, or `None` when none is left.
    pub fn in_flight(&self) -> Option<InFlight> {
        let b = std::fs::read(self.in_flight_path()).ok()?;
        // The first value only: see `mark_in_flight` for what can follow it.
        serde_json::Deserializer::from_slice(&b)
            .into_iter()
            .next()?
            .ok()
    }

    /// Note that a tool batch is about to run, before it touches anything. One
    /// small synchronous write, so a crash mid-batch is visible on resume.
    pub fn mark_in_flight(&self, running: Vec<String>, files: Vec<String>) {
        if self.ephemeral {
            return;
        }
        let f = InFlight {
            at: chrono::Local::now().to_rfc3339(),
            running,
            files,
        };
        let p = self.in_flight_path();
        let Ok(bytes) = serde_json::to_vec(&f) else {
            return;
        };
        // Rewritten in place: a temp file and a rename cost several times as
        // much (on APFS especially), before every tool batch. The one write
        // lands whole even if Chanto is killed; a kill between it and the
        // truncate can leave the end of a longer earlier marker after it,
        // which `in_flight` ignores.
        let write = |p: &Path| -> std::io::Result<()> {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(p)?;
            file.write_all(&bytes)?;
            file.set_len(bytes.len() as u64)
        };
        if write(&p).is_err() {
            let _ = std::fs::create_dir_all(p.parent().unwrap());
            let _ = write(&p);
        }
    }

    /// The batch returned (finished, failed or cancelled): its results are in
    /// the transcript, so the marker's job is done.
    pub fn clear_in_flight(&self) {
        let _ = std::fs::remove_file(self.in_flight_path());
    }

    fn in_flight_path(&self) -> PathBuf {
        self.path().with_extension("inflight")
    }

    /// Update the title and timestamp for a save. False when there's
    /// nothing worth saving yet (no user message).
    pub fn touch(&mut self, messages: &[Message]) -> bool {
        if !messages.iter().any(|m| m.role == Role::User) {
            return false;
        }
        if self.title.is_empty()
            && let Some(m) = messages.iter().find(|m| m.role == Role::User)
        {
            // Titles are listed in plain text, so mask secrets even when transcript redaction is off.
            self.title = crate::util::one_line(
                &crate::secrets::redact_for_export(
                    m.content.split("<system-reminder>").next().unwrap_or(""),
                ),
                80,
            );
        }
        self.updated = chrono::Local::now().to_rfc3339();
        true
    }

    /// Write the session file. Takes the messages by value (the caller's
    /// already-redacted copy) and drops images in place, so history isn't
    /// cloned again.
    pub fn write(&self, mut messages: Vec<Message>) -> Result<()> {
        if self.ephemeral {
            return Ok(());
        }
        let p = self.path();
        std::fs::create_dir_all(p.parent().unwrap())?;
        // Don't persist images (large); keep a placeholder.
        for m in &mut messages {
            if !m.images.is_empty() {
                m.content
                    .push_str(&format!("\n[{} image(s) omitted]", m.images.len()));
                m.images.clear();
            }
        }
        let tmp = p.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4().simple()));
        // Transcripts hold source code, tool output and prompts: keep them
        // readable only by the user. Create at 0600 (not chmod-after-write) so
        // there is no window where the file is world-readable.
        let bytes = serde_json::to_vec(&SessionFile {
            meta: self.clone(),
            messages,
        })?;
        let written = (|| -> std::io::Result<()> {
            #[cfg(unix)]
            {
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&tmp)?;
                f.write_all(&bytes)?;
            }
            #[cfg(not(unix))]
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &p)
        })();
        // A full disk left a temp file behind on every save.
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        Ok(written?)
    }

    pub fn save(&mut self, messages: Vec<Message>) -> Result<()> {
        if self.touch(&messages) || self.path().exists() {
            self.write(messages)?;
        }
        Ok(())
    }

    /// A session file by path.
    pub fn load_file(p: &Path) -> Result<(Session, Vec<Message>)> {
        let f: SessionFile = serde_json::from_slice(&std::fs::read(p)?)?;
        let mut messages = f.messages;
        repair_tool_pairs(&mut messages);
        Ok((f.meta, messages))
    }

    pub fn load(cwd: &Path, id: &str) -> Result<(Session, Vec<Message>)> {
        let p = Self::locate(cwd, id, false)?;
        let f: SessionFile = serde_json::from_slice(&std::fs::read(p)?)?;
        let mut messages = f.messages;
        repair_tool_pairs(&mut messages);
        Ok((f.meta, messages))
    }

    /// The file of this directory's session `id`, or the one starting with it
    /// (the first such; with `unique`, an error when several do).
    fn locate(cwd: &Path, id: &str, unique: bool) -> Result<PathBuf> {
        if (unique && id.is_empty()) || id.contains(['/', '\\']) || id == "." || id == ".." {
            anyhow::bail!("invalid session id");
        }
        let dir = project_dir(cwd);
        let p = dir.join(format!("{id}.json"));
        if p.exists() {
            return Ok(p);
        }
        // Allow unique prefixes.
        let mut found = std::fs::read_dir(&dir)
            .map_err(|_| anyhow::anyhow!("no session `{id}` for this directory"))?
            .flatten()
            .map(|e| e.path())
            .filter(|x| x.extension().and_then(|e| e.to_str()) == Some("json"))
            .filter(|x| {
                x.file_stem()
                    .map(|s| s.to_string_lossy().starts_with(id))
                    .unwrap_or(false)
            });
        let first = found
            .next()
            .ok_or_else(|| anyhow::anyhow!("no session `{id}` for this directory"))?;
        if unique && found.next().is_some() {
            anyhow::bail!("`{id}` matches more than one session; give more of the id");
        }
        Ok(first)
    }

    /// The project directory of the one session, in any project, whose id is `id`
    /// or starts with it. `None` if there is no match or more than one.
    pub fn find_dir(id: &str) -> Option<PathBuf> {
        if id.is_empty() || id.contains(['/', '\\']) || id == "." || id == ".." {
            return None;
        }
        let mut found = home_dir()
            .join("sessions")
            .read_dir()
            .ok()?
            .flatten()
            .flat_map(|p| p.path().read_dir().into_iter().flatten().flatten())
            .map(|e| e.path())
            .filter(|x| {
                x.extension().and_then(|e| e.to_str()) == Some("json")
                    && x.file_stem()
                        .is_some_and(|s| s.to_string_lossy().starts_with(id))
            });
        let (one, more) = (found.next()?, found.next());
        if more.is_some() {
            return None;
        }
        let f: SessionFile = serde_json::from_slice(&std::fs::read(one).ok()?).ok()?;
        Some(f.meta.cwd)
    }

    /// Every saved session, in any project, with how many user messages it
    /// holds; the transcripts themselves are skipped, not kept.
    pub fn summaries() -> Vec<(Session, usize)> {
        #[derive(Deserialize)]
        struct Probe {
            role: Role,
        }
        #[derive(Deserialize)]
        struct Summary {
            #[serde(flatten)]
            meta: Session,
            #[serde(default)]
            messages: Vec<Probe>,
        }
        let projects: Vec<PathBuf> = home_dir()
            .join("sessions")
            .read_dir()
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect();
        projects
            .iter()
            .flat_map(|d| d.read_dir().into_iter().flatten().flatten())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
            .filter_map(|p| serde_json::from_slice::<Summary>(&std::fs::read(&p).ok()?).ok())
            .map(|s| {
                let prompts = s.messages.iter().filter(|m| m.role == Role::User).count();
                (s.meta, prompts)
            })
            .collect()
    }

    /// The newest readable session for a directory (`-c`), and the session
    /// files modified after it that couldn't be read, newest first, with why.
    pub fn latest(cwd: &Path) -> (Option<Session>, Vec<(PathBuf, String)>) {
        let Ok(rd) = std::fs::read_dir(project_dir(cwd)) else {
            return (None, vec![]);
        };
        let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
            .map(|p| {
                (
                    std::fs::metadata(&p)
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH),
                    p,
                )
            })
            .collect();
        files.sort_by_key(|f| std::cmp::Reverse(f.0));
        let mut bad = vec![];
        for (_, p) in files {
            let read = std::fs::read(&p)
                .map_err(|e| e.to_string())
                .and_then(|b| serde_json::from_slice::<SessionFile>(&b).map_err(|e| e.to_string()));
            match read {
                // Archiving rewrites the file, so it would otherwise look newest.
                Ok(f) if f.meta.archived => {}
                Ok(f) => return (Some(f.meta), bad),
                Err(e) => bad.push((p, e)),
            }
        }
        (None, bad)
    }

    /// Sessions for a directory, newest first.
    pub fn list(cwd: &Path) -> Vec<Session> {
        let Ok(rd) = std::fs::read_dir(project_dir(cwd)) else {
            return vec![];
        };
        let mut out: Vec<Session> = rd
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .filter_map(|e| {
                // Read only the metadata cheaply by parsing the whole file; sessions are small enough.
                let f: SessionFile = serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok()?;
                Some(f.meta)
            })
            .collect();
        out.sort_by(|a, b| b.updated.cmp(&a.updated));
        out
    }

    /// Sessions for a directory in tree order with each one's first prompt,
    /// for `/resume` and `chanto sessions`: archived ones only with
    /// `archived`, and only those matching `query` (every word, ignoring
    /// case, in the title, first prompt or id) when it isn't empty.
    pub fn browse(cwd: &Path, query: &str, archived: bool) -> Vec<Listed> {
        let Ok(rd) = std::fs::read_dir(project_dir(cwd)) else {
            return vec![];
        };
        let mut prompts = std::collections::HashMap::new();
        let mut all: Vec<Session> = rd
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .filter_map(|e| {
                serde_json::from_slice::<SessionFile>(&std::fs::read(e.path()).ok()?).ok()
            })
            .filter(|f| archived || !f.meta.archived)
            .map(|f| {
                prompts.insert(f.meta.id.clone(), first_prompt(&f.messages));
                f.meta
            })
            .collect();
        all.sort_by(|a, b| b.updated.cmp(&a.updated));
        Self::tree_of(all)
            .into_iter()
            .map(|(session, depth)| {
                let first_prompt = prompts.remove(&session.id).unwrap_or_default();
                Listed {
                    session,
                    depth,
                    first_prompt,
                }
            })
            .filter(|l| l.matches(query))
            .collect()
    }

    /// Archive (or unarchive) a saved session by id or unique prefix. Its
    /// transcript is kept and it still resumes by id.
    pub fn set_archived(cwd: &Path, id: &str, archived: bool) -> Result<Session> {
        let p = Self::locate(cwd, id, true)?;
        let mut f: SessionFile = serde_json::from_slice(&std::fs::read(&p)?)?;
        f.meta.archived = archived;
        f.meta.write(f.messages)?;
        Ok(f.meta)
    }

    /// Delete a saved session by id or unique prefix: its transcript, and the
    /// rewind snapshots (shadow-history refs) no other session of the project
    /// still uses. Project files are never touched.
    pub fn delete(cwd: &Path, id: &str) -> Result<Session> {
        let p = Self::locate(cwd, id, true)?;
        let session = serde_json::from_slice::<SessionFile>(&std::fs::read(&p)?).map(|f| f.meta);
        std::fs::remove_file(&p)?;
        // An unreadable file is still deleted; it has no snapshots to find.
        let Ok(session) = session else {
            let mut s = Session::new(cwd, "");
            s.id = p
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            return Ok(s);
        };
        let mut trees: Vec<String> = session
            .checkpoints
            .iter()
            .filter_map(|c| c.tree.clone())
            .chain(session.rewind_undo.clone())
            .collect();
        let others = Self::list(&session.cwd);
        trees.retain(|t| {
            !others.iter().any(|o| {
                o.rewind_undo.as_ref() == Some(t)
                    || o.checkpoints.iter().any(|c| c.tree.as_ref() == Some(t))
            })
        });
        trees.sort();
        trees.dedup();
        if let Some(repo) = shadow_repo(&session.cwd).filter(|r| r.join("HEAD").exists()) {
            for tree in trees
                .iter()
                .filter(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_hexdigit()))
            {
                // Best effort: refs nothing uses are also pruned by the daily maintenance.
                let _ = std::process::Command::new("git")
                    .env("GIT_DIR", &repo)
                    .args([
                        "-c",
                        "core.hooksPath=/dev/null",
                        "update-ref",
                        "-d",
                        &format!("refs/checkpoints/{tree}"),
                    ])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
        Ok(session)
    }
}

/// Put tool results directly after the assistant message that asked for them.
/// A transcript saved while a turn was running can have a reminder between the
/// assistant's `tool_calls` and the answers, which OpenAI-style providers
/// reject. Missing results from an interrupted turn get an explicit unknown
/// outcome so resuming never requires replaying a possibly mutating tool.
/// Well-formed transcripts are left unchanged.
pub fn repair_tool_pairs(msgs: &mut Vec<Message>) {
    let mut i = 0;
    while i < msgs.len() {
        if msgs[i].role != Role::Assistant || msgs[i].tool_calls.is_empty() {
            i += 1;
            continue;
        }
        let ids: Vec<String> = msgs[i].tool_calls.iter().map(|c| c.id.clone()).collect();
        // Only messages before the next assistant message can answer this one.
        let end = msgs[i + 1..]
            .iter()
            .position(|m| m.role == Role::Assistant)
            .map_or(msgs.len(), |p| i + 1 + p);
        let mut found: Vec<usize> = (i + 1..end)
            .filter(|&k| {
                msgs[k].role == Role::Tool
                    && msgs[k]
                        .tool_call_id
                        .as_deref()
                        .is_some_and(|id| ids.iter().any(|x| x == id))
            })
            .collect();
        // Remove from the back so the earlier indexes stay valid.
        let moved: Vec<Message> = found
            .drain(..)
            .rev()
            .map(|k| msgs.remove(k))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let mut results = moved;
        for call in &msgs[i].tool_calls {
            if !results
                .iter()
                .any(|m| m.tool_call_id.as_deref() == Some(call.id.as_str()))
            {
                results.push(Message::tool(
                    call,
                    "Tool result unavailable: the previous turn was interrupted before its result was recorded. The tool may have run; inspect the current state before retrying any action.",
                ));
            }
        }
        let n = results.len();
        for (j, m) in results.into_iter().enumerate() {
            msgs.insert(i + 1 + j, m);
        }
        i += 1 + n;
    }
}

/// A row of [`Session::browse`].
#[derive(Debug, Clone)]
pub struct Listed {
    pub session: Session,
    pub depth: usize,
    pub first_prompt: String,
}

impl Listed {
    /// Every word of `query` appears (ignoring case) in the title, first prompt or id.
    pub fn matches(&self, query: &str) -> bool {
        let hay = format!(
            "{}\n{}\n{}",
            self.session.title, self.first_prompt, self.session.id
        )
        .to_lowercase();
        query
            .split_whitespace()
            .all(|w| hay.contains(&w.to_lowercase()))
    }
}

/// The first prompt the user typed, on one line (reminders and secrets masked).
fn first_prompt(messages: &[Message]) -> String {
    messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| {
            m.content
                .split("<system-reminder>")
                .next()
                .unwrap_or("")
                .trim()
        })
        .find(|t| !t.is_empty())
        .map(|t| crate::util::one_line(&crate::secrets::redact_for_export(t), 200))
        .unwrap_or_default()
}

/// The shadow-history repository `/rewind` snapshots a project into
/// (named as `history::Shadow` names it).
fn shadow_repo(cwd: &Path) -> Option<PathBuf> {
    use crate::util::Canonical as _;
    let root = cwd.canonical().ok()?;
    let mut hash = 0xcbf29ce484222325u64;
    for b in root.as_os_str().as_encoded_bytes() {
        hash = (hash ^ *b as u64).wrapping_mul(0x100000001b3);
    }
    Some(home_dir().join("history").join(format!("{hash:016x}")))
}

/// GitHub, GitLab and Bitbucket pull or merge request URLs in command output.
pub fn pull_request_urls(text: &str) -> Vec<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"https://(?:github\.com/[\w.-]+/[\w.-]+/pull|gitlab\.com/[\w./-]+?/-/merge_requests|bitbucket\.org/[\w.-]+/[\w.-]+/pull-requests)/\d+").unwrap());
    let mut out: Vec<String> = re.find_iter(text).map(|m| m.as_str().to_string()).collect();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_tool_batch_keeps_results_and_fills_missing_ones() {
        let calls: Vec<_> = ["c1", "c2"]
            .into_iter()
            .map(|id| crate::llm::ToolCall {
                id: id.into(),
                name: "Write".into(),
                arguments: serde_json::json!({}),
            })
            .collect();
        for partial in [false, true] {
            let mut assistant = Message::assistant("working");
            assistant.tool_calls = calls.clone();
            let mut msgs = vec![assistant, Message::user("continue")];
            if partial {
                msgs.push(Message::tool(&calls[0], "saved result"));
            }
            msgs.push(Message::assistant("next turn"));
            repair_tool_pairs(&mut msgs);
            assert_eq!(msgs.len(), 5);
            for (i, call) in calls.iter().enumerate() {
                assert_eq!(msgs[i + 1].role, Role::Tool);
                assert_eq!(msgs[i + 1].tool_call_id.as_deref(), Some(call.id.as_str()));
            }
            assert!(msgs[2].content.contains("may have run"));
            if partial {
                assert_eq!(msgs[1].content, "saved result");
            }
            assert_eq!(msgs[3].content, "continue");
            let before = serde_json::to_value(&msgs).unwrap();
            repair_tool_pairs(&mut msgs);
            assert_eq!(serde_json::to_value(&msgs).unwrap(), before);
        }
    }

    #[test]
    fn tool_results_are_moved_back_under_their_call() {
        let call = crate::llm::ToolCall {
            id: "c1".into(),
            name: "Read".into(),
            arguments: serde_json::json!({}),
        };
        let mut assistant = Message::assistant("reading");
        assistant.tool_calls = vec![call.clone()];
        let mut msgs = vec![
            Message::user("go"),
            assistant,
            Message::user("<system-reminder>low disk</system-reminder>"),
            Message::tool(&call, "contents"),
            Message::user("next"),
        ];
        repair_tool_pairs(&mut msgs);
        let roles: Vec<Role> = msgs.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![
                Role::User,
                Role::Assistant,
                Role::Tool,
                Role::User,
                Role::User
            ]
        );
        assert_eq!(msgs[2].content, "contents");
        // A well-formed transcript is left alone.
        let before: Vec<(Role, String)> =
            msgs.iter().map(|m| (m.role, m.content.clone())).collect();
        repair_tool_pairs(&mut msgs);
        let after: Vec<(Role, String)> = msgs.iter().map(|m| (m.role, m.content.clone())).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn latest_skips_an_unreadable_session_and_says_why() {
        let _g = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("chanto-latest-{}", uuid::Uuid::new_v4()));
        unsafe {
            std::env::set_var("CHANTO_HOME", &home);
        }
        let cwd = home.join("proj");
        assert!(Session::latest(&cwd).0.is_none());
        let mut good = Session::new(&cwd, "model");
        good.save(vec![Message::user("hi")]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let broken = Session::new(&cwd, "model");
        std::fs::write(broken.path(), "{\"meta\": {\"id\": tr").unwrap();
        let (s, bad) = Session::latest(&cwd);
        assert_eq!(s.map(|s| s.id), Some(good.id.clone()));
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].0, broken.path());
        assert!(!bad[0].1.is_empty());
        // Only the broken one: nothing to fall back to, still reported.
        std::fs::remove_file(good.path()).unwrap();
        let (s, bad) = Session::latest(&cwd);
        assert!(s.is_none() && bad.len() == 1);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn delete_drops_only_its_own_rewind_snapshots() {
        let _g = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("chanto-delete-{}", uuid::Uuid::new_v4()));
        unsafe {
            std::env::set_var("CHANTO_HOME", &home);
        }
        let cwd = home.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("a.txt"), "one").unwrap();
        let shadow = crate::history::Shadow::new(&cwd, &Default::default()).unwrap();
        let shared = shadow.snapshot().unwrap();
        std::fs::write(cwd.join("a.txt"), "two").unwrap();
        let own = shadow.snapshot().unwrap();
        let cp = |tree: &str| crate::agent::Checkpoint {
            id: "c".into(),
            timestamp: "t".into(),
            tree: Some(tree.into()),
            msg_index: 1,
            prompt: "p".into(),
            files: Default::default(),
        };
        let mut gone = Session::new(&cwd, "m");
        gone.checkpoints = vec![cp(&shared), cp(&own)];
        gone.save(vec![Message::user("hi")]).unwrap();
        let mut kept = Session::new(&cwd, "m");
        kept.checkpoints = vec![cp(&shared)];
        kept.save(vec![Message::user("hello")]).unwrap();
        let refs = || {
            let out = std::process::Command::new("git")
                .env("GIT_DIR", shadow_repo(&cwd).unwrap())
                .args(["for-each-ref", "--format=%(refname)"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        assert!(refs().contains(&own) && refs().contains(&shared));
        Session::delete(&cwd, &gone.id).unwrap();
        assert!(!gone.path().exists() && kept.path().exists());
        let after = refs();
        assert!(!after.contains(&own), "{after}");
        assert!(
            after.contains(&shared),
            "a snapshot another session uses stays: {after}"
        );
        // Project files are untouched.
        assert_eq!(std::fs::read_to_string(cwd.join("a.txt")).unwrap(), "two");
        unsafe {
            std::env::remove_var("CHANTO_HOME");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn find_dir_locates_a_session_from_any_project() {
        let _g = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("chanto-find-{}", uuid::Uuid::new_v4()));
        unsafe {
            std::env::set_var("CHANTO_HOME", &home);
        }
        let (a, b) = (home.join("a"), home.join("b"));
        let mut s = Session::new(&a, "model");
        s.save(vec![Message::user("hi")]).unwrap();
        Session::new(&b, "model")
            .save(vec![Message::user("hi")])
            .unwrap();
        assert!(Session::load(&b, &s.id).is_err());
        assert_eq!(Session::find_dir(&s.id), Some(a.clone()));
        assert_eq!(Session::find_dir(&s.id[..s.id.len() - 2]), Some(a));
        // A date prefix matches both sessions: ambiguous.
        assert_eq!(Session::find_dir(&s.id[..8]), None);
        assert_eq!(Session::find_dir(""), None);
        assert_eq!(Session::find_dir("no-such-id"), None);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn forks_diverge_truncate_and_list_as_tree() {
        let _g = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("chanto-fork-{}", uuid::Uuid::new_v4()));
        unsafe {
            std::env::set_var("CHANTO_HOME", &home);
        }
        let cwd = home.join("project");
        let mut parent = Session::new(&cwd, "model");
        parent.settings = serde_json::json!({"mode": "plan"});
        let messages = vec![
            Message::system("sys"),
            Message::user("one"),
            Message::assistant("first"),
            Message::user("two"),
            Message::assistant("second"),
        ];
        parent.checkpoints = [1, 3]
            .iter()
            .map(|&msg_index| crate::agent::Checkpoint {
                id: format!("cp{msg_index}"),
                timestamp: "date".into(),
                tree: Some("tree".into()),
                msg_index,
                prompt: messages[msg_index].content.clone(),
                files: Default::default(),
            })
            .collect();
        parent.save(messages.clone()).unwrap();
        let original = std::fs::read(parent.path()).unwrap();
        let (mut child, mut copy) = parent.fork(&messages, Some("branch"), None).unwrap();
        assert_eq!(copy.len(), messages.len());
        assert_eq!(child.settings, parent.settings);
        assert_eq!(child.checkpoints.len(), 2);
        copy.push(Message::user("child only"));
        child.save(copy).unwrap();
        assert_eq!(std::fs::read(parent.path()).unwrap(), original);
        let (past, copy) = parent.fork(&messages, None, Some(1)).unwrap();
        assert_eq!(copy.len(), 3);
        assert_eq!(past.checkpoints.len(), 1);
        assert_eq!(past.fork_point, Some(3));
        assert!(parent.fork(&messages, None, Some(10)).is_err());
        let tree = Session::tree(&cwd);
        assert_eq!(tree.len(), 3);
        assert_eq!(tree[0].0.id, parent.id);
        assert_eq!(
            tree.iter().map(|(_, depth)| *depth).collect::<Vec<_>>(),
            vec![0, 1, 1]
        );
        unsafe {
            std::env::remove_var("CHANTO_HOME");
        }
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn write_drops_images_and_round_trips() {
        let _g = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("chanto-sess-{}", std::process::id()));
        // SAFETY: test-only, under TEST_HOME_LOCK.
        let previous = std::env::var_os("CHANTO_HOME");
        unsafe { std::env::set_var("CHANTO_HOME", &home) };
        let cwd = home.join("proj");
        let mut s = Session::new(&cwd, "m");
        assert!(
            !s.touch(&[Message::system("sys")]),
            "nothing to save without a user message"
        );
        let mut u = Message::user("look at this");
        u.images = vec!["aGk=".into()];
        let msgs = vec![Message::system("sys"), u, Message::assistant("ok")];
        s.save(msgs).unwrap();
        let (meta, back) = Session::load(&cwd, &s.id).unwrap();
        assert_eq!(meta.title, "look at this");
        // Secrets never reach the title, which is listed in plain text.
        let mut s2 = Session::new(&cwd, "m");
        let key = format!("sk-ant-api03-{}", "Q".repeat(40));
        s2.save(vec![
            Message::system("sys"),
            Message::user(format!("my key is {key}")),
        ])
        .unwrap();
        let (meta2, _) = Session::load(&cwd, &s2.id).unwrap();
        assert!(!meta2.title.contains(&key), "{}", meta2.title);
        assert!(back[1].images.is_empty() && back[1].content.contains("1 image(s) omitted"));
        assert_eq!(back[2].content, "ok");
        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("CHANTO_HOME", previous);
            } else {
                std::env::remove_var("CHANTO_HOME");
            }
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn interrupted_notice_lists_the_running_calls_one_per_line() {
        let f = InFlight {
            at: "2026-10-25T00:00:00-07:00".into(),
            running: vec![
                "Read: Read(src/tui/run.rs)".into(),
                "Bash: grep -rn x".into(),
            ],
            files: vec!["src/lib.rs".into()],
        };
        let notice = interrupted_notice(&f);
        assert!(
            notice
                .contains("\nIt was running:\n- Read: Read(src/tui/run.rs)\n- Bash: grep -rn x\n"),
            "{notice}"
        );
        assert!(
            notice.contains("\nIt had already changed src/lib.rs;"),
            "{notice}"
        );
    }

    #[test]
    fn in_flight_marker_is_rewritten_in_place() {
        let _g = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home =
            std::env::temp_dir().join(format!("chanto-sess-inflight-{}", std::process::id()));
        let previous = std::env::var_os("CHANTO_HOME");
        // SAFETY: test-only, under TEST_HOME_LOCK.
        unsafe { std::env::set_var("CHANTO_HOME", &home) };
        let s = Session::new(&home.join("proj"), "m");
        assert!(s.interrupted().is_none());
        // The session's directory doesn't exist yet.
        s.mark_in_flight(
            vec!["Bash: a long command".repeat(4)],
            vec!["src/a.rs".into()],
        );
        let first = s.interrupted().unwrap();
        assert!(
            first.contains("a long command") && first.contains("src/a.rs"),
            "{first}"
        );
        // A shorter marker replaces a longer one whole.
        s.mark_in_flight(vec!["Read: b.rs".into()], vec![]);
        let f = s.in_flight().unwrap();
        assert_eq!(f.running, ["Read: b.rs"]);
        assert!(f.files.is_empty());
        // Killed between the write and the truncate: the earlier marker's
        // tail after the new one is ignored.
        let p = s.in_flight_path();
        let mut b = std::fs::read(&p).unwrap();
        b.extend_from_slice(b"ommand\"],\"files\":[\"src/a.rs\"]}");
        std::fs::write(&p, b).unwrap();
        assert_eq!(s.in_flight().unwrap().running, ["Read: b.rs"]);
        s.clear_in_flight();
        assert!(s.interrupted().is_none());
        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("CHANTO_HOME", previous);
            } else {
                std::env::remove_var("CHANTO_HOME");
            }
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn ephemeral_sessions_are_not_written_and_names_resolve() {
        let _g = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("chanto-sess-eph-{}", std::process::id()));
        let previous = std::env::var_os("CHANTO_HOME");
        // SAFETY: test-only, under TEST_HOME_LOCK.
        unsafe { std::env::set_var("CHANTO_HOME", &home) };
        let cwd = home.join("proj");
        let mut s = Session::new(&cwd, "m");
        s.ephemeral = true;
        s.save(vec![Message::system("sys"), Message::user("hi")])
            .unwrap();
        assert!(!s.path().exists());
        let mut named = Session::new(&cwd, "m");
        named.title = "auth-refactor".into();
        named
            .save(vec![Message::system("sys"), Message::user("hi")])
            .unwrap();
        assert_eq!(Session::id_for(&cwd, "auth-refactor"), named.id);
        assert_eq!(Session::id_for(&cwd, &named.id), named.id);
        assert_eq!(Session::id_for(&cwd, "nope"), "nope");
        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("CHANTO_HOME", previous);
            } else {
                std::env::remove_var("CHANTO_HOME");
            }
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn save_clears_an_existing_transcript() {
        let _g = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("chanto-sess-clear-{}", std::process::id()));
        // SAFETY: test-only, under TEST_HOME_LOCK.
        let previous = std::env::var_os("CHANTO_HOME");
        unsafe { std::env::set_var("CHANTO_HOME", &home) };
        let cwd = home.join("proj");
        let mut s = Session::new(&cwd, "m");
        s.save(vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant("hi"),
        ])
        .unwrap();
        // /rewind can truncate the history to the system message; the saved
        // transcript must follow, or resuming resurrects the discarded turn.
        s.save(vec![Message::system("sys")]).unwrap();
        let (_, back) = Session::load(&cwd, &s.id).unwrap();
        assert_eq!(back.len(), 1, "rewound history must not stay on disk");
        // A failed save (here the path is a directory; in life a full disk)
        // leaves no temp file behind.
        let mut s2 = Session::new(&cwd, "m");
        std::fs::create_dir_all(s2.path().join("blocker")).unwrap();
        assert!(
            s2.save(vec![Message::system("sys"), Message::user("hello")])
                .is_err()
        );
        let dir = s2.path().parent().unwrap().to_path_buf();
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("CHANTO_HOME", previous);
            } else {
                std::env::remove_var("CHANTO_HOME");
            }
        }
        let _ = std::fs::remove_dir_all(&home);
    }
}
