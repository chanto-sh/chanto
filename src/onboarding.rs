//! Interactive setup and shared model lists.
use crate::{
    config::{self, Config},
    detect::{self, Candidate},
};
use anyhow::{Context, Result, bail};
use std::{
    io::{IsTerminal, Write},
    path::Path,
};

#[derive(Debug)]
pub struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Setup cancelled. Continue with chanto --model <provider/model>, or run chanto setup later."
        )
    }
}
impl std::error::Error for Cancelled {}

pub fn save_to(
    path: &Path,
    model: &str,
    provider: Option<&str>,
    base_url: Option<&str>,
) -> Result<()> {
    if model.trim().is_empty() {
        bail!("Model cannot be empty; use provider/model or a local model name");
    }
    if provider.is_some_and(|p| crate::llm::ProviderKind::parse(p).is_none()) {
        bail!(
            "Unknown provider; use ollama, openai, anthropic, openai-responses, bedrock, vertex, azure or foundry"
        );
    }
    if let Some(url) = base_url {
        let url = reqwest::Url::parse(url).context("Invalid base URL")?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("Base URL must use http or https");
        }
    }
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .context("Cannot edit invalid chanto config; fix it first")?;
    for (key, value) in [
        ("model", Some(model)),
        ("provider", provider),
        ("base_url", base_url),
    ] {
        if let Some(value) = value {
            let decor = doc
                .get(key)
                .and_then(|v| v.as_value())
                .map(|v| v.decor().clone());
            doc[key] = toml_edit::value(value);
            if let Some(decor) = decor {
                *doc[key].as_value_mut().unwrap().decor_mut() = decor;
            }
        }
    }
    if base_url.is_none() && provider.is_some() {
        doc.remove("base_url");
    }
    std::fs::create_dir_all(path.parent().context("config needs parent directory")?)?;
    let tmp = path.with_extension(format!("toml.{}.tmp", uuid::Uuid::new_v4().simple()));
    std::fs::write(&tmp, doc.to_string())?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

pub fn save(model: &str, provider: Option<&str>, base_url: Option<&str>) -> Result<()> {
    save_to(
        &config::home_dir().join("config.toml"),
        model,
        provider,
        base_url,
    )
}

pub fn save_candidate(c: &Candidate) -> Result<()> {
    let mut cfg = Config::default();
    detect::apply(&mut cfg, c);
    let p = match cfg.provider {
        crate::llm::ProviderKind::Openai => "openai",
        _ => "ollama",
    };
    save(
        c.model.as_deref().context("candidate has no model")?,
        Some(p),
        cfg.base_url.as_deref(),
    )
}

