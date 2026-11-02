//! Setup discovery. Priority: Chanto config, usable Codex model, hosted
//! credentials (catalog order), local coder models, other local models, then
//! cloud platforms (ambient AWS or Google credentials are often for
//! something other than models, so they never outrank a local model).
use crate::{auth, config, llm::ProviderKind, providers};
use serde::Serialize;
use std::time::Duration;

#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    pub source: String,
    pub provider: String,
    pub model: Option<String>,
    pub note: String,
    pub recommended: bool,
}

pub const RECOMMENDED: &[(&str, &str)] = &[
    ("anthropic", "claude-sonnet-5-5"),
    ("openai", "gpt-6.1-sol"),
    ("openrouter", "anthropic/claude-sonnet-5.5"),
    ("gemini", "gemini-3.8-flash"),
    ("deepseek", "deepseek-flash"),
    ("xai", "grok-4"),
    ("mistral", "mistral-medium-latest"),
    ("groq", "openai/gpt-oss-120b"),
    ("cerebras", "qwen-3-235b-a22b-instruct-2507"),
    ("together", "Qwen/Qwen3-Coder-480B-A35B-Instruct-FP8"),
    (
        "fireworks",
        "accounts/fireworks/models/qwen3-coder-480b-a35b-instruct",
    ),
    ("moonshot", "kimi-k2-0905-preview"),
    ("zai", "glm-4.5"),
    ("minimax", "MiniMax-M2"),
    ("kimi-coding", "kimi-for-coding"),
    ("minimax-coding", "MiniMax-M3.1-Flash-Preview"),
    ("dashscope", "qwen3-coder-plus"),
    ("huggingface", "Qwen/Qwen3-Coder-480B-A35B-Instruct"),
    ("nvidia", "qwen/qwen3-coder-480b-a35b-instruct"),
    ("vercel", "anthropic/claude-sonnet-4.5"),
    ("opencode", "big-pickle"),
    ("baseten", "Qwen/Qwen3-Coder-480B-A35B-Instruct"),
    (
        "bedrock",
        "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
    ),
    ("vertex", "claude-sonnet-4-5@20250929"),
    ("foundry", "claude-sonnet-4-5"),
];

pub fn default_model(provider: &str) -> Option<String> {
    // Azure deployments are named per account.
    if provider == "azure" {
        return std::env::var("AZURE_OPENAI_DEPLOYMENT")
            .ok()
            .filter(|d| !d.is_empty())
            .map(|d| format!("azure/{d}"));
    }
    RECOMMENDED
        .iter()
        .find(|(p, _)| *p == provider)
        .map(|(p, m)| format!("{p}/{m}"))
}

fn is_cloud(provider: &str) -> bool {
    providers::find(provider).is_some_and(|h| h.kind.is_cloud())
}

pub fn coder(model: &str) -> bool {
    let m = model.to_lowercase();
    ["coder", "qwen3", "gpt-oss", "devstral"]
        .iter()
        .any(|s| m.contains(s))
}

/// Where a catalog provider's credential comes from. Cloud platforms count
/// only with a location too (region, project or endpoint); this reads
/// environment variables and local files, nothing more.
/// How to sign in when `spec` names a built-in provider (`deepseek/…`) that has
/// no credentials and no `[providers.<name>]` section. Without one the model
/// would quietly be sent to the default server instead, which then reports an
/// unknown model.
pub fn missing_login(cfg: &crate::config::Config, spec: &str) -> Option<String> {
    let (prefix, _) = spec.split_once('/')?;
    // With a custom default server (a LiteLLM proxy, LM Studio, vLLM), names like
    // `deepseek/…` are that server's own model ids, served without a key here.
    if cfg.providers.contains_key(prefix) || !cfg.allows_builtin_hosted() {
        return None;
    }
    let h = providers::find(prefix)?;
    if credential_source(prefix).is_some() {
        return None;
    }
    let how = if h.kind.is_cloud() {
        format!(
            "set up your {} credentials (`chanto doctor` shows what's missing)",
            h.name
        )
    } else if h.id == crate::chanto_cloud::PROVIDER_ID {
        "run `chanto cloud login` or set CHANTO_CLOUD_TOKEN".into()
    } else if h.browser_login {
        format!("run `chanto login {}`", h.id)
    } else {
        format!("run `chanto login {}` or set {}", h.id, h.env.join(" or "))
    };
    Some(format!("No {} credentials for `{spec}`: {how}.", h.name))
}

pub fn credential_source(provider: &str) -> Option<String> {
    if provider == crate::chanto_cloud::PROVIDER_ID {
        return crate::chanto_cloud::credential_source();
    }
    let h = providers::find(provider)?;
    if h.kind.is_cloud() {
        let vars = crate::cloud::Vars::process();
        let settings = crate::cloud::Settings::default();
        let key = auth::get(provider)
            .map(|_| config::home_dir().join("auth.json").display().to_string())
            .or_else(|| {
                h.env
                    .iter()
                    .find(|v| std::env::var(v).is_ok_and(|s| !s.is_empty()))
                    .map(|v| format!("env:{v}"))
            });
        return match key {
            // A Bedrock API key or Azure key still needs a region or endpoint.
            Some(k) => providers::cloud_located(h.kind, &settings, &vars).then_some(k),
            None => providers::cloud_ready(h.kind, &settings, &vars),
        };
    }
    if auth::get(provider).is_some() {
        return Some(config::home_dir().join("auth.json").display().to_string());
    }
    h.env
        .iter()
        .find(|v| std::env::var(v).is_ok_and(|s| !s.is_empty()))
        .map(|v| format!("env:{v}"))
}

