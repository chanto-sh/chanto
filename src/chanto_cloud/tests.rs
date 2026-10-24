use super::agents::{RunArgs, SpecArgs, exit_code, parse_file, parse_trigger, start};
use super::*;
use crate::runs::cloud::Cloud;
use axum::Json;
use axum::routing::{get, post};
use chanto_proto::agents::AgentTrigger;
use chanto_proto::runs::{Placement, Run, RunStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

fn run(status: RunStatus) -> Run {
    serde_json::from_value(json!({
        "id": "r1", "org": "acme", "status": status, "repo": "https://github.com/acme/api.git", "git_ref": null, "task": "t",
        "placement": "auto", "labels": [], "runner_kind": "hosted", "attempts": 1, "cancel_requested": false,
        "created_at": "2026-10-14T10:00:00Z", "started_at": "2026-10-14T10:00:05Z",
        "finished_at": if status.is_terminal() { json!("2026-10-14T10:03:05Z") } else { Value::Null },
        "result": if status.is_terminal() { json!({"summary": "Labelled 3 issues.", "branch": "chanto/r1", "pr_url": "https://github.com/acme/api/pull/7"}) } else { Value::Null },
        "agent": "triage",
    }))
    .unwrap()
}

#[test]
fn errors_say_what_to_do() {
    assert_eq!(
        error_message(r#"{"error":"quota_exceeded","message":"no trial credit left"}"#),
        "no trial credit left"
    );
    assert_eq!(
        error_message(
            r#"{"type":"error","error":{"type":"billing_error","message":"spending limit"}}"#
        ),
        "spending limit"
    );
    assert!(
        gateway_error(
            402,
            r#"{"error":{"message":"the org has reached this month's spending limit"}}"#
        )
        .contains("spending limit")
    );
    assert!(gateway_error(402, "{}").contains("raise its spending limit"));
    assert!(
        gateway_error(401, r#"{"error":{"message":"bad token"}}"#).contains("chanto cloud login")
    );
    assert!(gateway_error(404, r#"{"type":"error","error":{"type":"not_found_error","message":"the model gateway isn't enabled on this server"}}"#)
        .contains("isn't enabled"));
}

#[test]
fn triggers_parse_from_flags() {
    assert_eq!(
        parse_trigger("issue_opened").unwrap(),
        AgentTrigger::IssueOpened
    );
    assert_eq!(
        parse_trigger("issue-labeled:bug").unwrap(),
        AgentTrigger::IssueLabeled {
            label: Some("bug".into())
        }
    );
    assert_eq!(
        parse_trigger("issue_labeled").unwrap(),
        AgentTrigger::IssueLabeled { label: None }
    );
    assert_eq!(
        parse_trigger("pull_request_updated").unwrap(),
        AgentTrigger::PullRequestUpdated
    );
    assert!(parse_trigger("mention:x").is_err() && parse_trigger("push").is_err());
}

#[test]
fn agent_files_and_flags() {
    let toml = r#"
        name = "triage"
        instructions = "Label new issues."
        repo = "https://github.com/acme/api.git"
        branch = "main"
        placement = "self-hosted"
        schedule = "0 9 * * 1-5"
        triggers = ["issue_opened", { on = "issue_labeled", label = "bug" }]
    "#;
    let spec = parse_file(toml, true, None).unwrap();
    assert_eq!(spec.git_ref.as_deref(), Some("main"));
    assert_eq!(spec.placement, Some(Placement::SelfHosted));
    assert_eq!(
        spec.triggers,
        vec![
            AgentTrigger::IssueOpened,
            AgentTrigger::IssueLabeled {
                label: Some("bug".into())
            }
        ]
    );
    // `show --json` output is a valid file; the command line names it.
    let shown = json!({"id": "x", "org": "acme", "name": "old", "instructions": "i", "repo": "r", "enabled": false, "last_run": null, "created_at": "2026-01-01T00:00:00Z"});
    let spec = parse_file(&shown.to_string(), false, Some("new")).unwrap();
    assert!(spec.name == "new" && !spec.enabled);

    let args = SpecArgs::try_parse_from([
        "x",
        "--on",
        "pull_request_opened",
        "--on",
        "mention",
        "--label",
        "gpu",
        "--schedule",
        "none",
        "--disable",
        "--placement",
        "hosted",
    ])
    .unwrap();
    let mut base = parse_file(toml, true, None).unwrap();
    base.labels = vec!["old".into()];
    let out = args.apply(base).unwrap();
    assert_eq!(
        out.triggers,
        vec![AgentTrigger::PullRequestOpened, AgentTrigger::Mention]
    );
    assert!(
        out.labels == ["gpu"]
            && out.schedule.is_none()
            && !out.enabled
            && out.placement == Some(Placement::Hosted)
    );
    let none = SpecArgs::try_parse_from(["x", "--on", "none"])
        .unwrap()
        .apply(out)
        .unwrap();
    assert!(
        none.triggers.is_empty() && none.labels == ["gpu"],
        "lists not given stay"
    );
    assert!(
        SpecArgs::try_parse_from(["x", "--instructions", "a", "--instructions-file", "b"]).is_err()
    );
}

/// `SpecArgs` as a standalone parser, for the tests above.
impl SpecArgs {
    fn try_parse_from<const N: usize>(args: [&str; N]) -> Result<SpecArgs, clap::Error> {
        #[derive(clap::Parser)]
        struct P {
            #[command(flatten)]
            s: SpecArgs,
        }
        <P as clap::Parser>::try_parse_from(args).map(|p| p.s)
    }
}

/// A server for one run of agent `triage` in `acme`: `statuses` are what
/// `GET /v1/runs/r1` answers in turn (the last one repeats).
async fn run_server(statuses: Vec<RunStatus>) -> (Cloud, Arc<std::sync::Mutex<Vec<String>>>) {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicU32::new(0));
    let (s1, s2) = (seen.clone(), seen.clone());
    let statuses = Arc::new(statuses);
    let app = axum::Router::new()
        .route("/v1/orgs/acme/agents/triage/runs", post(move |headers: axum::http::HeaderMap, Json(b): Json<Value>| async move {
            s1.lock().unwrap().push(format!("start {} {}", headers["authorization"].to_str().unwrap(), b));
            Json(run(RunStatus::Queued))
        }))
        .route("/v1/runs/r1", get(move || async move {
            let i = calls.fetch_add(1, Ordering::SeqCst) as usize;
            Json(run(statuses[i.min(statuses.len() - 1)]))
        }))
        .route("/v1/runs/r1/events", get(move |q: axum::extract::Query<std::collections::HashMap<String, String>>| async move {
            s2.lock().unwrap().push(format!("events after {}", q["after"]));
            let events = if q["after"] == "0" {
                json!([{"seq": 1, "at": "2026-10-14T10:00:06Z", "data": {"type": "tool_start", "id": "t", "label": "Read README.md", "parent": null}},
                       {"seq": 2, "at": "2026-10-14T10:00:07Z", "data": {"type": "text", "text": "Done."}}])
            } else {
                json!([])
            };
            Json(json!({"events": events}))
        }));
    (Cloud::new(&serve(app).await, "cb_tok").unwrap(), seen)
}

#[tokio::test]
async fn run_wait_exits_zero_only_when_the_run_succeeds() {
    let args = RunArgs::default_wait();
    let (c, seen) = run_server(vec![RunStatus::Running, RunStatus::Succeeded]).await;
    assert_eq!(
        start(&c, "acme", "triage", args.clone(), false)
            .await
            .unwrap(),
        0
    );
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen[0],
        r#"start Bearer cb_tok {"git_ref":"main","input":"look at #42"}"#
    );
    assert!(
        seen.contains(&"events after 2".to_string()),
        "events resume after the last seq: {seen:?}"
    );

    let (c, _) = run_server(vec![RunStatus::Running, RunStatus::Failed]).await;
    assert_eq!(
        start(&c, "acme", "triage", args.clone(), false)
            .await
            .unwrap(),
        1
    );
    // --json waits quietly; cancelled is a failure too.
    let (c, seen) = run_server(vec![RunStatus::Queued, RunStatus::Cancelled]).await;
    assert_eq!(start(&c, "acme", "triage", args, true).await.unwrap(), 1);
    assert!(!seen.lock().unwrap().iter().any(|s| s.starts_with("events")));
    assert_eq!(exit_code(None), 130, "stopped following");
}

impl RunArgs {
    fn default_wait() -> RunArgs {
        #[derive(clap::Parser)]
        struct P {
            #[command(flatten)]
            r: RunArgs,
        }
        <P as clap::Parser>::try_parse_from([
            "x",
            "--input",
            "look at #42",
            "--ref",
            "main",
            "--wait",
        ])
        .unwrap()
        .r
    }
}

#[tokio::test]
async fn provider_sends_the_token_and_org_and_explains_402() {
    use crate::llm::mock::{serve, sse};
    use crate::llm::{ChatRequest, Message, ProviderKind};
    let ok = sse(&[
        (
            "message_start",
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 3, "output_tokens": 1}}}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}),
        ),
        (
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ]);
    let limit = r#"{"type":"error","error":{"type":"billing_error","message":"the org has reached this month's spending limit"}}"#.to_string();
    let (base, seen) = serve(vec![(200, ok), (402, limit), (200, "{}".into())]).await;
    let req = |model: &str| ChatRequest {
        model: model.into(),
        messages: vec![Message::user("hi")],
        tools: vec![],
        think: None,
        effort: None,
        temperature: None,
        max_tokens: Some(100),
        json: false,
    };

    let p = provider_for(&base, "cb_tok", Some("acme".into()), "claude-sonnet-4-5");
    assert_eq!(p.kind, ProviderKind::Anthropic);
    assert_eq!(
        p.chat(&req("claude-sonnet-4-5"), &mut |_| {})
            .await
            .unwrap()
            .text,
        "hi"
    );
    let e = p
        .chat(&req("claude-sonnet-4-5"), &mut |_| {})
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("spending limit") && e.contains("chanto cloud status"),
        "{e}"
    );
    assert!(
        crate::llm::failure_hint(&e, ProviderKind::Anthropic, &base, "m", Some(PROVIDER_ID))
            .is_none(),
        "the message already says what to do"
    );
    // Other models go to the OpenAI-compatible route; no org: no header.
    let p = provider_for(&base, "cb_tok", None, "coder");
    assert_eq!(p.kind, ProviderKind::Openai);
    let _ = p.chat(&req("coder"), &mut |_| {}).await;

    let seen = seen.lock().unwrap();
    let h = seen.headers[0].to_ascii_lowercase();
    assert!(
        h.starts_with("post /v1/model/anthropic/v1/messages "),
        "{h}"
    );
    assert!(
        h.contains("x-api-key: cb_tok") && h.contains("x-chanto-org: acme"),
        "{h}"
    );
    let h = seen.headers[2].to_ascii_lowercase();
    assert!(
        h.starts_with("post /v1/model/openai/v1/chat/completions ")
            && h.contains("authorization: bearer cb_tok")
            && !h.contains("x-chanto-org"),
        "{h}"
    );
}

#[tokio::test]
async fn provider_lists_the_gateway_catalog() {
    let app = axum::Router::new().route("/v1/gateway/models", get(|headers: axum::http::HeaderMap| async move {
        assert_eq!(headers["authorization"], "Bearer cb_tok");
        Json(json!({"models": [{"id": "claude-sonnet-4-5", "api": "anthropic", "default": true}, {"id": "coder", "api": "openai", "default": false}]}))
    }));
    let url = serve(app).await;
    assert_eq!(
        provider_for(&url, "cb_tok", None, "")
            .list_models()
            .await
            .unwrap(),
        ["claude-sonnet-4-5", "coder"]
    );
}