/// Arrow-key setup before the normal TUI starts; restores the terminal on every exit.
pub fn choose(title: &str, labels: &[String], plain: bool) -> Result<usize> {
    if labels.is_empty() {
        bail!("No choices available");
    }
    // Plain mode is the scriptable one (`chanto setup --plain`), so it works
    // with piped answers. Only the raw-mode picker needs a terminal.
    if plain {
        println!("{title}");
        for (i, label) in labels.iter().enumerate() {
            let (headers, item) = split_label(label);
            for h in headers {
                println!("{h}:");
            }
            println!("{}. {item}", i + 1);
        }
        loop {
            print!("Choice [1]: ");
            std::io::stdout().flush()?;
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line)? == 0 {
                return Err(Cancelled.into());
            }
            let n = if line.trim().is_empty() {
                Some(1)
            } else {
                line.trim().parse::<usize>().ok()
            };
            if let Some(n) = n.filter(|n| *n > 0 && *n <= labels.len()) {
                return Ok(n - 1);
            }
        }
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "Interactive setup needs a terminal. Use chanto setup --plain or chanto setup --model <spec>"
        );
    }
    use crossterm::{
        event::{self, Event, KeyCode, KeyModifiers},
        execute, terminal,
    };
    terminal::enable_raw_mode()?;
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
            let _ =
                crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        }
    }
    let _restore = Restore;
    execute!(std::io::stdout(), terminal::EnterAlternateScreen)?;
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
    let mut sel = labels
        .iter()
        .position(|s| s.contains("Recommended: "))
        .unwrap_or(0);
    loop {
        terminal.draw(|f| {
            let area = picker_area(f.area(), labels);
            // Section headers get rows of their own that the cursor skips; long labels wrap.
            let width = area.width.saturating_sub(4) as usize;
            let (mut items, mut rows) = (vec![], vec![]);
            for label in labels {
                let (headers, item) = split_label(label);
                for h in headers {
                    items.push(ratatui::widgets::ListItem::new(ratatui::text::Line::styled(h.to_string(), ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::BOLD | ratatui::style::Modifier::DIM))));
                }
                rows.push(items.len());
                items.push(ratatui::widgets::ListItem::new(crate::tui::markdown::wrap(&ratatui::text::Line::from(item.to_string()), width, 2)));
            }
            let list = ratatui::widgets::List::new(items).block(ratatui::widgets::Block::bordered().title(title).title_bottom("↑/↓ select · Enter confirm · Esc cancel"))
                .highlight_symbol("› ").highlight_style(ratatui::style::Style::default().bg(ratatui::style::Color::DarkGray).fg(ratatui::style::Color::White).add_modifier(ratatui::style::Modifier::BOLD));
            let mut state = ratatui::widgets::ListState::default().with_selected(Some(rows[sel]));
            if title == "Set up chanto" {
                let intro = ratatui::widgets::Paragraph::new("No model is set up yet. Pick one — change it later with /model or `chanto setup`.").wrap(ratatui::widgets::Wrap { trim: true });
                f.render_widget(intro, ratatui::layout::Rect::new(area.x, area.y.saturating_sub(3), area.width, 3));
            }
            f.render_stateful_widget(list, area, &mut state);
        })?;
        if let Event::Key(k) = event::read()? {
            match k.code {
                KeyCode::Up => sel = sel.saturating_sub(1),
                KeyCode::Down => sel = (sel + 1).min(labels.len() - 1),
                KeyCode::Enter => return Ok(sel),
                KeyCode::Esc => return Err(Cancelled.into()),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Err(Cancelled.into());
                }
                _ => {}
            }
        }
    }
}

/// A picker label's section headers (lines before the last) and the choice itself.
fn split_label(label: &str) -> (Vec<&str>, &str) {
    let mut lines: Vec<&str> = label.lines().collect();
    let item = lines.pop().unwrap_or("");
    (lines, item)
}

fn picker_area(screen: ratatui::layout::Rect, labels: &[String]) -> ratatui::layout::Rect {
    let width = (labels
        .iter()
        .flat_map(|s| s.lines())
        .map(|s| s.chars().count())
        .max()
        .unwrap_or(40)
        + 6)
    .max(78)
    .min(screen.width as usize) as u16;
    // Lines too wide for the box wrap (roughly; the list scrolls if it's off by one).
    let inner = (width as usize).saturating_sub(4).max(8);
    let rows: usize = labels
        .iter()
        .flat_map(|s| s.lines())
        .map(|l| l.chars().count().div_ceil(inner).max(1))
        .sum();
    let height = (rows + 2).min(screen.height.saturating_sub(3) as usize) as u16;
    ratatui::layout::Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + (screen.height - height) / 2,
        width,
        height,
    )
}

