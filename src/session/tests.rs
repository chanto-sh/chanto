use super::*;

#[test]
fn interrupted_tool_batch_keeps_results_and_fills_missing_ones() {
    let calls: Vec<_> = ["c1", "c2"]
        .into_iter()
        .map(|id| crate::llm::ToolCall {
            id: id.into(),
            name: "Write".into(),
            arguments: serde_json::json!({}),
        })
        .collect();
    for partial in [false, true] {
        let mut assistant = Message::assistant("working");
        assistant.tool_calls = calls.clone();
        let mut msgs = vec![assistant, Message::user("continue")];
        if partial {
            msgs.push(Message::tool(&calls[0], "saved result"));
        }
        msgs.push(Message::assistant("next turn"));
        repair_tool_pairs(&mut msgs);
        assert_eq!(msgs.len(), 5);
        for (i, call) in calls.iter().enumerate() {
            assert_eq!(msgs[i + 1].role, Role::Tool);
            assert_eq!(msgs[i + 1].tool_call_id.as_deref(), Some(call.id.as_str()));
        }
        assert!(msgs[2].content.contains("may have run"));
        if partial {
            assert_eq!(*msgs[1].content, "saved result");
        }
        assert_eq!(*msgs[3].content, "continue");
        let before = serde_json::to_value(&msgs).unwrap();
        repair_tool_pairs(&mut msgs);
        assert_eq!(serde_json::to_value(&msgs).unwrap(), before);
    }
}

#[test]
fn tool_results_are_moved_back_under_their_call() {
    let call = crate::llm::ToolCall {
        id: "c1".into(),
        name: "Read".into(),
        arguments: serde_json::json!({}),
    };
    let mut assistant = Message::assistant("reading");
    assistant.tool_calls = vec![call.clone()];
    let mut msgs = vec![
        Message::user("go"),
        assistant,
        Message::user("<system-reminder>low disk</system-reminder>"),
        Message::tool(&call, "contents"),
        Message::user("next"),
    ];
    repair_tool_pairs(&mut msgs);
    let roles: Vec<Role> = msgs.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            Role::User,
            Role::Assistant,
            Role::Tool,
            Role::User,
            Role::User
        ]
    );
    assert_eq!(*msgs[2].content, "contents");
    // A well-formed transcript is left alone.
    let before: Vec<(Role, String)> = msgs
        .iter()
        .map(|m| (m.role, String::clone(&m.content)))
        .collect();
    repair_tool_pairs(&mut msgs);
    let after: Vec<(Role, String)> = msgs
        .iter()
        .map(|m| (m.role, String::clone(&m.content)))
        .collect();
    assert_eq!(before, after);
}

#[test]
fn a_long_well_formed_transcript_is_unchanged() {
    // Several calls per turn, some answered out of call order: all in
    // place, so nothing moves.
    let mut msgs = vec![Message::user("go")];
    for t in 0..300 {
        let calls: Vec<_> = (0..3)
            .map(|c| crate::llm::ToolCall {
                id: format!("t{t}c{c}"),
                name: "Read".into(),
                arguments: serde_json::json!({}),
            })
            .collect();
        let mut assistant = Message::assistant("reading");
        assistant.tool_calls = calls.clone();
        msgs.push(assistant);
        for c in [2, 0, 1] {
            msgs.push(Message::tool(&calls[c], format!("result {t}.{c}")));
        }
        if t % 7 == 0 {
            msgs.push(Message::user("<system-reminder>note</system-reminder>"));
        }
    }
    let before = serde_json::to_value(&msgs).unwrap();
    repair_tool_pairs(&mut msgs);
    assert_eq!(serde_json::to_value(&msgs).unwrap(), before);
}

