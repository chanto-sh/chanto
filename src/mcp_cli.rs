//! `chanto mcp add|list|get|remove`: manage MCP servers from the shell, like
//! `claude mcp` and `codex mcp`. Edits config files as text so comments and
//! layout survive.

use crate::config::{Config, home_dir};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

const USAGE: &str = "\
Usage:
  chanto mcp add [options] <name> -- <command> [args...]   stdio server
  chanto mcp add [options] --transport http <name> <url>   HTTP server (sse: legacy HTTP+SSE server)
  chanto mcp add-json [-s <scope>] [--client-secret] <name> '<json>'   from a JSON entry (Claude Code's format)
  chanto mcp add-from-claude-desktop [-s <scope>] [--all | <names>...]   import Claude Desktop's servers
  chanto mcp list
  chanto mcp get <name>
  chanto mcp remove [--scope <scope>] <name>

Options:
  -s, --scope <local|project|user>  where to save it (default: local)
                                    local:   ./.chanto/config.local.toml (just you, this project)
                                    project: ./.chanto/config.toml (shared via git)
                                    user:    ~/.chanto/config.toml (all projects)
  -t, --transport <stdio|http|sse>  default: stdio, or http when given a URL
  -e, --env KEY=VALUE               environment variable for a stdio server (repeatable)
  -H, --header 'Name: value'        HTTP header (repeatable)";

fn scope_path(scope: &str, cwd: &Path) -> Result<PathBuf> {
    Ok(match scope {
        "local" => cwd.join(".chanto").join("config.local.toml"),
        "project" => cwd.join(".chanto").join("config.toml"),
        "user" => home_dir().join("config.toml"),
        s => bail!("unknown scope {s:?} (local, project or user)"),
    })
}

/// Write a config file. The user-scope config (`~/.chanto/config.toml`) can hold
/// `env` values, `Authorization` headers and OAuth client secrets, so keep it
/// owner-only; project and local files are shared via git and stay normal.
fn write_config(path: &Path, text: &str) -> std::io::Result<()> {
    if path.starts_with(home_dir()) {
        crate::util::write_private(path, text.as_bytes())
    } else {
        std::fs::write(path, text)
    }
}

fn quote(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn key(name: &str) -> String {
    if !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        name.to_string()
    } else {
        quote(name)
    }
}

/// Header lines that open this server's tables: `[mcp_servers.<name>]` and its subtables.
fn is_server_header(line: &str, name: &str) -> bool {
    let t = line.trim();
    let Some(inner) = t.strip_prefix('[').and_then(|r| r.split(']').next()) else {
        return false;
    };
    let inner = inner.trim_start_matches('[').trim();
    [
        format!("mcp_servers.{}", key(name)),
        format!("mcp_servers.{}", quote(name)),
    ]
    .iter()
    .any(|p| {
        inner == p
            || inner
                .strip_prefix(p.as_str())
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

fn servers_in(text: &str) -> Result<toml::Table> {
    let t: toml::Table = toml::from_str(text)?;
    Ok(t.get("mcp_servers")
        .or_else(|| t.get("mcpServers"))
        .and_then(|v| v.as_table())
        .cloned()
        .unwrap_or_default())
}

/// Text appended to a config file for a new server.
fn render(
    name: &str,
    transport: &str,
    target: &[String],
    env: &[(String, String)],
    headers: &[(String, String)],
) -> String {
    let mut out = format!("\n[mcp_servers.{}]\n", key(name));
    if transport != "stdio" {
        out += &format!("url = {}\n", quote(&target[0]));
        if transport == "sse" {
            out += "transport = \"sse\"\n";
        }
    } else {
        out += &format!("command = {}\n", quote(&target[0]));
        if target.len() > 1 {
            out += &format!(
                "args = [{}]\n",
                target[1..]
                    .iter()
                    .map(|a| quote(a))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    for (table, pairs) in [("env", env), ("headers", headers)] {
        if !pairs.is_empty() {
            out += &format!("\n[mcp_servers.{}.{table}]\n", key(name));
            for (k, v) in pairs {
                out += &format!("{} = {}\n", key(k), quote(v));
            }
        }
    }
    out
}

fn add(args: &[String], cwd: &Path) -> Result<String> {
    let (mut scope, mut transport) = ("local".to_string(), None::<String>);
    let (mut env, mut headers, mut rest) = (vec![], vec![], vec![]);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |flag: &str| {
            it.next()
                .cloned()
                .with_context(|| format!("{flag} needs a value"))
        };
        match a.as_str() {
            "--" => {
                rest.extend(it.by_ref().cloned());
                break;
            }
            "-s" | "--scope" => scope = val(a)?,
            "-t" | "--transport" => transport = Some(val(a)?),
            "-e" | "--env" => {
                let v = val(a)?;
                let (k, v) = v
                    .split_once('=')
                    .with_context(|| format!("--env expects KEY=VALUE, got {v:?}"))?;
                env.push((k.to_string(), v.to_string()));
            }
            "-H" | "--header" => {
                let v = val(a)?;
                let (k, v) = v
                    .split_once(':')
                    .with_context(|| format!("--header expects 'Name: value', got {v:?}"))?;
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
            s if s.starts_with('-') && rest.is_empty() => bail!("unknown option {s}\n\n{USAGE}"),
            _ => rest.push(a.clone()),
        }
    }
    let (Some(name), target) = (
        rest.first().cloned(),
        rest.get(1..).unwrap_or_default().to_vec(),
    ) else {
        bail!(USAGE)
    };
    if target.is_empty() {
        bail!("missing the server's command or URL\n\n{USAGE}");
    }
    let looks_http = target[0].starts_with("http://") || target[0].starts_with("https://");
    let transport = transport.unwrap_or_else(|| {
        if looks_http && target.len() == 1 {
            "http".into()
        } else {
            "stdio".into()
        }
    });
    match transport.as_str() {
        "http" | "streamable-http" | "sse" if target.len() == 1 && looks_http => {}
        "http" | "streamable-http" | "sse" => bail!("an HTTP server takes a single http(s) URL"),
        "stdio" => {}
        t => bail!("unknown transport {t:?} (stdio, http or sse)"),
    }
    let transport = match transport.as_str() {
        "stdio" => "stdio",
        "sse" => "sse",
        _ => "http",
    };
    if transport != "stdio" && !env.is_empty() {
        bail!("--env applies to stdio servers; use --header for HTTP servers");
    }
    let path = scope_path(&scope, cwd)?;
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if servers_in(&text)
        .with_context(|| format!("parsing {}", path.display()))?
        .contains_key(&name)
    {
        bail!(
            "{name} already exists in {}; remove it first",
            path.display()
        );
    }
    let block = render(&name, transport, &target, &env, &headers);
    append(&path, &text, &block)?;
    Ok(added_note(&name, transport, &path, &scope, cwd))
}

/// Append a server's tables to a config file, checking the result still parses.
fn append(path: &Path, text: &str, block: &str) -> Result<()> {
    let new = format!(
        "{}{}",
        text,
        if text.is_empty() {
            block.trim_start().to_string()
        } else if text.ends_with('\n') {
            block.to_string()
        } else {
            format!("\n{block}")
        }
    );
    servers_in(&new).with_context(|| {
        format!(
            "the result wouldn't parse; is mcp_servers an inline table in {}?",
            path.display()
        )
    })?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_config(path, &new)?;
    Ok(())
}

/// The `Added ...` line, plus what to know about the scope.
fn added_note(name: &str, transport: &str, path: &Path, scope: &str, cwd: &Path) -> String {
    let mut msg = format!(
        "Added {transport} MCP server {name} to {} ({scope} scope).",
        path.display()
    );
    if scope != "user" && !crate::config::is_trusted_here(cwd) {
        msg += "\nNote: this project isn't trusted yet, so chanto ignores its MCP servers until you trust it.";
    }
    if scope == "local" {
        msg += "\nKeep .chanto/config.local.toml out of git (add it to .gitignore).";
    }
    msg
}

/// Names `chanto mcp` adds: letters, numbers, hyphens and underscores, as in Claude Code.
fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A Claude Code / Claude Desktop JSON server entry as a Chanto server table.
fn json_to_table(v: &serde_json::Value) -> Result<toml::Table> {
    use toml::Value as T;
    let o = v
        .as_object()
        .context("the server configuration must be a JSON object")?;
    let strs = |key: &str| -> Result<Option<toml::Table>> {
        match o.get(key) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::Object(m)) => m
                .iter()
                .map(|(k, v)| {
                    Ok((
                        k.clone(),
                        T::String(
                            v.as_str()
                                .with_context(|| format!("{key}.{k} must be a string"))?
                                .to_string(),
                        ),
                    ))
                })
                .collect::<Result<toml::Table>>()
                .map(Some),
            Some(_) => bail!("{key} must be an object of strings"),
        }
    };
    let kind = o.get("type").and_then(|t| t.as_str());
    let url = o.get("url").and_then(|u| u.as_str());
    let mut t = toml::Table::new();
    match (kind, url) {
        (Some("ws"), _) => bail!("WebSocket (`ws`) MCP servers aren't supported by chanto"),
        (Some("sdk"), _) => {
            bail!("`sdk` servers run inside the app that registers them; they can't be added here")
        }
        (Some(k), _) if !matches!(k, "stdio" | "http" | "streamable-http" | "sse") => {
            bail!("unknown type {k:?} (stdio, http or sse)")
        }
        (Some("stdio"), Some(_)) | (Some("stdio"), None) | (None, None) => {
            let cmd = o
                .get("command")
                .and_then(|c| c.as_str())
                .filter(|c| !c.is_empty())
                .context("a stdio server needs a \"command\"")?;
            t.insert("command".into(), T::String(cmd.into()));
            if let Some(a) = o.get("args") {
                let args: Vec<T> = a
                    .as_array()
                    .context("args must be an array")?
                    .iter()
                    .map(|x| {
                        x.as_str()
                            .map(|s| T::String(s.into()))
                            .context("args must be strings")
                    })
                    .collect::<Result<_>>()?;
                if !args.is_empty() {
                    t.insert("args".into(), T::Array(args));
                }
            }
            if let Some(cwd) = o.get("cwd").and_then(|c| c.as_str()) {
                t.insert("cwd".into(), T::String(cwd.into()));
            }
            if let Some(env) = strs("env")? {
                t.insert("env".into(), T::Table(env));
            }
        }
        (k, u) => {
            let u = u
                .filter(|u| !u.is_empty())
                .context("an HTTP server needs a \"url\"")?;
            // `${VAR}` may supply the scheme; anything else must spell it out.
            let lower = u.to_ascii_lowercase();
            if !(lower.starts_with("http://")
                || lower.starts_with("https://")
                || u.starts_with('$'))
            {
                bail!(
                    "the url {u:?} has no http:// or https:// scheme; write it in full, e.g. \"https://{}\"",
                    u.split("://").last().unwrap_or(u)
                );
            }
            t.insert("url".into(), T::String(u.into()));
            if k == Some("sse") {
                t.insert("transport".into(), T::String("sse".into()));
            }
            if let Some(h) = strs("headers")? {
                t.insert("headers".into(), T::Table(h));
            }
            if o.contains_key("headersHelper") {
                bail!(
                    "headersHelper isn't supported by chanto; put the header in \"headers\" (it may use ${{VAR}})"
                );
            }
        }
    }
    if let Some(oauth) = o.get("oauth").and_then(|v| v.as_object()) {
        let mut a = toml::Table::new();
        if let Some(id) = oauth.get("clientId").and_then(|v| v.as_str()) {
            a.insert("client_id".into(), T::String(id.into()));
        }
        if let Some(p) = oauth.get("callbackPort").and_then(|v| v.as_i64()) {
            a.insert("callback_port".into(), T::Integer(p));
        }
        if let Some(s) = oauth.get("scopes").and_then(|v| v.as_str()) {
            a.insert("scopes".into(), T::String(s.into()));
        }
        if !a.is_empty() {
            t.insert("oauth".into(), T::Table(a));
        }
    }
    Ok(t)
}

/// The tables to append for a server.
fn render_table(name: &str, t: &toml::Table) -> Result<String> {
    let mut servers = toml::Table::new();
    servers.insert(name.to_string(), toml::Value::Table(t.clone()));
    let mut root = toml::Table::new();
    root.insert("mcp_servers".into(), toml::Value::Table(servers));
    Ok(format!("\n{}", toml::to_string(&root)?))
}

fn transport_of(t: &toml::Table) -> &'static str {
    match (
        t.contains_key("url"),
        t.get("transport").and_then(|v| v.as_str()),
    ) {
        (true, Some("sse")) => "sse",
        (true, _) => "http",
        _ => "stdio",
    }
}

/// `chanto mcp add-json [--scope s] [--client-secret] <name> '<json>'`.
fn add_json(args: &[String], cwd: &Path) -> Result<String> {
    let mut scope = "local".to_string();
    let mut secret = false;
    let mut rest = vec![];
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-s" | "--scope" => scope = it.next().cloned().context("--scope needs a value")?,
            "--client-secret" => secret = true,
            s if s.starts_with('-') && rest.len() < 2 && !s.starts_with("-{") => {
                bail!("unknown option {s}\n\n{USAGE}")
            }
            _ => rest.push(a.clone()),
        }
    }
    let [name, json] = rest.as_slice() else {
        bail!("usage: chanto mcp add-json [--scope local|project|user] <name> '<json>'")
    };
    if !valid_name(name) {
        bail!("server names may contain only letters, numbers, hyphens and underscores: {name:?}");
    }
    let v: serde_json::Value = serde_json::from_str(json)
        .context("the configuration isn't valid JSON (quote it for your shell)")?;
    let v = match v.get("mcpServers") {
        Some(_) => bail!("pass the server's own object, not the {{\"mcpServers\": ...}} wrapper"),
        None => v,
    };
    let mut t = json_to_table(&v)?;
    if secret {
        let s = match std::env::var("MCP_CLIENT_SECRET") {
            Ok(s) if !s.is_empty() => s,
            _ => {
                use std::io::{BufRead, IsTerminal, Write};
                if !std::io::stdin().is_terminal() {
                    bail!(
                        "--client-secret: set MCP_CLIENT_SECRET, or run in a terminal to type it"
                    );
                }
                eprint!("OAuth client secret: ");
                std::io::stderr().flush()?;
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line)?;
                line.trim().to_string()
            }
        };
        match t
            .entry("oauth")
            .or_insert_with(|| toml::Value::Table(Default::default()))
        {
            toml::Value::Table(o) => {
                o.insert("client_secret".into(), toml::Value::String(s));
            }
            _ => unreachable!(),
        }
    }
    let path = scope_path(&scope, cwd)?;
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if servers_in(&text)
        .with_context(|| format!("parsing {}", path.display()))?
        .contains_key(name)
    {
        bail!(
            "{name} already exists in {}; remove it first",
            path.display()
        );
    }
    append(&path, &text, &render_table(name, &t)?)?;
    Ok(added_note(name, transport_of(&t), &path, &scope, cwd))
}

/// Where Claude Desktop keeps its MCP servers, most likely first: macOS,
/// Windows, Linux, then a Windows install seen from WSL.
pub fn claude_desktop_paths() -> Vec<PathBuf> {
    let home = crate::compat::home();
    let file = "claude_desktop_config.json";
    let mut v = vec![home.join("Library/Application Support/Claude").join(file)];
    if let Some(appdata) =
        std::env::var_os("APPDATA").filter(|_| std::env::var_os("CHANTO_COMPAT_HOME").is_none())
    {
        v.push(PathBuf::from(appdata).join("Claude").join(file));
    }
    v.push(home.join("AppData/Roaming/Claude").join(file));
    match std::env::var_os("XDG_CONFIG_HOME")
        .filter(|_| std::env::var_os("CHANTO_COMPAT_HOME").is_none())
    {
        Some(x) => v.push(PathBuf::from(x).join("Claude").join(file)),
        None => v.push(home.join(".config/Claude").join(file)),
    }
    if std::env::var_os("CHANTO_COMPAT_HOME").is_none()
        && let Ok(users) = std::fs::read_dir("/mnt/c/Users")
    {
        v.extend(
            users
                .flatten()
                .map(|u| u.path().join("AppData/Roaming/Claude").join(file)),
        );
    }
    v
}

/// The servers in Claude Desktop's config: (path, name → entry).
fn claude_desktop_servers() -> Result<(PathBuf, serde_json::Map<String, serde_json::Value>)> {
    let paths = claude_desktop_paths();
    let Some(p) = paths.iter().find(|p| p.is_file()) else {
        bail!(
            "no Claude Desktop configuration found (looked in {})",
            paths
                .iter()
                .map(|p| crate::util::tilde(p))
                .collect::<Vec<_>>()
                .join(", ")
        );
    };
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p)?)
        .with_context(|| format!("parsing {}", p.display()))?;
    Ok((
        p.clone(),
        v.get("mcpServers")
            .and_then(|m| m.as_object())
            .cloned()
            .unwrap_or_default(),
    ))
}