async fn choose_account_model(mut candidate: Candidate, plain: bool) -> Result<Candidate> {
    if matches!(candidate.provider.as_str(), "chatgpt" | "chanto") && candidate.model.is_none() {
        let (p, name) = if candidate.provider == "chatgpt" {
            ("chatgpt", "ChatGPT")
        } else {
            ("chanto", "chanto cloud")
        };
        let provider = crate::llm::builtin_provider(p, "").with_context(|| {
            format!(
                "Run chanto login {} first",
                if p == "chanto" { "cloud" } else { p }
            )
        })?;
        let models =
            tokio::time::timeout(std::time::Duration::from_secs(10), provider.list_models())
                .await
                .with_context(|| format!("{name} catalog timed out; rerun chanto setup"))??;
        let i = choose(
            &format!("Choose a {name} model from your account"),
            &models,
            plain,
        )?;
        candidate.model = Some(format!("{p}/{}", models[i]));
    }
    Ok(candidate)
}

pub async fn picker(plain: bool) -> Result<Candidate> {
    {
        let candidates = detect::detect().await;
        let mut usable: Vec<_> = candidates
            .into_iter()
            .filter(|c| {
                c.model.is_some()
                    || (matches!(c.provider.as_str(), "chatgpt" | "chanto")
                        && c.note == "Credential available")
            })
            .collect();
        usable.sort_by_key(|c| !matches!(c.provider.as_str(), "ollama" | "lmstudio" | "llamacpp"));
        let mut labels: Vec<_> = usable
            .iter()
            .map(|c| {
                format!(
                    "{}{} — {} ({})",
                    if c.recommended { "Recommended: " } else { "" },
                    c.model.as_deref().unwrap_or(if c.provider == "chanto" {
                        "chanto cloud models"
                    } else {
                        "ChatGPT account catalog"
                    }),
                    c.note,
                    c.source
                )
            })
            .collect();
        labels.extend([
            "Log in with ChatGPT",
            "Log in with OpenRouter",
            "Paste an API key for a provider",
            "OpenRouter's free models — sign in; rate-limited (about 50 requests a day, 1,000 with $10 of credit); their providers may log or train on prompts, so avoid private code",
            "Gemini's free tier — paste a free key from Google AI Studio; Google may use free-tier prompts to improve its products",
            "A model on this machine — free and private (how to install Ollama)",
        ].map(String::from));
        let local_count = usable
            .iter()
            .take_while(|c| matches!(c.provider.as_str(), "ollama" | "lmstudio" | "llamacpp"))
            .count();
        if local_count > 0 {
            // A local server is already here: the last choice is about getting more models.
            *labels.last_mut().unwrap() =
                "Another local model — free and private (how to download more)".into();
        }
        if local_count > 0 {
            labels[0] = format!("Found on this machine\n{}", labels[0]);
        }
        if local_count < usable.len() {
            labels[local_count] = format!("Found credentials\n{}", labels[local_count]);
        }
        labels[usable.len()] = format!("Sign in\n{}", labels[usable.len()]);
        labels[usable.len() + 3] = format!("Free\n{}", labels[usable.len() + 3]);
        if plain {
            println!(
                "No model is set up yet. Pick one — change it later with /model or `chanto setup`."
            );
        }
        let i = choose("Set up chanto", &labels, plain)?;
        if i < usable.len() {
            let c = choose_account_model(usable[i].clone(), plain).await?;
            // Existing defaults retain their provider and endpoint.
            if c.note.starts_with("Existing chanto") {
                return Ok(c);
            }
            save_candidate(&c)?;
            return Ok(c);
        }
        match i - usable.len() {
            0 | 1 => {
                let p = if i == usable.len() {
                    "chatgpt"
                } else {
                    "openrouter"
                };
                crate::auth::cli(&["login".into(), p.into()]).await?;
                let c = Candidate {
                    source: config::home_dir().join("auth.json").display().to_string(),
                    provider: p.into(),
                    model: detect::default_model(p),
                    note: "Signed in".into(),
                    recommended: true,
                };
                let c = choose_account_model(c, plain).await?;
                save_candidate(&c)?;
                Ok(c)
            }
            2 => {
                // Vertex AI has no keys: it uses gcloud's Application Default Credentials.
                let labels = crate::providers::CATALOG
                    .iter()
                    .filter(|h| !matches!(h.id, "chatgpt" | "vertex" | "chanto"))
                    .map(|h| h.id.to_string())
                    .collect::<Vec<_>>();
                let p = &labels[choose("API key provider", &labels, plain)?];
                crate::auth::cli(&["login".into(), p.clone(), "--key".into()]).await?;
                let c = Candidate {
                    source: config::home_dir().join("auth.json").display().to_string(),
                    provider: p.clone(),
                    model: detect::default_model(p),
                    note: "Key saved".into(),
                    recommended: true,
                };
                save_candidate(&c)?;
                Ok(c)
            }
            3 => {
                crate::auth::cli(&["login".into(), "openrouter".into()]).await?;
                let free = openrouter_free_models().await?;
                if free.is_empty() {
                    bail!(
                        "OpenRouter has no free models with tool calling right now; rerun chanto setup and pick another option"
                    );
                }
                let labels: Vec<String> = free.iter().map(|m| m.label()).collect();
                let i = choose(
                    "Choose a free OpenRouter model (only ones that can call tools)",
                    &labels,
                    plain,
                )?;
                let c = Candidate {
                    source: config::home_dir().join("auth.json").display().to_string(),
                    provider: "openrouter".into(),
                    model: Some(format!("openrouter/{}", free[i].id)),
                    note: "Free model".into(),
                    recommended: true,
                };
                save_candidate(&c)?;
                Ok(c)
            }
            4 => {
                println!(
                    "Create a free key at https://aistudio.google.com/apikey, then paste it below."
                );
                crate::auth::cli(&["login".into(), "gemini".into(), "--key".into()]).await?;
                let provider = crate::llm::builtin_provider("gemini", "")
                    .context("The Gemini key wasn't saved; rerun chanto setup")?;
                let mut models: Vec<String> = tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    provider.list_models(),
                )
                .await
                .context("Gemini's model list timed out; rerun chanto setup")??
                .into_iter()
                .map(|m| m.trim_start_matches("models/").to_string())
                .filter(|m| gemini_chat_model(m))
                .collect();
                if models.is_empty() {
                    bail!(
                        "This key lists no Gemini models; check it at https://aistudio.google.com/apikey"
                    );
                }
                // The free tier is mostly the Flash models.
                models.sort_by_key(|m| {
                    (
                        !m.contains("flash"),
                        m.contains("lite"),
                        std::cmp::Reverse(m.clone()),
                    )
                });
                let i = choose(
                    "Choose a Gemini model (the free tier mostly covers Flash)",
                    &models,
                    plain,
                )?;
                let c = Candidate {
                    source: config::home_dir().join("auth.json").display().to_string(),
                    provider: "gemini".into(),
                    model: Some(format!("gemini/{}", models[i])),
                    note: "Free-tier key saved".into(),
                    recommended: true,
                };
                save_candidate(&c)?;
                Ok(c)
            }
            _ => {
                println!(
                    "Install Ollama from https://ollama.com, then run:\n  ollama serve\n  ollama pull qwen3-coder\n  chanto setup"
                );
                bail!("Local setup instructions shown; rerun after installing a model");
            }
        }
    }
}

