//! TodoWrite, AskUserQuestion, ExitPlanMode, Skill.

use super::{Ctx, Tool, ToolKind, ToolOutput, arg_str};
use crate::agent::Event;
use crate::permissions::Mode;
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Todo {
    pub content: String,
    pub status: String,
    #[serde(default, rename = "activeForm")]
    pub active_form: String,
}

pub struct TodoWrite;

#[async_trait]
impl Tool for TodoWrite {
    fn name(&self) -> String {
        "TodoWrite".into()
    }
    fn description(&self) -> String {
        "Track a multi-step job as a structured task list so progress is visible. Worth it once a task needs \
three or more steps; not worth it for a one-step request. Each call replaces the list wholesale, and only one \
item should be `in_progress` at a time — mark it `completed` as soon as it's done."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {"type": "array", "items": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string", "description": "Imperative description, e.g. 'Run tests'"},
                        "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]},
                        "activeForm": {"type": "string", "description": "Present continuous form, e.g. 'Running tests'"}
                    },
                    "required": ["content", "status"]
                }}
            },
            "required": ["todos"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Interactive
    }
    fn label(&self, _args: &Value) -> String {
        "TodoWrite".into()
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let todos: Vec<Todo> = serde_json::from_value(args["todos"].clone())
            .map_err(|e| anyhow::anyhow!("invalid todos: {e}"))?;
        // TaskCreated / TaskCompleted hooks can refuse new and finished items.
        let old = ctx.env.todos.lock().unwrap().clone();
        let (todos, refused) = crate::hooks::todo_changes(&ctx.env, &old, todos).await;
        *ctx.env.todos.lock().unwrap() = todos.clone();
        ctx.env.emit(Event::Todos(todos.clone()));
        let done = todos.iter().filter(|t| t.status == "completed").count();
        let mut display = String::new();
        for t in &todos {
            let mark = match t.status.as_str() {
                "completed" => "☒",
                "in_progress" => "◐",
                _ => "☐",
            };
            display.push_str(&format!("{mark} {}\n", t.content));
        }
        if !refused.is_empty() {
            return Ok(ToolOutput::err(format!(
                "Error: {}\nThe rest of the list was saved ({done}/{} completed).",
                refused.join("\n"),
                todos.len()
            ))
            .display(display.trim_end().to_string()));
        }
        Ok(ToolOutput::ok(format!(
            "Todos updated ({done}/{} completed). Keep working through them; update the list as you go.",
            todos.len()
        ))
        .display(display.trim_end().to_string()))
    }
}

pub struct AskUserQuestion;

