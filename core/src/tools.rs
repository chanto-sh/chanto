//! The file tools over an [`Fs`]: Read, Write, Edit, Glob, Grep and Ls as
//! the browser build runs them. Their descriptions and schemas are the
//! CLI's own (its tools take them from here), and what they return follows
//! the CLI's, message for message, for what a file system in memory can do:
//! no images, PDFs or language servers.

use crate::edit::{apply_edit, changed_snippet};
use crate::fs::Fs;
use crate::message::ToolSpec;
use crate::read::{not_utf8, read_window, render_notebook, similar_files};
use crate::search::GrepOptions;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Lines Read shows when not told how many.
pub const DEFAULT_LIMIT: usize = 2000;
/// Paths Glob lists, and Grep's default in path modes.
pub const MAX_RESULTS: usize = 200;
/// Entries Glob visits at most: an absolute `/**` would otherwise walk everything.
pub const WALK_BUDGET: usize = 400_000;

pub const READ_DESCRIPTION: &str = "Read a file from disk, numbered `cat -n` style (line number, tab, line). The default is the first \
2000 lines; pass offset/limit for big files. Handles images (png/jpg/gif/webp, shown to vision models), \
PDFs (text extracted) and notebooks. Read before editing, and fetch several files in parallel.";

pub fn read_schema() -> Value {
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

pub const WRITE_DESCRIPTION: &str = "Write a file (creating parent directories), overwriting any existing file. If the file exists you \
must Read it first. Prefer Edit for modifying existing files; use Write for new files or full rewrites.";

pub fn write_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "file_path": {"type": "string"},
            "content": {"type": "string"}
        },
        "required": ["file_path", "content"]
    })
}

pub const EDIT_DESCRIPTION: &str = "Replace an exact string in a file. You must Read the file first. old_string must match the file \
exactly (including indentation; do not include the line-number prefix from Read output) and be unique \
unless replace_all is true. Include enough surrounding lines to make it unique.";

pub fn edit_schema() -> Value {
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

pub const GLOB_DESCRIPTION: &str = "Find files by name pattern (`.gitignore` respected), e.g. `**/*.rs` or `src/**/*.{ts,tsx}`. \
Results come back newest-modified first.";

pub fn glob_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "description": "Glob pattern"},
            "path": {"type": "string", "description": "Directory to search (default: cwd)"}
        },
        "required": ["pattern"]
    })
}

pub const GREP_DESCRIPTION: &str = "Search file contents with a regex (ripgrep syntax; `.gitignore` respected). `output_mode` decides \
the result: `content` gives the matching lines (with `-n`, `-A`, `-B`, `-C` context), `files_with_matches` \
just the paths (default), `count` the totals. Narrow by `glob` (e.g. `*.rs`) or `type` (e.g. `rust`, `py`, `js`). \
Reach for this instead of grep/rg in Bash.";

pub fn grep_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "description": "Regular expression"},
            "path": {"type": "string", "description": "File or directory (default: cwd)"},
            "glob": {"type": "string", "description": "Glob filter, e.g. `*.{ts,tsx}`"},
            "type": {"type": "string", "description": "File type filter, e.g. rust, py, js, go"},
            "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"]},
            "-i": {"type": "boolean", "description": "Case insensitive"},
            "-n": {"type": "boolean", "description": "Show line numbers (content mode, default true)"},
            "-A": {"type": "integer"},
            "-B": {"type": "integer"},
            "-C": {"type": "integer"},
            "multiline": {"type": "boolean", "description": "Allow patterns to span lines"},
            "head_limit": {"type": "integer", "description": "Limit output to first N lines/entries"}
        },
        "required": ["pattern"]
    })
}

pub const LS_DESCRIPTION: &str =
    "List a directory as a tree (respects .gitignore), up to `depth` levels (default 2).";

pub fn ls_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "depth": {"type": "integer"}
        }
    })
}

