//! Tool search (`ToolSearch`): MCP tool schemas stay out of the
//! prompt, the model sees their names in a `<system-reminder>`, and loads the
//! schemas it needs with this tool. Loaded tools join the end of the tool list
//! (earlier definitions keep their order, so the prompt prefix stays cached).
//!
//! Nothing here keeps state: what was announced and what was loaded are read
//! back from the conversation, so resume, rewind and compaction stay consistent.

use super::{Ctx, Tool, ToolKind, ToolOutput};
use crate::llm::{Message, Role, ToolSpec};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const NAME: &str = "ToolSearch";

/// The first line of a deferred-tools announcement.
const ANNOUNCE: &str = "The following deferred tools are now available via ToolSearch.";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Setting {
    Off,
    /// Always defer.
    On,
    /// Defer once the deferrable schemas would take this percentage of the context.
    Auto(f64),
}

impl Setting {
    pub fn parse(v: &str) -> Option<Setting> {
        let v = v.trim().to_ascii_lowercase();
        match v.as_str() {
            "" => None,
            "true" | "1" | "on" | "yes" | "always" => Some(Setting::On),
            "false" | "0" | "off" | "no" | "never" => Some(Setting::Off),
            "auto" => Some(Setting::Auto(10.0)),
            _ => v
                .strip_prefix("auto:")
                .and_then(|n| n.trim().trim_end_matches('%').parse::<f64>().ok())
                .filter(|n| (0.0..=100.0).contains(n))
                .map(Setting::Auto),
        }
    }

    /// `ENABLE_TOOL_SEARCH` (process environment, then the config's `env`), then `tool_search`; default auto (10%).
    pub fn from_config(cfg: &crate::config::Config) -> Setting {
        std::env::var("ENABLE_TOOL_SEARCH")
            .ok()
            .or_else(|| cfg.env.get("ENABLE_TOOL_SEARCH").cloned())
            .and_then(|v| Setting::parse(&v))
            .or_else(|| cfg.tool_search.as_deref().and_then(Setting::parse))
            .unwrap_or(Setting::Auto(10.0))
    }

    /// Whether to defer `deferrable` given a context window of `context_tokens`.
    pub fn defers(self, deferrable: &[ToolSpec], context_tokens: u64) -> bool {
        if deferrable.is_empty() {
            return false;
        }
        match self {
            Setting::Off => false,
            Setting::On => true,
            Setting::Auto(pct) => {
                let chars = serde_json::to_string(deferrable)
                    .map(|s| s.len())
                    .unwrap_or(0) as f64;
                chars / 4.0 >= context_tokens as f64 * pct / 100.0
            }
        }
    }
}

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: NAME.into(),
        description: ToolSearch.description(),
        parameters: ToolSearch.schema(),
    }
}

/// One line per tool: its name and the first line of its description.
fn hint(t: &ToolSpec) -> String {
    let first = t
        .description
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if first.is_empty() {
        t.name.clone()
    } else {
        format!("{} — {}", t.name, crate::util::truncate(first, 100))
    }
}

/// The reminder announcing deferred tools the model hasn't been told about.
pub fn announcement(new: &[&ToolSpec]) -> String {
    let mut s = format!(
        "<system-reminder>\n{ANNOUNCE} Their schemas are not loaded, so calling them directly fails until you load them: \
         call ToolSearch with query \"select:<name>[,<name>...]\" for exact names, or with keywords to search.\n"
    );
    for t in new {
        s.push_str(&hint(t));
        s.push('\n');
    }
    s.push_str("</system-reminder>");
    s
}

/// Tool names already announced in the conversation.
pub fn announced(messages: &[Message]) -> HashSet<String> {
    let mut out = HashSet::new();
    for m in messages.iter().filter(|m| m.content.contains(ANNOUNCE)) {
        for block in m.content.split(ANNOUNCE).skip(1) {
            let body = block.split("</system-reminder>").next().unwrap_or("");
            for line in body.lines().skip(1) {
                let name = line.split(" — ").next().unwrap_or("").trim();
                if !name.is_empty() {
                    out.insert(name.to_string());
                }
            }
        }
    }
    out
}

