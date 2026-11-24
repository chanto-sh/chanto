//! Index: a structural skeleton of source files — imports, type definitions,
//! function signatures and their line ranges — so the model reads structure
//! first and full bodies only where it needs them.
//!
//! Powered by tree-sitter. Reads are the largest token cost in a coding
//! session; a skeleton keeps the map and drops the bodies. The line ranges let
//! the model follow up with `Read(offset/limit)` for the exact lines it wants.

use super::{Ctx, Tool, ToolKind, ToolOutput, arg_u64};
use crate::util::{rel, resolve};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tree_sitter::{Language, Node, Parser};

const DEFAULT_MAX_FILES: usize = 15;
const DEFAULT_TOKENS: usize = 1500;
const MAX_FILE_BYTES: u64 = 512 << 10;
const MAX_DEPTH: usize = 12;

const FUNC: &[&str] = &[
    "function_item",
    "function_definition",
    "function_declaration",
    "generator_function_declaration",
    "method_definition",
    "method_declaration",
    "method_signature",
    "abstract_method_signature",
];
const CONTAINER: &[&str] = &[
    "impl_item",
    "class_definition",
    "class_declaration",
    "trait_item",
    "trait_declaration",
    "mod_item",
    "interface_declaration",
];
const TYPES: &[&str] = &[
    "struct_item",
    "enum_item",
    "union_item",
    "type_item",
    "type_alias_declaration",
    "enum_declaration",
    "type_declaration",
];
const SIMPLE: &[&str] = &[
    "use_declaration",
    "import_statement",
    "import_from_statement",
    "export_statement",
    "const_item",
    "static_item",
    "lexical_declaration",
    "macro_definition",
    "import_declaration",
    "const_declaration",
    "var_declaration",
];

/// Extensions without a tree-sitter grammar here still get a regex skeleton.
const FALLBACK_EXTS: &[&str] = &[
    "java", "kt", "kts", "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "cs", "rb", "php", "swift",
    "scala", "sh", "bash", "zsh", "sql", "lua", "r", "ex", "exs", "dart", "m", "mm", "pl", "pm",
    "vue", "svelte",
];

/// Definition patterns for the fallback, in the style of `RepoMap`'s regexes.
const FALLBACK_PATTERNS: &[(&str, &str)] = &[
    ("func", r"(?m)^\s*func\s+(?:\([^)]*\)\s*)?([A-Za-z_]\w*)"),
    (
        "def",
        r"(?m)^\s*(?:async\s+)?def\s+(?:self\.)?([A-Za-z_]\w*)",
    ),
    (
        "function",
        r"(?m)^\s*(?:export\s+)?(?:async\s+)?function\s+([A-Za-z_]\w*)",
    ),
    (
        "class",
        r"(?m)^\s*(?:export\s+)?(?:public\s+|private\s+|protected\s+|abstract\s+|final\s+|sealed\s+|data\s+)*class\s+([A-Za-z_]\w*)",
    ),
    (
        "interface",
        r"(?m)^\s*(?:export\s+)?interface\s+([A-Za-z_]\w*)",
    ),
    ("struct", r"(?m)^\s*(?:pub\s+)?struct\s+([A-Za-z_]\w*)"),
    ("enum", r"(?m)^\s*(?:pub\s+)?enum\s+([A-Za-z_]\w*)"),
    ("type", r"(?m)^\s*(?:pub\s+)?type\s+([A-Za-z_]\w*)"),
    ("impl", r"(?m)^\s*impl(?:<[^>]*>)?\s+([A-Za-z_]\w*)"),
    (
        "module",
        r"(?m)^\s*(?:export\s+)?(?:module|namespace)\s+([A-Za-z_]\w*)",
    ),
    (
        "fun",
        r"(?m)^\s*(?:public\s+|private\s+|internal\s+|protected\s+|suspend\s+|inline\s+)*fun\s+([A-Za-z_]\w*)",
    ),
    (
        "sub",
        r"(?m)^\s*(?:public\s+|private\s+|protected\s+)?sub\s+([A-Za-z_]\w*)",
    ),
];

pub struct Index;

fn language_for(path: &Path) -> Option<Language> {
    match path.extension()?.to_str()? {
        "rs" => Some(tree_sitter_rust::LANGUAGE.into()),
        "py" => Some(tree_sitter_python::LANGUAGE.into()),
        "js" | "jsx" | "mjs" | "cjs" => Some(tree_sitter_javascript::LANGUAGE.into()),
        "go" => Some(tree_sitter_go::LANGUAGE.into()),
        "ts" | "mts" | "cts" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "tsx" => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
        _ => None,
    }
}

fn is_fallback_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| FALLBACK_EXTS.contains(&e))
        .unwrap_or(false)
}

fn supported(path: &Path) -> bool {
    language_for(path).is_some() || is_fallback_ext(path)
}

fn first_descendant<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
        if let Some(found) = first_descendant(child, kind) {
            return Some(found);
        }
    }
    None
}

