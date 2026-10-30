//! File tools: Read, Write, Edit, MultiEdit.

use super::{Ctx, Tool, ToolKind, ToolOutput, arg_bool, arg_str, arg_u64};
use crate::util::{rel, resolve};
use anyhow::{Result, bail};
use async_trait::async_trait;
use base64::Engine;
use chanto_core::edit::{apply_edit, changed_snippet, count_changes};
use chanto_core::fs::NativeFs;
use chanto_core::read::{MAX_READ_BYTES, not_utf8, read_window, render_notebook, similar_files};
use serde_json::{Value, json};
use std::path::Path;

const DEFAULT_LIMIT: usize = 2000;

/// Each (file, offset, limit) with the file's modification time and size.
type ReadLogMap =
    std::collections::HashMap<(std::path::PathBuf, usize, usize), (std::time::SystemTime, u64)>;

/// What one agent has read and still has in its conversation: each (file,
/// offset, limit) with the file's modification time and size at the time.
/// Reading the same lines of an unchanged file again gets a note instead of
/// the same text; cleared whenever the conversation loses tool output
/// (compaction, clearing old results, rewind, a new session).
#[derive(Default)]
pub struct ReadLog(std::sync::Mutex<ReadLogMap>);

impl ReadLog {
    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
    fn unchanged(
        &self,
        key: &(std::path::PathBuf, usize, usize),
        stamp: (std::time::SystemTime, u64),
    ) -> bool {
        self.0.lock().unwrap().get(key) == Some(&stamp)
    }
    fn note(&self, key: (std::path::PathBuf, usize, usize), stamp: (std::time::SystemTime, u64)) {
        self.0.lock().unwrap().insert(key, stamp);
    }
}

fn path_label(name: &str, args: &Value, ctx_cwd: Option<&Path>) -> String {
    let p = args["file_path"]
        .as_str()
        .or(args["path"].as_str())
        .unwrap_or("?");
    let shown = match ctx_cwd {
        Some(cwd) => rel(cwd, &resolve(cwd, p)),
        None => p.to_string(),
    };
    format!("{name}({shown})")
}

fn cwd_now() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_default()
}

pub struct Read;

