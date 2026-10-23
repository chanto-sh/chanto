//! `ChantoCloud`: the agent's view of Chanto cloud once you're signed in
//! (`chanto cloud login`): cloud runs of this repository and cloud agents.
//! Listing and reading need no approval; starting or cancelling work does.

use super::{Ctx, Tool, ToolKind, ToolOutput, arg_str};
use crate::runs::cloud::{Cloud, find_run, resolve_org};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chanto_proto::agents::{AgentList, RunAgent};
use chanto_proto::runs::{CreateRun, Run, RunList};
use serde_json::{Value, json};

pub struct ChantoCloud;

const ACTIONS: [&str; 6] = ["runs", "show", "start", "cancel", "agents", "run_agent"];

/// Only offered when signed in, so the model doesn't reach for it otherwise.
pub fn available() -> bool {
    Cloud::signed_in().is_ok()
}

#[async_trait]
impl Tool for ChantoCloud {
    fn name(&self) -> String {
        "ChantoCloud".into()
    }
    fn description(&self) -> String {
        "Chanto cloud, where the user is signed in. Actions: `runs` lists recent cloud runs; `show` gives one run's \
status, summary, branch and pull request (`id`, a prefix is enough); `start` queues a cloud run of this repository's \
current branch with `task` (the branch must be pushed); `cancel` stops a run (`id`); `agents` lists the org's cloud \
agents; `run_agent` starts one (`name`, optional `input`). Starting and cancelling ask the user first. Use it when the \
user asks to hand work to the cloud or about their cloud runs or agents, not for work you can do here."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ACTIONS},
                "id": {"type": "string", "description": "Run id or prefix (show, cancel)"},
                "task": {"type": "string", "description": "What the cloud run should do (start)"},
                "name": {"type": "string", "description": "Agent name (run_agent)"},
                "input": {"type": "string", "description": "Extra instructions for this agent run (run_agent)"},
                "org": {"type": "string", "description": "Org slug; defaults to the user's default org"}
            },
            "required": ["action"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::External
    }
    fn kind_for(&self, args: &Value) -> ToolKind {
        match args["action"].as_str() {
            Some("runs" | "show" | "agents") => ToolKind::ReadOnly,
            _ => ToolKind::External,
        }
    }
    fn label(&self, args: &Value) -> String {
        let detail = ["task", "id", "name"]
            .iter()
            .find_map(|k| args[*k].as_str())
            .map(|s| format!(": {}", crate::util::one_line(s, 60)))
            .unwrap_or_default();
        format!(
            "ChantoCloud({}{detail})",
            args["action"].as_str().unwrap_or("?")
        )
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        match act(&args, ctx).await {
            Ok(text) => {
                let text = crate::tools::bash::strip_ansi(&text);
                let first = text.lines().next().unwrap_or("").to_string();
                Ok(ToolOutput::ok(text).display(first))
            }
            Err(e) => Ok(ToolOutput::err(format!("{e:#}"))),
        }
    }
}

