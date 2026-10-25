//! In-session scheduling tools (`CronCreate` / `CronList` / `CronDelete` and
//! `ScheduleWakeup`), on top of [`crate::cron::Scheduler`].
//! Offered only where a frontend fires due tasks (the TUI and the plain REPL).

use super::{Ctx, Tool, ToolKind, ToolOutput, arg_bool, arg_str, arg_u64};
use crate::cron::{Source, at};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

fn unavailable(ctx: &Ctx) -> Option<ToolOutput> {
    if ctx.depth > 0 {
        return Some(ToolOutput::err(
            "Scheduling is only available to the main agent.",
        ));
    }
    if !ctx.env.scheduler.driven() {
        return Some(ToolOutput::err(
            "Scheduled tasks need an interactive session (the TUI or `--plain` REPL); this one can't fire them.",
        ));
    }
    None
}

pub struct CronCreate;

#[async_trait]
impl Tool for CronCreate {
    fn name(&self) -> String {
        "CronCreate".into()
    }
    fn description(&self) -> String {
        "Schedule a prompt to run later in this session: recurring on a 5-field cron expression (minute hour day-of-month month \
day-of-week, local time; *, n, a-b, a,b and */n), or once (recurring: false) at the next matching time, e.g. a reminder. \
Tasks fire only while the session is open and idle, and recurring ones end after 7 days. Returns the task's 8-character id."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cron": {"type": "string", "description": "e.g. \"*/10 * * * *\" (every 10 minutes), \"30 14 * * *\" (2:30pm)"},
                "prompt": {"type": "string", "description": "The prompt to run when it fires, as plain text for you (it can't start with !, / or #)"},
                "recurring": {"type": "boolean", "description": "false: fire once, then delete itself (default true)"}
            },
            "required": ["cron", "prompt"]
        })
    }
    /// A scheduled prompt starts a turn later with no one watching, so
    /// creating one is approved like an action with unknown effects: asked
    /// in default and accept-edits mode, judged by the classifier in auto.
    fn kind(&self) -> ToolKind {
        ToolKind::External
    }
    fn label(&self, args: &Value) -> String {
        format!(
            "CronCreate({} · {})",
            args["cron"].as_str().unwrap_or(""),
            crate::util::one_line(args["prompt"].as_str().unwrap_or(""), 60)
        )
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        if let Some(e) = unavailable(ctx) {
            return Ok(e);
        }
        let recurring =
            args.get("recurring").is_none_or(|v| v.is_null()) || arg_bool(&args, "recurring");
        Ok(
            match ctx.env.scheduler.create(
                arg_str(&args, "cron")?,
                arg_str(&args, "prompt")?,
                recurring,
            ) {
                Ok(t) => {
                    let kind = if recurring {
                        format!("recurring ({})", t.describe_schedule())
                    } else {
                        "one-shot".into()
                    };
                    ToolOutput::ok(format!(
                        "Scheduled task {} ({kind}); next run {}.",
                        t.id,
                        t.next.to_rfc3339()
                    ))
                    .display(format!(
                        "Scheduled {} · next run {}",
                        t.id,
                        at(t.next)
                    ))
                }
                Err(e) => ToolOutput::err(e),
            },
        )
    }
}

pub struct CronList;

#[async_trait]
impl Tool for CronList {
    fn name(&self) -> String {
        "CronList".into()
    }
    fn description(&self) -> String {
        "List this session's scheduled tasks (CronCreate tasks and /loop) with their ids, schedules, next run times and prompts.".into()
    }
    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }
    fn label(&self, _args: &Value) -> String {
        "CronList".into()
    }
    async fn run(&self, _args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        if let Some(e) = unavailable(ctx) {
            return Ok(e);
        }
        let tasks = ctx.env.scheduler.tasks();
        if tasks.is_empty() {
            return Ok(ToolOutput::ok("No scheduled tasks.").display("No scheduled tasks"));
        }
        let rows: Vec<String> = tasks
            .iter()
            .map(|t| {
                let kind = match t.source {
                    Source::Cron => "",
                    Source::Loop => " [/loop]",
                    Source::Wakeup => " [self-paced /loop wake-up]",
                };
                format!(
                    "{} · {}{kind} · next run {} · {}",
                    t.id,
                    t.describe_schedule(),
                    at(t.next),
                    crate::util::one_line(
                        t.prompt.as_deref().unwrap_or("(default loop prompt)"),
                        120
                    )
                )
            })
            .collect();
        Ok(ToolOutput::ok(rows.join("\n")).display(rows.join("\n")))
    }
}

pub struct CronDelete;

#[async_trait]
impl Tool for CronDelete {
    fn name(&self) -> String {
        "CronDelete".into()
    }
    fn description(&self) -> String {
        "Cancel a scheduled task by its id (from CronCreate or CronList).".into()
    }
    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }
    fn label(&self, args: &Value) -> String {
        format!("CronDelete({})", args["id"].as_str().unwrap_or(""))
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        if let Some(e) = unavailable(ctx) {
            return Ok(e);
        }
        let id = arg_str(&args, "id")?.trim();
        Ok(match ctx.env.scheduler.delete(id) {
            Some(t) => ToolOutput::ok(format!("Cancelled scheduled task {}.", t.id)),
            None => ToolOutput::err(format!(
                "No scheduled task with id {id}. CronList shows them."
            )),
        })
    }
}

pub struct ScheduleWakeup;

#[async_trait]
impl Tool for ScheduleWakeup {
    fn name(&self) -> String {
        "ScheduleWakeup".into()
    }
    fn description(&self) -> String {
        "Only during a self-paced /loop: at the end of each iteration, schedule the next one delay_seconds from now (60 to 3600; \
short while something is about to change, longer when nothing is pending) with a short reason, or pass stop: true when the \
task is complete to end the loop."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "delay_seconds": {"type": "integer", "description": "60 to 3600"},
                "reason": {"type": "string", "description": "Why this delay, in a few words"},
                "stop": {"type": "boolean", "description": "End the loop instead"}
            }
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }
    fn label(&self, args: &Value) -> String {
        if arg_bool(args, "stop") {
            return "ScheduleWakeup(stop)".into();
        }
        format!(
            "ScheduleWakeup({}s)",
            arg_u64(args, "delay_seconds").unwrap_or(0)
        )
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        if let Some(e) = unavailable(ctx) {
            return Ok(e);
        }
        let reason = args["reason"].as_str().map(str::to_string);
        Ok(
            match ctx.env.scheduler.schedule_wakeup(
                arg_u64(&args, "delay_seconds"),
                reason,
                arg_bool(&args, "stop"),
            ) {
                Ok(msg) => ToolOutput::ok(msg.clone()).display(msg),
                Err(e) => ToolOutput::err(e),
            },
        )
    }
}
