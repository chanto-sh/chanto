//! Runs: a task executed by a runner, either one the customer operates
//! (self-hosted) or one the service operates (hosted). Runners pull work over
//! outbound HTTPS, so a self-hosted runner needs no inbound access and the
//! customer's code and model keys stay on their machines.
//!
//! Client routes (user token):
//! - `POST /v1/orgs/{org}/runs` ([`CreateRun`]) → [`Run`]
//! - `GET /v1/orgs/{org}/runs` → [`RunList`]
//! - `GET /v1/runs/{id}` → [`Run`]
//! - `GET /v1/runs/{id}/events?after=<seq>` → [`RunEventList`]
//! - `POST /v1/runs/{id}/cancel` → [`Run`]
//!
//! Runner routes (runner token from [`RunnerRegistered`]):
//! - `POST /v1/runners/register` ([`RegisterRunner`]) → [`RunnerRegistered`]
//! - `POST /v1/runner/claim` ([`Claim`]) → [`Job`], or 204 if nothing arrived within `wait_secs`
//! - `POST /v1/runner/runs/{id}/heartbeat` → [`HeartbeatReply`]
//! - `POST /v1/runner/runs/{id}/credentials` → [`RunCredentials`]
//! - `POST /v1/runner/runs/{id}/events` ([`AppendEvents`])
//! - `POST /v1/runner/runs/{id}/complete` ([`CompleteRun`])
//!
//! Run tools (a run's own commands, with [`RunTools`]'s token):
//! - `GET /v1/run-tools/models/{upstream}`: a model provider's list, with its
//!   prices (as the provider's own `GET /models` gives it)
//!
//! A runner keeps its job by sending a heartbeat within `lease_secs`. One that
//! misses it loses the job (it is queued again) and gets `409 conflict` on its
//! next call, which means: stop working on it.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where a run may execute.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    /// Only on the org's own runners.
    SelfHosted,
    /// Only on the service's runners.
    Hosted,
    /// The org's runners if one is online, else hosted (also after a grace
    /// period if they are all busy).
    #[default]
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerKind {
    SelfHosted,
    Hosted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RunStatus::Succeeded | RunStatus::Failed | RunStatus::Cancelled
        )
    }
}

/// Whose model keys runs use: an org setting that a run or an agent can
/// override (`model_keys` on [`CreateRun`] and `AgentSpec`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelKeys {
    /// The org's own: secrets (e.g. `ANTHROPIC_API_KEY`) or the runner's configuration.
    #[default]
    Own,
    /// The service's, through its model gateway, counted toward spending.
    Hosted,
    /// The org's own model endpoint, called directly by its runs.
    Endpoint,
}

/// Whether the model runs after a run's script: every time, only when the
/// script changed files, only when it failed, or never (script only).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRuns {
    #[default]
    Always,
    OnChange,
    OnFailure,
    Never,
}

/// The most a script can be, in bytes.
pub const MAX_SCRIPT_BYTES: usize = 16 * 1024;
/// At most this many checks per run or agent, each at most [`MAX_CHECK_BYTES`].
pub const MAX_CHECKS: usize = 10;
pub const MAX_CHECK_BYTES: usize = 1000;
/// The environment variable a run's checks reach Chanto in, one per line;
/// set but blank: no checks.
pub const CHECKS_ENV: &str = "CHANTO_CHECKS";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CreateRun {
    /// Git URL the runner clones.
    pub repo: String,
    /// Branch, tag or commit; the default branch if absent.
    #[serde(default)]
    pub git_ref: Option<String>,
    pub task: String,
    /// The org's default if absent.
    #[serde(default)]
    pub placement: Option<Placement>,
    /// Only runners that have every one of these labels can take the run.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Whose model keys this run uses; the org's setting if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_keys: Option<ModelKeys>,
    /// The gateway model, when the run uses the service's keys; the org's
    /// choice (else the gateway's default) if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_model: Option<String>,
    /// Shell commands run first, exactly as written, in the checkout (bash).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// With a script: when the model runs after it (`task` is its
    /// instructions). Without one, the model always runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_runs: Option<ModelRuns>,
    /// What "done" is checked with ([`MAX_CHECKS`] commands that must exit
    /// 0). Absent: detected from the repository; empty: no checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunResult {
    pub summary: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub pr_url: Option<String>,
    /// Why no pull request was opened for a pushed branch (set by the server).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_error: Option<String>,
    /// The commit the run started from (set by the runner).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// Who or what started a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunOrigin {
    /// Someone in the org, from the dashboard, the CLI or an agent's "Run now".
    Person { login: String },
    /// An issue or pull request on GitHub: a mention, a label, a new issue…
    Github {
        repo: String,
        number: u64,
        is_pr: bool,
    },
    /// Its agent's schedule.
    Schedule,
}

