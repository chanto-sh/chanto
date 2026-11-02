use super::*;

#[tokio::test]
async fn the_file_tools_work_on_a_file_system_in_memory() {
    let dir = std::env::temp_dir().join(format!("chanto-memfs-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let env = crate::agent::tests::make_env(
        "http://localhost:1",
        &dir,
        crate::permissions::Mode::Default,
        32768,
    );
    // A real absolute path on every platform, never created on disk.
    let root = std::env::temp_dir().join(format!("chanto-mem-{}", uuid::Uuid::new_v4()));
    let at = |rel: &str| root.join(rel).display().to_string();
    let mem = std::sync::Arc::new(chanto_core::fs::MemFs::new());
    mem.insert(at("p/a.txt"), "one\ntwo\n");
    env.set_fs(mem.clone());
    let ctx = Ctx {
        env,
        cancel: tokio_util::sync::CancellationToken::new(),
        depth: 0,
        call_id: None,
        cwd_override: None,
        reads: None,
    };
    let text = |p: &str| String::from_utf8(mem.read(Path::new(&at(p))).unwrap()).unwrap();
    let out = Read
        .run(json!({"file_path": at("p/a.txt")}), &ctx)
        .await
        .unwrap();
    assert!(out.content.contains("2\ttwo"), "{}", out.content);
    let out = Read
        .run(json!({"file_path": at("p/a.tx")}), &ctx)
        .await
        .unwrap();
    assert!(
        out.is_error && out.content.contains("Did you mean one of: a.txt"),
        "{}",
        out.content
    );
    let out = Edit
        .run(
            json!({"file_path": at("p/a.txt"), "old_string": "two", "new_string": "three"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(text("p/a.txt"), "one\nthree\n");
    let out = Write
        .run(
            json!({"file_path": at("p/new/b.txt"), "content": "hi\n"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(text("p/new/b.txt"), "hi\n");
    // A file changed behind the agent's back must be read again first.
    mem.insert(at("p/a.txt"), "changed elsewhere\n");
    let out = Write
        .run(json!({"file_path": at("p/a.txt"), "content": "x\n"}), &ctx)
        .await
        .unwrap();
    assert!(
        out.is_error && out.content.contains("modified since you last read it"),
        "{}",
        out.content
    );
    assert!(!root.exists(), "nothing reached the disk");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[tokio::test]
async fn reading_a_named_pipe_is_refused_rather_than_hanging() {
    let dir = std::env::temp_dir().join(format!("chanto-fifo-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(dir.join("fifo"))
            .status()
            .unwrap()
            .success()
    );
    let env = crate::agent::tests::make_env(
        "http://localhost:1",
        &dir,
        crate::permissions::Mode::Default,
        32768,
    );
    let ctx = Ctx {
        env,
        cancel: tokio_util::sync::CancellationToken::new(),
        depth: 0,
        call_id: None,
        cwd_override: None,
        reads: None,
    };
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        Read.run(
            json!({"file_path": dir.join("fifo").display().to_string()}),
            &ctx,
        ),
    )
    .await
    .expect("Read of a FIFO hung")
    .unwrap();
    assert!(
        out.is_error && out.content.contains("not a regular file"),
        "{}",
        out.content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_oversize_image_is_refused_rather_than_read_whole() {
    let dir = std::env::temp_dir().join(format!("chanto-blob-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    // A sparse 21 MiB file: over the 20 MiB whole-file cap, no real disk use.
    let big = dir.join("huge.png");
    let f = std::fs::File::create(&big).unwrap();
    f.set_len(21 * 1024 * 1024).unwrap();
    let env = crate::agent::tests::make_env(
        "http://localhost:1",
        &dir,
        crate::permissions::Mode::Default,
        32768,
    );
    let ctx = Ctx {
        env,
        cancel: tokio_util::sync::CancellationToken::new(),
        depth: 0,
        call_id: None,
        cwd_override: None,
        reads: None,
    };
    let out = Read
        .run(json!({"file_path": big.display().to_string()}), &ctx)
        .await
        .unwrap();
    assert!(
        out.is_error && out.content.contains("capped at 20 MiB"),
        "{}",
        out.content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn editing_a_non_utf8_file_names_the_encoding() {
    let dir = std::env::temp_dir().join(format!("chanto-enc-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let env = crate::agent::tests::make_env(
        "http://localhost:1",
        &dir,
        crate::permissions::Mode::Default,
        32768,
    );
    let ctx = Ctx {
        env: env.clone(),
        cancel: tokio_util::sync::CancellationToken::new(),
        depth: 0,
        call_id: None,
        cwd_override: None,
        reads: None,
    };
    for (name, bytes, want) in [
        ("latin1.txt", b"caf\xe9 au lait\n".to_vec(), "Latin-1"),
        ("utf16.txt", vec![0xFF, 0xFE, b'h', 0, b'i', 0], "UTF-16 LE"),
    ] {
        let file = dir.join(name);
        std::fs::write(&file, &bytes).unwrap();
        env.mark_read(&file);
        let err = Edit.run(json!({"file_path": file.display().to_string(), "old_string": "caf", "new_string": "tea"}), &ctx).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("isn't UTF-8 text") && msg.contains(want) && msg.contains("iconv"),
            "{msg}"
        );
        assert_eq!(
            std::fs::read(&file).unwrap(),
            bytes,
            "the file is untouched"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unchanged_file_is_not_sent_twice() {
    let dir = std::env::temp_dir().join(format!("chanto-reads-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("a.txt");
    std::fs::write(&file, "one\ntwo\n").unwrap();
    let env = crate::agent::tests::make_env(
        "http://localhost:1",
        &dir,
        crate::permissions::Mode::Default,
        32768,
    );
    let log = std::sync::Arc::new(ReadLog::default());
    let ctx = Ctx {
        env: env.clone(),
        cancel: tokio_util::sync::CancellationToken::new(),
        depth: 0,
        call_id: None,
        cwd_override: None,
        reads: Some(log.clone()),
    };
    let read = |args: Value| async { Read.run(args, &ctx).await.unwrap().content };
    let path = file.display().to_string();
    assert!(read(json!({"file_path": path})).await.contains("two"));
    assert!(
        read(json!({"file_path": path}))
            .await
            .contains("hasn't changed"),
        "same lines, same file"
    );
    assert!(
        read(json!({"file_path": path, "offset": 2}))
            .await
            .contains("two"),
        "other lines are read"
    );
    // A change, or a conversation that lost the output, reads it again.
    std::fs::write(&file, "one\ntwo\nthree\n").unwrap();
    assert!(read(json!({"file_path": path})).await.contains("three"));
    log.clear();
    assert!(read(json!({"file_path": path})).await.contains("three"));
    // Outside an agent's turn, always the text.
    let plain = Ctx {
        env,
        cancel: tokio_util::sync::CancellationToken::new(),
        depth: 0,
        call_id: None,
        cwd_override: None,
        reads: None,
    };
    assert!(
        Read.run(json!({"file_path": path}), &plain)
            .await
            .unwrap()
            .content
            .contains("three")
    );
    assert!(
        Read.run(json!({"file_path": path}), &plain)
            .await
            .unwrap()
            .content
            .contains("three")
    );
    let _ = std::fs::remove_dir_all(&dir);
}
