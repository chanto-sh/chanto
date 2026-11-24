//! Task: delegate work to a subagent with a fresh context window.

use super::{Ctx, Tool, ToolKind, ToolOutput, arg_str};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct Task;

#[async_trait]
impl Tool for Task {
    fn name(&self) -> String {
        "Task".into()
    }
    fn description(&self) -> String {
        "Launch a subagent with its own context window to handle a self-contained task autonomously, \
returning only its final report. Use it for broad codebase exploration (subagent_type \"Explore\": \
read-only, fast) or multi-step side tasks (\"general-purpose\") so your own context stays small. \
For a second opinion on a hard decision, a plan, or your changes before you finish, use \"oracle\": a read-only \
reviewer (possibly a stronger model) that hunts for bugs and flawed assumptions; give it the plan or the changed \
files and what they should do. Launch several in parallel for independent work. The subagent cannot see this conversation: give a \
complete, specific prompt and say exactly what to report back. run_in_background: true returns at once and \
delivers the report later (keep working meanwhile). isolation: \"worktree\" gives it its own git worktree and branch \
(for parallel edits). resume: an earlier background agent's id, to send it follow-up instructions in prompt."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": {"type": "string", "description": "3-5 word task summary"},
                "prompt": {"type": "string", "description": "Full instructions for the subagent"},
                "subagent_type": {"type": "string", "description": "Explore | general-purpose | oracle | a custom agent name"},
                "run_in_background": {"type": "boolean"},
                "isolation": {"type": "string", "enum": ["worktree"]},
                "resume": {"type": "string", "description": "Background agent id to continue"}
            },
            "required": ["description", "prompt"]
        })
    }
    fn kind(&self) -> ToolKind {
        // The subagent's own actions are permission-checked individually.
        ToolKind::Interactive
    }
    fn label(&self, args: &Value) -> String {
        format!(
            "Task({}: {})",
            args["subagent_type"].as_str().unwrap_or("general-purpose"),
            args["description"].as_str().unwrap_or("")
        )
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        if ctx.depth >= crate::resources::agent_depth(&ctx.env.config.resources) {
            return Ok(ToolOutput::err(
                "Subagents cannot launch further subagents here (resources.max_agent_depth).",
            ));
        }
        let prompt = arg_str(&args, "prompt")?;
        if let Some(id) = args["resume"].as_str().filter(|s| !s.is_empty()) {
            return Ok(crate::agent::resume_background(&ctx.env, id, prompt));
        }
        let kind = args["subagent_type"].as_str().unwrap_or("general-purpose");
        let opts = crate::agent::SubagentOpts {
            background: super::arg_bool(&args, "run_in_background"),
            worktree: args["isolation"].as_str() == Some("worktree"),
            description: args["description"].as_str().unwrap_or("").to_string(),
            ..Default::default()
        };
        crate::agent::run_subagent(ctx, kind, prompt, opts).await
    }
}