/// A structure-only skeleton for a language without a tree-sitter grammar.
fn regex_skeleton(name: &str, src: &[u8]) -> String {
    let text = String::from_utf8_lossy(src);
    let mut out = vec![format!("# {name}")];
    for (kind, pat) in FALLBACK_PATTERNS {
        let Ok(re) = regex::Regex::new(pat) else {
            continue;
        };
        for cap in re.captures_iter(&text) {
            let Some(m) = cap.get(1) else { continue };
            let line = text[..m.start()].bytes().filter(|b| *b == b'\n').count() + 1;
            out.push(format!("{kind} {}  [{line}]", m.as_str()));
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|l| seen.insert(l.clone()));
    if out.len() > 400 {
        out.truncate(400);
        out.push("... (more definitions not shown)".into());
    }
    out.join("\n")
}

fn text(node: Node, src: &[u8]) -> String {
    String::from_utf8_lossy(&src[node.byte_range()]).into_owned()
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
        out.push_str("...");
        out
    }
}

fn range(node: Node) -> (usize, usize) {
    (node.start_position().row + 1, node.end_position().row + 1)
}

fn name_of(node: Node, src: &[u8]) -> String {
    node.child_by_field_name("name")
        .map(|n| collapse(&text(n, src)))
        .unwrap_or_default()
}

fn sig(node: Node, src: &[u8]) -> String {
    let (start, end) = range(node);
    let kind = node.kind();
    let name = name_of(node, src);
    if kind == "type_declaration" {
        if let Some(spec) = first_descendant(node, "type_spec") {
            return format!("type {}  [{start}-{end}]", name_of(spec, src));
        }
    }
    if FUNC.contains(&kind) {
        let params = node
            .child_by_field_name("parameters")
            .map(|n| collapse(&text(n, src)))
            .unwrap_or_else(|| "()".into());
        let params = truncate(&params, 160);
        let mut s = format!("fn {name}{params}");
        if let Some(ret) = node.child_by_field_name("return_type") {
            let r = collapse(&text(ret, src));
            let r = r.trim_start_matches(':').trim();
            s.push_str(" -> ");
            s.push_str(&truncate(r, 120));
        }
        return format!("{s}  [{start}-{end}]");
    }
    if TYPES.contains(&kind) {
        return format!(
            "{} {name}  [{start}-{end}]",
            kind.split('_').next().unwrap_or(kind)
        );
    }
    if kind == "trait_item" || kind == "trait_declaration" {
        return format!("trait {name}  [{start}-{end}]");
    }
    if SIMPLE.contains(&kind) {
        let line = truncate(&collapse(&text(node, src)), 140);
        return format!("{line}  [{start}-{end}]");
    }
    format!("{kind} {name}  [{start}-{end}]")
}

fn container_sig(node: Node, src: &[u8]) -> String {
    let (start, end) = range(node);
    let kind = node.kind();
    let name = name_of(node, src);
    let head = match kind {
        "impl_item" => {
            let ty = node
                .child_by_field_name("type")
                .map(|n| collapse(&text(n, src)))
                .unwrap_or_else(|| name.clone());
            match node.child_by_field_name("trait") {
                Some(t) => format!("impl {} for {ty}", truncate(&collapse(&text(t, src)), 80)),
                None => format!("impl {ty}"),
            }
        }
        "class_definition" | "class_declaration" => {
            let supers = node
                .child_by_field_name("superclasses")
                .map(|n| collapse(&text(n, src)))
                .unwrap_or_default();
            if supers.is_empty() {
                format!("class {name}")
            } else {
                format!("class {name}{supers}")
            }
        }
        "trait_item" | "trait_declaration" => format!("trait {name}"),
        "mod_item" => format!("mod {name}"),
        "interface_declaration" => format!("interface {name}"),
        _ => format!("{kind} {name}"),
    };
    format!("{head}  [{start}-{end}]")
}

fn walk_container(node: Node, src: &[u8], out: &mut Vec<String>, indent: &str) {
    let Some(body) = node.child_by_field_name("body") else {
        return;
    };
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if FUNC.contains(&child.kind())
            || TYPES.contains(&child.kind())
            || SIMPLE.contains(&child.kind())
        {
            out.push(format!("{indent}{}", sig(child, src)));
        } else if child.kind() == "trait_item" {
            out.push(format!("{indent}{}", sig(child, src)));
        } else if CONTAINER.contains(&child.kind()) {
            out.push(format!("{indent}{}", container_sig(child, src)));
            walk_container(child, src, out, &format!("{indent}  "));
        }
    }
}

