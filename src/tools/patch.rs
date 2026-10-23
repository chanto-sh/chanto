//! `apply_patch`: Codex's file-oriented patch format. Models trained on
//! Codex (e.g. gpt-oss) emit it natively, either as a tool call or as a
//! shell heredoc, so supporting it saves them from translating into Edit.
//!
//! ```text
//! *** Begin Patch
//! *** Add File: path
//! +line
//! *** Update File: path
//! *** Move to: new/path
//! @@ optional context line
//!  context
//! -old
//! +new
//! *** Delete File: path
//! *** End Patch
//! ```

use super::{Ctx, Tool, ToolKind, ToolOutput, arg_str};
use crate::util::resolve;
use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq)]
pub enum Op {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        chunks: Vec<Chunk>,
    },
}

#[derive(Debug, PartialEq, Default)]
pub struct Chunk {
    /// `@@ <context>` header: a line to seek to before matching.
    pub header: Option<String>,
    pub old: Vec<String>,
    pub new: Vec<String>,
    /// `*** End of File`: the chunk must match at the end.
    pub eof: bool,
}

pub fn parse(patch: &str) -> Result<Vec<Op>> {
    let lines: Vec<&str> = patch.trim().lines().collect();
    let start = lines
        .iter()
        .position(|l| l.trim() == "*** Begin Patch")
        .ok_or_else(|| anyhow::anyhow!("patch must start with `*** Begin Patch`"))?;
    let end = lines
        .iter()
        .rposition(|l| l.trim() == "*** End Patch")
        .unwrap_or(lines.len());
    let mut ops = Vec::new();
    let mut i = start + 1;
    while i < end {
        let l = lines[i];
        if let Some(p) = l.strip_prefix("*** Add File: ") {
            i += 1;
            let mut content = String::new();
            while i < end && !lines[i].starts_with("*** ") {
                content.push_str(lines[i].strip_prefix('+').unwrap_or(lines[i]));
                content.push('\n');
                i += 1;
            }
            ops.push(Op::Add {
                path: p.trim().into(),
                content,
            });
        } else if let Some(p) = l.strip_prefix("*** Delete File: ") {
            ops.push(Op::Delete {
                path: p.trim().into(),
            });
            i += 1;
        } else if let Some(p) = l.strip_prefix("*** Update File: ") {
            i += 1;
            let mut move_to = None;
            if let Some(m) = lines.get(i).and_then(|l| l.strip_prefix("*** Move to: ")) {
                move_to = Some(m.trim().to_string());
                i += 1;
            }
            let mut chunks: Vec<Chunk> = Vec::new();
            let mut cur = Chunk::default();
            while i < end && !(lines[i].starts_with("*** ") && lines[i].trim() != "*** End of File")
            {
                let l = lines[i];
                if let Some(h) = l.strip_prefix("@@") {
                    if !cur.old.is_empty() || !cur.new.is_empty() {
                        chunks.push(std::mem::take(&mut cur));
                    }
                    let h = h.trim();
                    cur.header = (!h.is_empty()).then(|| h.to_string());
                } else if l.trim() == "*** End of File" {
                    cur.eof = true;
                } else if let Some(r) = l.strip_prefix('-') {
                    cur.old.push(r.into());
                } else if let Some(a) = l.strip_prefix('+') {
                    cur.new.push(a.into());
                } else {
                    // Context line (leading space; tolerate a missing one).
                    let c = l.strip_prefix(' ').unwrap_or(l);
                    cur.old.push(c.into());
                    cur.new.push(c.into());
                }
                i += 1;
            }
            if !cur.old.is_empty() || !cur.new.is_empty() {
                chunks.push(cur);
            }
            if chunks.is_empty() && move_to.is_none() {
                bail!("Update File {p} has no changes");
            }
            ops.push(Op::Update {
                path: p.trim().into(),
                move_to,
                chunks,
            });
        } else if l.trim().is_empty() {
            i += 1;
        } else {
            bail!(
                "unexpected line in patch: `{l}` (expected `*** Add File:`, `*** Update File:` or `*** Delete File:`)"
            );
        }
    }
    if ops.is_empty() {
        bail!("patch contains no file operations");
    }
    Ok(ops)
}

