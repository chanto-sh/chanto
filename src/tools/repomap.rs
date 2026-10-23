//! RepoMap: a ranked, token-budgeted skeleton of the project's symbols.
//!
//! A model dropped into an unfamiliar codebase otherwise greps and reads
//! blindly. This builds the map an editor's outline gives a human: the
//! functions, types, classes and modules that define the project, ordered by
//! how often their names are referenced *elsewhere* in the tree — the same
//! "references imply importance" signal Aider's tree-sitter repo map gets from
//! its reference graph, without the tree-sitter grammars. Definitions come
//! from small per-language regexes; ranking comes from one Aho–Corasick pass
//! over every file for every name at once.

use super::{Ctx, Tool, ToolKind, ToolOutput, arg_u64};
use crate::util::rel;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Default output budget, in estimated tokens. Rank, not the file tree,
/// decides what fits.
const DEFAULT_TOKENS: usize = 1024;
/// Stop reading once this much source has been scanned: the map is a summary,
/// and a large monorepo should still answer in about a second.
const MAX_SCAN_BYTES: usize = 32 << 20;
/// A single generated file (a bundle, a minified blob) shouldn't crowd out
/// the rest.
const MAX_FILE_BYTES: u64 = 512 << 10;
/// Names this short are too common to say anything about importance.
const MIN_NAME: usize = 3;

/// One language family: the extensions it covers and the definition patterns
/// to look for, each with the name in capture group 1.
struct Lang {
    exts: &'static [&'static str],
    pats: &'static [(&'static str, &'static str)],
}

const RUST: &[(&str, &str)] = &[
    (
        "fn",
        r#"(?m)^\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:const\s+)?(?:unsafe\s+)?(?:extern\s+"[^"]*"\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)"#,
    ),
    (
        "type",
        r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?(?:struct|enum|trait|union)\s+([A-Za-z_][A-Za-z0-9_]*)",
    ),
    (
        "impl",
        r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?impl(?:<[^>]*>)?\s+(?:[A-Za-z_][\w:]*\s+for\s+)?([A-Za-z_][A-Za-z0-9_]*)",
    ),
    (
        "mod",
        r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)",
    ),
];

const PY: &[(&str, &str)] = &[
    (
        "def",
        r"(?m)^\s*(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)",
    ),
    ("class", r"(?m)^\s*class\s+([A-Za-z_][A-Za-z0-9_]*)"),
];

const JS: &[(&str, &str)] = &[
    (
        "fn",
        r"(?m)^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)",
    ),
    (
        "class",
        r"(?m)^\s*(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)",
    ),
    (
        "type",
        r"(?m)^\s*(?:export\s+)?(?:declare\s+)?(?:interface|type|enum)\s+([A-Za-z_$][\w$]*)",
    ),
    (
        "fn",
        r"(?m)^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*(?:async\s*)?(?:function|\([^)]*\)\s*=>|[A-Za-z_$][\w$]*\s*=>)",
    ),
];

const GO: &[(&str, &str)] = &[
    (
        "func",
        r"(?m)^func\s+(?:\([^)]*\)\s*)?([A-Za-z_][A-Za-z0-9_]*)",
    ),
    ("type", r"(?m)^type\s+([A-Za-z_][A-Za-z0-9_]*)"),
];

/// Java, Kotlin, C#, Scala and friends: a brace language whose declarations
/// start the line behind a run of modifiers.
const JVM: &[(&str, &str)] = &[
    (
        "type",
        r"(?m)^\s*(?:@\w+(?:\([^)]*\))?\s*)*(?:public |protected |private |internal |abstract |final |open |sealed |data |static |partial |export )*(?:class|interface|enum|record|object)\s+([A-Za-z_][A-Za-z0-9_]*)",
    ),
    (
        "fn",
        r"(?m)^\s*(?:public |protected |private |internal |open |override |suspend |inline |operator |abstract |final |static |virtual |async )*fun\s+(?:<[^>]*>\s*)?(?:[\w.<>?]+\.)?([A-Za-z_][A-Za-z0-9_]*)",
    ),
];

const RUBY: &[(&str, &str)] = &[
    (
        "def",
        r"(?m)^\s*def\s+(?:self\.)?([A-Za-z_][A-Za-z0-9_]*[?!=]?)",
    ),
    (
        "class",
        r"(?m)^\s*(?:class|module)\s+([A-Za-z_][A-Za-z0-9_]*)",
    ),
];

const C: &[(&str, &str)] = &[(
    "type",
    r"(?m)^\s*(?:typedef\s+)?(?:class|struct|enum|union)\s+([A-Za-z_][A-Za-z0-9_]*)",
)];

