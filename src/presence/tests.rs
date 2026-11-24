use super::*;

fn record(id: &str, beat: i64, files: &[(&str, i64)]) -> Record {
    Record {
        id: id.into(),
        pid: std::process::id(),
        task: format!("task {id}"),
        beat,
        files: files.iter().map(|(f, t)| (f.to_string(), *t)).collect(),
    }
}

#[test]
fn live_peers_and_recent_conflicts() {
    let dir = std::env::temp_dir().join(format!("chanto-presence-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let at = 10_000;
    // A process that has exited: its record still beats recently, but it's gone.
    let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" })
        .args(if cfg!(windows) {
            &["/c", "exit"][..]
        } else {
            &[][..]
        })
        .spawn()
        .unwrap();
    let dead = Record {
        pid: child.id(),
        ..record("killed", at - 10, &[("/p/a.rs", at - 5)])
    };
    child.wait().unwrap();
    for r in [
        record("me", at, &[]),
        record(
            "live",
            at - 30,
            &[("/p/a.rs", at - 100), ("/p/old.rs", at - 5000)],
        ),
        record("gone", at - 500, &[("/p/a.rs", at - 10)]),
        dead,
    ] {
        std::fs::write(
            dir.join(format!("{}.json", r.id)),
            serde_json::to_vec(&r).unwrap(),
        )
        .unwrap();
    }
    let peers = read(&dir, "me", at);
    // Not this session, not one that stopped beating, not one whose process exited.
    assert_eq!(
        peers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        ["live"]
    );
    // A recent edit by a live peer is a conflict; an old one isn't.
    assert_eq!(
        conflict(&peers, "/p/a.rs", at),
        Some(("live".into(), at - 100, "task live".into()))
    );
    assert_eq!(conflict(&peers, "/p/old.rs", at), None);
    assert_eq!(conflict(&peers, "/p/other.rs", at), None);
    assert_eq!(
        describe(&peers[0], at),
        "- \"task live\"; recently edited /p/a.rs"
    );
    // What the *model* is told about a peer: the files, never its prompt.
    assert_eq!(
        describe_files(&peers[0], at),
        "- another chanto session recently edited /p/a.rs"
    );
    assert!(!describe_files(&peers[0], at).contains("task live"));
    let _ = std::fs::remove_dir_all(&dir);
}