/// Whether the user chose a cloud platform for Chanto, rather than merely
/// having its credentials around (an AWS profile or gcloud login is there for
/// other tools too). Model lists show Bedrock, Vertex, Azure and Foundry only
/// then; `chanto setup` still offers them, since picking one there is the choice.
pub fn cloud_opted_in(provider: &str, cfg: Option<&config::Config>) -> bool {
    let Some(h) = providers::find(provider).filter(|h| h.kind.is_cloud()) else {
        return true;
    };
    let flag = |v: String| {
        std::env::var(v)
            .is_ok_and(|s| !s.is_empty() && s != "0" && !s.eq_ignore_ascii_case("false"))
    };
    let claude_flag = match provider {
        "bedrock" => Some("CLAUDE_CODE_USE_BEDROCK"),
        "vertex" => Some("CLAUDE_CODE_USE_VERTEX"),
        "foundry" => Some("CLAUDE_CODE_USE_FOUNDRY"),
        _ => None,
    };
    auth::get(provider).is_some()
        || h.env
            .iter()
            .any(|v| std::env::var(v).is_ok_and(|s| !s.is_empty()))
        || flag(format!("CHANTO_USE_{}", provider.to_ascii_uppercase()))
        || claude_flag.is_some_and(|v| flag(v.to_string()))
        || cfg.is_some_and(|c| {
            c.providers.contains_key(provider)
                || c.model
                    .as_deref()
                    .is_some_and(|m| m.starts_with(&format!("{provider}/")))
        })
}

