use super::*;
use serde_json::json;
use std::sync::Arc;

fn opus(u: &Usage) -> Option<f64> {
    crate::cost::price("anthropic", "claude-opus-5-5", u, &BTreeMap::new())
}

#[test]
fn guard_labels_stay_short_but_say_what_fired() {
    for (cause, label) in [
        (
            "the same Read call ran 4 times in the main agent with the same result",
            "repeat",
        ),
        (
            "3 agents running in parallel; each resends its own context",
            "parallel",
        ),
        (
            "the MAIN's context is ~48000 tokens (60% of the window)",
            "context",
        ),
        ("~$0.42/min over the last 5 minutes", "burn rate"),
        ("using ~12% of your 5h window an hour", "rate"),
        (
            "src/lib.rs (~9000 tokens) has been read 5 times",
            "re-reads",
        ),
        ("something else entirely", "guard"),
    ] {
        assert_eq!(guard_label(cause), label, "{cause}");
    }
}

#[test]
fn estimates_are_cache_aware() {
    let shape = Shape {
        agents: 2,
        calls: 2,
        start_tokens: 10_000,
        growth: 1_000,
        output: 100,
    };
    let e = estimate(shape, "claude-opus-5-5", &opus, None);
    // Call 1 writes 10k to the cache ($5/M) + 100 out ($20/M); call 2 reads
    // 10k ($0.2/M), writes 1k, and 11k + 100 tokens are counted.
    let one = 10_000.0 * 5.0 / 1e6
        + 100.0 * 20.0 / 1e6
        + (10_000.0 * 0.2 + 1_000.0 * 5.0 + 100.0 * 20.0) / 1e6;
    assert!((e.usd.unwrap() - 2.0 * one).abs() < 1e-9, "{:?}", e.usd);
    assert_eq!(e.tokens, 2 * (10_100 + 11_100));
    // Without caching (every call at the input rate) it would cost more.
    let flat =
        |u: &Usage| Some((u.prompt_tokens as f64 * 4.0 + u.completion_tokens as f64 * 20.0) / 1e6);
    assert!(estimate(shape, "x", &flat, None).usd.unwrap() > e.usd.unwrap());
    assert!(
        e.amount().starts_with("~$0.")
            && e.summary()
                .contains("2 agents × ~2 calls on claude-opus-5-5"),
        "{}",
        e.summary()
    );
}

#[test]
fn unknown_prices_show_tokens_and_the_plan_share() {
    let none = |_: &Usage| None;
    let shape = Shape::new(4, 10, 20_000);
    let e = estimate(shape, "gpt-6.1-sol", &none, None);
    assert_eq!(e.usd, None);
    assert!(
        e.amount().contains("price unknown") && !e.amount().contains('$'),
        "{}",
        e.amount()
    );
    // A plan where 1% of the 5h window is ~100k tokens.
    let p = estimate(shape, "gpt-6.1-sol", &none, Some((100_000.0, "5h".into())));
    let pct = p.plan_percent.unwrap();
    assert!((pct - p.tokens as f64 / 100_000.0).abs() < 1e-9);
    assert!(p.amount().contains("% of your 5h window"), "{}", p.amount());
    // Local models are free and never over a threshold.
    let free = estimate(shape, "qwen", &|_: &Usage| Some(0.0), None);
    assert!(free.amount().contains("free") && !free.over(&SpendConfig::default()));
}