#[async_trait]
impl Tool for Read {
    fn name(&self) -> String {
        "Read".into()
    }
    fn description(&self) -> String {
        "Read a file from disk, numbered `cat -n` style (line number, tab, line). The default is the first \
2000 lines; pass offset/limit for big files. Handles images (png/jpg/gif/webp, shown to vision models), \
PDFs (text extracted) and notebooks. Read before editing, and fetch several files in parallel."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string", "description": "Absolute or project-relative path"},
                "offset": {"type": "integer", "description": "1-based line number to start from"},
                "limit": {"type": "integer", "description": "Number of lines to read"}
            },
            "required": ["file_path"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }
    fn rule_subjects(&self, args: &Value) -> Vec<String> {
        vec![args["file_path"].as_str().unwrap_or("").to_string()]
    }
    fn label(&self, args: &Value) -> String {
        path_label("Read", args, Some(&cwd_now()))
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let cwd = ctx.cwd();
        let path = resolve(&cwd, arg_str(&args, "file_path")?);
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let client_text = if matches!(
            ext.as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "pdf"
        ) {
            None
        } else {
            ctx.env.frontend.read_text_file(&path).await
        };
        if let Some(text) = client_text {
            let text = text?;
            ctx.env.mark_read(&path);
            let offset = arg_u64(&args, "offset").unwrap_or(1).max(1) as usize;
            let limit = arg_u64(&args, "limit").unwrap_or(DEFAULT_LIMIT as u64) as usize;
            if ext == "ipynb" {
                return Ok(ToolOutput::ok(render_notebook(text.as_bytes())));
            }
            let out: String = text
                .lines()
                .enumerate()
                .skip(offset - 1)
                .take(limit)
                .map(|(i, l)| format!("{}\t{}\n", i + 1, l))
                .collect();
            return Ok(ToolOutput::ok(crate::util::truncate(&out, MAX_READ_BYTES)));
        }
        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(_) => {
                let hint = similar_files(&NativeFs, &path);
                return Ok(ToolOutput::err(format!(
                    "File does not exist: {}{hint}",
                    path.display()
                )));
            }
        };
        if meta.is_dir() {
            return Ok(ToolOutput::err(format!(
                "{} is a directory. Use Ls or Glob to list it.",
                path.display()
            )));
        }
        // A named pipe blocks the read until something writes to it (forever,
        // in a repository that ships one); devices and sockets aren't files either.
        if !meta.is_file() {
            return Ok(ToolOutput::err(format!(
                "{} is not a regular file (a pipe, socket or device), so it can't be read.",
                path.display()
            )));
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        // Images and notebooks are read whole (base64-encoded / JSON-parsed) and
        // a PDF's extracted text is buffered in memory, none of them windowed the
        // way the plain-text path is. Read runs in every mode with no prompt, so a
        // prompt-injected `Read` of a crafted multi-gigabyte file of one of these
        // types could exhaust memory; cap the size these paths will pull in.
        const MAX_BLOB_BYTES: u64 = 20 * 1024 * 1024;
        if matches!(
            ext.as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "pdf" | "ipynb"
        ) && meta.len() > MAX_BLOB_BYTES
        {
            return Ok(ToolOutput::err(format!(
                "{} is {} bytes; a {} file is read in full, which is capped at {} MiB.",
                path.display(),
                meta.len(),
                ext,
                MAX_BLOB_BYTES / (1024 * 1024)
            )));
        }
        if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp") {
            let bytes = tokio::fs::read(&path).await?;
            ctx.env.mark_read(&path);
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            if !ctx.env.vision() {
                return Ok(ToolOutput::ok(format!(
                    "[image file {} ({} bytes); the current model has no vision support]",
                    path.display(),
                    bytes.len()
                )));
            }
            return Ok(ToolOutput {
                content: format!(
                    "Image file {} ({} bytes) attached.",
                    path.display(),
                    bytes.len()
                ),
                images: vec![b64],
                ..Default::default()
            }
            .display(format!("Read image ({} KB)", bytes.len() / 1024)));
        }
        if ext == "pdf" {
            let out = tokio::process::Command::new("pdftotext")
                .args(["-layout", &path.to_string_lossy(), "-"])
                .output()
                .await;
            return Ok(match out {
                Ok(o) if o.status.success() => {
                    ctx.env.mark_read(&path);
                    let text = String::from_utf8_lossy(&o.stdout).to_string();
                    let n = text.lines().count();
                    ToolOutput::ok(crate::util::truncate(&text, MAX_READ_BYTES))
                        .display(format!("Read PDF ({})", crate::util::plural(n, "line")))
                }
                _ => ToolOutput::err(
                    "Could not extract PDF text (install poppler: `brew install poppler`).",
                ),
            });
        }
        if ext == "ipynb" {
            let bytes = tokio::fs::read(&path).await?;
            ctx.env.mark_read(&path);
            return Ok(ToolOutput::ok(render_notebook(&bytes)));
        }
        let offset = arg_u64(&args, "offset").unwrap_or(1).max(1) as usize;
        let limit = arg_u64(&args, "limit")
            .map(|l| l as usize)
            .unwrap_or(DEFAULT_LIMIT);
        let key = (crate::permissions::normalize(&path), offset, limit);
        let stamp = meta.modified().ok().map(|t| (t, meta.len()));
        if let (Some(log), Some(stamp)) = (&ctx.reads, stamp)
            && log.unchanged(&key, stamp)
        {
            ctx.env.mark_read(&path);
            return Ok(ToolOutput::ok(format!(
                "<system-reminder>{} hasn't changed since you read these lines earlier in this conversation; that output is still current, so use it rather than reading again.</system-reminder>",
                path.display()
            ))
            .display("Unchanged since last read"));
        }
        // Stream the file: only the requested lines are kept, so a small
        // window into a huge log doesn't load the whole file.
        let p2 = path.clone();
        let w = tokio::task::spawn_blocking(move || {
            let f = std::fs::File::open(&p2)?;
            let size = f.metadata()?.len();
            read_window(f, size, offset, limit)
        })
        .await??;
        if w.binary {
            return Ok(ToolOutput::err(format!(
                "{} appears to be a binary file ({} bytes).",
                path.display(),
                w.size
            )));
        }
        ctx.env.mark_read(&path);
        if let (Some(log), Some(stamp)) = (&ctx.reads, stamp) {
            log.note(key, stamp);
        }
        if w.size == 0 {
            return Ok(ToolOutput::ok(
                "<system-reminder>File exists but is empty.</system-reminder>",
            )
            .display("Read 0 lines"));
        }
        let (mut out, shown, total) = (w.out, w.shown, w.total);
        if offset > total {
            return Ok(ToolOutput::err(format!(
                "offset {offset} is beyond end of file ({total} lines)"
            )));
        }
        if offset - 1 + shown < total {
            out.push_str(&format!(
                "\n[showing lines {}-{} of {}; use offset/limit to read more]\n",
                offset,
                offset - 1 + shown,
                total
            ));
        }
        Ok(ToolOutput::ok(out).display(format!("Read {}", crate::util::plural(shown, "line"))))
    }
}

