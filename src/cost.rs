//! Content-free accounting and opt-in OTLP/HTTP export.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cached_input: Option<f64>,
    pub cache_write: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Telemetry {
    pub otlp_endpoint: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub service_name: Option<String>,
}

/// Cloud platforms price models like their first-party APIs where they host
/// the same model: Claude on Bedrock, Vertex AI and Foundry as Anthropic,
/// Azure OpenAI deployments named after a model as OpenAI. Regional-endpoint
/// and data-zone premiums are not modeled. `None` for other cloud models.
fn cloud_alias(provider: &str, model: &str) -> Option<(String, Vec<String>)> {
    let m = model.to_ascii_lowercase();
    match provider {
        "azure" => Some(("openai".into(), vec![model.to_string()])),
        "bedrock" | "vertex" | "foundry" if m.contains("claude") => {
            // us.anthropic.claude-sonnet-4-5-20250929-v1:0, claude-sonnet-4-5@20250929, …
            let mut name = &m[m.find("claude").unwrap()..];
            name = name.split('@').next().unwrap_or(name);
            if let Some(i) = name.rfind("-v")
                && name[i + 2..]
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == ':')
            {
                name = &name[..i];
            }
            let undated = match name.rsplit_once('-') {
                Some((head, date))
                    if date.len() == 8 && date.chars().all(|c| c.is_ascii_digit()) =>
                {
                    head
                }
                _ => name,
            };
            Some((
                "anthropic".into(),
                vec![name.to_string(), undated.to_string()],
            ))
        }
        _ => None,
    }
}

/// Where the current price list is published, so prices can change without a release.
pub const PRICES_URL: &str = concat!(site_url!(), "/prices.json");

/// The downloaded price list (see [`start_price_refresh`]), keyed like the
/// built-in table: `provider/model`, or the bare id for OpenRouter.
static PUBLISHED: std::sync::RwLock<Option<BTreeMap<String, Price>>> = std::sync::RwLock::new(None);

#[derive(Deserialize)]
struct PriceList {
    prices: BTreeMap<String, Price>,
}

/// Chanto cloud's prices for `chanto/<model>` (what the org is charged),
/// from its gateway catalog: see [`refresh_gateway_prices`].
static GATEWAY: std::sync::RwLock<Option<BTreeMap<String, Price>>> = std::sync::RwLock::new(None);
static GATEWAY_REFRESH: std::sync::Once = std::sync::Once::new();

fn gateway_cache() -> std::path::PathBuf {
    crate::config::home_dir()
        .join("cache")
        .join("gateway-prices.json")
}

/// The gateway catalog (`GET /v1/gateway/models`) as prices by model id.
fn load_gateway_prices(bytes: &[u8]) -> Option<BTreeMap<String, Price>> {
    #[derive(Deserialize)]
    struct Model {
        id: String,
        input_usd_per_mtok: f64,
        output_usd_per_mtok: f64,
        #[serde(default)]
        cache_read_usd_per_mtok: Option<f64>,
    }
    #[derive(Deserialize)]
    struct List {
        models: Vec<Model>,
    }
    let list: List = serde_json::from_slice(bytes).ok()?;
    let ok = |v: f64| v.is_finite() && v >= 0.;
    Some(
        list.models
            .into_iter()
            .filter(|m| {
                ok(m.input_usd_per_mtok)
                    && ok(m.output_usd_per_mtok)
                    && m.cache_read_usd_per_mtok.is_none_or(ok)
            })
            .map(|m| {
                (
                    m.id,
                    Price {
                        input: m.input_usd_per_mtok,
                        output: m.output_usd_per_mtok,
                        cached_input: m.cache_read_usd_per_mtok,
                        cache_write: None,
                    },
                )
            })
            .collect(),
    )
}