#[test]
fn the_confirmation_fires_above_the_threshold_only() {
    let cfg = SpendConfig::default();
    let shape = Shape::new(4, 15, 15_000);
    let big = estimate(shape, "claude-opus-5-5", &opus, None);
    assert!(big.usd.unwrap() > 2.0, "{:?}", big.usd);
    let small = estimate(Shape::new(2, 3, 5_000), "claude-opus-5-5", &opus, None);
    assert_eq!(
        decide(&cfg, &small, None, "start 2 subagents", true),
        Preflight::Quiet
    );
    let haiku = estimate(
        shape,
        "claude-haiku-5-5",
        &|u: &Usage| crate::cost::price("anthropic", "claude-haiku-5-5", u, &BTreeMap::new()),
        None,
    );
    match decide(&cfg, &big, Some(&haiku), "start 4 subagents", true) {
        Preflight::Ask { question, options } => {
            assert!(
                question.contains("start 4 subagents") && question.contains("$2.00"),
                "{question}"
            );
            let choices: Vec<_> = options.iter().map(|(_, c)| c.clone()).collect();
            assert_eq!(
                choices,
                vec![
                    Choice::Continue,
                    Choice::Cheaper("claude-haiku-5-5".into()),
                    Choice::Limit(2),
                    Choice::Cancel
                ]
            );
        }
        other => panic!("{other:?}"),
    }
    // Non-interactive: warn by default, stop when asked to; never silent.
    assert!(
        matches!(decide(&cfg, &big, None, "x", false), Preflight::Warn(m) if m.contains("Continuing"))
    );
    let stop = SpendConfig {
        non_interactive: Some("stop".into()),
        ..Default::default()
    };
    assert!(
        matches!(decide(&stop, &big, None, "x", false), Preflight::Stop(m) if m.contains("Not started"))
    );
    assert_eq!(decide(&stop, &small, None, "x", false), Preflight::Quiet);
    // A higher threshold lets it through; plan share and tokens have their own.
    let lax = SpendConfig {
        confirm_over_usd: Some(1000.0),
        ..Default::default()
    };
    assert_eq!(decide(&lax, &big, None, "x", true), Preflight::Quiet);
    let plan = estimate(shape, "m", &|_: &Usage| None, Some((50_000.0, "5h".into())));
    assert!(plan.over(&cfg), "{:?}", plan.plan_percent);
    let unknown = estimate(Shape::new(1, 2, 1_000), "m", &|_: &Usage| None, None);
    assert!(!unknown.over(&cfg));
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: "c".into(),
        name: name.into(),
        arguments: args,
    }
}

fn quiet_cfg() -> SpendConfig {
    SpendConfig {
        calls_per_agent: Some(10),
        ..Default::default()
    }
}

#[test]
fn detectors_fire_once_each() {
    let g = Guard::new(&quiet_cfg());
    // Parallel agents: at the threshold, once.
    assert!(g.parallel(3).is_none());
    assert!(
        g.parallel(5)
            .unwrap()
            .contains("5 agents running in parallel")
    );
    assert!(g.parallel(8).is_none(), "only once");
    // The same large file read again and again.
    let big = "x".repeat(40_000);
    let read = call("Read", json!({"file_path": "src/big.rs"}));
    assert!(g.tool_call(MAIN, &read, &big, false).is_empty());
    assert!(
        g.tool_call(MAIN, &read, &"y".repeat(40_000), false)
            .is_empty()
    );
    let n = g.tool_call(MAIN, &read, &"z".repeat(40_000), false);
    assert!(
        n.len() == 1 && n[0].contains("src/big.rs") && n[0].contains("3 times"),
        "{n:?}"
    );
    assert!(
        g.tool_call(MAIN, &read, &big, false)
            .iter()
            .all(|m| !m.contains("src/big.rs"))
    );
    // Small files can be read as often as needed.
    let g2 = Guard::new(&quiet_cfg());
    for i in 0..5 {
        assert!(
            g2.tool_call(
                MAIN,
                &call("Read", json!({"file_path": "a"})),
                &rand_text(10 + i),
                false
            )
            .is_empty()
        );
    }
    // A loop: the same call with the same result.
    let grep = call("Grep", json!({"pattern": "fn main"}));
    let mut fired = vec![];
    for _ in 0..6 {
        fired.extend(g.tool_call("agent-1", &grep, "src/main.rs:1", false));
    }
    assert_eq!(fired.len(), 1, "{fired:?}");
    assert!(fired[0].contains("4 times") && fired[0].contains("no progress"));
    // The same call with new results each time is progress.
    let g3 = Guard::new(&quiet_cfg());
    for i in 0..6 {
        assert!(
            g3.tool_call(MAIN, &grep, &format!("result {i}"), false)
                .is_empty()
        );
    }
}

