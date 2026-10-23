//! `chanto migrate`: translate another coding agent's configuration into
//! Chanto's own, using the configured model. The result is shown for review;
//! nothing is written unless you pass `--apply` (which backs up the old config
//! and merges the new settings into `~/.chanto/config.toml`).

use anyhow::{Context, Result, bail};

use crate::llm::{ChatRequest, Message};

/// Default config locations per known tool, tried in order. `~` is the home dir.
/// This is only a convenience; `--path <file>` works for any agent.
const KNOWN: &[(&str, &[&str])] = &[
    (
        "opencode",
        &[
            "opencode.jsonc",
            "opencode.json",
            "~/.config/opencode/opencode.json",
            "~/.config/opencode/config.json",
        ],
    ),
    (
        "continue",
        &["~/.continue/config.json", ".continue/config.json"],
    ),
    ("cline", &[".clinerules", ".cline/config.json"]),
    ("cursor", &[".cursorrules", ".cursor/rules"]),
    ("windsurf", &[".windsurfrules"]),
    ("aider", &[".aider.conf.yml", "~/.aider.conf.yml"]),
];

/// The Chanto config surface the model should target. Kept short on purpose:
/// only the keys that other agents tend to have equivalents for.
const REFERENCE: &str = "chanto config.toml keys:
  model = \"provider/model\"           # default model, e.g. anthropic/claude-opus-5-5
  provider = \"ollama|openai|anthropic|openai-responses|bedrock|vertex|azure|foundry\"
  base_url = \"http://host:port/...\"   # custom endpoint for `provider`
  mode = \"default|acceptEdits|plan|auto|bypassPermissions\"
  subagent_model, classifier_model, oracle_model = \"provider/model\"
  temperature (float), num_ctx (int)
  [providers.<name>]                   # a named provider: kind=, base_url=, model=
  [mcp_servers.<name>]                 # command=, args=[], env={}  — or url= for a remote server; disabled=true to skip
  [permissions] allow = [\"Bash(cmd:*)\"], deny = [...]
Repo instructions and rules belong in AGENTS.md or CLAUDE.md, not config.toml.";

fn expand(p: &str) -> std::path::PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    std::path::PathBuf::from(p)
}

/// Pull the first fenced code block out of the model's reply (```toml preferred).
fn extract_toml(s: &str) -> Option<String> {
    for fence in ["```toml", "```"] {
        if let Some(start) = s.find(fence) {
            let after = &s[start + fence.len()..];
            // The rest of the opening line is the block's language tag, not config.
            let after = match after.split_once('\n') {
                Some((tag, rest))
                    if tag
                        .trim()
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')) =>
                {
                    rest
                }
                _ => after,
            };
            if let Some(end) = after.find("```") {
                let body = after[..end].trim_matches(|c: char| c == '\n' || c == '\r');
                if !body.trim().is_empty() {
                    return Some(body.to_string());
                }
            }
        }
    }
    None
}

fn notes(out: &str) -> String {
    // Everything after the first heading is the model's human-facing summary.
    match out
        .find("## What mapped")
        .or_else(|| out.find("## Skipped"))
    {
        Some(i) => out[i..].trim().to_string(),
        None => String::new(),
    }
}

