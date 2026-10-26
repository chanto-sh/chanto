//! Tool trait, registry and shared execution context.

pub mod bash;
pub mod browser;
pub mod cloud;
pub mod computer;
pub mod fs;
pub mod lsp;
pub mod memory;
pub mod misc;
pub mod monitor;
pub mod patch;
pub mod repomap;
pub mod schedule;
pub mod search;
pub mod semantic;
pub mod task;
pub mod tool_search;
pub mod web;

use crate::agent::Env;
use crate::llm::ToolSpec;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// No side effects (read, search, list).
    ReadOnly,
    /// Modifies files.
    Edit,
    /// Runs shell commands.
    Execute,
    /// Reads from the network.
    Network,
    /// Drives a web browser.
    Browser,
    /// MCP / external tools with unknown effects.
    External,
    /// Talks to the user (questions, plan approval) — never gated.
    Interactive,
}

#[derive(Debug, Default, Clone)]
pub struct ToolOutput {
    /// What the model sees.
    pub content: String,
    /// Base64 images for vision models (screenshots, image reads).
    pub images: Vec<String>,
    pub is_error: bool,
    /// Short human-facing summary (may contain ANSI).
    pub display: Option<String>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        ToolOutput {
            content: content.into(),
            ..Default::default()
        }
    }
    pub fn err(content: impl Into<String>) -> Self {
        ToolOutput {
            content: content.into(),
            is_error: true,
            ..Default::default()
        }
    }
    pub fn display(mut self, d: impl Into<String>) -> Self {
        self.display = Some(d.into());
        self
    }
}

pub struct Ctx {
    pub env: Arc<Env>,
    pub cancel: CancellationToken,
    /// Nesting depth (0 = main agent, 1 = subagent).
    pub depth: usize,
    /// Id of the tool call being executed (for progress events).
    pub call_id: Option<String>,
    /// Working directory of an agent isolated in a git worktree.
    pub cwd_override: Option<std::path::PathBuf>,
    /// The calling agent's reads, so an unchanged file isn't sent twice
    /// (`None` outside an agent's turn).
    pub reads: Option<Arc<fs::ReadLog>>,
}

