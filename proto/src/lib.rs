//! Wire types shared by the Chanto CLI and its hosted service.
//!
//! Everything here is plain JSON over HTTPS. Authenticated routes take
//! `Authorization: Bearer <token>`, where the token comes from the device
//! login flow (`/v1/auth/device/*`) or an API token created on the service.
//!
//! Routes:
//! - `POST /v1/auth/device/start` → [`DeviceStart`]
//! - `POST /v1/auth/device/poll` ([`DevicePoll`]) → [`DevicePollResult`]
//! - `GET /v1/auth/providers` → [`AuthProviders`]
//! - `POST /v1/auth/browser/start` ([`BrowserStart`]) → [`BrowserStarted`]
//! - `POST /v1/auth/browser/poll` ([`BrowserLoginPoll`]) → [`DevicePollResult`]
//! - `DELETE /v1/auth/token`: revoke the token used for the request
//! - `GET /v1/me` → [`User`]
//! - `POST /v1/shares?title=<title>` (body: the share HTML) → [`Share`]
//! - `GET /v1/shares` → [`ShareList`]
//! - `DELETE /v1/shares/{id}`
//! - `GET /s/{id}`: the share page, public to anyone with the link
//!
//! Errors are an [`ApiError`] body with a non-2xx status. Runs and runners
//! are in [`runs`]; agents, which start runs, in [`agents`]; driving a
//! terminal session from the dashboard in [`remote`].

pub mod agents;
pub mod remote;
pub mod runs;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Longer titles are truncated by the server.
pub const SHARE_TITLE_MAX: usize = 200;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub login: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

/// Show `user_code` and `verification_uri` to the user, then poll with
/// `device_code` every `interval` seconds until `expires_in` runs out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceStart {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DevicePoll {
    pub device_code: String,
    /// Names the issued token, e.g. the machine's hostname.
    #[serde(default)]
    pub client_name: Option<String>,
}

/// An expired or denied code is an [`ApiError`] (`expired`, `access_denied`)
/// instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DevicePollResult {
    /// Not approved yet: poll again after `interval` seconds (may have grown).
    Pending { interval: u64 },
    Complete {
        token: String,
        user: User,
        /// When the token stops working; absent or `None`: until revoked.
        #[serde(default)]
        expires_at: Option<DateTime<Utc>>,
    },
}

/// One signed-in device: an API token minted by a sign-in (a browser, a CLI
/// machine, a runner).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    /// Given at sign-in: a browser description or the machine's name.
    pub name: String,
    pub created_at: DateTime<Utc>,
    /// Last time a request used it; `None` for a token that never has.
    pub last_used_at: Option<DateTime<Utc>>,
    /// The token this request authenticated with.
    #[serde(default)]
    pub current: bool,
    /// When it stops working; absent or `None`: until revoked.
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

/// A token replacing the one the request used: rotate before it expires, or
/// after a suspected leak. The old token stops working.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenRenewal {
    pub token: String,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceList {
    /// Newest first.
    pub devices: Vec<Device>,
}

/// The sign-in providers a server offers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthProviders {
    pub providers: Vec<AuthProvider>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthProvider {
    /// `github`, `google`, `gitlab` or `bitbucket`.
    pub id: String,
    pub name: String,
    /// Through the browser (`/v1/auth/browser/*`).
    pub browser: bool,
    /// Through the device flow (`/v1/auth/device/*`; GitHub only).
    pub device: bool,
}

/// Starts a browser sign-in: open `authorize_url`, then poll with
/// `poll_code` until it completes. `return_to` (under the server's website)
/// is where the browser goes afterwards; without it, a page saying to go
/// back to the terminal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowserStart {
    pub provider: String,
    #[serde(default)]
    pub client_name: Option<String>,
    #[serde(default)]
    pub return_to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowserStarted {
    pub authorize_url: String,
    /// A secret: whoever polls with it gets the token.
    pub poll_code: String,
    pub expires_in: u64,
    /// Shown in the terminal and typed into the browser's confirmation page,
    /// so a sign-in link sent to someone else can't be completed silently.
    /// Empty from older servers, which have no confirmation step.
    #[serde(default)]
    pub user_code: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowserLoginPoll {
    pub poll_code: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Share {
    pub id: String,
    /// Anyone with this URL can read the share.
    pub url: String,
    pub title: String,
    pub size_bytes: u64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShareList {
    /// Newest first.
    pub shares: Vec<Share>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unauthorized,
    Forbidden,
    NotFound,
    /// The resource changed under you, e.g. a runner that lost its lease.
    Conflict,
    BadRequest,
    TooLarge,
    /// Too many requests in a short time; `Retry-After` says when to try again.
    RateLimited,
    /// The plan's limit is used up; upgrading or waiting for the next period helps.
    QuotaExceeded,
    Expired,
    AccessDenied,
    NotConfigured,
    Upstream,
    Internal,
    /// A code this version of the client doesn't know.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiError {
    pub error: ErrorCode,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_result_is_tagged_by_status() {
        let pending = serde_json::to_string(&DevicePollResult::Pending { interval: 5 }).unwrap();
        assert_eq!(pending, r#"{"status":"pending","interval":5}"#);
    }

    #[test]
    fn known_and_unknown_error_codes_parse() {
        let e: ApiError =
            serde_json::from_str(r#"{"error":"rate_limited","message":"slow down"}"#).unwrap();
        assert_eq!(e.error, ErrorCode::RateLimited);
        let e: ApiError =
            serde_json::from_str(r#"{"error":"a_future_code","message":"who knows"}"#).unwrap();
        assert_eq!(e.error, ErrorCode::Unknown);
    }
}
