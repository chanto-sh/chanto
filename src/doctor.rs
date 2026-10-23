//! One redacted report for the CLI and the session command.
use crate::{config::Config, detect, llm::Provider};
use anyhow::Result;
use serde_json::{Value, json};
use std::time::Duration;

pub fn resolve(cfg: &Config, spec: &str) -> (Provider, String, String) {
    if let Some((p, m)) = spec.split_once('/') {
        if let Some(pc) = cfg.providers.get(p) {
            return (
                pc.provider(if pc.kind.is_hosted() {
                    crate::llm::hosted_context(pc.kind, m)
                } else {
                    cfg.num_ctx()
                }),
                m.into(),
                p.into(),
            );
        }
        if (cfg.allows_builtin_hosted() || !matches!(p, "openai" | "anthropic"))
            && let Some(provider) = crate::llm::builtin_provider(p, m)
        {
            return (provider, m.into(), p.into());
        }
        if p == "ollama" {
            return (
                Provider::new(
                    crate::llm::ProviderKind::Ollama,
                    "http://localhost:11434",
                    None,
                    cfg.num_ctx(),
                ),
                m.into(),
                p.into(),
            );
        }
        if matches!(p, "lmstudio" | "llamacpp") {
            let url = if p == "lmstudio" {
                "http://localhost:1234/v1"
            } else {
                "http://localhost:8080/v1"
            };
            return (
                Provider::new(crate::llm::ProviderKind::Openai, url, None, cfg.num_ctx()),
                m.into(),
                p.into(),
            );
        }
    }
    (
        Provider::new(
            cfg.provider,
            &cfg.base_url(),
            cfg.api_key.clone(),
            cfg.num_ctx(),
        ),
        spec.into(),
        match cfg.provider {
            crate::llm::ProviderKind::Ollama => "ollama",
            crate::llm::ProviderKind::Anthropic => "anthropic",
            crate::llm::ProviderKind::Openai | crate::llm::ProviderKind::OpenaiResponses => {
                "openai"
            }
            k => k.cloud_id().unwrap_or("cloud"),
        }
        .into(),
    )
}

/// Where a setting shown by doctor comes from. A `provider/model` spec takes
/// its provider and endpoint from the `[providers.<name>]` entry it names, or
/// from the spec itself for built-in prefixes (`ollama/…`, `openrouter/…`),
/// whatever the top-level `provider` and `base_url` say.
fn setting_source(cfg: &Config, spec: &str, key: &str) -> String {
    let origin = |k: &str| cfg.origins.get(k).map(|p| p.display().to_string());
    let prefix = spec.split_once('/').map(|(p, _)| p);
    if matches!(key, "provider" | "base_url") && prefix.is_some() && resolve(cfg, spec).1 != spec {
        let k = if prefix.is_some_and(|p| cfg.providers.contains_key(p)) {
            "providers"
        } else {
            "model"
        };
        if let Some(o) = origin(k) {
            return o;
        }
    }
    origin(key).unwrap_or_else(|| "built-in default".into())
}

/// Where each MCP server is configured.
fn mcp_sources(cfg: &Config) -> std::collections::BTreeMap<String, String> {
    cfg.mcp_servers
        .keys()
        .map(|name| {
            let origin = cfg
                .origins
                .get(&format!("mcp_servers.{name}"))
                .or_else(|| cfg.origins.get("mcp_servers"));
            (
                name.clone(),
                origin
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "unknown".into()),
            )
        })
        .collect()
}

/// The MCP section: one server per line with where it's configured.
fn mcp_lines(sources: &Value, home: &str) -> String {
    let lines: Vec<String> = sources
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, src)| format!("{name}  ← {}", shorten(&human(src), home)))
        .collect();
    if lines.is_empty() {
        "none".into()
    } else {
        lines.join("\n  ")
    }
}

fn endpoint(url: &str) -> String {
    // Do not emit URL userinfo, query strings, or fragments.
    let mut url = reqwest::Url::parse(url).ok();
    if let Some(u) = &mut url {
        let _ = u.set_username("");
        let _ = u.set_password(None);
        u.set_query(None);
        u.set_fragment(None);
    }
    url.map(|u| u.to_string())
        .unwrap_or_else(|| "invalid URL".into())
}