pub async fn cli(args: &[String]) -> Result<String> {
    #[derive(clap::Parser)]
    struct Args {
        /// Agent to migrate from (opencode, continue, cline, cursor, windsurf, aider)
        tool: Option<String>,
        /// Config file to read (overrides the tool's default locations)
        #[arg(long)]
        path: Option<std::path::PathBuf>,
        /// Merge the result into ~/.chanto/config.toml (default: just show it)
        #[arg(long)]
        apply: bool,
    }
    let a = <Args as clap::Parser>::try_parse_from(
        std::iter::once("chanto migrate".to_string()).chain(args.iter().cloned()),
    )?;

    // Locate and read the source config.
    let (label, path) = if let Some(p) = a.path.clone() {
        (a.tool.clone().unwrap_or_else(|| "another agent".into()), p)
    } else if let Some(tool) = &a.tool {
        let locs = KNOWN
            .iter()
            .find(|(t, _)| t.eq_ignore_ascii_case(tool))
            .map(|(_, l)| *l)
            .unwrap_or(&[]);
        match locs.iter().map(|p| expand(p)).find(|p| p.exists()) {
            Some(p) => (tool.clone(), p),
            None => bail!(
                "Couldn't find {tool}'s config in the usual places. Pass the file with `--path <file>`."
            ),
        }
    } else {
        bail!(
            "Usage: chanto migrate <tool> [--path <file>] [--apply]\n  tools: opencode, continue, cline, cursor, windsurf, aider (or any, with --path)"
        );
    };
    let source =
        std::fs::read_to_string(&path).with_context(|| format!("Reading {}", path.display()))?;
    if source.trim().is_empty() {
        bail!("{} is empty", path.display());
    }

    // Resolve the model that will do the translation.
    let cwd = std::env::current_dir()?;
    let (cfg, _) = crate::config::Config::load(&cwd, crate::config::is_trusted(&cwd))?;
    let Some(model_spec) = cfg.model.clone() else {
        bail!("No model is set up. Run `chanto setup` first, then `chanto migrate`.");
    };
    let (provider, resolved, _name) = crate::doctor::resolve(&cfg, &model_spec);

    eprintln!(
        "Reading {} and translating with {resolved}…",
        path.display()
    );
    let req = ChatRequest {
        model: resolved,
        messages: vec![
            Message::system(format!(
                "You convert another coding agent's configuration into a chanto config.toml.\n\n{REFERENCE}\n\n\
                 Output ONLY a fenced ```toml block with the chanto config, including only settings that map \
                 cleanly (model/provider, custom endpoints, MCP servers, permission rules). Do not invent values \
                 or guess keys; omit anything uncertain. If nothing maps to config.toml, output no code block. \
                 After the block, add a short '## What mapped' bulleted list and a '## Skipped' list for anything \
                 that didn't translate, and where it should go instead (e.g. AGENTS.md for instructions)."
            )),
            Message::user(format!(
                "This is a {label} configuration ({}):\n\n{source}",
                path.display()
            )),
        ],
        tools: vec![],
        think: Some(false),
        effort: None,
        temperature: cfg.temperature,
        max_tokens: Some(4096),
        json: false,
    };
    let out = provider
        .chat(&req, &mut |_| {})
        .await
        .context("The model call failed")?
        .text;

    let summary = notes(&out);
    let Some(toml) = extract_toml(&out) else {
        return Ok(format!(
            "Nothing in your {label} setup maps to a chanto config.toml.\n\n{}",
            if summary.is_empty() {
                out.trim()
            } else {
                &summary
            }
        ));
    };

    // Validate before showing or writing.
    toml.parse::<toml_edit::DocumentMut>()
        .context("The generated config isn't valid TOML")?;
    let valid = toml::from_str::<crate::config::Config>(&toml);

    if !a.apply {
        let warn = match &valid {
            Ok(_) => String::new(),
            Err(e) => format!(
                "\n\nNote: this didn't fully validate as a chanto config ({e}); review before applying."
            ),
        };
        return Ok(format!(
            "Proposed chanto config from your {label} setup:\n\n```toml\n{toml}\n```\n\n{summary}{warn}\n\n\
             Review it, then re-run with `--apply` to merge it into ~/.chanto/config.toml."
        ));
    }

    if let Err(e) = valid {
        bail!(
            "Won't apply: the generated config didn't validate as a chanto config: {e}\n\n{toml}"
        );
    }

    // Merge into config.toml, backing up the old one.
    let dest = crate::config::home_dir().join("config.toml");
    std::fs::create_dir_all(dest.parent().unwrap()).ok();
    let existing = std::fs::read_to_string(&dest).unwrap_or_default();
    if !existing.trim().is_empty() {
        std::fs::write(dest.with_extension("toml.bak"), &existing).ok();
    }
    let mut doc = existing
        .parse::<toml_edit::DocumentMut>()
        .unwrap_or_default();
    let add = toml
        .parse::<toml_edit::DocumentMut>()
        .expect("validated above");
    for (k, v) in add.iter() {
        doc.insert(k, v.clone());
    }
    std::fs::write(&dest, doc.to_string())
        .with_context(|| format!("Writing {}", dest.display()))?;
    Ok(format!(
        "✓ Merged your {label} settings into {}{}.\n\n{summary}",
        dest.display(),
        if existing.trim().is_empty() {
            String::new()
        } else {
            " (previous config saved as config.toml.bak)".into()
        }
    ))
}

#[cfg(test)]
mod tests;