pub async fn probe(provider: &str, url: &str) -> Vec<Candidate> {
    let client = match crate::http::builder()
        .timeout(Duration::from_millis(300))
        .no_proxy()
        .build()
    {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let url = url.trim_end_matches('/');
    let path = if provider == "ollama" {
        "/api/tags"
    } else if url.ends_with("/v1") {
        "/models"
    } else {
        "/v1/models"
    };
    let result = tokio::time::timeout(Duration::from_millis(300), async {
        client
            .get(format!("{url}{path}"))
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await
    })
    .await;
    let Ok(Ok(v)) = result else { return vec![] };
    let models = if provider == "ollama" {
        &v["models"]
    } else {
        &v["data"]
    };
    let mut models: Vec<_> = models.as_array().into_iter().flatten().collect();
    models.sort_by(|a, b| {
        local_rank(b)
            .partial_cmp(&local_rank(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    models
        .into_iter()
        .filter_map(|m| {
            let name = m[if provider == "ollama" { "name" } else { "id" }].as_str()?;
            let context = m["num_ctx"]
                .as_u64()
                .or_else(|| m["details"]["num_ctx"].as_u64());
            // Ollama says how big each model is, which tells otherwise similar entries apart.
            let mut size: Vec<String> = Vec::new();
            if let Some(p) = m["details"]["parameter_size"].as_str() {
                size.push(format!("{p} parameters"));
            }
            if let Some(b) = m["size"].as_u64().filter(|b| *b > 0) {
                size.push(format!("{:.1} GB", b as f64 / 1e9));
            }
            let size = if size.is_empty() {
                String::new()
            } else {
                format!(" · {}", size.join(", "))
            };
            Some(Candidate {
                source: url.into(),
                provider: provider.into(),
                model: Some(name.into()),
                note: if context.is_some_and(|n| n < 32768) {
                    format!("Warning: context <32k; set num_ctx = 32768{size}")
                } else if coder(name) {
                    format!("Coder/tool-capable model{size}")
                } else {
                    format!("Local model; tool support unknown{size}")
                },
                recommended: false,
            })
        })
        .collect()
}

fn local_rank(m: &serde_json::Value) -> (bool, f64, &str) {
    let name = m["name"]
        .as_str()
        .or_else(|| m["id"].as_str())
        .unwrap_or("");
    let size = m["details"]["parameter_size"]
        .as_str()
        .unwrap_or("")
        .to_uppercase();
    let count = size
        .trim_end_matches(['B', 'M'])
        .parse::<f64>()
        .unwrap_or(0.0);
    (
        coder(name),
        if size.ends_with('M') {
            count / 1000.0
        } else {
            count
        },
        m["modified_at"].as_str().unwrap_or(""),
    )
}

pub fn selection_notice(c: &Candidate) -> String {
    let model = c.model.as_deref().unwrap_or("");
    let spec = if matches!(c.provider.as_str(), "ollama" | "lmstudio" | "llamacpp") {
        format!("{}/{model}", c.provider)
    } else {
        model.into()
    };
    format!(
        "chanto: using {spec} (found on {}; run \"chanto setup\" to choose)",
        c.source.trim_start_matches("http://")
    )
}

pub fn rank(candidates: &mut [Candidate]) {
    candidates.sort_by_key(|c| {
        if c.source.ends_with(".chanto/config.toml")
            || c.source == config::home_dir().join("config.toml").display().to_string()
        {
            0
        } else if (c.source.ends_with(".codex/config.toml") || c.note.starts_with("Codex model"))
            && c.model.is_some()
        {
            1
        } else if is_cloud(&c.provider) {
            5
        } else if !c.source.starts_with("http:") && c.model.is_some() {
            2
        } else if c.model.as_deref().is_some_and(coder) {
            3
        } else {
            4
        }
    });
    for c in candidates.iter_mut() {
        c.recommended = false;
    }
    if let Some(c) = candidates.iter_mut().find(|c| c.model.is_some()) {
        c.recommended = true;
    }
}

pub async fn detect() -> Vec<Candidate> {
    let mut out = Vec::new();
    let global = config::home_dir().join("config.toml");
    let raw = std::fs::read_to_string(&global)
        .ok()
        .and_then(|s| toml::from_str::<toml::Table>(&s).ok());
    if let Some(m) = raw
        .as_ref()
        .and_then(|t| t.get("model"))
        .and_then(|v| v.as_str())
    {
        out.push(Candidate {
            source: global.display().to_string(),
            provider: raw
                .as_ref()
                .and_then(|t| t.get("provider"))
                .and_then(|v| v.as_str())
                .unwrap_or("ollama")
                .into(),
            model: Some(m.into()),
            note: "Existing chanto default; setup complete".into(),
            recommended: false,
        });
    }
    for h in providers::CATALOG {
        if let Some(source) = credential_source(h.id) {
            let note = if h.kind.is_cloud() {
                format!(
                    "{} credentials found; model access not verified (chanto doctor checks)",
                    h.name
                )
            } else {
                "Credential available".into()
            };
            out.push(Candidate {
                source,
                provider: h.id.into(),
                model: default_model(h.id),
                note,
                recommended: false,
            });
        }
    }
    let enabled = std::env::var("CHANTO_NO_CODEX_CONFIG").as_deref() != Ok("1")
        && raw
            .as_ref()
            .and_then(|t| t.get("compat"))
            .and_then(|v| v.get("codex"))
            .and_then(|v| v.as_bool())
            != Some(false);
    if enabled {
        let path = crate::compat::codex_dir().join("config.toml");
        if let Some(t) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str::<toml::Table>(&s).ok())
            && let Some(m) = t.get("model").and_then(|v| v.as_str())
        {
            let p = t
                .get("model_provider")
                .and_then(|v| v.as_str())
                .unwrap_or("openai");
            let p = if p == "openai" && credential_source("chatgpt").is_some() {
                "chatgpt"
            } else {
                p
            };
            if providers::find(p).is_some() {
                let usable = credential_source(p).is_some();
                let effort = t
                    .get("model_reasoning_effort")
                    .and_then(|v| v.as_str())
                    .unwrap_or("default");
                out.push(Candidate {
                    source: path.display().to_string(),
                    provider: p.into(),
                    model: usable.then(|| format!("{p}/{m}")),
                    note: format!(
                        "Codex model {m}; reasoning {effort}. {}",
                        if usable {
                            "Credentials available"
                        } else {
                            "Codex login is not reused; run chanto login chatgpt"
                        }
                    ),
                    recommended: false,
                });
            }
        }
    }
    let (ollama, studio, cpp) = tokio::join!(
        probe("ollama", "http://localhost:11434"),
        probe("lmstudio", "http://localhost:1234"),
        probe("llamacpp", "http://localhost:8080")
    );
    out.extend(ollama);
    out.extend(studio);
    out.extend(cpp);
    rank(&mut out);
    out
}

pub fn apply(cfg: &mut config::Config, c: &Candidate) {
    cfg.model = c.model.clone();
    if c.note.starts_with("Existing chanto") {
        return;
    }
    match c.provider.as_str() {
        "ollama" => {
            cfg.provider = ProviderKind::Ollama;
            cfg.base_url = Some(c.source.clone());
        }
        "lmstudio" | "llamacpp" => {
            cfg.provider = ProviderKind::Openai;
            cfg.base_url = Some(format!("{}/v1", c.source));
        }
        _ => {
            cfg.provider = ProviderKind::Ollama;
            cfg.base_url = None;
        }
    }
}

pub fn no_candidate(candidates: &[Candidate]) -> String {
    let found = if candidates.is_empty() {
        "nothing".into()
    } else {
        candidates
            .iter()
            .map(|c| {
                format!(
                    "{} ({})",
                    c.model.as_deref().unwrap_or(&c.provider),
                    crate::util::tilde(std::path::Path::new(&c.source))
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "No usable model available.\nFound: {found}\n\
         Run `chanto setup`, `chanto login openrouter`, \
         `export OPENAI_API_KEY=...`, or `ollama pull qwen3-coder`.\n\
         Select explicitly with `chanto -m <provider/model>`."
    )
}

#[cfg(test)]
mod tests;
