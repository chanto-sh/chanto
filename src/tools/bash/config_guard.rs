//! Linux: put back files that run code later if a sandboxed command changed them.
//!
//! Landlock only grants access, so keeping `.git/hooks` or `.chanto` read-only
//! inside a writable project would also stop `git commit` and every build. This
//! compares them around a command instead. Those files run outside the sandbox
//! on the next `git` command or session, so a change can't be left in place.
//! macOS doesn't need it: Seatbelt denies these writes outright.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Under the project root; directories cover what's inside them.
const PROTECTED: &[&str] = &[
    ".git/config",
    ".git/hooks",
    ".chanto",
    ".codex",
    ".claude/settings.json",
    ".claude/settings.local.json",
    ".claude/hooks",
    ".husky",
    ".githooks",
    ".mcp.json",
    ".vscode/settings.json",
    ".vscode/tasks.json",
];
/// Chanto's own working files, written by the commands' results as they run.
const SKIP: &[&str] = &[".chanto/worktrees", ".chanto/outcome"];
const MAX_FILES: usize = 2000;
/// Larger files are compared by size and time, and can't be put back.
const MAX_BYTES: u64 = 1 << 20;

#[derive(PartialEq, Clone)]
enum Entry {
    File(Vec<u8>, u32),
    Link(PathBuf),
    Dir,
    Large(u64, Option<std::time::SystemTime>),
}

pub(super) struct ConfigGuard {
    root: PathBuf,
    entries: BTreeMap<PathBuf, Entry>,
}

fn entry(p: &Path) -> Option<Entry> {
    use std::os::unix::fs::PermissionsExt;
    let md = p.symlink_metadata().ok()?;
    if md.file_type().is_symlink() {
        return Some(Entry::Link(std::fs::read_link(p).ok()?));
    }
    if md.is_dir() {
        return Some(Entry::Dir);
    }
    if md.len() > MAX_BYTES {
        return Some(Entry::Large(md.len(), md.modified().ok()));
    }
    Some(Entry::File(std::fs::read(p).ok()?, md.permissions().mode()))
}

fn scan(root: &Path) -> BTreeMap<PathBuf, Entry> {
    let mut out = BTreeMap::new();
    let mut todo: Vec<PathBuf> = PROTECTED.iter().map(|r| root.join(r)).collect();
    // The parents too: a parent swapped for a link would carry writes elsewhere.
    todo.extend([".git", ".claude", ".vscode"].map(|r| root.join(r)));
    while let Some(p) = todo.pop() {
        if out.len() >= MAX_FILES || SKIP.iter().any(|s| p == root.join(s)) {
            continue;
        }
        let Some(e) = entry(&p) else { continue };
        let is_parent = [".git", ".claude", ".vscode"]
            .iter()
            .any(|r| p == root.join(r));
        if e == Entry::Dir
            && !is_parent
            && let Ok(rd) = std::fs::read_dir(&p)
        {
            todo.extend(rd.flatten().map(|c| c.path()));
        }
        out.insert(p, e);
    }
    out
}

fn put_back(path: &Path, was: &Entry) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let remove = |p: &Path| match p.symlink_metadata() {
        Ok(md) if md.is_dir() => std::fs::remove_dir_all(p),
        Ok(_) => std::fs::remove_file(p),
        Err(_) => Ok(()),
    };
    match was {
        Entry::File(bytes, mode) => {
            remove(path)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, bytes)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(*mode))
        }
        Entry::Link(target) => {
            remove(path)?;
            std::os::unix::fs::symlink(target, path)
        }
        Entry::Dir => {
            if !path.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                remove(path)?;
                std::fs::create_dir_all(path)?;
            }
            Ok(())
        }
        Entry::Large(..) => Err(std::io::Error::other("too large to keep a copy")),
    }
}

impl ConfigGuard {
    pub(super) fn take(root: &Path) -> ConfigGuard {
        ConfigGuard {
            root: root.to_path_buf(),
            entries: scan(root),
        }
    }

