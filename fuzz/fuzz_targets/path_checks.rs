//! Path checks behind file-tool permissions.
//!
//! The workspace check (`permissions::in_workspace`) decides whether a file
//! tool may write a path without asking, and `permissions::is_protected_path`
//! whether it touches git internals. Both are compared with the OS itself, on
//! a real directory tree with symlinks that stay inside the project, lead out
//! of it, loop, dangle, and point into `.git`:
//! - a path judged inside the project must really land inside it: it either
//!   resolves (`fs::canonicalize`) there, or, when it doesn't exist yet,
//!   creating it puts the new file there;
//! - a path that really lands inside `.git` must be judged protected.
//!
//! Paths whose parent directory doesn't exist are skipped: writing them would
//! first create directories, which the target doesn't do.

#![no_main]

use libfuzzer_sys::fuzz_target;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

struct Tree {
    /// The tree's root, canonical.
    root: PathBuf,
    /// The project, canonical.
    proj: PathBuf,
}

fn tree() -> &'static Tree {
    static TREE: OnceLock<Tree> = OnceLock::new();
    TREE.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("chanto-fuzz-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let d = std::fs::canonicalize(&d).unwrap();
        let p = d.join("proj");
        let o = d.join("outside");
        for dir in [
            p.join("sub/inner"),
            p.join(".git/hooks"),
            o.join("deep/dir"),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        for f in [p.join("file"), p.join("sub/file"), o.join("file")] {
            std::fs::write(f, "x").unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            for (target, link) in [
                (Path::new("sub"), p.join("in")),
                (Path::new("../outside/deep"), p.join("out")),
                (o.as_path(), p.join("abs")),
                (Path::new(".."), p.join("up")),
                (Path::new("../.."), p.join("sub/back")),
                (Path::new("../outside/missing"), p.join("dangle_out")),
                (Path::new("sub/missing"), p.join("dangle_in")),
                (Path::new("loop"), p.join("loop")),
                (Path::new(".git"), p.join("gitlink")),
                (Path::new("in/back"), p.join("chain")),
                (Path::new("../proj"), o.join("toproj")),
            ] {
                symlink(target, link).unwrap();
            }
        }
        Tree { root: d, proj: p }
    })
}

/// Where the OS puts a write to `p`: the file it resolves to, or the file
/// creating it makes (through a dangling link, its target). `None` when the
/// write would fail, or would need directories created first.
fn lands(p: &Path, t: &Tree) -> Option<PathBuf> {
    if let Ok(c) = std::fs::canonicalize(p) {
        return Some(c);
    }
    let name = p.file_name()?;
    let parent = std::fs::canonicalize(p.parent()?).ok()?;
    if !parent.starts_with(&t.root) {
        // Outside the tree: judge without touching anything there.
        return Some(parent.join(name));
    }
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(p)
        .ok()?;
    let real = std::fs::canonicalize(p).ok();
    if let Some(r) = &real {
        let _ = std::fs::remove_file(r);
    }
    real
}

fuzz_target!(|data: &[u8]| {
    let t = tree();
    // The first byte picks the directory the call runs in (`0` to `4`, in
    // the order below); the rest is the path argument, where a leading `@`
    // stands for the tree's root.
    let Some((&c, rest)) = data.split_first() else {
        return;
    };
    let Ok(path) = std::str::from_utf8(rest) else {
        return;
    };
    // A NUL can't reach the OS; a leading `~` names the real home directory.
    if path.is_empty() || path.contains('\0') || path.starts_with('~') || path.len() > 1024 {
        return;
    }
    let cwd = match c.wrapping_sub(b'0') % 5 {
        0 => t.proj.clone(),
        1 => t.proj.join("sub"),
        2 => t.proj.join("in"),
        3 => t.root.join("outside"),
        _ => t.root.clone(),
    };
    let path = match path.strip_prefix('@') {
        Some(p) => format!("{}{p}", t.root.display()),
        None => path.to_string(),
    };

    let opened = chanto::fuzzing::tool_path(&path, &cwd);
    let Some(real) = lands(&opened, t) else {
        return;
    };
    if chanto::fuzzing::in_project(&path, &cwd, &t.proj) {
        assert!(
            real.starts_with(&t.proj),
            "{path:?} (cwd {}) judged inside the project but lands at {}",
            cwd.display(),
            real.display()
        );
    }
    if real.starts_with(t.proj.join(".git")) {
        assert!(
            chanto::permissions::is_protected_path(&opened),
            "{path:?} (cwd {}) lands in .git at {} but isn't protected",
            cwd.display(),
            real.display()
        );
    }
});
