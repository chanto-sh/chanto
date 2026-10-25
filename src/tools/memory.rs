use super::{Ctx, Tool, ToolKind, ToolOutput, arg_str};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
pub struct Memory;
#[async_trait]
impl Tool for Memory {
    fn name(&self) -> String {
        "Memory".into()
    }
    fn description(&self) -> String {
        "Save durable user preferences, corrections, and non-obvious project facts across sessions. Read relevant memories from the index on demand. Update/delete facts that prove wrong. Never save secrets, transient task state, or facts readily derivable from repository files. Every mutation is visible and undoable with /memory undo <scope>.".into()
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"action":{"type":"string","enum":["save","update","delete","read"]},"scope":{"type":"string","enum":["user","project"]},"name":{"type":"string"},"description":{"type":"string"},"type":{"type":"string","enum":["user","feedback","project","reference"]},"body":{"type":"string"}},"required":["action","name"]})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }
    fn kind_for(&self, args: &Value) -> ToolKind {
        if args["action"] == "read" {
            ToolKind::ReadOnly
        } else {
            ToolKind::Edit
        }
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let action = arg_str(&args, "action")?;
        let name = arg_str(&args, "name")?;
        let scope = args["scope"]
            .as_str()
            .unwrap_or(&ctx.env.config.memory.scope);
        let store = crate::memory::Store::new(&ctx.cwd());
        if action == "read" {
            return Ok(ToolOutput::ok(store.read(scope, name)?.body));
        }
        if ctx.env.config.sandbox.read_only {
            return Ok(ToolOutput::err(
                "Memory writes are disabled by read-only policy",
            ));
        }
        if !ctx.env.config.memory.auto {
            return Ok(ToolOutput::err(
                "Automatic memory writes are disabled; the user can use /memory",
            ));
        }
        match action {
            "save" | "update" => {
                if action == "update" {
                    store.read(scope, name)?;
                }
                store.save(
                    scope,
                    name,
                    arg_str(&args, "description")?,
                    arg_str(&args, "type")?,
                    arg_str(&args, "body")?,
                )?;
            }
            "delete" => store.delete(scope, name)?,
            _ => anyhow::bail!("invalid Memory action"),
        }
        let verb = if action == "delete" {
            "Deleted"
        } else {
            "Saved"
        };
        // Say what was stored and where, so a save is never silent about its content.
        let detail = if action == "delete" {
            String::new()
        } else {
            format!(
                " — {}",
                crate::util::one_line(args["description"].as_str().unwrap_or(""), 120)
            )
        };
        Ok(ToolOutput::ok(format!(
            "{verb} memory: {name}; undo with /memory undo {scope}"
        ))
        .display(format!(
            "\x1b[2m{verb} {scope} memory: {name}{detail}\x1b[0m"
        )))
    }
}