/// What a run has used so far.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunUsage {
    /// Calls to the model gateway (models on the service's keys).
    pub model_requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub model_usd: f64,
    /// Time on a hosted runner, as charged.
    pub hosted_seconds: u64,
    pub hosted_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    /// The org's slug.
    pub org: String,
    pub status: RunStatus,
    pub repo: String,
    pub git_ref: Option<String>,
    pub task: String,
    pub placement: Placement,
    pub labels: Vec<String>,
    /// Who is running it (or last ran it).
    pub runner_kind: Option<RunnerKind>,
    pub attempts: u32,
    pub cancel_requested: bool,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub result: Option<RunResult>,
    /// The agent that started it, by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The model override it was queued with ([`CreateRun`]); absent: the org's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_keys: Option<ModelKeys>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<RunOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_runs: Option<ModelRuns>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<Vec<String>>,
    /// Absent until it has used anything metered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<RunUsage>,
    /// Why it hasn't started, while it's queued (one run's page, not lists).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<RunWait>,
}

/// Why a queued run hasn't started yet ([`Run::waiting`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunWait {
    /// The org's own runners are online; one takes it when it's free.
    OwnRunner,
    /// A hosted runner is starting for it.
    HostedStarting,
    /// The org's hosted runs are all in use (its hosted concurrency).
    HostedBusy,
    /// Nothing can take it: none of the org's runners are online, and hosted
    /// runners are ruled out (its placement, self-hosted-only mode, or a
    /// server without them).
    NoRunner,
    /// Hosted runners need billing on (a card on file).
    HostedNeedsBilling,
    /// The org has reached its spending limit for the month.
    SpendLimit,
    /// A hosted runner would use the org's own model keys, and it has none saved.
    ModelKeyMissing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunList {
    /// Newest first.
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEvent {
    /// Starts at 1 and increases by 1 per event within a run.
    pub seq: u64,
    pub at: DateTime<Utc>,
    /// The same objects as `--output-format stream-json`.
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEventList {
    pub events: Vec<RunEvent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegisterRunner {
    /// From an org admin (self-hosted) or the operator (hosted pool).
    pub registration_token: String,
    pub name: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub version: Option<String>,
    /// Take one job, then deregister: for runners in throwaway VMs or
    /// containers, so no state carries over between jobs.
    #[serde(default)]
    pub ephemeral: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunnerRegistered {
    pub runner_id: String,
    pub runner_token: String,
    pub kind: RunnerKind,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Claim {
    /// How long to wait for a job before answering 204; the server caps it.
    #[serde(default)]
    pub wait_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub run_id: String,
    pub repo: String,
    pub git_ref: Option<String>,
    pub task: String,
    pub labels: Vec<String>,
    /// Starts at 1; higher means an earlier runner lost the job.
    pub attempt: u32,
    pub lease_secs: u64,
    /// Run first, before (or instead of) the model: see [`CreateRun`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    #[serde(default)]
    pub model_runs: ModelRuns,
    /// The run's checks ([`CreateRun::checks`]); the runner hands them to
    /// Chanto as [`CHECKS_ENV`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatReply {
    /// Stop, then complete the run as `cancelled`.
    pub cancel_requested: bool,
    pub lease_secs: u64,
}

/// What a run needs that the runner's machine may not have. Only the runner
/// holding the run gets them; fetch them when the run starts and again once
/// `git.expires_at` has passed. Treat every value as a secret: keep it out of
/// URLs, logs and transcripts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunCredentials {
    /// For cloning `Job::repo` and pushing the run's branch. `None`: the org
    /// has no connection for the repo; use the machine's own git credentials.
    #[serde(default)]
    pub git: Option<GitCredential>,
    /// The org's secrets, as environment variables for the run, including for
    /// its shell commands.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The model through the server's gateway, when the org uses the
    /// service's model keys. `None`: the runner's own model configuration
    /// (the org's own keys may be among `env`).
    #[serde(default)]
    pub model: Option<ModelAccess>,
    /// The server's run tools (`/v1/run-tools/…`), for the run's own commands
    /// and scripts: `CHANTO_CLOUD_URL` and `CHANTO_RUN_TOKEN` in their
    /// environment. Not the model gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<RunTools>,
}

/// A run's access to the server's run tools; works only while it's running.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunTools {
    pub url: String,
    pub token: String,
}

/// A model endpoint for one run, used instead of the runner's configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelAccess {
    /// The API it speaks, as a Chanto provider kind (`anthropic`).
    pub provider: String,
    pub base_url: String,
    /// Works only while this runner holds the run.
    pub api_key: String,
    /// The model to use; `None`: the runner's default for the provider.
    #[serde(default)]
    pub model: Option<String>,
}

/// HTTP basic credentials for one repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GitCredential {
    /// The HTTPS URL they are for; use it instead of `Job::repo`, which may be
    /// an SSH URL.
    pub url: String,
    pub username: String,
    pub password: String,
    /// `None`: doesn't expire during the run.
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppendEvents {
    pub events: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompleteRun {
    /// `succeeded`, `failed` or `cancelled`.
    pub status: RunStatus,
    pub result: RunResult,
}