/// Which of `names` the user picked: `all` / empty → all, `none`, or
/// numbers and names separated by commas or spaces.
fn pick(answer: &str, names: &[String]) -> Result<Vec<String>> {
    let a = answer.trim();
    if a.is_empty() || a.eq_ignore_ascii_case("all") || a == "*" {
        return Ok(names.to_vec());
    }
    if a.eq_ignore_ascii_case("none") || a.eq_ignore_ascii_case("n") {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for tok in a
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
    {
        let name = match tok.parse::<usize>() {
            Ok(i) if i >= 1 && i <= names.len() => names[i - 1].clone(),
            Ok(i) => bail!("no server number {i} (1-{})", names.len()),
            Err(_) if names.iter().any(|n| n == tok) => tok.to_string(),
            Err(_) => bail!("no server named {tok:?}"),
        };
        if !out.contains(&name) {
            out.push(name);
        }
    }
    Ok(out)
}

/// `name`, or `name_1`, `name_2`, ... when taken.
fn unique_name(name: &str, taken: &dyn Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    (1..)
        .map(|i| format!("{name}_{i}"))
        .find(|n| !taken(n))
        .unwrap()
}

/// `chanto mcp add-from-claude-desktop [--scope s] [--all | <names>...]`.
fn add_from_claude_desktop(args: &[String], cwd: &Path) -> Result<String> {
    let mut scope = "local".to_string();
    let mut all = false;
    let mut wanted = vec![];
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-s" | "--scope" => scope = it.next().cloned().context("--scope needs a value")?,
            "-a" | "--all" => all = true,
            s if s.starts_with('-') => bail!("unknown option {s}"),
            _ => wanted.push(a.clone()),
        }
    }
    let path = scope_path(&scope, cwd)?;
    let (src, servers) = claude_desktop_servers()?;
    if servers.is_empty() {
        return Ok(format!(
            "Claude Desktop ({}) has no MCP servers to import.",
            crate::util::tilde(&src)
        ));
    }
    let names: Vec<String> = servers.keys().cloned().collect();
    let chosen = if all {
        names.clone()
    } else if !wanted.is_empty() {
        pick(&wanted.join(","), &names)?
    } else {
        use std::io::{BufRead, IsTerminal, Write};
        if !std::io::stdin().is_terminal() {
            bail!(
                "Claude Desktop has these MCP servers: {}. Pass their names, or --all, to import them.",
                names.join(", ")
            );
        }
        println!(
            "MCP servers in Claude Desktop ({}):",
            crate::util::tilde(&src)
        );
        for (i, n) in names.iter().enumerate() {
            let s = &servers[n];
            let target = s
                .get("url")
                .and_then(|u| u.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "{} {}",
                        s["command"].as_str().unwrap_or(""),
                        s["args"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|a| a.as_str())
                            .collect::<Vec<_>>()
                            .join(" ")
                    )
                });
            println!(
                "  {}. {n}  {}",
                i + 1,
                crate::util::one_line(target.trim(), 80)
            );
        }
        print!("Import which? [all] (numbers or names, comma-separated; `none` to cancel): ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        pick(&line, &names)?
    };
    if chosen.is_empty() {
        return Ok("Nothing imported.".into());
    }
    // Names already in use anywhere (every scope, trusted or not, and imported ones).
    let mut existing: Vec<String> = Config::load(cwd, crate::config::is_trusted_here(cwd))
        .map(|(c, _)| c.mcp_servers.keys().cloned().collect())
        .unwrap_or_default();
    for s in ["local", "project", "user"] {
        let text = scope_path(s, cwd)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        existing.extend(
            servers_in(&text)
                .map(|t| t.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default(),
        );
    }
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut out = Vec::new();
    for name in chosen {
        if !valid_name(&name) {
            out.push(format!("Skipped {name:?}: names may contain only letters, numbers, hyphens and underscores."));
            continue;
        }
        let t = match json_to_table(&servers[&name]) {
            Ok(t) => t,
            Err(e) => {
                out.push(format!("Skipped {name}: {e:#}"));
                continue;
            }
        };
        let in_file = servers_in(&text).with_context(|| format!("parsing {}", path.display()))?;
        let final_name = unique_name(&name, &|n| {
            in_file.contains_key(n) || existing.iter().any(|e| e == n)
        });
        append(&path, &text, &render_table(&final_name, &t)?)?;
        text = std::fs::read_to_string(&path)?;
        out.push(format!(
            "Imported {name}{} ({}).",
            if final_name != name {
                format!(" as {final_name}")
            } else {
                String::new()
            },
            transport_of(&t)
        ));
    }
    out.push(format!(
        "Saved to {} ({scope} scope). `chanto mcp list` shows them.",
        path.display()
    ));
    if scope != "user" && !crate::config::is_trusted_here(cwd) {
        out.push("Note: this project isn't trusted yet, so chanto ignores its MCP servers until you trust it (or use --scope user).".into());
    }
    Ok(out.join("\n"))
}

