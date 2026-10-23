//! `/remote`: continue the terminal session from your phone.
//!
//! The terminal keeps driving the session; `/remote` adds a second window onto
//! it. It starts a small HTTP server on this machine's Tailscale address
//! (loopback when there is none, never all interfaces) that serves the web UI
//! for this one session, behind a bearer token like `chanto web`:
//!
//! - [`Mirror`] wraps the TUI's frontend, so every event also goes to the
//!   phone, and permission prompts and questions are asked on both sides at
//!   once. The first answer wins; the other side's dialog closes.
//! - What the phone sends reaches the TUI as an [`Input`] and is handled like
//!   typing: a message mid-turn goes to the model at its next step, a message
//!   when idle starts a turn, and interrupt is esc.
//! - A late-joining phone gets the conversation as it stood when the last turn
//!   started plus the events since then (see [`Hub::snapshot`]).
//!
//! `/remote cloud` goes through Chanto cloud instead, for phones that can't
//! reach this machine. The cloud is a blind relay (see `remote_crypto`), and it
//! also serves a page from this binary on loopback, so there is a copy of the
//! dashboard whose code comes from the running Chanto rather than the server.
//!
//! The TUI reports what doesn't pass through the frontend (prompts, slash
//! command output, turn ends, resets) through the free functions at the
//! bottom, which do nothing while remote control is off.

use crate::agent::{Agent, Env, Event, Frontend, PermissionReply, PermissionRequest, event_json};
use crate::remote_crypto::{Handshake, Purpose, RemoteCrypto, SessionKey};
use anyhow::Result;
use async_trait::async_trait;
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex as AsyncMutex, broadcast, oneshot};
use tokio_util::sync::CancellationToken;

/// Preferred port (next to `chanto web`'s 4141); a random one if it's taken.
const PORT: u16 = 4142;
/// The one session a remote-control server serves.
const SESSION_ID: &str = "terminal";
/// Events kept for phones that connect mid-turn.
const LOG_MAX: usize = 4000;
/// A prompt left unanswered this long sends a push notification...
const NOTIFY_AFTER: Duration = Duration::from_secs(20);
/// ...at most this often.
const NOTIFY_GAP: Duration = Duration::from_secs(60);

const NOT_HERE: &str = "Remote control continues the interactive terminal session (`chanto`). For other sessions, run `chanto web --tailscale` (see \"Control Chanto from your phone\" in docs/reference.md).";

/// What the phone asks the terminal to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// A message, or a slash command, as if typed.
    Prompt(String),
    /// Stop the running turn (esc).
    Interrupt,
}

/// The session the hub serves: the TUI, or a fake in tests.
pub trait Host: Send + Sync {
    /// Hand input to the session; false once it has gone away.
    fn send(&self, input: Input) -> bool;
    /// The conversation as of now, unless a turn holds the session.
    fn snapshot(&self) -> Option<Vec<Value>>;
    /// `{model, mode, todos}`.
    fn info(&self) -> Value;
}

struct TuiHost {
    agent: Arc<AsyncMutex<Agent>>,
    env: Arc<Env>,
    send: Box<dyn Fn(Input) -> bool + Send + Sync>,
}

impl Host for TuiHost {
    fn send(&self, input: Input) -> bool {
        (self.send)(input)
    }
    fn snapshot(&self) -> Option<Vec<Value>> {
        self.agent.try_lock().ok().map(|a| messages_json(&a))
    }
    fn info(&self) -> Value {
        json!({"model": self.env.model(), "mode": self.env.mode(), "todos": *self.env.todos.lock().unwrap()})
    }
}

/// The transcript in the web UI's shape (as `GET /sessions/{id}` in server.rs).
fn messages_json(a: &Agent) -> Vec<Value> {
    a.messages
        .iter()
        .skip(1)
        .map(|m| json!({"role": m.role, "content": m.content, "tool_calls": m.tool_calls.iter().map(|c| json!({"id": c.id, "name": c.name, "arguments": c.arguments})).collect::<Vec<_>>()}))
        .collect()
}

// ------------------------------------------------------------------- hub

/// What a newly connected phone needs to catch up: the conversation at a
/// point, and the events since that point.
#[derive(Default)]
struct Mirrored {
    messages: Vec<Value>,
    log: VecDeque<Value>,
    /// A turn is running (between its prompt and its end), so the
    /// conversation can't be re-read without losing its events.
    busy: bool,
}

/// One remote-control server's state: the event fan-out, open prompts, and
/// the catch-up state.
pub struct Hub {
    id: String,
    token: String,
    host: Arc<dyn Host>,
    tx: broadcast::Sender<Value>,
    state: Mutex<Mirrored>,
    /// Open prompts: the answer channel and the event, replayed to phones that
    /// connect while it waits.
    pending: Mutex<HashMap<String, (oneshot::Sender<Value>, Value)>>,
    closed: CancellationToken,
    push: Option<Push>,
}

impl Hub {
    fn new(id: &str, token: &str, host: Arc<dyn Host>, push: Option<Push>) -> Arc<Hub> {
        let messages = host.snapshot().unwrap_or_default();
        let (tx, _) = broadcast::channel(1024);
        Arc::new(Hub {
            id: id.into(),
            token: token.into(),
            host,
            tx,
            state: Mutex::new(Mirrored {
                messages,
                ..Default::default()
            }),
            pending: Default::default(),
            closed: CancellationToken::new(),
            push,
        })
    }

    /// Send an event to connected phones and keep it for later ones. Streamed
    /// text is merged into one entry per run, so a long answer is one event.
    fn record(&self, ev: Value) {
        let mut st = self.state.lock().unwrap();
        let merged = match (st.log.back_mut(), ev["type"].as_str()) {
            (Some(last), Some(kind @ ("text" | "reasoning"))) if last["type"] == kind => {
                let text = format!(
                    "{}{}",
                    last["text"].as_str().unwrap_or(""),
                    ev["text"].as_str().unwrap_or("")
                );
                last["text"] = json!(text);
                true
            }
            _ => false,
        };
        if !merged {
            st.log.push_back(ev.clone());
            if st.log.len() > LOG_MAX {
                st.log.pop_front();
            }
        }
        // Sent under the lock, so a subscriber sees each event exactly once:
        // in its catch-up copy of the log or live.
        let _ = self.tx.send(ev);
    }

    /// A prompt shown in the terminal (typed there or sent from the phone).
    fn user(&self, text: &str, quiet: bool) {
        self.state.lock().unwrap().busy = true;
        if !quiet {
            self.record(json!({"type": "user", "text": text}));
        }
    }

    fn turn_done(&self, error: Option<&str>) {
        let mut ev = json!({"type": "turn_done"});
        if let Some(e) = error {
            ev["error"] = json!(e);
        }
        self.record(ev);
        self.state.lock().unwrap().busy = false;
    }

    /// The conversation was replaced (`/clear`, `/resume`, `/rewind`): start
    /// over from it, and tell phones to reload.
    fn reset(&self) {
        let mut st = self.state.lock().unwrap();
        if let Some(m) = self.host.snapshot() {
            st.messages = m;
            st.log.clear();
        }
        let _ = self.tx.send(json!({"type": "reset"}));
    }

    /// The catch-up conversation plus `{model, mode, todos, busy}`. Between
    /// turns it's re-read (when the session is free), and the log restarts.
    fn snapshot(&self) -> Value {
        let mut st = self.state.lock().unwrap();
        if !st.busy
            && let Some(m) = self.host.snapshot()
        {
            st.messages = m;
            st.log.clear();
        }
        let mut out = self.host.info();
        out["id"] = json!(self.id);
        out["busy"] = json!(st.busy);
        out["messages"] = json!(st.messages);
        out
    }

