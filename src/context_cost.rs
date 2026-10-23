//! `/context skills` (alias `/skill-doctor`): what each skill, subagent
//! definition and MCP server adds to the prompt, so it's clear what bloats
//! the context. Two costs each, in estimated tokens (4 characters a token):
//! the *listing*, sent with every request, and the *full* cost once it is
//! used (a skill's body when loaded, a subagent's prompt in its own context,
//! an MCP server's every tool definition).

use crate::agent::Env;
use crate::context::{AgentDef, Skill};
use crate::llm::ToolSpec;

/// One row of the report.
#[derive(Debug, Clone, PartialEq)]
pub struct Cost {
    pub kind: &'static str,
    pub name: String,
    /// Tokens on every request.
    pub listing: u64,
    /// Tokens once used.
    pub full: u64,
    pub note: String,
}

fn tokens(chars: usize) -> u64 {
    chars.div_ceil(4) as u64
}

/// A skill: its line in the system prompt's skill list (none when only
/// `/name` can run it), and its body when the Skill tool loads it.
pub fn skill(sk: &Skill) -> Cost {
    let body = std::fs::read_to_string(&sk.path)
        .map(|t| crate::context::strip_frontmatter(&t).len())
        .unwrap_or(0);
    let listing = if sk.model_invocable {
        tokens(format!("- {}: {}\n", sk.name, sk.description).len())
    } else {
        0
    };
    let note = if sk.model_invocable {
        String::new()
    } else {
        "not listed to the model (only `/name` runs it)".into()
    };
    Cost {
        kind: "skill",
        name: sk.name.clone(),
        listing,
        full: listing + tokens(body),
        note,
    }
}

/// A subagent definition: its line in the system prompt, and its prompt,
/// which goes into the subagent's own context when it runs (not this one).
pub fn agent(a: &AgentDef) -> Cost {
    let listing = tokens(format!("- {}: {}\n", a.name, a.description).len());
    Cost {
        kind: "agent",
        name: a.name.clone(),
        listing,
        full: listing + tokens(a.prompt.len()),
        note: "full cost is in the subagent's own context".into(),
    }
}

/// An MCP server: its instructions (or one-line note) and tool definitions.
/// With `deferred`, tool search holds back the tools that aren't marked
/// always-load: only a one-line hint each is sent until one is loaded.
pub fn mcp(
    name: &str,
    instructions: Option<&str>,
    tools: &[(ToolSpec, bool)],
    deferred: bool,
) -> Cost {
    let note_text = match instructions {
        Some(i) => format!("## {name}\n{}", crate::util::truncate(i, 2000)),
        None => format!("## {name} ({} tools)", tools.len()),
    };
    let def = |t: &ToolSpec| tokens(serde_json::to_string(t).map(|s| s.len()).unwrap_or(0));
    let hint = |t: &ToolSpec| {
        tokens(t.name.len() + t.description.lines().next().unwrap_or("").len().min(100) + 4)
    };
    let all: u64 = tools.iter().map(|(t, _)| def(t)).sum();
    let upfront: u64 = tools
        .iter()
        .map(|(t, always)| if deferred && !always { hint(t) } else { def(t) })
        .sum();
    let held = tools
        .iter()
        .filter(|(_, always)| deferred && !always)
        .count();
    let note = if held > 0 {
        format!("{} tools, {held} deferred by tool search", tools.len())
    } else {
        format!("{} tools", tools.len())
    };
    Cost {
        kind: "MCP server",
        name: name.to_string(),
        listing: tokens(note_text.len()) + upfront,
        full: tokens(note_text.len()) + all,
        note,
    }
}