fn remove(args: &[String], cwd: &Path) -> Result<String> {
    let mut scope = None;
    let mut name = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-s" | "--scope" => scope = Some(it.next().cloned().context("--scope needs a value")?),
            s if s.starts_with('-') => bail!("unknown option {s}"),
            _ => name = Some(a.clone()),
        }
    }
    let Some(name) = name else { bail!(USAGE) };
    let scopes: Vec<String> = match scope {
        Some(s) => vec![s],
        None => ["local", "project", "user"].map(String::from).to_vec(),
    };
    for scope in scopes {
        let path = scope_path(&scope, cwd)?;
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !servers_in(&text)
            .map(|s| s.contains_key(&name))
            .unwrap_or(false)
        {
            continue;
        }
        // Drop the server's table headers and their bodies.
        let mut out = String::new();
        let mut skipping = false;
        for line in text.split_inclusive('\n') {
            let t = line.trim_start();
            if t.starts_with('[') {
                skipping = is_server_header(line, &name);
            }
            if !skipping {
                out.push_str(line);
            }
        }
        if servers_in(&out)
            .map(|s| s.contains_key(&name))
            .unwrap_or(true)
        {
            bail!(
                "couldn't remove {name} from {} automatically (inline table?); edit the file by hand",
                path.display()
            );
        }
        let out = out.trim_end().to_string() + "\n";
        let body = if out.trim().is_empty() {
            String::new()
        } else {
            out
        };
        write_config(&path, &body)?;
        return Ok(format!(
            "Removed MCP server {name} from {} ({scope} scope).",
            path.display()
        ));
    }
    bail!(
        "no MCP server named {name} in chanto's config files (servers imported from Claude Code or Codex are managed there)"
    )
}

