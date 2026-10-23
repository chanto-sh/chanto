//! Built-in hosted providers: use `<provider>/<model>` (for example
//! `groq/llama-3.3-70b-versatile`) with the provider's usual API-key
//! variable set, a key saved with `chanto login <provider>`, or (ChatGPT,
//! OpenRouter) a browser login.
//!
//! The cloud platforms (`bedrock`, `vertex`, `azure`, `foundry`) use the
//! cloud's own credentials (AWS profiles, Google ADC, Entra ID) or an API key,
//! and derive their endpoint from region, project or resource.

use crate::llm::ProviderKind;

pub struct Hosted {
    pub id: &'static str,
    pub name: &'static str,
    pub kind: ProviderKind,
    pub base_url: &'static str,
    /// Environment variables checked for an API key, in order.
    pub env: &'static [&'static str],
    /// Signs in through the browser (`chanto login <id>`).
    pub browser_login: bool,
}

const fn p(
    id: &'static str,
    name: &'static str,
    kind: ProviderKind,
    base_url: &'static str,
    env: &'static [&'static str],
) -> Hosted {
    Hosted {
        id,
        name,
        kind,
        base_url,
        env,
        browser_login: false,
    }
}

const OAI: ProviderKind = ProviderKind::Openai;

pub const CATALOG: &[Hosted] = &[
    p(
        "anthropic",
        "Anthropic",
        ProviderKind::Anthropic,
        "https://api.anthropic.com",
        &["ANTHROPIC_API_KEY"],
    ),
    p(
        "openai",
        "OpenAI",
        ProviderKind::OpenaiResponses,
        "https://api.openai.com/v1",
        &["OPENAI_API_KEY"],
    ),
    Hosted {
        id: "chatgpt",
        name: "OpenAI with your ChatGPT plan",
        kind: ProviderKind::OpenaiResponses,
        base_url: "https://api.openai.com/v1",
        env: &[],
        browser_login: true,
    },
    Hosted {
        id: "openrouter",
        name: "OpenRouter",
        kind: OAI,
        base_url: "https://openrouter.ai/api/v1",
        env: &["OPENROUTER_API_KEY"],
        browser_login: true,
    },
    p(
        "gemini",
        "Google Gemini",
        OAI,
        "https://generativelanguage.googleapis.com/v1beta/openai",
        &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
    ),
    p(
        "deepseek",
        "DeepSeek",
        OAI,
        "https://api.deepseek.com/v1",
        &["DEEPSEEK_API_KEY"],
    ),
    p("xai", "xAI", OAI, "https://api.x.ai/v1", &["XAI_API_KEY"]),
    p(
        "mistral",
        "Mistral",
        OAI,
        "https://api.mistral.ai/v1",
        &["MISTRAL_API_KEY"],
    ),
    p(
        "groq",
        "Groq",
        OAI,
        "https://api.groq.com/openai/v1",
        &["GROQ_API_KEY"],
    ),
    p(
        "cerebras",
        "Cerebras",
        OAI,
        "https://api.cerebras.ai/v1",
        &["CEREBRAS_API_KEY"],
    ),
    p(
        "together",
        "Together AI",
        OAI,
        "https://api.together.xyz/v1",
        &["TOGETHER_API_KEY"],
    ),
    p(
        "fireworks",
        "Fireworks",
        OAI,
        "https://api.fireworks.ai/inference/v1",
        &["FIREWORKS_API_KEY"],
    ),
    p(
        "moonshot",
        "Moonshot AI (Kimi)",
        OAI,
        "https://api.moonshot.ai/v1",
        &["MOONSHOT_API_KEY"],
    ),
    p(
        "zai",
        "Z.ai (GLM coding plan)",
        OAI,
        "https://api.z.ai/api/coding/paas/v4",
        &["ZAI_API_KEY"],
    ),
    p(
        "minimax",
        "MiniMax",
        OAI,
        "https://api.minimax.io/v1",
        &["MINIMAX_API_KEY"],
    ),
    // Subscription plans whose vendors allow them in third-party coding tools.
    // Their keys are separate from the same vendor's pay-as-you-go keys.
    p(
        "kimi-coding",
        "Kimi Code (coding plan)",
        OAI,
        "https://api.kimi.ai/coding/v1",
        &["KIMI_CODE_API_KEY"],
    ),
    p(
        "minimax-coding",
        "MiniMax (M Plan coding plan)",
        OAI,
        "https://api.minimax.io/v1",
        &["MINIMAX_CODING_API_KEY"],
    ),
    p(
        "qwen-coding",
        "Qwen Code (Alibaba coding plan)",
        OAI,
        "https://coding-intl.dashscope.aliyuncs.com/v1",
        &["ALIBABA_CODING_PLAN_API_KEY"],
    ),
    p(
        "tencent-coding",
        "Tencent CodeBuddy (coding plan)",
        OAI,
        "https://api.lkeap.cloud.tencent.com/coding/v3",
        &["TENCENT_CODING_PLAN_API_KEY"],
    ),
    p(
        "volcengine-coding",
        "Volcengine (Doubao coding plan)",
        OAI,
        "https://ark.cn-beijing.volces.com/api/coding/v3",
        &["ARK_CODING_PLAN_API_KEY"],
    ),
    p(
        "stepfun-plan",
        "StepFun Step plan",
        OAI,
        "https://api.stepfun.ai/step_plan/v1",
        &["STEPFUN_API_KEY"],
    ),
    p(
        "cline",
        "Cline Pass",
        OAI,
        "https://api.cline.bot/api/v1",
        &["CLINE_API_KEY"],
    ),
    p(
        "kimi-coding-cn",
        "Kimi Code (coding plan, China)",
        OAI,
        "https://api.kimi.com/coding/v1",
        &["KIMI_CODE_API_KEY"],
    ),
    p(
        "qwen-coding-cn",
        "Qwen Code (Alibaba coding plan, China)",
        OAI,
        "https://coding.dashscope.aliyuncs.com/v1",
        &["ALIBABA_CODING_PLAN_API_KEY"],
    ),
    p(
        "stepfun-plan-cn",
        "StepFun Step plan (China)",
        OAI,
        "https://api.stepfun.com/step_plan/v1",
        &["STEPFUN_API_KEY"],
    ),
    p(
        "dashscope",
        "Alibaba Qwen (DashScope)",
        OAI,
        "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        &["DASHSCOPE_API_KEY"],
    ),
    p(
        "huggingface",
        "Hugging Face",
        OAI,
        "https://router.huggingface.co/v1",
        &["HF_TOKEN"],
    ),
    p(
        "nvidia",
        "NVIDIA NIM",
        OAI,
        "https://integrate.api.nvidia.com/v1",
        &["NVIDIA_API_KEY"],
    ),
    p(
        "vercel",
        "Vercel AI Gateway",
        OAI,
        "https://ai-gateway.vercel.sh/v1",
        &["AI_GATEWAY_API_KEY"],
    ),
    p(
        "opencode",
        "OpenCode Zen",
        OAI,
        "https://opencode.ai/zen/v1",
        &["OPENCODE_API_KEY"],
    ),
    p(
        "baseten",
        "Baseten",
        OAI,
        "https://inference.baseten.co/v1",
        &["BASETEN_API_KEY"],
    ),
    p(
        "perplexity",
        "Perplexity",
        OAI,
        "https://api.perplexity.ai",
        &["PERPLEXITY_API_KEY"],
    ),
    p(
        "cohere",
        "Cohere",
        OAI,
        "https://api.cohere.ai/compatibility/v1",
        &["COHERE_API_KEY"],
    ),
    p(
        "nebius",
        "Nebius Token Factory",
        OAI,
        "https://api.tokenfactory.nebius.com/v1",
        &["NEBIUS_API_KEY"],
    ),
    p(
        "deepinfra",
        "DeepInfra",
        OAI,
        "https://api.deepinfra.com/v1/openai",
        &["DEEPINFRA_API_KEY"],
    ),
    p(
        "sambanova",
        "SambaNova",
        OAI,
        "https://api.sambanova.ai/v1",
        &["SAMBANOVA_API_KEY"],
    ),
    p(
        "siliconflow",
        "SiliconFlow",
        OAI,
        "https://api.siliconflow.com/v1",
        &["SILICONFLOW_API_KEY"],
    ),
    p(
        "upstage",
        "Upstage (Solar)",
        OAI,
        "https://api.upstage.ai/v1/solar",
        &["UPSTAGE_API_KEY"],
    ),
    p(
        "ai21",
        "AI21",
        OAI,
        "https://api.ai21.com/studio/v1",
        &["AI21_API_KEY"],
    ),
    p(
        "inception",
        "Inception (Mercury)",
        OAI,
        "https://api.inceptionlabs.ai/v1",
        &["INCEPTION_API_KEY"],
    ),
    p(
        "requesty",
        "Requesty",
        OAI,
        "https://router.requesty.ai/v1",
        &["REQUESTY_API_KEY"],
    ),
    p(
        "venice",
        "Venice AI",
        OAI,
        "https://api.venice.ai/api/v1",
        &["VENICE_API_KEY"],
    ),
    p(
        "chutes",
        "Chutes",
        OAI,
        "https://llm.chutes.ai/v1",
        &["CHUTES_API_KEY"],
    ),
    p(
        "nano-gpt",
        "Nano-GPT",
        OAI,
        "https://nano-gpt.com/api/v1",
        &["NANO_GPT_API_KEY"],
    ),
    p(
        "scaleway",
        "Scaleway",
        OAI,
        "https://api.scaleway.ai/v1",
        &["SCALEWAY_API_KEY"],
    ),
    p(
        "digitalocean",
        "DigitalOcean",
        OAI,
        "https://inference.do-ai.run/v1",
        &["DIGITALOCEAN_ACCESS_TOKEN"],
    ),
    p(
        "wandb",
        "Weights & Biases",
        OAI,
        "https://api.inference.wandb.ai/v1",
        &["WANDB_API_KEY"],
    ),
    p(
        "ovhcloud",
        "OVHcloud",
        OAI,
        "https://oai.endpoints.kepler.ai.cloud.ovh.net/v1",
        &["OVHCLOUD_API_KEY"],
    ),
    p(
        "crusoe",
        "Crusoe",
        OAI,
        "https://api.inference.crusoecloud.com/v1",
        &["CRUSOE_API_KEY"],
    ),
    p(
        "morph",
        "Morph",
        OAI,
        "https://api.morphllm.com/v1",
        &["MORPH_API_KEY"],
    ),
    p(
        "poe",
        "Poe",
        OAI,
        "https://api.poe.com/v1",
        &["POE_API_KEY"],
    ),
    p(
        "stepfun",
        "StepFun",
        OAI,
        "https://api.stepfun.com/v1",
        &["STEPFUN_API_KEY"],
    ),
    p(
        "volcengine",
        "Volcengine (Doubao)",
        OAI,
        "https://ark.cn-beijing.volces.com/api/v3",
        &["ARK_API_KEY"],
    ),
    p(
        "xiaomi",
        "Xiaomi MiMo",
        OAI,
        "https://api.xiaomimimo.com/v1",
        &["XIAOMI_API_KEY"],
    ),
    p(
        "moonshot-cn",
        "Moonshot AI (China)",
        OAI,
        "https://api.moonshot.cn/v1",
        &["MOONSHOT_API_KEY"],
    ),
    p(
        "ollama-cloud",
        "Ollama Cloud",
        OAI,
        "https://ollama.com/v1",
        &["OLLAMA_API_KEY"],
    ),
    p(
        "bedrock",
        "Amazon Bedrock",
        ProviderKind::Bedrock,
        "",
        &["AWS_BEARER_TOKEN_BEDROCK"],
    ),
    p("vertex", "Google Vertex AI", ProviderKind::Vertex, "", &[]),
    p(
        "azure",
        "Azure OpenAI",
        ProviderKind::Azure,
        "",
        crate::cloud::azure::OPENAI_KEY_ENV,
    ),
    p(
        "foundry",
        "Azure AI Foundry",
        ProviderKind::Foundry,
        "",
        crate::cloud::azure::FOUNDRY_KEY_ENV,
    ),
    // Its server comes from the sign-in or CHANTO_CLOUD_URL (see chanto_cloud).
    Hosted {
        id: "chanto",
        name: "Chanto cloud",
        kind: ProviderKind::Anthropic,
        base_url: "",
        env: &[],
        browser_login: true,
    },
];