    /// The events to replay to a new subscriber (the log, then open prompts)
    /// and its live receiver.
    fn subscribe(&self) -> (Vec<Value>, broadcast::Receiver<Value>) {
        let st = self.state.lock().unwrap();
        let rx = self.tx.subscribe();
        let mut replay: Vec<Value> = st.log.iter().cloned().collect();
        replay.extend(
            self.pending
                .lock()
                .unwrap()
                .values()
                .map(|(_, ev)| ev.clone()),
        );
        (replay, rx)
    }

    /// Ask the phone too: returns the request id and where its answer arrives.
    fn open(self: &Arc<Self>, mut ev: Value) -> (String, oneshot::Receiver<Value>) {
        let id = format!("req-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let (tx, rx) = oneshot::channel();
        ev["request_id"] = json!(id);
        self.pending
            .lock()
            .unwrap()
            .insert(id.clone(), (tx, ev.clone()));
        let _ = self.tx.send(ev);
        self.notify_later(id.clone());
        (id, rx)
    }

    /// The prompt is settled (on either side, or abandoned): phones close it.
    fn done(&self, id: &str) {
        self.pending.lock().unwrap().remove(id);
        let _ = self
            .tx
            .send(json!({"type": "request_done", "request_id": id}));
    }

    /// An answer from the phone; false when nothing waits for it any more.
    fn answer(&self, id: &str, body: Value) -> bool {
        match self.pending.lock().unwrap().remove(id) {
            Some((tx, _)) => tx.send(body).is_ok(),
            None => false,
        }
    }

    /// Stop serving: streams end, and open prompts are left to the terminal.
    fn close(&self) {
        self.closed.cancel();
        self.pending.lock().unwrap().clear();
    }

    fn is_busy(&self) -> bool {
        self.state.lock().unwrap().busy
    }

    /// Push a notification if the prompt is still open after a while, so
    /// answering at the terminal straight away never pings the phone.
    fn notify_later(self: &Arc<Self>, id: String) {
        if self.push.is_none() {
            return;
        }
        let hub = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(NOTIFY_AFTER).await;
            let Some(hub) = hub.upgrade() else { return };
            if hub.closed.is_cancelled() || !hub.pending.lock().unwrap().contains_key(&id) {
                return;
            }
            let Some(push) = &hub.push else { return };
            {
                let mut last = push.last.lock().unwrap();
                if last.is_some_and(|t| t.elapsed() < NOTIFY_GAP) {
                    return;
                }
                *last = Some(Instant::now());
            }
            push.send().await;
        });
    }
}

/// `[notify]` ntfy topic and webhook (the background runs' settings). Like
/// run notifications, they never carry the prompt, code or output.
struct Push {
    ntfy: Option<String>,
    webhook: Option<String>,
    link: String,
    http: reqwest::Client,
    last: Mutex<Option<Instant>>,
}

impl Push {
    fn new(cfg: &crate::runs::notify::NotifyConfig, base: &str) -> Option<Push> {
        if cfg.ntfy.is_none() && cfg.webhook.is_none() {
            return None;
        }
        Some(Push {
            ntfy: cfg.ntfy.clone(),
            webhook: cfg.webhook.clone(),
            link: cfg.link.clone().unwrap_or_else(|| base.to_string()),
            http: crate::http::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            last: Mutex::new(None),
        })
    }

    /// Failures are ignored: printing would scribble over the TUI.
    async fn send(&self) {
        let title = "chanto: your terminal session needs input";
        let message = "A permission prompt or question is waiting. Answer it on your phone or in the terminal.";
        if let Some(url) = &self.ntfy {
            let _ = self
                .http
                .post(url)
                .header("Title", title)
                .header("Tags", "question")
                .header("Priority", "high")
                .header("Click", self.link.as_str())
                .body(message)
                .send()
                .await;
        }
        if let Some(url) = &self.webhook {
            let _ = self.http.post(url).json(&json!({"event": "session.needs-input", "title": title, "message": message, "url": self.link})).send().await;
        }
    }
}

// ---------------------------------------------------------------- mirror

/// Where the [`Mirror`] finds the active hub (none while remote control is off).
pub type Slot = Arc<RwLock<Option<Arc<Hub>>>>;

/// The TUI's frontend, plus the phone while remote control is on.
pub struct Mirror {
    inner: Arc<dyn Frontend>,
    slot: Slot,
}

impl Mirror {
    pub fn new(inner: Arc<dyn Frontend>, slot: Slot) -> Mirror {
        Mirror { inner, slot }
    }
    fn hub(&self) -> Option<Arc<Hub>> {
        self.slot.read().unwrap().clone()
    }
}

/// Closes the prompt on the phone however the wait ends: answered on either
/// side, or dropped because the turn was interrupted.
struct Asked {
    hub: Arc<Hub>,
    id: String,
}

impl Drop for Asked {
    fn drop(&mut self) {
        self.hub.done(&self.id);
    }
}

fn permission_reply(a: &Value) -> PermissionReply {
    match a["decision"].as_str() {
        Some("allow") => PermissionReply::Allow,
        Some("allow_always") => PermissionReply::AllowAlways,
        _ => PermissionReply::Deny(
            a["feedback"]
                .as_str()
                .filter(|f| !f.is_empty())
                .map(str::to_string),
        ),
    }
}

#[async_trait]
impl Frontend for Mirror {
    fn emit(&self, ev: Event) {
        if let Some(hub) = self.hub()
            && let Some(v) = event_json(&ev)
        {
            hub.record(v);
        }
        self.inner.emit(ev);
    }

    async fn permission(&self, req: PermissionRequest) -> PermissionReply {
        let Some(hub) = self.hub() else {
            return self.inner.permission(req).await;
        };
        let ev = json!({"type": "permission_request", "tool": req.tool.clone(), "label": req.label.clone(), "detail": req.detail.clone(),
            "reason": req.reason.clone(), "always_label": req.always_label.clone()});
        let (id, rx) = hub.open(ev);
        let _asked = Asked { hub, id };
        // Dropping the terminal's wait closes its dialog (the TUI checks).
        let local = self.inner.permission(req);
        tokio::pin!(local);
        tokio::select! {
            r = &mut local => r,
            // An error means remote control went off: the terminal decides.
            Ok(a) = rx => {
                let reply = permission_reply(&a);
                let what = match &reply {
                    PermissionReply::Allow => "allowed once",
                    PermissionReply::AllowAlways => "always allowed",
                    PermissionReply::AllowEdited(_) => "allowed with edits",
                    PermissionReply::Deny(_) => "denied",
                    PermissionReply::AutoDenied(_) => "denied after no answer",
                };
                self.emit(Event::Info(format!("Answered on your phone: {what}.")));
                reply
            }
        }
    }

    async fn question(&self, question: &str, options: &[String]) -> Option<String> {
        let Some(hub) = self.hub() else {
            return self.inner.question(question, options).await;
        };
        let (id, rx) =
            hub.open(json!({"type": "question", "question": question, "options": options}));
        let _asked = Asked { hub, id };
        let local = self.inner.question(question, options);
        tokio::pin!(local);
        tokio::select! {
            r = &mut local => r,
            Ok(a) = rx => {
                let answer = a["answer"].as_str().map(str::to_string);
                self.emit(Event::Info(match &answer {
                    Some(text) => format!("Answered on your phone: {}", crate::util::one_line(text, 80)),
                    None => "Question dismissed on your phone.".into(),
                }));
                answer
            }
        }
    }

