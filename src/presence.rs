//! Sessions working in the same project, so agents running in different
//! terminals know about each other. Each live session keeps a small record
//! (its task, the files it recently edited, a heartbeat) under
//! `~/.chanto/presence/<project>/`; the others read it to steer clear of the
//! same files, and an edit to a file another session just changed stops once
//! to say who has it. Records that stop beating are ignored and cleaned up.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// A record older than this belongs to a session that has gone.
const ALIVE: i64 = 90;
/// Another session's edit this recent makes a file "theirs" for now.
const RECENT: i64 = 600;
const BEAT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Record {
    pub id: String,
    pub pid: u32,
    /// The latest prompt, one line.
    pub task: String,
    /// Unix seconds of the last heartbeat.
    pub beat: i64,
    /// Files edited (absolute path, unix seconds), newest last.
    pub files: Vec<(String, i64)>,
}

struct Me {
    dir: PathBuf,
    record: Mutex<Record>,
    /// Warnings already given, so a deliberate retry goes ahead.
    warned: Mutex<HashSet<String>>,
}

static ME: OnceLock<Me> = OnceLock::new();

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn dir_for(root: &Path) -> PathBuf {
    crate::config::home_dir()
        .join("presence")
        .join(crate::session::project_key(root))
}

impl Me {
    fn path(&self) -> PathBuf {
        self.dir
            .join(format!("{}.json", self.record.lock().unwrap().id))
    }

    fn save(&self) {
        let mut r = self.record.lock().unwrap();
        r.beat = now();
        let _ = std::fs::create_dir_all(&self.dir);
        let tmp = self.dir.join(format!(".{}.tmp", r.id));
        if std::fs::write(&tmp, serde_json::to_vec(&*r).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, self.dir.join(format!("{}.json", r.id)));
        }
    }
}

/// Whether this session takes part in presence at all. Off with
/// `CHANTO_NO_PRESENCE` (any value but `0`): for runs that shouldn't appear to,
/// or be steered by, other sessions in the same project (benchmarks, scripts).
pub fn enabled() -> bool {
    !matches!(
        std::env::var("CHANTO_NO_PRESENCE").as_deref(),
        Ok(v) if v != "0"
    )
}

/// Register this process as working in `root`, and keep the record fresh.
pub fn start(root: &Path) {
    if !enabled() {
        return;
    }
    let me = Me {
        dir: dir_for(root),
        record: Mutex::new(Record {
            id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
            pid: std::process::id(),
            ..Default::default()
        }),
        warned: Default::default(),
    };
    if ME.set(me).is_err() {
        return;
    }
    let me = ME.get().unwrap();
    me.save();
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(BEAT);
            if let Some(me) = ME.get() {
                me.save();
            }
        }
    });
}

/// Remove this process's record (normal exit; a crash just stops the beat).
pub fn stop() {
    if let Some(me) = ME.get() {
        let _ = std::fs::remove_file(me.path());
    }
}

/// What this session is doing now: its latest prompt.
pub fn set_task(task: &str) {
    if let Some(me) = ME.get() {
        me.record.lock().unwrap().task = crate::util::one_line(task, 120);
        me.save();
    }
}

/// Live sessions in the same project, other than this one.
pub fn peers() -> Vec<Record> {
    let Some(me) = ME.get() else { return vec![] };
    let mine = me.record.lock().unwrap().id.clone();
    read(&me.dir, &mine, now())
}

fn read(dir: &Path, mine: &str, at: i64) -> Vec<Record> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Some(r) = std::fs::read(&p)
            .ok()
            .and_then(|b| serde_json::from_slice::<Record>(&b).ok())
        else {
            continue;
        };
        if r.id == mine {
            continue;
        }
        if at - r.beat > ALIVE {
            // Gone for a day: tidy up.
            if at - r.beat > 86_400 {
                let _ = std::fs::remove_file(&p);
            }
            continue;
        }
        out.push(r);
    }
    // A session killed outright (terminal tab closed) never removes its record;
    // don't wait out its last heartbeat if its process is already gone.
    out.retain(|r| r.pid == 0 || crate::resources::pid_alive(r.pid));
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Before editing `path`: if another live session edited it in the last ten
/// minutes, say so once (the agent can then pick other files, or retry to go
/// ahead). Records the edit for the others either way.
pub fn claim(path: &Path) -> Option<String> {
    let me = ME.get()?;
    // The same file however a session spells its path.
    let key = std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string();
    let at = now();
    let mine = me.record.lock().unwrap().id.clone();
    let warning = conflict(&read(&me.dir, &mine, at), &key, at).filter(|(peer, when, _)| {
        me.warned
            .lock()
            .unwrap()
            .insert(format!("{peer}|{key}|{when}"))
    });
    {
        let mut r = me.record.lock().unwrap();
        r.files.retain(|(f, _)| f != &key);
        r.files.push((key.clone(), at));
        let n = r.files.len();
        if n > 50 {
            r.files.drain(..n - 50);
        }
    }
    me.save();
    warning.map(|(_, when, _)| {
        format!(
            "Another chanto session working in this project edited {key} {} ago. To avoid overwriting each other, work on other files if you can, or tell the user. To edit it anyway, make the same edit again.",
            ago(at - when)
        )
    })
}

