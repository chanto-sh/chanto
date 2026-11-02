//! `/mcp` with servers behind it: listing, turning them off and on for the
//! project, reconnecting, and what each refuses.

use super::*;
use crate::agent::{EnvInit, Frontend, PermissionReply, PermissionRequest};
use crate::config::{Config, McpServerConfig};
use crate::llm::{Provider, ProviderKind};
use crate::mcp::{McpClient, Status};
use crate::mcp_policy::Gate;
use crate::test_support::isolated;
use std::sync::Arc;

struct Quiet;

#[async_trait::async_trait]
impl Frontend for Quiet {
    fn emit(&self, _: Event) {}
    async fn permission(&self, _: PermissionRequest) -> PermissionReply {
        PermissionReply::Deny(None)
    }
    async fn question(&self, _: &str, _: &[String]) -> Option<String> {
        None
    }
}

async fn show(env: &Arc<Env>, line: &str) -> String {
    match handle_env(env, line).await {
        Some(Outcome::Show(s)) => s,
        _ => panic!("{line} didn't show anything"),
    }
}

fn missing() -> McpServerConfig {
    McpServerConfig {
        command: "definitely-not-a-real-mcp-server-7f3a".into(),
        ..Default::default()
    }
}

#[cfg(unix)]
#[test]
fn servers_are_turned_off_and_on_for_the_project() {
    if isolated("commands::mcp_tests::servers_are_turned_off_and_on_for_the_project") {
        return;
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let dir = crate::agent::tests::tmpdir("cmd-mcp-toggle");
        let live_cfg = crate::mcp::tests::slow_server(0.0);
        let off_cfg = McpServerConfig {
            disabled: true,
            ..missing()
        };
        let gated_cfg = McpServerConfig {
            gate: Some(Gate::Policy("blocked by deniedMcpServers".into())),
            ..missing()
        };
        let mut config = Config::default();
        config.provider = ProviderKind::Openai;
        for (n, c) in [
            ("live", &live_cfg),
            ("cfgoff", &off_cfg),
            ("gated", &gated_cfg),
            ("broken", &missing()),
            ("paused", &missing()),
        ] {
            config.mcp_servers.insert(n.into(), c.clone());
        }
        config
            .origins
            .insert("mcp_servers.cfgoff".into(), "/etc/chanto.toml".into());
        let live = McpClient::connect("live", &live_cfg).await.unwrap();
        let mcp = vec![
            live.clone(),
            McpClient::offline("cfgoff", &off_cfg, Status::Off("disabled".into())),
            McpClient::offline(
                "gated",
                &gated_cfg,
                Status::Off(gated_cfg.gate.clone().unwrap().describe()),
            ),
            McpClient::offline(
                "broken",
                &missing(),
                Status::Failed("MCP server `broken` exited".into()),
            ),
            McpClient::offline("paused", &missing(), Status::Off(Gate::Disabled.describe())),
        ];
        let env = Env::new(EnvInit {
            config,
            provider: Provider::new(ProviderKind::Openai, "http://127.0.0.1:1/v1", None, 32768),
            model: "mock".into(),
            mode: Mode::Default,
            vision: false,
            cwd: dir.clone(),
            tools: crate::tools::Registry::builtin(),
            mcp,
            frontend: Arc::new(Quiet),
            interactive: false,
        });

        // The listing: one line per server, with what to do about it.
        let listing = show(&env, "/mcp").await;
        for line in [
            "- **live** ✓ connected — 1 tool · resources\n",
            "- **cfgoff** ⊘ disabled: `disabled = true` in `/etc/chanto.toml`\n",
            "- **gated** ⊘ disabled: blocked by deniedMcpServers\n",
            "- **broken** ✗ failed: the process exited — `/mcp reconnect broken`\n",
            "- **paused** ⊘ disabled: disabled for this project\n",
        ] {
            assert!(listing.contains(line), "missing {line:?} in:\n{listing}");
        }
        let rows = mcp_rows(&env);
        let row = |n: &str| rows.iter().find(|r| r.name == n).unwrap().can_enable;
        assert!(row("paused"), "a project disable is ours to undo");
        assert!(!row("gated") && !row("cfgoff"));
        // Off servers stay out of the ratio; the failed one asks for attention.
        assert_eq!(mcp_summary(&env), (1, 2, true));

        // Stdio servers have no sign-in.
        assert_eq!(
            show(&env, "/mcp logout live").await,
            "`live` had no stored token."
        );
        let login = show(&env, "/mcp login live").await;
        assert!(
            login.starts_with("Login to `live` failed: `live` is a local (stdio) server"),
            "{login}"
        );

        // Off for this project, remembered in the project choices.
        assert_eq!(
            show(&env, "/mcp disable live").await,
            "Disabled `live` for this project."
        );
        assert!(matches!(live.status(), Status::Off(_)));
        let saved = std::fs::read_to_string(crate::config::home_dir().join("mcp-projects.json"))
            .expect("the choice is saved");
        assert!(saved.contains("\"live\""), "{saved}");
        assert_eq!(
            show(&env, "/mcp reconnect live").await,
            "`live` is disabled; `/mcp enable live` turns it on."
        );
        assert_eq!(
            show(&env, "/mcp enable live").await,
            "Enabled `live`: 1 tool."
        );
        assert_eq!(live.status(), Status::Connected);
        let saved =
            std::fs::read_to_string(crate::config::home_dir().join("mcp-projects.json")).unwrap();
        assert!(!saved.contains("\"live\""), "{saved}");
        assert_eq!(
            show(&env, "/mcp reconnect live").await,
            "Reconnected `live`: 1 tool."
        );

        // What `/mcp` can't turn on, it says where to change.
        assert_eq!(
            show(&env, "/mcp enable gated").await,
            "`gated` stays off: blocked by deniedMcpServers."
        );
        assert_eq!(
            show(&env, "/mcp enable cfgoff").await,
            "`cfgoff` has `disabled = true` in `/etc/chanto.toml`; remove that to turn it on."
        );
        let failed = show(&env, "/mcp reconnect broken").await;
        assert!(
            failed.starts_with("`broken` failed to connect: "),
            "{failed}"
        );

        // `all` picks the servers each verb applies to.
        assert_eq!(
            show(&env, "/mcp reconnect all").await.lines().count(),
            1,
            "only the failed server is retried"
        );
        assert_eq!(
            show(&env, "/mcp disable all").await,
            "Disabled `live` for this project.\nDisabled `broken` for this project."
        );
        let enabled = show(&env, "/mcp enable all").await;
        let lines: Vec<&str> = enabled.lines().collect();
        assert_eq!(lines.len(), 5, "{enabled}");
        assert!(lines.contains(&"Enabled `live`: 1 tool."), "{enabled}");
        assert!(lines.contains(&"`gated` stays off: blocked by deniedMcpServers."));
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("`paused` failed to connect: ")),
            "{enabled}"
        );
        assert_eq!(live.status(), Status::Connected);
        live.shutdown(Status::Off("test over".into()));
        let _ = std::fs::remove_dir_all(&dir);
    });
}