/// Picker suggestions for the cloud platforms (Azure deployments are named
/// per account, so they come from `AZURE_OPENAI_DEPLOYMENT` instead).
pub fn cloud_models(id: &str) -> &'static [&'static str] {
    match id {
        "bedrock" => &[
            "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "global.anthropic.claude-opus-4-5-20251101-v1:0",
            "global.anthropic.claude-haiku-4-5-20251001-v1:0",
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "openai.gpt-oss-120b-1:0",
            "us.amazon.nova-pro-v1:0",
            "us.meta.llama3-3-70b-instruct-v1:0",
        ],
        "vertex" => &[
            "claude-sonnet-4-5@20250929",
            "claude-opus-4-5@20251101",
            "claude-haiku-4-5@20251001",
            "gemini-2.5-pro",
            "gemini-2.5-flash",
        ],
        "foundry" => &["claude-sonnet-4-5", "claude-opus-4-5", "claude-haiku-4-5"],
        _ => &[],
    }
}

/// Ambient cloud credentials (AWS chain, Google ADC, Entra ID), described
/// without values. Only reads environment variables and local files.
pub fn cloud_source(
    kind: ProviderKind,
    settings: &crate::cloud::Settings,
    vars: &crate::cloud::Vars,
) -> Option<String> {
    use crate::cloud::{aws, azure, gcp};
    match kind {
        ProviderKind::Bedrock => aws::source(settings, vars),
        ProviderKind::Vertex => gcp::source(vars),
        ProviderKind::Azure | ProviderKind::Foundry => {
            let foundry = kind == ProviderKind::Foundry;
            if let Some((k, _)) = azure::env_key(foundry, vars) {
                return Some(format!("env:{k}"));
            }
            (vars.get("AZURE_CLIENT_SECRET").is_some()
                || crate::shell::command_exists(&vars.cli("az")))
            .then(|| azure::entra_source(vars).to_string())
        }
        _ => None,
    }
}