impl Ctx {
    pub fn cwd(&self) -> std::path::PathBuf {
        self.cwd_override.clone().unwrap_or_else(|| self.env.cwd())
    }
    pub fn progress(&self, text: String) {
        if let Some(id) = &self.call_id {
            self.env.emit(crate::agent::Event::ToolProgress {
                id: id.clone(),
                text,
            });
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> String;
    fn description(&self) -> String;
    fn schema(&self) -> Value;
    fn kind(&self) -> ToolKind;
    /// Strings matched against permission rules (commands, paths, domains).
    fn rule_subjects(&self, _args: &Value) -> Vec<String> {
        vec![]
    }
    /// Compact label shown in the transcript, e.g. `Read(src/main.rs)`.
    fn label(&self, args: &Value) -> String {
        format!(
            "{}({})",
            self.name(),
            crate::util::one_line(&args.to_string(), 60)
        )
    }
    /// Kind may depend on arguments (e.g. Browser screenshot vs click).
    fn kind_for(&self, _args: &Value) -> ToolKind {
        self.kind()
    }
    /// The tool says of itself that it only reads (an MCP `readOnlyHint` from
    /// a local server). Advisory: it doesn't make the tool [`ToolKind::ReadOnly`],
    /// since a read-only fetch or search can still carry data out. A user's
    /// allow rule for the tool covers it in plan mode, and the explicit
    /// read-only sandbox policy lets it run.
    fn read_only_hint(&self) -> bool {
        false
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput>;
}

pub struct Registry {
    pub tools: Vec<Arc<dyn Tool>>,
    /// Callable but not advertised to the model (saves prompt tokens),
    /// e.g. `apply_patch` for Codex-trained models that use it unprompted.
    pub hidden: Vec<Arc<dyn Tool>>,
}

impl Registry {
    pub fn builtin() -> Registry {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(bash::Bash),
            Arc::new(bash::BashOutput),
            Arc::new(bash::KillShell),
            Arc::new(monitor::Monitor),
            Arc::new(fs::Read),
            Arc::new(fs::Write),
            Arc::new(fs::Edit),
            Arc::new(fs::MultiEdit),
            Arc::new(fs::NotebookEdit),
            Arc::new(search::Glob),
            Arc::new(search::Grep),
            Arc::new(search::Ls),
            Arc::new(repomap::RepoMap),
            Arc::new(semantic::SemanticSearch),
            Arc::new(web::WebFetch),
            Arc::new(web::WebSearch),
            Arc::new(browser::Browser),
            Arc::new(memory::Memory),
            Arc::new(misc::TodoWrite),
            Arc::new(misc::AskUserQuestion),
            Arc::new(misc::ExitPlanMode),
            Arc::new(misc::Skill),
            Arc::new(task::Task),
        ];
        // The scheduling tools are advertised only where a frontend fires
        // scheduled tasks (see `crate::cron::available`).
        let hidden: Vec<Arc<dyn Tool>> = vec![
            Arc::new(patch::ApplyPatch),
            Arc::new(schedule::CronCreate),
            Arc::new(schedule::CronList),
            Arc::new(schedule::CronDelete),
            Arc::new(schedule::ScheduleWakeup),
        ];
        Registry { tools, hidden }
    }

    /// Advertise hidden tools too (except the scheduling tools, which only a
    /// frontend that fires scheduled tasks offers; see `crate::cron::available`).
    pub fn show_hidden(&mut self) {
        let (keep, show): (Vec<_>, Vec<_>) = std::mem::take(&mut self.hidden)
            .into_iter()
            .partition(|t| crate::cron::TOOLS.contains(&t.name().as_str()));
        self.hidden = keep;
        self.tools.extend(show);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .or_else(|| {
                self.tools
                    .iter()
                    .find(|t| t.name().eq_ignore_ascii_case(name))
            })
            .or_else(|| self.hidden.iter().find(|t| t.name() == name))
            .cloned()
    }

    pub fn specs(&self, filter: &dyn Fn(&str) -> bool) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .filter(|t| filter(&t.name()))
            .map(|t| ToolSpec {
                name: t.name(),
                description: t.description(),
                parameters: t.schema(),
            })
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools
            .iter()
            .chain(&self.hidden)
            .map(|t| t.name())
            .collect()
    }
}

/// Fetch a string argument or fail with a model-friendly message.
pub fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required string parameter `{key}`"))
}

pub fn arg_bool(args: &Value, key: &str) -> bool {
    match args.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => false,
    }
}

pub fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    match args.get(key) {
        Some(Value::Number(n)) => n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Advertised tool definitions ride along with every request, so their size
    /// is a standing cost on every turn. Run with `--nocapture` for the
    /// per-tool table; the budget keeps a new tool from quietly adding a couple
    /// of hundred tokens to every call.
    #[test]
    fn advertised_tools_stay_within_a_prompt_budget() {
        let reg = Registry::builtin();
        let mut rows: Vec<(usize, String)> = reg
            .tools
            .iter()
            .map(|t| {
                let spec = crate::llm::ToolSpec {
                    name: t.name(),
                    description: t.description(),
                    parameters: t.schema(),
                };
                let bytes = serde_json::to_string(&spec).map(|s| s.len()).unwrap_or(0);
                (bytes, t.name())
            })
            .collect();
        rows.sort_by_key(|a| std::cmp::Reverse(a.0));
        let total: usize = rows.iter().map(|(bytes, _)| bytes).sum();
        for (bytes, name) in &rows {
            println!("{bytes:6}  {name}");
        }
        println!(
            "{total:6}  total ({} tools, ~{} tokens)",
            rows.len(),
            total.div_ceil(4)
        );
        assert!(
            total <= 22_000,
            "advertised tools take {total} bytes (~{} tokens)",
            total.div_ceil(4)
        );
    }
}