/// Deferred tools in use, in the order they were first loaded (by ToolSearch,
/// or called directly), restricted to `deferred`.
pub fn loaded(messages: &[Message], deferred: &[ToolSpec]) -> Vec<String> {
    let known: HashSet<&str> = deferred.iter().map(|t| t.name.as_str()).collect();
    let mut out: Vec<String> = Vec::new();
    let mut add = |n: &str| {
        if known.contains(n) && !out.iter().any(|o| o == n) {
            out.push(n.to_string());
        }
    };
    for m in messages {
        match m.role {
            Role::Tool if m.tool_name.as_deref() == Some(NAME) => {
                for line in m.content.lines().filter(|l| l.starts_with('{')) {
                    if let Some(n) = serde_json::from_str::<Value>(line)
                        .ok()
                        .and_then(|v| v["name"].as_str().map(str::to_string))
                    {
                        add(&n);
                    }
                }
            }
            Role::Assistant => {
                for c in &m.tool_calls {
                    add(&c.name);
                }
            }
            _ => {}
        }
    }
    out
}

/// Split a name into lowercase words (`mcp__github__create_issue` → mcp, github, create, issue).
fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        let boundary =
            !c.is_alphanumeric() || (c.is_uppercase() && i > 0 && chars[i - 1].is_lowercase());
        if boundary && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if c.is_alphanumeric() {
            cur.extend(c.to_lowercase());
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Tools matching `query`: `select:A,B` picks exact names; otherwise keywords
/// are scored against names (heavier) and descriptions, and `+word` requires
/// the word in the name.
pub fn search<'a>(
    query: &str,
    max: usize,
    tools: &'a [ToolSpec],
) -> (Vec<&'a ToolSpec>, Vec<String>) {
    let q = query.trim();
    if let Some(list) = q.strip_prefix("select:") {
        let mut found = Vec::new();
        let mut missing = Vec::new();
        for name in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            match tools
                .iter()
                .find(|t| t.name == name)
                .or_else(|| tools.iter().find(|t| t.name.eq_ignore_ascii_case(name)))
            {
                Some(t) if !found.iter().any(|f: &&ToolSpec| f.name == t.name) => found.push(t),
                Some(_) => {}
                None => missing.push(name.to_string()),
            }
        }
        return (found, missing);
    }
    let mut required = Vec::new();
    let mut terms = Vec::new();
    for w in q.split_whitespace() {
        match w.strip_prefix('+') {
            Some(r) if !r.is_empty() => required.push(r.to_lowercase()),
            _ => terms.extend(words(w)),
        }
    }
    let mut scored: Vec<(u32, usize, &ToolSpec)> = tools
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let lname = t.name.to_lowercase();
            if !required.iter().all(|r| lname.contains(r.as_str())) {
                return None;
            }
            let name_words = words(&t.name);
            let desc = t.description.to_lowercase();
            let desc = crate::util::truncate(&desc, 4096);
            let desc_words = words(&desc);
            let mut score = 0;
            for term in &terms {
                if name_words.iter().any(|w| w == term) {
                    score += 10;
                } else if lname.contains(term.as_str()) {
                    score += 5;
                } else if desc_words.iter().any(|w| w == term) {
                    score += 2;
                } else if desc.contains(term.as_str()) {
                    score += 1;
                }
            }
            if terms.is_empty() {
                score = 1;
            }
            (score > 0).then_some((score, i, t))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    (
        scored
            .into_iter()
            .take(max.max(1))
            .map(|(_, _, t)| t)
            .collect(),
        vec![],
    )
}

/// Built-in tools `optimize.defer_tools` keeps out of the tool list until
/// loaded: big schemas that went unused across ~1,850 benchmark sessions.
pub const DEFERRED_BUILTINS: &[&str] = &[
    "Browser",
    "Monitor",
    "NotebookEdit",
    "SemanticSearch",
    "RepoMap",
];

/// Which of [`DEFERRED_BUILTINS`] to send upfront anyway, decided once when a
/// session starts (so its tool list doesn't change): the ones the request or
/// project points at, and any already used in this project (`used`).
pub fn upfront_builtins(root: &Path, request: &str, used: &[String]) -> Vec<String> {
    let r = request.to_lowercase();
    let says = |words: &[&str]| words.iter().any(|w| r.contains(w));
    let (files, notebooks) = count_files(root);
    let mut out: Vec<String> = Vec::new();
    let mut keep = |name: &str, why: bool| {
        if why || used.iter().any(|u| u == name) {
            out.push(name.to_string());
        }
    };
    keep(
        "Browser",
        says(&[
            "http://",
            "https://",
            "localhost",
            "browser",
            "screenshot",
            "website",
            "web page",
            "webpage",
        ]),
    );
    keep(
        "Monitor",
        says(&[
            "monitor",
            "watch ",
            "dev server",
            "long-running",
            "long running",
        ]),
    );
    keep("NotebookEdit", notebooks || says(&[".ipynb", "notebook"]));
    // Big repositories are where a map or a semantic index beats grep.
    keep("RepoMap", files > LARGE_REPO);
    keep("SemanticSearch", files > LARGE_REPO);
    out
}