/// Where the credential for `spec` comes from (a config file, `env:VAR`, a
/// saved login), without contacting anything. `None` for local servers and
/// cloud platforms, whose credentials only `report` resolves.
pub fn credential(cfg: &Config, spec: &str) -> Option<String> {
    let (p, m, name) = resolve(cfg, spec);
    let named = spec
        .split_once('/')
        .and_then(|(name, _)| cfg.providers.get(name));
    let prefix = spec.split_once('/').map(|(p, _)| p);
    let local =
        named.is_none() && prefix.is_some_and(|p| matches!(p, "ollama" | "lmstudio" | "llamacpp"));
    let builtin = named.is_none()
        && prefix.is_some_and(|p| {
            (cfg.allows_builtin_hosted() || !matches!(p, "openai" | "anthropic"))
                && crate::llm::builtin_provider(p, &m).is_some()
        });
    if local || p.kind.is_cloud() {
        None
    } else if named.is_some_and(|pc| pc.api_key.is_some()) {
        Some(
            cfg.origins
                .get("providers")
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "configured provider key".into()),
        )
    } else if named.is_some() || !builtin {
        if cfg.api_key.is_some() && named.is_none() {
            Some(
                cfg.origins
                    .get("api_key")
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "configured key".into()),
            )
        } else if cfg.api_key_helper.is_some() && named.is_none() {
            Some("helper".into())
        } else {
            p.kind
                .key_env()
                .filter(|v| std::env::var(v).is_ok_and(|s| !s.is_empty()))
                .map(|v| format!("env:{v}"))
        }
    } else {
        detect::credential_source(&name)
    }
}

