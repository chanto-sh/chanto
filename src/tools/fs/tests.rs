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

fn three_cell_notebook() -> String {
    json!({
        "nbformat": 4,
        "nbformat_minor": 5,
        "metadata": {},
        "cells": [
            {"cell_type": "code", "metadata": {}, "source": ["a = 1\n"], "outputs": [{"output_type": "stream", "name": "stdout", "text": ["1\n"]}], "execution_count": 3},
            {"cell_type": "markdown", "metadata": {}, "source": ["# B\n"]},
            {"cell_type": "code", "metadata": {}, "source": ["c = 3\n"], "outputs": [], "execution_count": null}
        ]
    })
    .to_string()
}

#[test]
fn a_bad_cell_index_is_refused_rather_than_editing_cell_zero() {
    let nb = three_cell_notebook();
    for (args, want) in [
        (
            json!({"edit_mode": "delete", "cell_index": -1}),
            "use 0 to 2",
        ),
        (
            json!({"edit_mode": "replace", "cell_index": -1, "new_source": "x"}),
            "isn't a cell number",
        ),
        (
            json!({"edit_mode": "insert", "cell_index": -1, "new_source": "x"}),
            "use 0 to 3",
        ),
        (
            json!({"edit_mode": "delete", "cell_index": 1.5}),
            "isn't a cell number",
        ),
        (
            json!({"edit_mode": "delete", "cell_index": "last"}),
            "isn't a cell number",
        ),
        (json!({"edit_mode": "delete"}), "cell_index is required"),
        (json!({"new_source": "x"}), "cell_index is required"),
        (
            json!({"edit_mode": "insert", "new_source": "x"}),
            "cell_index is required",
        ),
        (
            json!({"edit_mode": "delete", "cell_index": 3}),
            "out of range",
        ),
        (
            json!({"edit_mode": "insert", "cell_index": 4, "new_source": "x"}),
            "out of range",
        ),
    ] {
        let err = notebook_preview(&nb, &args).unwrap_err().to_string();
        assert!(err.contains(want), "{args}: {err}");
        assert!(err.contains("3 cell(s)"), "{args}: {err}");
    }
    let empty = json!({"cells": []}).to_string();
    let err = notebook_preview(&empty, &json!({"edit_mode": "delete", "cell_index": 0}))
        .unwrap_err()
        .to_string();
    assert!(err.contains("no cells"), "{err}");
}

#[test]
fn a_valid_cell_index_still_works_in_every_form() {
    let nb = three_cell_notebook();
    let cells = |s: String| serde_json::from_str::<Value>(&s).unwrap()["cells"].clone();
    // Deleting the last cell by its real index.
    let out =
        cells(notebook_preview(&nb, &json!({"edit_mode": "delete", "cell_index": 2})).unwrap());
    assert_eq!(out.as_array().unwrap().len(), 2);
    assert_eq!(out[0]["source"][0], "a = 1\n");
    // Models sometimes send `2.0` or `"2"`.
    for idx in [json!(2.0), json!("2")] {
        let out =
            cells(notebook_preview(&nb, &json!({"cell_index": idx, "new_source": "z"})).unwrap());
        assert_eq!(out[2]["source"][0], "z");
        assert_eq!(out[0]["source"][0], "a = 1\n");
    }
    // Insert at the cell count appends.
    let out = cells(
        notebook_preview(
            &nb,
            &json!({"edit_mode": "insert", "cell_index": 3, "new_source": "end"}),
        )
        .unwrap(),
    );
    assert_eq!(out[3]["source"][0], "end");
}

#[test]
fn changing_a_cells_type_keeps_it_valid_nbformat() {
    let nb = three_cell_notebook();
    let cells = |s: String| serde_json::from_str::<Value>(&s).unwrap()["cells"].clone();
    // Code to markdown drops the code-only keys.
    let out = cells(
        notebook_preview(
            &nb,
            &json!({"cell_index": 0, "cell_type": "markdown", "new_source": "# A"}),
        )
        .unwrap(),
    );
    assert_eq!(out[0]["cell_type"], "markdown");
    let keys = out[0].as_object().unwrap();
    assert!(
        !keys.contains_key("outputs") && !keys.contains_key("execution_count"),
        "{keys:?}"
    );
    // Markdown to code gains them, empty.
    let out = cells(
        notebook_preview(
            &nb,
            &json!({"cell_index": 1, "cell_type": "code", "new_source": "b = 2"}),
        )
        .unwrap(),
    );
    assert_eq!(out[1]["outputs"], json!([]));
    assert!(out[1].as_object().unwrap().contains_key("execution_count"));
    assert_eq!(out[1]["execution_count"], Value::Null);
}