/// Free OpenRouter models that Chanto has benchmarked, best first: (model id,
/// result on the quick tier). Shown before untested ones while they're free.
const TESTED_FREE: &[(&str, &str)] = &[];

struct FreeModel {
    id: String,
    context: u64,
    until: Option<String>,
    tested: Option<&'static str>,
}

impl FreeModel {
    fn label(&self) -> String {
        let mut s = format!("{} — {}k context", self.id, self.context / 1000);
        if let Some(t) = self.tested {
            s.push_str(&format!(", tested: {t}"));
        }
        if let Some(d) = &self.until {
            s.push_str(&format!(", free until {d}"));
        }
        s
    }
}

/// OpenRouter's free models that can work as an agent: tool calling and at
/// least 64k of context, from its public model list. Benchmarked ones first,
/// then coding models, then larger context.
async fn openrouter_free_models() -> Result<Vec<FreeModel>> {
    let list: serde_json::Value = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        reqwest::get("https://openrouter.ai/api/v1/models")
            .await?
            .error_for_status()?
            .json()
            .await
    })
    .await
    .context("OpenRouter's model list timed out; rerun chanto setup")??;
    Ok(free_agent_models(&list))
}

fn free_agent_models(list: &serde_json::Value) -> Vec<FreeModel> {
    let mut out: Vec<FreeModel> = list["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            let tools = m["supported_parameters"]
                .as_array()
                .is_some_and(|p| p.iter().any(|x| x == "tools"));
            let context = m["context_length"].as_u64().unwrap_or(0);
            (id.ends_with(":free")
                && tools
                && context >= 65_536
                && !id.contains("safety")
                && !id.contains("guard"))
            .then(|| FreeModel {
                id: id.to_string(),
                context,
                until: m["expiration_date"]
                    .as_str()
                    .map(|d| d.get(..10).unwrap_or(d).to_string()),
                tested: TESTED_FREE.iter().find(|(t, _)| *t == id).map(|(_, r)| *r),
            })
        })
        .collect();
    let rank = |m: &FreeModel| {
        TESTED_FREE
            .iter()
            .position(|(t, _)| *t == m.id)
            .unwrap_or(usize::MAX)
    };
    let coding = |m: &FreeModel| {
        ["coder", "code", "devstral", "swe", "laguna"]
            .iter()
            .any(|k| m.id.contains(k))
    };
    out.sort_by_key(|m| {
        (
            rank(m),
            !coding(m),
            std::cmp::Reverse(m.context),
            m.id.clone(),
        )
    });
    out
}

