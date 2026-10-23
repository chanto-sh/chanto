//! The panel under the prompt: a `main` row and one row per
//! subagent (foreground or background) with what it's doing now, its elapsed
//! time and tokens, then background shells. ↓ from an empty prompt selects a
//! row; Enter on an agent shows its live transcript, Enter on `main` (or Esc)
//! goes back.

use crate::agent::live::{Row, RowStatus};

/// Rows shown at most before "↓ N more".
pub const MAX_ROWS: usize = 5;

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Main,
    /// A subagent the registry tracks (its transcript can be shown).
    Agent(Row),
    /// A background agent started outside a Task call (no live transcript).
    Background {
        id: String,
        label: String,
        secs: u64,
    },
    Shell {
        id: String,
        label: String,
        /// A Monitor watch rather than a plain background shell.
        monitor: bool,
    },
}

/// The panel's items: `main` first whenever any agent is listed, then agents
/// (oldest first), then shells.
pub fn items(
    agents: Vec<Row>,
    background: Vec<(String, String, u64)>,
    shells: Vec<(String, String, bool)>,
) -> Vec<Item> {
    let mut out = Vec::new();
    if !agents.is_empty() || !background.is_empty() {
        out.push(Item::Main);
    }
    out.extend(agents.into_iter().map(Item::Agent));
    out.extend(
        background
            .into_iter()
            .map(|(id, label, secs)| Item::Background { id, label, secs }),
    );
    out.extend(
        shells
            .into_iter()
            .map(|(id, label, monitor)| Item::Shell { id, label, monitor }),
    );
    out
}

/// Which items are on screen with at most `max` lines: `(start, end, hidden
/// below)`. Past `max` items the last line becomes "↓ N more", and the window
/// scrolls so the selection stays visible.
pub fn window(len: usize, sel: Option<usize>, max: usize) -> (usize, usize, usize) {
    if len <= max {
        return (0, len, 0);
    }
    if max < 2 {
        return (0, 0, len);
    }
    let rows = max - 1;
    let sel = sel.unwrap_or(0).min(len - 1);
    let start = sel.saturating_sub(rows - 1).min(len - rows);
    let end = start + rows;
    (start, end, len - end)
}

/// Lines the panel needs for `len` items.
pub fn height(len: usize, max: usize) -> usize {
    len.min(max)
}

/// An agent row's text: `kind  activity` on the left, `1m 35s · ↓ 36.2k tokens`
/// on the right, padded to `width` columns (the marker is drawn separately).
pub fn agent_row(
    kind: &str,
    activity: &str,
    secs: u64,
    tokens: u64,
    status: &RowStatus,
    width: usize,
) -> (String, String) {
    let mut right = crate::util::duration(secs);
    if tokens > 0 {
        right.push_str(&format!(" · ↓ {} tokens", crate::util::short_num(tokens)));
    }
    match status {
        RowStatus::Failed => right.push_str(" · failed"),
        RowStatus::Done => right.push_str(" · done"),
        RowStatus::Running => {}
    }
    let kind = crate::util::one_line(kind, 24);
    let room = width.saturating_sub(right.chars().count() + kind.chars().count() + 4);
    let activity = crate::util::one_line(activity, room.max(1));
    let mut left = if room == 0 {
        kind
    } else {
        format!("{kind}  {activity}")
    };
    let pad = width.saturating_sub(left.chars().count() + right.chars().count() + 2);
    left.push_str(&" ".repeat(pad));
    (left, right)
}

/// The selection after ↑ (None leaves the panel) or ↓.
pub fn step(sel: usize, len: usize, down: bool) -> Option<usize> {
    if down {
        Some((sel + 1).min(len.saturating_sub(1)))
    } else {
        sel.checked_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str) -> Row {
        Row {
            key: key.into(),
            id: format!("agent-{key}"),
            kind: "general-purpose".into(),
            activity: "Reading".into(),
            secs: 95,
            tokens: 36_200,
            status: RowStatus::Running,
            background: false,
        }
    }

    #[test]
    fn main_row_only_with_agents() {
        assert_eq!(
            items(
                vec![],
                vec![],
                vec![("bash_1".into(), "npm run dev".into(), false)]
            ),
            vec![Item::Shell {
                id: "bash_1".into(),
                label: "npm run dev".into(),
                monitor: false,
            }]
        );
        let it = items(
            vec![row("a")],
            vec![("agent-x".into(), "fork".into(), 3)],
            vec![("bash_1".into(), "ls".into(), false)],
        );
        assert_eq!(it.len(), 4);
        assert_eq!(it[0], Item::Main);
        assert!(
            matches!(it[1], Item::Agent(_))
                && matches!(it[2], Item::Background { .. })
                && matches!(it[3], Item::Shell { .. })
        );
    }

    #[test]
    fn overflow_scrolls_with_selection() {
        // Fits: everything, nothing hidden.
        assert_eq!(window(3, None, 5), (0, 3, 0));
        assert_eq!(window(5, Some(4), 5), (0, 5, 0));
        // Eight items in five lines: four rows and "↓ 4 more".
        assert_eq!(window(8, None, 5), (0, 4, 4));
        assert_eq!(window(8, Some(3), 5), (0, 4, 4));
        assert_eq!(window(8, Some(4), 5), (1, 5, 3));
        assert_eq!(window(8, Some(7), 5), (4, 8, 0));
        assert_eq!(height(8, 5), 5);
        assert_eq!(height(2, 5), 2);
    }

    #[test]
    fn agent_row_fits_width() {
        let (l, r) = agent_row(
            "general-purpose",
            "Writing candidates to cand_english.txt",
            95,
            36_200,
            &RowStatus::Running,
            80,
        );
        assert_eq!(r, "1m 35s · ↓ 36.2k tokens");
        assert!(l.starts_with("general-purpose  Writing candidates"));
        assert_eq!(l.chars().count() + r.chars().count() + 2, 80);
        // Narrow: the activity is cut, never the stats.
        let (l, r) = agent_row(
            "general-purpose",
            "Writing candidates to cand_english.txt",
            95,
            36_200,
            &RowStatus::Failed,
            60,
        );
        assert!(r.ends_with("· failed"));
        assert!(
            l.starts_with("general-purpose  Writin") && l.contains('…'),
            "{l}"
        );
        assert_eq!(l.chars().count() + r.chars().count() + 2, 60);
        // Too narrow for any activity: the type and stats stay.
        let (l, _) = agent_row(
            "general-purpose",
            "Writing",
            95,
            36_200,
            &RowStatus::Failed,
            40,
        );
        assert_eq!(l.trim_end(), "general-purpose");
        // No tokens yet: just the time.
        assert_eq!(
            agent_row("Explore", "x", 5, 0, &RowStatus::Running, 40).1,
            "5s"
        );
    }

    #[test]
    fn up_from_top_leaves_the_panel() {
        assert_eq!(step(0, 3, false), None);
        assert_eq!(step(1, 3, false), Some(0));
        assert_eq!(step(2, 3, true), Some(2));
        assert_eq!(step(0, 3, true), Some(1));
    }
}