/// The tools this module runs, as the model sees them.
pub fn specs() -> Vec<ToolSpec> {
    let spec = |name: &str, description: &str, parameters: Value| ToolSpec {
        name: name.into(),
        description: description.into(),
        parameters,
    };
    vec![
        spec("Read", READ_DESCRIPTION, read_schema()),
        spec("Write", WRITE_DESCRIPTION, write_schema()),
        spec("Edit", EDIT_DESCRIPTION, edit_schema()),
        spec("Glob", GLOB_DESCRIPTION, glob_schema()),
        spec("Grep", GREP_DESCRIPTION, grep_schema()),
        spec("Ls", LS_DESCRIPTION, ls_schema()),
    ]
}

/// What a tool hands back to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }
    fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// What the tools work on: the files, where relative paths start, and when
/// each file was last read (a file must be read, and not changed since,
/// before it is written).
pub struct Workspace<'a> {
    pub fs: &'a dyn Fs,
    pub cwd: PathBuf,
    pub read_times: HashMap<PathBuf, SystemTime>,
}

impl<'a> Workspace<'a> {
    pub fn new(fs: &'a dyn Fs, cwd: impl Into<PathBuf>) -> Self {
        Self {
            fs,
            cwd: cwd.into(),
            read_times: HashMap::new(),
        }
    }

    /// Run tool `name` with `args`.
    pub fn run(&mut self, name: &str, args: &Value) -> ToolOutput {
        let result = match name {
            "Read" => self.read(args),
            "Write" => self.write(args),
            "Edit" => self.edit(args),
            "Glob" => self.glob(args),
            "Grep" => self.grep(args),
            "Ls" => Ok(self.ls(args)),
            other => Err(format!("Unknown tool: {other}")),
        };
        result.unwrap_or_else(ToolOutput::err)
    }

