//! Lsp: code intelligence from the project's language servers.

use super::{Ctx, Tool, ToolKind, ToolOutput, arg_str, arg_u64};
use crate::util::resolve;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct Lsp;

#[async_trait]
impl Tool for Lsp {
    fn name(&self) -> String {
        "Lsp".into()
    }
    fn description(&self) -> String {
        "Query the language server. Operations: diagnostics (errors/warnings in file_path), definition / references / hover (at file_path:line:character, 1-based), documentSymbol (outline of file_path), workspaceSymbol (search symbols by query).".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {"type": "string", "enum": ["diagnostics", "definition", "references", "hover", "documentSymbol", "workspaceSymbol"]},
                "file_path": {"type": "string"},
                "line": {"type": "integer", "description": "1-based"},
                "character": {"type": "integer", "description": "1-based column"},
                "query": {"type": "string", "description": "workspaceSymbol search text"}
            },
            "required": ["operation"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }
    fn rule_subjects(&self, args: &Value) -> Vec<String> {
        args["file_path"]
            .as_str()
            .map(|p| vec![p.to_string()])
            .unwrap_or_default()
    }
    fn label(&self, args: &Value) -> String {
        let op = args["operation"].as_str().unwrap_or("?");
        match (args["file_path"].as_str(), args["query"].as_str()) {
            (Some(f), _) if args["line"].is_number() => format!("Lsp({op} {f}:{})", args["line"]),
            (Some(f), _) => format!("Lsp({op} {f})"),
            (None, Some(q)) => format!("Lsp({op} {q})"),
            _ => format!("Lsp({op})"),
        }
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let op = arg_str(&args, "operation")?;
        let file = args["file_path"].as_str().map(|p| resolve(&ctx.cwd(), p));
        let line = arg_u64(&args, "line").unwrap_or(1);
        let col = arg_u64(&args, "character").unwrap_or(1);
        let q = args["query"].as_str().unwrap_or("");
        Ok(
            match crate::lsp::query(&ctx.env, op, file.as_deref(), line, col, q).await {
                Ok(s) => ToolOutput::ok(s),
                Err(e) => ToolOutput::err(format!("Error: {e:#}")),
            },
        )
    }
}