#[test]
fn latest_skips_an_unreadable_session_and_says_why() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("chanto-latest-{}", uuid::Uuid::new_v4()));
    unsafe {
        std::env::set_var("CHANTO_HOME", &home);
    }
    let cwd = home.join("proj");
    assert!(Session::latest(&cwd).0.is_none());
    let mut good = Session::new(&cwd, "model");
    good.save(vec![Message::user("hi")]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let broken = Session::new(&cwd, "model");
    std::fs::write(broken.path(), "{\"meta\": {\"id\": tr").unwrap();
    let (s, bad) = Session::latest(&cwd);
    assert_eq!(s.map(|s| s.id), Some(good.id.clone()));
    assert_eq!(bad.len(), 1);
    assert_eq!(bad[0].0, broken.path());
    assert!(!bad[0].1.is_empty());
    // Only the broken one: nothing to fall back to, still reported.
    std::fs::remove_file(good.path()).unwrap();
    let (s, bad) = Session::latest(&cwd);
    assert!(s.is_none() && bad.len() == 1);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn delete_drops_only_its_own_rewind_snapshots() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("chanto-delete-{}", uuid::Uuid::new_v4()));
    unsafe {
        std::env::set_var("CHANTO_HOME", &home);
    }
    let cwd = home.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("a.txt"), "one").unwrap();
    let shadow = crate::history::Shadow::new(&cwd, &Default::default()).unwrap();
    let shared = shadow.snapshot().unwrap();
    std::fs::write(cwd.join("a.txt"), "two").unwrap();
    let own = shadow.snapshot().unwrap();
    let cp = |tree: &str| crate::agent::Checkpoint {
        id: "c".into(),
        timestamp: "t".into(),
        tree: Some(tree.into()),
        msg_index: 1,
        prompt: "p".into(),
        files: Default::default(),
    };
    let mut gone = Session::new(&cwd, "m");
    gone.checkpoints = vec![cp(&shared), cp(&own)];
    gone.save(vec![Message::user("hi")]).unwrap();
    let mut kept = Session::new(&cwd, "m");
    kept.checkpoints = vec![cp(&shared)];
    kept.save(vec![Message::user("hello")]).unwrap();
    let refs = || {
        let out = std::process::Command::new("git")
            .env("GIT_DIR", shadow_repo(&cwd).unwrap())
            .args(["for-each-ref", "--format=%(refname)"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    assert!(refs().contains(&own) && refs().contains(&shared));
    Session::delete(&cwd, &gone.id).unwrap();
    assert!(!gone.path().exists() && kept.path().exists());
    let after = refs();
    assert!(!after.contains(&own), "{after}");
    assert!(
        after.contains(&shared),
        "a snapshot another session uses stays: {after}"
    );
    // Project files are untouched.
    assert_eq!(std::fs::read_to_string(cwd.join("a.txt")).unwrap(), "two");
    unsafe {
        std::env::remove_var("CHANTO_HOME");
    }
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn find_dir_locates_a_session_from_any_project() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("chanto-find-{}", uuid::Uuid::new_v4()));
    unsafe {
        std::env::set_var("CHANTO_HOME", &home);
    }
    let (a, b) = (home.join("a"), home.join("b"));
    let mut s = Session::new(&a, "model");
    s.save(vec![Message::user("hi")]).unwrap();
    Session::new(&b, "model")
        .save(vec![Message::user("hi")])
        .unwrap();
    assert!(Session::load(&b, &s.id).is_err());
    assert_eq!(Session::find_dir(&s.id), Some(a.clone()));
    assert_eq!(Session::find_dir(&s.id[..s.id.len() - 2]), Some(a));
    // A date prefix matches both sessions: ambiguous.
    assert_eq!(Session::find_dir(&s.id[..8]), None);
    assert_eq!(Session::find_dir(""), None);
    assert_eq!(Session::find_dir("no-such-id"), None);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn forks_diverge_truncate_and_list_as_tree() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("chanto-fork-{}", uuid::Uuid::new_v4()));
    unsafe {
        std::env::set_var("CHANTO_HOME", &home);
    }
    let cwd = home.join("project");
    let mut parent = Session::new(&cwd, "model");
    parent.settings = serde_json::json!({"mode": "plan"});
    let messages = vec![
        Message::system("sys"),
        Message::user("one"),
        Message::assistant("first"),
        Message::user("two"),
        Message::assistant("second"),
    ];
    parent.checkpoints = [1, 3]
        .iter()
        .map(|&msg_index| crate::agent::Checkpoint {
            id: format!("cp{msg_index}"),
            timestamp: "date".into(),
            tree: Some("tree".into()),
            msg_index,
            prompt: String::clone(&messages[msg_index].content),
            files: Default::default(),
        })
        .collect();
    parent.save(messages.clone()).unwrap();
    let original = std::fs::read(parent.path()).unwrap();
    let (mut child, mut copy) = parent.fork(&messages, Some("branch"), None).unwrap();
    assert_eq!(copy.len(), messages.len());
    assert_eq!(child.settings, parent.settings);
    assert_eq!(child.checkpoints.len(), 2);
    copy.push(Message::user("child only"));
    child.save(copy).unwrap();
    assert_eq!(std::fs::read(parent.path()).unwrap(), original);
    let (past, copy) = parent.fork(&messages, None, Some(1)).unwrap();
    assert_eq!(copy.len(), 3);
    assert_eq!(past.checkpoints.len(), 1);
    assert_eq!(past.fork_point, Some(3));
    assert!(parent.fork(&messages, None, Some(10)).is_err());
    let tree = Session::tree(&cwd);
    assert_eq!(tree.len(), 3);
    assert_eq!(tree[0].0.id, parent.id);
    assert_eq!(
        tree.iter().map(|(_, depth)| *depth).collect::<Vec<_>>(),
        vec![0, 1, 1]
    );
    unsafe {
        std::env::remove_var("CHANTO_HOME");
    }
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn write_drops_images_and_round_trips() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("chanto-sess-{}", std::process::id()));
    // SAFETY: test-only, under TEST_HOME_LOCK.
    let previous = std::env::var_os("CHANTO_HOME");
    unsafe { std::env::set_var("CHANTO_HOME", &home) };
    let cwd = home.join("proj");
    let mut s = Session::new(&cwd, "m");
    assert!(
        !s.touch(&[Message::system("sys")]),
        "nothing to save without a user message"
    );
    let mut u = Message::user("look at this");
    u.images = vec!["aGk=".into()];
    let msgs = vec![Message::system("sys"), u, Message::assistant("ok")];
    s.save(msgs).unwrap();
    let (meta, back) = Session::load(&cwd, &s.id).unwrap();
    assert_eq!(meta.title, "look at this");
    // Secrets never reach the title, which is listed in plain text.
    let mut s2 = Session::new(&cwd, "m");
    let key = format!("sk-ant-api03-{}", "Q".repeat(40));
    s2.save(vec![
        Message::system("sys"),
        Message::user(format!("my key is {key}")),
    ])
    .unwrap();
    let (meta2, _) = Session::load(&cwd, &s2.id).unwrap();
    assert!(!meta2.title.contains(&key), "{}", meta2.title);
    assert!(back[1].images.is_empty() && back[1].content.contains("1 image(s) omitted"));
    assert_eq!(*back[2].content, "ok");
    unsafe {
        if let Some(previous) = previous {
            std::env::set_var("CHANTO_HOME", previous);
        } else {
            std::env::remove_var("CHANTO_HOME");
        }
    }
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn interrupted_notice_lists_the_running_calls_one_per_line() {
    let f = InFlight {
        at: "2026-10-25T00:00:00-07:00".into(),
        running: vec![
            "Read: Read(src/tui/run.rs)".into(),
            "Bash: grep -rn x".into(),
        ],
        files: vec!["src/lib.rs".into()],
    };
    let notice = interrupted_notice(&f);
    assert!(
        notice.contains("\nIt was running:\n- Read: Read(src/tui/run.rs)\n- Bash: grep -rn x\n"),
        "{notice}"
    );
    assert!(
        notice.contains("\nIt had already changed src/lib.rs;"),
        "{notice}"
    );
}