pub async fn report(cfg: &Config, model: Option<&str>) -> Value {
    let candidates = detect::detect().await;
    let model = model.or(cfg.model.as_deref());
    let spec = model.unwrap_or("");
    let (p, m, name) = resolve(cfg, spec);
    // Cloud platforms: credential source, location and a request that spends no tokens.
    let cloud = if p.kind.is_cloud() {
        Some(
            tokio::time::timeout(Duration::from_secs(30), p.cloud_check(&m))
                .await
                .unwrap_or_else(|_| crate::llm::CloudCheck {
                    credential: None,
                    details: vec![],
                    reachable: false,
                    note: "timed out after 30s resolving credentials or contacting the endpoint"
                        .into(),
                }),
        )
    } else {
        None
    };
    let credential = if let Some(c) = &cloud {
        c.credential.clone()
    } else {
        credential(cfg, spec)
    };
    // Why a hosted endpoint didn't answer, to say what to fix (a rejected key isn't a network problem).
    let mut list_error = None;
    let (reachable, native_tools) = if let Some(c) = &cloud {
        (c.reachable, Some(true))
    } else if p.kind == crate::llm::ProviderKind::Ollama && !m.is_empty() {
        let request = async {
            crate::http::client()
                .post(format!("{}/api/show", p.base_url.trim_end_matches('/')))
                .json(&json!({"model": m}))
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await
        };
        match tokio::time::timeout(Duration::from_secs(3), request).await {
            Ok(Ok(v)) => (
                true,
                v["capabilities"]
                    .as_array()
                    .map(|caps| caps.iter().any(|c| c.as_str() == Some("tools"))),
            ),
            _ => (false, None),
        }
    } else {
        let listed = tokio::time::timeout(Duration::from_secs(3), p.list_models()).await;
        if let Ok(Err(e)) = &listed {
            list_error = Some(format!("{e:#}"));
        }
        (
            listed.is_ok_and(|r| r.is_ok()),
            matches!(
                p.kind,
                crate::llm::ProviderKind::Anthropic | crate::llm::ProviderKind::OpenaiResponses
            )
            .then_some(true),
        )
    };
    let mut warnings = cfg.diagnostics.clone();
    warnings.extend(
        candidates
            .iter()
            .filter(|c| c.note.starts_with("Warning"))
            .map(|c| {
                format!(
                    "{}: {}",
                    c.model.as_deref().unwrap_or("local model"),
                    c.note
                )
            }),
    );
    if m.is_empty() {
        warnings
            .push("No model is set up: run `chanto setup` (or just `chanto`) to pick one".into());
    }
    if p.kind == crate::llm::ProviderKind::Ollama && p.num_ctx < 32768 {
        warnings.push("Ollama context <32k; set num_ctx = 32768".into());
    }
    if let Some(c) = cloud.as_ref().filter(|c| c.note != "credentials accepted") {
        warnings.push(format!("{name}: {}", c.note));
    }
    // A server that's down keeps the generic warning below (that hint points back at doctor).
    else if let Some(hint) = list_error
        .as_deref()
        .and_then(|e| crate::llm::failure_hint(e, p.kind, &endpoint(&p.base_url), &m, Some(&name)))
        .filter(|h| !h.starts_with("Can't reach"))
    {
        let from = credential
            .as_deref()
            .map(|c| format!("it comes from {c}"))
            .unwrap_or_else(|| "none is configured".into());
        warnings.push(hint.replace("`chanto doctor` shows where it comes from", &from));
    } else if !reachable {
        warnings.push(format!(
            "Provider unreachable{}; check network/base_url or run chanto setup",
            if credential.is_some() {
                " with credentials present"
            } else {
                ""
            }
        ));
    }
    let base_url = match &cloud {
        Some(c) if p.base_url.is_empty() => c
            .details
            .iter()
            .find(|(k, _)| k == "endpoint")
            .map(|(_, v)| endpoint(v))
            .unwrap_or_else(|| "derived from region, project or resource".into()),
        _ => endpoint(&p.base_url),
    };
    let mut settings = serde_json::Map::new();
    for key in [
        "model",
        "provider",
        "base_url",
        "mode",
        "permissions",
        "mcp_servers",
        "hooks",
        "sandbox",
    ] {
        settings.insert(
            key.into(),
            json!({"source": setting_source(cfg, spec, key)}),
        );
    }
    settings.get_mut("permissions").unwrap()["allow_count"] = json!(cfg.permissions.allow.len());
    settings.get_mut("permissions").unwrap()["deny_count"] = json!(cfg.permissions.deny.len());
    if model != cfg.model.as_deref() {
        settings.get_mut("model").unwrap()["source"] = json!("session selection");
    }
    settings.get_mut("model").unwrap()["value"] = json!(model);
    settings.get_mut("provider").unwrap()["value"] = json!(name);
    settings.get_mut("base_url").unwrap()["value"] = json!(base_url);
    settings.get_mut("mcp_servers").unwrap()["configured"] =
        json!(cfg.mcp_servers.keys().collect::<Vec<_>>());
    let mut mcp_files: Vec<String> = mcp_sources(cfg).into_values().collect();
    mcp_files.sort();
    mcp_files.dedup();
    if !mcp_files.is_empty() {
        settings.get_mut("mcp_servers").unwrap()["source"] = json!(mcp_files.join(", "));
    }
    settings.get_mut("mode").unwrap()["value"] = json!(cfg.mode.unwrap_or_default());
    settings.get_mut("sandbox").unwrap()["enabled"] = json!(cfg.sandbox.enabled);
    settings.get_mut("sandbox").unwrap()["network"] = json!(cfg.sandbox.network_policy());
    settings.get_mut("sandbox").unwrap()["allowed_domains"] = json!(cfg.sandbox.network_domains());
    settings.get_mut("sandbox").unwrap()["denied_domains"] = json!(cfg.sandbox.denied_domains);
    settings.get_mut("sandbox").unwrap()["network_description"] =
        json!(cfg.sandbox.network_description());
    settings.get_mut("hooks").unwrap()["configured"] = json!(!cfg.hooks.is_empty());
    let mut resolved = json!({ "model": model, "provider": name, "base_url": base_url, "credential_source": credential, "reachable": reachable, "native_tools": native_tools });
    if let Some(c) = &cloud {
        // Region, project, API and the check result; never a credential value.
        for (k, v) in c.details.iter().filter(|(k, _)| k != "endpoint") {
            resolved[k] = json!(v);
        }
        resolved["check"] = json!(c.note);
    }
    json!({
        "schema_version": 1,
        "version": env!("CARGO_PKG_VERSION"),
        "binary": std::env::current_exe().ok(),
        "config_home": crate::config::home_dir(),
        "layers": cfg.layers,
        "settings": settings,
        "resolved_model": resolved,
        "sandbox": if crate::tools::bash::sandbox_available() { if cfg!(target_os = "macos") { "macOS seatbelt" } else if cfg!(windows) { "Windows restricted token (writes only)" } else { "Linux Landlock" } } else { "none" },
        "mcp_servers": cfg.mcp_servers.keys().collect::<Vec<_>>(),
        "mcp_sources": mcp_sources(cfg),
        "candidates": candidates,
        "error_reports": crate::feedback::status(cfg),
        "folder_trusted": crate::config::is_trusted(&std::env::current_dir().unwrap_or_default()),
        "tools": { "rg": crate::shell::command_exists("rg"), "git": crate::shell::command_exists("git"), "pdftotext": crate::shell::command_exists("pdftotext"), "chrome": chrome_available() },
        "cloud": crate::chanto_cloud::doctor_info(),
        "local_servers": candidates.into_iter().filter(|c| c.source.starts_with("http://localhost:")).collect::<Vec<_>>(),
        "warnings": warnings,
    })
}