/// Files in a project above which RepoMap and SemanticSearch are sent upfront.
const LARGE_REPO: usize = 1000;

/// Files under `root` (respecting .gitignore, stopping once both answers are
/// known) and whether any is a notebook.
fn count_files(root: &Path) -> (usize, bool) {
    let (mut files, mut notebooks) = (0, false);
    let walker = ignore::WalkBuilder::new(root)
        .max_depth(Some(6))
        .hidden(true)
        .git_ignore(true)
        .build();
    for e in walker.flatten() {
        if e.file_type().is_some_and(|t| t.is_file()) {
            files += 1;
            notebooks |= e.path().extension().is_some_and(|x| x == "ipynb");
            if files > LARGE_REPO && (notebooks || files > 20 * LARGE_REPO) {
                break;
            }
        }
    }
    (files, notebooks)
}

/// The per-project record of deferred built-ins that sessions used.
fn used_file(cwd: &Path) -> PathBuf {
    crate::session::project_dir(cwd).join("deferred-tools-used")
}

/// Deferred built-ins used in this project before (see [`record_used`]).
pub fn used_builtins(cwd: &Path) -> Vec<String> {
    std::fs::read_to_string(used_file(cwd))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| DEFERRED_BUILTINS.contains(l))
        .map(str::to_string)
        .collect()
}

/// Remember that this project used `name`, so later sessions send it upfront.
pub fn record_used(cwd: &Path, name: &str) {
    let path = used_file(cwd);
    let mut names = used_builtins(cwd);
    if names.iter().any(|n| n == name) {
        return;
    }
    names.push(name.to_string());
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, names.join("\n") + "\n");
}

/// The tools tool search would defer right now: MCP tools (all but
/// always-load ones) and, with `optimize.defer_tools`, [`DEFERRED_BUILTINS`].
pub fn deferrable(env: &crate::agent::Env) -> Vec<ToolSpec> {
    let mut out: Vec<ToolSpec> = env
        .mcp
        .iter()
        .flat_map(|c| c.spec_entries())
        .filter(|(_, upfront)| !upfront)
        .map(|(s, _)| s)
        .collect();
    if env.config.optimize.defer_tools() {
        out.extend(env.tools.specs(&|n| DEFERRED_BUILTINS.contains(&n)));
    }
    out
}

pub struct ToolSearch;