    fn resolve(&self, p: &str) -> PathBuf {
        let p = Path::new(p);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cwd.join(p)
        }
    }

    fn rel(&self, p: &Path) -> String {
        p.strip_prefix(&self.cwd)
            .map(|r| r.display().to_string())
            .unwrap_or_else(|_| p.display().to_string())
    }

    fn mark_read(&mut self, p: &Path) {
        if let Some(t) = self.fs.metadata(p).ok().and_then(|m| m.modified) {
            self.read_times.insert(p.to_path_buf(), t);
        }
    }

    /// Why `path` can't be written yet: it exists and wasn't read, or it
    /// changed since.
    fn stale(&self, path: &Path) -> Option<String> {
        let now = self.fs.metadata(path).ok()?.modified;
        match self.read_times.get(path) {
            None => Some(format!(
                "You must Read {} before modifying it (so your edit is based on its current contents).",
                path.display()
            )),
            Some(t) if now.is_some_and(|m| m > *t) => Some(format!(
                "{} was modified since you last read it (by the user or a tool). Read it again before editing.",
                path.display()
            )),
            Some(_) => None,
        }
    }

    fn read_text(&self, path: &Path) -> Result<String, String> {
        let bytes = self.fs.read(path).map_err(|e| e.to_string())?;
        String::from_utf8(bytes).map_err(|e| not_utf8(path, e.as_bytes()))
    }

    fn read(&mut self, args: &Value) -> Result<ToolOutput, String> {
        let path = self.resolve(str_arg(args, "file_path")?);
        let Ok(meta) = self.fs.metadata(&path) else {
            let hint = similar_files(self.fs, &path);
            return Err(format!("File does not exist: {}{hint}", path.display()));
        };
        if meta.is_dir() {
            return Err(format!(
                "{} is a directory. Use Ls or Glob to list it.",
                path.display()
            ));
        }
        if !meta.is_file() {
            return Err(format!(
                "{} is not a regular file (a pipe, socket or device), so it can't be read.",
                path.display()
            ));
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp") {
            self.mark_read(&path);
            return Ok(ToolOutput::ok(format!(
                "[image file {} ({} bytes); the current model has no vision support]",
                path.display(),
                meta.len
            )));
        }
        if ext == "pdf" {
            return Err(format!(
                "Could not extract PDF text from {}.",
                path.display()
            ));
        }
        if ext == "ipynb" {
            let bytes = self.fs.read(&path).map_err(|e| e.to_string())?;
            self.mark_read(&path);
            return Ok(ToolOutput::ok(render_notebook(&bytes)));
        }
        let offset = arg_u64(args, "offset").unwrap_or(1).max(1) as usize;
        let limit = arg_u64(args, "limit").map_or(DEFAULT_LIMIT, |l| l as usize);
        let file = self.fs.open(&path).map_err(|e| e.to_string())?;
        let w = read_window(file, meta.len, offset, limit).map_err(|e| e.to_string())?;
        if w.binary {
            return Err(format!(
                "{} appears to be a binary file ({} bytes).",
                path.display(),
                w.size
            ));
        }
        self.mark_read(&path);
        if w.size == 0 {
            return Ok(ToolOutput::ok(
                "<system-reminder>File exists but is empty.</system-reminder>",
            ));
        }
        let (mut out, shown, total) = (w.out, w.shown, w.total);
        if offset > total {
            return Err(format!(
                "offset {offset} is beyond end of file ({total} lines)"
            ));
        }
        if offset - 1 + shown < total {
            out.push_str(&format!(
                "\n[showing lines {}-{} of {}; use offset/limit to read more]\n",
                offset,
                offset - 1 + shown,
                total
            ));
        }
        Ok(ToolOutput::ok(out))
    }

    fn write(&mut self, args: &Value) -> Result<ToolOutput, String> {
        let path = self.resolve(str_arg(args, "file_path")?);
        let content = str_arg(args, "content")?;
        if let Some(e) = self.stale(&path) {
            return Err(e);
        }
        let existed = self.fs.exists(&path);
        self.fs
            .write(&path, content.as_bytes())
            .map_err(|e| e.to_string())?;
        self.mark_read(&path);
        let n = content.lines().count();
        Ok(ToolOutput::ok(if existed {
            format!("File {} overwritten ({n} lines).", path.display())
        } else {
            format!("File created: {} ({n} lines).", path.display())
        }))
    }

    fn edit(&mut self, args: &Value) -> Result<ToolOutput, String> {
        let path = self.resolve(str_arg(args, "file_path")?);
        let old = str_arg(args, "old_string")?;
        let new = str_arg(args, "new_string")?;
        if !self.fs.exists(&path) {
            if old.is_empty() {
                self.fs
                    .write(&path, new.as_bytes())
                    .map_err(|e| e.to_string())?;
                self.mark_read(&path);
                return Ok(ToolOutput::ok(format!("Created {}", path.display())));
            }
            return Err(format!("File does not exist: {}", path.display()));
        }
        if let Some(e) = self.stale(&path) {
            return Err(e);
        }
        let original = self.read_text(&path)?;
        let (text, total) = apply_edit(&original, old, new, arg_bool(args, "replace_all"))?;
        self.fs
            .write(&path, text.as_bytes())
            .map_err(|e| e.to_string())?;
        self.mark_read(&path);
        Ok(ToolOutput::ok(format!(
            "The file {} has been updated ({total} replacement{}). Snippet of the result:\n{}",
            path.display(),
            if total == 1 { "" } else { "s" },
            changed_snippet(&original, &text)
        )))
    }

    fn glob(&self, args: &Value) -> Result<ToolOutput, String> {
        let mut root = args["path"]
            .as_str()
            .map_or(self.cwd.clone(), |p| self.resolve(p));
        let mut pattern = str_arg(args, "pattern")?.to_string();
        if pattern.starts_with('/') {
            // An absolute pattern: a literal directory root and a relative glob.
            let meta = pattern.find(['*', '?', '[', '{']).unwrap_or(pattern.len());
            let cut = pattern[..meta].rfind('/').unwrap_or(0);
            root = PathBuf::from(if cut == 0 { "/" } else { &pattern[..cut] });
            pattern = pattern[cut + 1..].to_string();
            if pattern.is_empty() {
                pattern = "**/*".into();
            }
        }
        let (files, total) =
            crate::search::glob(self.fs, &root, &pattern, MAX_RESULTS, WALK_BUDGET)
                .map_err(|e| e.to_string())?;
        Ok(ToolOutput::ok(glob_output(
            files.iter().map(|(p, _)| self.rel(p)),
            total,
        )))
    }

    fn grep(&self, args: &Value) -> Result<ToolOutput, String> {
        let pattern = str_arg(args, "pattern")?;
        let path = args["path"]
            .as_str()
            .map_or(self.cwd.clone(), |p| self.resolve(p));
        let mode = args["output_mode"]
            .as_str()
            .unwrap_or("files_with_matches")
            .to_string();
        let head = args["head_limit"]
            .as_u64()
            .map_or(if mode == "content" { 400 } else { MAX_RESULTS }, |n| {
                n as usize
            });
        let opts = grep_options(args, &mode, head);
        let (out, total) = crate::search::grep(self.fs, &path, pattern, &opts, &|p| self.rel(p))
            .map_err(|e| e.to_string())?;
        Ok(ToolOutput::ok(grep_output(&out, total, head, None)))
    }

    fn ls(&self, args: &Value) -> ToolOutput {
        let root = args["path"]
            .as_str()
            .map_or(self.cwd.clone(), |p| self.resolve(p));
        let depth = arg_u64(args, "depth").unwrap_or(2) as usize;
        ToolOutput::ok(crate::search::tree(self.fs, &root, depth))
    }
}

