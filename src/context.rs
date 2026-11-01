//! System prompt assembly: environment info, project instruction files
//! (AGENTS.md / CLAUDE.md / CHANTO.md), skills, custom commands and agents.

use crate::config::home_dir;
use crate::util::Canonical as _;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    /// Listed to the model and loadable with the Skill tool
    /// (off with `disable-model-invocation: true`).
    pub model_invocable: bool,
    /// Runs as `/name` (off with `user-invocable: false`).
    pub user_invocable: bool,
    /// `context: fork`: run in a subagent of this type (`agent:`, default general-purpose).
    pub fork: Option<String>,
    /// Defined by the project (not the user); its !`cmd` blocks need folder trust.
    pub project: bool,
    pub meta: SkillMeta,
}

#[derive(Debug, Clone)]
pub struct CustomCommand {
    pub name: String,
    pub description: String,
    pub body: String,
    /// Defined by the project (not the user); its !`cmd` blocks need folder trust.
    pub project: bool,
    pub meta: SkillMeta,
}

/// Frontmatter skills and command files share that changes how they run
/// (Claude Code's fields of the same names).
#[derive(Debug, Clone, Default)]
pub struct SkillMeta {
    /// `argument-hint`: shown next to `/name` in completion and `/help`.
    pub argument_hint: Option<String>,
    /// `allowed-tools`: permission rules pre-approved for the turn that invokes it.
    pub allowed_tools: Vec<String>,
    /// `model`: the model for the rest of that turn (or the forked subagent's).
    pub model: Option<String>,
    /// `effort`: the effort level for the rest of that turn.
    pub effort: Option<String>,
    /// `hooks`: registered when it's invoked, kept for the rest of the session.
    pub hooks: Option<crate::config::HooksConfig>,
    /// `shell`: `bash` (default) or `powershell`, for its !`cmd` blocks.
    pub shell: Option<String>,
    /// `background` (with `context: fork`): false waits for the subagent's report.
    pub background: Option<bool>,
}

/// An `mcpServers` entry of a subagent: a configured server's name, or an
/// inline definition connected while the subagent runs.
#[derive(Debug, Clone)]
pub enum AgentMcp {
    Named(String),
    Inline(String, Box<crate::config::McpServerConfig>),
}

#[derive(Debug, Clone, Default)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// Allowed tools; `None` is all of them.
    pub tools: Option<Vec<String>>,
    /// `disallowedTools`, removed from whatever `tools` allows.
    pub disallowed_tools: Vec<String>,
    /// As written: a model spec, `inherit` or a Claude Code alias (`sonnet`, …).
    pub model: Option<String>,
    pub prompt: String,
    /// `permissionMode` as written (`default`, `acceptEdits`, `auto`, `dontAsk`,
    /// `bypassPermissions`, `plan`, `manual`).
    pub permission_mode: Option<String>,
    /// `maxTurns`: model calls before it stops with a partial report.
    pub max_turns: Option<u32>,
    /// `skills`: skills whose content is preloaded into its prompt.
    pub skills: Vec<String>,
    /// `mcpServers`.
    pub mcp_servers: Vec<AgentMcp>,
    /// `hooks`: active only while it runs (`Stop` fires as `SubagentStop`).
    pub hooks: Option<crate::config::HooksConfig>,
    /// `memory`: `user`, `project` or `local` persistent memory directory.
    pub memory: Option<String>,
    /// `background: true`: always runs in the background.
    pub background: bool,
    /// `effort`: its effort level.
    pub effort: Option<String>,
    /// `isolation: worktree`: runs in a temporary git worktree.
    pub isolation: Option<String>,
    /// `initialPrompt`: the first user turn when it runs as the main session agent.
    pub initial_prompt: Option<String>,
    /// `color`: how it's shown (red, blue, green, yellow, purple, orange, pink, cyan).
    pub color: Option<String>,
    /// `omitClaudeMd: true`: starts without the instruction files.
    pub omit_claude_md: bool,
    /// Defined by a project (its hooks and inline MCP servers need folder trust).
    pub project: bool,
    /// From a plugin (permissionMode, mcpServers, hooks and initialPrompt are ignored).
    pub plugin: bool,
    pub path: PathBuf,
}

pub fn strip_frontmatter(s: &str) -> &str {
    if let Some(rest) = s.strip_prefix("---")
        && let Some(end) = rest.find("\n---")
    {
        return rest[end + 4..].trim_start();
    }
    s
}

/// A frontmatter value.
#[derive(Debug, Clone, PartialEq)]
pub enum FmValue {
    Str(String),
    List(Vec<String>),
    /// Something this parser doesn't read: a nested map, an unclosed quote.
    Other,
}

/// Parse frontmatter, the YAML subset these files use: plain and quoted
/// scalars (continued on indented lines), `[a, b]` and `- a` lists, `|` and
/// `>` block scalars, comments. Nested maps come back as [`FmValue::Other`].
pub fn frontmatter(s: &str) -> Vec<(String, FmValue)> {
    let Some(rest) = s.strip_prefix("---") else {
        return vec![];
    };
    let Some(end) = rest.find("\n---") else {
        return vec![];
    };
    let lines: Vec<&str> = rest[..end]
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line.trim().is_empty() || line.trim_start().starts_with('#') || indent(line) > 0 {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        // The value's further lines: indented ones, and `- ` items right under an empty key.
        let start = i;
        while i < lines.len()
            && (lines[i].trim().is_empty()
                || indent(lines[i]) > 0
                || (v.is_empty() && lines[i].starts_with('-')))
        {
            i += 1;
        }
        out.push((
            k.trim().trim_matches('"').trim_matches('\'').to_string(),
            fm_value(v, &lines[start..i]),
        ));
    }
    out
}

fn indent(l: &str) -> usize {
    l.len() - l.trim_start().len()
}

fn fm_value(v: &str, more: &[&str]) -> FmValue {
    if v.is_empty() || v.starts_with('#') {
        let mut items = Vec::new();
        for l in more
            .iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            let Some(item) = l
                .strip_prefix('-')
                .filter(|x| x.is_empty() || x.starts_with([' ', '\t']))
            else {
                return FmValue::Other;
            };
            match scalar(item) {
                Some(s) if !s.is_empty() => items.push(s),
                Some(_) => {}
                None => return FmValue::Other,
            }
        }
        return if items.is_empty() {
            FmValue::Str(String::new())
        } else {
            FmValue::List(items)
        };
    }
    let first = v.as_bytes()[0];
    if (first == b'|' || first == b'>')
        && strip_comment(&v[1..])
            .chars()
            .all(|c| matches!(c, '-' | '+' | '0'..='9'))
    {
        let ind = more
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| indent(l))
            .min()
            .unwrap_or(0);
        let mut s = String::new();
        let mut prev_text = false;
        for l in more {
            let l: &str = if l.trim().is_empty() {
                ""
            } else {
                l.get(ind..).unwrap_or(l.trim_start())
            };
            if first == b'|' {
                s.push_str(l);
                s.push('\n');
            } else if l.is_empty() {
                s.push('\n');
                prev_text = false;
            } else {
                if prev_text {
                    s.push(' ');
                }
                s.push_str(l);
                prev_text = true;
            }
        }
        let mut s = s.trim_end_matches('\n').to_string();
        if !v.contains('-') && !s.is_empty() {
            s.push('\n');
        }
        return FmValue::Str(s);
    }
    // Scalars and flow lists may continue on indented lines.
    let mut joined = v.to_string();
    for t in more.iter().map(|l| l.trim()).filter(|t| !t.is_empty()) {
        joined.push(' ');
        joined.push_str(t);
    }
    match first {
        b'[' => flow_list(&joined)
            .map(FmValue::List)
            .unwrap_or(FmValue::Other),
        b'{' => FmValue::Other,
        _ => scalar(&joined).map(FmValue::Str).unwrap_or(FmValue::Other),
    }
}

/// One scalar: quoted (with escapes) or plain (minus a trailing comment).
/// `None` for an unclosed quote.
fn scalar(s: &str) -> Option<String> {
    let s = s.trim();
    let mut chars = s.char_indices();
    match chars.next() {
        Some((_, '"')) => {
            let mut out = String::new();
            let mut esc = false;
            for (i, c) in chars {
                if esc {
                    out.push(match c {
                        'n' => '\n',
                        't' => '\t',
                        c => c,
                    });
                    esc = false;
                } else if c == '\\' {
                    esc = true;
                } else if c == '"' {
                    return comment_only(&s[i + 1..]).then_some(out);
                } else {
                    out.push(c);
                }
            }
            None
        }
        Some((_, '\'')) => {
            let mut out = String::new();
            let mut it = chars.peekable();
            while let Some((i, c)) = it.next() {
                if c != '\'' {
                    out.push(c);
                } else if matches!(it.peek(), Some((_, '\''))) {
                    it.next();
                    out.push('\'');
                } else {
                    return comment_only(&s[i + 1..]).then_some(out);
                }
            }
            None
        }
        _ => Some(strip_comment(s).to_string()),
    }
}

fn comment_only(t: &str) -> bool {
    let t = t.trim();
    t.is_empty() || t.starts_with('#')
}

fn strip_comment(s: &str) -> &str {
    let s = s.trim();
    if s.starts_with('#') {
        return "";
    }
    match s.find(" #").or_else(|| s.find("\t#")) {
        Some(i) => s[..i].trim_end(),
        None => s,
    }
}