    async fn choose(&self, question: &str, options: &[String]) -> Option<String> {
        let Some(hub) = self.hub() else {
            return self.inner.choose(question, options).await;
        };
        let (id, rx) =
            hub.open(json!({"type": "question", "question": question, "options": options}));
        let _asked = Asked { hub, id };
        let local = self.inner.choose(question, options);
        tokio::pin!(local);
        tokio::select! {
            r = &mut local => r,
            Ok(a) = rx => {
                // The phone's answer box is free text: accept only one of the options.
                let answer = a["answer"].as_str().and_then(|t| options.iter().find(|o| o.eq_ignore_ascii_case(t.trim())).cloned());
                self.emit(Event::Info(match &answer {
                    Some(text) => format!("Answered on your phone: {text}"),
                    None => "Question dismissed on your phone.".into(),
                }));
                answer
            }
        }
    }

    async fn tool_call(&self, id: &str, name: &str, args: &Value, cwd: &Path) {
        self.inner.tool_call(id, name, args, cwd).await
    }
    fn file_restored(&self, path: &Path, text: &str) {
        self.inner.file_restored(path, text)
    }
    fn file_diff(&self, id: &str, path: &Path, old: Option<&str>, new: &str) {
        self.inner.file_diff(id, path, old, new)
    }
    async fn read_text_file(&self, path: &Path) -> Option<Result<String>> {
        self.inner.read_text_file(path).await
    }
    async fn write_text_file(&self, path: &Path, text: &str) -> Option<Result<()>> {
        self.inner.write_text_file(path, text).await
    }
    fn writes_via_client(&self) -> bool {
        self.inner.writes_via_client()
    }
}

// ---------------------------------------------------------------- server

#[derive(Clone)]
struct St {
    hub: Arc<Hub>,
}

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({"error": msg.into()}))).into_response()
}

async fn auth(State(st): State<St>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(crate::server::bearer_token)
        .is_some_and(|t| crate::server::constant_time_eq(t.as_bytes(), st.hub.token.as_bytes()));
    if !ok {
        return err(StatusCode::UNAUTHORIZED, "missing or wrong bearer token");
    }
    if st.hub.closed.is_cancelled() {
        return err(StatusCode::SERVICE_UNAVAILABLE, "remote control is off");
    }
    next.run(req).await
}

#[allow(clippy::result_large_err)]
fn this_session(st: &St, id: &str) -> Result<(), Response> {
    if id == st.hub.id {
        Ok(())
    } else {
        Err(err(StatusCode::NOT_FOUND, format!("no session {id}")))
    }
}

async fn health(State(st): State<St>) -> Response {
    Json(json!({"ok": true, "version": env!("CARGO_PKG_VERSION"), "runs": false, "remote": true, "session": st.hub.id})).into_response()
}

async fn list_sessions(State(st): State<St>) -> Response {
    let model = st.hub.host.info()["model"].clone();
    Json(json!({"sessions": [{"id": st.hub.id, "busy": st.hub.is_busy(), "model": model, "title": "This terminal session"}]})).into_response()
}

async fn create_session() -> Response {
    err(
        StatusCode::CONFLICT,
        "remote control serves only the terminal's session; start new sessions in the terminal or with `chanto web`",
    )
}

async fn get_session(State(st): State<St>, UrlPath(id): UrlPath<String>) -> Response {
    if let Err(e) = this_session(&st, &id) {
        return e;
    }
    Json(st.hub.snapshot()).into_response()
}

async fn events(State(st): State<St>, UrlPath(id): UrlPath<String>) -> Response {
    if let Err(e) = this_session(&st, &id) {
        return e;
    }
    let (replay, rx) = st.hub.subscribe();
    let replay = futures::stream::iter(
        replay
            .into_iter()
            .map(|v| Ok::<_, std::convert::Infallible>(SseEvent::default().data(v.to_string()))),
    );
    let live = futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(v) => {
                    return Some((
                        Ok::<_, std::convert::Infallible>(SseEvent::default().data(v.to_string())),
                        rx,
                    ));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    // Ends when remote control goes off, so the server can shut down.
    let stream = futures::StreamExt::take_until(
        futures::StreamExt::chain(replay, live),
        st.hub.closed.clone().cancelled_owned(),
    );
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn send_message(
    State(st): State<St>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Err(e) = this_session(&st, &id) {
        return e;
    }
    let Some(text) = body["text"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return err(StatusCode::BAD_REQUEST, "`text` is required");
    };
    let busy = st.hub.is_busy();
    if !st.hub.host.send(Input::Prompt(text.to_string())) {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "the terminal session has ended",
        );
    }
    // While a turn runs it reaches the model at its next step, as if typed.
    (
        StatusCode::ACCEPTED,
        Json(if busy {
            json!({"queued": true})
        } else {
            json!({"started": true})
        }),
    )
        .into_response()
}

async fn respond(
    State(st): State<St>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Err(e) = this_session(&st, &id) {
        return e;
    }
    let Some(rid) = body["request_id"].as_str().map(str::to_string) else {
        return err(StatusCode::BAD_REQUEST, "`request_id` is required");
    };
    if st.hub.answer(&rid, body) {
        Json(json!({"ok": true})).into_response()
    } else {
        err(
            StatusCode::NOT_FOUND,
            format!("no pending request {rid} (it may have been answered in the terminal)"),
        )
    }
}

async fn interrupt(State(st): State<St>, UrlPath(id): UrlPath<String>) -> Response {
    if let Err(e) = this_session(&st, &id) {
        return e;
    }
    if !st.hub.host.send(Input::Interrupt) {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "the terminal session has ended",
        );
    }
    Json(json!({"ok": true})).into_response()
}

/// The web UI and the part of the session API it uses for one session.
/// Rewind, fork and delete stay in the terminal.
fn router(hub: Arc<Hub>) -> Router {
    let st = St { hub };
    let api = Router::new()
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}", get(get_session))
        .route("/sessions/{id}/events", get(events))
        .route("/sessions/{id}/messages", post(send_message))
        .route("/sessions/{id}/respond", post(respond))
        .route("/sessions/{id}/interrupt", post(interrupt))
        .route("/voice", get(crate::server::voice_status))
        .route(
            "/voice/transcribe",
            post(crate::server::voice_transcribe)
                .layer(axum::extract::DefaultBodyLimit::max(25 << 20)),
        )
        .layer(axum::middleware::from_fn_with_state(st.clone(), auth))
        .route("/health", get(health))
        .with_state(st);
    crate::server::web_router(api)
}

// ------------------------------------------------------------- the TUI

/// Set once by the TUI: where its frontend looks for the hub, and the session.
static SLOT: OnceLock<Slot> = OnceLock::new();
static HOST: OnceLock<Arc<dyn Host>> = OnceLock::new();
/// The current link's token. A fresh one is minted each time remote control
/// is turned on, so `/remote off` then on revokes the old link; it stays put
/// while remote is on, so the phone can reconnect (reload its tab).
static TOKEN: RwLock<Option<String>> = RwLock::new(None);