fn describe(name: &str, s: &crate::config::McpServerConfig) -> String {
    let target = match &s.url {
        Some(u) => format!(
            "{u} ({})",
            if s.transport.as_deref() == Some("sse") {
                "sse"
            } else {
                "http"
            }
        ),
        None => format!("{} {}", s.command, s.args.join(" "))
            .trim()
            .to_string(),
    };
    let mut tags = vec![];
    if s.disabled {
        tags.push("disabled".to_string());
    }
    if let Some(g) = &s.gate {
        tags.push(g.describe());
    }
    if let Some(from) = &s.imported_from {
        tags.push(format!("from {from}"));
    }
    let tags = if tags.is_empty() {
        String::new()
    } else {
        format!("  [{}]", tags.join(", "))
    };
    format!("{name}: {target}{tags}")
}

/// Run `chanto mcp <args>`; returns the text to print.
pub fn run(args: &[String]) -> Result<String> {
    let cwd = std::env::current_dir()?;
    let trusted = crate::config::is_trusted_here(&cwd);
    match args.first().map(String::as_str) {
        Some("add") => add(&args[1..], &cwd),
        Some("add-json") => add_json(&args[1..], &cwd),
        Some("add-from-claude-desktop") => add_from_claude_desktop(&args[1..], &cwd),
        Some("remove" | "rm") => remove(&args[1..], &cwd),
        Some("list" | "ls") => list(&cwd, trusted),
        Some("get") => {
            let name = args.get(1).context("usage: chanto mcp get <name>")?;
            let (cfg, _) = Config::load(&cwd, trusted)?;
            let s = cfg
                .mcp_servers
                .get(name)
                .with_context(|| format!("no MCP server named {name}"))?;
            let mut out = describe(name, s);
            for k in s.env.keys() {
                out += &format!("\n  env {k}=…");
            }
            for k in s.headers.keys() {
                out += &format!("\n  header {k}: …");
            }
            Ok(out)
        }
        _ => Ok(USAGE.to_string()),
    }
}