/// Gemini models that can hold a conversation, not embedding, speech or image ones.
fn gemini_chat_model(m: &str) -> bool {
    m.starts_with("gemini")
        && !["embedding", "tts", "image", "audio", "live", "native"]
            .iter()
            .any(|k| m.contains(k))
}

/// Provider catalogs share timeouts, prefixes and failure reporting in every frontend.
pub async fn available_models(
    cfg: &Config,
    default: crate::llm::Provider,
) -> (Vec<String>, Vec<String>) {
    use crate::llm::Provider;
    let mut failures = Vec::new();
    let mut out = Vec::new();
    let mut named: Vec<(String, Provider)> = cfg
        .providers
        .iter()
        .map(|(name, pc)| (name.clone(), pc.provider(32768)))
        .collect();
    named.push((String::new(), default.clone()));
    for h in crate::providers::CATALOG {
        let b = h.id;
        // Cloud platforms are listed only when configured: ambient AWS or
        // Google credentials are often for something else.
        if (cfg.allows_builtin_hosted() || !matches!(b, "openai" | "anthropic"))
            && !cfg.providers.contains_key(b)
            && !h.kind.is_cloud()
            && let Some(p) = crate::llm::builtin_provider(b, "")
        {
            named.push((b.to_string(), p));
        }
    }
    let results = futures::future::join_all(named.into_iter().map(|(name, p)| async move {
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(10), p.list_models()).await;
        (name, result)
    }))
    .await;
    for (name, result) in results {
        // The default server has no provider name: call it by its address.
        let label = if name.is_empty() {
            cfg.base_url()
        } else {
            name.clone()
        };
        match result {
            Ok(Ok(ms)) if !ms.is_empty() => out.extend(ms.into_iter().map(|m| {
                if name.is_empty() {
                    m
                } else {
                    format!("{name}/{m}")
                }
            })),
            Ok(Ok(_))
                if !cfg
                    .providers
                    .get(&name)
                    .is_some_and(|pc| pc.kind.is_cloud()) =>
            {
                failures.push(format!("{label} returned no models"))
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                let e = format!("{e:#}");
                let why = if crate::llm::connection_refused(&e) {
                    "not running".to_string()
                } else {
                    crate::util::one_line(crate::llm::root_cause(&e), 120)
                };
                failures.push(format!("{label}: {why}"))
            }
            Err(_) => failures.push(format!("{label}: timed out")),
        }
    }
    (out, failures)
}