/// Find `needle` in `hay` at or after `from`, trying exact, then trailing-
/// whitespace-insensitive, then fully trimmed comparison.
fn seek(hay: &[String], needle: &[String], from: usize, eof: bool) -> Option<usize> {
    if needle.is_empty() {
        return Some(if eof { hay.len() } else { from.min(hay.len()) });
    }
    if needle.len() > hay.len() {
        return None;
    }
    let cmps: [fn(&str, &str) -> bool; 3] = [
        |a, b| a == b,
        |a, b| a.trim_end() == b.trim_end(),
        |a, b| a.trim() == b.trim(),
    ];
    for eq in cmps {
        let starts: Vec<usize> = if eof {
            vec![hay.len() - needle.len()]
        } else {
            (from..=hay.len() - needle.len()).collect()
        };
        for s in starts {
            if s >= from && needle.iter().enumerate().all(|(k, n)| eq(&hay[s + k], n)) {
                return Some(s);
            }
        }
    }
    None
}

/// Apply chunks to file text.
pub fn apply_chunks(text: &str, chunks: &[Chunk], path: &str) -> Result<String> {
    let trailing_nl = text.ends_with('\n') || text.is_empty();
    // `str::lines` drops the `\r` of CRLF files; keep the file's style so a
    // one-line edit doesn't quietly convert a whole Windows file to LF.
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut pos = 0;
    for (n, c) in chunks.iter().enumerate() {
        if let Some(h) = &c.header
            && let Some(k) = seek(&lines, std::slice::from_ref(h), pos, false)
        {
            pos = k + 1;
        }
        // A hunk of pure additions with no anchor goes at the end of the file (as in Codex).
        let eof = c.eof || (c.old.is_empty() && c.header.is_none());
        let at = seek(&lines, &c.old, pos, eof)
            .or_else(|| seek(&lines, &c.old, 0, eof))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "chunk {} of {path} did not match the file. Expected these lines:\n{}\nRead the file and retry with exact context.",
                    n + 1,
                    c.old.join("\n")
                )
            })?;
        lines.splice(at..at + c.old.len(), c.new.iter().cloned());
        pos = at + c.new.len();
    }
    let mut out = lines.join(nl);
    if trailing_nl && !out.is_empty() {
        out.push_str(nl);
    }
    Ok(out)
}

pub fn paths(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|l| {
            [
                "*** Add File: ",
                "*** Update File: ",
                "*** Delete File: ",
                "*** Move to: ",
            ]
            .iter()
            .find_map(|p| l.strip_prefix(p))
        })
        .map(|p| p.trim().to_string())
        .collect()
}

/// Extract the patch from a shell command like `apply_patch <<'EOF' ... EOF`
/// or `apply_patch '*** Begin Patch ...'`.
pub fn from_shell(command: &str) -> Option<String> {
    let c = command.trim_start();
    let rest = c
        .strip_prefix("apply_patch")
        .or_else(|| c.strip_prefix("applypatch"))?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let begin = rest.find("*** Begin Patch")?;
    let end = rest.rfind("*** End Patch")? + "*** End Patch".len();
    (begin < end).then(|| rest[begin..end].to_string())
}

pub struct ApplyPatch;

