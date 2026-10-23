//! HTTP routes for background runs, mounted by `chanto serve` behind its
//! bearer token:
//!
//! - `POST /runs` `{task, cwd | source, model?, provider?, base_url?, mode?,
//!   verify?, sandbox?, max_cost_usd?, max_minutes?, start?, kind?, team?}`
//!   → the run (201); `start`: a commit to begin at instead of HEAD plus
//!   uncommitted changes, `kind: "goal"`: work on it as a goal, `team`: the
//!   team it belongs to
//! - `GET /runs` → `{runs: [...]}`, newest first
//! - `GET /runs/{id}` → the run plus `diff_stat`, `commits`, `last_messages`
//! - `GET /runs/{id}/events?since=<seq>` → SSE: logged events after `since`,
//!   then live ones; ends after the run reaches a final state
//! - `POST /runs/{id}/respond` `{request_id?, decision | answer, feedback?}`
//! - `POST /runs/{id}/stop`, `POST /runs/{id}/resume`, `DELETE /runs/{id}`
//! - `POST /runs/{id}/apply` `{pr?}`: apply a local run onto its checkout's
//!   current branch, or push it and open a pull request
//! - `POST /runs/bundles` (body: a git bundle) → `{bundle_id}`, then
//!   `POST /runs` with `source: {kind: "bundle", bundle_id, ref, head?,
//!   head_ref?, snapshot?}`; or `source: {kind: "git", url, ref, commit?}`
//! - `GET /runs/{id}/bundle` → a git bundle of the run's branch after its base
//!
//! Every event carries `seq` and `run_id`; `run_state` events carry the
//! whole run record. Ids may be abbreviated to a unique prefix.

use super::manager::{Manager, NewRun};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({"error": msg.into()}))).into_response()
}

fn not_found_or(e: anyhow::Error) -> Response {
    let msg = format!("{e:#}");
    let code = if msg.starts_with("no run ") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::CONFLICT
    };
    err(code, msg)
}

pub fn router<S: Clone + Send + Sync + 'static>(m: Arc<Manager>) -> Router<S> {
    let limit = m
        .max_bundle_bytes
        .load(std::sync::atomic::Ordering::Relaxed);
    Router::new()
        .route(
            "/runs/bundles",
            post(upload).layer(axum::extract::DefaultBodyLimit::max(limit)),
        )
        .route("/runs/{id}/bundle", get(download))
        .route("/runs/{id}/apply", post(apply))
        .route("/runs", post(create).get(list))
        .route("/runs/{id}", get(detail).delete(remove))
        .route("/runs/{id}/events", get(events))
        .route("/runs/{id}/respond", post(respond))
        .route("/runs/{id}/stop", post(stop))
        .route("/runs/{id}/resume", post(resume))
        .with_state(m)
}

async fn create(State(m): State<Arc<Manager>>, body: Option<Json<Value>>) -> Response {
    let body = body.map(|b| b.0).unwrap_or_else(|| json!({}));
    let req: NewRun = match serde_json::from_value(body) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("invalid run options: {e}")),
    };
    match m.create(req).await {
        Ok(run) => (StatusCode::CREATED, Json(run.to_json())).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn list(State(m): State<Arc<Manager>>) -> Response {
    Json(json!({"runs": m.list().iter().map(|r| r.to_json()).collect::<Vec<_>>()})).into_response()
}

async fn detail(State(m): State<Arc<Manager>>, Path(id): Path<String>) -> Response {
    match m.detail(&id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => not_found_or(e),
    }
}

async fn respond(
    State(m): State<Arc<Manager>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    match m.respond(&id, body) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => not_found_or(e),
    }
}

async fn stop(State(m): State<Arc<Manager>>, Path(id): Path<String>) -> Response {
    match m.stop(&id) {
        Ok(r) => Json(r.to_json()).into_response(),
        Err(e) => not_found_or(e),
    }
}

async fn resume(State(m): State<Arc<Manager>>, Path(id): Path<String>) -> Response {
    match m.resume(&id) {
        Ok(r) => Json(r.to_json()).into_response(),
        Err(e) => not_found_or(e),
    }
}

async fn remove(State(m): State<Arc<Manager>>, Path(id): Path<String>) -> Response {
    match m.remove(&id).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => not_found_or(e),
    }
}

async fn upload(State(m): State<Arc<Manager>>, body: axum::body::Bytes) -> Response {
    match m.save_bundle(&body).await {
        Ok(id) => (StatusCode::CREATED, Json(json!({"bundle_id": id}))).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn download(State(m): State<Arc<Manager>>, Path(id): Path<String>) -> Response {
    match m.bundle(&id).await {
        Ok(bytes) => ([("content-type", "application/x-git-bundle")], bytes).into_response(),
        Err(e) => not_found_or(e),
    }
}

async fn apply(
    State(m): State<Arc<Manager>>,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    let pr = body.is_some_and(|b| b.0["pr"] == true);
    match m.apply(&id, pr).await {
        Ok(out) => Json(json!({"ok": true, "output": out})).into_response(),
        Err(e) => not_found_or(e),
    }
}

fn is_final(ev: &Value) -> bool {
    ev["type"] == "run_state"
        && ev["state"]
            .as_str()
            .is_some_and(|s| !matches!(s, "queued" | "running" | "needs-input"))
}

async fn events(
    State(m): State<Arc<Manager>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let since: u64 = q.get("since").and_then(|s| s.parse().ok()).unwrap_or(0);
    // Subscribe before reading the log so nothing falls in between.
    let (run, rx) = match m.subscribe(&id) {
        Ok(x) => x,
        Err(e) => return not_found_or(e),
    };
    let logged: Vec<Value> = m
        .store
        .events(&run.id)
        .into_iter()
        .filter(|e| e["seq"].as_u64().unwrap_or(0) > since)
        .collect();
    let mut last = logged
        .last()
        .and_then(|e| e["seq"].as_u64())
        .unwrap_or(since);
    let mut replay = logged;
    // The open request, in case it scrolled out of the replay window.
    if let Some(p) = &run.pending
        && !replay
            .iter()
            .any(|e| e["request_id"] == p["request_id"] && e["type"] == p["type"])
    {
        replay.push(p.clone());
    }
    let done = !run.state.is_active();
    if done && !replay.last().is_some_and(is_final) {
        // Close with the final state, even when the log was trimmed.
        replay.push(json!({"type": "run_state", "state": run.state, "run": run.to_json()}));
    }
    last = last.max(since);
    let shutdown = m.shutdown.clone();
    let sse =
        |v: &Value| Ok::<_, std::convert::Infallible>(SseEvent::default().data(v.to_string()));
    let replay = futures::stream::iter(replay.iter().map(sse).collect::<Vec<_>>());
    let live = futures::stream::unfold((rx, last, done), move |(mut rx, last, done)| {
        let shutdown = shutdown.clone();
        async move {
            if done {
                return None;
            }
            loop {
                let v = tokio::select! {
                    v = rx.recv() => v,
                    _ = shutdown.cancelled() => return None,
                };
                match v {
                    Ok(v) if v["seq"].as_u64().is_some_and(|s| s <= last) => continue,
                    Ok(v) => {
                        let seq = v["seq"].as_u64().unwrap_or(last);
                        let fin = is_final(&v);
                        return Some((sse(&v), (rx, seq, fin)));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        }
    });
    Sse::new(futures::StreamExt::chain(replay, live))
        .keep_alive(KeepAlive::default())
        .into_response()
}
