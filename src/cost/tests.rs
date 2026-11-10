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
fn a_log_deleted_while_open_is_written_again() {
    let root = std::env::temp_dir().join(format!("chanto-log-{}", uuid::Uuid::new_v4()));
    let lines = || {
        std::fs::read_to_string(log_path(&root))
            .unwrap_or_default()
            .lines()
            .count()
    };
    write_log(&root, "s", "info", "one", json!({})).unwrap();
    write_log(&root, "s", "info", "two", json!({})).unwrap();
    assert_eq!(lines(), 2);
    std::fs::remove_file(log_path(&root)).unwrap();
    write_log(&root, "s", "info", "three", json!({})).unwrap();
    assert_eq!(lines(), 1, "a fresh file, not the deleted one");
    std::fs::remove_dir_all(&root).unwrap();
    write_log(&root, "s", "info", "four", json!({})).unwrap();
    assert_eq!(lines(), 1, "the directory comes back too");
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

#[test]
fn every_bundled_model_has_a_sane_price() {
    for key in [
        "openai/gpt-4o",
        "openai/gpt-4o-mini",
        "openai/gpt-4.1",
        "openai/gpt-4.1-mini",
        "openai/gpt-4.1-nano",
        "openai/gpt-5",
        "openai/gpt-5-mini",
        "anthropic/claude-sonnet-4-5",
        "anthropic/claude-sonnet-5-5",
        "anthropic/claude-opus-5-5",
        "anthropic/claude-haiku-5-5",
        "openai/gpt-6.1-sol",
        "openai/gpt-6-sol",
        "openai/gpt-6-astra",
        "openai/gpt-6-luna",
        "gemini/gemini-3.8-flash",
        "mistral/mistral-medium-latest",
        "anthropic/claude-opus-4-5",
        "anthropic/claude-haiku-4-5",
        "anthropic/claude-opus-4-6",
        "anthropic/claude-opus-4",
        "anthropic/claude-sonnet-4",
        "anthropic/claude-3-5-haiku",
        "vertex/gemini-2.5-flash",
        "bedrock/openai.gpt-oss-120b-1:0",
        "bedrock/openai.gpt-oss-20b-1:0",
        "bedrock/amazon.nova-lite-v1:0",
        "bedrock/amazon.nova-micro-v1:0",
        "bedrock/meta.llama3-3-70b-instruct-v1:0",
        "bedrock/deepseek.r1-v1:0",
        "gemini/gemini-2.0-flash",
        "gemini/gemini-2.5-flash",
        "deepseek/deepseek-chat",
        "deepseek/deepseek-flash",
        "deepseek/deepseek-v4-pro",
        "xai/grok-3",
        "mistral/mistral-small-2503",
        "groq/openai/gpt-oss-120b",
        "groq/openai/gpt-oss-20b",
        "fireworks/accounts/fireworks/models/gpt-oss-120b",
        "opencode/minimax-m2.7",
        "opencode/claude-sonnet-4-6",
        "dashscope/qwen-plus-2025-01-25",
        "moonshot/kimi-k2-turbo",
        "baseten/openai/gpt-oss-120b",
        "vercel/openai/gpt-4.1",
        "cerebras/gpt-oss-120b",
        "together/meta-llama/Llama-3.3-70B-Instruct-Turbo",
        "minimax/MiniMax-M2",
        "minimax/MiniMax-M2.7",
    ] {
        let (provider, model) = key.split_once('/').unwrap();
        let p = bundled(provider, model).unwrap_or_else(|| panic!("{key}"));
        assert!(p.input >= 0. && p.output >= p.input * 0.5, "{key}");
    }
    assert!(bundled("openrouter", "google/gemini-2.5-flash").is_some());
    assert!(bundled("openrouter", "x-ai/grok-3").is_some());
    for local in ["local", "ollama", "lmstudio", "llamacpp"] {
        assert_eq!(bundled(local, "anything").unwrap().input, 0.);
    }
}

#[test]
fn gateway_catalog_drops_bad_rates() {
    let list = load_gateway_prices(
        br#"{"models":[
            {"id":"good","input_usd_per_mtok":1.0,"output_usd_per_mtok":2.0,"cache_read_usd_per_mtok":0.1},
            {"id":"plain","input_usd_per_mtok":1.0,"output_usd_per_mtok":2.0},
            {"id":"negative","input_usd_per_mtok":-1.0,"output_usd_per_mtok":2.0},
            {"id":"badcache","input_usd_per_mtok":1.0,"output_usd_per_mtok":2.0,"cache_read_usd_per_mtok":-3.0}
        ]}"#,
    )
    .unwrap();
    assert_eq!(list.keys().collect::<Vec<_>>(), vec!["good", "plain"]);
    assert_eq!(list["good"].cached_input, Some(0.1));
    assert!(list["plain"].cached_input.is_none() && list["plain"].cache_write.is_none());
    assert!(load_gateway_prices(b"not json").is_none());
    assert!(load_price_list(b"{}").is_none());
    assert!(
        gateway("openai", "good").is_none(),
        "only the cloud provider"
    );
}