/// Every row for this session, the costliest listing first.
pub fn costs(env: &Env) -> Vec<Cost> {
    let mut rows: Vec<Cost> = env.skills.read().unwrap().iter().map(skill).collect();
    rows.extend(env.agents.read().unwrap().iter().map(agent));
    let servers: Vec<_> = env
        .mcp
        .iter()
        .filter(|c| c.is_connected())
        .map(|c| (c.name.clone(), c.instructions(), c.spec_entries()))
        .collect();
    let deferrable: Vec<ToolSpec> = servers
        .iter()
        .flat_map(|(_, _, t)| t.iter().filter(|(_, up)| !up).map(|(t, _)| t.clone()))
        .collect();
    let deferred = crate::tools::tool_search::Setting::from_config(&env.config)
        .defers(&deferrable, env.context_max());
    rows.extend(
        servers
            .iter()
            .map(|(name, i, t)| mcp(name, i.as_deref(), t, deferred)),
    );
    rows.sort_by(|a, b| b.listing.cmp(&a.listing).then(b.full.cmp(&a.full)));
    rows
}

/// The report as markdown.
pub fn report(rows: &[Cost], context_max: u64) -> String {
    if rows.is_empty() {
        return "## Context: skills, agents and MCP servers\nNone loaded: no skills, subagent definitions or connected MCP servers add to the prompt.".into();
    }
    let listing: u64 = rows.iter().map(|r| r.listing).sum();
    let pct = |n: u64| {
        if context_max > 0 {
            format!(
                " ({:.1}% of {context_max})",
                n as f64 * 100.0 / context_max as f64
            )
        } else {
            String::new()
        }
    };
    let mut s = format!(
        "## Context: skills, agents and MCP servers\nListed on every request: **~{listing} tokens**{}. *Listing* is what each adds to every request; *full* is its cost once used (a skill loaded, a subagent run, every MCP tool loaded).\n\n| | name | listing | full | |\n|---|---|---:|---:|---|\n",
        pct(listing)
    );
    for r in rows {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            r.kind, r.name, r.listing, r.full, r.note
        ));
    }
    s.push_str("\nTo trim: `disable-model-invocation: true` keeps a skill out of the listing (`/name` still runs it), `/mcp disable <server>` drops a server, and `tool_search = \"true\"` defers MCP tool definitions until they're needed.");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, desc: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: desc.into(),
            parameters: serde_json::json!({"type": "object", "properties": {"q": {"type": "string", "description": "x".repeat(400)}}}),
        }
    }

    #[test]
    fn skills_cost_a_line_until_loaded() {
        let dir = std::env::temp_dir().join(format!("chanto-skillcost-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        std::fs::write(
            &path,
            format!(
                "---\nname: big\ndescription: d\n---\n{}",
                "word ".repeat(800)
            ),
        )
        .unwrap();
        let mut sk = Skill {
            name: "big".into(),
            description: "Does big things".into(),
            path,
            model_invocable: true,
            user_invocable: true,
            fork: None,
            project: false,
            meta: Default::default(),
        };
        let c = skill(&sk);
        assert_eq!(c.listing, tokens("- big: Does big things\n".len()));
        assert!(c.full >= c.listing + 1000, "{c:?}");
        sk.model_invocable = false;
        let c = skill(&sk);
        assert_eq!(c.listing, 0);
        assert!(c.full >= 1000 && c.note.contains("not listed"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mcp_tools_cost_less_when_deferred() {
        let tools = vec![
            (spec("mcp__s__a", "Search things\nmore"), false),
            (spec("mcp__s__b", "Fetch"), true),
        ];
        let upfront = mcp("s", Some("Use me well"), &tools, false);
        let deferred = mcp("s", Some("Use me well"), &tools, true);
        assert_eq!(upfront.listing, upfront.full);
        assert_eq!(deferred.full, upfront.full);
        assert!(
            deferred.listing < upfront.listing && deferred.note.contains("1 deferred"),
            "{deferred:?}"
        );
        let rows = vec![
            deferred,
            Cost {
                kind: "skill",
                name: "x".into(),
                listing: 7,
                full: 70,
                note: String::new(),
            },
        ];
        let r = report(&rows, 1000);
        assert!(
            r.contains("| MCP server | s |")
                && r.contains("| skill | x | 7 | 70 |")
                && r.contains("of 1000"),
            "{r}"
        );
    }
}