struct Running {
    hub: Arc<Hub>,
    base: String,
    launch: String,
    tailnet: bool,
    /// Through Chanto cloud, the session key's short fingerprint, so both
    /// ends can confirm they hold the same key.
    fingerprint: Option<String>,
    /// Through Chanto cloud: the session's client and API path (to end it).
    cloud: Option<(crate::runs::cloud::Cloud, String)>,
    /// Through Chanto cloud: the same page served from this machine, for people
    /// who would rather run code from the binary than JavaScript from the server.
    local: Option<String>,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

thread_local! {
    /// Set while the TUI starts a turn whose prompt isn't shown (see [`quietly`]).
    static QUIET: Cell<bool> = const { Cell::new(false) };
}

/// Wrap the TUI's frontend so `/remote` can mirror it.
pub fn mirror_tui(inner: Arc<dyn Frontend>) -> Arc<dyn Frontend> {
    let slot = SLOT.get_or_init(Default::default).clone();
    Arc::new(Mirror::new(inner, slot))
}

/// Make the TUI's session available to `/remote`; `send` hands it phone input.
pub fn attach_tui(
    agent: Arc<AsyncMutex<Agent>>,
    env: Arc<Env>,
    send: impl Fn(Input) -> bool + Send + Sync + 'static,
) {
    let _ = HOST.set(Arc::new(TuiHost {
        agent,
        env,
        send: Box::new(send),
    }));
}

fn active() -> Option<Arc<Hub>> {
    SLOT.get().and_then(|s| s.read().unwrap().clone())
}

/// A prompt shown in the transcript (typed, or from the phone).
pub fn user(text: &str) {
    if let Some(hub) = active() {
        hub.user(text, QUIET.with(Cell::get));
    }
}

/// Start a turn without showing its prompt on the phone (messages that were
/// already shown, background agents' reports).
pub fn quietly<R>(f: impl FnOnce() -> R) -> R {
    QUIET.with(|q| q.set(true));
    let r = f();
    QUIET.with(|q| q.set(false));
    r
}

/// Output of a slash command or `!command`.
pub fn show(markdown: &str) {
    if let Some(hub) = active() {
        hub.record(json!({"type": "info", "message": markdown}));
    }
}

pub fn turn_done(error: Option<&str>) {
    if let Some(hub) = active() {
        hub.turn_done(error);
    }
}

pub fn reset() {
    if let Some(hub) = active() {
        hub.reset();
    }
}

impl Running {
    /// What `/remote` prints. The link carries the access token, so the screen
    /// (and a screen share or recording) shows it masked: the full link goes to
    /// the clipboard, or `/remote link` prints it.
    fn describe(&self, reveal: bool) -> String {
        if self.cloud.is_some() {
            let fingerprint = match &self.fingerprint {
                Some(f) => format!("\n\nKey fingerprint `{f}` — the dashboard shows the same one."),
                None => String::new(),
            };
            // The fragment carries the key, so a screen share shows a masked
            // link; the full one goes to the clipboard or `/remote link`.
            let (link, how) = if reveal {
                (self.launch.clone(), String::new())
            } else if crate::commands::copy_to_clipboard(&self.launch).is_ok() {
                (
                    format!("{}#k=••••••", self.base),
                    "\n\nThe full link, with its key, is on your clipboard; `/remote link` prints it."
                        .to_string(),
                )
            } else {
                (
                    format!("{}#k=••••••", self.base),
                    "\n\nThe key is hidden here; `/remote link` prints the full link (keep it private)."
                        .to_string(),
                )
            };
            let local = match &self.local {
                Some(link) => {
                    // The key is what matters; the token only opens this machine.
                    let link = if reveal {
                        link.clone()
                    } else {
                        format!("{}/#token=••••••", link.split('#').next().unwrap_or(link))
                    };
                    format!(
                        "\n\nFor a page you can check, this machine also serves this session's dashboard itself, straight out of the chanto binary (loopback only):\n\n{link}{}\n\nThe cloud never serves that copy: you can read it in the source and in the binary you're running.",
                        if reveal {
                            ""
                        } else {
                            "\n\n(`/remote link` prints it in full.)"
                        }
                    )
                }
                None => String::new(),
            };
            return format!(
                "**Remote control is on, through chanto cloud.** Open this session in the dashboard (signed in as you), on your phone or any browser:\n\n{}{}{}{}\n\nThe link's fragment carries the key and a one-time handshake key: the browser agrees a fresh session key with this machine over P-256 ECDH, and the server sees neither. Once a browser joins, the terminal forgets the handshake's private half, so a link that leaks later can't open the session. Everything you and the dashboard type, and the folder and machine names, stay sealed in transit. Keep the link private: it is the invitation. Nothing listens on this machine's external interfaces: chanto connects out to the server.{}\n\nWhat the dashboard sees and the page from this machine sees are one session: whichever side answers a prompt first wins. One browser uses the link at a time; reloading that tab keeps it. `/remote off` stops it.",
                link,
                how,
                fingerprint,
                local,
                if self.local.is_some() {
                    " The local page above is served from chanto itself, not by the cloud, so the cloud can't see the key even in the JavaScript."
                } else {
                    ""
                }
            );
        }
        let reach = if self.tailnet {
            "Open this on your phone (with Tailscale connected to the same tailnet)"
        } else {
            "Listening on this machine only (127.0.0.1); open"
        };
        let (link, how) = if reveal {
            (
                self.launch.clone(),
                "The link carries the access token, so keep it private.",
            )
        } else if crate::commands::copy_to_clipboard(&self.launch).is_ok() {
            (
                format!("{}/#token=••••••", self.base),
                "The full link, with its access token, is on your clipboard; `/remote link` prints it.",
            )
        } else {
            (
                format!("{}/#token=••••••", self.base),
                "The token is hidden here; `/remote link` prints the full link (keep it private).",
            )
        };
        format!(
            "**Remote control is on.** {reach}:\n\n`{link}`\n\n{how} The phone sees this session live and can send messages, answer prompts and interrupt; whichever side answers a prompt first wins. `/remote off` stops it."
        )
    }
}

/// `/remote [on|off|status|local]` (alias `/remote-control`).
pub async fn command(env: &Env, arg: &str) -> String {
    match arg.trim() {
        // Through Chanto cloud when signed in to it; else the tailnet.
        "" | "on" | "start" => {
            if crate::runs::cloud::Cloud::signed_in().is_ok() {
                start_cloud().await
            } else {
                start(env, false).await
            }
        }
        "cloud" => start_cloud().await,
        "tailscale" | "tailnet" => start(env, false).await,
        "local" => start(env, true).await,
        "off" | "stop" => stop(),
        "status" | "link" => {
            let running = RUNNING.lock().unwrap().as_ref().map(|r| r.describe(arg.trim() == "link"));
            running.unwrap_or_else(|| "Remote control is off. `/remote` turns it on.".into())
        }
        _ => "Usage: `/remote` (on: through Chanto cloud when you're signed in to it, else your tailnet), `/remote cloud`, `/remote tailscale`, `/remote local` (this machine only), `/remote off`, `/remote status`, or `/remote link` (the full link).".into(),
    }
}

async fn start(env: &Env, local_only: bool) -> String {
    let (Some(slot), Some(host)) = (SLOT.get(), HOST.get()) else {
        return NOT_HERE.into();
    };
    let running = RUNNING.lock().unwrap().as_ref().map(|r| r.describe(false));
    if let Some(d) = running {
        return d;
    }
    // Tailnet address when there is one; otherwise loopback. Never 0.0.0.0.
    let (ip, name, note) = if local_only {
        ("127.0.0.1".to_string(), None, None)
    } else {
        let found = tokio::task::spawn_blocking(|| {
            crate::server::tailscale_ipv4().map(|ip| (ip, crate::server::tailscale_dns_name()))
        })
        .await;
        match found {
            Ok(Ok((ip, dns))) => (
                ip.to_string(),
                Some(dns.unwrap_or_else(|| ip.to_string())),
                None,
            ),
            Ok(Err(e)) => (
                "127.0.0.1".to_string(),
                None,
                Some(format!(
                    "Tailscale isn't available ({e}), so the phone can't reach this yet. Once it's up, run `/remote off` and `/remote` again."
                )),
            ),
            Err(e) => (
                "127.0.0.1".to_string(),
                None,
                Some(format!("Couldn't check Tailscale ({e}).")),
            ),
        }
    };
    let listener = match tokio::net::TcpListener::bind((ip.as_str(), PORT)).await {
        Ok(l) => l,
        Err(_) => match tokio::net::TcpListener::bind((ip.as_str(), 0)).await {
            Ok(l) => l,
            Err(e) => return format!("Couldn't start remote control on {ip}: {e}"),
        },
    };
    let port = match listener.local_addr() {
        Ok(a) => a.port(),
        Err(e) => return format!("Couldn't start remote control: {e}"),
    };
    let base = format!(
        "http://{}:{port}",
        name.clone().unwrap_or_else(|| ip.clone())
    );
    let token = fresh_token();
    let hub = Hub::new(
        SESSION_ID,
        &token,
        host.clone(),
        Push::new(&env.config.notify, &base),
    );
    let app = router(hub.clone());
    let closed = hub.closed.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(closed.cancelled_owned())
            .await;
    });
    *slot.write().unwrap() = Some(hub.clone());
    let running = Running {
        hub,
        launch: format!("{base}/#token={}", urlencoding::encode(&token)),
        base,
        tailnet: name.is_some(),
        fingerprint: None,
        cloud: None,
        local: None,
    };
    let mut out = running.describe(false);
    if let Some(n) = note {
        out.push_str("\n\n");
        out.push_str(&n);
    }
    *RUNNING.lock().unwrap() = Some(running);
    out
}