/// Where the model list is remembered between sessions, per set of providers.
fn models_cache() -> std::path::PathBuf {
    config::home_dir().join("cache").join("models.json")
}

/// What decides the list: each configured provider's kind and server, the
/// default server, and which built-in providers have credentials (logging in to
/// one adds its models).
fn models_cache_key(env: &crate::agent::Env) -> String {
    let providers: Vec<String> = env
        .config
        .providers
        .iter()
        .map(|(n, p)| format!("{n}={:?}@{}", p.kind, p.base_url))
        .collect();
    let logged_in: Vec<&str> = crate::providers::CATALOG
        .iter()
        .map(|h| h.id)
        .filter(|id| {
            detect::credential_source(id).is_some() && detect::cloud_opted_in(id, Some(&env.config))
        })
        .collect();
    format!(
        "{providers:?} {:?}@{} {logged_in:?}",
        env.provider().kind,
        env.provider().base_url
    )
}

/// The model list from the last fetch, in this session or an earlier one, and
/// how old it is: the picker opens with it at once instead of waiting on every
/// provider's server.
pub fn cached_models(env: &crate::agent::Env) -> Option<(Vec<String>, std::time::Duration)> {
    let all: serde_json::Value =
        serde_json::from_slice(&std::fs::read(models_cache()).ok()?).ok()?;
    let e = &all[models_cache_key(env)];
    let models: Vec<String> = serde_json::from_value(e["models"].clone()).ok()?;
    let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(e["at"].as_u64()?);
    Some((models, at.elapsed().unwrap_or_default()))
}

