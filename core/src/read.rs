//! Read's text work: a window of lines from a stream, notebooks, and what
//! to say when a file is missing or isn't text.

use crate::fs::Fs;
use serde_json::Value;
use std::path::Path;

/// The longest line shown whole; longer ones are cut and say so.
pub const MAX_LINE: usize = 2000;
/// The most text one Read returns.
pub const MAX_READ_BYTES: usize = 256 * 1024;

/// Lines `offset..offset+limit` of a text file, formatted like `cat -n`.
pub struct Window {
    pub out: String,
    pub shown: usize,
    pub total: usize,
    pub binary: bool,
    pub size: u64,
}

/// Read a window of lines without holding the file in memory: reads in
/// chunks, keeps at most MAX_LINE bytes of each wanted line, and counts the
/// rest; `size` is the whole file's. Same lines as `str::lines` (a trailing newline doesn't start a new
/// line; `\r\n` endings are stripped).
pub fn read_window(
    mut f: impl std::io::Read,
    size: u64,
    offset: usize,
    limit: usize,
) -> std::io::Result<Window> {
    let mut w = Window {
        out: String::new(),
        shown: 0,
        total: 0,
        binary: false,
        size,
    };
    let mut buf = vec![0u8; 64 * 1024];
    let first = (offset - 1, (offset - 1).saturating_add(limit));
    let mut cur: Vec<u8> = Vec::new();
    let mut dropped = 0usize; // bytes of the current line beyond the cap
    let mut last = 0u8; // the current line's last byte so far
    let (mut pending, mut stopped, mut checked) = (false, false, false);
    let mut line = 0usize; // 0-based index of the line being read
    let emit = |w: &mut Window, bytes: &[u8], idx: usize, stopped: &mut bool, dropped: usize| {
        if idx < first.0 || idx >= first.1 || *stopped {
            return;
        }
        if w.out.len() > MAX_READ_BYTES {
            w.out.push_str(&format!(
                "… [output limit reached at line {}; use offset/limit to read more]\n",
                idx + 1
            ));
            *stopped = true;
            return;
        }
        let text = String::from_utf8_lossy(bytes);
        let total = bytes.len() + dropped;
        // Say when a line was cut: a silent cut makes the model copy partial
        // text into an Edit, which then fails to match.
        let text = if text.len() > MAX_LINE {
            truncated_line(&text, total)
        } else {
            text
        };
        w.out.push_str(&format!("{}\t{}\n", idx + 1, text));
        w.shown += 1;
    };
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        if !checked {
            // The same binary check as before: a NUL in the first 8000 bytes.
            checked = true;
            if chunk.iter().take(8000).any(|&b| b == 0) {
                w.binary = true;
                return Ok(w);
            }
        }
        let mut start = 0;
        while start < chunk.len() {
            let wanted = line >= first.0 && line < first.1 && !stopped;
            match memchr::memchr(b'\n', &chunk[start..]) {
                Some(i) => {
                    if wanted {
                        let take = (MAX_LINE + 4).saturating_sub(cur.len()).min(i);
                        cur.extend_from_slice(&chunk[start..start + take]);
                        dropped += i - take;
                    }
                    if i > 0 {
                        last = chunk[start + i - 1];
                    }
                    // CRLF: the `\r` is not part of the line.
                    if wanted && last == b'\r' {
                        if dropped > 0 {
                            dropped -= 1
                        } else {
                            cur.pop();
                        }
                    }
                    last = 0;
                    emit(&mut w, &cur, line, &mut stopped, dropped);
                    cur.clear();
                    dropped = 0;
                    line += 1;
                    pending = false;
                    start += i + 1;
                }
                None => {
                    if wanted {
                        let take = (MAX_LINE + 4)
                            .saturating_sub(cur.len())
                            .min(chunk.len() - start);
                        cur.extend_from_slice(&chunk[start..start + take]);
                        dropped += (chunk.len() - start) - take;
                    }
                    last = chunk[chunk.len() - 1];
                    pending = true;
                    start = chunk.len();
                }
            }
        }
    }
    if pending {
        emit(&mut w, &cur, line, &mut stopped, dropped);
        line += 1;
    }
    w.total = line;
    Ok(w)
}