// ------------------------------------------------------------- through Chanto cloud

/// Serves the page compiled into this binary on loopback: the same dashboard,
/// but the code the browser runs is the code from the signed Chanto build, not
/// JavaScript the cloud sent. It reads the same hub, so both views stay in step.
async fn serve_locally(hub: Arc<Hub>) -> Option<String> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.ok()?;
    let port = listener.local_addr().ok()?.port();
    let closed = hub.closed.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(hub))
            .with_graceful_shutdown(closed.cancelled_owned())
            .await;
    });
    Some(format!("http://127.0.0.1:{port}"))
}

/// `/remote cloud`: the hub, relayed through Chanto cloud. Nothing listens on
/// an external interface: one task uploads the session's events (and a fresh
/// snapshot after each turn), another long-polls for what the dashboard sends.
/// The page compiled into this binary is served on loopback alongside it.
async fn start_cloud() -> String {
    use chanto_proto::remote::{CreateRemoteSession, RemoteSessionCreated};
    let (Some(slot), Some(host)) = (SLOT.get(), HOST.get()) else {
        return NOT_HERE.into();
    };
    let running = RUNNING.lock().unwrap().as_ref().map(|r| r.describe(false));
    if let Some(d) = running {
        return d;
    }
    let account = match crate::runs::cloud::Cloud::signed_in() {
        Ok(c) => c,
        Err(e) => return format!("{e:#}"),
    };
    let cwd = std::env::current_dir().ok();
    let title = cwd
        .as_ref()
        .and_then(|d| d.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "Terminal session".into());
    let machine = tokio::task::spawn_blocking(|| {
        std::process::Command::new("hostname")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|h| !h.is_empty())
    })
    .await
    .ok()
    .flatten();
    // The relay never sees these: they are sealed and travel as envelopes. The
    // link's key bootstraps the ECDH handshake a reader starts; the session's
    // real keys come from that exchange.
    let bootstrap = match SessionKey::generate() {
        Ok(k) => k,
        Err(e) => return format!("Couldn't start remote control through chanto cloud: {e:#}"),
    };
    let handshake = match Handshake::generate() {
        Ok(h) => h,
        Err(e) => return format!("Couldn't start remote control through chanto cloud: {e:#}"),
    };
    let meta =
        json!({"title": title, "machine": machine, "cwd": cwd.map(|d| d.display().to_string())});
    let sealed_meta = match bootstrap.seal(Purpose::Meta, &meta) {
        Ok(m) => m,
        Err(e) => return format!("Couldn't start remote control through chanto cloud: {e:#}"),
    };
    let req = CreateRemoteSession {
        sealed_meta: Some(sealed_meta),
        ..Default::default()
    };
    let created: RemoteSessionCreated = match account.post("/v1/remote/sessions", &req).await {
        Ok(c) => c,
        Err(e) => return format!("Couldn't start remote control through Chanto cloud: {e:#}"),
    };
    let crypto = Arc::new(RemoteCrypto::with_parts(&created.id, bootstrap, handshake));
    let session = crate::runs::cloud::Cloud::new(&account.url, &created.token);
    let path = format!("/v1/remote/sessions/{}", created.id);
    let token = uuid::Uuid::new_v4().simple().to_string();
    let hub = Hub::new(SESSION_ID, &token, host.clone(), None);
    tokio::spawn(upload(
        hub.clone(),
        session.clone(),
        path.clone(),
        crypto.clone(),
    ));
    tokio::spawn(relay_commands(
        hub.clone(),
        session.clone(),
        path.clone(),
        crypto.clone(),
    ));
    // The page from this binary, for anyone who would rather not trust the
    // cloud's copy of the dashboard.
    let local = serve_locally(hub.clone())
        .await
        .map(|base| format!("{base}/#token={}", urlencoding::encode(&token)));
    *slot.write().unwrap() = Some(hub.clone());
    let running = Running {
        hub,
        base: created.url.clone(),
        // The key rides in the fragment, so the link is the credential; the
        // server can't read what it relays.
        launch: crypto.link(&created.url),
        tailnet: false,
        fingerprint: Some(crypto.fingerprint()),
        cloud: Some((session, path)),
        local,
    };
    let out = running.describe(false);
    *RUNNING.lock().unwrap() = Some(running);
    out
}

/// Streamed text arrives in pieces: one event per run of them.
fn merge_text(events: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(events.len());
    for ev in events {
        if let (Some(last), Some(kind @ ("text" | "reasoning"))) =
            (out.last_mut(), ev["type"].as_str())
            && last["type"] == kind
        {
            let text = format!(
                "{}{}",
                last["text"].as_str().unwrap_or(""),
                ev["text"].as_str().unwrap_or("")
            );
            last["text"] = json!(text);
            continue;
        }
        out.push(ev);
    }
    out
}

/// How long a burst of events is gathered into one post. The first event
/// after a quiet spell goes out at once, so a prompt or a tool starting shows
/// up without waiting for the window; only the events that follow it within
/// this window are coalesced.
const GATHER: Duration = Duration::from_millis(250);

