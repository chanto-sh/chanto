---
title: 'API'
section: 'Cloud & automation'
order: 32
description: 'The HTTP API used by the CLI, runners and the dashboard.'
---

> Chanto cloud isn't publicly available yet. This describes the client interface and how it will work; nothing else in Chanto needs it.

Everything is JSON over HTTPS. Authenticated routes take `Authorization: Bearer <token>`. Errors are a JSON body with a non-2xx status:

```json
{
  "error": "quota_exceeded",
  "message": "the free plan allows 2 self-hosted runners; upgrade the org for more"
}
```

Error codes: `unauthorized`, `forbidden`, `not_found`, `conflict`, `bad_request`, `too_large`, `quota_exceeded`, `rate_limited`, `expired`, `access_denied`, `not_configured`, `upstream`, `internal`. Clients should treat unknown codes as errors too. The wire types are in the Chanto repository's `proto/` crate (MIT or Apache-2.0).

## Accounts

| Route                           |                                                                                                                                                                                                                                                  |
| ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `GET /v1/auth/providers`        | The sign-in providers the server has set up: `{ "providers": [{ "id", "name", "browser", "device" }] }`.                                                                                                                                         |
| `POST /v1/auth/device/start`    | Start a GitHub device login: returns `device_code`, `user_code`, `verification_uri`, `expires_in`, `interval`.                                                                                                                                   |
| `POST /v1/auth/device/poll`     | `{ "device_code", "client_name" }` → `{"status": "pending", "interval"}` or `{"status": "complete", "token", "user"}`.                                                                                                                           |
| `POST /v1/auth/browser/start`   | `{ "provider", "client_name", "return_to" }` → `{ "authorize_url", "poll_code", "expires_in" }`. `provider` is `github`, `google`, `gitlab` or `bitbucket`; `return_to` must be under the server's website. Send the browser to `authorize_url`. |
| `GET /v1/auth/browser/callback` | Where the provider sends the browser back.                                                                                                                                                                                                       |
| `POST /v1/auth/browser/poll`    | `{ "poll_code" }` → the same answers as the device poll. The token is minted only then.                                                                                                                                                          |
| `DELETE /v1/auth/token`         | Revoke the token used for the request.                                                                                                                                                                                                           |
| `GET /v1/me`                    | The signed-in user.                                                                                                                                                                                                                              |

## Shares

| Route                           |                                                         |
| ------------------------------- | ------------------------------------------------------- |
| `POST /v1/shares?title=<title>` | Body: the share HTML. Returns the share with its `url`. |
| `GET /v1/shares`                | Your shares, newest first.                              |
| `DELETE /v1/shares/{id}`        | Delete one of your shares.                              |
| `GET /s/{id}`                   | The share page, public to anyone with the link.         |

## Orgs (Chanto cloud)