/// The first MAX_LINE bytes of an over-long line, marked as cut.
pub fn truncated_line(text: &str, total: usize) -> std::borrow::Cow<'_, str> {
    let cut = floor_char(text, MAX_LINE);
    format!("{}… [line truncated: {cut} of {total} bytes]", &text[..cut]).into()
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// A note naming files close to `path`, which doesn't exist (empty when none are).
pub fn similar_files(fs: &dyn Fs, path: &Path) -> String {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return String::new();
    };
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut cands = read_names(fs, dir, false);
    // Compare against the file name, or the directory name when the directory
    // itself is missing (`src/agentss/mod.rs` → suggest `agents/`).
    let mut key = name.to_string();
    if cands.is_empty()
        && let Some(parent) = dir.parent()
    {
        cands = read_names(fs, parent, true);
        key = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(name)
            .to_string();
    }
    let q = key.to_ascii_lowercase();
    let limit = (q.len() / 3).clamp(1, 4);
    let mut scored: Vec<(usize, String)> = cands
        .into_iter()
        .filter(|c| c != name)
        .filter_map(|c| {
            let cl = c.trim_end_matches('/').to_ascii_lowercase();
            let d = levenshtein(&cl, &q);
            (d <= limit || cl.contains(&q) || q.contains(&cl)).then_some((d, c))
        })
        .collect();
    scored.sort();
    scored.truncate(5);
    if scored.is_empty() {
        String::new()
    } else {
        format!(
            "\nDid you mean one of: {}?",
            scored
                .into_iter()
                .map(|(_, c)| c)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

fn read_names(fs: &dyn Fs, dir: &Path, dirs_only: bool) -> Vec<String> {
    let Ok(entries) = fs.read_dir(dir) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter(|e| !dirs_only || e.is_dir)
        .map(|e| {
            if dirs_only {
                format!("{}/", e.name)
            } else {
                e.name
            }
        })
        .collect()
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// A notebook's cells and their text output, for reading.
pub fn render_notebook(bytes: &[u8]) -> String {
    let v: Value = serde_json::from_slice(bytes).unwrap_or_default();
    let mut out = String::new();
    for (i, cell) in v["cells"].as_array().into_iter().flatten().enumerate() {
        let src = match &cell["source"] {
            Value::Array(a) => a.iter().filter_map(|s| s.as_str()).collect::<String>(),
            Value::String(s) => s.clone(),
            _ => String::new(),
        };
        out.push_str(&format!(
            "<cell {} type={}>\n{}\n",
            i,
            cell["cell_type"].as_str().unwrap_or("?"),
            src
        ));
        for o in cell["outputs"].as_array().into_iter().flatten() {
            if let Some(t) = o["text"].as_array() {
                out.push_str("<output>\n");
                out.push_str(&t.iter().filter_map(|s| s.as_str()).collect::<String>());
                out.push_str("\n</output>\n");
            }
        }
        out.push_str("</cell>\n");
    }
    crate::text::truncate(&out, MAX_READ_BYTES)
}

/// Why a file can't be edited as text, with a likely encoding and a
/// conversion the model can run.
pub fn not_utf8(path: &Path, b: &[u8]) -> String {
    let (enc, iconv) = if b.starts_with(&[0xFF, 0xFE]) {
        (
            "UTF-16 LE (it starts with a UTF-16 byte-order mark)",
            Some("UTF-16"),
        )
    } else if b.starts_with(&[0xFE, 0xFF]) {
        (
            "UTF-16 BE (it starts with a UTF-16 byte-order mark)",
            Some("UTF-16"),
        )
    } else if b.iter().take(8192).any(|&c| c == 0) {
        (
            "binary, or UTF-16 without a byte-order mark (it contains NUL bytes)",
            None,
        )
    } else {
        ("probably Latin-1 / Windows-1252", Some("WINDOWS-1252"))
    };
    let p = path.display();
    let hint = match iconv {
        Some(from) => format!(
            " To edit it, convert it first with Bash, e.g. `iconv -f {from} -t UTF-8 '{p}' > '{p}.utf8' && mv '{p}.utf8' '{p}'` (this changes the file's encoding), or make the change with a script that keeps the encoding."
        ),
        None => " Edit can't change it; use Bash if it must be modified.".to_string(),
    };
    format!("{p} isn't UTF-8 text: {enc}.{hint}")
}

#[cfg(test)]
mod read_tests;