/// `chanto providers` status for a cloud platform: credentials and location, or what to set.
pub fn cloud_status(h: &Hosted) -> String {
    use crate::cloud::{aws, gcp};
    let vars = crate::cloud::Vars::process();
    let settings = crate::cloud::Settings::default();
    let key = crate::auth::get(h.id)
        .is_some()
        .then(|| "key saved".to_string());
    let Some(source) = key.or_else(|| cloud_source(h.kind, &settings, &vars)) else {
        return match h.kind {
            ProviderKind::Bedrock => {
                "set AWS_PROFILE or AWS_ACCESS_KEY_ID (or AWS_BEARER_TOKEN_BEDROCK), and AWS_REGION"
                    .into()
            }
            ProviderKind::Vertex => {
                "run `gcloud auth application-default login`, set GOOGLE_CLOUD_PROJECT".into()
            }
            ProviderKind::Azure => {
                "set AZURE_OPENAI_ENDPOINT and AZURE_OPENAI_API_KEY (or `az login`)".into()
            }
            _ => "set AZURE_FOUNDRY_RESOURCE and AZURE_FOUNDRY_API_KEY (or `az login`)".into(),
        };
    };
    let location = match h.kind {
        ProviderKind::Bedrock => aws::region(&settings, &vars)
            .map(|(r, _)| format!("region {r}"))
            .ok_or("no region: set AWS_REGION"),
        ProviderKind::Vertex => gcp::project(&settings, &vars)
            .map(|(p, _)| format!("project {p}, {}", gcp::region(&settings, &vars).0))
            .ok_or("no project: set GOOGLE_CLOUD_PROJECT"),
        _ if cloud_located(h.kind, &settings, &vars) => Ok("endpoint set".into()),
        ProviderKind::Azure => Err("no endpoint: set AZURE_OPENAI_ENDPOINT"),
        _ => Err("no endpoint: set AZURE_FOUNDRY_RESOURCE"),
    };
    match location {
        Ok(l) => format!("{source} · {l}"),
        Err(e) => format!("{source} · {e}"),
    }
}

