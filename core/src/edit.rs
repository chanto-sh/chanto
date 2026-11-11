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
        if let Some(m) = fuzzy_find(text, old) {
            let new = fit_to_match(text, &m, old, new)?;
            let mut out = String::with_capacity(text.len() + new.len());
            out.push_str(&text[..m.start]);
            out.push_str(&new);
            out.push_str(&text[m.end..]);
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

/// Where a whitespace-tolerant match landed: the byte range (from the first
/// matched line's start to the end of the last one's text, before its line
/// ending) and the matched lines.
struct FuzzyMatch<'a> {
    start: usize,
    end: usize,
    lines: Vec<&'a str>,
}

/// Line-based match ignoring trailing whitespace and indentation
/// differences. Returns the match only if it is unique.
fn fuzzy_find<'a>(text: &'a str, old: &str) -> Option<FuzzyMatch<'a>> {
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
            found = Some(FuzzyMatch {
                start: starts[i].0,
                end,
                lines: starts[i..i + old_lines.len()]
                    .iter()
                    .map(|(_, l)| l.trim_end_matches(['\n', '\r']))
                    .collect(),
            });
        }
    }
    found
}

fn indent(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// `new` rewritten to sit where a fuzzy match landed, so the edit changes
/// only what the model changed: its indentation in the file's own
/// (`old_string` often drops the first line's indent, or uses spaces where
/// the file has tabs), its line endings in the file's, and no doubled
/// newline when both strings end in one (the matched range stops before the
/// last line's ending). When the indentation can't be worked out for sure,
/// the edit is refused with the matched lines, so the model can retry with
/// them exactly instead of getting a silently misindented file.
fn fit_to_match(
    text: &str,
    m: &FuzzyMatch,
    old: &str,
    new: &str,
) -> std::result::Result<String, String> {
    let crlf = text[m.start..].contains("\r\n")
        && text[m.start..]
            .split_inclusive('\n')
            .take(m.lines.len())
            .all(|l| l.ends_with("\r\n") || !l.ends_with('\n'));
    let mut new = new.replace("\r\n", "\n");
    if old.ends_with('\n') && new.ends_with('\n') {
        new.pop();
    }
    // Pairs of (indent in old_string, indent in the file), non-blank lines.
    let pairs: Vec<(&str, &str)> = old
        .lines()
        .zip(&m.lines)
        .filter(|(o, _)| !o.trim().is_empty())
        .map(|(o, f)| (indent(o), indent(f)))
        .collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    let new_indents: Vec<&str> = new_lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| indent(l))
        .collect();
    let refuse = || {
        Err(format!(
            "old_string only matched after ignoring whitespace, and the indentation of new_string can't be fitted to the file for sure. The matched lines are:\n{}\nRetry with old_string copied exactly from them, and new_string indented the same way.",
            m.lines.join("\n")
        ))
    };
    // How each new line's indentation becomes the file's.
    let map: Box<dyn Fn(usize, &str) -> Option<String>> = if pairs.iter().all(|(o, f)| o == f) {
        // Only trailing whitespace differed: keep new_string as written.
        Box::new(|_, n| Some(n.to_string()))
    } else if matches!(
        (pairs.first(), new_indents.first()),
        (Some((o, f)), Some(n)) if n == f && n != o
    ) {
        // new_string starts at the file's indent, not old_string's: it
        // was written in the file's own indentation already.
        Box::new(|_, n| Some(n.to_string()))
    } else if !pairs.is_empty()
        && new_indents
            .iter()
            .all(|n| pairs.iter().any(|(o, _)| o == n))
    {
        // Written in old_string's indentation: translate it line by line,
        // unless one old indent stands for two different file indents.
        let pairs = pairs.clone();
        Box::new(move |_, n| {
            let mut targets = pairs.iter().filter(|(o, _)| *o == n).map(|(_, f)| *f);
            let first = targets.next()?;
            targets.all(|f| f == first).then(|| first.to_string())
        })
    } else if new_indents.iter().all(|n| {
        m.lines
            .iter()
            .any(|f| !f.trim().is_empty() && indent(f) == *n)
    }) {
        // Already in the file's indentation.
        Box::new(|_, n| Some(n.to_string()))
    } else if let Some((o, f)) = shift(&pairs) {
        // A uniform shift (another indent width or tabs for spaces).
        Box::new(move |_, n| n.strip_prefix(o.as_str()).map(|rest| format!("{f}{rest}")))
    } else if let (Some((o0, f0)), Some((o, f))) = (pairs.first().copied(), shift(&pairs[1..])) {
        // A uniform shift, except old_string dropped the first line's indent.
        Box::new(move |i, n| {
            if i == 0 && n == o0 {
                Some(f0.to_string())
            } else {
                n.strip_prefix(o.as_str()).map(|rest| format!("{f}{rest}"))
            }
        })
    } else {
        return refuse();
    };
    let mut out = Vec::with_capacity(new_lines.len());
    let mut first_text = true;
    for line in &new_lines {
        if line.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        let i = if first_text { 0 } else { 1 };
        first_text = false;
        match map(i, indent(line)) {
            Some(ind) => out.push(format!("{ind}{}", line.trim_start())),
            None => return refuse(),
        }
    }
    let joined = out.join("\n");
    Ok(if crlf {
        joined.replace('\n', "\r\n")
    } else {
        joined
    })
}

/// The (old prefix, file prefix) every pair shares, when each old indent and
/// its file indent differ only by those prefixes.
fn shift(pairs: &[(&str, &str)]) -> Option<(String, String)> {
    let (o0, f0) = pairs.iter().min_by_key(|(o, _)| o.len())?;
    let (o0, f0) = (o0.to_string(), f0.to_string());
    pairs
        .iter()
        .all(|(o, f)| matches!((o.strip_prefix(o0.as_str()), f.strip_prefix(f0.as_str())), (Some(a), Some(b)) if a == b || a.replace("    ", "\t") == b || b.replace("    ", "\t") == a))
        .then_some((o0, f0))
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