/// `[a, "b, c"]` → `a`, `b, c`. `None` if unclosed or nested.
fn flow_list(s: &str) -> Option<Vec<String>> {
    let inner = s.strip_prefix('[')?;
    let mut items: Vec<String> = Vec::new();
    let (mut cur, mut quote, mut esc) = (String::new(), None::<char>, false);
    for (i, c) in inner.char_indices() {
        if let Some(q) = quote {
            cur.push(c);
            if esc {
                esc = false;
            } else if c == '\\' && q == '"' {
                esc = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            ',' => items.push(std::mem::take(&mut cur)),
            ']' => {
                items.push(std::mem::take(&mut cur));
                if !comment_only(&inner[i + 1..]) {
                    return None;
                }
                let mut out = Vec::new();
                for x in items.iter().map(|x| x.trim()).filter(|x| !x.is_empty()) {
                    out.push(scalar(x)?);
                }
                return Some(out);
            }
            '[' | '{' => return None,
            _ => cur.push(c),
        }
    }
    None
}

/// A text field; lists are joined with ", ".
pub(crate) fn fm_get(fm: &[(String, FmValue)], k: &str) -> Option<String> {
    let v = match &fm.iter().find(|(a, _)| a == k)?.1 {
        FmValue::Str(s) => s.trim().to_string(),
        FmValue::List(v) => v.join(", "),
        FmValue::Other => return None,
    };
    Some(v).filter(|v| !v.is_empty())
}

/// A list field (`tools`, `paths`): a list or a comma-separated string.
/// `None` when absent or empty, `Some(Err(()))` when present but unreadable.
pub(crate) fn fm_list(fm: &[(String, FmValue)], k: &str) -> Option<Result<Vec<String>, ()>> {
    match &fm.iter().find(|(a, _)| a == k)?.1 {
        FmValue::Str(s) if s.trim().is_empty() => None,
        FmValue::Str(s) => Some(Ok(split_commas(s))),
        FmValue::List(v) => Some(Ok(v
            .iter()
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect())),
        FmValue::Other => Some(Err(())),
    }
}

/// A boolean field: `true`/`false`, also `yes`/`no`, `on`/`off`, `1`/`0`, in
/// any case (as Claude Code reads skill frontmatter).
fn fm_bool(fm: &[(String, FmValue)], k: &str) -> Option<bool> {
    match fm_get(fm, k)?.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// A tool list that may also be space-separated (`allowed-tools: Read Grep`,
/// `Bash(git add *) Bash(git commit *)`): splits on commas and on spaces
/// outside parentheses.
fn fm_tools(fm: &[(String, FmValue)], k: &str) -> Vec<String> {
    let items = match &fm.iter().find(|(a, _)| a == k).map(|(_, v)| v) {
        Some(FmValue::Str(s)) => vec![s.clone()],
        Some(FmValue::List(v)) => v.clone(),
        _ => return vec![],
    };
    let mut out = Vec::new();
    for item in items {
        let (mut cur, mut depth) = (String::new(), 0i32);
        for c in item.chars() {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                ',' | ' ' | '\t' if depth <= 0 => {
                    out.push(std::mem::take(&mut cur));
                    continue;
                }
                _ => {}
            }
            cur.push(c);
        }
        out.push(cur);
    }
    out.into_iter()
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

/// The frontmatter as YAML (block maps and sequences, flow lists and maps,
/// quoted and plain scalars, `|`/`>` blocks), for the nested fields the flat
/// reader skips (`hooks`, `mcpServers`). Plain `true`/`false`, `null` and
/// numbers are typed. Empty when there's no frontmatter.
pub fn frontmatter_yaml(s: &str) -> serde_json::Map<String, serde_json::Value> {
    let Some(rest) = s.strip_prefix("---") else {
        return Default::default();
    };
    let Some(end) = rest.find("\n---") else {
        return Default::default();
    };
    let mut lines: Vec<(usize, String)> = rest[..end]
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(|l| (indent(l), l.trim().to_string()))
        .collect();
    let mut i = 0;
    match yaml_node(&mut lines, &mut i, 0) {
        serde_json::Value::Object(m) => m,
        _ => Default::default(),
    }
}

/// The node starting at `lines[*i]`, whose lines are indented by `ind`.
fn yaml_node(lines: &mut Vec<(usize, String)>, i: &mut usize, ind: usize) -> serde_json::Value {
    use serde_json::Value;
    let Some((_, first)) = lines.get(*i) else {
        return Value::Null;
    };
    if first == "-" || first.starts_with("- ") {
        let mut items = Vec::new();
        while *i < lines.len()
            && lines[*i].0 == ind
            && (lines[*i].1 == "-" || lines[*i].1.starts_with("- "))
        {
            let text = lines[*i].1[1..].trim_start().to_string();
            let off = lines[*i].1.len() - text.len();
            if text.is_empty() {
                *i += 1;
                match lines.get(*i).map(|l| l.0) {
                    Some(n) if n > ind => items.push(yaml_node(lines, i, n)),
                    _ => items.push(Value::Null),
                }
            } else if yaml_key(&text).is_some() {
                // `- key: value` starts a map whose keys line up with `key`.
                lines[*i] = (ind + off, text);
                items.push(yaml_node(lines, i, ind + off));
            } else {
                items.push(yaml_scalar(&text));
                *i += 1;
            }
        }
        return Value::Array(items);
    }
    if yaml_key(first).is_none() {
        let v = yaml_scalar(first);
        *i += 1;
        return v;
    }
    let mut map = serde_json::Map::new();
    while *i < lines.len() && lines[*i].0 == ind {
        let line = lines[*i].1.clone();
        let Some((k, v)) = yaml_key(&line) else { break };
        *i += 1;
        let value = if v.is_empty() {
            match lines.get(*i) {
                Some((n, _)) if *n > ind => {
                    let n = *n;
                    yaml_node(lines, i, n)
                }
                // A sequence may sit at its key's own indentation.
                Some((n, t)) if *n == ind && (t == "-" || t.starts_with("- ")) => {
                    yaml_node(lines, i, ind)
                }
                _ => Value::Null,
            }
        } else if (v.starts_with('|') || v.starts_with('>'))
            && v[1..].chars().all(|c| matches!(c, '-' | '+' | '0'..='9'))
        {
            let start = *i;
            while *i < lines.len() && lines[*i].0 > ind {
                *i += 1;
            }
            let base = lines[start..*i].iter().map(|l| l.0).min().unwrap_or(0);
            let body: Vec<String> = lines[start..*i]
                .iter()
                .map(|(n, t)| format!("{}{t}", " ".repeat(n - base)))
                .collect();
            let joined = if v.starts_with('|') {
                body.join("\n")
            } else {
                body.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" ")
            };
            Value::String(joined)
        } else {
            // A plain scalar may continue on more-indented lines.
            let mut text = v.to_string();
            if !v.starts_with(['"', '\'', '[', '{']) {
                while *i < lines.len() && lines[*i].0 > ind {
                    text.push(' ');
                    text.push_str(&lines[*i].1);
                    *i += 1;
                }
            }
            yaml_scalar(&text)
        };
        map.insert(k, value);
    }
    Value::Object(map)
}

/// `key: rest` (or `key:`) → (key, rest). None when the line isn't a map entry.
fn yaml_key(line: &str) -> Option<(String, String)> {
    if line.starts_with(['[', '{', '-']) {
        return None;
    }
    let (key, rest) = if let Some(q @ ('"' | '\'')) = line.chars().next() {
        let close = line[1..].find(q)? + 1;
        (line[1..close].to_string(), &line[close + 1..])
    } else {
        let at = line
            .char_indices()
            .find(|&(j, c)| {
                c == ':'
                    && line[j + 1..]
                        .chars()
                        .next()
                        .is_none_or(|n| n == ' ' || n == '\t')
            })?
            .0;
        (line[..at].trim().to_string(), &line[at..])
    };
    let rest = rest.strip_prefix(':')?;
    if !(rest.is_empty() || rest.starts_with([' ', '\t'])) || key.is_empty() {
        return None;
    }
    let rest = rest.trim();
    // A comment-only value is an empty one (a nested node follows).
    Some((
        key,
        if rest.starts_with('#') {
            String::new()
        } else {
            rest.to_string()
        },
    ))
}

fn yaml_scalar(text: &str) -> serde_json::Value {
    use serde_json::Value;
    let t = text.trim();
    if t.starts_with('[')
        && let Some(items) = flow_items(t, '[', ']')
    {
        return Value::Array(items.iter().map(|x| yaml_scalar(x)).collect());
    }
    if t.starts_with('{')
        && let Some(items) = flow_items(t, '{', '}')
    {
        let mut m = serde_json::Map::new();
        for item in items {
            if let Some((k, v)) = yaml_key(&item) {
                m.insert(k, yaml_scalar(&v));
            }
        }
        return Value::Object(m);
    }
    if t.starts_with(['"', '\'']) {
        return scalar(t).map(Value::String).unwrap_or(Value::Null);
    }
    let t = strip_comment(t);
    match t {
        "true" | "True" | "TRUE" => Value::Bool(true),
        "false" | "False" | "FALSE" => Value::Bool(false),
        "" | "~" | "null" | "Null" | "NULL" => Value::Null,
        _ => t
            .parse::<i64>()
            .map(Value::from)
            .ok()
            .or_else(|| {
                t.parse::<f64>()
                    .ok()
                    .filter(|f| f.is_finite())
                    .map(Value::from)
            })
            .unwrap_or_else(|| Value::String(t.to_string())),
    }
}

/// The top-level items of a `[...]` or `{...}` flow collection (nested ones
/// kept whole).
fn flow_items(s: &str, open: char, close: char) -> Option<Vec<String>> {
    let inner = s.strip_prefix(open)?;
    let (mut items, mut cur, mut depth, mut quote) =
        (Vec::new(), String::new(), 0i32, None::<char>);
    for (i, c) in inner.char_indices() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '[' | '{' => depth += 1,
            ']' | '}' if depth > 0 => depth -= 1,
            c if c == close => {
                items.push(std::mem::take(&mut cur));
                return comment_only(&inner[i + 1..]).then(|| {
                    items
                        .into_iter()
                        .map(|x| x.trim().to_string())
                        .filter(|x| !x.is_empty())
                        .collect()
                });
            }
            ',' if depth == 0 => {
                items.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    None
}

/// `hooks:` from YAML frontmatter, in the settings layout; handlers Chanto
/// can't run are dropped. None when absent or unreadable.
fn fm_hooks(
    yaml: &serde_json::Map<String, serde_json::Value>,
) -> Option<crate::config::HooksConfig> {
    let v = yaml.get("hooks")?.as_object()?;
    // Handler by handler, so one with a mistyped field doesn't take the rest with it.
    let mut h = crate::config::HooksConfig::new();
    for (event, groups) in v {
        for g in groups.as_array().into_iter().flatten() {
            let mut g = g.clone();
            let nested = g.as_object_mut().and_then(|o| o.remove("hooks"));
            let Ok(mut group) = serde_json::from_value::<crate::config::HookConfig>(g) else {
                continue;
            };
            group.hooks = nested
                .as_ref()
                .and_then(|n| n.as_array())
                .into_iter()
                .flatten()
                .filter_map(|x| serde_json::from_value(x.clone()).ok())
                .collect();
            h.entry(event.clone()).or_default().push(group);
        }
    }
    let _ = crate::config::flatten_hooks(&mut h);
    (!h.is_empty()).then_some(h)
}

/// `mcpServers:` entries: names, and `{name: config}` maps (in a list or as one map).
fn fm_mcp(yaml: &serde_json::Map<String, serde_json::Value>) -> Vec<AgentMcp> {
    let inline = |m: &serde_json::Map<String, serde_json::Value>| -> Vec<AgentMcp> {
        m.iter()
            .filter_map(|(k, v)| {
                serde_json::from_value(v.clone())
                    .ok()
                    .map(|c| AgentMcp::Inline(k.clone(), Box::new(c)))
            })
            .collect()
    };
    match yaml.get("mcpServers") {
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .flat_map(|e| match e {
                serde_json::Value::String(s) => vec![AgentMcp::Named(s.clone())],
                serde_json::Value::Object(m) => inline(m),
                _ => vec![],
            })
            .collect(),
        Some(serde_json::Value::Object(m)) => inline(m),
        Some(serde_json::Value::String(s)) => {
            split_commas(s).into_iter().map(AgentMcp::Named).collect()
        }
        _ => vec![],
    }
}

/// The frontmatter fields beyond name, description, prompt, tools and model,
/// from a JSON definition (`--agents`, which uses the same keys).
pub fn agent_fields_from_json(def: &mut AgentDef, d: &serde_json::Value) {
    let Some(o) = d.as_object() else { return };
    let text = |k: &str| {
        o.get(k)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    };
    let flag = |k: &str| {
        o.get(k).and_then(|v| {
            v.as_bool().or_else(|| {
                v.as_str()
                    .map(|s| matches!(s.to_ascii_lowercase().as_str(), "true" | "yes" | "on" | "1"))
            })
        })
    };
    def.permission_mode = text("permissionMode");
    def.max_turns = o
        .get("maxTurns")
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
        .map(|n| n as u32)
        .filter(|n| *n > 0);
    def.skills = match o.get("skills") {
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect(),
        Some(serde_json::Value::String(s)) => split_commas(s),
        _ => vec![],
    };
    def.mcp_servers = fm_mcp(o);
    def.hooks = fm_hooks(o);
    def.memory = text("memory")
        .map(|m| m.to_ascii_lowercase())
        .filter(|m| matches!(m.as_str(), "user" | "project" | "local"));
    def.background = flag("background").unwrap_or(false);
    def.effort = text("effort");
    def.isolation = text("isolation");
    def.initial_prompt = text("initialPrompt");
    def.color = text("color").map(|c| c.to_ascii_lowercase());
    def.omit_claude_md = flag("omitClaudeMd").unwrap_or(false);
}

/// A top-level field's text as written (`argument-hint: [issue] [format]` is
/// a hint, not a YAML list); one pair of surrounding quotes is removed.
fn fm_raw(text: &str, key: &str) -> Option<String> {
    let rest = text.strip_prefix("---")?;
    let block = &rest[..rest.find("\n---")?];
    let line = block
        .lines()
        .find_map(|l| l.strip_prefix(key)?.trim_start().strip_prefix(':'))?
        .trim();
    let unquoted = ['"', '\'']
        .iter()
        .find_map(|q| line.strip_prefix(*q)?.strip_suffix(*q))
        .unwrap_or(line);
    Some(unquoted.to_string()).filter(|s| !s.is_empty())
}

/// The skill/command fields of [`SkillMeta`].
fn skill_meta(fm: &[(String, FmValue)], text: &str) -> SkillMeta {
    let yaml = if text.contains("hooks:") {
        frontmatter_yaml(text)
    } else {
        Default::default()
    };
    SkillMeta {
        argument_hint: fm_raw(text, "argument-hint"),
        allowed_tools: fm_tools(fm, "allowed-tools"),
        model: fm_get(fm, "model").filter(|m| !m.eq_ignore_ascii_case("inherit")),
        effort: fm_get(fm, "effort"),
        hooks: fm_hooks(&yaml),
        shell: fm_get(fm, "shell").map(|s| s.to_ascii_lowercase()),
        background: fm_bool(fm, "background"),
    }
}

/// Split on commas outside `{}` and `()` (`src/*.{ts,tsx}`, `Bash(a, b)`).
fn split_commas(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut depth) = (Vec::new(), String::new(), 0i32);
    for c in s.chars() {
        match c {
            '{' | '(' => depth += 1,
            '}' | ')' => depth -= 1,
            ',' if depth <= 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push(cur);
    out.into_iter()
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

/// Directories searched for skills/commands/agents, lowest precedence first
/// (Codex keeps commands in `prompts/`; `.agents/skills` is the shared skills dir).
/// Project dirs come from every directory between the git root and cwd, the
/// nearest last so it wins a name clash. The flag marks project dirs.
fn config_dirs(cwd: &Path) -> Vec<(PathBuf, bool)> {
    // `--bare`: nothing from disk (only `--plugin-dir` plugins and `--agents`).
    if crate::launch::get().no_customizations() {
        return Vec::new();
    }
    let home = crate::compat::home();
    let mut out: Vec<(PathBuf, bool)> = [
        crate::compat::codex_dir(),
        home.join(".agents"),
        home.join(".claude"),
        home_dir(),
    ]
    .into_iter()
    .map(|d| (d, false))
    .collect();
    let mut chain = root_chain(cwd);
    if chain.is_empty() {
        chain.push(cwd.to_path_buf());
    }
    for dir in chain {
        for sub in [".codex", ".agents", ".claude", ".chanto"] {
            out.push((dir.join(sub), true));
        }
    }
    out
}

/// `.md` files below `dir`, recursively and sorted, each with its path under
/// `dir` as a `:`-separated name (`frontend/test.md` → `frontend:test`).
fn md_files(dir: &Path) -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<(PathBuf, String)>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in paths {
            let join = |n: &str| {
                if prefix.is_empty() {
                    n.to_string()
                } else {
                    format!("{prefix}:{n}")
                }
            };
            if p.is_dir() {
                let n = p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if depth < 4 && !n.starts_with('.') {
                    walk(&p, &join(&n), depth + 1, out);
                }
            } else if p.extension().and_then(|x| x.to_str()) == Some("md") {
                let n = join(&p.file_stem().unwrap_or_default().to_string_lossy());
                out.push((p, n));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, "", 0, &mut out);
    out
}

/// Component directories (`skills`, `commands`, `agents`), lowest precedence
/// first: enabled plugins (names prefixed `plugin:`), then home and project dirs.
/// The flag marks project-defined components.
pub(crate) fn component_dirs(cwd: &Path, kind: &str) -> Vec<(PathBuf, Option<String>, bool)> {
    let mut out: Vec<(PathBuf, Option<String>, bool)> = Vec::new();
    for p in crate::compat::active_plugins() {
        out.extend(
            p.dirs(kind)
                .into_iter()
                .map(|d| (d, Some(p.name.clone()), false)),
        );
    }
    for (d, project) in config_dirs(cwd) {
        out.push((d.join(kind), None, project));
        // Codex reads prompts only from the user's home, never from a project.
        if kind == "commands" && !project {
            out.push((d.join("prompts"), None, false));
        }
    }
    out
}

fn namespaced(ns: &Option<String>, name: String) -> String {
    match ns {
        Some(p) => format!("{p}:{name}"),
        None => name,
    }
}

#[derive(Debug, Clone)]
pub struct OutputStyle {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// Built-in output styles (as in Claude Code) plus `output-styles/*.md` files.
/// `optimize.concise_prose`: the Concise output style's wording, limited to
/// prose so it can't shorten code, tool calls or thinking.
pub const CONCISE_PROSE: &str = "# Brevity\nKeep prose replies short. Lead with the result or the answer; skip preambles, recaps of what you did step by step, and restating the task. Keep status updates between tool calls to a short sentence, or none. Still say plainly when something failed or couldn't be verified. This applies only to prose addressed to the user: never shorten code, tool arguments, file contents or your reasoning.";

/// `optimize.quiet` (`--quiet`): no narration between tool calls, and one
/// short structured summary at the end. Fixed text, so the prompt prefix
/// stays the same from request to request.
pub const QUIET: &str = "# Quiet mode\nDon't write narration or status text between tool calls (no \"Let me…\", \"Now I'll…\", or recaps of a step): call the tools directly. When the task is done, write one short summary: what changed (the files), what was verified and how (the commands run and their result, or that nothing was verified), and anything left to do or uncertain. This applies only to prose addressed to the user. It never applies to code, tool arguments, file contents or your reasoning, and never stops you from asking the user a question you genuinely need answered.";

/// `optimize.spec_check`: treat a written request as the contract, and exercise
/// the result against it when no test does. Fixed text, so the prompt prefix
/// stays stable within a session. The verification sentence is kept: a shorter
/// wording without it was no cheaper in practice (the model still spent the
/// turns) and bought less.
pub const SPEC_CHECK: &str = "# Checking a written spec\nWhen the request is a specification rather than a failing test, its wording is the contract: implement every entry point callers need (not just the helper the text names) and match the exact shape it describes — order, nesting, indexing, counts. Before finishing, exercise what you wrote as a caller would: with no test covering it, run a short ad-hoc check using the spec's examples and the boundaries it implies (empty, zero, negative, both int and float for \"a number\"), then delete the scratch file.";

pub fn load_output_styles(cwd: &Path) -> Vec<OutputStyle> {
    let mut out = vec![
        OutputStyle {
            name: "Explanatory".into(),
            description: "Explains implementation choices and codebase patterns as it works".into(),
            body: "While working, add short \"Insight\" notes that explain why you chose an approach, the trade-offs involved, and patterns in this codebase worth knowing. Keep them brief and specific to the code at hand.".into(),
        },
        OutputStyle {
            name: "Learning".into(),
            description: "Pauses to let you write small pieces yourself, to learn by doing".into(),
            body: "Help the user learn while working. For small, well-scoped pieces of the change (a function body, a condition, a test case), leave a clearly marked `TODO(human)` with guidance instead of writing it yourself, explain what it needs to do, and ask the user to fill it in. Do the rest yourself.".into(),
        },
        OutputStyle {
            name: "Concise".into(),
            description: "Short answers: the result first, little else".into(),
            body: "Keep replies short. Lead with the result or the answer; skip preambles, recaps of what you did step by step, and restating the question. Use a sentence where a paragraph would do and a list only when it's clearer. Still say plainly when something failed or couldn't be verified.".into(),
        },
        OutputStyle {
            name: "Proactive".into(),
            description: "Takes the obvious next steps without being asked".into(),
            body: "Take initiative. When the task implies follow-up work (updating the callers of a changed function, the tests and docs that go with a change, a fix the error you just found clearly needs), do it rather than stopping to ask, and say what you did. Ask first only for anything destructive, outward-facing (pushing, publishing, sending) or a genuine choice between approaches. Finish by naming anything else worth doing that you didn't.".into(),
        },
    ];
    let trusted = crate::config::is_trusted(cwd);
    for (d, ns, project) in component_dirs(cwd, "output-styles") {
        // A project output style can shadow a built-in or user-selected style by
        // name, and the selected style's body is injected into the depth-0
        // system prompt — so an untrusted repo must not supply one (the same
        // folder-trust gate skills, commands and agents use).
        if project && !trusted {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("md") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            let fm = frontmatter(&text);
            let stem = p
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let name = namespaced(&ns, fm_get(&fm, "name").unwrap_or(stem));
            out.retain(|s| !s.name.eq_ignore_ascii_case(&name));
            out.push(OutputStyle {
                name,
                description: fm_get(&fm, "description").unwrap_or_default(),
                body: strip_frontmatter(&text).trim().to_string(),
            });
        }
    }
    out
}

pub fn load_skills(cwd: &Path) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    for (d, ns, project) in component_dirs(cwd, "skills") {
        let Ok(rd) = std::fs::read_dir(d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path().join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            let fm = frontmatter(&text);
            let name = namespaced(
                &ns,
                fm_get(&fm, "name").unwrap_or_else(|| e.file_name().to_string_lossy().to_string()),
            );
            let description = fm_get(&fm, "description").unwrap_or_default();
            let fork = (fm_get(&fm, "context").as_deref() == Some("fork"))
                .then(|| fm_get(&fm, "agent").unwrap_or_else(|| "general-purpose".into()));
            out.retain(|s| s.name != name);
            out.push(Skill {
                name,
                description,
                path: p,
                model_invocable: !fm_bool(&fm, "disable-model-invocation").unwrap_or(false),
                user_invocable: fm_bool(&fm, "user-invocable").unwrap_or(true),
                fork,
                project,
                meta: skill_meta(&fm, &text),
            });
        }
    }
    out
}

/// Commands; files in subdirectories are namespaced (`frontend/test.md` → `/frontend:test`).
pub fn load_commands(cwd: &Path) -> Vec<CustomCommand> {
    let mut out: Vec<CustomCommand> = Vec::new();
    for (d, ns, project) in component_dirs(cwd, "commands") {
        for (p, rel) in md_files(&d) {
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            let fm = frontmatter(&text);
            let name = namespaced(&ns, rel);
            let body = strip_frontmatter(&text).to_string();
            let description = fm_get(&fm, "description")
                .unwrap_or_else(|| crate::util::one_line(body.lines().next().unwrap_or(""), 60));
            out.retain(|c| c.name != name);
            let meta = skill_meta(&fm, &text);
            out.push(CustomCommand {
                name,
                description,
                body,
                project,
                meta,
            });
        }
    }
    out
}

pub fn load_agents(cwd: &Path) -> Vec<AgentDef> {
    let mut out: Vec<AgentDef> = Vec::new();
    // Subfolders only organize: the name comes from frontmatter or the file.
    for (d, ns, project) in component_dirs(cwd, "agents") {
        for (p, _) in md_files(&d) {
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            let fm = frontmatter(&text);
            let yaml = if text.contains("hooks:") || text.contains("mcpServers:") {
                frontmatter_yaml(&text)
            } else {
                Default::default()
            };
            let plugin = ns.is_some();
            let name = namespaced(
                &ns,
                fm_get(&fm, "name").unwrap_or_else(|| {
                    p.file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                }),
            );
            let mut description = fm_get(&fm, "description").unwrap_or_default();
            let (tools, disallowed) = (fm_list(&fm, "tools"), fm_list(&fm, "disallowedTools"));
            // A tool list this parser can't read must never mean "all tools".
            let tools = if matches!(tools, Some(Err(()))) || matches!(disallowed, Some(Err(()))) {
                description.push_str(" (unreadable tools frontmatter: runs with no tools)");
                Some(Vec::new())
            } else {
                tools.and_then(Result::ok)
            };
            out.retain(|a| a.name != name);
            out.push(AgentDef {
                name,
                description,
                tools,
                disallowed_tools: disallowed.and_then(Result::ok).unwrap_or_default(),
                model: fm_get(&fm, "model"),
                prompt: strip_frontmatter(&text).to_string(),
                permission_mode: fm_get(&fm, "permissionMode").filter(|_| !plugin),
                max_turns: fm_get(&fm, "maxTurns")
                    .and_then(|n| n.parse().ok())
                    .filter(|n| *n > 0),
                skills: fm_list(&fm, "skills")
                    .and_then(Result::ok)
                    .unwrap_or_default(),
                mcp_servers: if plugin { vec![] } else { fm_mcp(&yaml) },
                hooks: if plugin { None } else { fm_hooks(&yaml) },
                memory: fm_get(&fm, "memory")
                    .map(|m| m.to_ascii_lowercase())
                    .filter(|m| matches!(m.as_str(), "user" | "project" | "local")),
                background: fm_bool(&fm, "background").unwrap_or(false),
                effort: fm_get(&fm, "effort"),
                isolation: fm_get(&fm, "isolation"),
                initial_prompt: fm_get(&fm, "initialPrompt").filter(|_| !plugin),
                color: fm_get(&fm, "color").map(|c| c.to_ascii_lowercase()),
                omit_claude_md: fm_bool(&fm, "omitClaudeMd").unwrap_or(false),
                project,
                plugin,
                path: p.clone(),
            });
        }
    }
    // opencode agents rank below every native one; project ones need trust.
    if crate::compat::enabled().opencode {
        let dirs: Vec<PathBuf> = root_chain(cwd)
            .iter()
            .map(|d| d.join(".opencode"))
            .filter(|d| d.is_dir())
            .collect();
        let project = !dirs.is_empty() && crate::config::is_trusted(cwd);
        for a in crate::compat::opencode_agents(if project { &dirs } else { &[] }) {
            if !out.iter().any(|o| o.name == a.name) {
                out.push(a);
            }
        }
    }
    out
}

/// Per-directory instruction file names, in load order. In a directory with
/// `AGENTS.override.md` (Codex), it replaces `AGENTS.md`.
const DIR_INSTRUCTION_FILES: &[&str] = &[
    "AGENTS.override.md",
    "AGENTS.md",
    "CLAUDE.md",
    ".claude/CLAUDE.md",
    "CHANTO.md",
    "CLAUDE.local.md",
    "AGENTS.local.md",
];

const MAX_INSTRUCTION_BYTES: usize = 40_000;
/// All instruction files together (about 40k tokens).
const MAX_INSTRUCTION_TOTAL: usize = 160_000;

/// Instruction files in one directory (no recursion into subdirectories).
fn dir_instruction_files(dir: &Path) -> Vec<PathBuf> {
    let has_override = dir.join("AGENTS.override.md").is_file();
    DIR_INSTRUCTION_FILES
        .iter()
        .filter(|n| !(has_override && **n == "AGENTS.md"))
        .map(|n| dir.join(n))
        .filter(|p| p.is_file())
        .collect()
}

/// Read an instruction file, inlining `@path` imports (Claude Code style,
/// relative to the file, up to 5 levels deep). With `confine`, imports must
/// stay inside that directory (project files in an untrusted folder can't
/// pull in e.g. `~/.ssh/id_rsa`).
pub fn read_instructions(p: &Path, confine: Option<&Path>) -> Option<String> {
    fn go(
        p: &Path,
        depth: usize,
        seen: &mut Vec<PathBuf>,
        confine: Option<&Path>,
    ) -> Option<String> {
        let canon = p.canonical().ok()?;
        if seen.contains(&canon) {
            return None;
        }
        seen.push(canon);
        let text = std::fs::read_to_string(p).ok()?;
        if depth >= 5 || !text.contains('@') {
            return Some(text);
        }
        let base = p.parent().unwrap_or(Path::new("."));
        let mut out = String::new();
        let mut in_code = false;
        for line in text.lines() {
            out.push_str(line);
            out.push('\n');
            if line.trim_start().starts_with("```") {
                in_code = !in_code;
            }
            if in_code {
                continue;
            }
            // Strip inline code spans before looking for @imports.
            let plain: String = line.split('`').step_by(2).collect::<Vec<_>>().join(" ");
            for tok in plain.split_whitespace().filter_map(|t| t.strip_prefix('@')) {
                let tok = tok.trim_end_matches([',', ';', ')', ']']);
                if tok.is_empty() || tok.contains("://") {
                    continue;
                }
                let path = match tok.strip_prefix("~/") {
                    Some(rest) => crate::compat::home().join(rest),
                    None => base.join(tok),
                };
                let allowed = match (confine, path.canonical()) {
                    (None, _) => true,
                    (Some(root), Ok(c)) => c.starts_with(root),
                    (Some(_), Err(_)) => false,
                };
                if allowed
                    && path.is_file()
                    && let Some(inc) = go(&path, depth + 1, seen, confine)
                {
                    out.push_str(&format!(
                        "<imported path=\"{}\">\n{}\n</imported>\n",
                        path.display(),
                        inc.trim()
                    ));
                }
            }
        }
        Some(out)
    }
    go(p, 0, &mut Vec::new(), confine)
}

/// Directories from the git root (or cwd) down to cwd.
fn root_chain(cwd: &Path) -> Vec<PathBuf> {
    let home = crate::compat::home();
    let mut chain = Vec::new();
    let mut d = Some(cwd);
    while let Some(dir) = d {
        if dir == home {
            break;
        }
        chain.push(dir.to_path_buf());
        if dir.join(".git").exists() {
            break;
        }
        d = dir.parent();
    }
    chain.reverse();
    chain
}

/// Instruction files from home and from the git root down to cwd.
pub fn instruction_files(cwd: &Path) -> Vec<(PathBuf, String)> {
    if crate::launch::get().no_customizations() {
        return Vec::new();
    }
    let home = crate::compat::home();
    let codex = crate::compat::codex_dir();
    let codex_global = if codex.join("AGENTS.override.md").is_file() {
        codex.join("AGENTS.override.md")
    } else {
        codex.join("AGENTS.md")
    };
    let mut paths = vec![
        home_dir().join("AGENTS.md"),
        home.join(".claude").join("CLAUDE.md"),
        codex_global,
    ];
    // Claude Code rules without a `paths` filter: ~/.claude/rules/**/*.md, then
    // .claude/rules/**/*.md from the git root down to cwd.
    let chain = root_chain(cwd);
    let rule_dirs = std::iter::once(home.join(".claude").join("rules"))
        .chain(chain.iter().map(|d| d.join(".claude").join("rules")));
    for d in rule_dirs {
        paths.extend(md_files(&d).into_iter().map(|(p, _)| p).filter(|p| {
            std::fs::read_to_string(p)
                .map(|t| rule_paths(&t).is_empty())
                .unwrap_or(false)
        }));
    }
    // Other tools' files (Gemini, Cursor, Copilot, Aider, opencode): user-level
    // ones always, project ones only in a trusted folder.
    let compat = crate::compat::enabled();
    paths.extend(crate::compat::foreign_user_instructions(&compat));
    let project = chain
        .iter()
        .find(|d| d.join(".git").exists())
        .cloned()
        .unwrap_or_else(|| cwd.to_path_buf());
    let mut foreign: Vec<PathBuf> = Vec::new();
    let mut on_request: Vec<(PathBuf, &str, String)> = Vec::new();
    for dir in &chain {
        paths.extend(dir_instruction_files(dir));
        foreign.extend(crate::compat::foreign_dir_files(dir, &compat));
        for r in crate::compat::foreign_rules(dir, &compat) {
            match r.apply {
                crate::compat::Apply::Always => foreign.push(r.path),
                crate::compat::Apply::OnRequest(d) => on_request.push((r.path, r.tool, d)),
                crate::compat::Apply::Globs(_) => {}
            }
        }
    }
    let trusted =
        (!foreign.is_empty() || !on_request.is_empty()) && crate::config::is_trusted(&project);
    if trusted {
        paths.extend(foreign);
    }
    let mut out = load_unique(paths, &project);
    if trusted && !on_request.is_empty() {
        let mut index = String::from(
            "Rules to read (with the Read tool) when the task matches their description:\n",
        );
        for (p, tool, d) in &on_request {
            index.push_str(&format!("- {} ({tool}): {d}\n", p.display()));
        }
        out.push((project.join(".cursor").join("rules"), index));
    }
    out
}

/// Other tools' path-scoped rules for this project (Cursor `globs`, Copilot
/// `applyTo`), for listings: the file and its globs. Empty unless trusted.
pub fn scoped_foreign_rules(cwd: &Path) -> Vec<(PathBuf, Vec<String>)> {
    let compat = crate::compat::enabled();
    let mut chain = root_chain(cwd);
    if chain.is_empty() {
        chain.push(cwd.to_path_buf());
    }
    let out: Vec<(PathBuf, Vec<String>)> = chain
        .iter()
        .flat_map(|d| crate::compat::rule_files(d, &compat))
        .filter_map(|r| match r.apply {
            crate::compat::Apply::Globs(g) => Some((r.path, g)),
            _ => None,
        })
        .collect();
    if out.is_empty() || !crate::config::is_trusted(&chain[0]) {
        return vec![];
    }
    out
}

/// Read files, skipping duplicates (same file via symlink, or identical text).
fn load_unique(paths: Vec<PathBuf>, project: &Path) -> Vec<(PathBuf, String)> {
    // Most tool calls find no new instruction files: skip the trust lookup.
    if paths.is_empty() {
        return Vec::new();
    }
    // Files the user owns (outside the project) may import anything; project
    // files only from inside the project unless the folder is trusted.
    let given = project;
    let project = project
        .canonical()
        .unwrap_or_else(|_| project.to_path_buf());
    let trusted = crate::config::is_trusted(&project);
    let mut out: Vec<(PathBuf, String)> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    let (mut total, mut omitted) = (0usize, Vec::new());
    for p in paths {
        let Ok(canon) = p.canonical() else { continue };
        if seen.contains(&canon) {
            continue;
        }
        seen.push(canon.clone());
        // Where the file sits, not where a symlink points: a project's
        // `CLAUDE.md -> ~/.ssh/id_rsa` is still a project file.
        let in_project =
            canon.starts_with(&project) || p.starts_with(&project) || p.starts_with(given);
        if in_project && !trusted && !canon.starts_with(&project) {
            continue;
        }
        let confine = (!trusted && in_project).then_some(project.as_path());
        let Some(t) = read_instructions(&p, confine) else {
            continue;
        };
        let t = crate::util::strip_hidden(&t).into_owned();
        let t = strip_frontmatter(&t).to_string();
        if t.trim().is_empty() || out.iter().any(|(_, o)| o.trim() == t.trim()) {
            continue;
        }
        let t = crate::util::truncate(&t, MAX_INSTRUCTION_BYTES);
        // A glob like opencode's `"instructions": ["docs/**/*.md"]` can name
        // hundreds of files: past the budget, list them instead (a 400-doc
        // repo made a 4 MB system prompt, more than any model's context).
        if total + t.len() > MAX_INSTRUCTION_TOTAL {
            omitted.push(p);
            continue;
        }
        total += t.len();
        out.push((p, t));
    }
    if let (false, Some((_, last))) = (omitted.is_empty(), out.last_mut()) {
        let shown: Vec<String> = omitted
            .iter()
            .take(50)
            .map(|p| crate::util::rel(&project, p))
            .collect();
        let more = if omitted.len() > shown.len() {
            format!("\n…and {} more", omitted.len() - shown.len())
        } else {
            String::new()
        };
        last.push_str(&format!(
            "\n\n({} more instruction files weren't loaded, to keep the context small. Read the ones that matter for the task:\n{}{more})",
            omitted.len(),
            shown.join("\n")
        ));
    }
    out
}

/// `paths:` globs from a rule file's frontmatter (a list or comma-separated).
fn rule_paths(text: &str) -> Vec<String> {
    fm_list(&frontmatter(text), "paths")
        .and_then(Result::ok)
        .unwrap_or_default()
}

/// Directories seen holding none of the entries path-scoped rules live under,
/// with their modification time then.
static NO_RULES: std::sync::Mutex<
    Option<std::collections::HashMap<PathBuf, std::time::SystemTime>>,
> = std::sync::Mutex::new(None);

/// Whether `base` may hold path-scoped rules (`.claude/rules`, Cursor's or
/// Copilot's). Every file-touching tool call asks, for each directory from the
/// git root down; most hold none of them, and looking costs several failed
/// lookups each. Creating one changes the directory's modification time, so a
/// directory seen empty is skipped until that changes. A time within a couple
/// of seconds of now could hide a change made in the same clock tick, so only
/// an older one is trusted (as git does with its index).
fn may_hold_rules(base: &Path) -> bool {
    let Ok(mtime) = std::fs::metadata(base).and_then(|m| m.modified()) else {
        return true;
    };
    let mut cache = NO_RULES.lock().unwrap();
    let cache = cache.get_or_insert_with(Default::default);
    if cache.get(base) == Some(&mtime) {
        return false;
    }
    let any = [".claude", ".cursor", ".cursorrules", ".github"]
        .iter()
        .any(|n| std::fs::symlink_metadata(base.join(n)).is_ok());
    if any {
        cache.remove(base);
    } else if mtime
        .elapsed()
        .is_ok_and(|age| age > std::time::Duration::from_secs(2))
    {
        cache.insert(base.to_path_buf(), mtime);
    }
    any
}

/// Instruction files in subdirectories between the startup directory and a
/// file the agent just touched (loaded on demand, like Claude Code's nested
/// CLAUDE.md and Codex's per-directory AGENTS.md scopes). `loaded` tracks
/// directories already handled.
pub fn nested_instructions(
    cwd: &Path,
    file: &Path,
    loaded: &mut std::collections::HashSet<PathBuf>,
) -> Vec<(PathBuf, String)> {
    let Ok(rel) = file.strip_prefix(cwd) else {
        return vec![];
    };
    if crate::launch::get().no_customizations() {
        return vec![];
    }
    let mut dir = cwd.to_path_buf();
    let mut paths = Vec::new();
    let comps: Vec<_> = rel.components().collect();
    // Read from the config file, so only once something needs scanning: most
    // calls touch directories already handled.
    let compat = std::cell::OnceCell::new();
    let compat = || compat.get_or_init(crate::compat::enabled);
    // Other tools' files here need folder trust (checked only when there are any).
    let mut foreign = Vec::new();
    for c in comps.iter().take(comps.len().saturating_sub(1)) {
        dir.push(c);
        if loaded.insert(dir.clone()) {
            paths.extend(dir_instruction_files(&dir));
            foreign.extend(crate::compat::foreign_dir_files(&dir, compat()));
        }
    }
    // Path-scoped Claude Code rules (.claude/rules/**/*.md with `paths:` globs,
    // relative to the directory holding `.claude`), from the git root down to cwd.
    let mut bases = root_chain(cwd);
    if bases.is_empty() {
        bases.push(cwd.to_path_buf());
    }
    for base in &bases {
        let Ok(rel) = file.strip_prefix(base) else {
            continue;
        };
        if !may_hold_rules(base) {
            continue;
        }
        for (p, _) in md_files(&base.join(".claude").join("rules")) {
            if loaded.contains(&p) {
                continue;
            }
            let globs = std::fs::read_to_string(&p)
                .map(|t| rule_paths(&t))
                .unwrap_or_default();
            let hit = globs.iter().any(|g| {
                globset::Glob::new(g)
                    .map(|g| g.compile_matcher().is_match(rel))
                    .unwrap_or(false)
            });
            if hit {
                loaded.insert(p.clone());
                paths.push(p);
            }
        }
        // Cursor `globs` and Copilot `applyTo` rules.
        for r in crate::compat::rule_files(base, compat()) {
            let crate::compat::Apply::Globs(globs) = &r.apply else {
                continue;
            };
            if !loaded.contains(&r.path)
                && globs.iter().any(|g| {
                    globset::Glob::new(g)
                        .map(|g| g.compile_matcher().is_match(rel))
                        .unwrap_or(false)
                })
            {
                loaded.insert(r.path.clone());
                foreign.push(r.path);
            }
        }
    }
    if !foreign.is_empty() && crate::config::is_trusted(cwd) {
        paths.extend(foreign);
    }
    load_unique(paths, cwd)
}

/// Git details started early (at launch) and used once by the first system
/// prompt; later prompts ask git again so they stay current.
static GIT_PREFETCH: std::sync::Mutex<Option<(PathBuf, std::thread::JoinHandle<Option<String>>)>> =
    std::sync::Mutex::new(None);

pub fn prefetch_git(cwd: &Path) {
    let dir = cwd.to_path_buf();
    let d = dir.clone();
    *GIT_PREFETCH.lock().unwrap() = Some((dir, std::thread::spawn(move || git_info_now(&d))));
}

fn git_info(cwd: &Path) -> Option<String> {
    let pending = GIT_PREFETCH.lock().unwrap().take();
    match pending {
        Some((dir, h)) if dir == cwd => h.join().ok().flatten(),
        _ => git_info_now(cwd),
    }
}

/// Whether git could find a repository from `cwd`. False only when it surely
/// can't: no `GIT_DIR`/`GIT_WORK_TREE` pointing elsewhere, and no `.git` or
/// bare repository in `cwd` or any directory above it. Saves spawning git
/// processes that would only fail.
fn may_be_in_git_repo(cwd: &Path) -> bool {
    if std::env::var_os("GIT_DIR").is_some() || std::env::var_os("GIT_WORK_TREE").is_some() {
        return true;
    }
    cwd.ancestors().any(|d| {
        d.join(".git").exists()
            || (d.join("HEAD").is_file() && d.join("objects").is_dir() && d.join("refs").is_dir())
    })
}

fn git_info_now(cwd: &Path) -> Option<String> {
    if !may_be_in_git_repo(cwd) {
        return None;
    }
    // Before the folder is trusted: git must run nothing the repository names.
    let run = |args: &[&str]| crate::hostgit::read(cwd, args);
    // Three git processes; run them side by side rather than one after another.
    let (branch, status, log) = std::thread::scope(|sc| {
        let status = sc.spawn(|| run(&["status", "--short", "--ignore-submodules=all"]));
        let log = sc.spawn(|| run(&["log", "--oneline", "-5"]));
        let branch = run(&["rev-parse", "--abbrev-ref", "HEAD"]);
        (
            branch,
            status.join().ok().flatten(),
            log.join().ok().flatten(),
        )
    });
    let branch = branch?;
    let (status, log) = (status.unwrap_or_default(), log.unwrap_or_default());
    let status = if status.is_empty() {
        "(clean)".to_string()
    } else {
        crate::util::truncate(&status, 1500)
    };
    Some(format!(
        "Current branch: {branch}\n\nStatus:\n{status}\n\nRecent commits:\n{log}"
    ))
}

pub(crate) const BASE_PROMPT: &str = r#"You are an expert software engineering agent running in the user's terminal, with tools to read, search and edit code, run commands, browse the web and control a browser. You help with coding tasks: fixing bugs, adding features, refactoring, explaining code, and more.

# How to work
- Understand before acting: search (Grep/Glob) and Read the relevant code first. Never guess at file contents or APIs; check them.
- Follow the existing conventions of the codebase (style, libraries, patterns). Check that a library is actually used in the project before using it.
- Make focused changes that do exactly what was asked. Don't add unrequested features, refactors, or comments that narrate the change.
- Prefer the smallest fix the codebase already supports. If an existing option, helper or fallback already produces the behaviour needed — absolute paths, a default applied when a lookup fails, a fallback a sibling API uses — use it instead of adding a new parameter or a parallel mechanism. Fix the differing call site, not a new layer around it.
- Treat existing behaviour as the contract: prefer the reading of the request that adds the new behaviour without changing unrelated output, or tests that already exist. If an existing test would have to change for your approach to work, that is usually a sign the design is wrong — reconsider it before editing the test.
- For a bug report, reproduce the symptom before fixing it: follow the report to the code path it describes, make the smallest test or command that fails for the reported reason (not a related one), and watch it fail. When the report contrasts two behaviours ("this API returns X, that one returns Y"), compare the two code paths and fix the one that differs — including the values the report leaves unset — rather than changing something nearby that looks plausible. Then watch the reproduction pass; a plausible-looking change elsewhere isn't evidence.
- Fix the cause, not the symptom: when a value can arrive in more than one equivalent form, canonicalize it where it enters rather than special-casing every comparison; when a step fails on broken input, prefer a form that doesn't depend on it (for example `type(x)` instead of `x.__class__`) over catching the failure and substituting a fallback.
- Verify your work: run the project's build, type checker, linter and tests where they exist. Use the project's own check — its test command, or the suite its CI runs, covering what you changed — not only the tests you wrote. If something fails, fix it and re-run. Do not claim success without evidence.
- Never make a check pass by weakening it: don't delete, skip, or loosen an existing test or assertion. Fix the code the test exercises; if the new behaviour genuinely conflicts with that test, say so instead of editing it silently.
- Stay within the project: search it, not the whole filesystem (`find /` takes minutes). If a test failed before your change because the environment lacks something (a missing package, a tool), say so and move on; don't install or copy packages into the project's environment unless that's the task.
- Work from the code in the working directory and its own dependencies. Don't hunt for a ready-made fix elsewhere: no searching the disk, package caches, other virtualenvs or other checkouts, no downloading other releases of the project's packages, no digging through git's unreachable objects, no fetching the project's upstream fix, PR or commit.
- For multi-step tasks, plan with TodoWrite and keep it updated. Mark each step completed as soon as it is done.
- When something fails, read the error carefully and diagnose the root cause instead of retrying blindly.
- Be autonomous: keep going until the task is fully done. Only stop to ask the user when you are genuinely blocked on a decision only they can make.

# Tool use
- Use the dedicated tools instead of shell equivalents: Read (not cat/head), Edit/Write (not sed/echo redirection), Grep (not grep/rg), Glob (not find/ls for searching).
- Call multiple independent tools in the same response (e.g. read several files at once) — they run in parallel. Read everything you will need in one response rather than one file per step; the first message lists the project's files.
- Change existing files with Edit (small, exact replacements); use Write only for new files or when most of a file changes.
- Once the checks you ran pass, stop: don't re-run them for more detail. Keep the final summary short.
- Edit requires old_string to match the file exactly; copy it from Read output without the line-number prefix.
- Use Task with subagent_type "Explore" for broad open-ended searches across a large codebase, to keep your context small.
- Use WebSearch/WebFetch for documentation and current information; use the Browser tool when a page needs JavaScript, interaction, or login.
- Long-running work must be watched, not just started. Run builds, test suites, benchmarks and other slow commands in the background, then actively monitor them — use the Monitor tool so their output reaches you as it arrives, or poll them — and react to what they report. Never launch a long job and move on without checking whether it is progressing, finished or stuck.
- If a tool call is denied, do not retry the same call; adjust your approach or ask the user.
- Never run destructive or irreversible commands (deleting data, force pushing, resetting work) unless the user explicitly asked for it.
- Never commit, push or create PRs unless the user asks.

# Communication
- Be concise and direct. Use GitHub-flavored markdown. Reference code as `path/to/file.rs:42`.
- Don't narrate every step; briefly say what you're about to do before significant actions, and summarize the outcome at the end (what changed, how it was verified, anything left to do).
- Report results honestly: if tests fail or something could not be verified, say so."#;

pub struct PromptInputs<'a> {
    pub cwd: &'a Path,
    pub model: &'a str,
    pub skills: &'a [Skill],
    pub agents: &'a [AgentDef],
    pub mcp_note: Option<String>,
    /// Include the instruction files (off for `omitClaudeMd` subagents).
    pub instructions: bool,
}

/// Where the per-project part of the system prompt starts: everything before
/// it is the same in every session, everything from it on depends on the
/// directory, the day and the project.
pub const PROJECT_CONTEXT: &str = "\n\n# Environment\n";

pub fn system_prompt(p: &PromptInputs) -> String {
    let mut s = String::from(BASE_PROMPT);
    s.push_str(PROJECT_CONTEXT);
    s.push_str(&format!("- Working directory: {}\n", p.cwd.display()));
    s.push_str(&format!(
        "- Platform: {} ({})\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    let shell = match crate::shell::tool_flavor() {
        crate::shell::Flavor::Posix => "bash",
        crate::shell::Flavor::PowerShell => {
            "PowerShell, not bash: write commands in its syntax (Windows PowerShell 5 has no `&&`; use `;` or `if ($?)`)"
        }
        crate::shell::Flavor::Cmd => "cmd.exe, not bash: write commands in its syntax, on one line",
    };
    s.push_str(&format!(
        "- Shell: {shell}\n- Date: {}\n",
        chrono::Local::now().format("%Y-%m-%d")
    ));
    s.push_str(&format!("- Model: {}\n", p.model));
    let is_git = git_info(p.cwd);
    s.push_str(&format!(
        "- Git repository: {}\n",
        if is_git.is_some() { "yes" } else { "no" }
    ));
    if let Some(g) = is_git {
        s.push_str(&format!("\n<git_status>\n{g}\n</git_status>\n"));
    }
    // `disable-model-invocation` skills run only as `/name`.
    if p.skills.iter().any(|sk| sk.model_invocable) {
        s.push_str(
            "\n# Skills\nLoad a skill with the Skill tool when the task matches its description:\n",
        );
        for sk in p.skills.iter().filter(|sk| sk.model_invocable) {
            s.push_str(&format!("- {}: {}\n", sk.name, sk.description));
        }
    }
    if !p.agents.is_empty() {
        s.push_str("\n# Custom subagents (use with Task subagent_type)\n");
        for a in p.agents {
            s.push_str(&format!("- {}: {}\n", a.name, a.description));
        }
    }
    if let Some(m) = &p.mcp_note {
        s.push_str(&format!("\n# MCP servers\n{m}\n"));
    }
    let files = if p.instructions {
        instruction_files(p.cwd)
    } else {
        vec![]
    };
    if !files.is_empty() {
        s.push_str("\n# Project instructions\nThese instructions from the user's instruction files OVERRIDE default behavior. Follow them exactly.\n");
        for (path, text) in files {
            s.push_str(&format!(
                "\n<file path=\"{}\">\n{}\n</file>\n",
                path.display(),
                text.trim()
            ));
        }
    }
    s
}

pub fn mode_reminder(mode: crate::permissions::Mode) -> Option<&'static str> {
    use crate::permissions::Mode;
    match mode {
        Mode::Plan => Some(
            "<system-reminder>Plan mode is active. Do NOT make any edits, run any non-readonly commands, or otherwise change the system. Research the codebase with read-only tools, then present a concrete implementation plan by calling ExitPlanMode. Ask clarifying questions with AskUserQuestion if needed.</system-reminder>",
        ),
        Mode::Auto => Some(
            "<system-reminder>Auto mode is active: work autonomously without asking for confirmation. Actions are screened by a safety monitor; when it blocks one, the user is asked to approve it themselves (or, if no user is attached, the action is refused). Do not retry a blocked action expecting it to go through on its own — prefer a safer alternative the monitor will allow.</system-reminder>",
        ),
        _ => None,
    }
}

pub const INIT_PROMPT: &str = r#"Survey this repository and write an AGENTS.md at its root for the coding agents that will work here next.

Cover:
- How to build, check and test the project, including the command for a single test.
- The architecture a newcomer can't infer from one file: how the pieces fit and where the main flows live.
- The conventions the code actually follows: naming, layout, error handling, test style.

Keep it short and specific to this repository — skip file inventories and generic advice. Fold in what matters from any existing AGENTS.md, CLAUDE.md, README.md, .cursor rules or .github/copilot-instructions.md. If AGENTS.md is already present, edit it rather than starting over."#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_runs_only_where_a_repository_can_be_found() {
        let d = std::env::temp_dir().join(format!("chanto-git-{}", uuid::Uuid::new_v4().simple()));
        let sub = d.join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        let env_points_elsewhere =
            std::env::var_os("GIT_DIR").is_some() || std::env::var_os("GIT_WORK_TREE").is_some();
        let outside = std::env::temp_dir()
            .ancestors()
            .any(|a| a.join(".git").exists());
        if !env_points_elsewhere && !outside {
            assert!(!may_be_in_git_repo(&sub));
        }
        // A work tree (`.git` is a directory, or a file in a linked worktree)
        // found from any directory below it.
        std::fs::write(d.join(".git"), "gitdir: /elsewhere\n").unwrap();
        assert!(may_be_in_git_repo(&sub));
        std::fs::remove_file(d.join(".git")).unwrap();
        // A bare repository.
        std::fs::create_dir_all(d.join("objects")).unwrap();
        std::fs::create_dir_all(d.join("refs")).unwrap();
        std::fs::write(d.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        assert!(may_be_in_git_repo(&d));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn instruction_files_have_a_total_budget() {
        let d = std::env::temp_dir().join(format!(
            "chanto-ctx-budget-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(d.join("docs")).unwrap();
        let paths: Vec<PathBuf> = (0..10)
            .map(|i| {
                let p = d.join(format!("docs/d{i}.md"));
                std::fs::write(&p, format!("doc {i} {}", "x".repeat(39_000))).unwrap();
                p
            })
            .collect();
        let out = load_unique(paths, &d);
        let total: usize = out.iter().map(|(_, t)| t.len()).sum();
        assert!(
            out.len() == 4 && total < MAX_INSTRUCTION_TOTAL + 2000,
            "{} files, {total} bytes",
            out.len()
        );
        let note = &out.last().unwrap().1;
        assert!(
            note.contains("6 more instruction files weren't loaded") && note.contains("d9.md"),
            "{}",
            &note[note.len() - 300..]
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn instruction_discovery() {
        let d = std::env::temp_dir().join(format!("chanto-ctx-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        let d = d.canonical().unwrap();
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
        let w = |p: &str, t: &str| {
            let p = d.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, t).unwrap();
        };
        std::fs::create_dir_all(d.join(".git")).unwrap();
        w("AGENTS.md", "base agents");
        w("AGENTS.override.md", "override agents");
        w(
            "CLAUDE.md",
            "claude root\nSee @docs/style.md and `@not/imported.md`.",
        );
        w("docs/style.md", "use tabs");
        w("home/secret.txt", "TOPSECRET");
        w("AGENTS.local.md", "local @home/secret.txt @../../etc/hosts");
        w(".claude/rules/always.md", "always rule");
        w(
            ".claude/rules/ts.md",
            "---\npaths:\n  - \"web/**/*.ts\"\n---\nts rule",
        );
        w("pkg/AGENTS.md", "pkg rules");
        w("web/app.ts", "x");
        let files = instruction_files(&d);
        let all: String = files
            .iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("override agents"));
        assert!(!all.contains("base agents"), "override replaces AGENTS.md");
        assert!(all.contains("use tabs"), "@import inlined");
        assert!(
            all.contains("TOPSECRET"),
            "imports inside the project are fine"
        );
        assert!(
            !all.contains("localhost"),
            "untrusted project files can't import outside the project"
        );
        #[cfg(unix)]
        {
            // Nor be a symlink to a file outside it.
            let out = d
                .parent()
                .unwrap()
                .join(format!("chanto-ctx-out-{}", uuid::Uuid::new_v4().simple()));
            std::fs::write(&out, "OUTSIDE-SECRET").unwrap();
            std::os::unix::fs::symlink(&out, d.join("CLAUDE.local.md")).unwrap();
            let files = instruction_files(&d);
            std::fs::remove_file(d.join("CLAUDE.local.md")).unwrap();
            std::fs::remove_file(&out).unwrap();
            assert!(
                !files.iter().any(|(_, t)| t.contains("OUTSIDE-SECRET")),
                "a symlink out of an untrusted project isn't read"
            );
        }
        assert!(all.contains("always rule"));
        assert!(!all.contains("ts rule"), "path-scoped rule loads lazily");
        assert!(!all.contains("pkg rules"), "nested files load lazily");
        let mut loaded = std::collections::HashSet::new();
        let n = nested_instructions(&d, &d.join("pkg/lib.rs"), &mut loaded);
        assert_eq!(n.len(), 1);
        assert!(n[0].1.contains("pkg rules"));
        assert!(
            nested_instructions(&d, &d.join("pkg/other.rs"), &mut loaded).is_empty(),
            "only once"
        );
        let n = nested_instructions(&d, &d.join("web/app.ts"), &mut loaded);
        assert!(n.iter().any(|(_, t)| t.contains("ts rule")));
    }

    #[cfg(unix)]
    #[test]
    fn a_rule_added_after_a_directory_was_seen_empty_is_found() {
        let d = std::env::temp_dir().join(format!(
            "chanto-ctx-norules-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(d.join(".git")).unwrap();
        let d = d.canonical().unwrap();
        // Untouched for an hour: old enough for "no rules here" to be remembered.
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let age = |p: &Path| {
            std::fs::File::open(p)
                .unwrap()
                .set_modified(hour_ago)
                .unwrap()
        };
        age(&d);
        let mut loaded = std::collections::HashSet::new();
        assert!(nested_instructions(&d, &d.join("a.ts"), &mut loaded).is_empty());
        assert!(!may_hold_rules(&d), "remembered as holding no rules");
        std::fs::create_dir_all(d.join(".claude/rules")).unwrap();
        std::fs::write(
            d.join(".claude/rules/ts.md"),
            "---\npaths:\n  - \"**/*.ts\"\n---\nts rule",
        )
        .unwrap();
        let n = nested_instructions(&d, &d.join("b.ts"), &mut loaded);
        assert!(n.iter().any(|(_, t)| t.contains("ts rule")), "{n:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn other_tools_instructions_need_trust() {
        let d = std::env::temp_dir().join(format!(
            "chanto-ctx-foreign-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(d.join(".git")).unwrap();
        let d = d.canonical().unwrap();
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
        let w = |p: &str, t: &str| {
            let p = d.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, t).unwrap();
        };
        w("home/.gemini/GEMINI.md", "user gemini");
        w("AGENTS.md", "shared text");
        w("GEMINI.md", "shared text");
        w("sub/GEMINI.md", "sub gemini");
        w(".cursorrules", "legacy cursor");
        w(
            ".cursor/rules/api.mdc",
            "---\ndescription: API conventions\n---\napi",
        );
        w(
            ".github/instructions/rs.instructions.md",
            "---\napplyTo: \"**/*.rs\"\n---\nrust rule",
        );
        let text = |f: &[(PathBuf, String)]| {
            f.iter()
                .map(|(_, t)| t.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let all = text(&instruction_files(&d));
        assert!(all.contains("user gemini"), "user files load anywhere");
        assert!(
            !all.contains("legacy cursor"),
            "project files of other tools need trust"
        );
        crate::config::trust(&d).unwrap();
        let files = instruction_files(&d);
        let all = text(&files);
        assert!(all.contains("legacy cursor"));
        assert_eq!(
            all.matches("shared text").count(),
            1,
            "identical content is loaded once"
        );
        assert!(all.contains("api.mdc (Cursor): API conventions"));
        assert!(!all.contains("rust rule"));
        let mut loaded = std::collections::HashSet::new();
        let n = text(&nested_instructions(&d, &d.join("sub/lib.rs"), &mut loaded));
        assert!(n.contains("sub gemini") && n.contains("rust rule"));
        assert_eq!(scoped_foreign_rules(&d).len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn frontmatter_yaml_subset() {
        let fm = frontmatter(
            "---\n# comment\nname: \"quoted: name\"\ndescription: >-\n  Folded\n  text\n\n  next\nnotes: |\n  line 1\n    indented\ntools:\n  - Read\n  - 'Grep'   # trailing\nflow: [Bash(git:*), \"a, b\"]\nplain: Read, Grep # c\nlong: starts here\n  and continues\nbad: [Read, Grep\nhooks:\n  Stop:\n    - command: x\nempty:\n---\nbody",
        );
        let get = |k: &str| fm.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        assert_eq!(get("name"), Some(FmValue::Str("quoted: name".into())));
        assert_eq!(
            get("description"),
            Some(FmValue::Str("Folded text\nnext".into()))
        );
        assert_eq!(
            get("notes"),
            Some(FmValue::Str("line 1\n  indented\n".into()))
        );
        assert_eq!(
            get("tools"),
            Some(FmValue::List(vec!["Read".into(), "Grep".into()]))
        );
        assert_eq!(
            get("flow"),
            Some(FmValue::List(vec!["Bash(git:*)".into(), "a, b".into()]))
        );
        assert_eq!(
            get("long"),
            Some(FmValue::Str("starts here and continues".into()))
        );
        assert_eq!(get("bad"), Some(FmValue::Other));
        assert_eq!(get("hooks"), Some(FmValue::Other));
        assert_eq!(
            fm_list(&fm, "plain"),
            Some(Ok(vec!["Read".to_string(), "Grep".to_string()]))
        );
        assert_eq!(fm_list(&fm, "bad"), Some(Err(())));
        assert_eq!(fm_list(&fm, "empty"), None);
        assert_eq!(fm_get(&fm, "tools").as_deref(), Some("Read, Grep"));
        assert_eq!(
            split_commas("src/*.{ts,tsx}, lib/*.rs"),
            vec!["src/*.{ts,tsx}", "lib/*.rs"]
        );
        assert!(frontmatter("---\nbroken").is_empty());
    }

    #[test]
    fn nested_frontmatter_reads_as_yaml() {
        let text = "---\nname: db-reader\n# c\nhooks:\n  PreToolUse:\n    - matcher: \"Bash\"\n      hooks:\n        - type: command\n          command: \"./validate.sh # not a comment\"\n          timeout: 30\n          async: true\n  Stop:\n  - hooks:\n    - type: prompt\n      prompt: |\n        Line one\n          indented\n      continueOnBlock: yes\nmcpServers:\n  - playwright:\n      type: stdio\n      command: npx\n      args: [\"-y\", '@playwright/mcp@latest']\n  - github\nflow: {a: 1, b: [x, y]}\n---\nbody";
        let y = frontmatter_yaml(text);
        assert_eq!(y["name"], "db-reader");
        let pre = &y["hooks"]["PreToolUse"][0];
        assert_eq!(pre["matcher"], "Bash");
        assert_eq!(pre["hooks"][0]["command"], "./validate.sh # not a comment");
        assert_eq!(pre["hooks"][0]["timeout"], 30);
        assert_eq!(pre["hooks"][0]["async"], true);
        assert_eq!(
            y["hooks"]["Stop"][0]["hooks"][0]["prompt"],
            "Line one\n  indented"
        );
        assert_eq!(y["hooks"]["Stop"][0]["hooks"][0]["continueOnBlock"], "yes");
        assert_eq!(
            y["mcpServers"][0]["playwright"]["args"],
            serde_json::json!(["-y", "@playwright/mcp@latest"])
        );
        assert_eq!(y["mcpServers"][1], "github");
        assert_eq!(y["flow"], serde_json::json!({"a": 1, "b": ["x", "y"]}));
        let h = fm_hooks(&y).unwrap();
        assert_eq!(h["PreToolUse"][0].command, "./validate.sh # not a comment");
        assert!(h["PreToolUse"][0].run_async && h["PreToolUse"][0].matcher == "Bash");
        assert!(
            !h.contains_key("Stop"),
            "a hook whose fields don't parse is dropped, the rest kept"
        );
        let m = fm_mcp(&y);
        assert!(
            matches!(&m[0], AgentMcp::Inline(n, c) if n == "playwright" && c.command == "npx" && c.args.len() == 2)
        );
        assert!(matches!(&m[1], AgentMcp::Named(n) if n == "github"));
        assert!(frontmatter_yaml("no frontmatter").is_empty());
    }

    #[test]
    fn skill_and_agent_frontmatter_fields() {
        let d = std::env::temp_dir().join(format!("chanto-fm-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(d.join(".git")).unwrap();
        let d = d.canonical().unwrap();
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
        let w = |p: &str, t: &str| {
            let p = d.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, t).unwrap();
        };
        w(
            ".claude/skills/commit/SKILL.md",
            "---\ndescription: Commit\nargument-hint: \"[message]\"\nallowed-tools: Bash(git add *) Bash(git commit *), Read\nmodel: inherit\neffort: low\nshell: PowerShell\ncontext: fork\nbackground: No\nhooks:\n  PostToolUse:\n    - matcher: Bash\n      hooks:\n        - type: command\n          command: echo hi\n          once: true\n---\nCommit.",
        );
        w(
            ".claude/commands/fix.md",
            "---\nargument-hint: [issue]\nallowed-tools:\n  - Edit\nmodel: sonnet\n---\nFix $1",
        );
        w(
            ".claude/agents/full.md",
            "---\nname: full\ndescription: All fields\npermissionMode: acceptEdits\nmaxTurns: 12\nskills: [commit, other]\nmcpServers:\n  - github\nhooks:\n  Stop:\n    - hooks:\n        - type: command\n          command: ./stop.sh\nmemory: Local\nbackground: true\neffort: max\nisolation: worktree\ninitialPrompt: /review\ncolor: Purple\nomitClaudeMd: true\nunknownKey: ignored\n---\nPrompt.",
        );
        let skills = load_skills(&d);
        let s = &skills.iter().find(|s| s.name == "commit").unwrap().meta;
        assert_eq!(s.argument_hint.as_deref(), Some("[message]"));
        assert_eq!(
            s.allowed_tools,
            vec!["Bash(git add *)", "Bash(git commit *)", "Read"]
        );
        assert_eq!(
            (
                s.model.as_deref(),
                s.effort.as_deref(),
                s.shell.as_deref(),
                s.background
            ),
            (None, Some("low"), Some("powershell"), Some(false))
        );
        assert!(s.hooks.as_ref().unwrap()["PostToolUse"][0].once);
        let c = &load_commands(&d)
            .into_iter()
            .find(|c| c.name == "fix")
            .unwrap()
            .meta;
        assert_eq!(
            (c.argument_hint.as_deref(), c.model.as_deref()),
            (Some("[issue]"), Some("sonnet"))
        );
        assert_eq!(c.allowed_tools, vec!["Edit"]);
        let a = load_agents(&d)
            .into_iter()
            .find(|a| a.name == "full")
            .unwrap();
        assert_eq!(
            (
                a.permission_mode.as_deref(),
                a.max_turns,
                a.memory.as_deref(),
                a.effort.as_deref()
            ),
            (Some("acceptEdits"), Some(12), Some("local"), Some("max"))
        );
        assert_eq!(a.skills, vec!["commit", "other"]);
        assert!(matches!(&a.mcp_servers[..], [AgentMcp::Named(n)] if n == "github"));
        assert_eq!(a.hooks.as_ref().unwrap()["Stop"][0].command, "./stop.sh");
        assert!(a.background && a.omit_claude_md && a.project && !a.plugin);
        assert_eq!(
            (
                a.isolation.as_deref(),
                a.initial_prompt.as_deref(),
                a.color.as_deref()
            ),
            (Some("worktree"), Some("/review"), Some("purple"))
        );
        assert_eq!(a.prompt.trim(), "Prompt.");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn components_from_root_down_with_subfolders() {
        let d = std::env::temp_dir().join(format!("chanto-comp-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(d.join(".git")).unwrap();
        let d = d.canonical().unwrap();
        let _home_lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
        let w = |p: &str, t: &str| {
            let p = d.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, t).unwrap();
        };
        w(
            ".claude/commands/frontend/test.md",
            "run the frontend tests",
        );
        w(
            ".claude/skills/s1/SKILL.md",
            "---\ndescription: far\n---\nfar",
        );
        w(
            "sub/.claude/skills/s1/SKILL.md",
            "---\ndescription: near\n---\nnear",
        );
        w(
            ".claude/skills/manual/SKILL.md",
            "---\ndescription: manual\ndisable-model-invocation: true\nuser-invocable: false\ncontext: fork\nagent: Explore\n---\nx",
        );
        w(
            ".claude/agents/review/reader.md",
            "---\nname: reader\ndescription: >\n  Reads\n  code\ntools:\n  - Read\n  - Grep\ndisallowedTools: [Bash]\nmodel: sonnet\n---\nRead only.",
        );
        w(
            ".claude/agents/broken.md",
            "---\nname: broken\ntools: [Read, Grep\n---\nx",
        );
        w(".claude/rules/backend/deep.md", "deep rule");
        let cwd = d.join("sub");
        let cmds = load_commands(&cwd);
        let c = cmds
            .iter()
            .find(|c| c.name == "frontend:test")
            .expect("subfolder command is namespaced");
        assert!(c.project);
        let skills = load_skills(&cwd);
        assert_eq!(
            skills.iter().find(|s| s.name == "s1").unwrap().description,
            "near",
            "nearer dir wins"
        );
        let m = skills.iter().find(|s| s.name == "manual").unwrap();
        assert!(!m.model_invocable && !m.user_invocable && m.project);
        assert_eq!(m.fork.as_deref(), Some("Explore"));
        let agents = load_agents(&cwd);
        let r = agents
            .iter()
            .find(|a| a.name == "reader")
            .expect("agents in subfolders load");
        assert_eq!(r.tools, Some(vec!["Read".to_string(), "Grep".to_string()]));
        assert_eq!(r.disallowed_tools, vec!["Bash".to_string()]);
        assert_eq!(r.description, "Reads code");
        assert_eq!(r.model.as_deref(), Some("sonnet"));
        let b = agents.iter().find(|a| a.name == "broken").unwrap();
        assert_eq!(b.tools, Some(vec![]), "unreadable tools fail closed");
        let all: String = instruction_files(&cwd)
            .into_iter()
            .map(|(_, t)| t)
            .collect();
        assert!(
            all.contains("deep rule"),
            "rules load recursively from the git root"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod output_style_tests {
    use super::*;

    #[test]
    fn loads_builtin_and_custom_styles() {
        let _g = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let d =
            std::env::temp_dir().join(format!("chanto-styles-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(d.join(".chanto/output-styles")).unwrap();
        std::fs::write(
            d.join(".chanto/output-styles/terse.md"),
            "---\nname: Terse\ndescription: One-line answers\n---\nAnswer in one line.\n",
        )
        .unwrap();
        let names =
            |styles: &[OutputStyle]| styles.iter().map(|s| s.name.clone()).collect::<Vec<_>>();

        // Untrusted folder: the built-ins load, but the project style is skipped
        // — a project style can shadow a built-in and its body reaches the
        // system prompt, so it needs folder trust (like skills/commands/agents).
        let untrusted = names(&load_output_styles(&d));
        for b in ["Explanatory", "Learning", "Concise", "Proactive"] {
            assert!(
                untrusted.contains(&b.to_string()),
                "built-in {b} missing: {untrusted:?}"
            );
        }
        assert!(
            !untrusted.contains(&"Terse".to_string()),
            "untrusted project style must not load: {untrusted:?}"
        );

        // Trusting the folder lets its style load (into the isolated test home).
        crate::config::trust(&d).unwrap();
        let styles = load_output_styles(&d);
        let t = styles
            .iter()
            .find(|s| s.name == "Terse")
            .expect("trusted project style loads");
        assert_eq!(t.body, "Answer in one line.");
        assert_eq!(t.description, "One-line answers");
        let _ = std::fs::remove_dir_all(&d);
    }
}
