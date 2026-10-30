//! Edit and MultiEdit's text work: finding the text to replace, and
//! describing the change.

/// Apply a single replacement, returning the new text or a model-facing error.
pub fn apply_edit(
    text: &str,
    old: &str,
    new: &str,
    all: bool,
) -> std::result::Result<(String, usize), String> {
    if old == new {
        return Err("old_string and new_string are identical; nothing to change.".into());
    }
    if old.is_empty() {
        return Err("old_string must not be empty (use Write to create a file).".into());
    }
    let count = text.matches(old).count();
    if count == 0 {
        // Tolerate CRLF files and trailing-whitespace drift.
        if text.contains("\r\n") && !old.contains('\r') {
            let o = old.replace('\n', "\r\n");
            if text.contains(&o) {
                return apply_edit(text, &o, &new.replace('\n', "\r\n"), all);
            }
        }
        if let Some((s, e)) = fuzzy_find(text, old) {
            let mut out = String::with_capacity(text.len() + new.len());
            out.push_str(&text[..s]);
            out.push_str(new);
            out.push_str(&text[e..]);
            return Ok((out, 1));
        }
        return Err("old_string not found in file. Make sure it matches the file contents exactly, including whitespace and indentation (Read the file again if unsure).".into());
    }
    if count > 1 && !all {
        return Err(format!(
            "old_string matches {count} locations. Provide more surrounding context to make it unique, or set replace_all=true."
        ));
    }
    Ok((
        if all {
            text.replace(old, new)
        } else {
            text.replacen(old, new, 1)
        },
        count,
    ))
}

/// Line-based match ignoring trailing whitespace and uniform indentation
/// differences. Returns a byte range only if the match is unique.
fn fuzzy_find(text: &str, old: &str) -> Option<(usize, usize)> {
    let old_lines: Vec<&str> = old.lines().map(|l| l.trim()).collect();
    if old_lines.len() < 2 || old_lines.iter().all(|l| l.is_empty()) {
        return None;
    }
    let mut starts = Vec::new();
    let mut off = 0;
    for l in text.split_inclusive('\n') {
        starts.push((off, l));
        off += l.len();
    }
    let mut found = None;
    for i in 0..starts.len() {
        if i + old_lines.len() > starts.len() {
            break;
        }
        if (0..old_lines.len()).all(|j| starts[i + j].1.trim() == old_lines[j]) {
            if found.is_some() {
                return None; // ambiguous
            }
            let last = &starts[i + old_lines.len() - 1];
            let end = last.0 + last.1.trim_end_matches(['\n', '\r']).len();
            found = Some((starts[i].0, end));
        }
    }
    found
}

/// Lines added and removed between two versions of a file.
pub fn count_changes(a: &str, b: &str) -> (usize, usize) {
    use similar::{ChangeTag, TextDiff};
    let (_, a, b) = crate::text::changed_region(a, b, 0);
    let d = TextDiff::from_lines(a, b);
    let mut adds = 0;
    let mut dels = 0;
    for c in d.iter_all_changes() {
        match c.tag() {
            ChangeTag::Insert => adds += 1,
            ChangeTag::Delete => dels += 1,
            _ => {}
        }
    }
    (adds, dels)
}

/// The changed lines of `b` with three lines of context, numbered like `cat -n`.
pub fn changed_snippet(a: &str, b: &str) -> String {
    // Work on the changed region (with enough unchanged context for the
    // three-line margin below) instead of splitting the whole file.
    let (offset, a, b) = crate::text::changed_region(a, b, 4);
    changed_snippet_full(a, b, offset)
}

fn changed_snippet_full(a: &str, b: &str, offset: usize) -> String {
    let al: Vec<&str> = a.lines().collect();
    let bl: Vec<&str> = b.lines().collect();
    let first = al
        .iter()
        .zip(bl.iter())
        .position(|(x, y)| x != y)
        .unwrap_or(al.len().min(bl.len()));
    let tail_same = al
        .iter()
        .rev()
        .zip(bl.iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let end_b = bl.len().saturating_sub(tail_same).max(first + 1);
    let start = first.saturating_sub(3);
    let end = (end_b + 3).min(bl.len());
    let mut out = String::new();
    for (i, line) in bl.iter().enumerate().take(end.min(start + 60)).skip(start) {
        out.push_str(&format!("{}\t{}\n", i + 1 + offset, line));
    }
    out
}

#[cfg(test)]
mod edit_tests;