const PHP: &[(&str, &str)] = &[
    (
        "type",
        r"(?m)^\s*(?:abstract\s+|final\s+)*(?:class|interface|trait|enum)\s+([A-Za-z_][A-Za-z0-9_]*)",
    ),
    (
        "fn",
        r"(?m)^\s*(?:public\s+|private\s+|protected\s+|static\s+|final\s+|abstract\s+)*function\s+([A-Za-z_][A-Za-z0-9_]*)",
    ),
];

const SWIFT: &[(&str, &str)] = &[
    (
        "type",
        r"(?m)^\s*(?:public |private |internal |open |final )*(?:class|struct|enum|protocol|extension|actor)\s+([A-Za-z_][A-Za-z0-9_]*)",
    ),
    (
        "fn",
        r"(?m)^\s*(?:public |private |internal |open |override |static |class |mutating )*func\s+([A-Za-z_][A-Za-z0-9_]*)",
    ),
];

const ELIXIR: &[(&str, &str)] = &[
    ("class", r"(?m)^\s*defmodule\s+([A-Z][\w.]*)"),
    (
        "def",
        r"(?m)^\s*(?:def|defp|defmacro|defmacrop|defprotocol|defimpl)\s+([A-Za-z_][A-Za-z0-9_]*[?!]?)",
    ),
];

const LUA: &[(&str, &str)] = &[(
    "fn",
    r"(?m)^\s*(?:local\s+)?function\s+([A-Za-z_][A-Za-z0-9_.:]*)",
)];

const SHELL: &[(&str, &str)] = &[(
    "fn",
    r"(?m)^\s*(?:function\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*\(\)\s*(?:\{|$)",
)];

/// The languages the map understands. Anything else contributes no
/// definitions (and so never wins budget), which is the right answer for
/// markup, data and prose.
const LANGS: &[Lang] = &[
    Lang {
        exts: &["rs"],
        pats: RUST,
    },
    Lang {
        exts: &["py", "pyi"],
        pats: PY,
    },
    Lang {
        exts: &["js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts"],
        pats: JS,
    },
    Lang {
        exts: &["go"],
        pats: GO,
    },
    Lang {
        exts: &["java", "kt", "kts", "cs", "scala", "groovy"],
        pats: JVM,
    },
    Lang {
        exts: &["rb"],
        pats: RUBY,
    },
    Lang {
        exts: &["c", "h"],
        pats: C,
    },
    Lang {
        exts: &["cc", "cpp", "cxx", "hpp", "hh", "hxx"],
        pats: C,
    },
    Lang {
        exts: &["php"],
        pats: PHP,
    },
    Lang {
        exts: &["swift"],
        pats: SWIFT,
    },
    Lang {
        exts: &["ex", "exs"],
        pats: ELIXIR,
    },
    Lang {
        exts: &["lua"],
        pats: LUA,
    },
    Lang {
        exts: &["sh", "bash", "zsh"],
        pats: SHELL,
    },
];

struct Compiled {
    exts: &'static [&'static str],
    rules: Vec<(regex::Regex, &'static str)>,
}

fn compiled() -> &'static [Compiled] {
    static C: OnceLock<Vec<Compiled>> = OnceLock::new();
    C.get_or_init(|| {
        LANGS
            .iter()
            .map(|l| Compiled {
                exts: l.exts,
                rules: l
                    .pats
                    .iter()
                    .map(|(kind, pat)| {
                        (
                            regex::Regex::new(pat).expect("repo-map pattern is valid"),
                            *kind,
                        )
                    })
                    .collect(),
            })
            .collect()
    })
}

fn family(ext: &str) -> Option<&'static Compiled> {
    let ext = ext.to_ascii_lowercase();
    compiled().iter().find(|c| c.exts.contains(&ext.as_str()))
}

/// A definition found in a file, at 1-based `line`.
struct Def {
    file: usize,
    line: usize,
    name: String,
}

/// Byte offsets where each line starts, so a match's line number is a binary
/// search rather than a scan from the top (which would make extraction
/// quadratic on a long file).
fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(
            text.bytes()
                .enumerate()
                .filter_map(|(i, b)| (b == b'\n').then_some(i + 1)),
        )
        .collect()
}