/// Fetch the model list and remember it. `quiet` skips the warning about
/// providers that couldn't list models (for background refreshes).
pub async fn refresh_models(env: &crate::agent::Env, quiet: bool) -> Vec<String> {
    let (listed, failures) = available_models(&env.config, env.provider()).await;
    if !quiet && !failures.is_empty() {
        env.emit(crate::agent::Event::Warn(format!(
            "Some providers could not list models — {}",
            failures.join("; ")
        )));
    }
    let fetched = !listed.is_empty();
    let mut out = listed;
    out.extend(suggestions(Some(&env.config)).await);
    out.sort();
    out.dedup();
    // The default provider's models are listed bare; when that's the local
    // Ollama, its `ollama/<m>` suggestions would list each model twice.
    let p = env.provider();
    if p.kind == crate::llm::ProviderKind::Ollama
        && p.base_url.trim_end_matches('/') == "http://localhost:11434"
    {
        let all = out.clone();
        out.retain(|m| {
            m.strip_prefix("ollama/")
                .is_none_or(|bare| !all.iter().any(|x| x == bare))
        });
    }
    // Offline, or every provider failed: keep the last good list rather than
    // replacing it with suggestions alone.
    if fetched || failures.is_empty() {
        let path = models_cache();
        let mut all: serde_json::Value = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        all[models_cache_key(env)] = serde_json::json!({"at": now, "models": out});
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Write then rename, so a reader (another session) never sees half a file.
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        if std::fs::write(&tmp, all.to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
    out
}

pub async fn suggestions(cfg: Option<&Config>) -> Vec<String> {
    let mut out = Vec::new();
    let allowed = |p: &str| {
        cfg.is_none_or(|c| {
            c.allows_builtin_hosted()
                || !matches!(p, "openai" | "anthropic")
                || c.providers.contains_key(p)
        })
    };
    for c in detect::detect().await {
        if !c.note.starts_with("Existing chanto") && !allowed(&c.provider) {
            continue;
        }
        if !c.note.starts_with("Existing chanto") && !detect::cloud_opted_in(&c.provider, cfg) {
            continue;
        }
        if let Some(m) = c.model {
            out.push(
                if c.source.starts_with("http:")
                    && matches!(c.provider.as_str(), "ollama" | "lmstudio" | "llamacpp")
                {
                    format!("{}/{m}", c.provider)
                } else {
                    m
                },
            );
        }
    }
    for (p, m) in detect::RECOMMENDED {
        if allowed(p)
            && detect::cloud_opted_in(p, cfg)
            && (detect::credential_source(p).is_some()
                || cfg.is_some_and(|c| c.providers.contains_key(*p)))
        {
            out.push(format!("{p}/{m}"));
        }
    }
    // Coding plans: the models their docs name.
    for h in crate::providers::CATALOG
        .iter()
        .filter(|h| !crate::providers::plan_models(h.id).is_empty())
    {
        if allowed(h.id) && detect::credential_source(h.id).is_some() {
            out.extend(
                crate::providers::plan_models(h.id)
                    .iter()
                    .map(|m| format!("{}/{m}", h.id)),
            );
        }
    }
    // Cloud platforms: a few common models (Azure: the configured deployment).
    for h in crate::providers::CATALOG
        .iter()
        .filter(|h| h.kind.is_cloud())
    {
        if detect::cloud_opted_in(h.id, cfg)
            && (detect::credential_source(h.id).is_some()
                || cfg.is_some_and(|c| c.providers.contains_key(h.id)))
        {
            out.extend(
                crate::providers::cloud_models(h.id)
                    .iter()
                    .map(|m| format!("{}/{m}", h.id)),
            );
            out.extend(detect::default_model(h.id));
        }
    }
    out.sort();
    out.dedup();
    out
}

pub async fn cli(args: &[String]) -> Result<String> {
    #[derive(clap::Parser)]
    struct Args {
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        plain: bool,
    }
    let a = <Args as clap::Parser>::try_parse_from(
        std::iter::once("chanto setup".to_string()).chain(args.iter().cloned()),
    )?;
    let model = if let Some(m) = a.model {
        save(&m, a.provider.as_deref(), a.base_url.as_deref())?;
        m
    } else {
        let c = picker(a.plain).await?;
        crate::feedback::ask_in_setup();
        c.model
            .clone()
            .or_else(|| detect::default_model(&c.provider))
            .unwrap_or_else(|| c.provider.clone())
    };
    // A clear finish: confirm the connection and model, how to change it, and the
    // first thing to try.
    let line = match verify_connection(&model).await {
        Verify::Ok => format!("✓ Connected. Using {model}."),
        Verify::ModelMissing(models) => {
            let hint = if models.is_empty() {
                "Run `chanto models` to see what's installed".to_string()
            } else {
                format!(
                    "Try {}",
                    models
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            format!("Connected, but {model} isn't available. {hint}, or run `chanto setup`.")
        }
        Verify::Unreachable => format!(
            "Using {model}. Saved the default, but couldn't reach the provider just now. \
             Check the key or network, then run `chanto doctor`."
        ),
        Verify::Unknown => format!("✓ Ready. Using {model}."),
    };
    Ok(format!(
        "{line}\n  Change it anytime with /model or `chanto setup`.\n  \
         Start a task:  chanto \"<what you want done>\""
    ))
}

/// Outcome of the post-setup connection check.
enum Verify {
    /// Reachable, and the chosen model is in the provider's list.
    Ok,
    /// Reachable, but the model isn't available (with the models that are).
    ModelMissing(Vec<String>),
    /// Couldn't reach the provider.
    Unreachable,
    /// Couldn't even attempt the check.
    Unknown,
}

/// Best-effort check after setup: list the provider's models and confirm the
/// chosen one is there. Catches a bad key or endpoint, and a model that isn't
/// pulled or available on the plan.
async fn verify_connection(spec: &str) -> Verify {
    let Some(cwd) = std::env::current_dir().ok() else {
        return Verify::Unknown;
    };
    let Ok((cfg, _)) = crate::config::Config::load(&cwd, crate::config::is_trusted(&cwd)) else {
        return Verify::Unknown;
    };
    let (provider, model, _name) = crate::doctor::resolve(&cfg, spec);
    match tokio::time::timeout(std::time::Duration::from_secs(8), provider.list_models()).await {
        Ok(Ok(models)) => {
            let short = model
                .rsplit('/')
                .next()
                .unwrap_or(model.as_str())
                .to_string();
            let found = models.iter().any(|m| {
                m == &model
                    || m == &short
                    || m.starts_with(short.as_str())
                    || short.starts_with(m.as_str())
            });
            if found {
                Verify::Ok
            } else {
                Verify::ModelMissing(models)
            }
        }
        _ => Verify::Unreachable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn free_models_are_filtered_for_agents() {
        let list = serde_json::json!({"data": [
            {"id": "a/big:free", "context_length": 1_000_000, "supported_parameters": ["tools"]},
            {"id": "b/coder:free", "context_length": 128_000, "supported_parameters": ["tools", "temperature"], "expiration_date": "2026-11-01T00:00:00Z"},
            {"id": "c/no-tools:free", "context_length": 200_000, "supported_parameters": ["temperature"]},
            {"id": "d/tiny-ctx:free", "context_length": 8_192, "supported_parameters": ["tools"]},
            {"id": "e/content-safety:free", "context_length": 128_000, "supported_parameters": ["tools"]},
            {"id": "f/paid", "context_length": 200_000, "supported_parameters": ["tools"]},
        ]});
        let got = free_agent_models(&list);
        assert_eq!(
            got.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["b/coder:free", "a/big:free"]
        );
        assert_eq!(
            got[0].label(),
            "b/coder:free — 128k context, free until 2026-11-01"
        );
        assert!(
            gemini_chat_model("gemini-3-flash")
                && !gemini_chat_model("gemini-embedding-001")
                && !gemini_chat_model("gemini-2.5-flash-preview-tts")
        );
    }

    #[test]
    fn picker_is_centered_and_fits_content() {
        let area = picker_area(
            ratatui::layout::Rect::new(0, 0, 160, 50),
            &["model".into(), "Sign in\nChatGPT".into()],
        );
        assert_eq!(area.width, 78);
        assert_eq!(area.height, 5);
        assert_eq!(area.x, 41);
        assert_eq!(area.y, 22);
        // A label wider than the screen wraps instead of being cut off.
        let long = "x ".repeat(100);
        assert_eq!(
            picker_area(ratatui::layout::Rect::new(0, 0, 100, 50), &[long]).height,
            5
        );
        assert_eq!(split_label("Free\nGemini"), (vec!["Free"], "Gemini"));
        assert_eq!(split_label("model"), (vec![], "model"));
    }

    #[test]
    fn saving_default_preserves_keys_comments_and_tables() {
        let dir = std::env::temp_dir().join(format!("chanto-save-{}", uuid::Uuid::new_v4()));
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "# my config\nmodel = \"old\" # keep this comment\nnum_ctx = 65536\n[compat]\nclaude = false\n").unwrap();
        save_to(&path, "groq/openai/gpt-oss-120b", None, None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let table: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(table["model"].as_str(), Some("groq/openai/gpt-oss-120b"));
        assert_eq!(table["num_ctx"].as_integer(), Some(65536));
        assert_eq!(table["compat"]["claude"].as_bool(), Some(false));
        assert!(text.contains("# my config") && text.contains("# keep this comment"));
        save_to(
            &path,
            "local",
            Some("openai"),
            Some("http://localhost:1234/v1"),
        )
        .unwrap();
        save_to(&path, "openai/gpt-5", Some("ollama"), None).unwrap();
        let table: toml::Table = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(!table.contains_key("base_url"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod setup_tests;