#[test]
fn a_bad_override_prices_nothing() {
    let usage = crate::llm::Usage {
        prompt_tokens: 10,
        completion_tokens: 10,
        ..Default::default()
    };
    let mut overrides = BTreeMap::new();
    overrides.insert(
        "openai/gpt-4o".to_string(),
        Price {
            input: f64::NAN,
            output: 1.,
            cached_input: None,
            cache_write: None,
        },
    );
    assert!(price("openai", "gpt-4o", &usage, &overrides).is_none());
    // A reported negative cost is ignored in favour of the price table.
    let reported = crate::llm::Usage {
        cost: Some(-1.),
        ..usage.clone()
    };
    assert!(price("openai", "gpt-4o", &reported, &BTreeMap::new()).unwrap() > 0.);
}

#[tokio::test]
async fn accounting_estimates_logs_and_summarises() {
    let root = std::env::temp_dir().join(format!("chanto-acct-{}", uuid::Uuid::new_v4().simple()));
    let mut config = crate::config::Config::default();
    config.pricing.insert(
        "acme/own".into(),
        Price {
            input: 1.,
            output: 1.,
            cached_input: None,
            cache_write: None,
        },
    );
    let mut a = Accounting::new(&config);
    a.root = root.clone();
    a.set_session("sess-1");
    let usage = crate::llm::Usage {
        prompt_tokens: 1_000_000,
        completion_tokens: 0,
        ..Default::default()
    };
    assert!(a.has_override("acme", "own"));
    assert_eq!(a.estimate("acme", "own", &usage), (Some(1.), "configured"));
    assert_eq!(
        a.estimate("openai", "gpt-4o", &usage),
        (Some(2.5), "catalog")
    );
    assert_eq!(a.estimate("nope", "nothing", &usage), (None, "default"));
    assert!(a.budgets().is_empty());
    assert!(a.warning().is_none());
    assert!(a.check().is_ok());

    a.model_call("acme", "own", Some(&usage), 0.5).await;
    assert_eq!(a.total(), 1.);
    assert!(!a.unknown());
    a.model_call("nope", "nothing", Some(&usage), 0.5).await;
    assert!(a.unknown());
    assert!(a.summary().ends_with(" + price unknown"), "{}", a.summary());
    a.model_call("acme", "own", None, 0.1).await;
    assert_eq!(a.total(), 1., "a failed call costs nothing");
    a.tool_call("Read", 0.01, true).await;

    let log = std::fs::read_to_string(log_path(&root)).unwrap();
    let events: Vec<Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(events.len(), 4);
    assert!(events.iter().all(|e| e["session_id"] == "sess-1"));
    assert_eq!(events[2]["level"], "error");
    assert_eq!(events[2]["data"]["status"], "error");
    assert_eq!(events[3]["event"], "tool_call");

    a.restore(2.5, false);
    assert_eq!(a.total(), 2.5);
    assert!(!a.summary().contains("unknown"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_zero_budget_stops_at_once() {
    let mut a = Accounting::new(&crate::config::Config {
        max_cost_usd: Some(0.),
        ..Default::default()
    });
    a.root = std::env::temp_dir().join(format!("chanto-zero-{}", uuid::Uuid::new_v4().simple()));
    assert!(a.check().is_err());
    a.state.lock().unwrap().unknown = true;
    let note = a.warning().unwrap();
    assert!(
        note.contains("80%") && note.contains("no known price"),
        "{note}"
    );
}

#[test]
fn old_logs_are_pruned() {
    let root = std::env::temp_dir().join(format!("chanto-prune-{}", uuid::Uuid::new_v4().simple()));
    let dir = root.join("logs");
    std::fs::create_dir_all(&dir).unwrap();
    let old = dir.join("chanto-2000-01-01.log");
    let other = dir.join("notes.txt");
    std::fs::write(&old, "x").unwrap();
    std::fs::write(&other, "x").unwrap();
    write_log(
        &root,
        "s",
        "info",
        "e",
        json!({"n": [1, "two", {"k": true}]}),
    )
    .unwrap();
    assert!(!old.exists() && other.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn otlp_payloads_carry_metrics_and_status() {
    let data = json!({"provider": "openai", "input_tokens": 10, "output_tokens": 5, "cost": 0.5, "ok": false, "skip": null});
    let attrs = attributes(&data);
    assert_eq!(attrs.len(), 5, "nulls are dropped");
    assert!(attrs.iter().any(|a| a["value"]["boolValue"] == false));
    let out = otlp("model_call", &data, 1.5, "svc");
    assert_eq!(out[0].0, "traces");
    assert_eq!(
        out[0].1["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["status"]["code"],
        2
    );
    let metrics = &out[1].1["resourceMetrics"][0]["scopeMetrics"][0]["metrics"];
    assert_eq!(metrics.as_array().unwrap().len(), 4);
    assert_eq!(metrics[3]["unit"], "USD");
    let ok = otlp("tool_call", &json!({"ok": true}), 0., "svc");
    assert_eq!(
        ok[0].1["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["status"]["code"],
        1
    );
}

#[test]
fn price_refresh_is_skipped_when_disabled() {
    // Disabled: returns before touching the cache or the network.
    start_price_refresh(false, true);
}