fn rand_text(n: usize) -> String {
    "a".repeat(n)
}

#[test]
fn huge_resent_context_and_burn_rate_fire_once() {
    let g = Guard::new(&quiet_cfg());
    let t0 = Instant::now();
    let u = Usage {
        prompt_tokens: 150_000,
        cached_tokens: 140_000,
        completion_tokens: 500,
        ..Default::default()
    };
    let mut notes = vec![];
    for i in 0..8 {
        notes.extend(g.model_call_at(
            MAIN,
            "claude-opus-5-5",
            &u,
            Some(0.05),
            200_000,
            t0 + Duration::from_secs(i * 20),
        ));
    }
    let resent: Vec<_> = notes
        .iter()
        .filter(|n| n.contains("resent on every call"))
        .collect();
    assert_eq!(resent.len(), 1, "{notes:?}");
    assert!(
        resent[0].contains("150k") && resent[0].contains("75%"),
        "{}",
        resent[0]
    );
    // $0.40 over ~2.3 minutes is under $0.50/min: no burn notice.
    assert!(!notes.iter().any(|n| n.contains("/min over")), "{notes:?}");
    // Several agents spending fast: one burn notice.
    let mut burn = vec![];
    for i in 0..20 {
        let key = format!("agent-{}", i % 5);
        burn.extend(g.model_call_at(
            &key,
            "claude-opus-5-5",
            &Usage {
                prompt_tokens: 20_000,
                ..Default::default()
            },
            Some(0.2),
            200_000,
            t0 + Duration::from_secs(160 + i * 3),
        ));
    }
    let b: Vec<_> = burn.iter().filter(|n| n.contains("/min over")).collect();
    assert_eq!(b.len(), 1, "{burn:?}");
    assert!(b[0].contains("across"), "{}", b[0]);
    // Small contexts never trip the resent detector.
    let g2 = Guard::new(&quiet_cfg());
    for i in 0..10 {
        assert!(
            g2.model_call_at(
                MAIN,
                "m",
                &Usage {
                    prompt_tokens: 5_000,
                    ..Default::default()
                },
                Some(0.001),
                200_000,
                t0 + Duration::from_secs(i * 30)
            )
            .is_empty()
        );
    }
}

#[test]
fn plan_windows_teach_tokens_per_percent() {
    use crate::llm::limits::{Snapshot, Window};
    let g = Guard::new(&quiet_cfg());
    let snap = |p: f64| Snapshot {
        primary: Some(Window {
            used_percent: p,
            window_minutes: Some(300),
            resets_at: None,
        }),
        secondary: None,
        plan_type: Some("plus".into()),
    };
    g.limits(&snap(10.0));
    g.model_call(
        MAIN,
        "gpt-6.1-sol",
        &Usage {
            prompt_tokens: 190_000,
            completion_tokens: 10_000,
            ..Default::default()
        },
        None,
        400_000,
    );
    g.limits(&snap(12.0));
    let (per, w) = g.plan_rate().unwrap();
    assert!((per - 100_000.0).abs() < 1e-6 && w == "5h", "{per} {w}");
    // A reset (the percentage drops) starts a new anchor rather than a negative rate.
    g.limits(&snap(1.0));
    assert!((g.plan_rate().unwrap().0 - 100_000.0).abs() < 1e-6);
}