fn chrome_available() -> bool {
    [
        "/Applications/Google Chrome.app",
        "/Applications/Chromium.app",
        "/Applications/Brave Browser.app",
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    ]
    .iter()
    .any(|p| std::path::Path::new(p).exists())
        || ["google-chrome", "chromium", "chrome"]
            .iter()
            .any(|c| crate::shell::command_exists(c))
}

fn human(value: &Value) -> String {
    match value {
        Value::Null => "none".into(),
        Value::Bool(b) => if *b { "yes" } else { "no" }.into(),
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            if items.is_empty() {
                "none".into()
            } else {
                items.iter().map(human).collect::<Vec<_>>().join(", ")
            }
        }
        _ => value.to_string(),
    }
}

fn shorten(path: &str, home: &str) -> String {
    if path == home {
        "~".into()
    } else if let Some(rest) = path.strip_prefix(&format!("{home}/")) {
        format!("~/{rest}")
    } else {
        path.into()
    }
}

pub fn render(report: &Value) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut out = format!(
        "chanto {}\nBinary: {}\nConfig home: {}\n",
        human(&report["version"]),
        shorten(&human(&report["binary"]), &home),
        shorten(&human(&report["config_home"]), &home)
    );
    out.push_str("\nConfig layers (lowest → highest)\n");
    if report["layers"].as_array().is_none_or(|l| l.is_empty()) {
        out.push_str(
            "  none yet (built-in defaults; `chanto setup` writes ~/.chanto/config.toml)\n",
        );
    }
    for layer in report["layers"].as_array().into_iter().flatten() {
        out.push_str(&format!("  {}\n", shorten(&human(layer), &home)));
    }
    out.push_str("\nEffective settings\n");
    let rows: Vec<_> = report["settings"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, v)| {
            let value = if key == "permissions" {
                format!(
                    "{} allow, {} deny",
                    human(&v["allow_count"]),
                    human(&v["deny_count"])
                )
            } else {
                human(
                    v.get("value")
                        .or_else(|| v.get("configured"))
                        .or_else(|| v.get("enabled"))
                        .unwrap_or(&Value::Null),
                )
            };
            (key, value, shorten(&human(&v["source"]), &home))
        })
        .collect();
    let width = rows
        .iter()
        .map(|(_, v, _)| v.chars().count())
        .max()
        .unwrap_or(0);
    for (key, value, source) in rows {
        out.push_str(&format!("  {key:<12} {value:width$}  ← {source}\n"));
    }
    for (title, key) in [("Model", "resolved_model"), ("Tools", "tools")] {
        out.push_str(&format!("\n{title}\n"));
        for (k, v) in report[key].as_object().into_iter().flatten() {
            out.push_str(&format!("  {k:<18} {}\n", shorten(&human(v), &home)));
        }
    }
    if report["cloud"].is_object() {
        out.push_str("\nchanto cloud\n");
        for (k, v) in report["cloud"].as_object().into_iter().flatten() {
            out.push_str(&format!("  {k:<18} {}\n", shorten(&human(v), &home)));
        }
    }
    out.push_str(&format!("\nSandbox\n  {}\n  Network: {}\n  Folder trusted: {}\n\nError reports\n  {}\n\nMCP\n  {}\n\nLocal servers\n", human(&report["sandbox"]), human(&report["settings"]["sandbox"]["network_description"]), human(&report["folder_trusted"]), human(&report["error_reports"]), mcp_lines(&report["mcp_sources"], &home)));
    if report["local_servers"]
        .as_array()
        .is_none_or(|l| l.is_empty())
    {
        out.push_str("  none found\n");
    }
    for c in report["local_servers"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  {:<12} {}  ← {}\n",
            human(&c["provider"]),
            human(&c["model"]),
            human(&c["source"])
        ));
    }
    out.push_str("\nWarnings\n");
    if report["warnings"].as_array().is_none_or(|w| w.is_empty()) {
        out.push_str("  none\n");
    }
    for w in report["warnings"].as_array().into_iter().flatten() {
        out.push_str(&format!("  {}\n", shorten(&human(w), &home)));
    }
    out
}

