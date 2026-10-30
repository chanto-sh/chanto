//! Small text helpers the tools and the terminal UI share.

/// The changed middle of two texts, for diffing large files cheaply: drops
/// the identical lines at the start and end, keeping `context` of them on
/// each side. Returns (lines dropped from the start, old middle, new middle).
pub fn changed_region<'a>(old: &'a str, new: &'a str, context: usize) -> (usize, &'a str, &'a str) {
    let (a, b) = (old.as_bytes(), new.as_bytes());
    let mut pre = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    // Back up to the start of the line, then `context` more lines.
    pre = a[..pre]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |i| i + 1);
    for _ in 0..context {
        if pre == 0 {
            break;
        }
        pre = a[..pre - 1]
            .iter()
            .rposition(|&c| c == b'\n')
            .map_or(0, |i| i + 1);
    }
    let max_suf = a.len().min(b.len()) - pre;
    let mut suf = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take(max_suf)
        .take_while(|(x, y)| x == y)
        .count();
    // Only drop whole lines from the end, then keep `context` of them.
    let mut cut = a.len() - suf;
    cut = a[cut..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(a.len(), |i| cut + i + 1);
    for _ in 0..context {
        if cut >= a.len() {
            break;
        }
        cut = a[cut..]
            .iter()
            .position(|&c| c == b'\n')
            .map_or(a.len(), |i| cut + i + 1);
    }
    suf = a.len() - cut;
    let lines_before = memchr::memchr_iter(b'\n', &a[..pre]).count();
    (
        lines_before,
        &old[pre..old.len() - suf],
        &new[pre..new.len() - suf],
    )
}

/// Truncate to at most `max` bytes on a char boundary, appending a marker.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated {} chars]", &s[..end], s.len() - end)
}