/// A boolean argument; models sometimes send `"true"`.
pub fn arg_bool(args: &Value, key: &str) -> bool {
    match args.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => false,
    }
}

/// A count argument; models sometimes send `5.0` or `"5"`.
pub fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    match args.get(key) {
        Some(Value::Number(n)) => n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args[key]
        .as_str()
        .ok_or_else(|| format!("missing string argument `{key}`"))
}

/// What Grep's arguments ask for, besides the pattern and path.
pub fn grep_options(args: &Value, mode: &str, head: usize) -> GrepOptions {
    let num = |k: &str| arg_u64(args, k);
    GrepOptions {
        mode: mode.to_string(),
        case_insensitive: arg_bool(args, "-i"),
        multiline: arg_bool(args, "multiline"),
        glob: args["glob"].as_str().map(str::to_string),
        file_type: args["type"].as_str().map(str::to_string),
        before: num("-B").or(num("-C")).unwrap_or(0) as usize,
        after: num("-A").or(num("-C")).unwrap_or(0) as usize,
        head,
    }
}

/// Glob's answer: the paths found, and how many more there were.
pub fn glob_output(paths: impl Iterator<Item = String>, total: usize) -> String {
    if total == 0 {
        return "No files found".into();
    }
    let mut out: Vec<String> = paths.collect();
    if total > MAX_RESULTS {
        out.push(format!(
            "… ({} more; use a more specific pattern)",
            total - MAX_RESULTS
        ));
    }
    out.join("\n")
}

/// Grep's answer: the first `head` lines of `out`, how many more there were,
/// and a warning about paths it couldn't search.
pub fn grep_output(out: &str, total: usize, head: usize, warn: Option<&str>) -> String {
    if total == 0 {
        return warn.unwrap_or("No matches found").to_string();
    }
    let mut s = out.lines().take(head).collect::<Vec<_>>().join("\n");
    if total > head {
        s.push_str(&format!(
            "\n… ({} more lines; narrow the search or raise head_limit)",
            total - head
        ));
    }
    if let Some(w) = warn {
        s.push_str(&format!("\n{w}"));
    }
    crate::text::truncate(&s, 60_000)
}

#[cfg(test)]
mod tools_tests;