/// Credentials plus a location (region, project or endpoint), so requests can
/// be made without further setup: the source description, for detection.
pub fn cloud_ready(
    kind: ProviderKind,
    settings: &crate::cloud::Settings,
    vars: &crate::cloud::Vars,
) -> Option<String> {
    let source = cloud_source(kind, settings, vars)?;
    cloud_located(kind, settings, vars).then_some(source)
}

/// A region (Bedrock), project (Vertex) or endpoint (Azure) is known.
pub fn cloud_located(
    kind: ProviderKind,
    settings: &crate::cloud::Settings,
    vars: &crate::cloud::Vars,
) -> bool {
    use crate::cloud::{aws, azure, gcp};
    match kind {
        ProviderKind::Bedrock => aws::region(settings, vars).is_some(),
        ProviderKind::Vertex => gcp::project(settings, vars).is_some(),
        ProviderKind::Azure => azure::openai_base(settings, "", vars).is_ok(),
        ProviderKind::Foundry => azure::foundry_root(settings, "", vars).is_ok(),
        _ => false,
    }
}

/// Models a coding plan serves (from the vendor's docs): picker suggestions,
/// and the model list when the plan's API doesn't list them.
pub fn plan_models(id: &str) -> &'static [&'static str] {
    match id {
        "kimi-coding" | "kimi-coding-cn" => &[
            "kimi-for-coding",
            "kimi-for-coding-highspeed",
            "k3",
            "k3-256k",
        ],
        "minimax-coding" => &[
            "MiniMax-M3.1-Flash-Preview",
            "MiniMax-M3",
            "MiniMax-M2.7",
            "MiniMax-M2.7-highspeed",
        ],
        _ => &[],
    }
}