#[test]
fn cost_breakdown_explains_a_recorded_session() {
    let g = Guard::new(&quiet_cfg());
    let t0 = Instant::now();
    g.agent_started(
        "t1",
        "Explore",
        "map the parser",
        "anthropic/claude-opus-5-5",
    );
    g.agent_started(
        "t2",
        "Explore",
        "map the lexer",
        "anthropic/claude-opus-5-5",
    );
    g.parallel(2);
    let main = Usage {
        prompt_tokens: 180_000,
        cached_tokens: 170_000,
        completion_tokens: 800,
        ..Default::default()
    };
    for i in 0..6 {
        let c = opus(&main);
        g.model_call_at(
            MAIN,
            "anthropic/claude-opus-5-5",
            &main,
            c,
            200_000,
            t0 + Duration::from_secs(i * 60),
        );
    }
    let sub = Usage {
        prompt_tokens: 30_000,
        cached_tokens: 25_000,
        cache_write_tokens: 5_000,
        completion_tokens: 600,
        ..Default::default()
    };
    for k in ["t1", "t2"] {
        for i in 0..5 {
            g.model_call_at(
                k,
                "anthropic/claude-opus-5-5",
                &sub,
                opus(&sub),
                200_000,
                t0 + Duration::from_secs(i * 10),
            );
        }
    }
    g.tool_call(
        "t1",
        &call("Read", json!({"file_path": "src/parser.rs"})),
        &"p".repeat(30_000),
        false,
    );
    g.tool_call(
        "t2",
        &call("Read", json!({"file_path": "src/parser.rs"})),
        &"p".repeat(30_000),
        false,
    );
    let reprice = |m: &str, u: &Usage| {
        crate::cost::price(
            "anthropic",
            m.trim_start_matches("anthropic/"),
            u,
            &BTreeMap::new(),
        )
    };
    let r = g.report(200_000, &reprice, &|m: &str| sibling(m));
    assert!(
        r.contains("## By agent")
            && r.contains("Explore: map the parser")
            && r.contains("main agent"),
        "{r}"
    );
    assert!(
        r.contains("## By model") && r.contains("anthropic/claude-opus-5-5"),
        "{r}"
    );
    assert!(
        r.contains("## By tool") && r.contains("Read: 2 calls"),
        "{r}"
    );
    assert!(r.contains("src/parser.rs × 2"), "{r}");
    assert!(
        r.contains("Subagents ran on anthropic/claude-opus-5-5")
            && r.contains("anthropic/claude-sonnet-5-5 would have cut that by ~"),
        "{r}"
    );
    assert!(
        r.contains("Context was resent 6 times at ~180k tokens"),
        "{r}"
    );
    assert!(r.contains("Up to 2 agents ran in parallel"), "{r}");
}

#[test]
fn learned_calls_need_a_few_samples() {
    let mut l = Learned::default();
    for c in [10, 20, 30] {
        l.add_agent(c);
    }
    assert!(
        (l.subagent_calls.unwrap() - 20.0).abs() < 1e-9,
        "{:?}",
        l.subagent_calls
    );
    assert_eq!(
        sibling("anthropic/claude-opus-5-5").as_deref(),
        Some("anthropic/claude-sonnet-5-5")
    );
    assert_eq!(sibling("gpt-6-luna"), None);
}

#[test]
fn today_sums_the_log_by_model() {
    let log = [
        json!({"event": "model_call", "data": {"provider": "anthropic", "model": "m", "input_tokens": 1000, "output_tokens": 10, "cost": 0.5, "status": "ok"}}),
        json!({"event": "model_call", "data": {"provider": "anthropic", "model": "m", "input_tokens": 1000, "output_tokens": 10, "cost": 0.25, "status": "ok"}}),
        json!({"event": "model_call", "data": {"provider": "x", "model": "y", "input_tokens": 5, "output_tokens": 5, "cost": null, "status": "ok"}}),
        json!({"event": "tool_call", "data": {}}),
    ]
    .map(|v| v.to_string())
    .join("\n");
    let t = today(&log);
    assert!(
        t.contains("$0.75") && t.contains("anthropic/m**: 2 calls") && t.contains("price unknown"),
        "{t}"
    );
}