#[async_trait]
impl Tool for ApplyPatch {
    fn name(&self) -> String {
        "apply_patch".into()
    }
    fn description(&self) -> String {
        "Edit files with a patch: `*** Begin Patch`, then `*** Add File: <path>` (lines prefixed +), \
`*** Update File: <path>` (optional `*** Move to: <path>`, then hunks starting with `@@` using ' ' context, '-' removed, '+' added lines), \
or `*** Delete File: <path>`, and finally `*** End Patch`."
            .into()
    }
    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {"input": {"type": "string", "description": "The full patch text"}}, "required": ["input"]})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }
    fn rule_subjects(&self, args: &Value) -> Vec<String> {
        paths(args["input"].as_str().unwrap_or(""))
    }
    fn label(&self, args: &Value) -> String {
        let p = paths(args["input"].as_str().unwrap_or(""));
        format!("Patch({})", p.join(", "))
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let cwd = ctx.cwd();
        let patch = arg_str(&args, "input").or_else(|_| arg_str(&args, "patch"))?;
        let ops = match parse(patch) {
            Ok(o) => o,
            Err(e) => return Ok(ToolOutput::err(format!("Invalid patch: {e}"))),
        };
        // Apply every op to an in-memory view first (so later ops see earlier
        // ones and a bad chunk leaves no file half-written), then write.
        let mut state: Vec<(PathBuf, Option<String>, Option<String>)> = Vec::new(); // (path, original, current)
        fn slot<'a>(
            state: &'a mut Vec<(PathBuf, Option<String>, Option<String>)>,
            p: &Path,
        ) -> &'a mut (PathBuf, Option<String>, Option<String>) {
            if let Some(i) = state.iter().position(|(q, _, _)| q == p) {
                return &mut state[i];
            }
            let orig = std::fs::read_to_string(p).ok();
            state.push((p.to_path_buf(), orig.clone(), orig));
            state.last_mut().unwrap()
        }
        // Read the editor's current text before applying any patch operations.
        for path in paths(patch) {
            let p = resolve(&cwd, &path);
            if state.iter().any(|(q, _, _)| q == &p) {
                continue;
            }
            let original = super::fs::read_text(ctx, &p).await.ok();
            state.push((p, original.clone(), original));
        }
        for op in &ops {
            match op {
                Op::Add { path, content } => {
                    // `*** Add File:` means "create", so refuse to clobber an
                    // existing file (Codex's apply_patch errors here too). Without
                    // this a model-emitted patch silently destroys user files.
                    let s = slot(&mut state, &resolve(&cwd, path));
                    if s.1.is_some() {
                        return Ok(ToolOutput::err(format!(
                            "Cannot add {path}: file already exists (use `*** Update File:` to change it)"
                        )));
                    }
                    s.2 = Some(content.clone());
                }
                Op::Delete { path } => {
                    let s = slot(&mut state, &resolve(&cwd, path));
                    if s.2.is_none() {
                        return Ok(ToolOutput::err(format!(
                            "Cannot delete {path}: file not found"
                        )));
                    }
                    s.2 = None;
                }
                Op::Update {
                    path,
                    move_to,
                    chunks,
                } => {
                    let s = slot(&mut state, &resolve(&cwd, path));
                    let Some(cur) = s.2.clone() else {
                        return Ok(ToolOutput::err(format!(
                            "Cannot update {path}: file not found"
                        )));
                    };
                    let new = match apply_chunks(&cur, chunks, path) {
                        Ok(n) => n,
                        Err(e) => return Ok(ToolOutput::err(format!("{e}"))),
                    };
                    match move_to {
                        Some(m) => {
                            s.2 = None;
                            slot(&mut state, &resolve(&cwd, m)).2 = Some(new);
                        }
                        None => s.2 = Some(new),
                    }
                }
            }
        }
        let writes: Vec<(PathBuf, Option<String>, Option<String>)> =
            state.into_iter().filter(|(_, o, n)| o != n).collect();
        let mut summary = Vec::new();
        let mut display = Vec::new();
        // Files already applied, with their prior content, so a failure partway
        // through a multi-file patch can be undone rather than left half-applied.
        let mut applied: Vec<(PathBuf, Option<String>)> = Vec::new();
        for (p, old, new) in &writes {
            let r = rel(&cwd, p);
            let result = match new {
                Some(n) => {
                    let w = super::fs::write_text(ctx, p, old.as_deref(), n).await;
                    if w.is_ok() {
                        ctx.env.mark_read(p);
                    }
                    w
                }
                None => {
                    ctx.env.snapshot_file(p);
                    let result = tokio::fs::remove_file(p).await.map_err(anyhow::Error::from);
                    if result.is_ok() {
                        ctx.env.frontend.file_diff(
                            ctx.call_id.as_deref().unwrap_or(""),
                            p,
                            old.as_deref(),
                            "",
                        );
                    }
                    result
                }
            };
            if let Err(e) = result {
                for (rp, rold) in applied.iter().rev() {
                    match rold {
                        Some(c) => {
                            let _ = super::fs::write_text(ctx, rp, None, c).await;
                        }
                        None => {
                            let _ = tokio::fs::remove_file(rp).await;
                        }
                    }
                }
                return Ok(ToolOutput::err(format!(
                    "Patch failed on {r}: {e} — reverted {} file(s) already written",
                    applied.len()
                )));
            }
            applied.push((p.clone(), old.clone()));
            match new {
                Some(n) => {
                    let verb = if old.is_some() { "M" } else { "A" };
                    summary.push(format!("{verb} {r}"));
                    display.push(format!(
                        "{verb} {r}\n{}",
                        crate::ui::diff(old.as_deref().unwrap_or(""), n, 30)
                    ));
                }
                None => {
                    summary.push(format!("D {r}"));
                    display.push(format!("D {r}"));
                }
            }
        }
        Ok(ToolOutput::ok(format!(
            "Success. Updated the following files:\n{}",
            summary.join("\n")
        ))
        .display(display.join("\n")))
    }
}