/// Ensure the model has read the current version of a file before modifying it,
/// and that another Chanto session isn't working on it right now.
fn check_fresh(ctx: &Ctx, path: &Path) -> Option<String> {
    stale(ctx, path).or_else(|| crate::presence::claim(path))
}

fn stale(ctx: &Ctx, path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    match ctx.env.read_time(path) {
        None => Some(format!(
            "You must Read {} before modifying it (so your edit is based on its current contents).",
            path.display()
        )),
        Some(t) => {
            let m = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
            if m > t {
                Some(format!(
                    "{} was modified since you last read it (by the user or a tool). Read it again before editing.",
                    path.display()
                ))
            } else {
                None
            }
        }
    }
}

pub struct Write;

#[async_trait]
impl Tool for Write {
    fn name(&self) -> String {
        "Write".into()
    }
    fn description(&self) -> String {
        "Write a file (creating parent directories), overwriting any existing file. If the file exists you \
must Read it first. Prefer Edit for modifying existing files; use Write for new files or full rewrites."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["file_path", "content"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }
    fn rule_subjects(&self, args: &Value) -> Vec<String> {
        vec![args["file_path"].as_str().unwrap_or("").to_string()]
    }
    fn label(&self, args: &Value) -> String {
        path_label("Write", args, Some(&cwd_now()))
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let cwd = ctx.cwd();
        let path = resolve(&cwd, arg_str(&args, "file_path")?);
        let content = arg_str(&args, "content")?;
        if let Some(e) = check_fresh(ctx, &path) {
            return Ok(ToolOutput::err(e));
        }
        let old = read_text(ctx, &path).await.ok();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        write_text(ctx, &path, old.as_deref(), content).await?;
        ctx.env.mark_read(&path);
        let n = content.lines().count();
        let display = match &old {
            Some(o) => format!(
                "Updated {} ({})\n{}",
                rel(&cwd, &path),
                crate::util::plural(n, "line"),
                crate::ui::diff(o, content, 30)
            ),
            None => format!(
                "Wrote {} to {}",
                crate::util::plural(n, "line"),
                rel(&cwd, &path)
            ),
        };
        let msg = if old.is_some() {
            format!("File {} overwritten ({n} lines).", path.display())
        } else {
            format!("File created: {} ({n} lines).", path.display())
        };
        Ok(ToolOutput::ok(msg).display(display))
    }
}

pub struct Edit;

#[async_trait]
impl Tool for Edit {
    fn name(&self) -> String {
        "Edit".into()
    }
    fn description(&self) -> String {
        "Replace an exact string in a file. You must Read the file first. old_string must match the file \
exactly (including indentation; do not include the line-number prefix from Read output) and be unique \
unless replace_all is true. Include enough surrounding lines to make it unique."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
                "old_string": {"type": "string", "description": "Exact text to replace"},
                "new_string": {"type": "string", "description": "Replacement text"},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)"}
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }
    fn rule_subjects(&self, args: &Value) -> Vec<String> {
        vec![args["file_path"].as_str().unwrap_or("").to_string()]
    }
    fn label(&self, args: &Value) -> String {
        path_label("Edit", args, Some(&cwd_now()))
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let edits = vec![(
            arg_str(&args, "old_string")?.to_string(),
            arg_str(&args, "new_string")?.to_string(),
            arg_bool(&args, "replace_all"),
        )];
        edit_file(ctx, arg_str(&args, "file_path")?, edits).await
    }
}

pub struct MultiEdit;