/// A model's context window on a catalog provider, where the vendor documents
/// one and the API doesn't report it.
pub fn context(id: &str, model: &str) -> Option<u32> {
    match (id, model) {
        ("kimi-coding" | "kimi-coding-cn", "kimi-for-coding") => Some(1_048_576),
        // `k3` reaches 1M only on higher membership tiers, so budget for 256K.
        ("kimi-coding" | "kimi-coding-cn", _) => Some(262_144),
        ("minimax-coding" | "minimax", m) if m.starts_with("MiniMax-M3") => Some(1_000_000),
        ("minimax-coding" | "minimax", m) if m.starts_with("MiniMax-M2") => Some(204_800),
        _ => None,
    }
}

/// Where to create a key, said before `chanto login` asks for one.
pub fn key_page(id: &str) -> Option<&'static str> {
    match id {
        "kimi-coding" | "kimi-coding-cn" => Some(
            "Create a Kimi Code key at https://www.kimi.com/code/console (a Moonshot platform key won't work here).",
        ),
        "minimax-coding" => Some(
            "Copy your M Plan subscription key (sk-cp-…) from https://platform.minimax.io/console/plan (a pay-as-you-go key won't work here).",
        ),
        _ => None,
    }
}

pub fn find(id: &str) -> Option<&'static Hosted> {
    CATALOG.iter().find(|h| h.id == id)
}

/// How a provider would authenticate right now, if at all.
pub enum Cred {
    Key(String),
    /// A browser login: the token comes from `auth::token` per request.
    Login,
    /// The cloud's own credentials (described, never the values).
    Cloud(String),
}

pub fn credential(h: &Hosted) -> Option<Cred> {
    // The Chanto cloud sign-in is shared with `chanto cloud login`.
    if h.id == crate::chanto_cloud::PROVIDER_ID {
        return crate::chanto_cloud::credential_source().map(|_| Cred::Login);
    }
    match crate::auth::get(h.id) {
        Some(crate::auth::Credential::ApiKey { key }) => return Some(Cred::Key(key)),
        Some(crate::auth::Credential::Oauth { .. }) => return Some(Cred::Login),
        Some(crate::auth::Credential::Remote { .. }) | None => {}
        Some(crate::auth::Credential::Keychain) => {}
    }
    if let Some(k) = h
        .env
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|k| !k.is_empty()))
    {
        return Some(Cred::Key(k));
    }
    if h.kind.is_cloud() {
        return cloud_source(h.kind, &Default::default(), &crate::cloud::Vars::process())
            .map(Cred::Cloud);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_ids_are_unique_and_urls_are_https() {
        let mut ids: Vec<&str> = CATALOG.iter().map(|h| h.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), CATALOG.len());
        // Cloud platforms derive their endpoint from region, project or resource.
        assert!(CATALOG.iter().all(|h| h.base_url.starts_with("https://")
            || ((h.kind.is_cloud() || h.id == "chanto") && h.base_url.is_empty())));
        assert!(find("groq").is_some() && find("nope").is_none());
    }

    #[test]
    fn every_provider_has_a_way_to_authenticate() {
        for h in CATALOG {
            assert!(
                !h.env.is_empty() || h.browser_login || h.kind.is_cloud() || h.id == "chanto",
                "{} has no key variable, login or cloud credentials",
                h.id
            );
        }
    }

    #[test]
    fn coding_plans_use_their_own_openai_endpoints() {
        for id in [
            "zai",
            "kimi-coding",
            "kimi-coding-cn",
            "qwen-coding",
            "qwen-coding-cn",
            "tencent-coding",
            "volcengine-coding",
            "stepfun-plan",
            "stepfun-plan-cn",
            "cline",
        ] {
            let h = find(id).unwrap_or_else(|| panic!("{id} is in the catalog"));
            assert_eq!(h.kind, OAI, "{id} should be OpenAI-compatible");
            assert!(
                h.base_url.starts_with("https://"),
                "{id} needs an https endpoint"
            );
            assert!(!h.env.is_empty(), "{id} needs its own key variable");
        }
    }
}
