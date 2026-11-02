//! `mcp_tool` hooks against a stdio MCP server, and against servers that
//! aren't configured or aren't connected.

use super::*;
use crate::agent::{EnvInit, Frontend, PermissionReply, PermissionRequest};
use crate::config::{Config, McpServerConfig};
use crate::llm::{Provider, ProviderKind};
use crate::mcp::{McpClient, Status};

struct Quiet;

#[async_trait::async_trait]
impl Frontend for Quiet {
    fn emit(&self, _: crate::agent::Event) {}
    async fn permission(&self, _: PermissionRequest) -> PermissionReply {
        PermissionReply::Deny(None)
    }
    async fn question(&self, _: &str, _: &[String]) -> Option<String> {
        None
    }
}

/// A server with a `context` tool that answers with text and a `fails` tool
/// that answers with an error result.
fn hook_server() -> McpServerConfig {
    let script = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
*'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"sh"}}}\n' "$id" ;;
*'"method":"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"context","inputSchema":{"type":"object"}},{"name":"fails","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
*'"name":"fails"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"the check broke"}],"isError":true}}\n' "$id" ;;
*'"method":"tools/call"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"  context from the server  "}]}}\n' "$id" ;;
*'"id":'*) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
  esac
done"#;
    McpServerConfig {
        command: "sh".into(),
        args: vec!["-c".into(), script.into()],
        ..Default::default()
    }
}

fn mcp_hook(server: &str, tool: &str) -> HookConfig {
    HookConfig {
        kind: "mcp_tool".into(),
        server: server.into(),
        tool: tool.into(),
        input: Some(json!({"prompt": "${prompt}"})),
        ..Default::default()
    }
}

#[tokio::test]
async fn mcp_tool_hooks_add_context_and_report_failures() {
    let dir = crate::agent::tests::tmpdir("hooks-mcp");
    let cfg = hook_server();
    let live = McpClient::connect("hooksrv", &cfg).await.unwrap();
    let down = McpClient::offline(
        "down",
        &cfg,
        Status::Failed("MCP server `down` exited".into()),
    );
    let mut config = Config::default();
    config.provider = ProviderKind::Openai;
    config.hooks.insert(
        "UserPromptSubmit".into(),
        vec![
            mcp_hook("ghost", "context"),
            mcp_hook("down", "context"),
            mcp_hook("hooksrv", "fails"),
            mcp_hook("hooksrv", "context"),
        ],
    );
    let env = crate::agent::Env::new(EnvInit {
        config,
        provider: Provider::new(ProviderKind::Openai, "http://127.0.0.1:1/v1", None, 32768),
        model: "mock".into(),
        mode: crate::permissions::Mode::Default,
        vision: false,
        cwd: dir.clone(),
        tools: crate::tools::Registry::builtin(),
        mcp: vec![live.clone(), down],
        frontend: std::sync::Arc::new(Quiet),
        interactive: false,
    });
    let res = user_prompt_submit(&env, "fix the bug").await;
    assert!(res.failed, "the broken hooks count as failures");
    assert!(res.block.is_none(), "a failing mcp_tool hook doesn't block");
    assert_eq!(
        res.messages,
        vec![
            "UserPromptSubmit hook failed: MCP server `ghost` is not configured".to_string(),
            "UserPromptSubmit hook failed: MCP server `down` is not connected".to_string(),
            "UserPromptSubmit hook failed: the check broke".to_string(),
        ]
    );
    // The working tool's text is read like a command's stdout: trimmed, and
    // plain text on UserPromptSubmit becomes context.
    assert_eq!(res.context, vec!["context from the server".to_string()]);
    assert_eq!(res.stdout, vec!["context from the server".to_string()]);
    live.shutdown(Status::Off("test over".into()));
    let _ = std::fs::remove_dir_all(&dir);
}