/// `chanto mcp list`.
fn list(cwd: &Path, trusted: bool) -> Result<String> {
    let (cfg, _) = Config::load(cwd, trusted)?;
    let approvals = crate::mcp_policy::Approvals::load(
        cwd,
        &cfg.compat,
        trusted,
        crate::config::is_trusted(cwd),
    );
    let unloaded = crate::mcp_policy::unloaded_mcpjson(cwd, &cfg.mcp_servers, &approvals);
    // Servers an untrusted folder's own config defines (say, just added with
    // `mcp add`): listed, but marked as not loaded.
    let held_back: Vec<(String, crate::config::McpServerConfig)> = if trusted {
        vec![]
    } else {
        Config::load(cwd, true)?
            .0
            .mcp_servers
            .into_iter()
            .filter(|(n, _)| {
                !cfg.mcp_servers.contains_key(n) && !unloaded.iter().any(|(u, _)| u == n)
            })
            .collect()
    };
    if cfg.mcp_servers.is_empty() && unloaded.is_empty() && held_back.is_empty() {
        return Ok("No MCP servers configured. Add one with `chanto mcp add`.".into());
    }
    let mut lines: Vec<String> = cfg
        .mcp_servers
        .iter()
        .map(|(n, s)| describe(n, s))
        .collect();
    lines.extend(held_back.iter().map(|(n, s)| {
        format!(
            "{}  (untrusted folder: not loaded until you trust it)",
            describe(n, s)
        )
    }));
    lines.extend(unloaded.iter().map(|(n, a)| match a {
        crate::mcp_policy::Approval::Rejected => format!("{n}: .mcp.json  [rejected (see disabledMcpjsonServers)]"),
        _ => format!("{n}: .mcp.json  [pending approval: trust the folder, or run /mcp enable {n} in a session]"),
    }));
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn user_config_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let _g = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let path = home_dir().join(format!("mcp-cli-perm-{}.toml", std::process::id()));
        write_config(&path, "[mcp_servers.a]\ncommand = \"x\"\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let _ = std::fs::remove_file(&path);
        assert_eq!(mode, 0o600, "the user config must be owner-only");
    }

    #[test]
    fn add_and_remove_keep_other_content() {
        let _g = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("chanto-mcpcli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".chanto")).unwrap();
        let file = dir.join(".chanto/config.local.toml");
        std::fs::write(
            &file,
            "# my settings\nmodel = \"qwen3:8b\"\n\n[mcp_servers.keep]\ncommand = \"keep-me\"\n",
        )
        .unwrap();
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        add(
            &s(&[
                "-e",
                "TOKEN=abc",
                "fs",
                "--",
                "npx",
                "-y",
                "@mcp/fs",
                "--root",
                ".",
            ]),
            &dir,
        )
        .unwrap();
        add(
            &s(&[
                "-H",
                "Authorization: Bearer x",
                "docs",
                "https://example.com/mcp",
            ]),
            &dir,
        )
        .unwrap();
        add(&s(&["-t", "sse", "old", "https://example.com/sse"]), &dir).unwrap();
        assert!(
            add(&s(&["fs", "--", "other"]), &dir).is_err(),
            "duplicate names are refused"
        );
        let servers = servers_in(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(
            servers["fs"]["args"].as_array().unwrap().len(),
            4,
            "flags after -- belong to the server"
        );
        assert_eq!(servers["fs"]["env"]["TOKEN"].as_str(), Some("abc"));
        assert_eq!(
            servers["docs"]["url"].as_str(),
            Some("https://example.com/mcp")
        );
        assert_eq!(
            servers["docs"]["headers"]["Authorization"].as_str(),
            Some("Bearer x")
        );
        assert_eq!(servers["old"]["transport"].as_str(), Some("sse"));
        remove(&s(&["fs"]), &dir).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        let servers = servers_in(&text).unwrap();
        assert!(
            !servers.contains_key("fs")
                && servers.contains_key("keep")
                && servers.contains_key("docs")
        );
        assert!(
            text.starts_with("# my settings\nmodel = \"qwen3:8b\""),
            "{text}"
        );
        assert!(remove(&s(&["nope"]), &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_entries_convert() {
        let t = json_to_table(&serde_json::json!({"type": "stdio", "command": "/bin/w", "args": ["--k", "abc"], "env": {"CACHE": "/tmp"}})).unwrap();
        assert_eq!(t["command"].as_str(), Some("/bin/w"));
        assert_eq!(t["args"].as_array().unwrap().len(), 2);
        assert_eq!(t["env"]["CACHE"].as_str(), Some("/tmp"));
        let t = json_to_table(&serde_json::json!({"type": "streamable-http", "url": "https://x/mcp", "headers": {"Authorization": "Bearer t"}, "oauth": {"clientId": "id", "callbackPort": 8080}})).unwrap();
        assert_eq!(transport_of(&t), "http");
        assert_eq!(t["oauth"]["client_id"].as_str(), Some("id"));
        assert_eq!(t["oauth"]["callback_port"].as_integer(), Some(8080));
        assert_eq!(
            transport_of(
                &json_to_table(&serde_json::json!({"type": "sse", "url": "https://x/sse"}))
                    .unwrap()
            ),
            "sse"
        );
        assert_eq!(
            transport_of(&json_to_table(&serde_json::json!({"url": "https://x/mcp"})).unwrap()),
            "http",
            "a url alone means http"
        );
        assert!(json_to_table(&serde_json::json!({"type": "ws", "url": "wss://x"})).is_err());
        assert!(json_to_table(&serde_json::json!({"type": "http"})).is_err());
        let e = json_to_table(&serde_json::json!({"type": "http", "url": "api.example.com/mcp"}))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("no http:// or https:// scheme")
                && e.contains("https://api.example.com/mcp"),
            "{e}"
        );
        assert!(json_to_table(&serde_json::json!({"url": "localhost:8080/mcp"})).is_err());
        assert!(json_to_table(&serde_json::json!({"url": "HTTPS://x/mcp"})).is_ok());
        assert!(
            json_to_table(&serde_json::json!({"url": "${MCP_URL}/mcp"})).is_ok(),
            "a variable may hold the scheme"
        );
        assert!(json_to_table(&serde_json::json!({"args": ["x"]})).is_err());
        assert!(json_to_table(&serde_json::json!({"command": "x", "env": {"A": 1}})).is_err());
        let block = render_table(
            "srv",
            &json_to_table(&serde_json::json!({"command": "x", "env": {"A": "1"}})).unwrap(),
        )
        .unwrap();
        let servers = servers_in(&block).unwrap();
        assert_eq!(servers["srv"]["env"]["A"].as_str(), Some("1"), "{block}");
    }

    #[test]
    fn picking_and_naming() {
        let names: Vec<String> = ["a", "b", "c"].map(String::from).to_vec();
        assert_eq!(pick("", &names).unwrap(), names);
        assert_eq!(pick("all", &names).unwrap(), names);
        assert!(pick("none", &names).unwrap().is_empty());
        assert_eq!(pick("1, c 1", &names).unwrap(), vec!["a", "c"]);
        assert!(pick("4", &names).is_err() && pick("zz", &names).is_err());
        let taken = |n: &str| n == "x" || n == "x_1";
        assert_eq!(unique_name("x", &taken), "x_2");
        assert_eq!(unique_name("y", &taken), "y");
        assert!(valid_name("my-server_2") && !valid_name("my server") && !valid_name(""));
    }

    #[test]
    fn add_json_and_import_from_claude_desktop() {
        let _g = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir =
            std::env::temp_dir().join(format!("chanto-mcpcli2-{}", uuid::Uuid::new_v4().simple()));
        let home = dir.join("home");
        let desktop = home.join("Library/Application Support/Claude");
        std::fs::create_dir_all(&desktop).unwrap();
        std::fs::create_dir_all(dir.join("proj")).unwrap();
        let proj = dir.join("proj");
        let prev = std::env::var_os("CHANTO_COMPAT_HOME");
        unsafe { std::env::set_var("CHANTO_COMPAT_HOME", &home) };
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        add_json(
            &s(&[
                "weather",
                r#"{"type":"http","url":"https://api.example.com/mcp"}"#,
            ]),
            &proj,
        )
        .unwrap();
        assert!(
            add_json(&s(&["weather", r#"{"command":"x"}"#]), &proj).is_err(),
            "duplicates are refused"
        );
        assert!(add_json(&s(&["bad name", r#"{"command":"x"}"#]), &proj).is_err());
        assert!(
            add_json(&s(&["w", r#"{"mcpServers":{}}"#]), &proj).is_err(),
            "the wrapper is refused"
        );
        std::fs::write(
            desktop.join("claude_desktop_config.json"),
            r#"{"mcpServers": {"weather": {"command": "npx", "args": ["-y", "w"]}, "fs": {"command": "fs-srv"}, "has space": {"command": "x"}}}"#,
        )
        .unwrap();
        // The project isn't trusted: the server is listed, marked as not loaded.
        let listed = list(&proj, false).unwrap();
        assert!(listed.contains("weather: https://api.example.com/mcp (http)  (untrusted folder: not loaded until you trust it)"), "{listed}");
        let listed = list(&proj, true).unwrap();
        assert!(
            listed.contains("weather: https://api.example.com/mcp (http)")
                && !listed.contains("untrusted"),
            "{listed}"
        );
        let e = add_json(
            &s(&["noscheme", r#"{"type":"http","url":"api.example.com/mcp"}"#]),
            &proj,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("no http:// or https:// scheme"), "{e}");
        let out = add_from_claude_desktop(&s(&["--all"]), &proj).unwrap();
        assert!(out.contains("Imported weather as weather_1"), "{out}");
        assert!(out.contains("Imported fs"), "{out}");
        assert!(out.contains("Skipped \"has space\""), "{out}");
        let servers =
            servers_in(&std::fs::read_to_string(proj.join(".chanto/config.local.toml")).unwrap())
                .unwrap();
        assert_eq!(
            servers["weather"]["url"].as_str(),
            Some("https://api.example.com/mcp")
        );
        assert_eq!(servers["weather_1"]["command"].as_str(), Some("npx"));
        assert!(servers.contains_key("fs") && !servers.contains_key("has space"));
        // Picking by name, into another scope.
        let out = add_from_claude_desktop(&s(&["-s", "project", "fs"]), &proj).unwrap();
        assert!(
            out.contains("Imported fs as fs_1"),
            "the local one is taken: {out}"
        );
        match prev {
            Some(v) => unsafe { std::env::set_var("CHANTO_COMPAT_HOME", v) },
            None => unsafe { std::env::remove_var("CHANTO_COMPAT_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