#[tokio::test]
async fn the_parallelism_cap_holds_agents_back() {
    let g = Guard::new(&quiet_cfg());
    g.set_cap(1);
    let cancel = tokio_util::sync::CancellationToken::new();
    let a = g.slot(&cancel).await.unwrap();
    let waited = tokio::time::timeout(Duration::from_millis(100), g.slot(&cancel)).await;
    assert!(waited.is_err(), "a second agent waits");
    drop(a);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), g.slot(&cancel))
            .await
            .is_ok()
    );
}

/// A frontend that answers the spend question with the option starting
/// with `pick`, and keeps what it was asked and shown.
struct Picker {
    pick: &'static str,
    asked: Mutex<Vec<(String, Vec<String>)>>,
    events: Mutex<Vec<Event>>,
}

#[async_trait::async_trait]
impl crate::agent::Frontend for Picker {
    fn emit(&self, e: Event) {
        self.events.lock().unwrap().push(e);
    }
    async fn permission(
        &self,
        _: crate::agent::PermissionRequest,
    ) -> crate::agent::PermissionReply {
        crate::agent::PermissionReply::Allow
    }
    async fn question(&self, q: &str, options: &[String]) -> Option<String> {
        self.asked
            .lock()
            .unwrap()
            .push((q.to_string(), options.to_vec()));
        options.iter().find(|o| o.starts_with(self.pick)).cloned()
    }
}

/// A turn that launches three Task agents at once, against a mock server
/// whose calls are priced high enough to cross the threshold.
async fn fan_out(
    pick: &'static str,
    interactive: bool,
    policy: Option<&str>,
    confirm_usd: f64,
) -> (Arc<Picker>, Arc<Mutex<Vec<serde_json::Value>>>, String) {
    use crate::agent::tests::{mock_server_routed, tmpdir};
    let calls: Vec<_> = ["A", "B", "C"]
        .iter()
        .enumerate()
        .map(|(i, k)| json!({"index": i, "id": format!("t{k}"), "type": "function", "function": {"name": "Task", "arguments": json!({"description": format!("dig {k}"), "prompt": format!("dig {k}"), "subagent_type": "general-purpose"}).to_string()}}))
        .collect();
    let (base, seen) = mock_server_routed(vec![
        (
            "fan out",
            vec![json!({"tool_calls": calls}), json!({"content": "all done"})],
        ),
        ("dig A", vec![json!({"content": "A found"})]),
        ("dig B", vec![json!({"content": "B found"})]),
        ("dig C", vec![json!({"content": "C found"})]),
    ])
    .await;
    let dir = tmpdir("spend-fanout");
    let mut config = crate::config::Config::default();
    config.provider = crate::llm::ProviderKind::Openai;
    // The mock server is local; price it like an expensive hosted model.
    config.pricing.insert(
        "local/mock".into(),
        crate::cost::Price {
            input: 50.0,
            output: 100.0,
            cached_input: Some(5.0),
            cache_write: Some(60.0),
        },
    );
    config.spend = SpendConfig {
        calls_per_agent: Some(15),
        confirm_over_usd: Some(confirm_usd),
        non_interactive: policy.map(str::to_string),
        ..Default::default()
    };
    let fe = Arc::new(Picker {
        pick,
        asked: Default::default(),
        events: Default::default(),
    });
    let env = Env::new(crate::agent::EnvInit {
        config,
        provider: crate::llm::Provider::new(crate::llm::ProviderKind::Openai, &base, None, 32768),
        model: "mock".into(),
        mode: crate::permissions::Mode::Bypass,
        vision: false,
        cwd: dir.clone(),
        tools: crate::tools::Registry::builtin(),
        mcp: vec![],
        frontend: fe.clone(),
        interactive,
    });
    let mut agent = crate::agent::Agent::new(env.clone());
    let end = agent
        .run_turn("fan out please", vec![], CancellationToken::new())
        .await
        .unwrap();
    let text = match end {
        crate::agent::TurnEnd::Done(t) => t,
        _ => panic!("interrupted"),
    };
    let tool_results: Vec<String> = agent
        .messages
        .iter()
        .filter(|m| m.role == crate::llm::Role::Tool)
        .map(|m| String::clone(&m.content))
        .collect();
    let _ = std::fs::remove_dir_all(dir);
    (fe, seen, format!("{text}\n{}", tool_results.join("\n")))
}