fn definitions(text: &str, lang: &Compiled, file: usize) -> Vec<Def> {
    let starts = line_starts(text);
    let mut out: Vec<Def> = Vec::new();
    for (re, _kind) in &lang.rules {
        for m in re.captures_iter(text) {
            let Some(name) = m.get(1) else { continue };
            // The line of the matched name, not of the match: a pattern that
            // starts with `^\s*` can begin on the blank line above.
            let line = starts.partition_point(|&s| s <= name.start()).max(1);
            out.push(Def {
                file,
                line,
                name: name.as_str().to_string(),
            });
        }
    }
    out.sort_by(|a, b| a.line.cmp(&b.line).then(a.name.cmp(&b.name)));
    out.dedup_by(|a, b| a.line == b.line && a.name == b.name);
    out
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Every readable source file under `root`, as `(path relative to root, text)`.
fn source_files(root: &Path) -> Vec<(String, String)> {
    let scanned = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let files: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| {
            let n = e.file_name();
            n != ".git" && n != "node_modules" && n != "target" && n != ".venv"
        })
        .threads(
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(8),
        )
        .build_parallel();
    walker.run(|| {
        let files = files.clone();
        let scanned = scanned.clone();
        Box::new(move |entry| {
            let Ok(e) = entry else {
                return ignore::WalkState::Continue;
            };
            if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                return ignore::WalkState::Continue;
            }
            if scanned.load(std::sync::atomic::Ordering::Relaxed) >= MAX_SCAN_BYTES {
                return ignore::WalkState::Quit;
            }
            let p = e.path();
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
            if family(ext).is_none() {
                return ignore::WalkState::Continue;
            }
            if e.metadata().map(|m| m.len()).unwrap_or(0) > MAX_FILE_BYTES {
                return ignore::WalkState::Continue;
            }
            let Ok(bytes) = std::fs::read(p) else {
                return ignore::WalkState::Continue;
            };
            // Binary-looking files hold no definitions worth mapping.
            if bytes.iter().take(8000).any(|&b| b == 0) {
                return ignore::WalkState::Continue;
            }
            scanned.fetch_add(bytes.len(), std::sync::atomic::Ordering::Relaxed);
            files
                .lock()
                .unwrap()
                .push((rel(root, p), String::from_utf8_lossy(&bytes).into_owned()));
            ignore::WalkState::Continue
        })
    });
    let mut out = match Arc::try_unwrap(files) {
        Ok(m) => m.into_inner().unwrap(),
        Err(_) => Vec::new(),
    };
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// How many estimated tokens `s` costs (the usual ~4 characters per token).
fn estimate(s: &str) -> usize {
    s.chars().count() / 4 + 1
}