| Route                                   |                                                                                                                                                                                                                   |
| --------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `GET /v1/orgs`                          | Your orgs with your role, plan and settings.                                                                                                                                                                      |
| `POST /v1/orgs`                         | `{ "slug", "name" }`: create a team org.                                                                                                                                                                          |
| `PATCH /v1/orgs/{org}`                  | Any of `name`, `default_placement`, `self_hosted_only`, `model_keys` (`own`, `hosted` or `endpoint`), `gateway_model`, `spend_cap_usd` (0–1000) and `secrets_for_triggered_runs` (admin).                         |
| `GET /v1/orgs/{org}/members`            | Members and roles.                                                                                                                                                                                                |
| `POST /v1/orgs/{org}/members`           | `{ "login", "role" }` (admin): add a member or change their role (`member` by default). They need to have signed in once; use `provider:login` (`gitlab:alice`) when people on different providers share a login. |
| `DELETE /v1/orgs/{org}/members/{login}` | Remove a member, or leave.                                                                                                                                                                                        |
| `GET /v1/orgs/{org}/usage`              | This month's spending (hosted runners and models on our keys), what the plan includes, what's left of the trial credit (or why it's waiting), the cap and the limit.                                              |

## Runs

| Route                                                   |                                                                                                                                                                                |
| ------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `POST /v1/orgs/{org}/runs`                              | `{ "repo", "git_ref", "task", "placement", "labels" }` → the run. `repo` is a git URL (https, ssh or `git@host:owner/repo`).                                                   |
| `GET /v1/orgs/{org}/runs?status=&agent=&limit=&before=` | Runs, newest first: filter by status or by the agent that started them, up to 100 a page, `before` a run id for the next page. Runs an agent started have its name in `agent`. |
| `GET /v1/runs/{id}`                                     | One run.                                                                                                                                                                       |
| `GET /v1/runs/{id}/events?after=<seq>`                  | The run's events after a sequence number.                                                                                                                                      |
| `POST /v1/runs/{id}/cancel`                             | Cancel a queued run, or ask a running one to stop.                                                                                                                             |

## Agents

| Route                                    |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| ---------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `GET /v1/orgs/{org}/agents`              | The org's agents, by name, each with `next_run_at`, `last_run` (`id`, `status`, `created_at`) and `last_error`.                                                                                                                                                                                                                                                                                                                                                                                       |
| `POST /v1/orgs/{org}/agents`             | Create an agent (admin): `{ "name", "description", "instructions", "repo", "git_ref", "placement", "labels", "schedule", "triggers", "enabled" }`. `name` is lowercase letters, digits and dashes; `schedule` a five-field cron in UTC, at most every 10 minutes; `triggers` a list of `{ "on": "issue_opened" }`, `{ "on": "issue_labeled", "label": "bug" }` (no `label`: any), `pull_request_opened`, `pull_request_updated` or `mention`, for repositories on GitHub. `409` if the name is taken. |
| `GET /v1/orgs/{org}/agents/{name}`       | One agent.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `PUT /v1/orgs/{org}/agents/{name}`       | Replace its settings, the same body as creating it (admin). The name can change.                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `DELETE /v1/orgs/{org}/agents/{name}`    | Delete it (admin); its runs are kept.                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| `POST /v1/orgs/{org}/agents/{name}/runs` | `{ "input", "git_ref" }`, both optional → the run. `input` is added to the instructions; `git_ref` overrides the agent's branch. Any member.                                                                                                                                                                                                                                                                                                                                                          |

## Runners

| Route                                      |                                                                                                               |
| ------------------------------------------ | ------------------------------------------------------------------------------------------------------------- |
| `POST /v1/orgs/{org}/runner-tokens`        | `{ "ttl_hours" }` (optional: default 7 days, at most 30): create a registration token (admin).                |
| `GET /v1/orgs/{org}/runner-tokens`         | The org's registration tokens (admin).                                                                        |
| `DELETE /v1/orgs/{org}/runner-tokens/{id}` | Revoke one (admin).                                                                                           |
| `GET /v1/orgs/{org}/runners`               | The org's runners and whether they're online.                                                                 |
| `DELETE /v1/orgs/{org}/runners/{id}`       | Remove a runner (admin).                                                                                      |
| `POST /v1/runners/register`                | `{ "registration_token", "name", "labels", "ephemeral" }` → the runner's own token.                           |
| `POST /v1/runner/claim`                    | `{ "wait_secs" }`: long-poll for a job (204 when none arrived; 401 once an ephemeral runner has had its run). |
| `POST /v1/runner/runs/{id}/credentials`    | What the run needs: git credentials for its repo, the org's secrets, and model access.                        |
| `POST /v1/runner/runs/{id}/heartbeat`      | Keep the lease; says whether a cancel was requested.                                                          |
| `POST /v1/runner/runs/{id}/events`         | `{ "events": [...] }`: append events.                                                                         |
| `POST /v1/runner/runs/{id}/complete`       | `{ "status", "result": { "summary", "branch" } }`.                                                            |

A runner that lost its lease gets `409 conflict` and must stop working on the run.

## Git, secrets and models

| Route                                                   |                                                                                                                                                                                                                                                       |
| ------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `GET /v1/orgs/{org}/git-connections`                    | The org's GitHub installations and GitLab or Bitbucket tokens.                                                                                                                                                                                        |
| `POST /v1/orgs/{org}/git-connections`                   | `{ "provider": "github", "installation_id" }`, or `gitlab` / `bitbucket` with `owner`, `token` (and `host` for GitLab) (admin).                                                                                                                       |
| `DELETE /v1/orgs/{org}/git-connections/{id}`            | Disconnect (admin).                                                                                                                                                                                                                                   |
| `GET /v1/orgs/{org}/secrets`                            | Secret names, never values.                                                                                                                                                                                                                           |
| `PUT /v1/orgs/{org}/secrets/{name}`                     | `{ "value" }` (admin).                                                                                                                                                                                                                                |
| `DELETE /v1/orgs/{org}/secrets/{name}`                  | Remove a secret (admin).                                                                                                                                                                                                                              |
| `GET /v1/gateway/models`                                | The models on our keys, with their prices.                                                                                                                                                                                                            |
| `GET`, `PUT`, `DELETE /v1/orgs/{org}/model-endpoint`    | The org's own model endpoint: `{ "api", "base_url", "model", "api_key" }` (admin). `api` is `openai` or `anthropic`; the key is never returned.                                                                                                       |
| `/v1/model/anthropic/v1/...`, `/v1/model/openai/v1/...` | The gateway itself: Anthropic Messages and OpenAI Chat Completions. Runners get a key that works only while they hold the run; people use their API token (`x-api-key` or a bearer token), billed to their personal org or the org in `x-chanto-org`. |

## Billing

| Route                                  |                                                                                                   |
| -------------------------------------- | ------------------------------------------------------------------------------------------------- |
| `POST /v1/orgs/{org}/billing/checkout` | `{ "plan" }` → a Stripe Checkout link to add a card and subscribe (admin); `free` for usage only. |
| `POST /v1/orgs/{org}/billing/plan`     | `{ "plan" }`: switch plans (admin).                                                               |
| `POST /v1/orgs/{org}/billing/portal`   | A Stripe billing portal link for cards and invoices (admin).                                      |

## Webhooks and health

| Route                     |                                                                                                                       |
| ------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| `POST /v1/github/webhook` | The GitHub App's events: comments, labels, issues and pull requests that start runs and agents.                       |
| `POST /mcp`               | The MCP server (streamable HTTP, JSON replies): tools for orgs, agents and runs, with an API token as a bearer token. |
| `POST /v1/stripe/webhook` | Stripe's subscription events.                                                                                         |
| `GET /healthz`            | 200 while the process is up.                                                                                          |
| `GET /readyz`             | 200 once the database answers (Chanto cloud only).                                                                    |