fn subagent_requests(seen: &Mutex<Vec<serde_json::Value>>) -> usize {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|b| b["messages"].to_string().contains("dig "))
        .filter(|b| !b["messages"].to_string().contains("fan out"))
        .count()
}

#[tokio::test]
async fn a_fan_out_over_the_threshold_asks_first() {
    let (fe, seen, out) = fan_out("Cancel", true, None, 2.0).await;
    let asked = fe.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(
        asked[0].0.contains("starting 3 subagents")
            && asked[0].0.contains("over your $2.00 confirm threshold"),
        "{}",
        asked[0].0
    );
    assert!(
        asked[0].1.iter().any(|o| o.starts_with("Continue (~$")),
        "{:?}",
        asked[0].1
    );
    assert!(
        asked[0]
            .1
            .iter()
            .any(|o| o.starts_with("Run at most 2 at a time")),
        "{:?}",
        asked[0].1
    );
    // Cancelled: no subagent ever called the model, and the model was told why.
    assert_eq!(subagent_requests(&seen), 0);
    assert!(
        out.contains("Not started: the user cancelled starting 3 subagents"),
        "{out}"
    );

    // Continue: they run, and the ledger knows them.
    let (fe, seen, out) = fan_out("Continue", true, None, 2.0).await;
    assert_eq!(fe.asked.lock().unwrap().len(), 1);
    assert_eq!(subagent_requests(&seen), 3, "{out}");
    assert!(out.contains("A found") && out.contains("C found"), "{out}");

    // Below the threshold nobody is asked.
    let (fe, seen, _) = fan_out("Cancel", true, None, 1_000_000.0).await;
    assert!(fe.asked.lock().unwrap().is_empty());
    assert_eq!(subagent_requests(&seen), 3);
}

#[tokio::test]
async fn unattended_fan_outs_follow_the_policy_and_say_so() {
    // Default policy: warn and continue.
    let (fe, seen, _) = fan_out("Cancel", false, None, 2.0).await;
    assert!(fe.asked.lock().unwrap().is_empty());
    assert_eq!(subagent_requests(&seen), 3);
    let warned = fe.events.lock().unwrap().iter().any(
        |e| matches!(e, Event::Warn(w) if w.contains("Spend check") && w.contains("Continuing")),
    );
    assert!(warned, "a non-interactive run must not continue silently");
    // "stop": not started, with a warning and the reason for the model.
    let (fe, seen, out) = fan_out("Cancel", false, Some("stop"), 2.0).await;
    assert_eq!(subagent_requests(&seen), 0);
    assert!(
        out.contains("Not started ([spend] non_interactive = \"stop\")"),
        "{out}"
    );
    assert!(
        fe.events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, Event::Warn(w) if w.contains("Not started")))
    );
}

#[tokio::test]
async fn the_cheaper_choice_moves_the_agents() {
    let g = Guard::new(&quiet_cfg());
    assert_eq!(g.subagent_model(), None);
    g.set_subagent_model(Some("anthropic/claude-haiku-5-5".into()));
    assert_eq!(
        g.subagent_model().as_deref(),
        Some("anthropic/claude-haiku-5-5")
    );
    g.set_cap(2);
    assert_eq!(g.cap(), 2);
}
