use super::*;

#[test]
fn portable_html_and_import_round_trip() {
    let _home = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _secrets = crate::secrets::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("chanto-share-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    unsafe {
        std::env::set_var("CHANTO_HOME", &dir);
    }
    let mut session = Session::new(&dir, "test-model");
    session.title = "<title>".into();
    let messages = vec![
        Message::system("system"),
        Message::user("/Users/test/project ghp_0123456789abcdefghijklmnopqrstuvwxyzAB </script>"),
        Message::assistant("reply"),
    ];
    let content = html(&session, &messages, false, "/Users/test").unwrap();
    assert!(!content.contains("/Users/test"));
    assert!(!content.contains("ghp_0123456789abcdefghijklmnopqrstuvwxyzAB"));
    assert!(!content.contains("src=\"") && !content.contains("href=\""));
    assert!(content.contains("&lt;/script&gt;") && content.contains("\\u003c/script\\u003e"));
    assert!(content.contains("color-scheme:light dark"));
    let path = dir.join("share.html");
    std::fs::write(&path, content).unwrap();
    let imported = import(&path, &dir).unwrap();
    assert_ne!(imported.id, session.id);
    let (_, back) = Session::load(&dir, &imported.id).unwrap();
    assert_eq!(back.len(), messages.len());
    assert!(back[1].content.contains("~/project"));
    assert_eq!(*back[2].content, "reply");
    let json = dir.join("session.json");
    std::fs::copy(imported.path(), &json).unwrap();
    assert_ne!(import(&json, &dir).unwrap().id, imported.id);
    let mut tool = Message::assistant("output-secret");
    tool.role = Role::Tool;
    assert!(
        !html(&session, &[tool], true, "")
            .unwrap()
            .contains("output-secret")
    );
    unsafe {
        std::env::remove_var("CHANTO_HOME");
    }
    // Best-effort: on Windows another process (a scanner or indexer) can
    // still hold a file we just wrote, and the temp dir is cleaned anyway.
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn import_drops_settings_links_and_future_dates() {
    let _home = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("chanto-share-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    unsafe {
        std::env::set_var("CHANTO_HOME", &dir);
    }
    let mut session = Session::new(&dir, "test-model");
    session.settings = serde_json::json!({"mode": "bypass", "acp_session": "x", "tool_policy": {}});
    session.pull_requests = vec!["https://github.com/o/r/pull/1".into()];
    session.parent_id = Some("other".into());
    session.fork_point = Some(3);
    session.updated = "2999-01-01T00:00:00+00:00".into();
    let file = SessionFile {
        meta: session,
        messages: vec![Message::user("hi")],
    };
    let path = dir.join("shared.json");
    std::fs::write(&path, serde_json::to_string(&file).unwrap()).unwrap();
    let imported = import(&path, &dir).unwrap();
    let (back, _) = Session::load(&dir, &imported.id).unwrap();
    assert!(back.settings.is_null(), "{}", back.settings);
    assert!(back.pull_requests.is_empty());
    assert!(back.parent_id.is_none() && back.fork_point.is_none());
    assert!(back.updated <= chrono::Local::now().to_rfc3339());
    unsafe {
        std::env::remove_var("CHANTO_HOME");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn gist_requires_exact_confirmation_and_uses_secret_default() {
    // PATH is process-wide; exercise the fake CLI in an isolated test process.
    if std::env::var_os("CHANTO_TEST_GIST_CHILD").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "share::tests::gist_requires_exact_confirmation_and_uses_secret_default",
                "--nocapture",
            ])
            .env("CHANTO_TEST_GIST_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let _home = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("chanto-gh-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    unsafe {
        std::env::set_var("CHANTO_HOME", &dir);
    }
    let path = dir.join("share.html");
    std::fs::write(&path, "exact upload bytes").unwrap();
    let gh = dir.join("gh");
    std::fs::write(&gh, "#!/bin/sh\nif [ \"$1\" = auth ]; then exit 0; fi\n[ \"$1 $2 $3 $4 $5\" = 'gist create --filename session.html -' ] || exit 2\nIFS= read -r text\n[ \"$text\" = 'exact upload bytes' ] || exit 3\nprintf 'https://gist.example/secret\\n'\n").unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let old_path = std::env::var_os("PATH");
    unsafe {
        std::env::set_var("PATH", &dir);
    }
    assert!(
        gist(&path, &mut |preview| {
            assert!(preview.contains("exact upload bytes"));
            false
        })
        .is_err()
    );
    assert_eq!(
        gist(&path, &mut |preview| {
            assert!(preview.contains("SECRET"));
            // A change after approval cannot change the bytes sent to gh.
            std::fs::write(&path, "different bytes").unwrap();
            true
        })
        .unwrap(),
        "https://gist.example/secret"
    );
    unsafe {
        if let Some(old) = old_path {
            std::env::set_var("PATH", old);
        } else {
            std::env::remove_var("PATH");
        }
        std::env::remove_var("CHANTO_HOME");
    }
    // Best-effort: on Windows another process (a scanner or indexer) can
    // still hold a file we just wrote, and the temp dir is cleaned anyway.
    let _ = std::fs::remove_dir_all(dir);
}