#[async_trait]
impl Tool for AskUserQuestion {
    fn name(&self) -> String {
        "AskUserQuestion".into()
    }
    fn description(&self) -> String {
        "Ask the user a multiple-choice question when you are blocked on a decision only they can make \
(ambiguous requirements, choosing between approaches). The user may also answer in free text. \
Do not use it for things you can find out yourself."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {"type": "string"},
                "options": {"type": "array", "items": {"type": "string"}, "description": "2-4 options"}
            },
            "required": ["question", "options"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Interactive
    }
    fn label(&self, args: &Value) -> String {
        format!(
            "AskUserQuestion({})",
            crate::util::one_line(args["question"].as_str().unwrap_or(""), 60)
        )
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let q = arg_str(&args, "question")?;
        let opts: Vec<String> = args["options"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if !ctx.env.interactive {
            return Ok(ToolOutput::err(
                "No user is available to answer (non-interactive mode). Make a reasonable assumption, state it, and continue.",
            ));
        }
        match ctx.env.frontend.question(q, &opts).await {
            Some(a) => Ok(ToolOutput::ok(format!("User answered: {a}")).display(format!("→ {a}"))),
            None => Ok(ToolOutput::err(
                "The user dismissed the question without answering.",
            )),
        }
    }
}

pub struct ExitPlanMode;

#[async_trait]
impl Tool for ExitPlanMode {
    fn name(&self) -> String {
        "ExitPlanMode".into()
    }
    fn description(&self) -> String {
        "Use in plan mode when you have finished planning an implementation: present the plan (markdown) for \
the user's approval. If approved, plan mode ends and you can start making changes."
            .into()
    }
    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {"plan": {"type": "string"}}, "required": ["plan"]})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Interactive
    }
    fn label(&self, _args: &Value) -> String {
        "ExitPlanMode".into()
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let plan = arg_str(&args, "plan")?;
        if ctx.env.config.sandbox.read_only {
            return Ok(ToolOutput::err(
                "The explicit read-only sandbox policy is active. The plan cannot enable implementation; restart with a writable sandbox policy to make changes.",
            ));
        }
        if ctx.env.mode() != Mode::Plan {
            return Ok(ToolOutput::ok("Not in plan mode; proceed with the task."));
        }
        if !ctx.env.interactive {
            ctx.env.set_mode(Mode::AcceptEdits);
            return Ok(ToolOutput::ok(
                "Plan accepted (non-interactive). Proceed with implementation.",
            ));
        }
        let opts = vec![
            "Proceed, accepting edits".to_string(),
            "Proceed in auto mode".to_string(),
            "Proceed, I'll approve each edit".to_string(),
            "Keep planning".to_string(),
        ];
        let ans = ctx
            .env
            .frontend
            .question(
                &format!("The plan is ready. How should I proceed?\n\n{plan}"),
                &opts,
            )
            .await;
        let mode = match ans.as_deref() {
            Some(a) if a == opts[0] => Some(Mode::AcceptEdits),
            Some(a) if a == opts[1] => Some(Mode::Auto),
            Some(a) if a == opts[2] => Some(Mode::Default),
            _ => None,
        };
        match (mode, ans) {
            (Some(m), _) => {
                ctx.env.set_mode(m);
                Ok(ToolOutput::ok(format!("User approved the plan. Mode is now {}. Start implementing it, tracking steps with TodoWrite.", m.label()))
                    .display("Plan approved"))
            }
            (None, Some(feedback)) if feedback != opts[3] => Ok(ToolOutput::err(format!(
                "User did not approve the plan yet. Their feedback: {feedback}\nRevise the plan and call ExitPlanMode again."
            ))),
            _ => Ok(ToolOutput::err(
                "User wants to keep planning. Ask what to change or refine the plan.",
            )),
        }
    }
}

pub struct Skill;

#[async_trait]
impl Tool for Skill {
    fn name(&self) -> String {
        "Skill".into()
    }
    fn description(&self) -> String {
        "Load a skill (packaged instructions for a specific kind of task). Available skills are listed in the system prompt. \
Call this when the task matches a skill's description, then follow its instructions."
            .into()
    }
    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {"skill": {"type": "string"}, "args": {"type": "string"}}, "required": ["skill"]})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }
    fn label(&self, args: &Value) -> String {
        format!("Skill({})", args["skill"].as_str().unwrap_or(""))
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let name = arg_str(&args, "skill")?;
        // `disable-model-invocation` skills are for the user's `/name` only.
        let skills: Vec<_> = ctx
            .env
            .skills
            .read()
            .unwrap()
            .iter()
            .filter(|s| s.model_invocable)
            .cloned()
            .collect();
        let Some(sk) = crate::util::find_named(&skills, name, |s| &s.name) else {
            let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
            return Ok(ToolOutput::err(format!(
                "Unknown skill `{name}`. Available: {}",
                names.join(", ")
            )));
        };
        let body = crate::commands::render_skill(&ctx.env, sk, args["args"].as_str().unwrap_or(""))
            .await?;
        // `context: fork` runs it in a subagent (inline when already in one).
        let fork = sk.fork.as_deref().filter(|_| ctx.depth == 0);
        // Its hooks, `allowed-tools`, `model` and `effort` apply from here to the end of the turn.
        crate::commands::activate_skill(
            &ctx.env,
            &sk.name,
            &sk.meta,
            sk.project,
            false,
            fork.is_some(),
        );
        if let Some(kind) = fork {
            return crate::commands::run_forked_skill(
                ctx,
                sk,
                kind,
                &body,
                format!("skill {}", sk.name),
            )
            .await;
        }
        Ok(
            ToolOutput::ok(format!("Skill `{}` loaded.\n\n{body}", sk.name))
                .display(format!("Loaded skill {}", sk.name)),
        )
    }
}
