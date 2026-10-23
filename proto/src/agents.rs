//! Agents: a standing job in an org, such as "triage new issues" or "keep
//! dependencies current". An agent has instructions and a repository, and
//! starts runs ([`crate::runs`]) on demand, on a schedule, or when something
//! happens in its repository. Each start is an ordinary run whose task is the
//! agent's instructions plus what started it.
//!
//! Client routes (user token):
//! - `GET /v1/orgs/{org}/agents` → [`AgentList`]
//! - `POST /v1/orgs/{org}/agents` ([`AgentSpec`]) → [`Agent`]
//! - `GET /v1/orgs/{org}/agents/{name}` → [`Agent`]
//! - `PUT /v1/orgs/{org}/agents/{name}` ([`AgentSpec`]) → [`Agent`]; renaming is allowed
//! - `DELETE /v1/orgs/{org}/agents/{name}`; its runs are kept
//! - `POST /v1/orgs/{org}/agents/{name}/runs` ([`RunAgent`]) → [`crate::runs::Run`]
//! - `GET /v1/orgs/{org}/runs?agent={name}` lists an agent's runs

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::runs::{ModelKeys, ModelRuns, Placement, RunStatus};

/// Agent names: lowercase letters, digits and dashes, starting with a letter.
pub const AGENT_NAME_MAX: usize = 64;

pub fn valid_agent_name(name: &str) -> bool {
    name.len() <= AGENT_NAME_MAX
        && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.ends_with('-')
}

/// What in an agent's repository starts it. Events only count from people
/// with write access (the repository's owner, members and collaborators), so
/// strangers can't spend an org's money from a public repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
pub enum AgentTrigger {
    /// A new issue.
    IssueOpened,
    /// A label added to an issue; any label if `label` is absent.
    IssueLabeled {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// A new pull request (or one marked ready for review), not from a fork.
    PullRequestOpened,
    /// New commits pushed to an open pull request, not from a fork.
    PullRequestUpdated,
    /// A comment on an issue or pull request that mentions the app and the
    /// agent by name: `@chanto triage …`.
    Mention,
}

/// What a user sets; the server adds the rest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSpec {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// The standing task, given to every run's model; may be empty when the
    /// script is all there is (`model_runs: never`).
    pub instructions: String,
    /// Git URL the runs clone.
    pub repo: String,
    /// Branch to start from; the default branch if absent. Pull request
    /// events work on the pull request's own branch.
    #[serde(default)]
    pub git_ref: Option<String>,
    /// The org's default if absent.
    #[serde(default)]
    pub placement: Option<Placement>,
    #[serde(default)]
    pub labels: Vec<String>,
    /// Five-field cron expression in UTC (`0 9 * * 1-5`: weekdays at 09:00),
    /// at most every 10 minutes. A scheduled start is skipped while the
    /// agent's previous run hasn't finished.
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub triggers: Vec<AgentTrigger>,
    /// A disabled agent only starts by hand.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Whose model keys its runs use; the org's setting if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_keys: Option<ModelKeys>,
    /// The gateway model, when its runs use the service's keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_model: Option<String>,
    /// Shell commands each run starts with, exactly as written: the
    /// deterministic part. Then the model follows `instructions`, as
    /// `model_runs` says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_runs: Option<ModelRuns>,
    /// What "done" is checked with: commands that must exit 0. Absent: the
    /// runner detects them from the repository (`cargo test`, `npm test`…);
    /// empty: no checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<Vec<String>>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRunRef {
    pub id: String,
    pub status: RunStatus,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    /// The org's slug.
    pub org: String,
    #[serde(flatten)]
    pub spec: AgentSpec,
    /// When the schedule next starts it, if it has one and is enabled.
    pub next_run_at: Option<DateTime<Utc>>,
    pub last_run: Option<AgentRunRef>,
    /// Why the last scheduled or event start didn't happen (a spending
    /// limit, a missing connection); cleared by the next start.
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentList {
    /// By name.
    pub agents: Vec<Agent>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunAgent {
    /// Added to the agent's instructions for this run ("look at issue #42").
    #[serde(default)]
    pub input: Option<String>,
    /// Overrides the agent's branch for this run.
    #[serde(default)]
    pub git_ref: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(valid_agent_name("triage") && valid_agent_name("deps-weekly2"));
        assert!(
            !valid_agent_name("")
                && !valid_agent_name("Triage")
                && !valid_agent_name("2x")
                && !valid_agent_name("a_b")
                && !valid_agent_name("a-")
        );
    }

    #[test]
    fn spec_wire_format() {
        let spec: AgentSpec = serde_json::from_value(serde_json::json!({
            "name": "triage", "instructions": "Label new issues.", "repo": "https://github.com/acme/api.git",
            "schedule": "0 9 * * 1-5", "triggers": [{"on": "issue_opened"}, {"on": "issue_labeled", "label": "bug"}]
        }))
        .unwrap();
        assert!(spec.enabled && spec.placement.is_none());
        assert_eq!(
            spec.triggers,
            vec![
                AgentTrigger::IssueOpened,
                AgentTrigger::IssueLabeled {
                    label: Some("bug".into())
                }
            ]
        );
        assert_eq!(
            serde_json::to_value(&spec.triggers[0]).unwrap(),
            serde_json::json!({"on": "issue_opened"})
        );
    }
}
