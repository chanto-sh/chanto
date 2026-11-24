//! `chanto agents` against a fake cloud API, in a child process so the token
//! and URL can come from the environment as they do in CI.

use super::*;
use crate::test_support::isolated;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::json;
use std::sync::{Arc, Mutex};

type Calls = Arc<Mutex<Vec<(String, String, Value)>>>;

const AT: &str = "2026-01-02T03:04:05Z";

fn agent_json(name: &str) -> Value {
    json!({
        "id": "ag1", "org": "acme", "name": name, "instructions": "do it",
        "repo": "https://git.test/r.git", "enabled": true,
        "next_run_at": null, "last_run": null, "last_error": null,
        "created_at": AT, "updated_at": AT
    })
}

fn run_json(status: &str) -> Value {
    json!({
        "id": "run1", "org": "acme", "status": status, "repo": "https://git.test/r.git",
        "git_ref": null, "task": "t", "placement": "auto", "labels": [],
        "runner_kind": null, "attempts": 0, "cancel_requested": false,
        "created_at": AT, "started_at": null, "finished_at": null, "result": null
    })
}

async fn cloud() -> (String, Calls) {
    let calls: Calls = Default::default();
    let c2 = calls.clone();
    let app = axum::Router::new().fallback(move |req: Request| {
        let calls = c2.clone();
        async move {
            let (method, path) = (req.method().to_string(), req.uri().to_string());
            let auth = req
                .headers()
                .get("authorization")
                .map(|v| v.to_str().unwrap().to_string());
            assert_eq!(auth.as_deref(), Some("Bearer cloud-token"));
            let bytes = axum::body::to_bytes(req.into_body(), 1 << 20)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            calls
                .lock()
                .unwrap()
                .push((method.clone(), path.clone(), body.clone()));
            let path = path.split('?').next().unwrap().to_string();
            match (method.as_str(), path.as_str()) {
                ("GET", "/v1/orgs/acme/agents") => {
                    axum::Json(json!({"agents": [agent_json("first")]})).into_response()
                }
                ("GET", "/v1/orgs/acme/agents/missing") => (
                    StatusCode::NOT_FOUND,
                    axum::Json(json!({"error": "no such agent"})),
                )
                    .into_response(),
                ("GET", p) if p.starts_with("/v1/orgs/acme/agents/") => {
                    axum::Json(agent_json("first")).into_response()
                }
                ("POST", "/v1/orgs/acme/agents") | ("PUT", _) => {
                    let mut a = agent_json("x");
                    if let Some(o) = a.as_object_mut() {
                        for (k, v) in body.as_object().unwrap() {
                            o.insert(k.clone(), v.clone());
                        }
                    }
                    axum::Json(a).into_response()
                }
                ("DELETE", _) => StatusCode::NO_CONTENT.into_response(),
                ("POST", p) if p.ends_with("/runs") => {
                    axum::Json(run_json("queued")).into_response()
                }
                ("GET", "/v1/orgs/acme/runs") => {
                    axum::Json(json!({"runs": [run_json("succeeded")]})).into_response()
                }
                ("GET", p) if p.contains("/runs/run1") => {
                    axum::Json(run_json("failed")).into_response()
                }
                ("GET", "/v1/orgs") => {
                    axum::Json(json!({"orgs": [{"slug": "acme", "name": "Acme"}]})).into_response()
                }
                _ => (StatusCode::IM_A_TEAPOT, format!("{method} {path}")).into_response(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    // SAFETY: the test runs alone in its own child process.
    unsafe {
        std::env::set_var("CHANTO_CLOUD_TOKEN", "cloud-token");
        std::env::set_var("CHANTO_CLOUD_URL", &base);
        std::env::remove_var("CHANTO_CLOUD_ORG");
    }
    (base, calls)
}

async fn agents(args: &[&str]) -> Result<i32> {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    cli(&args).await
}

fn last(calls: &Calls) -> (String, String, Value) {
    calls.lock().unwrap().last().cloned().unwrap()
}

#[tokio::test]
async fn each_command_makes_its_request() {
    if isolated("chanto_cloud::agents::cli_tests::each_command_makes_its_request") {
        return;
    }
    let (_, calls) = cloud().await;
    let dir = crate::agent::tests::tmpdir("agents-cli");

    // No subcommand lists, and the personal org is used when none is named.
    assert_eq!(agents(&[]).await.unwrap(), 0);
    let (m, p, _) = last(&calls);
    assert_eq!((m.as_str(), p.as_str()), ("GET", "/v1/orgs/acme/agents"));
    assert_eq!(
        agents(&["list", "--org", "acme", "--json"]).await.unwrap(),
        0
    );
    assert_eq!(
        agents(&["show", "first", "--org", "acme"]).await.unwrap(),
        0
    );
    assert_eq!(
        agents(&["show", "first", "--org", "acme", "--json"])
            .await
            .unwrap(),
        0
    );

    let e = agents(&["show", "missing", "--org", "acme"])
        .await
        .unwrap_err();
    assert!(
        format!("{e:#}").contains("agent missing in org acme"),
        "{e:#}"
    );

    // Flags build the spec, including its lists and clearing values.
    assert_eq!(
        agents(&[
            "create",
            "nightly",
            "--org",
            "acme",
            "--repo",
            " https://git.test/r.git ",
            "--instructions",
            "tidy up",
            "--description",
            "d",
            "--schedule",
            "0 3 * * *",
            "--on",
            "issue_labeled:bug",
            "--on",
            "mention",
            "--placement",
            "self-hosted",
            "--label",
            "gpu",
            "--branch",
            "dev",
            "--disable",
        ])
        .await
        .unwrap(),
        0
    );
    let (m, _, body) = last(&calls);
    assert_eq!(m, "POST");
    assert_eq!(body["name"], "nightly");
    assert_eq!(body["repo"], "https://git.test/r.git", "trimmed");
    assert_eq!(body["instructions"], "tidy up");
    assert_eq!(body["schedule"], "0 3 * * *");
    assert_eq!(body["labels"], json!(["gpu"]));
    assert_eq!(body["enabled"], false);
    assert_eq!(body["placement"], "self_hosted");
    assert_eq!(body["triggers"].as_array().unwrap().len(), 2);

    // Bad specs never reach the server.
    let before = calls.lock().unwrap().len();
    for (args, needle) in [
        (
            vec![
                "create",
                "Bad Name",
                "--org",
                "acme",
                "--repo",
                "r",
                "--instructions",
                "i",
            ],
            "agent names",
        ),
        (
            vec!["create", "ok", "--org", "acme", "--instructions", "i"],
            "needs a repository",
        ),
        (
            vec!["create", "ok", "--org", "acme", "--repo", "r"],
            "needs instructions",
        ),
        (
            vec![
                "create",
                "ok",
                "--org",
                "acme",
                "--repo",
                "r",
                "--instructions",
                "i",
                "--placement",
                "mars",
            ],
            "unknown placement",
        ),
        (
            vec![
                "create",
                "ok",
                "--org",
                "acme",
                "--repo",
                "r",
                "--instructions",
                "i",
                "--on",
                "never",
            ],
            "never",
        ),
        (
            vec![
                "create",
                "ok",
                "--org",
                "acme",
                "--repo",
                "r",
                "--instructions-file",
                "/no/such/file",
            ],
            "reading /no/such/file",
        ),
        (
            vec![
                "create",
                "ok",
                "--org",
                "acme",
                "--file",
                "/no/such/agent.toml",
            ],
            "reading /no/such/agent.toml",
        ),
        (vec!["edit", "first", "--org", "acme"], "nothing to change"),
        (vec!["delete", "first", "--org", "acme"], "needs --yes"),
    ] {
        let e = agents(&args).await.unwrap_err();
        assert!(format!("{e:#}").contains(needle), "{args:?}: {e:#}");
    }
    assert_eq!(calls.lock().unwrap().len(), before);

    // A file gives the spec, and flags go on top. The instructions can be a file too.
    let file = dir.join("agent.toml");
    std::fs::write(
        &file,
        "name = \"from-file\"\nrepo = \"https://git.test/f.git\"\ninstructions = \"file text\"\n",
    )
    .unwrap();
    let text = dir.join("instr.md");
    std::fs::write(&text, "instructions from a file").unwrap();
    agents(&[
        "create",
        "--org",
        "acme",
        "--file",
        file.to_str().unwrap(),
        "--instructions-file",
        text.to_str().unwrap(),
    ])
    .await
    .unwrap();
    let (_, _, body) = last(&calls);
    assert_eq!(body["name"], "from-file");
    assert_eq!(body["repo"], "https://git.test/f.git");
    assert_eq!(body["instructions"], "instructions from a file");

    // Editing changes only what's given, on top of the current spec; `none` clears.
    agents(&[
        "edit",
        "first",
        "--org",
        "acme",
        "--rename",
        "second",
        "--description",
        "none",
        "--label",
        "none",
        "--on",
        "none",
        "--enable",
    ])
    .await
    .unwrap();
    let (m, p, body) = last(&calls);
    assert_eq!(
        (m.as_str(), p.as_str()),
        ("PUT", "/v1/orgs/acme/agents/first")
    );
    assert_eq!(body["name"], "second");
    assert_eq!(body["instructions"], "do it", "kept");
    assert_eq!(body["labels"], json!([]));

    agents(&["rm", "first", "--org", "acme", "--yes"])
        .await
        .unwrap();
    let (m, p, _) = last(&calls);
    assert_eq!(
        (m.as_str(), p.as_str()),
        ("DELETE", "/v1/orgs/acme/agents/first")
    );

    agents(&["runs", "first", "--org", "acme", "--limit", "3"])
        .await
        .unwrap();
    let (_, p, _) = last(&calls);
    assert_eq!(p, "/v1/orgs/acme/runs?agent=first&limit=3");
    agents(&["runs", "first", "--org", "acme", "--json"])
        .await
        .unwrap();

    // A run sends its input; blank ones are dropped.
    agents(&[
        "run", "first", "--org", "acme", "--input", "issue 42", "--ref", "  ",
    ])
    .await
    .unwrap();
    let (m, p, body) = last(&calls);
    assert_eq!(
        (m.as_str(), p.as_str()),
        ("POST", "/v1/orgs/acme/agents/first/runs")
    );
    assert_eq!(body["input"], "issue 42");
    assert_eq!(body["git_ref"], Value::Null);
    agents(&["run", "first", "--org", "acme", "--json"])
        .await
        .unwrap();

    // Names go in the URL as one segment, whatever they contain.
    let _ = agents(&["show", "a/../b", "--org", "acme"]).await;
    let (_, p, _) = last(&calls);
    assert!(p.contains("a%2F..%2Fb"), "{p}");

    // CI's entry point.
    let args: Vec<String> = ["--agent", "first", "--org", "acme"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(cloud_run(&args).await.unwrap(), 0);
    let (_, p, _) = last(&calls);
    assert_eq!(p, "/v1/orgs/acme/agents/first/runs");
    assert!(cloud_run(&["--nope".to_string()]).await.is_err());
    assert!(agents(&["--nope"]).await.is_err());
    assert_eq!(agents(&["--help"]).await.unwrap(), 0);
    assert_eq!(cloud_run(&["--help".to_string()]).await.unwrap(), 0);
}

#[test]
fn signing_in_is_required() {
    if isolated("chanto_cloud::agents::cli_tests::signing_in_is_required") {
        return;
    }
    // SAFETY: the test runs alone in its own child process.
    unsafe { std::env::remove_var("CHANTO_CLOUD_TOKEN") };
    assert!(Cloud::signed_in().is_err());
}

#[test]
fn labels_and_tables_show_each_state() {
    use chanto_proto::agents::AgentTrigger as T;
    assert_eq!(trigger_label(&T::IssueOpened), "issue_opened");
    assert_eq!(
        trigger_label(&T::IssueLabeled {
            label: Some("bug".into())
        }),
        "issue_labeled:bug"
    );
    assert_eq!(
        trigger_label(&T::IssueLabeled { label: None }),
        "issue_labeled"
    );
    assert_eq!(trigger_label(&T::PullRequestOpened), "pull_request_opened");
    assert_eq!(
        trigger_label(&T::PullRequestUpdated),
        "pull_request_updated"
    );
    assert_eq!(trigger_label(&T::Mention), "mention");
    assert_eq!(placement_label(Placement::Auto), "auto");
    assert_eq!(placement_label(Placement::Hosted), "hosted");
    assert_eq!(placement_label(Placement::SelfHosted), "self-hosted");
    assert_eq!(
        parse_placement(" self-hosted ").unwrap(),
        Placement::SelfHosted
    );
    assert!(parse_placement("mars").is_err());

    let mut a: Agent = serde_json::from_value(agent_json("alpha")).unwrap();
    assert_eq!(starts(&a.spec), "by hand");
    a.spec.schedule = Some("0 3 * * *".into());
    a.spec.triggers = vec![T::Mention];
    assert_eq!(starts(&a.spec), "cron 0 3 * * *, mention");
    a.spec.enabled = false;
    assert_eq!(starts(&a.spec), "by hand (disabled)");

    let table = list_table(std::slice::from_ref(&a));
    assert!(table.contains("alpha"), "{table}");
    assert!(
        table
            .lines()
            .next()
            .unwrap()
            .to_lowercase()
            .contains("name"),
        "{table}"
    );
    a.last_error = Some("over the spending limit".into());
    let shown = show_text(&a);
    assert!(shown.starts_with("alpha (disabled, org acme)"), "{shown}");
    assert!(
        shown.contains("over the spending limit") && shown.contains("do it"),
        "{shown}"
    );

    let run: Run = serde_json::from_value(run_json("cancelled")).unwrap();
    let t = runs_table(std::slice::from_ref(&run));
    assert!(t.contains("run1") && t.contains("cancelled"), "{t}");
    assert_eq!(exit_code(Some(&run)), 1);
    assert_eq!(exit_code(None), 130);
    let ok: Run = serde_json::from_value(run_json("succeeded")).unwrap();
    assert_eq!(exit_code(Some(&ok)), 0);
}