/// Prices `chanto/<model>` from the last fetched catalog now, and fetches it
/// again from `url` once per process in the background (when there's a runtime).
pub fn refresh_gateway_prices(url: &str, token: &str) {
    GATEWAY_REFRESH.call_once(|| {
        let path = gateway_cache();
        if let Some(list) = std::fs::read(&path)
            .ok()
            .as_deref()
            .and_then(load_gateway_prices)
        {
            *GATEWAY.write().unwrap() = Some(list);
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (url, token) = (
            format!("{}/v1/gateway/models", url.trim_end_matches('/')),
            token.to_string(),
        );
        rt.spawn(async move {
            let client = crate::http::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .ok()?;
            let bytes = client
                .get(&url)
                .bearer_auth(&token)
                .send()
                .await
                .ok()?
                .error_for_status()
                .ok()?
                .bytes()
                .await
                .ok()?;
            let list = load_gateway_prices(&bytes)?;
            let _ = std::fs::create_dir_all(path.parent()?);
            let _ = std::fs::write(&path, &bytes);
            *GATEWAY.write().unwrap() = Some(list);
            Some(())
        });
    });
}

fn gateway(provider: &str, model: &str) -> Option<Price> {
    if provider != crate::chanto_cloud::PROVIDER_ID {
        return None;
    }
    GATEWAY.read().ok()?.as_ref()?.get(model).cloned()
}

fn published(provider: &str, model: &str) -> Option<Price> {
    let key = if provider == "openrouter" {
        model.to_string()
    } else {
        format!("{provider}/{model}")
    };
    PUBLISHED.read().ok()?.as_ref()?.get(&key).cloned()
}

fn prices_cache() -> std::path::PathBuf {
    crate::config::home_dir().join("cache").join("prices.json")
}

fn load_price_list(bytes: &[u8]) -> Option<BTreeMap<String, Price>> {
    let list: PriceList = serde_json::from_slice(bytes).ok()?;
    // A bad entry must not price a model at nothing.
    Some(
        list.prices
            .into_iter()
            .filter(|(_, p)| {
                p.input.is_finite() && p.output.is_finite() && p.input >= 0. && p.output >= 0.
            })
            .collect(),
    )
}

/// Use the last downloaded price list now, and refresh it in the background
/// at most once a day (`network`), under the same opt-outs as the update
/// check. The request carries nothing about the user.
pub fn start_price_refresh(enabled: bool, network: bool) {
    if !enabled
        || std::env::var_os("CI").is_some()
        || std::env::var("CHANTO_NO_UPDATE_CHECK").is_ok_and(|v| v == "1")
    {
        return;
    }
    let path = prices_cache();
    let cached = std::fs::read(&path).ok();
    if let Some(list) = cached.as_deref().and_then(load_price_list) {
        *PUBLISHED.write().unwrap() = Some(list);
    }
    let fresh = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < std::time::Duration::from_secs(86_400));
    if !network || fresh {
        return;
    }
    tokio::spawn(async move {
        let client = crate::http::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .ok()?;
        let bytes = client
            .get(PRICES_URL)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .bytes()
            .await
            .ok()?;
        let list = load_price_list(&bytes)?;
        let _ = std::fs::create_dir_all(path.parent()?);
        let _ = std::fs::write(&path, &bytes);
        *PUBLISHED.write().unwrap() = Some(list);
        Some(())
    });
}

fn bundled(provider: &str, model: &str) -> Option<Price> {
    if let Some((p, names)) = cloud_alias(provider, model) {
        return names.iter().find_map(|n| bundled(&p, n));
    }
    // Bedrock IDs carry a cross-region prefix that doesn't change the price.
    if provider == "bedrock"
        && let Some(rest) = [
            "us.", "eu.", "apac.", "global.", "jp.", "au.", "ca.", "us-gov.",
        ]
        .iter()
        .find_map(|p| model.strip_prefix(p))
    {
        return bundled(provider, rest);
    }
    let model = if provider == "vertex" {
        model.strip_prefix("google/").unwrap_or(model)
    } else {
        model
    };
    if matches!(provider, "local" | "ollama" | "lmstudio" | "llamacpp") {
        return Some(Price {
            input: 0.,
            output: 0.,
            cached_input: Some(0.),
            cache_write: Some(0.),
        });
    }
    // Exact identifiers only: a new model must never inherit another model's price.
    let key = if provider == "openrouter" {
        model.to_string()
    } else {
        format!("{provider}/{model}")
    };
    let (input, cached, output, write) = match key.as_str() {
        "openai/gpt-4o" => (2.5, 1.25, 10., 2.5),
        "openai/gpt-4o-mini" => (0.15, 0.075, 0.6, 0.15),
        "openai/gpt-4.1" => (2., 0.5, 8., 2.),
        "openai/gpt-4.1-mini" => (0.4, 0.1, 1.6, 0.4),
        "openai/gpt-4.1-nano" => (0.1, 0.025, 0.4, 0.1),
        "openai/gpt-5" => (1.25, 0.125, 10., 1.25),
        "openai/gpt-5-mini" => (0.25, 0.025, 2., 0.25),
        "anthropic/claude-sonnet-4-5"
        | "anthropic/claude-sonnet-4-6"
        | "anthropic/claude-sonnet-4-20250514" => (3., 0.3, 15., 3.75),
        // Current defaults (October 2026), from Anthropic's, OpenAI's and Google's pricing
        // pages. Gemini 3.8 Flash doubles on 2027-01-01; prices.json carries changes like that.
        "anthropic/claude-sonnet-5-5" => (2., 0.1, 10., 2.5),
        "anthropic/claude-opus-5-5" => (4., 0.2, 20., 5.),
        "anthropic/claude-haiku-5-5" => (0.1, 0.01, 0.5, 0.125),
        "openai/gpt-6.1-sol" => (2., 0.1, 10., 2.),
        "openai/gpt-6-sol" => (2., 0.2, 10., 2.),
        "openai/gpt-6-astra" => (10., 1., 50., 10.),
        "openai/gpt-6-luna" => (0.1, 0.01, 0.5, 0.1),
        "gemini/gemini-3.8-flash" | "google/gemini-3.8-flash" => (0.75, 0.075, 3.75, 0.75),
        "mistral/mistral-medium-latest" => (1.5, 1.5, 7.5, 1.5),
        "anthropic/claude-opus-4-5" => (5., 0.5, 25., 6.25),
        "anthropic/claude-haiku-4-5" => (1., 0.1, 5., 1.25),
        "anthropic/claude-opus-4-6" => (5., 0.5, 25., 6.25),
        "anthropic/claude-opus-4-1" | "anthropic/claude-opus-4" => (15., 1.5, 75., 18.75),
        "anthropic/claude-sonnet-4" | "anthropic/claude-3-7-sonnet" => (3., 0.3, 15., 3.75),
        "anthropic/claude-3-5-haiku" => (0.8, 0.08, 4., 1.),
        "vertex/gemini-2.5-pro" => (1.25, 0.125, 10., 1.25),
        "vertex/gemini-2.5-flash" => (0.3, 0.03, 2.5, 0.3),
        "bedrock/openai.gpt-oss-120b-1:0" => (0.15, 0.15, 0.6, 0.15),
        "bedrock/openai.gpt-oss-20b-1:0" => (0.07, 0.07, 0.3, 0.07),
        "bedrock/amazon.nova-pro-v1:0" => (0.8, 0.2, 3.2, 0.8),
        "bedrock/amazon.nova-lite-v1:0" => (0.06, 0.015, 0.24, 0.06),
        "bedrock/amazon.nova-micro-v1:0" => (0.035, 0.00875, 0.14, 0.035),
        "bedrock/meta.llama3-3-70b-instruct-v1:0" => (0.72, 0.72, 0.72, 0.72),
        "bedrock/deepseek.r1-v1:0" => (1.35, 1.35, 5.4, 1.35),
        "gemini/gemini-2.0-flash" | "google/gemini-2.0-flash-001" => (0.1, 0.025, 0.4, 0.1),
        "gemini/gemini-2.5-flash" | "google/gemini-2.5-flash" => (0.3, 0.03, 2.5, 0.3),
        "deepseek/deepseek-chat" => (0.28, 0.028, 0.42, 0.28),
        // DeepSeek's peak-hour rates (October 2026); off-peak is half, so this is an upper bound.
        "deepseek/deepseek-flash" | "deepseek/deepseek-v4-flash" => (0.3, 0.006, 1.2, 0.3),
        "deepseek/deepseek-v4-pro" => (1.32, 0.044, 3.96, 1.32),
        "xai/grok-3" | "x-ai/grok-3" => (3., 0.75, 15., 3.),
        "mistral/mistral-small-2503" => (0.1, 0.1, 0.3, 0.1),
        "groq/openai/gpt-oss-120b" => (0.15, 0.15, 0.6, 0.15),
        "groq/openai/gpt-oss-20b" => (0.075, 0.075, 0.3, 0.075),
        "fireworks/accounts/fireworks/models/gpt-oss-120b" => (0.15, 0.015, 0.6, 0.15),
        "opencode/minimax-m2.7" => (0.3, 0.06, 1.2, 0.3),
        "opencode/claude-sonnet-4-6" => (3., 0.3, 15., 3.75),
        "dashscope/qwen-plus-2025-01-25" => (0.4, 0.4, 1.2, 0.4),
        "moonshot/kimi-k2-turbo" | "moonshot/kimi-k2-thinking-turbo" => (1.15, 0.15, 8., 1.15),
        "baseten/openai/gpt-oss-120b" => (0.1, 0.1, 0.5, 0.1),
        "vercel/openai/gpt-4.1" => (2., 0.5, 8., 2.),
        "cerebras/gpt-oss-120b" => (0.35, 0.35, 0.75, 0.35),
        "together/meta-llama/Llama-3.3-70B-Instruct-Turbo" => (1.04, 1.04, 1.04, 1.04),
        "minimax/MiniMax-M2" | "minimax/MiniMax-M2.1" | "minimax/MiniMax-M2.5" => {
            (0.3, 0.03, 1.2, 0.375)
        }
        "minimax/MiniMax-M2.7" => (0.3, 0.06, 1.2, 0.375),
        _ => return None,
    };
    Some(Price {
        input,
        output,
        cached_input: Some(cached),
        cache_write: Some(write),
    })
}

