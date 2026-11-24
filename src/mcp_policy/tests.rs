use super::*;
use serde_json::json;

fn stdio(cmd: &[&str]) -> McpServerConfig {
    McpServerConfig {
        command: cmd[0].into(),
        args: cmd[1..].iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}
fn remote(url: &str) -> McpServerConfig {
    McpServerConfig {
        url: Some(url.into()),
        ..Default::default()
    }
}

#[test]
fn url_patterns_follow_claude_code() {
    assert!(url_matches(
        "https://mcp.example.com/*",
        "https://mcp.example.com/api"
    ));
    assert!(url_matches(
        "https://Mcp.Example.com/*",
        "https://mcp.example.com./api"
    ));
    assert!(url_matches(
        "https://mcp.example.com",
        "https://mcp.example.com/any/path"
    ));
    assert!(
        !url_matches(
            "https://mcp.example.com/*",
            "https://mcp.example.com:8443/api"
        ),
        "full host: default port only"
    );
    assert!(url_matches(
        "https://mcp.example.com:8443/*",
        "https://mcp.example.com:8443/api"
    ));
    assert!(url_matches(
        "https://mcp.example.com:*/*",
        "https://mcp.example.com:8443/api"
    ));
    assert!(
        url_matches("https://*.example.com/*", "https://a.example.com:9000/x"),
        "wildcard host: any port"
    );
    assert!(!url_matches(
        "https://*.example.com/*",
        "https://example.org/x"
    ));
    assert!(url_matches(
        "http://localhost:*/*",
        "http://localhost:3000/mcp"
    ));
    assert!(url_matches(
        "*://mcp.example.com/*",
        "http://mcp.example.com/x"
    ));
    assert!(!url_matches(
        "*://mcp.example.com/*",
        "http://mcp.example.com:8080/x"
    ));
    assert!(
        !url_matches(
            "https://mcp.example.com/api/*",
            "https://mcp.example.com/API/x"
        ),
        "paths are case-sensitive"
    );
}

#[test]
fn denylist_wins_and_allowlist_is_by_type() {
    let deny = json!({"deniedMcpServers": [{"serverName": "evil"}, {"serverUrl": "https://bad.example.com/*"}]});
    let allow = json!({"allowedMcpServers": [{"serverCommand": ["npx", "-y", "ok"]}, {"serverName": "named"}, {"serverUrl": "https://good.example.com/*"}]});
    let p = Policy::merge(std::slice::from_ref(&allow), std::slice::from_ref(&deny));
    assert!(
        p.check("evil", &stdio(&["npx", "-y", "ok"])).is_err(),
        "deny by name beats an allowed command"
    );
    assert!(
        p.check("x", &remote("https://bad.example.com/mcp"))
            .is_err()
    );
    assert!(p.check("x", &stdio(&["npx", "-y", "ok"])).is_ok());
    assert!(
        p.check("x", &stdio(&["npx", "ok"])).is_err(),
        "commands match exactly"
    );
    // A name counts only when the list has no entries of the server's own kind.
    assert!(p.check("named", &stdio(&["other"])).is_err());
    assert!(
        p.check("x", &remote("https://good.example.com/mcp"))
            .is_ok()
    );
    let names_only = Policy::merge(
        &[json!({"allowedMcpServers": [{"serverName": "named"}]})],
        &[],
    );
    assert!(names_only.check("named", &stdio(&["anything"])).is_ok());
    assert!(names_only.check("other", &stdio(&["anything"])).is_err());
    // Unset allows everything; an empty list allows nothing.
    assert!(Policy::merge(&[], &[]).check("x", &stdio(&["a"])).is_ok());
    assert!(
        Policy::merge(&[json!({"allowedMcpServers": []})], &[])
            .check("x", &stdio(&["a"]))
            .is_err()
    );
}

#[test]
fn managed_only_ignores_other_allowlists() {
    let managed =
        json!({"allowManagedMcpServersOnly": true, "allowedMcpServers": [{"serverName": "corp"}]});
    let user = json!({"allowedMcpServers": [{"serverName": "mine"}], "deniedMcpServers": [{"serverName": "corp2"}]});
    let p = Policy::merge(std::slice::from_ref(&managed), std::slice::from_ref(&user));
    assert!(p.check("mine", &stdio(&["a"])).is_err());
    assert!(p.check("corp", &stdio(&["a"])).is_ok());
    assert_eq!(p.denied.len(), 1, "denylists still merge from every scope");
    let soft = Policy::merge(
        &[json!({"allowedMcpServers": [{"serverName": "corp"}]})],
        &[user],
    );
    assert!(
        soft.check("mine", &stdio(&["a"])).is_ok(),
        "without the lock, users can broaden it"
    );
}

#[test]
fn a_trusted_folder_asks_again_when_a_server_changes_or_appears() {
    let first = json!({"command": "linter"});
    let later = json!({"command": "linter", "args": ["--other"]});
    let mut a = Approvals {
        trusted: true,
        trusted_here: true,
        ..Default::default()
    };
    assert_eq!(
        a.decide_server("lint", &first),
        Approval::Approved,
        "nothing recorded yet: starts from what's there"
    );
    a.digests.insert("lint".into(), server_digest(&first));
    assert_eq!(a.decide_server("lint", &first), Approval::Approved);
    assert_eq!(a.decide_server("lint", &later), Approval::Changed);
    assert_eq!(a.decide_server("added", &first), Approval::Changed);
    // A repository's own settings can't approve what it just changed.
    a.repo = vec!["lint".into()];
    assert_eq!(a.decide_server("lint", &later), Approval::Changed);
    // The user's own blanket setting is what it says.
    a.user_all = true;
    assert_eq!(a.decide_server("lint", &later), Approval::Approved);
    // An untrusted folder still doesn't start anything.
    let none = Approvals::default();
    assert_eq!(none.decide_server("lint", &first), Approval::Pending);
}

#[test]
fn mcpjson_approvals() {
    let mut a = Approvals {
        user: vec!["mine".into()],
        repo: vec!["repo".into()],
        rejected: vec!["no".into()],
        ..Default::default()
    };
    assert_eq!(
        a.decide("mine"),
        Approval::Pending,
        "a project server runs repo-shipped code: untrusted folder asks even when the user named it"
    );
    assert_eq!(
        a.decide("repo"),
        Approval::Pending,
        "a cloned repo can't approve its own servers"
    );
    assert_eq!(a.decide("other"), Approval::Pending);
    a.trusted = true;
    assert_eq!(a.decide("mine"), Approval::Approved);
    assert_eq!(a.decide("repo"), Approval::Approved);
    assert_eq!(a.decide("other"), Approval::Pending);
    a.trusted_here = true;
    assert_eq!(a.decide("other"), Approval::Approved);
    assert_eq!(
        a.decide("no"),
        Approval::Rejected,
        "disabledMcpjsonServers always rejects"
    );
    // enableAllProjectMcpServers doesn't start a cloned repo's servers
    // until the folder is trusted.
    let mut all = Approvals {
        user_all: true,
        ..Default::default()
    };
    assert_eq!(all.decide("anything"), Approval::Pending);
    all.trusted = true;
    assert_eq!(all.decide("anything"), Approval::Approved);
}

#[test]
fn gates_and_project_state() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("chanto-mcppol-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    set_disabled(&dir, "a", true).unwrap();
    set_mcpjson_approval(&dir, "b", true).unwrap();
    let st = project_state(&dir);
    assert_eq!(st.disabled_mcp_servers, vec!["a"]);
    assert_eq!(st.enabled_mcpjson_servers, vec!["b"]);
    let mut servers: BTreeMap<String, McpServerConfig> = [
        ("a".into(), stdio(&["x"])),
        ("c".into(), stdio(&["y"])),
        ("d".into(), stdio(&["z"])),
    ]
    .into();
    let p = Policy::merge(
        &[json!({"deniedMcpServers": [{"serverCommand": ["y"]}]})],
        &[],
    );
    gate_servers(
        &mut servers,
        &p,
        &st.disabled_mcp_servers,
        &["d".to_string()],
    );
    assert_eq!(servers["a"].gate, Some(Gate::Disabled));
    assert!(matches!(servers["c"].gate, Some(Gate::Policy(_))));
    assert_eq!(servers["d"].gate, Some(Gate::DisabledInClaude));
    set_disabled(&dir, "a", false).unwrap();
    set_mcpjson_approval(&dir, "b", false).unwrap();
    let st = project_state(&dir);
    assert!(st.disabled_mcp_servers.is_empty() && st.enabled_mcpjson_servers.is_empty());
    assert_eq!(st.disabled_mcpjson_servers, vec!["b"]);
    update_project(&dir, |p| *p = ProjectMcp::default()).unwrap();
    assert!(
        !read_state().contains_key(&project_key(&dir)),
        "empty entries are dropped"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
