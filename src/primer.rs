//! First-turn context that saves round trips: a compact listing of the
//! project, and the contents of small files the request names. A model that
//! can see what's there reads everything it needs in one step instead of
//! discovering files one tool call at a time.

use std::path::{Path, PathBuf};

const MAX_LISTED: usize = 80;
const MAX_FILE: u64 = 24 * 1024;
const MAX_TOTAL: usize = 48 * 1024;
const MAX_FILES: usize = 8;

/// Files in the project (respecting .gitignore), or a per-directory summary
/// when there are too many to list.
pub fn project_listing(root: &Path) -> Option<String> {
    let mut files: Vec<String> = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .max_depth(Some(4))
        .hidden(true)
        .git_ignore(true)
        .build();
    for e in walker.flatten() {
        if e.file_type().is_some_and(|t| t.is_file())
            && let Ok(rel) = e.path().strip_prefix(root)
        {
            files.push(rel.to_string_lossy().replace('\\', "/"));
        }
        if files.len() > 5000 {
            break;
        }
    }
    if files.is_empty() {
        return None;
    }
    files.sort();
    if files.len() <= MAX_LISTED {
        return Some(format!(
            "Files in the project ({}):\n{}",
            files.len(),
            files.join("\n")
        ));
    }
    // Too many: top-level entries with file counts.
    let mut top: std::collections::BTreeMap<String, usize> = Default::default();
    for f in &files {
        let first = f.split('/').next().unwrap_or(f);
        let key = if f.contains('/') {
            format!("{first}/")
        } else {
            first.to_string()
        };
        *top.entry(key).or_default() += 1;
    }
    let more = if files.len() > 5000 {
        "5000+".to_string()
    } else {
        files.len().to_string()
    };
    let lines: Vec<String> = top
        .iter()
        .take(MAX_LISTED)
        .map(|(k, n)| {
            if k.ends_with('/') {
                format!("{k} ({n} files)")
            } else {
                k.clone()
            }
        })
        .collect();
    Some(format!(
        "Project layout ({more} files; top level shown):\n{}",
        lines.join("\n")
    ))
}

/// Small text files the request names by path (`INSTRUCTIONS.md`, `src/lib.rs`).
pub fn mentioned_files(root: &Path, request: &str) -> Vec<(PathBuf, String)> {
    let mut out: Vec<(PathBuf, String)> = Vec::new();
    let mut total = 0;
    for raw in request.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                ',' | ';' | '(' | ')' | '`' | '"' | '\'' | '<' | '>' | '[' | ']'
            )
    }) {
        let tok = raw
            .trim_end_matches(['.', ':', '!', '?'])
            .trim_start_matches("./");
        if tok.len() < 3
            || !(tok.contains('.') || tok.contains('/'))
            || tok.contains("://")
            || tok.starts_with('/')
            || tok.contains("..")
        {
            continue;
        }
        let p = root.join(tok);
        if out.iter().any(|(q, _)| *q == p)
            || out.len() >= MAX_FILES
            || crate::secrets::is_secret_path(&p)
        {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_FILE || total + meta.len() as usize > MAX_TOTAL {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        total += text.len();
        out.push((p, text));
    }
    out
}

/// Line-numbered like the Read tool, so Edit's old_string can be copied from it.
pub fn numbered(text: &str) -> String {
    let mut s = String::with_capacity(text.len() + text.len() / 8);
    for (i, line) in text.lines().enumerate() {
        s.push_str(&format!("{}\t{}\n", i + 1, line));
    }
    s
}

#[cfg(test)]
mod tests;
