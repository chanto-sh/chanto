use super::envfile::plain_assignments;
use super::*;

#[test]
fn matchers_follow_claude_code() {
    assert!(matcher_hits("", "Bash"));
    assert!(matcher_hits("*", "Bash"));
    assert!(matcher_hits("Bash", "Bash"));
    assert!(!matcher_hits("Bash", "BashOutput"));
    assert!(matcher_hits("Edit|Write", "Write"));
    assert!(matcher_hits("Edit, Write", "Write"));
    assert!(matcher_hits("Edit,Write", "Edit"));
    assert!(!matcher_hits("Edit", "NotebookEdit"));
    // Anything else is an unanchored regex.
    assert!(matcher_hits("Edit.*", "NotebookEdit"));
    assert!(!matcher_hits("^Edit$", "NotebookEdit"));
    assert!(matcher_hits("^Bash$", "Bash"));
    assert!(matcher_hits("mcp__memory__.*", "mcp__memory__save"));
    assert!(!matcher_hits("mcp__memory", "mcp__memory__save"));
    // apply_patch counts as an edit.
    assert!(matcher_hits("Edit|Write", "apply_patch"));
    assert!(matcher_hits("Write,Edit", "apply_patch"));
    assert!(matcher_hits("^(Edit|Write)$", "apply_patch"));
    assert!(!matcher_hits("Bash", "apply_patch"));
}

#[test]
fn if_conditions_use_permission_rules() {
    let bash = |c: &str| json!({ "command": c });
    let ev = "PreToolUse";
    let none: &[String] = &[];
    assert_eq!(
        condition_holds("Bash(git *)", ev, Some("Bash"), &bash("git push"), none),
        Some(true)
    );
    assert_eq!(
        condition_holds("Bash(git:*)", ev, Some("Bash"), &bash("git push"), none),
        Some(true)
    );
    assert_eq!(
        condition_holds("Bash(git *)", ev, Some("Bash"), &bash("npm test"), none),
        Some(false)
    );
    assert_eq!(
        condition_holds(
            "Bash(git *)",
            ev,
            Some("Bash"),
            &bash("npm test && git push"),
            none
        ),
        Some(true)
    );
    assert_eq!(
        condition_holds(
            "Bash(git *)",
            ev,
            Some("Bash"),
            &bash("FOO=1 git status"),
            none
        ),
        Some(true)
    );
    assert_eq!(
        condition_holds(
            "Bash(rm *)",
            ev,
            Some("Bash"),
            &bash("echo $(rm -rf /tmp/x)"),
            none
        ),
        Some(true)
    );
    assert_eq!(
        condition_holds("Bash(rm *)", ev, Some("Bash"), &bash("echo `rm x`"), none),
        Some(true)
    );
    assert_eq!(
        condition_holds("Bash(git *)", ev, Some("Read"), &json!({}), none),
        Some(false)
    );
    assert_eq!(
        condition_holds("Bash", "PostToolUse", Some("Bash"), &bash("ls"), none),
        Some(true)
    );
    assert_eq!(
        condition_holds("Bash", ev, Some("Read"), &json!({}), none),
        Some(false)
    );
    // When what runs is unclear, the hook runs (as in Claude Code).
    assert_eq!(
        condition_holds(
            "Bash(rm *)",
            ev,
            Some("Bash"),
            &bash("echo $(a $(b))"),
            none
        ),
        Some(true)
    );
    // Other tools use their permission subjects.
    let paths = |p: &str| vec![p.to_string()];
    assert_eq!(
        condition_holds(
            "Edit(**/*.ts)",
            ev,
            Some("Write"),
            &json!({}),
            &paths("src/a.ts")
        ),
        Some(true)
    );
    assert_eq!(
        condition_holds(
            "Edit(**/*.ts)",
            ev,
            Some("Write"),
            &json!({}),
            &paths("src/a.rs")
        ),
        Some(false)
    );
    // Not evaluable: the caller skips the hook and warns.
    assert_eq!(
        condition_holds("Bash(git *)", "Stop", None, &json!({}), none),
        None
    );
    assert_eq!(
        condition_holds("Bash(git *)", "UserPromptSubmit", None, &json!({}), none),
        None
    );
    assert_eq!(
        condition_holds(
            "Bash(git *) && Edit",
            ev,
            Some("Bash"),
            &bash("git x"),
            none
        ),
        None
    );
    assert_eq!(
        condition_holds("Bash(git", ev, Some("Bash"), &bash("git x"), none),
        None
    );
    assert_eq!(
        condition_holds("", ev, Some("Bash"), &bash("git x"), none),
        None
    );
    assert!(condition_is_valid("Edit(*.ts)"));
    assert!(condition_is_valid("mcp__memory__save"));
    assert!(!condition_is_valid("Bash(a), Edit(b)"));
}