#[async_trait]
impl Tool for MultiEdit {
    fn name(&self) -> String {
        "MultiEdit".into()
    }
    fn description(&self) -> String {
        "Apply several Edit-style replacements to one file atomically, in order. All must succeed or none are applied.".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
                "edits": {"type": "array", "items": {
                    "type": "object",
                    "properties": {
                        "old_string": {"type": "string"},
                        "new_string": {"type": "string"},
                        "replace_all": {"type": "boolean"}
                    },
                    "required": ["old_string", "new_string"]
                }}
            },
            "required": ["file_path", "edits"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }
    fn rule_subjects(&self, args: &Value) -> Vec<String> {
        vec![args["file_path"].as_str().unwrap_or("").to_string()]
    }
    fn label(&self, args: &Value) -> String {
        path_label("MultiEdit", args, Some(&cwd_now()))
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let edits = args["edits"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("`edits` must be an array"))?
            .iter()
            .map(|e| {
                Ok((
                    arg_str(e, "old_string")?.to_string(),
                    arg_str(e, "new_string")?.to_string(),
                    arg_bool(e, "replace_all"),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        edit_file(ctx, arg_str(&args, "file_path")?, edits).await
    }
}

async fn edit_file(ctx: &Ctx, p: &str, edits: Vec<(String, String, bool)>) -> Result<ToolOutput> {
    let cwd = ctx.cwd();
    let path = resolve(&cwd, p);
    if !path.exists() {
        if edits.len() == 1 && edits[0].0.is_empty() {
            // Creating a new file via Edit with empty old_string.
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            ctx.env.snapshot_file(&path);
            crate::util::write_atomic(&path, edits[0].1.as_bytes()).await?;
            ctx.env.mark_read(&path);
            return Ok(ToolOutput::ok(format!("Created {}", path.display())));
        }
        bail!("File does not exist: {}", path.display());
    }
    if let Some(e) = check_fresh(ctx, &path) {
        return Ok(ToolOutput::err(e));
    }
    let original = read_text(ctx, &path).await?;
    let mut text = original.clone();
    let mut total = 0;
    for (i, (old, new, all)) in edits.iter().enumerate() {
        match apply_edit(&text, old, new, *all) {
            Ok((t, n)) => {
                text = t;
                total += n;
            }
            Err(e) => {
                let which = if edits.len() > 1 {
                    format!("Edit #{}: ", i + 1)
                } else {
                    String::new()
                };
                return Ok(ToolOutput::err(format!("{which}{e}")));
            }
        }
    }
    write_text(ctx, &path, Some(&original), &text).await?;
    ctx.env.mark_read(&path);
    // Show the model a small snippet around the change so it can verify.
    let snippet = changed_snippet(&original, &text);
    let (adds, dels) = count_changes(&original, &text);
    let display = format!(
        "Updated {} with {adds} addition{} and {dels} removal{}\n{}",
        rel(&cwd, &path),
        if adds == 1 { "" } else { "s" },
        if dels == 1 { "" } else { "s" },
        crate::ui::diff(&original, &text, 40)
    );
    Ok(ToolOutput::ok(format!(
        "The file {} has been updated ({total} replacement{}). Snippet of the result:\n{snippet}",
        path.display(),
        if total == 1 { "" } else { "s" }
    ))
    .display(display))
}

pub struct NotebookEdit;

#[async_trait]
impl Tool for NotebookEdit {
    fn name(&self) -> String {
        "NotebookEdit".into()
    }
    fn description(&self) -> String {
        "Edit a Jupyter notebook (.ipynb) cell by 0-based index: edit_mode `replace` (default) replaces the cell source, `insert` inserts a new cell before the index (or at the end if index == number of cells), `delete` removes it. Read the notebook first.".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "notebook_path": {"type": "string"},
                "cell_index": {"type": "integer"},
                "new_source": {"type": "string"},
                "cell_type": {"type": "string", "enum": ["code", "markdown"]},
                "edit_mode": {"type": "string", "enum": ["replace", "insert", "delete"]}
            },
            "required": ["notebook_path", "cell_index"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }
    fn rule_subjects(&self, args: &Value) -> Vec<String> {
        vec![args["notebook_path"].as_str().unwrap_or("").to_string()]
    }
    fn label(&self, args: &Value) -> String {
        format!(
            "NotebookEdit({} #{})",
            args["notebook_path"].as_str().unwrap_or(""),
            args["cell_index"]
        )
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let cwd = ctx.cwd();
        let path = resolve(&cwd, arg_str(&args, "notebook_path")?);
        if let Some(e) = check_fresh(ctx, &path) {
            return Ok(ToolOutput::err(e));
        }
        let original = read_text(ctx, &path).await?;
        let updated = notebook_preview(&original, &args)?;
        write_text(ctx, &path, Some(&original), &updated).await?;
        let mode = args["edit_mode"].as_str().unwrap_or("replace");
        let idx = args["cell_index"].as_u64().unwrap_or(0);
        ctx.env.mark_read(&path);
        Ok(ToolOutput::ok(format!(
            "Notebook {} updated ({mode} cell {idx}).",
            path.display()
        )))
    }
}

pub(crate) async fn read_text(ctx: &Ctx, path: &Path) -> Result<String> {
    match ctx.env.frontend.read_text_file(path).await {
        Some(r) => r,
        None => {
            let bytes = tokio::fs::read(path).await?;
            String::from_utf8(bytes).map_err(|e| anyhow::anyhow!(not_utf8(path, e.as_bytes())))
        }
    }
}

pub(crate) async fn write_text(
    ctx: &Ctx,
    path: &Path,
    old: Option<&str>,
    text: &str,
) -> Result<()> {
    // Capture editor contents, including unsaved changes, for rewind.
    if let Some(old) = old
        && let Some(cp) = ctx.env.checkpoints.lock().unwrap().last_mut()
    {
        cp.files
            .entry(crate::permissions::normalize(path))
            .or_insert_with(|| Some(old.as_bytes().to_vec()));
    }
    if old.is_none() {
        ctx.env.snapshot_file(path);
    }
    match ctx.env.frontend.write_text_file(path, text).await {
        Some(r) => r?,
        None => crate::util::write_atomic(path, text.as_bytes()).await?,
    }
    ctx.env
        .frontend
        .file_diff(ctx.call_id.as_deref().unwrap_or(""), path, old, text);
    Ok(())
}

pub(crate) fn notebook_preview(original: &str, args: &Value) -> Result<String> {
    let mut nb: Value = serde_json::from_str(original)?;
    let idx = arg_u64(args, "cell_index").unwrap_or(0) as usize;
    let mode = args["edit_mode"].as_str().unwrap_or("replace");
    let cells = nb["cells"]
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("not a notebook: no cells array"))?;
    let source = |s: &str| {
        Value::Array(
            s.split_inclusive('\n')
                .map(|l| Value::String(l.to_string()))
                .collect(),
        )
    };
    match mode {
        "delete" => {
            if idx >= cells.len() {
                return Err(anyhow::anyhow!(format!(
                    "cell_index {idx} out of range ({} cells)",
                    cells.len()
                )));
            }
            cells.remove(idx);
        }
        "insert" => {
            if idx > cells.len() {
                return Err(anyhow::anyhow!(format!(
                    "cell_index {idx} out of range ({} cells)",
                    cells.len()
                )));
            }
            let ty = args["cell_type"].as_str().unwrap_or("code");
            let mut cell = json!({"cell_type": ty, "metadata": {}, "source": source(args["new_source"].as_str().unwrap_or(""))});
            if ty == "code" {
                cell["outputs"] = json!([]);
                cell["execution_count"] = Value::Null;
            }
            cells.insert(idx, cell);
        }
        _ => {
            let Some(cell) = cells.get_mut(idx) else {
                return Err(anyhow::anyhow!(format!("cell_index {idx} out of range")));
            };
            cell["source"] = source(arg_str(args, "new_source")?);
            if let Some(t) = args["cell_type"].as_str() {
                cell["cell_type"] = json!(t);
            }
            if cell["cell_type"] == "code" {
                cell["outputs"] = json!([]);
                cell["execution_count"] = Value::Null;
            }
        }
    }
    Ok(serde_json::to_string_pretty(&nb)? + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn reading_a_named_pipe_is_refused_rather_than_hanging() {
        let dir = std::env::temp_dir().join(format!("chanto-fifo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(dir.join("fifo"))
                .status()
                .unwrap()
                .success()
        );
        let env = crate::agent::tests::make_env(
            "http://localhost:1",
            &dir,
            crate::permissions::Mode::Default,
            32768,
        );
        let ctx = Ctx {
            env,
            cancel: tokio_util::sync::CancellationToken::new(),
            depth: 0,
            call_id: None,
            cwd_override: None,
            reads: None,
        };
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            Read.run(
                json!({"file_path": dir.join("fifo").display().to_string()}),
                &ctx,
            ),
        )
        .await
        .expect("Read of a FIFO hung")
        .unwrap();
        assert!(
            out.is_error && out.content.contains("not a regular file"),
            "{}",
            out.content
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_oversize_image_is_refused_rather_than_read_whole() {
        let dir = std::env::temp_dir().join(format!("chanto-blob-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // A sparse 21 MiB file: over the 20 MiB whole-file cap, no real disk use.
        let big = dir.join("huge.png");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(21 * 1024 * 1024).unwrap();
        let env = crate::agent::tests::make_env(
            "http://localhost:1",
            &dir,
            crate::permissions::Mode::Default,
            32768,
        );
        let ctx = Ctx {
            env,
            cancel: tokio_util::sync::CancellationToken::new(),
            depth: 0,
            call_id: None,
            cwd_override: None,
            reads: None,
        };
        let out = Read
            .run(json!({"file_path": big.display().to_string()}), &ctx)
            .await
            .unwrap();
        assert!(
            out.is_error && out.content.contains("capped at 20 MiB"),
            "{}",
            out.content
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn editing_a_non_utf8_file_names_the_encoding() {
        let dir = std::env::temp_dir().join(format!("chanto-enc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let env = crate::agent::tests::make_env(
            "http://localhost:1",
            &dir,
            crate::permissions::Mode::Default,
            32768,
        );
        let ctx = Ctx {
            env: env.clone(),
            cancel: tokio_util::sync::CancellationToken::new(),
            depth: 0,
            call_id: None,
            cwd_override: None,
            reads: None,
        };
        for (name, bytes, want) in [
            ("latin1.txt", b"caf\xe9 au lait\n".to_vec(), "Latin-1"),
            ("utf16.txt", vec![0xFF, 0xFE, b'h', 0, b'i', 0], "UTF-16 LE"),
        ] {
            let file = dir.join(name);
            std::fs::write(&file, &bytes).unwrap();
            env.mark_read(&file);
            let err = Edit.run(json!({"file_path": file.display().to_string(), "old_string": "caf", "new_string": "tea"}), &ctx).await.unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("isn't UTF-8 text") && msg.contains(want) && msg.contains("iconv"),
                "{msg}"
            );
            assert_eq!(
                std::fs::read(&file).unwrap(),
                bytes,
                "the file is untouched"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_unchanged_file_is_not_sent_twice() {
        let dir = std::env::temp_dir().join(format!("chanto-reads-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "one\ntwo\n").unwrap();
        let env = crate::agent::tests::make_env(
            "http://localhost:1",
            &dir,
            crate::permissions::Mode::Default,
            32768,
        );
        let log = std::sync::Arc::new(ReadLog::default());
        let ctx = Ctx {
            env: env.clone(),
            cancel: tokio_util::sync::CancellationToken::new(),
            depth: 0,
            call_id: None,
            cwd_override: None,
            reads: Some(log.clone()),
        };
        let read = |args: Value| async { Read.run(args, &ctx).await.unwrap().content };
        let path = file.display().to_string();
        assert!(read(json!({"file_path": path})).await.contains("two"));
        assert!(
            read(json!({"file_path": path}))
                .await
                .contains("hasn't changed"),
            "same lines, same file"
        );
        assert!(
            read(json!({"file_path": path, "offset": 2}))
                .await
                .contains("two"),
            "other lines are read"
        );
        // A change, or a conversation that lost the output, reads it again.
        std::fs::write(&file, "one\ntwo\nthree\n").unwrap();
        assert!(read(json!({"file_path": path})).await.contains("three"));
        log.clear();
        assert!(read(json!({"file_path": path})).await.contains("three"));
        // Outside an agent's turn, always the text.
        let plain = Ctx {
            env,
            cancel: tokio_util::sync::CancellationToken::new(),
            depth: 0,
            call_id: None,
            cwd_override: None,
            reads: None,
        };
        assert!(
            Read.run(json!({"file_path": path}), &plain)
                .await
                .unwrap()
                .content
                .contains("three")
        );
        assert!(
            Read.run(json!({"file_path": path}), &plain)
                .await
                .unwrap()
                .content
                .contains("three")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