/// The live peer that edited `key` most recently within RECENT: (id, when, task).
fn conflict(peers: &[Record], key: &str, at: i64) -> Option<(String, i64, String)> {
    peers
        .iter()
        .filter_map(|p| {
            p.files
                .iter()
                .find(|(f, _)| f == key)
                .map(|(_, t)| (p.id.clone(), *t, p.task.clone()))
        })
        .filter(|(_, t, _)| at - t <= RECENT)
        .max_by_key(|(_, t, _)| *t)
}

fn ago(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{} min", secs / 60)
    }
}

/// For the agent at the start of a turn: who else is working here, on what.
/// Never includes a peer's prompt text: that belongs to the other session, and
/// putting one session's words in another's model context is how a stray
/// instruction (or a mis-sent prompt) ends up steering the wrong agent. The
/// human can see the tasks they care about with `/peers`.
pub fn context() -> Option<String> {
    let peers = peers();
    (!peers.is_empty()).then(|| {
        let at = now();
        let lines: Vec<String> = peers.iter().map(|p| describe_files(p, at)).collect();
        format!(
            "Other chanto sessions are working in this project right now:\n{}\nAvoid editing the files they're working on; if your task needs them, tell the user.",
            lines.join("\n")
        )
    })
}

/// "- recently edited a.rs, b.rs" — a peer without any prompt text.
pub fn describe_files(p: &Record, at: i64) -> String {
    let recent: Vec<&str> = recent_files(p, at);
    if recent.is_empty() {
        "- another chanto session is working here (no recent edits)".to_string()
    } else {
        format!(
            "- another chanto session recently edited {}",
            recent.join(", ")
        )
    }
}

fn recent_files(p: &Record, at: i64) -> Vec<&str> {
    p.files
        .iter()
        .rev()
        .filter(|(_, t)| at - t <= RECENT)
        .map(|(f, _)| f.as_str())
        .take(8)
        .collect()
}

/// "- "fix the login bug"; recently edited a.rs, b.rs"
pub fn describe(p: &Record, at: i64) -> String {
    let task = if p.task.is_empty() {
        "(no task yet)".to_string()
    } else {
        format!("\"{}\"", p.task)
    };
    let recent = recent_files(p, at);
    if recent.is_empty() {
        format!("- {task}")
    } else {
        format!("- {task}; recently edited {}", recent.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, beat: i64, files: &[(&str, i64)]) -> Record {
        Record {
            id: id.into(),
            pid: std::process::id(),
            task: format!("task {id}"),
            beat,
            files: files.iter().map(|(f, t)| (f.to_string(), *t)).collect(),
        }
    }

    #[test]
    fn live_peers_and_recent_conflicts() {
        let dir = std::env::temp_dir().join(format!("chanto-presence-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let at = 10_000;
        // A process that has exited: its record still beats recently, but it's gone.
        let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" })
            .args(if cfg!(windows) {
                &["/c", "exit"][..]
            } else {
                &[][..]
            })
            .spawn()
            .unwrap();
        let dead = Record {
            pid: child.id(),
            ..record("killed", at - 10, &[("/p/a.rs", at - 5)])
        };
        child.wait().unwrap();
        for r in [
            record("me", at, &[]),
            record(
                "live",
                at - 30,
                &[("/p/a.rs", at - 100), ("/p/old.rs", at - 5000)],
            ),
            record("gone", at - 500, &[("/p/a.rs", at - 10)]),
            dead,
        ] {
            std::fs::write(
                dir.join(format!("{}.json", r.id)),
                serde_json::to_vec(&r).unwrap(),
            )
            .unwrap();
        }
        let peers = read(&dir, "me", at);
        // Not this session, not one that stopped beating, not one whose process exited.
        assert_eq!(
            peers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            ["live"]
        );
        // A recent edit by a live peer is a conflict; an old one isn't.
        assert_eq!(
            conflict(&peers, "/p/a.rs", at),
            Some(("live".into(), at - 100, "task live".into()))
        );
        assert_eq!(conflict(&peers, "/p/old.rs", at), None);
        assert_eq!(conflict(&peers, "/p/other.rs", at), None);
        assert_eq!(
            describe(&peers[0], at),
            "- \"task live\"; recently edited /p/a.rs"
        );
        // What the *model* is told about a peer: the files, never its prompt.
        assert_eq!(
            describe_files(&peers[0], at),
            "- another chanto session recently edited /p/a.rs"
        );
        assert!(!describe_files(&peers[0], at).contains("task live"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