    /// Undo changes since [`ConfigGuard::take`]. Returns a note for the model
    /// and the user, if there was anything to undo.
    pub(super) fn restore(&self) -> Option<String> {
        let now = scan(&self.root);
        let mut changed = Vec::new();
        let mut failed = Vec::new();
        // What's new goes first, deepest first, so a directory that was swapped
        // for a link is gone before anything is written back through it.
        for path in now.keys().rev() {
            if !self.entries.contains_key(path) {
                changed.push(path.clone());
                match path.symlink_metadata() {
                    // Deepest first, so it's empty unless it holds a working
                    // file of Chanto's, which stays.
                    Ok(md) if md.is_dir() => {
                        let _ = std::fs::remove_dir(path);
                    }
                    Ok(_) if std::fs::remove_file(path).is_err() => failed.push(path.clone()),
                    _ => {}
                }
            }
        }
        for (path, was) in &self.entries {
            if now.get(path) != Some(was) {
                changed.push(path.clone());
                if put_back(path, was).is_err() {
                    failed.push(path.clone());
                }
            }
        }
        if changed.is_empty() {
            return None;
        }
        changed.sort();
        changed.dedup();
        let names = |list: &[PathBuf]| {
            list.iter()
                .map(|p| {
                    p.strip_prefix(&self.root)
                        .unwrap_or(p)
                        .display()
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut note = format!(
            "\n[The sandbox undid this command's changes to {}: these files run code outside it later. If the change is needed, run the command again with dangerouslyDisableSandbox: true, and the user will be asked to approve.]",
            names(&changed)
        );
        if !failed.is_empty() {
            note.push_str(&format!(
                "\n[WARNING: couldn't undo the change to {}. Check before running git in this project.]",
                names(&failed)
            ));
        }
        Some(note)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "chanto-guard-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(d.join(".git/hooks")).unwrap();
        std::fs::write(d.join(".git/config"), "[core]\n").unwrap();
        std::fs::write(d.join(".git/HEAD"), "ref\n").unwrap();
        std::fs::write(d.join("a.txt"), "a\n").unwrap();
        d
    }

    #[test]
    fn nothing_changed_says_nothing() {
        let d = project("same");
        let g = ConfigGuard::take(&d);
        std::fs::write(d.join("a.txt"), "edited\n").unwrap();
        std::fs::write(d.join(".git/HEAD"), "moved\n").unwrap();
        assert!(g.restore().is_none(), "ordinary work isn't touched");
        assert_eq!(
            std::fs::read_to_string(d.join(".git/HEAD")).unwrap(),
            "moved\n"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn planted_and_edited_files_are_put_back() {
        let d = project("plant");
        std::fs::write(d.join(".mcp.json"), "{}\n").unwrap();
        let g = ConfigGuard::take(&d);
        std::fs::write(d.join(".git/hooks/post-commit"), "evil").unwrap();
        std::fs::write(d.join(".git/config"), "[core]\n\tfsmonitor = evil\n").unwrap();
        std::fs::write(d.join(".mcp.json"), "evil").unwrap();
        std::fs::create_dir_all(d.join(".chanto")).unwrap();
        std::fs::write(d.join(".chanto/config.toml"), "evil").unwrap();
        let note = g.restore().expect("the changes should be reported");
        assert!(
            note.contains(".git/config") && note.contains(".mcp.json"),
            "{note}"
        );
        assert!(!d.join(".git/hooks/post-commit").exists());
        assert!(!d.join(".chanto").exists());
        assert_eq!(
            std::fs::read_to_string(d.join(".git/config")).unwrap(),
            "[core]\n"
        );
        assert_eq!(
            std::fs::read_to_string(d.join(".mcp.json")).unwrap(),
            "{}\n"
        );
        assert!(g.restore().is_none(), "nothing is left to undo");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_directory_swapped_for_a_link_is_put_back() {
        let d = project("swap");
        let elsewhere = project("elsewhere");
        std::fs::write(d.join(".git/hooks/keep"), "mine").unwrap();
        let g = ConfigGuard::take(&d);
        std::fs::remove_dir_all(d.join(".git/hooks")).unwrap();
        std::os::unix::fs::symlink(elsewhere.join(".git/hooks"), d.join(".git/hooks")).unwrap();
        assert!(g.restore().is_some());
        assert!(
            !d.join(".git/hooks")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(d.join(".git/hooks/keep")).unwrap(),
            "mine"
        );
        assert_eq!(
            std::fs::read_dir(elsewhere.join(".git/hooks"))
                .unwrap()
                .count(),
            0
        );
        std::fs::remove_dir_all(&d).unwrap();
        std::fs::remove_dir_all(&elsewhere).unwrap();
    }

    #[test]
    fn chantos_own_working_files_are_left_alone() {
        let d = project("own");
        let g = ConfigGuard::take(&d);
        std::fs::create_dir_all(d.join(".chanto/worktrees/w")).unwrap();
        std::fs::write(d.join(".chanto/worktrees/w/a"), "x").unwrap();
        std::fs::write(d.join(".chanto/config.toml"), "evil").unwrap();
        assert!(g.restore().is_some(), ".chanto itself is new");
        assert!(!d.join(".chanto/config.toml").exists());
        assert!(
            d.join(".chanto/worktrees/w/a").exists(),
            "a worktree survives"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }
}