async fn act(args: &Value, ctx: &Ctx) -> Result<String> {
    let c = Cloud::signed_in()
        .context("not signed in to Chanto cloud: the user can run `chanto cloud login`")?;
    let org_flag = args["org"].as_str();
    let action = arg_str(args, "action")?;
    Ok(match action {
        "runs" => {
            let org = resolve_org(&c, org_flag).await?;
            let list: RunList = c
                .get(&format!(
                    "/v1/orgs/{}/runs?limit=20",
                    urlencoding::encode(&org)
                ))
                .await?;
            crate::runs::cloud::table(&list.runs)
        }
        "show" => {
            crate::runs::cloud::detail_text(&find_run(&c, org_flag, arg_str(args, "id")?).await?)
        }
        "cancel" => {
            let run = find_run(&c, org_flag, arg_str(args, "id")?).await?;
            let r: Run = c
                .post(
                    &format!("/v1/runs/{}/cancel", urlencoding::encode(&run.id)),
                    &json!({}),
                )
                .await?;
            format!("Cloud run {} is now {:?}.", r.id, r.status).to_lowercase()
        }
        "start" => {
            let task = arg_str(args, "task")?.to_string();
            let cwd = ctx.cwd();
            // Not pushing: the user decides what leaves the machine; the branch must already be on origin.
            let src =
                tokio::task::spawn_blocking(move || crate::runs::cloud::local_source(&cwd, false))
                    .await??;
            let org = resolve_org(&c, org_flag).await?;
            let req = CreateRun {
                repo: src.repo.clone(),
                git_ref: Some(src.branch.clone()),
                task,
                ..Default::default()
            };
            let run = c.submit(&org, &req).await?;
            crate::runs::cloud::queued_message(&run)
        }
        "agents" => {
            let org = resolve_org(&c, org_flag).await?;
            let list: AgentList = c
                .get(&format!(
                    "/v1/orgs/{}/agents",
                    crate::chanto_cloud::agents::seg(&org)
                ))
                .await?;
            crate::chanto_cloud::agents::list_table(&list.agents)
        }
        "run_agent" => {
            let name = arg_str(args, "name")?;
            let org = resolve_org(&c, org_flag).await?;
            let body = RunAgent {
                input: args["input"]
                    .as_str()
                    .map(str::to_string)
                    .filter(|s| !s.trim().is_empty()),
                git_ref: None,
            };
            let run: Run = c
                .post(
                    &format!(
                        "/v1/orgs/{}/agents/{}/runs",
                        crate::chanto_cloud::agents::seg(&org),
                        crate::chanto_cloud::agents::seg(name)
                    ),
                    &body,
                )
                .await
                .with_context(|| format!("starting agent {name} in org {org}"))?;
            format!(
                "Started run {} of agent {name}. `show` with that id reports on it.",
                run.id
            )
        }
        other => bail!("unknown action {other}; use one of {}", ACTIONS.join(", ")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_json(id: &str, status: &str) -> Value {
        json!({"id": id, "org": "acme", "status": status, "repo": "https://github.com/acme/app.git", "git_ref": "main",
               "task": "fix the flaky test", "placement": "auto", "labels": [], "runner_kind": null, "attempts": 1,
               "cancel_requested": false, "created_at": "2026-10-15T10:00:00Z", "started_at": null, "finished_at": null, "result": null})
    }

    #[tokio::test]
    async fn lists_and_shows_runs_and_starts_agents() {
        let _lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let app = axum::Router::new()
            .route("/v1/orgs/acme/runs", axum::routing::get(|| async { axum::Json(json!({"runs": [run_json("0f9e8d7c-0000-0000-0000-000000000001", "running")]})) }))
            .route("/v1/orgs/acme/agents", axum::routing::get(|| async { axum::Json(json!({"agents": []})) }))
            .route(
                "/v1/orgs/acme/agents/docs/runs",
                axum::routing::post(|axum::Json(b): axum::Json<Value>| async move {
                    assert_eq!(b["input"], "check issue 42");
                    axum::Json(run_json("ab12cd34-0000-0000-0000-000000000002", "queued"))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        unsafe {
            std::env::set_var("CHANTO_CLOUD_TOKEN", "hct_test");
            std::env::set_var("CHANTO_CLOUD_URL", &url);
        }
        assert!(available());
        let dir = std::env::temp_dir();
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
        let out = ChantoCloud
            .run(json!({"action": "runs", "org": "acme"}), &ctx)
            .await
            .unwrap();
        assert!(
            !out.is_error
                && out.content.contains("0f9e8d7c")
                && out.content.contains("fix the flaky test"),
            "{}",
            out.content
        );
        let out = ChantoCloud
            .run(
                json!({"action": "show", "id": "0f9e8d7c", "org": "acme"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            out.content
                .contains("Cloud run 0f9e8d7c-0000-0000-0000-000000000001"),
            "{}",
            out.content
        );
        let out = ChantoCloud
            .run(json!({"action": "agents", "org": "acme"}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("No agents yet"), "{}", out.content);
        let out = ChantoCloud.run(json!({"action": "run_agent", "name": "docs", "input": "check issue 42", "org": "acme"}), &ctx).await.unwrap();
        assert!(
            !out.is_error && out.content.contains("ab12cd34"),
            "{}",
            out.content
        );
        // Reading is free; starting or cancelling asks.
        assert_eq!(
            ChantoCloud.kind_for(&json!({"action": "runs"})),
            ToolKind::ReadOnly
        );
        assert_eq!(
            ChantoCloud.kind_for(&json!({"action": "start"})),
            ToolKind::External
        );
        assert_eq!(
            ChantoCloud.kind_for(&json!({"action": "cancel"})),
            ToolKind::External
        );
        unsafe {
            std::env::remove_var("CHANTO_CLOUD_TOKEN");
            std::env::remove_var("CHANTO_CLOUD_URL");
        }
    }
}