pub fn price(
    provider: &str,
    model: &str,
    usage: &crate::llm::Usage,
    overrides: &BTreeMap<String, Price>,
) -> Option<f64> {
    if let Some(c) = usage.cost.filter(|c| c.is_finite() && *c >= 0.) {
        return Some(c);
    }
    let custom = overrides.get(&format!("{provider}/{model}"));
    let mut p = custom
        .cloned()
        .or_else(|| gateway(provider, model))
        .or_else(|| published(provider, model))
        .or_else(|| bundled(provider, model))?;
    if custom.is_none()
        && usage.prompt_tokens > 200_000
        && matches!(
            model,
            "claude-sonnet-4-5"
                | "claude-sonnet-4-20250514"
                | "anthropic/claude-sonnet-4-5"
                | "anthropic/claude-sonnet-4-20250514"
        )
    {
        p.input *= 2.;
        p.output *= 1.5;
        p.cached_input = p.cached_input.map(|c| c * 2.);
        p.cache_write = p.cache_write.map(|c| c * 2.);
    }
    // GPT-6 models: prompts over 272k tokens cost twice as much, output 1.5x (OpenAI's pricing page).
    if custom.is_none()
        && usage.prompt_tokens > 272_000
        && matches!(
            model.trim_start_matches("openai/"),
            "gpt-6.1-sol" | "gpt-6-sol" | "gpt-6-astra" | "gpt-6-luna"
        )
    {
        p.input *= 2.;
        p.output *= 1.5;
        p.cached_input = p.cached_input.map(|c| c * 2.);
        p.cache_write = p.cache_write.map(|c| c * 2.);
    }
    // Haiku 5.5: every rate is 5x for prompts over 100k tokens (Anthropic's pricing page).
    if custom.is_none()
        && usage.prompt_tokens > 100_000
        && matches!(model, "claude-haiku-5-5" | "anthropic/claude-haiku-5-5")
    {
        p.input *= 5.;
        p.output *= 5.;
        p.cached_input = p.cached_input.map(|c| c * 5.);
        p.cache_write = p.cache_write.map(|c| c * 5.);
    }
    if [
        p.input,
        p.output,
        p.cached_input.unwrap_or(p.input),
        p.cache_write.unwrap_or(p.input),
    ]
    .iter()
    .any(|v| !v.is_finite() || *v < 0.)
    {
        return None;
    }
    let cached = usage.cached_tokens.min(usage.prompt_tokens);
    let write = usage.cache_write_tokens.min(usage.prompt_tokens - cached);
    Some(
        ((usage.prompt_tokens - cached - write) as f64 * p.input
            + cached as f64 * p.cached_input.unwrap_or(p.input)
            + write as f64 * p.cache_write.unwrap_or(p.input)
            + usage.completion_tokens as f64 * p.output)
            / 1_000_000.,
    )
}