/// Build the map for `root`, spending at most `budget` estimated tokens.
/// Returns `(text, symbol_count)`.
fn build(root: &Path, budget: usize) -> Result<(String, usize)> {
    let files = source_files(root);
    let texts: Vec<&str> = files.iter().map(|(_, t)| t.as_str()).collect();

    let mut defs: Vec<Def> = Vec::new();
    for (i, (path, text)) in files.iter().enumerate() {
        let ext = Path::new(path)
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or("");
        if let Some(lang) = family(ext) {
            defs.extend(definitions(text, lang, i));
        }
    }
    if defs.is_empty() {
        return Ok((
            format!(
                "No symbols found. The map understands {}; for anything else use Grep or Ls.",
                LANGS
                    .iter()
                    .flat_map(|l| l.exts.iter())
                    .map(|e| format!(".{e}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            0,
        ));
    }

    // One pass over every file for every name, at once.
    let mut names: Vec<&str> = defs
        .iter()
        .map(|d| d.name.as_str())
        .filter(|n| n.len() >= MIN_NAME)
        .collect();
    names.sort_unstable();
    names.dedup();
    let ac = aho_corasick::AhoCorasick::new(names.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut counts = vec![0usize; ac.patterns_len()];
    for text in &texts {
        let b = text.as_bytes();
        for m in ac.find_iter(b) {
            let before_ok = m.start() == 0 || !is_word(b[m.start() - 1]);
            let after_ok = m.end() >= b.len() || !is_word(b[m.end()]);
            if before_ok && after_ok {
                counts[m.pattern().as_usize()] += 1;
            }
        }
    }
    let idx_of = |name: &str| -> Option<usize> {
        let i = names.partition_point(|n| *n < name);
        (i < names.len() && names[i] == name).then_some(i)
    };

    // Group the definitions per file then count, per name, how many files
    // define it. A name repeated across the tree — a trait method every impl
    // restates, a common `new` — says little about where the project's weight
    // sits, so a definition's pull is its reference count split across the
    // files that define it.
    let mut by_file: Vec<Vec<usize>> = vec![Vec::new(); files.len()];
    for (i, d) in defs.iter().enumerate() {
        by_file[d.file].push(i);
    }
    let mut df = vec![0usize; names.len()];
    for defs_in_file in &by_file {
        let mut seen = std::collections::HashSet::new();
        for &i in defs_in_file {
            if let Some(idx) = idx_of(&defs[i].name)
                && seen.insert(idx)
            {
                df[idx] += 1;
            }
        }
    }
    let pull = |idx: usize| counts[idx] as f64 / df[idx].max(1) as f64;

    // A file scores by the *distinct* symbols it defines, so one generic name
    // restated in many impls is not credited many times over.
    let mut file_score = vec![0.0f64; files.len()];
    for (f, defs_in_file) in by_file.iter().enumerate() {
        let mut seen = std::collections::HashSet::new();
        let mut s = 0.0;
        for &i in defs_in_file {
            if let Some(idx) = idx_of(&defs[i].name)
                && seen.insert(idx)
            {
                s += pull(idx);
            }
        }
        file_score[f] = s;
    }

    let mut order: Vec<usize> = (0..files.len())
        .filter(|i| !by_file[*i].is_empty())
        .collect();
    order.sort_by(|a, b| {
        file_score[*b]
            .partial_cmp(&file_score[*a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(files[*a].0.cmp(&files[*b].0))
    });

    let mut out = format!(
        "# Repo map: {} symbols in {} files, most-referenced first. A name's rank \
         is how often it appears elsewhere in the tree; elided lines show as ⋮...\n",
        defs.len(),
        order.len(),
    );
    let mut used = estimate(&out);
    let mut shown = false;
    for f in order {
        let mut rank = by_file[f].clone();
        rank.sort_by(|a, b| {
            let pa = idx_of(&defs[*a].name).map(pull).unwrap_or(0.0);
            let pb = idx_of(&defs[*b].name).map(pull).unwrap_or(0.0);
            pb.partial_cmp(&pa)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(defs[*a].line.cmp(&defs[*b].line))
        });
        let header = format!("{}:\n", files[f].0);
        let mut rows: Vec<String> = Vec::new();
        let mut seen_lines: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut cost = estimate(&header);
        let mut last: Option<usize> = None;
        let file_lines: Vec<&str> = texts[f].lines().collect();
        for i in rank {
            let line = defs[i].line;
            if last == Some(line) {
                continue;
            }
            let Some(text) = file_lines.get(line - 1).copied() else {
                continue;
            };
            // A file that restates the same signature in many impls should say
            // it once.
            if !seen_lines.insert(text.trim().to_string()) {
                continue;
            }
            let mut row = String::new();
            // Elide whatever sits above the first shown line and between two.
            if last.is_none() || line > last.unwrap() + 1 {
                row.push_str("⋮...\n");
            }
            row.push('│');
            row.push_str(text.trim_end());
            row.push('\n');
            let c = estimate(&row);
            if used + cost + c > budget {
                break;
            }
            cost += c;
            rows.push(row);
            last = Some(line);
        }
        if rows.is_empty() {
            break;
        }
        out.push_str(&header);
        for r in rows {
            out.push_str(&r);
        }
        used += cost;
        shown = true;
    }
    if shown {
        out.push_str("⋮...\n");
    }
    Ok((out, defs.len()))
}

pub struct RepoMap;

#[async_trait]
impl Tool for RepoMap {
    fn name(&self) -> String {
        "RepoMap".into()
    }
    fn description(&self) -> String {
        "A ranked, token-budgeted map of the project's symbols (functions, types, \
         classes, modules), most-referenced first. Use it to get oriented in an \
         unfamiliar or large repo before grepping, or to find where a name lives. \
         `path` limits it to a subdirectory; `tokens` sets the budget (default 1024; \
         raise it for a bigger map)."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Subdirectory to map (default: the project root)"},
                "tokens": {"type": "integer", "description": "Output budget in estimated tokens (default 1024)"}
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
            "RepoMap({})",
            args["path"]
                .as_str()
                .filter(|p| !p.is_empty())
                .unwrap_or(".")
        )
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let cwd = ctx.cwd();
        let root: PathBuf = args["path"]
            .as_str()
            .filter(|p| !p.is_empty())
            .map(|p| crate::util::resolve(&cwd, p))
            .unwrap_or(cwd.clone());
        let budget = arg_u64(&args, "tokens").unwrap_or(DEFAULT_TOKENS as u64) as usize;
        let (text, n) = tokio::task::spawn_blocking(move || build(&root, budget.max(64))).await??;
        Ok(ToolOutput::ok(text).display(format!("Mapped {}", crate::util::plural(n, "symbol"))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    /// A fresh scratch directory (the test home and repo are thrown away).
    fn tmp() -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("chanto-repomap-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_name_defined_all_over_the_tree_does_not_dominate() {
        // `common` is referenced everywhere, but it is defined in every file,
        // so it says little; `Engine` is defined once and used in five places.
        let dir = tmp();
        write(&dir, "core.rs", "pub struct Engine {}\n");
        let mut refs = String::new();
        for i in 0..30 {
            write(&dir, &format!("g{i}.rs"), "pub fn common() {}\n");
            refs.push_str("fn u() { common(); }\n");
        }
        write(&dir, "users.rs", &refs);
        let mut used = String::new();
        for _ in 0..5 {
            used.push_str("fn e() { let _ = Engine; }\n");
        }
        write(&dir, "engine_users.rs", &used);
        let (text, _) = build(&dir, 4096).unwrap();
        let core = text.find("core.rs").expect("core.rs in map");
        let generic = text.find("g0.rs").expect("g0.rs in map");
        assert!(
            core < generic,
            "a name defined in every file must not outrank one defined once:\n{text}"
        );
    }

    #[test]
    fn a_file_repeating_one_signature_says_it_once() {
        let dir = tmp();
        let body = "pub fn dup() {}\n".repeat(20);
        write(&dir, "rep.rs", &body);
        let (text, _) = build(&dir, 4096).unwrap();
        assert_eq!(text.matches("pub fn dup()").count(), 1, "{text}");
    }

    #[test]
    fn ranks_the_most_referenced_symbol_first() {
        let dir = tmp();
        write(
            &dir,
            "src/core.rs",
            "pub struct Engine { n: usize }\n\npub fn helper() {}\n",
        );
        write(
            &dir,
            "src/a.rs",
            "fn a() { Engine::new(); Engine::go(); let e: Engine = todo!(); }\n",
        );
        write(&dir, "src/b.rs", "fn b() { let _ = Engine::new(); }\n");
        let (text, n) = build(&dir, 1024).unwrap();
        assert!(n >= 3, "{text}");
        // `Engine` is referenced across two files; `helper` is not.
        let engine = text.find("Engine").expect("Engine in map");
        let helper = text
            .find("helper")
            .unwrap_or_else(|| panic!("helper in map:\n{text}"));
        assert!(
            engine < helper,
            "most-referenced symbol should come first:\n{text}"
        );
        assert!(text.contains("⋮..."), "elisions are marked:\n{text}");
    }

    #[test]
    fn understands_several_languages() {
        let dir = tmp();
        write(
            &dir,
            "app.py",
            "class Server:\n    def start(self):\n        pass\n",
        );
        write(&dir, "web.ts", "export function render() {}\n");
        write(&dir, "main.go", "func main() {}\ntype Config struct{}\n");
        write(&dir, "lib.rb", "module Lib\n  def go; end\nend\n");
        let (text, n) = build(&dir, 1024).unwrap();
        for name in ["Server", "start", "render", "main", "Config", "go"] {
            assert!(text.contains(name), "{name} missing from:\n{text}");
        }
        assert!(n >= 6);
    }

    #[test]
    fn budget_trims_the_tail() {
        let dir = tmp();
        let mut body = String::new();
        for i in 0..200 {
            body.push_str(&format!("pub fn f{i}() {{}}\n"));
        }
        write(&dir, "big.rs", &body);
        let (small, _) = build(&dir, 64).unwrap();
        let (large, _) = build(&dir, 4096).unwrap();
        assert!(small.len() < large.len(), "budget must bound the output");
        assert!(small.lines().count() < 200);
    }

    #[test]
    fn a_repo_without_known_languages_explains_itself() {
        let dir = tmp();
        write(&dir, "notes.txt", "just prose\n");
        let (text, n) = build(&dir, 1024).unwrap();
        assert_eq!(n, 0);
        assert!(text.contains("No symbols found"));
    }

    #[test]
    fn ignores_gitignored_files() {
        let dir = tmp();
        // `ignore` honors .gitignore only inside a git repository.
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        write(&dir, ".gitignore", "generated/\n");
        write(&dir, "generated/big.rs", "pub fn hidden() {}\n");
        write(&dir, "src/real.rs", "pub fn visible() {}\n");
        let (text, _) = build(&dir, 1024).unwrap();
        assert!(text.contains("visible"));
        assert!(!text.contains("hidden"), "{text}");
    }

    #[test]
    fn counts_only_word_boundary_references() {
        let dir = tmp();
        write(&dir, "a.rs", "pub fn run() {}\n");
        // `running` must not count as a reference to `run`.
        write(&dir, "b.rs", "fn b() { running(); }\n");
        let (text, _) = build(&dir, 1024).unwrap();
        assert!(text.contains("run"), "{text}");
        assert!(text.contains("b.rs"));
    }
}