#[async_trait]
impl Tool for ToolSearch {
    fn name(&self) -> String {
        NAME.into()
    }
    fn description(&self) -> String {
        "Load the full definitions of deferred tools (listed by name in a system reminder) so you can call them. \
         Query forms: \"select:A,B\" loads those exact tools; keywords (\"github issue\") search names and descriptions and load the best matches; \
         \"+slack send\" requires \"slack\" in the name. Loaded tools stay callable for the rest of the session; load several at once when you know you need them."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "\"select:<name>[,<name>...]\" or search keywords"},
                "max_results": {"type": "number", "description": "Most tools to load for a keyword search (default 5)"}
            },
            "required": ["query"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }
    fn label(&self, args: &Value) -> String {
        format!(
            "ToolSearch({})",
            crate::util::one_line(args["query"].as_str().unwrap_or(""), 60)
        )
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let query = super::arg_str(&args, "query")?;
        let max = super::arg_u64(&args, "max_results")
            .unwrap_or(5)
            .clamp(1, 50) as usize;
        let deferred = deferrable(&ctx.env);
        let (found, missing) = search(query, max, &deferred);
        let mut out = String::new();
        if !found.is_empty() {
            out.push_str(&format!(
                "Loaded {} tool(s); call them directly from now on. Definitions:\n",
                found.len()
            ));
            for t in &found {
                out.push_str(&json!({"name": t.name, "description": t.description, "parameters": t.parameters}).to_string());
                out.push('\n');
            }
        }
        for name in &missing {
            let already = ctx.env.tools.get(name).is_some()
                || ctx
                    .env
                    .mcp
                    .iter()
                    .any(|c| c.tool_names().iter().any(|n| n == name));
            out.push_str(&if already {
                format!("{name} is not deferred: it is already in your tool list.\n")
            } else {
                format!("No tool named {name}.\n")
            });
        }
        if found.is_empty() {
            if missing.is_empty() {
                out.push_str(&format!("No deferred tools match \"{query}\". Try other keywords, or select:<exact name> from the deferred tools listed earlier.\n"));
            }
            let down: Vec<String> = ctx
                .env
                .mcp
                .iter()
                .filter(|c| !c.is_connected())
                .map(|c| format!("{} ({:?})", c.name, c.status()))
                .collect();
            if !down.is_empty() {
                out.push_str(&format!(
                    "MCP servers that aren't connected (their tools can't be loaded): {}\n",
                    down.join(", ")
                ));
            }
        }
        let display = if found.is_empty() {
            "no match".to_string()
        } else {
            found
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        Ok(ToolOutput {
            content: out.trim_end().to_string(),
            is_error: found.is_empty(),
            ..Default::default()
        }
        .display(display))
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn upfront_builtins_follow_the_request_and_the_project() {
        let dir =
            std::env::temp_dir().join(format!("chanto-upfront-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.py"), "").unwrap();
        assert!(upfront_builtins(&dir, "fix the parser", &[]).is_empty());
        assert_eq!(
            upfront_builtins(&dir, "Take a screenshot of http://localhost:3000", &[]),
            ["Browser"]
        );
        assert_eq!(
            upfront_builtins(&dir, "start the dev server and watch it", &[]),
            ["Monitor"]
        );
        assert_eq!(
            upfront_builtins(&dir, "fix the parser", &["RepoMap".into()]),
            ["RepoMap"]
        );
        std::fs::write(dir.join("eda.ipynb"), "{}").unwrap();
        assert_eq!(
            upfront_builtins(&dir, "fix the parser", &[]),
            ["NotebookEdit"]
        );
        for i in 0..=LARGE_REPO {
            std::fs::write(dir.join(format!("f{i}.txt")), "").unwrap();
        }
        assert_eq!(
            upfront_builtins(&dir, "fix the parser", &[]),
            ["NotebookEdit", "RepoMap", "SemanticSearch"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    fn spec(name: &str, desc: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: desc.into(),
            parameters: json!({"type": "object"}),
        }
    }

    #[test]
    fn settings_parse() {
        assert_eq!(Setting::parse("auto"), Some(Setting::Auto(10.0)));
        assert_eq!(Setting::parse("auto:5"), Some(Setting::Auto(5.0)));
        assert_eq!(Setting::parse("TRUE"), Some(Setting::On));
        assert_eq!(Setting::parse("false"), Some(Setting::Off));
        assert_eq!(Setting::parse("auto:200"), None);
        let big = vec![spec("a", &"x".repeat(4000))];
        assert!(Setting::Auto(10.0).defers(&big, 8192));
        assert!(!Setting::Auto(10.0).defers(&big, 128_000));
        assert!(!Setting::On.defers(&[], 1));
    }

    #[test]
    fn select_and_keyword_search() {
        let tools = vec![
            spec("mcp__github__create_issue", "Create a GitHub issue"),
            spec("mcp__github__list_prs", "List pull requests"),
            spec("mcp__slack__send_message", "Send a Slack message"),
            spec(
                "mcp__mail__send",
                "Send an email; also posts to slack channels",
            ),
        ];
        let (f, m) = search("select:mcp__github__list_prs, nope", 5, &tools);
        assert_eq!(
            f.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["mcp__github__list_prs"]
        );
        assert_eq!(m, ["nope"]);
        let (f, _) = search("slack send", 5, &tools);
        assert_eq!(f[0].name, "mcp__slack__send_message");
        let (f, _) = search("+slack send", 5, &tools);
        assert_eq!(f.len(), 1);
        let (f, _) = search("github", 1, &tools);
        assert_eq!(f.len(), 1);
        let (f, _) = search("pull requests", 5, &tools);
        assert_eq!(f[0].name, "mcp__github__list_prs");
    }

    #[test]
    fn announcements_and_loads_are_read_back() {
        let tools = vec![spec("mcp__a__x", "Does x\nmore"), spec("mcp__a__y", "")];
        let note = announcement(&tools.iter().collect::<Vec<_>>());
        assert!(note.contains("mcp__a__x — Does x\n"));
        let mut msgs = vec![Message::user(format!("hi\n\n{note}"))];
        assert_eq!(
            announced(&msgs),
            HashSet::from(["mcp__a__x".to_string(), "mcp__a__y".to_string()])
        );
        let call = crate::llm::ToolCall {
            id: "1".into(),
            name: NAME.into(),
            arguments: json!({}),
        };
        msgs.push(Message::tool(
            &call,
            "Loaded 1 tool(s)\n{\"name\":\"mcp__a__y\",\"description\":\"\"}",
        ));
        let mut a = Message::assistant("");
        a.tool_calls = vec![crate::llm::ToolCall {
            id: "2".into(),
            name: "mcp__a__x".into(),
            arguments: json!({}),
        }];
        msgs.push(a);
        assert_eq!(loaded(&msgs, &tools), ["mcp__a__y", "mcp__a__x"]);
    }
}