#[test]
fn env_file_plain_assignments() {
    let ok = |s: &str| plain_assignments(s).unwrap();
    let text = "# set by a hook\n\nexport A=1\ndeclare -x B=x/y:z,@%+=.-\nC=\nexport D='two words $x'\nE=\"say \\\"hi\\\" \\$HOME \\`id\\` a\\\\b \\n\"\nF=a'b c'\"d\"\nA=later\n";
    assert_eq!(
        ok(text),
        vec![
            ("A".into(), "1".into()),
            ("B".into(), "x/y:z,@%+=.-".into()),
            ("C".into(), "".into()),
            ("D".into(), "two words $x".into()),
            ("E".into(), "say \"hi\" $HOME `id` a\\b \\n".into()),
            ("F".into(), "ab cd".into()),
            ("A".into(), "later".into()),
        ]
    );
    assert_eq!(ok(""), vec![]);
    assert_eq!(ok("  # only a comment\n\n"), vec![]);
    for bad in [
        "export PATH=\"$PATH:./node_modules/.bin\"",
        "export PATH=$PATH:/x",
        "source ~/.nvm/nvm.sh",
        ". ./env.sh",
        "export X=$'a\\nb'",
        "X=\"`id`\"",
        "X=a b",
        "X='unterminated",
        "X=\"unterminated",
        "1X=a",
        "export",
        "X=a # comment",
        "X=~/bin",
        "export A=1\nnvm use 20",
    ] {
        assert_eq!(plain_assignments(bad), None, "{bad}");
    }
}

#[test]
fn tool_response_is_an_object() {
    let r = tool_response("Bash", &json!({"command": "ls"}), "a\nb");
    assert_eq!(r["stdout"], "a\nb");
    assert_eq!(r["stderr"], "");
    assert_eq!(r["interrupted"], false);
    assert_eq!(
        tool_response("Bash", &json!({}), "(no output)")["stdout"],
        ""
    );
    assert_eq!(
        tool_response("Bash", &json!({}), "x\n[interrupted by user]")["interrupted"],
        true
    );
    let w = tool_response(
        "Write",
        &json!({"file_path": "/t/a", "content": "hi"}),
        "File created: /t/a (1 lines).",
    );
    assert_eq!(w["type"], "create");
    assert_eq!(w["filePath"], "/t/a");
    assert_eq!(
        tool_response(
            "Write",
            &json!({"file_path": "/t/a"}),
            "File /t/a overwritten (1 lines)."
        )["type"],
        "update"
    );
    let e = tool_response(
        "Edit",
        &json!({"file_path": "/t/a", "old_string": "x", "new_string": "y"}),
        "updated",
    );
    assert_eq!(e["oldString"], "x");
    assert_eq!(e["replaceAll"], false);
    assert_eq!(
        tool_response("Read", &json!({"file_path": "/t/a"}), "1\thi")["file"]["content"],
        "1\thi"
    );
    assert_eq!(
        tool_response("WebSearch", &json!({}), "results")["output"],
        "results"
    );
}

#[test]
fn http_headers_expand_only_allowed_vars() {
    unsafe { std::env::set_var("CHANTO_HOOK_TEST_TOKEN", "s3cret") };
    let allowed = vec!["CHANTO_HOOK_TEST_TOKEN".to_string()];
    assert_eq!(
        expand_header("Bearer $CHANTO_HOOK_TEST_TOKEN", &allowed),
        "Bearer s3cret"
    );
    assert_eq!(
        expand_header("Bearer ${CHANTO_HOOK_TEST_TOKEN}", &allowed),
        "Bearer s3cret"
    );
    assert_eq!(
        expand_header("Bearer $CHANTO_HOOK_TEST_TOKEN", &[]),
        "Bearer "
    );
    assert_eq!(expand_header("plain", &[]), "plain");
}

#[tokio::test]
async fn http_hook_posts_payload_and_maps_the_response() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut bodies = Vec::new();
        for (status, reply) in [
            ("200 OK", r#"{"decision":"block","reason":"no"}"#),
            ("200 OK", ""),
            ("200 OK", "plain text"),
            ("500 Internal Server Error", "{}"),
        ] {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            // Read the request headers and its Content-Length body.
            loop {
                let n = s.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(i) = text.find("\r\n\r\n") {
                    let len = text[..i]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if buf.len() >= i + 4 + len {
                        bodies.push(text[i + 4..].to_string());
                        break;
                    }
                }
            }
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
            s.write_all(resp.as_bytes()).await.unwrap();
            let _ = s.shutdown().await;
        }
        bodies
    });
    let h = HookConfig {
        kind: "http".into(),
        url: format!("http://{addr}/hook"),
        timeout_secs: Some(10),
        ..Default::default()
    };
    let payload = json!({"hook_event_name": "PreToolUse", "tool_name": "Bash"});
    let (code, out, _) = run_http(&h, &payload).await.unwrap();
    assert_eq!(code, 0);
    assert!(out.contains("\"block\""), "{out}");
    assert_eq!(
        run_http(&h, &payload).await.unwrap(),
        (0, String::new(), String::new())
    );
    assert_eq!(run_http(&h, &payload).await.unwrap().0, 1);
    assert_eq!(run_http(&h, &payload).await.unwrap().0, 1);
    let bodies = server.await.unwrap();
    assert!(bodies[0].contains("\"tool_name\":\"Bash\""), "{bodies:?}");
}