static DEBUG: AtomicBool = AtomicBool::new(false);
pub fn set_debug(debug: bool) {
    DEBUG.store(
        debug || std::env::var("CHANTO_LOG").as_deref() == Ok("debug"),
        Ordering::Relaxed,
    );
}
fn day() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}
fn log_path(root: &Path) -> PathBuf {
    root.join("logs").join(format!("chanto-{}.log", day()))
}

#[derive(Default)]
struct State {
    total: f64,
    unknown: bool,
    /// The highest budget note shown (50 or 80, % of the tightest budget).
    warned: u8,
    continued: bool,
}
pub struct Accounting {
    root: PathBuf,
    session: Mutex<String>,
    prices: BTreeMap<String, Price>,
    session_limit: Option<f64>,
    daily_limit: Option<f64>,
    /// `[spend] month_usd`: across sessions this calendar month.
    monthly_limit: Option<f64>,
    /// This month's spend in other saved sessions: (month, session id, USD),
    /// read once per session.
    month_base: Mutex<Option<(String, String, f64)>>,
    telemetry: Telemetry,
    state: Mutex<State>,
}
impl Accounting {
    pub fn new(config: &crate::config::Config) -> Self {
        Self {
            root: crate::config::home_dir(),
            session: Mutex::new(uuid::Uuid::new_v4().to_string()),
            prices: config.pricing.clone(),
            // `max_cost_usd` and `max_cost_usd_per_day` are the same budgets as
            // `[spend] session_usd` and `day_usd`, and win when both are set.
            session_limit: config.max_cost_usd.or(config.spend.session_usd),
            daily_limit: config.max_cost_usd_per_day.or(config.spend.day_usd),
            monthly_limit: config.spend.month_usd,
            month_base: Mutex::new(None),
            telemetry: config.telemetry.clone(),
            state: Mutex::new(State::default()),
        }
    }
    pub fn set_session(&self, id: &str) {
        *self.session.lock().unwrap() = id.into();
    }
    pub fn restore(&self, total: f64, unknown: bool) {
        let mut s = self.state.lock().unwrap();
        s.total = total;
        s.unknown = unknown;
        s.continued = false;
        s.warned = 0;
    }
    pub fn has_override(&self, provider: &str, model: &str) -> bool {
        self.prices.contains_key(&format!("{provider}/{model}"))
    }
    /// What `usage` would cost on `provider/model`, and where the price came
    /// from: `configured` (your pricing), `catalog`, or `default` (unknown).
    pub fn estimate(
        &self,
        provider: &str,
        model: &str,
        usage: &crate::llm::Usage,
    ) -> (Option<f64>, &'static str) {
        let cost = price(provider, model, usage, &self.prices);
        let kind = if self.has_override(provider, model) {
            "configured"
        } else if cost.is_some() {
            "catalog"
        } else {
            "default"
        };
        (cost, kind)
    }