/// The skeleton of one parsed file.
fn skeleton(name: &str, src: &[u8], lang: Language) -> Result<String> {
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .map_err(|e| anyhow::anyhow!("tree-sitter: {e}"))?;
    let tree = parser
        .parse(src, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter could not parse"))?;
    let mut out = vec![format!("# {name}")];
    let mut cursor = tree.root_node().walk();
    for node in tree.root_node().named_children(&mut cursor) {
        let kind = node.kind();
        if FUNC.contains(&kind) || TYPES.contains(&kind) || SIMPLE.contains(&kind) {
            out.push(sig(node, src));
        } else if kind == "trait_item" {
            out.push(sig(node, src));
        } else if CONTAINER.contains(&kind) {
            out.push(container_sig(node, src));
            walk_container(node, src, &mut out, "  ");
        } else if kind == "decorated_definition" {
            if let Some(inner) = node.child_by_field_name("definition") {
                out.push(sig(inner, src));
                if CONTAINER.contains(&inner.kind()) {
                    walk_container(inner, src, &mut out, "  ");
                }
            }
        } else if kind == "expression_statement" {
            out.push(format!(
                "{}  [{}-{}]",
                truncate(&collapse(&text(node, src)), 120),
                range(node).0,
                range(node).1
            ));
        }
    }
    Ok(out.join("\n"))
}

fn is_skippable(name: &str) -> bool {
    matches!(
        name,
        ".git" | "target" | "node_modules" | "dist" | "build" | ".venv" | "__pycache__"
    ) || name.starts_with('.')
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>, limit: usize, depth: usize) {
    if out.len() >= limit || depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if out.len() >= limit {
            return;
        }
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_skippable(&name) {
                collect(&path, out, limit, depth + 1);
            }
        } else if ft.is_file() && supported(&path) {
            out.push(path);
        }
    }
}

#[async_trait]
impl Tool for Index {
    fn name(&self) -> String {
        "Index".into()
    }
    fn description(&self) -> String {
        "A structural skeleton of a file or a small directory: imports, type \
         definitions, function signatures and the line range of each. Use it for the \
         structure of a file (or a handful) before reading bodies, then \
         `Read(path, offset, limit)` the ranges you need. Do NOT index a whole large \
         repo to find one symbol — that costs more than `Grep`. Parses Rust, \
         Python, JavaScript, TypeScript and Go with tree-sitter; other languages get a \
         regex outline. Output is capped by a token budget."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File or directory to index (default: the project root)"},
                "max_files": {"type": "integer", "description": "Cap on files when indexing a directory (default 15)"},
                "tokens": {"type": "integer", "description": "Output budget in estimated tokens (default 1500)"}
            }
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }
    fn rule_subjects(&self, args: &Value) -> Vec<String> {
        vec![
            args["path"]
                .as_str()
                .filter(|p| !p.is_empty())
                .unwrap_or(".")
                .to_string(),
        ]
    }
    fn label(&self, args: &Value) -> String {
        format!(
            "Index({})",
            args["path"]
                .as_str()
                .filter(|p| !p.is_empty())
                .unwrap_or(".")
        )
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let cwd = ctx.cwd();
        let target = args["path"]
            .as_str()
            .filter(|p| !p.is_empty())
            .map(|p| resolve(&cwd, p))
            .unwrap_or_else(|| cwd.clone());
        let max_files = arg_u64(&args, "max_files")
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_FILES);
        let budget = arg_u64(&args, "tokens")
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_TOKENS);

        let mut files = Vec::new();
        if target.is_file() {
            files.push(target.clone());
        } else {
            collect(&target, &mut files, max_files, 0);
        }
        if files.is_empty() {
            return Ok(ToolOutput::ok("No supported source files found."));
        }

        let mut out = Vec::new();
        let mut skipped = 0usize;
        let mut used = 0usize;
        let mut truncated = false;
        for path in &files {
            let lang = language_for(path);
            let too_big = std::fs::metadata(path)
                .map(|m| m.len() > MAX_FILE_BYTES)
                .unwrap_or(false);
            if too_big {
                skipped += 1;
                continue;
            }
            let Ok(src) = std::fs::read(path) else {
                skipped += 1;
                continue;
            };
            let name = rel(&cwd, path);
            let parsed = match lang {
                Some(l) => skeleton(&name, &src, l),
                None => Ok(regex_skeleton(&name, &src)),
            };
            match parsed {
                Ok(s) if !s.trim().is_empty() => {
                    let cost = s.len() / 4 + 1;
                    if !out.is_empty() && used + cost > budget {
                        truncated = true;
                        break;
                    }
                    used += cost;
                    out.push(s);
                }
                _ => skipped += 1,
            }
        }
        if out.is_empty() {
            return Ok(ToolOutput::ok("Could not parse any of the selected files."));
        }
        let mut body = out.join("\n\n");
        if truncated {
            body.push_str(&format!(
                "\n\n(output stopped at the {budget}-token budget; narrow `path` or raise `tokens`)"
            ));
        }
        if skipped > 0 {
            body.push_str(&format!(
                "\n\n({skipped} files skipped: too large or unparsable)"
            ));
        }
        Ok(ToolOutput::ok(body).display(format!(
            "Indexed {}",
            crate::util::plural(files.len(), "file")
        )))
    }
}
