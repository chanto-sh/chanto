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
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct Home {
        path: std::path::PathBuf,
        previous: Vec<(String, Option<std::ffi::OsString>)>,
    }
    impl Home {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("chanto-setup-{}", uuid::Uuid::new_v4()));
            let mut vars: Vec<String> = [
                "CHANTO_HOME",
                "CHANTO_COMPAT_HOME",
                "CODEX_HOME",
                "CHANTO_MODEL",
                "CHANTO_PROVIDER",
                "CHANTO_BASE_URL",
                "CHANTO_API_KEY",
                "CHANTO_PROFILE",
                "CHANTO_NO_CLAUDE_CONFIG",
                "CHANTO_NO_CODEX_CONFIG",
            ]
            .map(String::from)
            .to_vec();
            vars.extend(
                providers::CATALOG
                    .iter()
                    .flat_map(|p| p.env.iter().map(|v| v.to_string())),
            );
            vars.extend(crate::cloud::ENV_VARS.iter().map(|v| v.to_string()));
            let previous = vars
                .into_iter()
                .map(|v| {
                    let old = std::env::var_os(&v);
                    unsafe {
                        std::env::remove_var(&v);
                    }
                    (v, old)
                })
                .collect();
            std::fs::create_dir_all(path.join(".chanto")).unwrap();
            std::fs::create_dir_all(path.join(".codex")).unwrap();
            std::fs::create_dir_all(path.join(".claude")).unwrap();
            unsafe {
                std::env::set_var("CHANTO_HOME", path.join(".chanto"));
                std::env::set_var("CHANTO_COMPAT_HOME", &path);
                std::env::set_var("CODEX_HOME", path.join(".codex"));
            }
            Self { path, previous }
        }
    }
    impl Drop for Home {
        fn drop(&mut self) {
            for (v, old) in &self.previous {
                unsafe {
                    if let Some(old) = old {
                        std::env::set_var(v, old);
                    } else {
                        std::env::remove_var(v);
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn detection_order_and_compat_diagnostics_are_redacted() {
        let _lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = Home::new();
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "fake-key-never-emit");
        }
        std::fs::write(home.path.join(".codex/config.toml"), "model = \"codex-model\"\nmodel_provider = \"openai\"\nmodel_reasoning_effort = \"high\"\n").unwrap();
        std::fs::write(home.path.join(".claude/settings.json"), "{broken").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cs = detect().await;
            assert_eq!(cs[0].model.as_deref(), Some("openai/codex-model"));
            assert!(cs[0].recommended && cs[0].note.contains("high"));
            assert_eq!(cs.iter().filter(|c| c.recommended).count(), 1);
            let (cfg, _) = config::Config::load(&home.path, true).unwrap();
            // Layers are shown with the platform's separators.
            assert!(
                cfg.layers
                    .iter()
                    .any(|p| std::path::Path::new(p).ends_with(".codex/config.toml"))
            );
            assert!(
                cfg.diagnostics
                    .iter()
                    .any(|d| d.contains("settings.json") && d.contains("line 1"))
            );
            let mut cfg = cfg;
            cfg.api_key = Some("fake-key-never-emit".into());
            cfg.base_url = Some("http://localhost:1".into());
            let report = crate::doctor::report(&cfg, Some("local")).await;
            let layers = report["layers"].as_array().unwrap();
            assert!(
                layers
                    .iter()
                    .any(|p| std::path::Path::new(p.as_str().unwrap())
                        .ends_with(".codex/config.toml")),
                "{report}"
            );
            let report = report.to_string();
            assert!(!report.contains("fake-key-never-emit"));
            std::fs::write(
                home.path.join(".chanto/config.toml"),
                "model = \"explicit\"\n",
            )
            .unwrap();
            let cs = detect().await;
            assert_eq!(cs[0].model.as_deref(), Some("explicit"));
            let suggestions = crate::onboarding::suggestions(None).await;
            assert!(suggestions.iter().any(|m| m == "explicit"));
            assert!(!suggestions.iter().any(|m| m == "ollama/explicit"));
            unsafe {
                std::env::set_var("CHANTO_NO_CODEX_CONFIG", "1");
                std::env::set_var("CHANTO_NO_CLAUDE_CONFIG", "1");
            }
            let (cfg, _) = config::Config::load(&home.path, true).unwrap();
            assert!(!cfg.compat.codex && !cfg.compat.claude);
            assert!(
                cfg.layers
                    .iter()
                    .all(|p| !p.contains(".codex") && !p.contains(".claude"))
            );
            assert!(
                cfg.diagnostics
                    .iter()
                    .any(|d| d.contains("compat disabled"))
            );
            assert!(detect().await.iter().all(|c| !c.source.contains(".codex")));
        });
    }

    #[test]
    fn missing_login_explains_how_to_sign_in() {
        let _lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _home = Home::new();
        let cfg = crate::config::Config::default();
        let why = missing_login(&cfg, "deepseek/deepseek-chat").unwrap();
        assert!(
            why.contains("No DeepSeek credentials")
                && why.contains("chanto login deepseek")
                && why.contains("DEEPSEEK_API_KEY"),
            "{why}"
        );
        // Plain local names, unknown prefixes and configured providers are left alone.
        assert_eq!(missing_login(&cfg, "qwen3-coder"), None);
        assert_eq!(missing_login(&cfg, "hf.co/some/model"), None);
        let mut named = cfg.clone();
        named
            .providers
            .insert("deepseek".into(), Default::default());
        assert_eq!(missing_login(&named, "deepseek/deepseek-chat"), None);
        // A custom default server names its own models this way.
        let proxy = crate::config::Config {
            base_url: Some("http://localhost:4000/v1".into()),
            ..cfg.clone()
        };
        assert_eq!(missing_login(&proxy, "deepseek/deepseek-chat"), None);
        // With a key, nothing to say.
        unsafe { std::env::set_var("DEEPSEEK_API_KEY", "sk-test") };
        assert_eq!(missing_login(&cfg, "deepseek/deepseek-chat"), None);
    }

    #[tokio::test]
    async fn local_probe_parses_models_and_context_warning() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0; 1024];
            let n = stream.read(&mut buf).await.unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).contains("/api/tags"));
            let body = r#"{"models":[{"name":"generic","details":{"parameter_size":"100B"}},{"name":"qwen3:8b","details":{"parameter_size":"8B"},"modified_at":"2026-01-01"},{"name":"qwen3-coder","num_ctx":8192,"details":{"parameter_size":"32B"}},{"name":"qwen3:8b-new","details":{"parameter_size":"8B"},"modified_at":"2026-02-01"}]}"#;
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        let mut cs = probe("ollama", &format!("http://{addr}")).await;
        assert_eq!(cs.len(), 4);
        assert_eq!(cs[1].model.as_deref(), Some("qwen3:8b-new"));
        assert_eq!(cs[2].model.as_deref(), Some("qwen3:8b"));
        // Pure ranking avoids accessing any real credentials in this test.
        rank(&mut cs);
        assert_eq!(cs[0].model.as_deref(), Some("qwen3-coder"));
        assert!(cs[0].note.contains("<32k"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn explicit_v1_url_is_not_duplicated() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 1024];
            let n = socket.read(&mut buffer).await.unwrap();
            assert!(String::from_utf8_lossy(&buffer[..n]).starts_with("GET /v1/models "));
            let body = r#"{"data":[{"id":"generic"},{"id":"devstral"}]}"#;
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        assert_eq!(
            probe("openai", &url).await[0].model.as_deref(),
            Some("devstral")
        );
        server.await.unwrap();
        assert!(probe("ollama", "http://127.0.0.1:0").await.is_empty());
    }

    #[test]
    fn codex_model_uses_chanto_chatgpt_login() {
        let _lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = Home::new();
        auth::set(
            "chatgpt",
            auth::Credential::ApiKey {
                key: "test-login".into(),
            },
        )
        .unwrap();
        std::fs::write(
            home.path.join(".codex/config.toml"),
            "model = \"gpt-6.1-sol\"\nmodel_reasoning_effort = \"high\"\n",
        )
        .unwrap();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let cs = detect().await;
            assert_eq!(cs[0].model.as_deref(), Some("chatgpt/gpt-6.1-sol"));
            assert!(cs[0].recommended && cs[0].note.contains("high"));
        });
    }

    #[test]
    fn notice_is_one_line_and_explains_setup() {
        let c = Candidate {
            source: "http://localhost:11434".into(),
            provider: "ollama".into(),
            model: Some("qwen3:8b".into()),
            note: String::new(),
            recommended: true,
        };
        assert_eq!(
            selection_notice(&c),
            "chanto: using ollama/qwen3:8b (found on localhost:11434; run \"chanto setup\" to choose)"
        );
    }

    #[test]
    fn noninteractive_no_candidate_has_exact_fixes() {
        let msg = no_candidate(&[]);
        for fix in [
            "chanto setup",
            "chanto login openrouter",
            "export OPENAI_API_KEY=...",
            "ollama pull qwen3-coder",
        ] {
            assert!(msg.contains(fix));
        }
    }
    #[test]
    fn saved_credentials_and_config_opt_out_are_detected() {
        let _lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = Home::new();
        auth::set(
            "openrouter",
            auth::Credential::ApiKey {
                key: "saved-test-secret".into(),
            },
        )
        .unwrap();
        std::fs::write(
            home.path.join(".chanto/config.toml"),
            "[compat]\ncodex = false\nclaude = false\n",
        )
        .unwrap();
        std::fs::write(
            home.path.join(".codex/config.toml"),
            "model = \"foreign\"\n",
        )
        .unwrap();
        std::fs::write(home.path.join(".claude/settings.json"), "{broken").unwrap();
        let trust = serde_json::json!({"projects": {home.path.display().to_string(): {"hasTrustDialogAccepted": true}}});
        std::fs::write(home.path.join(".claude.json"), trust.to_string()).unwrap();
        assert!(!crate::compat::trusted_elsewhere(&home.path));
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let candidates = detect().await;
            let saved = candidates
                .iter()
                .find(|c| c.provider == "openrouter")
                .unwrap();
            assert!(saved.source.ends_with("auth.json"));
            assert!(saved.model.as_deref().unwrap().starts_with("openrouter/"));
            assert!(
                !serde_json::to_string(&candidates)
                    .unwrap()
                    .contains("saved-test-secret")
            );
            assert!(candidates.iter().all(|c| !c.source.contains(".codex")));
            let (cfg, _) = config::Config::load(&home.path, true).unwrap();
            assert!(!cfg.compat.codex && !cfg.compat.claude);
        });
    }

    #[test]
    fn model_save_command_switches_and_preserves_the_default_endpoint() {
        let _lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = Home::new();
        let path = home.path.join(".chanto/config.toml");
        std::fs::write(
            &path,
            "provider = \"openai\"\nbase_url = \"http://localhost:1234/v1\"\n# keep\n",
        )
        .unwrap();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let env = crate::agent::Env::new(crate::agent::EnvInit {
                config: config::Config::default(),
                provider: crate::llm::Provider::new(ProviderKind::Openai, "http://localhost:1/v1", None, 32768),
                model: "old".into(), mode: crate::permissions::Mode::Default, vision: false,
                cwd: home.path.clone(), tools: crate::tools::Registry::builtin(), mcp: vec![], interactive: false,
                frontend: std::sync::Arc::new(crate::plain::PlainFrontend::new(crate::plain::OutputFormat::Json, false)),
            });
            let result = crate::commands::handle_env(&env, "/model --save local/new-model").await;
            assert!(matches!(result, Some(crate::commands::Outcome::Show(s)) if s.contains("Switched model")));
            assert_eq!(env.model(), "local/new-model");
            let text = std::fs::read_to_string(&path).unwrap();
            let t: toml::Table = toml::from_str(&text).unwrap();
            assert_eq!(t["model"].as_str(), Some("local/new-model"));
            assert_eq!(t["base_url"].as_str(), Some("http://localhost:1234/v1"));
            assert!(text.contains("# keep"));
            // Plain /model saves too, as in Claude Code; --session doesn't.
            crate::commands::handle_env(&env, "/model local/plain").await;
            crate::commands::handle_env(&env, "/model --session local/just-now").await;
            assert_eq!(env.model(), "local/just-now");
            let t: toml::Table = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(t["model"].as_str(), Some("local/plain"));
        });
    }

    #[test]
    fn doctor_uses_named_provider_credentials_and_redacts_urls() {
        let _lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = Home::new();
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "env-secret-unused");
        }
        auth::set(
            "openai",
            auth::Credential::ApiKey {
                key: "saved-secret-unused".into(),
            },
        )
        .unwrap();
        let mut cfg = config::Config::default();
        let source = home.path.join(".chanto/config.toml");
        cfg.origins.insert("providers".into(), source.clone());
        cfg.layers.push(source.display().to_string());
        cfg.providers.insert(
            "openai".into(),
            config::ProviderConfig {
                kind: ProviderKind::Openai,
                base_url: "http://user:url-secret@localhost:1/v1?token=query-secret".into(),
                api_key: Some("configured-secret".into()),
                num_ctx: None,
                ..Default::default()
            },
        );
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let report = crate::doctor::report(&cfg, Some("openai/custom")).await;
            assert_eq!(
                report["resolved_model"]["credential_source"],
                source.display().to_string()
            );
            assert_eq!(
                report["resolved_model"]["base_url"],
                "http://localhost:1/v1"
            );
            let text = report.to_string();
            // Name which one leaked, never the value.
            for (what, value) in [
                ("the unused environment key", "env-secret-unused"),
                ("the unused saved key", "saved-secret-unused"),
                ("the configured key", "configured-secret"),
                ("the password in the base URL", "url-secret"),
                ("the token in the base URL's query", "query-secret"),
            ] {
                assert!(!text.contains(value), "doctor exposed {what}");
            }
            assert_eq!(report["settings"]["model"]["source"], "session selection");
        });
    }

    #[test]
    fn configured_catalogs_keep_prefixes_and_report_failures() {
        let _lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _home = Home::new();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/v1", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = [0; 4096];
                let n = socket.read(&mut buffer).await.unwrap();
                assert!(String::from_utf8_lossy(&buffer[..n]).contains("/v1/models"));
                let body = r#"{"data":[{"id":"nested/model"},{"id":"coder"}]}"#;
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            });
            let mut cfg = config::Config::default();
            cfg.providers.insert("custom".into(), config::ProviderConfig {
                kind: ProviderKind::Openai, base_url: url, api_key: None, num_ctx: None, ..Default::default()
            });
            let default = crate::llm::Provider::new(ProviderKind::Openai, "http://localhost:1/v1", None, 32768);
            let (models, failures) = crate::onboarding::available_models(&cfg, default).await;
            assert_eq!(models, ["custom/nested/model", "custom/coder"]);
            assert_eq!(failures.len(), 1);
            server.await.unwrap();
        });
    }

    #[tokio::test]
    async fn stalled_local_probe_is_bounded() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let start = std::time::Instant::now();
        assert!(
            probe("ollama", &format!("http://{address}"))
                .await
                .is_empty()
        );
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cloud_credentials_are_suggested_after_local_models_and_checked_by_doctor() {
        let _lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = Home::new();
        // AWS keys without a region are not enough for a suggestion.
        unsafe {
            std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDNEVEREMIT");
            std::env::set_var("AWS_SECRET_ACCESS_KEY", "secret-never-emit");
        }
        assert!(credential_source("bedrock").is_none());
        // A profile region completes it; Vertex needs ADC and a project.
        std::fs::create_dir_all(home.path.join(".aws")).unwrap();
        std::fs::write(
            home.path.join(".aws/config"),
            "[default]\nregion = us-west-2\n",
        )
        .unwrap();
        assert_eq!(
            credential_source("bedrock").as_deref(),
            Some("env:AWS_ACCESS_KEY_ID")
        );
        assert!(credential_source("vertex").is_none());
        // gcloud's own folder is %APPDATA%\gcloud on Windows; point it at the test home.
        unsafe {
            std::env::set_var("CLOUDSDK_CONFIG", home.path.join(".config/gcloud"));
        }
        let adc = home
            .path
            .join(".config/gcloud/application_default_credentials.json");
        std::fs::create_dir_all(adc.parent().unwrap()).unwrap();
        std::fs::write(&adc, r#"{"type":"authorized_user","client_id":"c","client_secret":"s","refresh_token":"r","quota_project_id":"proj"}"#).unwrap();
        assert!(
            credential_source("vertex")
                .unwrap()
                .starts_with("ADC user credentials")
        );
        assert!(credential_source("azure").is_none() && credential_source("foundry").is_none());
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cs = detect().await;
            let b = cs.iter().find(|c| c.provider == "bedrock").unwrap();
            assert_eq!(
                b.model.as_deref(),
                Some("bedrock/global.anthropic.claude-sonnet-4-5-20250929-v1:0")
            );
            assert!(b.note.contains("not verified"));
            // Ambient credentials alone don't put cloud models in the model list...
            let suggestions = crate::onboarding::suggestions(None).await;
            assert!(
                !suggestions
                    .iter()
                    .any(|s| s.starts_with("vertex/") || s.starts_with("bedrock/")),
                "{suggestions:?}"
            );
            // ...choosing the platform does (Chanto's flag, or Claude Code's).
            unsafe {
                std::env::set_var("CHANTO_USE_BEDROCK", "1");
                std::env::set_var("CLAUDE_CODE_USE_VERTEX", "1");
            }
            let suggestions = crate::onboarding::suggestions(None).await;
            unsafe {
                std::env::remove_var("CHANTO_USE_BEDROCK");
                std::env::remove_var("CLAUDE_CODE_USE_VERTEX");
            }
            assert!(
                suggestions.iter().any(|s| s == "vertex/gemini-2.5-pro")
                    && suggestions
                        .iter()
                        .any(|s| s == "bedrock/openai.gpt-oss-120b-1:0")
            );
            let mut picked = config::Config::default();
            picked.model = Some("bedrock/openai.gpt-oss-120b-1:0".into());
            assert!(
                cloud_opted_in("bedrock", Some(&picked))
                    && !cloud_opted_in("vertex", Some(&picked))
            );
            // A local model outranks ambient cloud credentials.
            let mut ranked = vec![
                b.clone(),
                Candidate {
                    source: "http://localhost:11434".into(),
                    provider: "ollama".into(),
                    model: Some("llama3".into()),
                    note: String::new(),
                    recommended: false,
                },
            ];
            rank(&mut ranked);
            assert_eq!(ranked[0].provider, "ollama");

            // doctor: credential source and region, a request that spends no tokens, no secrets.
            let (base, seen) = crate::llm::mock::serve_bytes(vec![(
                200,
                "application/json",
                br#"{"modelSummaries":[]}"#.to_vec(),
            )])
            .await;
            let mut cfg = config::Config::default();
            cfg.providers.insert(
                "bedrock".into(),
                config::ProviderConfig {
                    kind: ProviderKind::Bedrock,
                    base_url: base,
                    ..Default::default()
                },
            );
            let report = crate::doctor::report(
                &cfg,
                Some("bedrock/us.anthropic.claude-haiku-4-5-20251001-v1:0"),
            )
            .await;
            let r = &report["resolved_model"];
            assert_eq!(r["credential_source"], "env:AWS_ACCESS_KEY_ID");
            assert_eq!(r["region"], "us-west-2 (profile default)");
            assert_eq!(
                (r["reachable"].as_bool(), r["check"].as_str()),
                (Some(true), Some("credentials accepted"))
            );
            assert!(
                seen.lock().unwrap().headers[0]
                    .starts_with("GET /foundation-models?byOutputModality=TEXT ")
            );
            let text = report.to_string() + &crate::doctor::render(&report);
            assert!(!text.contains("AKIDNEVEREMIT") && !text.contains("secret-never-emit"));
            assert!(
                crate::doctor::render(&report).contains("region")
                    && !crate::doctor::render(&report).contains("{\"")
            );
        });
        // `chanto providers` shows where cloud credentials come from.
        let table = auth::providers_table();
        assert!(
            table.contains("bedrock") && table.contains("env:AWS_ACCESS_KEY_ID · region us-west-2"),
            "{table}"
        );
        assert!(table.contains("project proj, global"), "{table}");
    }
}