    pub fn total(&self) -> f64 {
        self.state.lock().unwrap().total
    }
    pub fn unknown(&self) -> bool {
        self.state.lock().unwrap().unknown
    }
    pub fn summary(&self) -> String {
        format!(
            "{}{}",
            crate::util::usd(self.total()),
            if self.unknown() {
                " + price unknown"
            } else {
                ""
            }
        )
    }
    fn daily(&self) -> f64 {
        std::fs::read_to_string(log_path(&self.root))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|v| v["event"] == "model_call")
            .filter_map(|v| v["data"]["cost"].as_f64())
            .sum()
    }
    /// This calendar month's spend: saved sessions last active this month
    /// (other than this one, read once) plus this session's total.
    fn monthly(&self) -> f64 {
        let month = chrono::Local::now().format("%Y-%m").to_string();
        let id = self.session.lock().unwrap().clone();
        let mut base = self.month_base.lock().unwrap();
        if !base
            .as_ref()
            .is_some_and(|(m, s, _)| *m == month && *s == id)
        {
            let others = crate::session::Session::summaries()
                .into_iter()
                .filter(|(s, _)| s.id != id && s.updated.starts_with(&month))
                .map(|(s, _)| s.stats.total_cost_usd)
                .sum();
            *base = Some((month, id, others));
        }
        base.as_ref().map(|b| b.2).unwrap_or(0.) + self.total()
    }
    /// The tightest budget: (spent / limit, which budget, spent, limit).
    /// The configured budgets: (which, spent, limit).
    pub fn budgets(&self) -> Vec<(&'static str, f64, f64)> {
        let mut budgets = vec![];
        if let Some(l) = self.session_limit {
            budgets.push(("session", self.total(), l));
        }
        if let Some(l) = self.daily_limit {
            budgets.push(("daily", self.daily(), l));
        }
        if let Some(l) = self.monthly_limit {
            budgets.push(("monthly", self.monthly(), l));
        }
        budgets
    }
    fn tightest(&self) -> Option<(f64, &'static str, f64, f64)> {
        self.budgets()
            .into_iter()
            .map(|(name, spent, limit)| {
                (
                    if limit <= 0. {
                        f64::INFINITY
                    } else {
                        spent / limit
                    },
                    name,
                    spent,
                    limit,
                )
            })
            .max_by(|a, b| a.0.total_cmp(&b.0))
    }
    fn ratio(&self) -> f64 {
        self.tightest().map(|t| t.0).unwrap_or(0.)
    }
    pub fn check(&self) -> anyhow::Result<()> {
        let continued = self.state.lock().unwrap().continued;
        if DEBUG.load(Ordering::Relaxed) {
            self.log(
                "debug",
                "budget_check",
                json!({"total_cost_usd": self.total(), "continued": continued}),
            );
        }
        if !continued && self.ratio() >= 1. {
            anyhow::bail!(
                "USD budget reached ({}). Turn stopped before the next model call.",
                self.summary()
            );
        }
        Ok(())
    }
    /// A note as the tightest budget passes 50% and 80%, once each.
    pub fn warning(&self) -> Option<String> {
        let (ratio, name, spent, limit) = self.tightest()?;
        let level = if ratio >= 0.8 {
            80
        } else if ratio >= 0.5 {
            50
        } else {
            return None;
        };
        let mut s = self.state.lock().unwrap();
        if s.warned >= level || s.continued {
            return None;
        }
        s.warned = level;
        Some(format!(
            "USD budget is at least {level}% spent: {} of the {name} budget of {}.{}",
            crate::util::usd(spent),
            crate::util::usd(limit),
            if s.unknown {
                " Some calls had no known price and aren't counted."
            } else {
                ""
            }
        ))
    }
    pub fn continue_budget(&self) {
        self.state.lock().unwrap().continued = true;
    }
    pub fn log(&self, level: &str, event: &str, data: Value) {
        let _ = write_log(
            &self.root,
            &self.session.lock().unwrap(),
            level,
            event,
            data,
        );
    }
    pub async fn model_call(
        &self,
        provider: &str,
        model: &str,
        usage: Option<&crate::llm::Usage>,
        latency: f64,
    ) {
        let cost = usage.and_then(|u| price(provider, model, u, &self.prices));
        if usage.is_some() {
            let mut s = self.state.lock().unwrap();
            s.total += cost.unwrap_or(0.);
            s.unknown |= cost.is_none();
        }
        let data = json!({"provider": provider, "model": model, "input_tokens": usage.map(|u| u.prompt_tokens), "output_tokens": usage.map(|u| u.completion_tokens), "cached_tokens": usage.map(|u| u.cached_tokens), "cache_write_tokens": usage.map(|u| u.cache_write_tokens), "cost": cost, "latency": latency, "status": if usage.is_some() { "ok" } else { "error" }});
        self.log(
            if usage.is_some() { "info" } else { "error" },
            "model_call",
            data.clone(),
        );
        self.export("model_call", data, latency).await;
    }
    pub async fn tool_call(&self, name: &str, duration: f64, ok: bool) {
        let data = json!({"name": name, "duration": duration, "ok": ok});
        self.log("info", "tool_call", data.clone());
        self.export("tool_call", data, duration).await;
    }
    async fn export(&self, name: &str, data: Value, duration: f64) {
        let Some(endpoint) = &self.telemetry.otlp_endpoint else {
            return;
        };
        let client = crate::http::client();
        for (signal, payload) in otlp(
            name,
            &data,
            duration,
            self.telemetry.service_name.as_deref().unwrap_or("chanto"),
        ) {
            let mut request = client
                .post(format!("{}/v1/{signal}", endpoint.trim_end_matches('/')))
                .timeout(std::time::Duration::from_secs(2))
                .json(&payload);
            for (k, v) in &self.telemetry.headers {
                request = request.header(k, v);
            }
            if request
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .is_err()
            {
                self.log("warn", "telemetry_error", json!({"signal": signal}));
            }
        }
    }
}
fn attributes(data: &Value) -> Vec<Value> {
    data.as_object().unwrap().iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| {
        let value = match v { Value::Bool(b) => json!({"boolValue": b}), Value::Number(n) => json!({"doubleValue": n}), _ => json!({"stringValue": crate::secrets::redact_always(v.as_str().unwrap_or("")).to_string()}) };
        json!({"key": k, "value": value})
    }).collect()
}
fn otlp(name: &str, data: &Value, duration: f64, service: &str) -> Vec<(&'static str, Value)> {
    let end = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;
    let start = end.saturating_sub((duration * 1e9) as u64);
    let resource = json!({"attributes": [{"key": "service.name", "value": {"stringValue": crate::secrets::redact_always(service).to_string()}}]});
    let attrs = attributes(data);
    let trace = uuid::Uuid::new_v4().simple().to_string();
    let span = uuid::Uuid::new_v4().simple().to_string()[..16].to_string();
    let mut metrics = vec![
        json!({"name": "chanto.call.duration", "unit": "s", "gauge": {"dataPoints": [{"attributes": attrs, "timeUnixNano": end.to_string(), "asDouble": duration}]}}),
    ];
    for field in ["input_tokens", "output_tokens", "cost"] {
        if let Some(value) = data[field].as_f64() {
            metrics.push(json!({"name": format!("chanto.call.{field}"), "unit": if field == "cost" { "USD" } else { "{token}" }, "sum": {"aggregationTemporality": 1, "isMonotonic": true, "dataPoints": [{"attributes": attrs, "startTimeUnixNano": start.to_string(), "timeUnixNano": end.to_string(), "asDouble": value}]}}));
        }
    }
    vec![
        (
            "traces",
            json!({"resourceSpans": [{"resource": resource, "scopeSpans": [{"scope": {"name": "chanto"}, "spans": [{"traceId": trace, "spanId": span, "name": name, "kind": 1, "startTimeUnixNano": start.to_string(), "endTimeUnixNano": end.to_string(), "attributes": attrs, "status": {"code": if data["status"] == "error" || data["ok"] == false { 2 } else { 1 }}}]}]}]}),
        ),
        (
            "metrics",
            json!({"resourceMetrics": [{"resource": resource, "scopeMetrics": [{"scope": {"name": "chanto"}, "metrics": metrics}]}]}),
        ),
    ]
}
fn write_log(
    root: &Path,
    session: &str,
    level: &str,
    event: &str,
    data: Value,
) -> anyhow::Result<()> {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = log_path(root);
    std::fs::create_dir_all(path.parent().unwrap())?;
    for entry in std::fs::read_dir(path.parent().unwrap())?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(date) = name
            .strip_prefix("chanto-")
            .and_then(|n| n.strip_suffix(".log"))
            .and_then(|n| chrono::NaiveDate::parse_from_str(n, "%Y-%m-%d").ok())
            && date < chrono::Local::now().date_naive() - chrono::Duration::days(13)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    fn scrub(value: &mut Value) {
        match value {
            Value::String(s) => *s = crate::secrets::redact_always(s).into_owned(),
            Value::Array(items) => items.iter_mut().for_each(scrub),
            Value::Object(fields) => fields.values_mut().for_each(scrub),
            _ => {}
        }
    }
    let mut record = json!({"time": chrono::Utc::now().to_rfc3339(), "level": level, "session_id": session, "event": event, "data": data});
    scrub(&mut record);
    let line = record.to_string();
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    writeln!(file, "{line}")?;
    if DEBUG.load(Ordering::Relaxed) {
        eprintln!("{line}");
    }
    Ok(())
}
pub async fn logs(tail: bool) -> anyhow::Result<()> {
    let root = crate::config::home_dir();
    println!("{}", log_path(&root).display());
    if tail {
        let mut path = log_path(&root);
        let mut offset = 0;
        loop {
            let current = log_path(&root);
            if current != path {
                path = current;
                offset = 0;
            }
            if let Ok(mut file) = std::fs::File::open(&path) {
                use std::io::{Seek, SeekFrom};
                if file.metadata()?.len() < offset {
                    offset = 0;
                }
                file.seek(SeekFrom::Start(offset))?;
                let mut content = String::new();
                file.read_to_string(&mut content)?;
                offset = file.stream_position()?;
                print!("{content}");
                std::io::stdout().flush()?;
            }
            tokio::select! { _ = tokio::signal::ctrl_c() => break, _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {} }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_models_use_first_party_prices_or_none() {
        let p = |prov: &str, m: &str| bundled(prov, m).map(|p| (p.input, p.output, p.cached_input));
        for (prov, m) in [
            ("bedrock", "us.anthropic.claude-sonnet-4-5-20250929-v1:0"),
            (
                "bedrock",
                "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
            ),
            ("bedrock", "anthropic.claude-sonnet-4-20250514-v1:0"),
            ("vertex", "claude-sonnet-4-5@20250929"),
            ("foundry", "claude-sonnet-4-5"),
        ] {
            assert_eq!(p(prov, m), Some((3., 15., Some(0.3))), "{prov}/{m}");
        }
        assert_eq!(
            p("bedrock", "eu.anthropic.claude-opus-4-1-20250805-v1:0"),
            Some((15., 75., Some(1.5)))
        );
        assert_eq!(
            p("vertex", "claude-haiku-4-5@20251001"),
            Some((1., 5., Some(0.1)))
        );
        assert_eq!(p("azure", "gpt-5"), p("openai", "gpt-5"));
        assert_eq!(
            p("vertex", "google/gemini-2.5-pro"),
            Some((1.25, 10., Some(0.125)))
        );
        assert_eq!(
            p("bedrock", "us.amazon.nova-pro-v1:0"),
            Some((0.8, 3.2, Some(0.2)))
        );
        // Unknown: a deployment named for nothing, an unlisted model, a future Claude.
        assert!(
            p("azure", "prod-chat").is_none()
                && p("bedrock", "mistral.mistral-large-2407-v1:0").is_none()
        );
        assert!(p("bedrock", "us.anthropic.claude-opus-9-v1:0").is_none());
    }
    #[test]
    fn cached_pricing_and_reported_cost() {
        let mut u = crate::llm::Usage {
            prompt_tokens: 1_000_000,
            cached_tokens: 200_000,
            cache_write_tokens: 100_000,
            completion_tokens: 100_000,
            ..Default::default()
        };
        assert!(
            (price("anthropic", "claude-sonnet-4-5", &u, &BTreeMap::new()).unwrap() - 7.32).abs()
                < 1e-9
        );
        assert!(
            (price("anthropic", "claude-sonnet-4-6", &u, &BTreeMap::new()).unwrap() - 4.035).abs()
                < 1e-9
        );
        let config: crate::config::Config = toml::from_str(
            "[pricing.\"custom/model\"]\ninput = 0.14\noutput = 0.28\ncached_input = 0.014\n",
        )
        .unwrap();
        assert!((price("custom", "model", &u, &config.pricing).unwrap() - 0.1428).abs() < 1e-9);
        assert_eq!(price("unknown", "unknown", &u, &BTreeMap::new()), None);
        u.cost = Some(0.123);
        assert_eq!(
            price("openrouter", "anything", &u, &BTreeMap::new()),
            Some(0.123)
        );
    }
    #[test]
    fn gpt_6_long_prompts_use_the_long_context_rates() {
        let u = |n| crate::llm::Usage {
            prompt_tokens: n,
            completion_tokens: 1_000_000,
            ..Default::default()
        };
        let short = price("openai", "gpt-6.1-sol", &u(272_000), &BTreeMap::new()).unwrap();
        let long = price("openai", "gpt-6.1-sol", &u(1_000_000), &BTreeMap::new()).unwrap();
        assert!(
            (short - (0.544 + 10.)).abs() < 1e-9 && (long - (4. + 15.)).abs() < 1e-9,
            "{short} {long}"
        );
    }

    #[test]
    fn haiku_5_5_long_prompts_cost_five_times_more() {
        let short = crate::llm::Usage {
            prompt_tokens: 100_000,
            completion_tokens: 0,
            ..Default::default()
        };
        let long = crate::llm::Usage {
            prompt_tokens: 200_000,
            completion_tokens: 0,
            ..Default::default()
        };
        let s = price("anthropic", "claude-haiku-5-5", &short, &BTreeMap::new()).unwrap();
        let l = price("anthropic", "claude-haiku-5-5", &long, &BTreeMap::new()).unwrap();
        assert!((s - 0.01).abs() < 1e-9 && (l - 0.1).abs() < 1e-9, "{s} {l}");
    }

    #[test]
    fn a_published_price_list_is_used_after_overrides_and_bad_entries_are_dropped() {
        let list = load_price_list(br#"{"updated": "2026-10-15", "prices": {
            "test/published-model": {"input": 1.0, "output": 2.0, "cached_input": 0.1, "cache_write": 1.25},
            "test/broken": {"input": -1.0, "output": 2.0, "cached_input": null, "cache_write": null}}}"#).unwrap();
        assert!(list.contains_key("test/published-model") && !list.contains_key("test/broken"));
        PUBLISHED
            .write()
            .unwrap()
            .get_or_insert_with(BTreeMap::new)
            .extend(list);
        let u = crate::llm::Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 1_000_000,
            ..Default::default()
        };
        assert_eq!(
            price("test", "published-model", &u, &BTreeMap::new()),
            Some(3.0)
        );
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "test/published-model".to_string(),
            Price {
                input: 0.5,
                output: 0.5,
                cached_input: None,
                cache_write: None,
            },
        );
        assert_eq!(
            price("test", "published-model", &u, &overrides),
            Some(1.0),
            "your own [pricing] wins"
        );
        assert!(load_price_list(b"not json").is_none());
    }

    #[test]
    fn chanto_cloud_models_cost_what_its_gateway_charges() {
        let list = load_gateway_prices(br#"{"models": [
            {"id": "test-glm", "api": "openai", "default": false, "input_usd_per_mtok": 1.4, "output_usd_per_mtok": 4.4, "cache_read_usd_per_mtok": 0.26},
            {"id": "test-broken", "input_usd_per_mtok": -1, "output_usd_per_mtok": 1}]}"#).unwrap();
        assert!(list.contains_key("test-glm") && !list.contains_key("test-broken"));
        GATEWAY
            .write()
            .unwrap()
            .get_or_insert_with(BTreeMap::new)
            .extend(list);
        let u = crate::llm::Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 1_000_000,
            cached_tokens: 500_000,
            ..Default::default()
        };
        let cost = price("chanto", "test-glm", &u, &BTreeMap::new()).unwrap();
        assert!(
            (cost - (0.5 * 1.4 + 0.5 * 0.26 + 4.4)).abs() < 1e-9,
            "{cost}"
        );
        assert_eq!(
            price("together", "test-glm", &u, &BTreeMap::new()),
            None,
            "only Chanto cloud's own models"
        );
        assert!(load_gateway_prices(b"{}").is_none());
    }

    #[tokio::test]
    async fn openrouter_usage_cost_passthrough() {
        use crate::llm::{ChatRequest, Provider, ProviderKind};
        let body = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices": [], "usage": {"prompt_tokens": 1000, "completion_tokens": 200, "prompt_tokens_details": {"cached_tokens": 300}, "cost": 0.0123}})
        );
        let (base, _) = crate::llm::mock::serve(vec![(200, body)]).await;
        let provider = Provider::new(ProviderKind::Openai, &base, None, 8192);
        let request = ChatRequest {
            model: "anything".into(),
            messages: vec![],
            tools: vec![],
            think: None,
            effort: None,
            temperature: None,
            max_tokens: None,
            json: false,
        };
        let response = provider.chat(&request, &mut |_| {}).await.unwrap();
        assert_eq!(response.usage.cached_tokens, 300);
        assert_eq!(
            price("openrouter", "anything", &response.usage, &BTreeMap::new()),
            Some(0.0123)
        );
    }

    #[test]
    fn budget_stop_and_redacted_log() {
        let root = std::env::temp_dir().join(format!("chanto-cost-{}", uuid::Uuid::new_v4()));
        let mut a = Accounting::new(&crate::config::Config {
            max_cost_usd: Some(1.),
            ..Default::default()
        });
        a.root = root.clone();
        a.state.lock().unwrap().total = 0.8;
        assert!(a.warning().is_some());
        assert!(a.check().is_ok());
        a.state.lock().unwrap().total = 1.;
        assert!(a.check().is_err());
        a.continue_budget();
        assert!(a.check().is_ok());
        a.log(
            "info",
            "permission",
            json!({"name": "ghp_0123456789abcdefghijklmnopqrstuvwxyzAB"}),
        );
        let line = std::fs::read_to_string(log_path(&root)).unwrap();
        assert!(!line.contains("ghp_"));
        assert!(serde_json::from_str::<Value>(&line).is_ok());
        a.daily_limit = Some(1.);
        a.state.lock().unwrap().continued = false;
        a.state.lock().unwrap().total = 0.;
        a.log("info", "model_call", json!({"cost": 1.}));
        assert!(
            a.check().is_err(),
            "daily spend must include other sessions' model-call logs"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn budgets_note_half_and_most_then_stop_including_the_month() {
        let config: crate::config::Config =
            toml::from_str("[spend]\nsession_usd = 10.0\nmonth_usd = 100.0\n").unwrap();
        let mut a = Accounting::new(&config);
        a.root = std::env::temp_dir().join(format!("chanto-budget-{}", uuid::Uuid::new_v4()));
        assert_eq!(
            a.session_limit,
            Some(10.),
            "[spend] session_usd is the session budget"
        );
        // Other sessions this month already spent $40 (read once, then cached).
        let month = chrono::Local::now().format("%Y-%m").to_string();
        *a.month_base.lock().unwrap() = Some((month, a.session.lock().unwrap().clone(), 40.));
        a.state.lock().unwrap().total = 4.;
        assert!(
            a.warning().is_none(),
            "40% of the session, 44% of the month"
        );
        a.state.lock().unwrap().total = 5.;
        let half = a.warning().unwrap();
        assert!(
            half.contains("50%") && half.contains("session budget of $10.00"),
            "{half}"
        );
        assert!(a.warning().is_none(), "each note once");
        a.state.lock().unwrap().total = 8.;
        assert!(a.warning().unwrap().contains("80%"));
        assert!(a.check().is_ok());
        a.state.lock().unwrap().total = 10.;
        assert!(a.check().is_err());
        // A tighter month: $95 elsewhere makes the month the binding budget.
        a.session_limit = None;
        let month = chrono::Local::now().format("%Y-%m").to_string();
        *a.month_base.lock().unwrap() = Some((month, a.session.lock().unwrap().clone(), 95.));
        a.state.lock().unwrap().total = 1.;
        assert!(a.check().is_ok());
        a.state.lock().unwrap().total = 5.;
        assert!(
            a.check()
                .unwrap_err()
                .to_string()
                .contains("budget reached")
        );
        assert_eq!(
            a.budgets().iter().map(|b| b.0).collect::<Vec<_>>(),
            vec!["monthly"]
        );
        // `max_cost_usd` wins over `[spend] session_usd`.
        let both: crate::config::Config =
            toml::from_str("max_cost_usd = 3.0\n[spend]\nsession_usd = 10.0\n").unwrap();
        assert_eq!(Accounting::new(&both).session_limit, Some(3.));
    }

    #[tokio::test]
    async fn otlp_mock_collector() {
        use axum::{Router, extract::State, routing::post};
        let seen = std::sync::Arc::new(Mutex::new(Vec::<Value>::new()));
        async fn receive(
            State(seen): State<std::sync::Arc<Mutex<Vec<Value>>>>,
            axum::Json(v): axum::Json<Value>,
        ) {
            seen.lock().unwrap().push(v);
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/v1/traces", post(receive))
            .route("/v1/metrics", post(receive))
            .with_state(seen.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut a = Accounting::new(&crate::config::Config::default());
        a.root = std::env::temp_dir().join(format!("chanto-otlp-{}", uuid::Uuid::new_v4()));
        a.telemetry.otlp_endpoint = Some(format!("http://{addr}"));
        a.tool_call("Read", 0.1, true).await;
        let values = seen.lock().unwrap();
        assert_eq!(values.len(), 2);
        let span = &values[0]["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(span["name"], "tool_call");
        assert_eq!(span["traceId"].as_str().unwrap().len(), 32);
        assert!(values[1]["resourceMetrics"][0]["scopeMetrics"][0]["metrics"].is_array());
        task.abort();
        std::fs::remove_dir_all(&a.root).unwrap();
    }
}
