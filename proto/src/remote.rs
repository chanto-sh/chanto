//! Remote control through Chanto cloud: a terminal session (`/remote cloud`)
//! that you can follow and drive from the dashboard, without opening a port on
//! your machine. The CLI connects out: it uploads the session's events and
//! long-polls for what the dashboard sends. (It may also serve the dashboard
//! on loopback, straight from its own binary, for readers who would rather not
//! run the cloud's copy of the page. That never touches the wire described here.)
//!
//! Everything a person would not want the server to read — the conversation,
//! what they type, the directory and machine — travels as [`Sealed`]: encrypted
//! with a key the CLI generates and puts only in the link's fragment, which a
//! browser never sends. The server stores and relays envelopes it can't open.
//!
//! CLI routes:
//! - `POST /v1/remote/sessions` ([`CreateRemoteSession`], your API token) →
//!   [`RemoteSessionCreated`]; the rest take its session token
//! - `PUT /v1/remote/sessions/{id}/snapshot` (a [`Sealed`] conversation):
//!   later readers start from it and the events after it
//! - `POST /v1/remote/sessions/{id}/events` ([`AppendRemoteEvents`])
//! - `GET /v1/remote/sessions/{id}/commands?after=<seq>&wait=<secs>` →
//!   [`RemoteCommandList`], waiting up to `wait` (at most 25) for one
//! - `POST /v1/remote/sessions/{id}/end`
//!
//! Dashboard routes (your sign-in; others' sessions are "not found"):
//! - `GET /v1/remote/sessions` → [`RemoteSessionList`]
//! - `GET /v1/remote/sessions/{id}` → [`RemoteSessionView`]
//! - `GET /v1/remote/sessions/{id}/events?after=<seq>&wait=<secs>` → [`RemoteEventList`]
//! - `POST /v1/remote/sessions/{id}/commands` ([`Sealed`])

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The most a `wait` can be, in seconds.
pub const MAX_WAIT_SECS: u64 = 25;

/// The URL fragment that carries a session's key (`#k=<base64url>`). Browsers
/// don't send fragments, so the server never sees it.
pub const KEY_FRAGMENT: &str = "k";

/// The URL fragment that carries the CLI's handshake public key
/// (`#p=<base64url>`), for the ephemeral key exchange a reader uses.
pub const PUBLIC_FRAGMENT: &str = "p";

/// An end-to-end encrypted payload. The relay stores and returns it as it is;
/// only the CLI and the reader, which share the key, can open it.
///
/// `id` is random per message and is bound into the authentication tag, so a
/// relay can't swap one envelope for another without it being noticed. Version
/// 2 adds the session, the reader and a per-message sequence to the tag as
/// well; those fields are absent from version 1 envelopes, which this server
/// still relays untouched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sealed {
    /// Envelope version, so the shape can change without a second endpoint.
    pub v: u8,
    /// Random per-message id, base64url.
    pub id: String,
    /// AEAD nonce, base64url.
    pub n: String,
    /// Ciphertext with its tag, base64url.
    pub c: String,
    /// Version 2: the message's position in its purpose's counter, starting
    /// at one. It is the AEAD nonce's low half and is authenticated, so a
    /// replayed or reordered envelope is rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// Version 2: which purpose the envelope was sealed for (`meta`, `events`…).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Version 2: the reader whose ephemeral key opened the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewer: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CreateRemoteSession {
    /// What the dashboard calls it, for sessions that aren't encrypted. Sealed
    /// sessions carry it in `sealed_meta` instead.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub machine: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// The title, machine and directory, sealed with the session's key. When
    /// set, the server keeps `title` generic and never sees the real ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed_meta: Option<Sealed>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteSessionCreated {
    pub id: String,
    /// For the CLI's routes; shown once.
    pub token: String,
    /// The session in the dashboard. The CLI adds the key fragment.
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteSession {
    pub id: String,
    pub title: String,
    pub machine: Option<String>,
    pub cwd: Option<String>,
    /// The sealed title, machine and directory, when the session is encrypted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed_meta: Option<Sealed>,
    pub created_at: DateTime<Utc>,
    /// When the CLI last checked in.
    pub last_seen_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    /// Connected now: checked in within the last minute and not ended.
    pub live: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteSessionList {
    /// Live ones first, then the most recent.
    pub sessions: Vec<RemoteSession>,
}

/// A session to show: the conversation as of `snapshot_seq` (`{model, mode,
/// todos, busy, messages}`, as the CLI sent it), then its events after that.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteSessionView {
    #[serde(flatten)]
    pub session: RemoteSession,
    /// The snapshot as the CLI sent it: a [`Sealed`] envelope when the session
    /// is encrypted (`sealed_meta` is set), plain JSON otherwise.
    pub snapshot: Value,
    pub snapshot_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppendRemoteEvents {
    /// The CLI's frontend events (`text`, `tool_start`, `permission_request`…),
    /// each sealed.
    pub events: Vec<Sealed>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteEvent {
    /// Starts at 1 and increases by 1 per event within a session.
    pub seq: u64,
    pub data: Sealed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteEventList {
    pub events: Vec<RemoteEvent>,
    /// The latest snapshot's position: a reader behind it should reload the session.
    pub snapshot_seq: u64,
    pub live: bool,
}

/// What the dashboard asks the terminal to do, once it opens the envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteCommand {
    /// The reader's first message, sealed with the link's bootstrap key: it
    /// carries the reader's ephemeral public key, which both ends use to derive
    /// the session's real keys. The CLI accepts one and then forgets the
    /// handshake's private half.
    Join {
        /// Random per reader, base64url.
        viewer: String,
        /// The reader's ephemeral P-256 public key, base64url.
        public: String,
    },
    /// A message or slash command, as if typed.
    Message { text: String },
    /// An answer to an open `permission_request` (`decision`: allow,
    /// allow_always or deny, with `feedback`) or `question` (`answer`).
    Respond {
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decision: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        feedback: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answer: Option<String>,
    },
    /// Stop the running turn (esc).
    Interrupt,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteCommandItem {
    pub seq: u64,
    pub command: Sealed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteCommandList {
    pub commands: Vec<RemoteCommandItem>,
}