#[test]
fn in_flight_marker_is_rewritten_in_place() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("chanto-sess-inflight-{}", std::process::id()));
    let previous = std::env::var_os("CHANTO_HOME");
    // SAFETY: test-only, under TEST_HOME_LOCK.
    unsafe { std::env::set_var("CHANTO_HOME", &home) };
    let s = Session::new(&home.join("proj"), "m");
    assert!(s.interrupted().is_none());
    // The session's directory doesn't exist yet.
    s.mark_in_flight(
        vec!["Bash: a long command".repeat(4)],
        vec!["src/a.rs".into()],
    );
    let first = s.interrupted().unwrap();
    assert!(
        first.contains("a long command") && first.contains("src/a.rs"),
        "{first}"
    );
    // A shorter marker replaces a longer one whole.
    s.mark_in_flight(vec!["Read: b.rs".into()], vec![]);
    let f = s.in_flight().unwrap();
    assert_eq!(f.running, ["Read: b.rs"]);
    assert!(f.files.is_empty());
    // Killed between the write and the truncate: the earlier marker's
    // tail after the new one is ignored.
    let p = s.in_flight_path();
    let mut b = std::fs::read(&p).unwrap();
    b.extend_from_slice(b"ommand\"],\"files\":[\"src/a.rs\"]}");
    std::fs::write(&p, b).unwrap();
    assert_eq!(s.in_flight().unwrap().running, ["Read: b.rs"]);
    s.clear_in_flight();
    assert!(s.interrupted().is_none());
    unsafe {
        if let Some(previous) = previous {
            std::env::set_var("CHANTO_HOME", previous);
        } else {
            std::env::remove_var("CHANTO_HOME");
        }
    }
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn ephemeral_sessions_are_not_written_and_names_resolve() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("chanto-sess-eph-{}", std::process::id()));
    let previous = std::env::var_os("CHANTO_HOME");
    // SAFETY: test-only, under TEST_HOME_LOCK.
    unsafe { std::env::set_var("CHANTO_HOME", &home) };
    let cwd = home.join("proj");
    let mut s = Session::new(&cwd, "m");
    s.ephemeral = true;
    s.save(vec![Message::system("sys"), Message::user("hi")])
        .unwrap();
    assert!(!s.path().exists());
    let mut named = Session::new(&cwd, "m");
    named.title = "auth-refactor".into();
    named
        .save(vec![Message::system("sys"), Message::user("hi")])
        .unwrap();
    assert_eq!(Session::id_for(&cwd, "auth-refactor"), named.id);
    assert_eq!(Session::id_for(&cwd, &named.id), named.id);
    assert_eq!(Session::id_for(&cwd, "nope"), "nope");
    unsafe {
        if let Some(previous) = previous {
            std::env::set_var("CHANTO_HOME", previous);
        } else {
            std::env::remove_var("CHANTO_HOME");
        }
    }
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn save_clears_an_existing_transcript() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("chanto-sess-clear-{}", std::process::id()));
    // SAFETY: test-only, under TEST_HOME_LOCK.
    let previous = std::env::var_os("CHANTO_HOME");
    unsafe { std::env::set_var("CHANTO_HOME", &home) };
    let cwd = home.join("proj");
    let mut s = Session::new(&cwd, "m");
    s.save(vec![
        Message::system("sys"),
        Message::user("hello"),
        Message::assistant("hi"),
    ])
    .unwrap();
    // /rewind can truncate the history to the system message; the saved
    // transcript must follow, or resuming resurrects the discarded turn.
    s.save(vec![Message::system("sys")]).unwrap();
    let (_, back) = Session::load(&cwd, &s.id).unwrap();
    assert_eq!(back.len(), 1, "rewound history must not stay on disk");
    // A failed save (here the path is a directory; in life a full disk)
    // leaves no temp file behind.
    let mut s2 = Session::new(&cwd, "m");
    std::fs::create_dir_all(s2.path().join("blocker")).unwrap();
    assert!(
        s2.save(vec![Message::system("sys"), Message::user("hello")])
            .is_err()
    );
    let dir = s2.path().parent().unwrap().to_path_buf();
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    unsafe {
        if let Some(previous) = previous {
            std::env::set_var("CHANTO_HOME", previous);
        } else {
            std::env::remove_var("CHANTO_HOME");
        }
    }
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_save_removes_what_a_crashed_save_left_behind() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("chanto-tmp-{}", uuid::Uuid::new_v4()));
    unsafe {
        std::env::set_var("CHANTO_HOME", &home);
    }
    let cwd = home.join("proj");
    let mut s = Session::new(&cwd, "model");
    s.save(vec![Message::user("hi")]).unwrap();
    let p = s.path();
    let dir = p.parent().unwrap().to_path_buf();
    let name = p.file_name().unwrap().to_str().unwrap().to_string();
    let leftover = |mid: &str| dir.join(format!("{name}.{mid}.tmp"));
    // A process that has exited: its save was cut off.
    let mut dead = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" })
        .args(if cfg!(windows) {
            &["/c", "exit"][..]
        } else {
            &[][..]
        })
        .spawn()
        .unwrap();
    let dead_pid = dead.id();
    dead.wait().unwrap();
    // One still running may be mid-save.
    let mut live = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sleep" })
        .args(if cfg!(windows) {
            &["/c", "ping -n 30 127.0.0.1 > NUL"][..]
        } else {
            &["30"][..]
        })
        .spawn()
        .unwrap();
    let files = [
        (leftover(&format!("{dead_pid}.aaaa")), false),
        (leftover(&format!("{}.bbbb", live.id())), true),
        (leftover(&format!("{}.cccc", std::process::id())), true),
        // Without a process id (older saves): kept until a minute old.
        (leftover("dddd"), true),
        (leftover("eeee"), false),
        // Another session's leftover isn't this save's business.
        (dir.join(format!("other.json.{dead_pid}.ffff.tmp")), true),
    ];
    for (f, _) in &files {
        std::fs::write(f, "{").unwrap();
    }
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    std::fs::File::options()
        .write(true)
        .open(leftover("eeee"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    // The session's first save in this process already cleaned up, so run
    // the cleanup itself.
    super::remove_crashed_saves_for_test(&p);
    for (f, kept) in &files {
        assert_eq!(f.exists(), *kept, "{}", f.display());
    }
    let _ = live.kill();
    let _ = live.wait();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn the_first_save_after_a_crash_cleans_up() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("chanto-crash-{}", uuid::Uuid::new_v4()));
    unsafe {
        std::env::set_var("CHANTO_HOME", &home);
    }
    let mut s = Session::new(&home.join("proj"), "model");
    let p = s.path();
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    // What a process killed mid-save leaves: its id, then the temp file.
    let mut dead = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" })
        .args(if cfg!(windows) {
            &["/c", "exit"][..]
        } else {
            &[][..]
        })
        .spawn()
        .unwrap();
    let pid = dead.id();
    dead.wait().unwrap();
    let left = p.with_extension(format!("json.{pid}.0123.tmp"));
    std::fs::write(&left, "{\"meta\":").unwrap();
    s.save(vec![Message::user("hi")]).unwrap();
    assert!(!left.exists());
    assert!(p.exists());
    let _ = std::fs::remove_dir_all(&home);
}