fn rel(cwd: &Path, p: &Path) -> String {
    p.strip_prefix(cwd).unwrap_or(p).display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_apply() {
        let patch = "*** Begin Patch\n*** Update File: a.py\n@@ def f():\n     x = 1\n-    return x\n+    return x + 1\n*** Add File: b.txt\n+hello\n*** Delete File: c.txt\n*** End Patch";
        let ops = parse(patch).unwrap();
        assert_eq!(ops.len(), 3);
        let Op::Update { chunks, .. } = &ops[0] else {
            panic!()
        };
        let src = "def g():\n    return x\n\ndef f():\n    x = 1\n    return x\n";
        let out = apply_chunks(src, chunks, "a.py").unwrap();
        assert_eq!(
            out,
            "def g():\n    return x\n\ndef f():\n    x = 1\n    return x + 1\n"
        );
        assert_eq!(
            ops[1],
            Op::Add {
                path: "b.txt".into(),
                content: "hello\n".into()
            }
        );
        assert_eq!(paths(patch), vec!["a.py", "b.txt", "c.txt"]);
    }

    #[test]
    fn crlf_files_stay_crlf() {
        let chunks = vec![Chunk {
            header: None,
            old: vec!["b".into()],
            new: vec!["B".into()],
            eof: false,
        }];
        assert_eq!(
            apply_chunks("a\r\nb\r\nc\r\n", &chunks, "w.txt").unwrap(),
            "a\r\nB\r\nc\r\n"
        );
        // LF files are untouched by the same rule.
        assert_eq!(
            apply_chunks("a\nb\nc\n", &chunks, "u.txt").unwrap(),
            "a\nB\nc\n"
        );
    }

    #[test]
    fn whitespace_tolerant_and_errors() {
        let chunks = vec![Chunk {
            header: None,
            old: vec!["a  ".into(), "b".into()],
            new: vec!["a".into(), "c".into()],
            eof: false,
        }];
        assert_eq!(apply_chunks("a\nb\n", &chunks, "f").unwrap(), "a\nc\n");
        let add = vec![Chunk {
            header: None,
            old: vec![],
            new: vec!["z".into()],
            eof: false,
        }];
        assert_eq!(apply_chunks("a\nb\n", &add, "f").unwrap(), "a\nb\nz\n");
        let bad = vec![Chunk {
            header: None,
            old: vec!["zzz".into()],
            new: vec![],
            eof: false,
        }];
        assert!(apply_chunks("a\n", &bad, "f").is_err());
    }

    #[test]
    fn shell_heredoc() {
        let cmd = "apply_patch <<'EOF'\n*** Begin Patch\n*** Add File: x\n+1\n*** End Patch\nEOF";
        assert_eq!(
            from_shell(cmd).unwrap(),
            "*** Begin Patch\n*** Add File: x\n+1\n*** End Patch"
        );
        assert!(from_shell("apply_patches foo").is_none());
    }
}