pub async fn cli(args: &[String]) -> Result<String> {
    if args.iter().any(|a| a != "--json") {
        anyhow::bail!("Usage: chanto doctor [--json]");
    }
    let cwd = std::env::current_dir()?;
    let (cfg, _) = Config::load_for_doctor(&cwd, crate::config::is_trusted(&cwd))?;
    let r = report(&cfg, None).await;
    Ok(if args.iter().any(|a| a == "--json") {
        serde_json::to_string_pretty(&r)?
    } else {
        render(&r)
    })
}

pub async fn models_cli(provider: Option<&str>) -> Result<String> {
    let cwd = std::env::current_dir()?;
    let (cfg, _) = Config::load(&cwd, crate::config::is_trusted(&cwd))?;
    let mut models = crate::onboarding::suggestions(Some(&cfg)).await;
    let default = Provider::new(
        cfg.provider,
        &cfg.base_url(),
        cfg.api_key.clone(),
        cfg.num_ctx(),
    );
    let (available, failures) = crate::onboarding::available_models(&cfg, default).await;
    models.extend(available);
    if !failures.is_empty() {
        eprintln!(
            "Some providers could not list models: {}",
            failures.join("; ")
        );
    }
    if let Some(p) = provider {
        models.retain(|m| m.starts_with(&format!("{p}/")) || (p == "ollama" && !m.contains('/')));
    }
    // The default server lists bare names; the suggestions already have them as `provider/name`.
    let prefixed: std::collections::HashSet<String> = models
        .iter()
        .filter_map(|m| m.strip_prefix("ollama/").map(String::from))
        .filter(|_| cfg.provider == crate::llm::ProviderKind::Ollama)
        .collect();
    models.retain(|m| m.contains('/') || !prefixed.contains(m));
    models.sort();
    models.dedup();
    if models.is_empty() {
        return Ok(match provider {
            Some(p) => format!(
                "No models found for `{p}`. `chanto providers` lists the providers and how to log in to each."
            ),
            None => "No models found. `chanto setup` helps you pick one.".into(),
        });
    }
    Ok(models.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn named_provider_settings_come_from_its_entry() {
        let mut cfg = Config::default();
        cfg.providers.insert(
            "local".into(),
            toml::from_str("kind = \"openai\"\nbase_url = \"http://localhost:4000/v1\"").unwrap(),
        );
        cfg.origins
            .insert("providers".into(), "/home/u/.chanto/config.toml".into());
        cfg.origins
            .insert("model".into(), "/p/.chanto/config.toml".into());
        assert_eq!(
            setting_source(&cfg, "local/m", "base_url"),
            "/home/u/.chanto/config.toml"
        );
        assert_eq!(
            setting_source(&cfg, "local/m", "provider"),
            "/home/u/.chanto/config.toml"
        );
        assert_eq!(
            setting_source(&cfg, "ollama/qwen3:8b", "provider"),
            "/p/.chanto/config.toml"
        );
        assert_eq!(
            setting_source(&cfg, "qwen3:8b", "provider"),
            "built-in default"
        );
        assert_eq!(setting_source(&cfg, "local/m", "mode"), "built-in default");
    }

    #[test]
    fn readable_report_and_home_paths() {
        assert_eq!(
            shorten("/tmp/home/.chanto/config.toml", "/tmp/home"),
            "~/.chanto/config.toml"
        );
        assert_eq!(
            shorten("/tmp/home-other/file", "/tmp/home"),
            "/tmp/home-other/file"
        );
        let r = json!({"settings": {"permissions": {"allow_count": 169, "deny_count": 0, "source": "settings.json"}, "model": {"value": "qwen3:8b", "source": "config.toml"}}});
        let text = render(&r);
        for section in [
            "Config layers",
            "Effective settings",
            "Model",
            "Sandbox",
            "MCP",
            "Local servers",
            "Tools",
            "Warnings",
        ] {
            assert!(text.contains(section));
        }
        assert!(text.contains("169 allow, 0 deny"));
        assert!(text.contains("← config.toml"));
        assert!(!text.contains("{\"") && !text.contains('\u{1b}'));
    }
}
