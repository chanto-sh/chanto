use super::*;

#[cfg(unix)]
#[test]
fn user_config_is_written_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let path = home_dir().join(format!("mcp-cli-perm-{}.toml", std::process::id()));
    write_config(&path, "[mcp_servers.a]\ncommand = \"x\"\n").unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    let _ = std::fs::remove_file(&path);
    assert_eq!(mode, 0o600, "the user config must be owner-only");
}

#[test]
fn add_and_remove_keep_other_content() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("chanto-mcpcli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".chanto")).unwrap();
    let file = dir.join(".chanto/config.local.toml");
    std::fs::write(
        &file,
        "# my settings\nmodel = \"qwen3:8b\"\n\n[mcp_servers.keep]\ncommand = \"keep-me\"\n",
    )
    .unwrap();
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    add(
        &s(&[
            "-e",
            "TOKEN=abc",
            "fs",
            "--",
            "npx",
            "-y",
            "@mcp/fs",
            "--root",
            ".",
        ]),
        &dir,
    )
    .unwrap();
    add(
        &s(&[
            "-H",
            "Authorization: Bearer x",
            "docs",
            "https://example.com/mcp",
        ]),
        &dir,
    )
    .unwrap();
    add(&s(&["-t", "sse", "old", "https://example.com/sse"]), &dir).unwrap();
    assert!(
        add(&s(&["fs", "--", "other"]), &dir).is_err(),
        "duplicate names are refused"
    );
    let servers = servers_in(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(
        servers["fs"]["args"].as_array().unwrap().len(),
        4,
        "flags after -- belong to the server"
    );
    assert_eq!(servers["fs"]["env"]["TOKEN"].as_str(), Some("abc"));
    assert_eq!(
        servers["docs"]["url"].as_str(),
        Some("https://example.com/mcp")
    );
    assert_eq!(
        servers["docs"]["headers"]["Authorization"].as_str(),
        Some("Bearer x")
    );
    assert_eq!(servers["old"]["transport"].as_str(), Some("sse"));
    remove(&s(&["fs"]), &dir).unwrap();
    let text = std::fs::read_to_string(&file).unwrap();
    let servers = servers_in(&text).unwrap();
    assert!(
        !servers.contains_key("fs") && servers.contains_key("keep") && servers.contains_key("docs")
    );
    assert!(
        text.starts_with("# my settings\nmodel = \"qwen3:8b\""),
        "{text}"
    );
    assert!(remove(&s(&["nope"]), &dir).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn json_entries_convert() {
    let t = json_to_table(&serde_json::json!({"type": "stdio", "command": "/bin/w", "args": ["--k", "abc"], "env": {"CACHE": "/tmp"}})).unwrap();
    assert_eq!(t["command"].as_str(), Some("/bin/w"));
    assert_eq!(t["args"].as_array().unwrap().len(), 2);
    assert_eq!(t["env"]["CACHE"].as_str(), Some("/tmp"));
    let t = json_to_table(&serde_json::json!({"type": "streamable-http", "url": "https://x/mcp", "headers": {"Authorization": "Bearer t"}, "oauth": {"clientId": "id", "callbackPort": 8080}})).unwrap();
    assert_eq!(transport_of(&t), "http");
    assert_eq!(t["oauth"]["client_id"].as_str(), Some("id"));
    assert_eq!(t["oauth"]["callback_port"].as_integer(), Some(8080));
    assert_eq!(
        transport_of(
            &json_to_table(&serde_json::json!({"type": "sse", "url": "https://x/sse"})).unwrap()
        ),
        "sse"
    );
    assert_eq!(
        transport_of(&json_to_table(&serde_json::json!({"url": "https://x/mcp"})).unwrap()),
        "http",
        "a url alone means http"
    );
    assert!(json_to_table(&serde_json::json!({"type": "ws", "url": "wss://x"})).is_err());
    assert!(json_to_table(&serde_json::json!({"type": "http"})).is_err());
    let e = json_to_table(&serde_json::json!({"type": "http", "url": "api.example.com/mcp"}))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("no http:// or https:// scheme") && e.contains("https://api.example.com/mcp"),
        "{e}"
    );
    assert!(json_to_table(&serde_json::json!({"url": "localhost:8080/mcp"})).is_err());
    assert!(json_to_table(&serde_json::json!({"url": "HTTPS://x/mcp"})).is_ok());
    assert!(
        json_to_table(&serde_json::json!({"url": "${MCP_URL}/mcp"})).is_ok(),
        "a variable may hold the scheme"
    );
    assert!(json_to_table(&serde_json::json!({"args": ["x"]})).is_err());
    assert!(json_to_table(&serde_json::json!({"command": "x", "env": {"A": 1}})).is_err());
    let block = render_table(
        "srv",
        &json_to_table(&serde_json::json!({"command": "x", "env": {"A": "1"}})).unwrap(),
    )
    .unwrap();
    let servers = servers_in(&block).unwrap();
    assert_eq!(servers["srv"]["env"]["A"].as_str(), Some("1"), "{block}");
}

#[test]
fn picking_and_naming() {
    let names: Vec<String> = ["a", "b", "c"].map(String::from).to_vec();
    assert_eq!(pick("", &names).unwrap(), names);
    assert_eq!(pick("all", &names).unwrap(), names);
    assert!(pick("none", &names).unwrap().is_empty());
    assert_eq!(pick("1, c 1", &names).unwrap(), vec!["a", "c"]);
    assert!(pick("4", &names).is_err() && pick("zz", &names).is_err());
    let taken = |n: &str| n == "x" || n == "x_1";
    assert_eq!(unique_name("x", &taken), "x_2");
    assert_eq!(unique_name("y", &taken), "y");
    assert!(valid_name("my-server_2") && !valid_name("my server") && !valid_name(""));
}

#[test]
fn add_json_and_import_from_claude_desktop() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir =
        std::env::temp_dir().join(format!("chanto-mcpcli2-{}", uuid::Uuid::new_v4().simple()));
    let home = dir.join("home");
    let desktop = home.join("Library/Application Support/Claude");
    std::fs::create_dir_all(&desktop).unwrap();
    std::fs::create_dir_all(dir.join("proj")).unwrap();
    let proj = dir.join("proj");
    let prev = std::env::var_os("CHANTO_COMPAT_HOME");
    unsafe { std::env::set_var("CHANTO_COMPAT_HOME", &home) };
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    add_json(
        &s(&[
            "weather",
            r#"{"type":"http","url":"https://api.example.com/mcp"}"#,
        ]),
        &proj,
    )
    .unwrap();
    assert!(
        add_json(&s(&["weather", r#"{"command":"x"}"#]), &proj).is_err(),
        "duplicates are refused"
    );
    assert!(add_json(&s(&["bad name", r#"{"command":"x"}"#]), &proj).is_err());
    assert!(
        add_json(&s(&["w", r#"{"mcpServers":{}}"#]), &proj).is_err(),
        "the wrapper is refused"
    );
    std::fs::write(
        desktop.join("claude_desktop_config.json"),
        r#"{"mcpServers": {"weather": {"command": "npx", "args": ["-y", "w"]}, "fs": {"command": "fs-srv"}, "has space": {"command": "x"}}}"#,
    )
    .unwrap();
    // The project isn't trusted: the server is listed, marked as not loaded.
    let listed = list(&proj, false).unwrap();
    assert!(listed.contains("weather: https://api.example.com/mcp (http)  (untrusted folder: not loaded until you trust it)"), "{listed}");
    let listed = list(&proj, true).unwrap();
    assert!(
        listed.contains("weather: https://api.example.com/mcp (http)")
            && !listed.contains("untrusted"),
        "{listed}"
    );
    let e = add_json(
        &s(&["noscheme", r#"{"type":"http","url":"api.example.com/mcp"}"#]),
        &proj,
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("no http:// or https:// scheme"), "{e}");
    let out = add_from_claude_desktop(&s(&["--all"]), &proj).unwrap();
    assert!(out.contains("Imported weather as weather_1"), "{out}");
    assert!(out.contains("Imported fs"), "{out}");
    assert!(out.contains("Skipped \"has space\""), "{out}");
    let servers =
        servers_in(&std::fs::read_to_string(proj.join(".chanto/config.local.toml")).unwrap())
            .unwrap();
    assert_eq!(
        servers["weather"]["url"].as_str(),
        Some("https://api.example.com/mcp")
    );
    assert_eq!(servers["weather_1"]["command"].as_str(), Some("npx"));
    assert!(servers.contains_key("fs") && !servers.contains_key("has space"));
    // Picking by name, into another scope.
    let out = add_from_claude_desktop(&s(&["-s", "project", "fs"]), &proj).unwrap();
    assert!(
        out.contains("Imported fs as fs_1"),
        "the local one is taken: {out}"
    );
    match prev {
        Some(v) => unsafe { std::env::set_var("CHANTO_COMPAT_HOME", v) },
        None => unsafe { std::env::remove_var("CHANTO_COMPAT_HOME") },
    }
    let _ = std::fs::remove_dir_all(&dir);
}
