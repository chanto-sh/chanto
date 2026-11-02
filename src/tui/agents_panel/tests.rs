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
