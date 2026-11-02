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
mod tests;