/// The next batch to upload: the events waiting now, or the next one and
/// whatever follows it within [`GATHER`] of the last post. `None` when remote
/// control ended. A batch handed in (the replay at startup) or the first
/// event after a quiet spell is returned without waiting.
async fn next_batch(
    rx: &mut broadcast::Receiver<Value>,
    closed: &CancellationToken,
    mut batch: Vec<Value>,
    last_send: Option<Instant>,
) -> Option<Vec<Value>> {
    while batch.is_empty() {
        tokio::select! {
            _ = closed.cancelled() => return None,
            ev = rx.recv() => match ev {
                Ok(v) => batch.push(v),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            },
        }
    }
    if last_send.is_none_or(|t| t.elapsed() >= GATHER) {
        return Some(batch);
    }
    let gather = tokio::time::sleep(GATHER);
    tokio::pin!(gather);
    loop {
        tokio::select! {
            _ = &mut gather => break,
            ev = rx.recv() => match ev {
                Ok(v) => batch.push(v),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    Some(batch)
}

/// Sends the conversation, then its events as they happen: the first of a
/// burst at once, the rest a quarter of a second later. A reset (`/clear`,
/// `/resume`) sends the new conversation first; the end of a turn, a fresh
/// snapshot after it, so readers start from the conversation rather than a
/// long log.
async fn upload(
    hub: Arc<Hub>,
    c: crate::runs::cloud::Cloud,
    path: String,
    crypto: Arc<RemoteCrypto>,
) {
    let events_url = format!("{path}/events");
    put_snapshot(&hub, &c, &path, &crypto).await;
    let (replay, mut rx) = hub.subscribe();
    let mut batch = replay;
    let mut last_send: Option<Instant> = None;
    loop {
        let Some(ready) =
            next_batch(&mut rx, &hub.closed, std::mem::take(&mut batch), last_send).await
        else {
            return;
        };
        let mut send = merge_text(ready);
        // Events before a reset belong to the conversation it replaced.
        if let Some(i) = send.iter().rposition(|e| e["type"] == "reset") {
            send.drain(..i);
            put_snapshot(&hub, &c, &path, &crypto).await;
        }
        let turn_ended = send.iter().any(|e| e["type"] == "turn_done");
        for chunk in send.chunks(400) {
            let Ok(events) = chunk
                .iter()
                .map(|ev| crypto.seal_event(ev))
                .collect::<Result<Vec<_>>>()
            else {
                continue;
            };
            let body = chanto_proto::remote::AppendRemoteEvents { events };
            let mut tries = 0;
            while c.post::<Value>(&events_url, &body).await.is_err() {
                tries += 1;
                // Given up on: the dashboard misses these, and catches up at the next snapshot.
                if tries >= 5 || hub.closed.is_cancelled() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(tries)).await;
            }
        }
        if turn_ended {
            put_snapshot(&hub, &c, &path, &crypto).await;
        }
        // A burst that follows this post is gathered, not posted piecemeal.
        last_send = Some(Instant::now());
    }
}

/// Upload the conversation as it stands, sealed for a joined reader (or with
/// the link's key before anyone has joined).
async fn put_snapshot(hub: &Hub, c: &crate::runs::cloud::Cloud, path: &str, crypto: &RemoteCrypto) {
    let Ok(sealed) = crypto.seal_snapshot(&hub.snapshot()) else {
        return;
    };
    let _ = c.put::<Value>(&format!("{path}/snapshot"), &sealed).await;
}

/// What the dashboard sends, handled like the local server's routes. The first
/// message bootstraps the session keys; after that every command is bound to
/// the reader, its purpose and a strictly increasing sequence.
async fn relay_commands(
    hub: Arc<Hub>,
    c: crate::runs::cloud::Cloud,
    path: String,
    crypto: Arc<RemoteCrypto>,
) {
    use chanto_proto::remote::{RemoteCommand, RemoteCommandList};
    let mut after = 0u64;
    // The last sequence acted on. Anything at or below it is a replay (or the
    // relay reordering), so it is dropped rather than run twice.
    let mut last_seq = 0u64;
    loop {
        let url = format!(
            "{path}/commands?after={after}&wait={}",
            chanto_proto::remote::MAX_WAIT_SECS
        );
        let poll = c.get::<RemoteCommandList>(&url);
        let list = tokio::select! {
            _ = hub.closed.cancelled() => return,
            r = poll => r,
        };
        match list {
            Ok(list) => {
                for item in list.commands {
                    after = item.seq;
                    if item.command.v == 1 {
                        // The reader's first message, sealed with the link's
                        // key. Only the first one is accepted.
                        if crypto.viewer().is_some() {
                            continue;
                        }
                        let Ok(RemoteCommand::Join { viewer, public }) =
                            crypto.open_join(&item.command)
                        else {
                            continue;
                        };
                        let Ok(Some(_)) = crypto.join(&viewer, &public) else {
                            continue;
                        };
                        // The conversation as it stands, now sealed for this
                        // reader, then a marker that says it's ready.
                        put_snapshot(&hub, &c, &path, &crypto).await;
                        if let Ok(ready) = crypto.seal_event(&json!({"type": "ready"})) {
                            let body = chanto_proto::remote::AppendRemoteEvents {
                                events: vec![ready],
                            };
                            let _ = c.post::<Value>(&format!("{path}/events"), &body).await;
                        }
                        continue;
                    }
                    if item.command.v != 2 {
                        continue;
                    }
                    let Some(key) = crypto.viewer() else {
                        continue;
                    };
                    if !key.accepts(last_seq, &item.command) {
                        continue;
                    }
                    let Some(seq) = item.command.seq else {
                        continue;
                    };
                    // A command we can't open wasn't meant for this session, or
                    // the relay altered it: skip it rather than act on it.
                    let Ok(command) = key.open::<RemoteCommand>(Purpose::Commands, &item.command)
                    else {
                        continue;
                    };
                    last_seq = seq;
                    match command {
                        RemoteCommand::Join { .. } => {}
                        RemoteCommand::Message { text } => {
                            hub.host.send(Input::Prompt(text));
                        }
                        RemoteCommand::Interrupt => {
                            hub.host.send(Input::Interrupt);
                        }
                        RemoteCommand::Respond {
                            request_id,
                            decision,
                            feedback,
                            answer,
                        } => {
                            let body = json!({"request_id": request_id, "decision": decision, "feedback": feedback, "answer": answer});
                            hub.answer(&request_id, body);
                        }
                    }
                }
            }
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

/// Mint and store a new link token, revoking any previous one.
fn fresh_token() -> String {
    let token = uuid::Uuid::new_v4().simple().to_string();
    *TOKEN.write().unwrap() = Some(token.clone());
    token
}

fn stop() -> String {
    // Clear the token first, so no stale link survives even a partial stop.
    *TOKEN.write().unwrap() = None;
    let running = RUNNING.lock().unwrap().take();
    let Some(r) = running else {
        return "Remote control is already off.".into();
    };
    if let Some(slot) = SLOT.get() {
        *slot.write().unwrap() = None;
    }
    r.hub.close();
    if let Some((c, path)) = r.cloud {
        tokio::spawn(async move {
            let _ = c.post::<Value>(&format!("{path}/end"), &json!({})).await;
        });
        return "Remote control is off; the dashboard no longer reaches this session.".into();
    }
    "Remote control is off; the phone's link no longer works.".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_token_rotates_on_each_start_and_clears_on_stop() {
        let a = fresh_token();
        let b = fresh_token();
        assert_ne!(
            a, b,
            "each start mints a new token, so the old link is revoked"
        );
        assert_eq!(TOKEN.read().unwrap().as_deref(), Some(b.as_str()));
        assert_eq!(stop(), "Remote control is already off.");
        assert!(TOKEN.read().unwrap().is_none(), "stop clears the token");
    }

    #[test]
    fn streamed_text_goes_to_the_cloud_as_one_event() {
        let events = vec![
            json!({"type": "text", "text": "Hel"}),
            json!({"type": "text", "text": "lo"}),
            json!({"type": "tool_start", "id": "t1", "label": "Bash(ls)"}),
            json!({"type": "reasoning", "text": "a"}),
            json!({"type": "reasoning", "text": "b"}),
            json!({"type": "text", "text": "!"}),
        ];
        let merged = merge_text(events);
        assert_eq!(merged.len(), 4);
        assert_eq!(
            (
                merged[0]["text"].as_str(),
                merged[2]["text"].as_str(),
                merged[3]["text"].as_str()
            ),
            (Some("Hello"), Some("ab"), Some("!"))
        );
    }

    #[tokio::test]
    async fn the_first_event_after_a_quiet_spell_is_sent_at_once() {
        let (tx, mut rx) = broadcast::channel(8);
        let closed = CancellationToken::new();
        tx.send(json!({"type": "text", "text": "hi"})).unwrap();
        let batch = tokio::time::timeout(
            Duration::from_millis(200),
            next_batch(&mut rx, &closed, Vec::new(), Some(Instant::now() - GATHER)),
        )
        .await
        .expect("the event waited for the gather window")
        .unwrap();
        assert_eq!(batch.len(), 1);
    }

    #[tokio::test]
    async fn the_replay_is_sent_at_once() {
        let (_tx, mut rx) = broadcast::channel(8);
        let closed = CancellationToken::new();
        let batch = tokio::time::timeout(
            Duration::from_millis(200),
            next_batch(&mut rx, &closed, vec![json!({"type": "info"})], None),
        )
        .await
        .expect("the replay waited for the gather window")
        .unwrap();
        assert_eq!(batch.len(), 1);
    }

    #[tokio::test]
    async fn a_burst_is_gathered_into_one_post() {
        let (tx, mut rx) = broadcast::channel(8);
        let closed = CancellationToken::new();
        let sender = tokio::spawn(async move {
            for text in ["a", "b", "c"] {
                tx.send(json!({"type": "text", "text": text})).unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let batch = tokio::time::timeout(
            Duration::from_secs(1),
            next_batch(&mut rx, &closed, Vec::new(), Some(Instant::now())),
        )
        .await
        .expect("the burst wasn't gathered")
        .unwrap();
        assert_eq!(batch.len(), 3);
        sender.await.unwrap();
    }

    #[tokio::test]
    async fn ending_remote_control_stops_the_upload() {
        let (_tx, mut rx) = broadcast::channel::<Value>(1);
        let closed = CancellationToken::new();
        closed.cancel();
        assert!(
            next_batch(&mut rx, &closed, Vec::new(), None)
                .await
                .is_none()
        );
    }

    #[derive(Default)]
    struct FakeHost {
        inputs: Mutex<Vec<Input>>,
        /// None: "a turn holds the session".
        messages: Mutex<Option<Vec<Value>>>,
    }

    impl Host for FakeHost {
        fn send(&self, input: Input) -> bool {
            self.inputs.lock().unwrap().push(input);
            true
        }
        fn snapshot(&self) -> Option<Vec<Value>> {
            self.messages.lock().unwrap().clone()
        }
        fn info(&self) -> Value {
            json!({"model": "mock", "mode": "default", "todos": []})
        }
    }

    /// A terminal whose dialogs stay open until the test answers them.
    #[derive(Default)]
    struct Terminal {
        asks: Mutex<Vec<oneshot::Sender<PermissionReply>>>,
        questions: Mutex<Vec<oneshot::Sender<Option<String>>>>,
        events: Mutex<Vec<Value>>,
    }

    #[async_trait]
    impl Frontend for Terminal {
        fn emit(&self, ev: Event) {
            if let Some(v) = event_json(&ev) {
                self.events.lock().unwrap().push(v);
            }
        }
        async fn permission(&self, _: PermissionRequest) -> PermissionReply {
            let (tx, rx) = oneshot::channel();
            self.asks.lock().unwrap().push(tx);
            rx.await.unwrap_or(PermissionReply::Deny(None))
        }
        async fn question(&self, _: &str, _: &[String]) -> Option<String> {
            let (tx, rx) = oneshot::channel();
            self.questions.lock().unwrap().push(tx);
            rx.await.ok().flatten()
        }
    }

    fn req() -> PermissionRequest {
        PermissionRequest {
            tool: "Bash".into(),
            label: "Bash(rm -rf build)".into(),
            detail: "rm -rf build".into(),
            always_label: "rm commands".into(),
            reason: None,
            proposed: None,
            auto_deny: None,
        }
    }

    fn setup() -> (Arc<FakeHost>, Arc<Hub>, Arc<Terminal>, Mirror) {
        let host = Arc::new(FakeHost::default());
        *host.messages.lock().unwrap() = Some(vec![json!({"role": "user", "content": "hi"})]);
        let hub = Hub::new(SESSION_ID, "secret", host.clone(), None);
        let term = Arc::new(Terminal::default());
        let slot: Slot = Arc::new(RwLock::new(Some(hub.clone())));
        let mirror = Mirror::new(term.clone(), slot);
        (host, hub, term, mirror)
    }

    async fn next_of(rx: &mut broadcast::Receiver<Value>, kind: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let v = rx.recv().await.unwrap();
                if v["type"] == kind {
                    return v;
                }
            }
        })
        .await
        .expect("event")
    }

    async fn until(f: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !f() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition");
    }

    #[test]
    fn events_are_mirrored_and_replayed_to_late_phones() {
        let (host, hub, term, mirror) = setup();
        let mut rx = hub.tx.subscribe();
        hub.user("fix the build", false);
        mirror.emit(Event::Text("Look".into()));
        mirror.emit(Event::Text("ing.".into()));
        mirror.emit(Event::ToolStart {
            id: "t1".into(),
            label: "Bash(make)".into(),
            parent: None,
        });
        // The terminal still gets everything.
        assert_eq!(term.events.lock().unwrap().len(), 3);
        // Live: each event as it happens.
        let live: Vec<Value> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(
            live.iter()
                .map(|v| v["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["user", "text", "text", "tool_start"]
        );
        // A phone connecting now: one merged text, then the tool.
        let (replay, _) = hub.subscribe();
        assert_eq!(replay.len(), 3);
        assert_eq!(replay[0], json!({"type": "user", "text": "fix the build"}));
        assert_eq!(replay[1]["text"], "Looking.");
        // Mid-turn the snapshot isn't re-read, so snapshot + log stays whole.
        *host.messages.lock().unwrap() = Some(vec![]);
        let snap = hub.snapshot();
        assert_eq!(snap["busy"], true);
        assert_eq!(snap["messages"][0]["content"], "hi");
        assert_eq!(hub.subscribe().0.len(), 3);
        // After the turn it is, and the log starts over.
        hub.turn_done(None);
        *host.messages.lock().unwrap() = Some(vec![
            json!({"role": "user", "content": "fix the build"}),
            json!({"role": "assistant", "content": "Looking."}),
        ]);
        let snap = hub.snapshot();
        assert_eq!(snap["busy"], false);
        assert_eq!(snap["messages"].as_array().unwrap().len(), 2);
        assert!(hub.subscribe().0.is_empty());
        // ...unless the session is still locked: then the log is kept.
        hub.turn_done(Some("boom"));
        *host.messages.lock().unwrap() = None;
        hub.snapshot();
        assert_eq!(
            hub.subscribe().0,
            vec![json!({"type": "turn_done", "error": "boom"})]
        );
        // Quiet prompts mark the turn without showing the text.
        hub.user("<task-notification>", true);
        assert!(hub.is_busy());
        assert_eq!(hub.subscribe().0.len(), 1);
    }

    #[tokio::test]
    async fn phone_answer_wins_and_closes_the_terminal_dialog() {
        let (_host, hub, term, mirror) = setup();
        let mut rx = hub.tx.subscribe();
        let mirror = Arc::new(mirror);
        let m = mirror.clone();
        let ask = tokio::spawn(async move { m.permission(req()).await });
        let ev = next_of(&mut rx, "permission_request").await;
        assert_eq!(ev["label"], "Bash(rm -rf build)");
        let id = ev["request_id"].as_str().unwrap().to_string();
        until(|| !term.asks.lock().unwrap().is_empty()).await;
        // A phone connecting now sees the open prompt.
        assert!(
            hub.subscribe()
                .0
                .iter()
                .any(|v| v["request_id"] == id.as_str())
        );
        assert!(hub.answer(&id, json!({"request_id": id, "decision": "allow_always"})));
        assert_eq!(ask.await.unwrap(), PermissionReply::AllowAlways);
        // The terminal's dialog sees its answer channel closed and goes away.
        assert!(term.asks.lock().unwrap()[0].is_closed());
        assert_eq!(
            next_of(&mut rx, "request_done").await["request_id"],
            id.as_str()
        );
        assert!(hub.pending.lock().unwrap().is_empty());
        assert!(term.events.lock().unwrap().iter().any(|e| {
            e["message"]
                .as_str()
                .is_some_and(|m| m.contains("always allowed"))
        }));
        // Answering again is refused.
        assert!(!hub.answer(&id, json!({"decision": "deny"})));
    }

    #[tokio::test]
    async fn terminal_answer_wins_and_closes_the_phone_dialog() {
        let (_host, hub, term, mirror) = setup();
        let mut rx = hub.tx.subscribe();
        let mirror = Arc::new(mirror);
        let m = mirror.clone();
        let ask = tokio::spawn(async move {
            m.question(
                "Which database?",
                &["Postgres".to_string(), "SQLite".to_string()],
            )
            .await
        });
        let ev = next_of(&mut rx, "question").await;
        assert_eq!(ev["options"][1], "SQLite");
        let id = ev["request_id"].as_str().unwrap().to_string();
        until(|| !term.questions.lock().unwrap().is_empty()).await;
        let tx = term.questions.lock().unwrap().pop().unwrap();
        tx.send(Some("SQLite".into())).unwrap();
        assert_eq!(ask.await.unwrap().as_deref(), Some("SQLite"));
        assert_eq!(
            next_of(&mut rx, "request_done").await["request_id"],
            id.as_str()
        );
        // The phone's late answer finds nothing to answer.
        assert!(!hub.answer(&id, json!({"answer": "Postgres"})));
        assert!(hub.subscribe().0.iter().all(|v| v["type"] != "question"));
    }

    #[tokio::test]
    async fn turning_remote_off_leaves_the_prompt_to_the_terminal() {
        let (_host, hub, term, mirror) = setup();
        let mut rx = hub.tx.subscribe();
        let mirror = Arc::new(mirror);
        let m = mirror.clone();
        let ask = tokio::spawn(async move { m.permission(req()).await });
        next_of(&mut rx, "permission_request").await;
        until(|| !term.asks.lock().unwrap().is_empty()).await;
        hub.close();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!ask.is_finished());
        let tx = term.asks.lock().unwrap().pop().unwrap();
        tx.send(PermissionReply::Deny(Some("not now".into())))
            .unwrap();
        assert_eq!(
            ask.await.unwrap(),
            PermissionReply::Deny(Some("not now".into()))
        );
        // With no hub in the slot, the mirror is the terminal alone.
        let (_host, _hub, term, _) = setup();
        let off = Mirror::new(term.clone(), Arc::new(RwLock::new(None)));
        off.emit(Event::Info("hello".into()));
        assert_eq!(term.events.lock().unwrap().len(), 1);
    }

    #[test]
    fn phone_decisions_map_to_replies() {
        assert_eq!(
            permission_reply(&json!({"decision": "allow"})),
            PermissionReply::Allow
        );
        assert_eq!(
            permission_reply(&json!({"decision": "allow_always"})),
            PermissionReply::AllowAlways
        );
        assert_eq!(
            permission_reply(&json!({"decision": "deny", "feedback": ""})),
            PermissionReply::Deny(None)
        );
        assert_eq!(
            permission_reply(&json!({"decision": "deny", "feedback": "use make"})),
            PermissionReply::Deny(Some("use make".into()))
        );
        assert_eq!(permission_reply(&json!({})), PermissionReply::Deny(None));
    }

    #[tokio::test]
    async fn http_api_routes_phone_input_to_the_terminal() {
        let (host, hub, _term, mirror) = setup();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let closed = hub.closed.clone();
        let app = router(hub.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(closed.cancelled_owned())
                .await
                .unwrap()
        });
        let client = crate::http::client();
        // The page is public; the API needs the token.
        assert_eq!(
            client
                .get(format!("{base}/"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let health: Value = client
            .get(format!("{base}/health"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            (health["remote"].clone(), health["session"].clone()),
            (json!(true), json!(SESSION_ID))
        );
        for path in [
            "/sessions",
            "/sessions/terminal",
            "/sessions/terminal/events",
            "/voice",
        ] {
            for token in [None, Some("wrong")] {
                let mut r = client.get(format!("{base}{path}"));
                if let Some(t) = token {
                    r = r.bearer_auth(t);
                }
                assert_eq!(
                    r.send().await.unwrap().status(),
                    StatusCode::UNAUTHORIZED,
                    "{path}"
                );
            }
        }
        let post = |path: &str, body: Value| {
            client
                .post(format!("{base}{path}"))
                .bearer_auth("secret")
                .json(&body)
                .send()
        };
        assert_eq!(
            post("/sessions/terminal/messages", json!({"text": "hi"}))
                .await
                .unwrap()
                .status(),
            StatusCode::ACCEPTED
        );
        let list: Value = client
            .get(format!("{base}/sessions"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list["sessions"][0]["id"], SESSION_ID);
        assert_eq!(
            post("/sessions/terminal/messages", json!({"text": "  "}))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            post("/sessions/other/messages", json!({"text": "hi"}))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            post("/sessions", json!({})).await.unwrap().status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            post("/sessions/terminal/interrupt", json!({}))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            *host.inputs.lock().unwrap(),
            vec![Input::Prompt("hi".into()), Input::Interrupt]
        );
        // A phone connecting mid-turn gets the snapshot, then the turn so far.
        hub.user("hi", false);
        mirror.emit(Event::Text("Working on it".into()));
        let snap: Value = client
            .get(format!("{base}/sessions/terminal"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            (snap["busy"].clone(), snap["messages"][0]["content"].clone()),
            (json!(true), json!("hi"))
        );
        let mut stream = client
            .get(format!("{base}/sessions/terminal/events"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap();
        let mut seen = String::new();
        while !seen.contains("Working on it") {
            let chunk = tokio::time::timeout(Duration::from_secs(5), stream.chunk())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        assert!(seen.contains("\"type\":\"user\""));
        // Answers route to the open prompt; unknown ones are 404.
        let m = Arc::new(mirror);
        let m2 = m.clone();
        let ask = tokio::spawn(async move { m2.permission(req()).await });
        until(|| !hub.pending.lock().unwrap().is_empty()).await;
        let id = hub.pending.lock().unwrap().keys().next().unwrap().clone();
        assert_eq!(
            post(
                "/sessions/terminal/respond",
                json!({"request_id": "req-nope", "decision": "allow"})
            )
            .await
            .unwrap()
            .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            post(
                "/sessions/terminal/respond",
                json!({"request_id": id, "decision": "deny", "feedback": "use make"})
            )
            .await
            .unwrap()
            .status(),
            StatusCode::OK
        );
        assert_eq!(
            ask.await.unwrap(),
            PermissionReply::Deny(Some("use make".into()))
        );
        // Off: the stream ends and the API refuses.
        hub.close();
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Ok(Some(_)) = stream.chunk().await {}
        })
        .await
        .expect("stream ends");
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server stops")
            .unwrap();
    }

    #[tokio::test]
    async fn the_page_cloud_mode_serves_locally_comes_from_this_binary() {
        let (_host, hub, _term, _mirror) = setup();
        let base = serve_locally(hub.clone()).await.expect("loopback listener");
        let client = crate::http::client();
        // The same page `chanto web` serves, with the headers that keep it
        // from leaking the link: no referrer, no caching.
        let page = client.get(format!("{base}/")).send().await.unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        assert_eq!(page.headers()["referrer-policy"], "no-referrer");
        assert_eq!(page.headers()["cache-control"], "no-store");
        assert!(page.text().await.unwrap().contains("/app.js"));
        // The API behind it still needs this session's token.
        assert_eq!(
            client
                .get(format!("{base}/sessions"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let list: Value = client
            .get(format!("{base}/sessions"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list["sessions"][0]["id"], SESSION_ID);
        // Turning remote control off stops it too.
        hub.close();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if client.get(format!("{base}/health")).send().await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the local page stops");
    }
}
