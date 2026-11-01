---
title: 'Cloud runs'
section: 'Cloud & automation'
order: 30
description: 'Queue runs and execute them on your own runners or hosted ones.'
---

> Chanto cloud isn't publicly available yet. This describes the client interface and how it will work; nothing else in Chanto needs it.

Chanto cloud is a shared run queue. You (or your team, CI, a script) queue a run in an org, and a runner takes it: one of your own machines, or a hosted runner. Every run is an ordinary Chanto background run on the runner, with its own worktree and branch, verified mode, budgets and sandbox. When it finishes, its branch is pushed to your repository.

To keep everything on a server of your own instead, use `chanto serve --runs` ([Remote runs](./reference.md#remote-runs-your-own-server)).

## Where runs go

Each run has a placement:

| Placement        | Runs on                                                                                                                                                                            |
| ---------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `self_hosted`    | Only your org's runners.                                                                                                                                                           |
| `hosted`         | Only hosted runners.                                                                                                                                                               |
| `auto` (default) | Your runners when one is online; otherwise, if your org can use hosted runners, a hosted runner. If your runners are all busy, a hosted runner takes the run after a grace period. |

Orgs can turn on **self-hosted only**, which refuses hosted runs and keeps `auto` runs on your runners. Use it when runs must stay on your machines (model calls still go to whichever provider the run uses).

Runs can also ask for **labels** (for example `gpu` or `linux`); only runners with every label take them.

## Your own runners

A runner is any machine with Chanto installed. It only makes outbound HTTPS requests, so it works behind a firewall, and it uses the machine's git credentials and Chanto configuration, so your code and model keys stay there.

1. In the dashboard, open your org's **Runners** page and create a registration token.
2. On the machine, run:

```sh
chanto runner connect https://<your chanto cloud> --registration-token hrr_…
```

The runner registers once and saves its own token in `~/.chanto/runner/connections.json`. Afterwards, `chanto runner connect https://<your chanto cloud>` is enough. Add `--label gpu` (repeatable) to advertise labels, `--name` to name the runner (default: the hostname), `--once` to exit after one job, and `--ephemeral` for throwaway VMs and containers: take one job, then deregister.

Nobody watches these runs, so permission prompts are denied with an explanation and questions are answered with "use your best judgment". Pick the runner's `[background] mode` (or `--mode`) with that in mind.

## Hosted runners

Hosted runners (early access) are ephemeral: each job gets a fresh VM that's gone when the run ends, so nothing carries over between jobs or customers. Hosted time is $0.005 a minute and counts toward your org's usage: paid plans include some every month, and past it orgs with a card go on pay-as-you-go up to their spending cap (see [pricing](https://chanto.sh/pricing) and the dashboard's **Usage & billing**).

To keep them from being misused:

- **They need a card on file**, even within the included amount. Without one, hosted runs are refused and `auto` runs wait for your own runners.
- **A hosted run stops after 2 hours**, or after 20 minutes without agent work (no events from the run and no model calls through Chanto cloud).
- **They need a model.** A hosted VM has no model setup of its own, so a hosted run needs the org to use our models or its own endpoint, or to have a key such as `ANTHROPIC_API_KEY` among its secrets.
- **They only reach the hosts coding work needs:** GitHub, GitLab and Bitbucket, package registries (crates.io, npm, PyPI, Go, RubyGems, Maven, Gradle), a few documentation sites, the main model providers, and Chanto cloud itself. Anything else, including a model endpoint or git server of your own, is blocked unless we add it for your org: contact us. On your own runners you choose what the network allows.

## Models

Each org picks whose model keys its runs use, under **Models**:

- **Your own** (the default): each runner uses its machine's model setup, so keys never reach Chanto cloud. Hosted runners can take a key from the org's secrets, e.g. `ANTHROPIC_API_KEY`.
- **Ours, at provider cost**: open-weight models through Chanto cloud's gateway, counted toward usage at the provider's price.
- **Your endpoint**: a model you serve or pay for (OpenAI-compatible or Anthropic Messages, over HTTPS), called directly by runs and not metered.

Start a runner with `chanto runner connect … --own-model` to keep its machine's model setup whatever the org chooses.

## Git and pull requests

Under **Git**, connect the GitHub app (install it from there and GitHub brings you back connected), a GitLab group or project access token, or a Bitbucket workspace or repository access token. Runs then get credentials for their repository only, and when a run succeeds with a pushed branch, Chanto cloud opens a pull request (a merge request on GitLab) and links it on the run.

With the GitHub app installed, mention it in an issue or pull request comment (`@chanto-cloud fix the flaky test`), or add the `chanto-cloud` label to an issue, to start a run. Only the repository's owner, organization members and collaborators can. The issue gets a link to the run, then the pull request or why the run failed.

## Agents

An agent is a standing job in an org: a name, instructions and a repository, plus the same placement and labels as a run. Each time it starts, it queues an ordinary run whose task is its instructions followed by what started it. Set them up under **Agents**; the dashboard has starting points for triaging new issues, weekly dependency updates and reviewing new pull requests.

An agent starts:

- **By hand**: **Run now** on its page, optionally with something to add to the instructions ("look at issue #42") and a different branch.
- **On a schedule**: a five-field cron expression in UTC, such as `0 9 * * 1-5` for weekdays at 09:00, at most every 10 minutes. A scheduled start is skipped while the agent's previous run is still going.
- **On GitHub events** in its repository: a new issue, a label added to an issue (one label, or any), a new pull request (or one marked ready for review), new commits on a pull request, or a mention. Events need the GitHub app connected under **Git** and only count from people with write access to the repository; pull requests from forks don't start runs. Pull request runs work on the pull request's branch.

To start an agent from a comment, mention the app and the agent by name on an issue or pull request: `@chanto-cloud triage the stack trace is from the Linux build`. Everything after the name is added to the instructions.

Admins create, change, disable and delete agents; any member can run one by hand. A disabled agent keeps its settings and only starts by hand. When a scheduled or GitHub start doesn't happen (a spending limit, a missing connection), the agent's page says why. Deleting an agent keeps its runs.

## From your terminal, CI and AI tools

Sign in once with `chanto cloud login` (it opens the browser). Then:

- **Agents:** `chanto agents list`, `create`, `edit`, `run <name> --wait` and `runs <name>`; `--org` picks a team org. See the [reference](./reference.md#chanto-cloud).
- **Models:** Chanto cloud is also a model provider. `chanto -m chanto/<model>` (or pick it in `/model`) uses the same models as runs on our keys, counted toward your personal org's usage, or the org you choose with `chanto cloud org <slug>`.
- **CI:** `chanto cloud run --agent <name> --wait` with `CHANTO_CLOUD_TOKEN` set starts an agent and fails the job unless the run succeeds. It works in any CI system that can run a shell script; the [reference](./reference.md#chanto-cloud) has GitHub Actions and GitLab CI examples.
- **AI tools (MCP):** Chanto cloud is an MCP server at `<api>/mcp`. Claude Code, Cursor and other MCP clients can list, create and run agents and follow runs from a chat:

  ```sh
  claude mcp add --transport http chanto-cloud https://api.chanto.sh/mcp \
    --header "Authorization: Bearer $CHANTO_CLOUD_TOKEN"
  ```

  The tools act on your personal org unless they're given another, with the same permissions as the dashboard.

## Secrets

Under **Secrets**, set environment variables for every run of the org, including its shell commands: API keys, test credentials. Values are encrypted, never shown again, and masked in transcripts.

## Following a run

The dashboard shows each run's live event stream: messages, tool calls, warnings and the final summary. You can cancel a run at any time; a running run stops at its runner's next heartbeat.

A runner keeps its run with a heartbeat. If it disappears (crash, lost network, Ctrl-C), the run goes back to the queue once its lease runs out, and fails after the maximum number of attempts.

## Orgs and plans

You sign in with GitHub, Google, GitLab or Bitbucket. Every account has a personal org. Team orgs have owners, admins and members: members queue and follow runs, admins manage runners, members and settings, and owners can also manage other owners. Add a member by their login once they've signed in; if two people on different providers share a login, say which, e.g. `gitlab:alice`.

Plans aren't priced per seat: they differ in limits and in how much usage they include each month. See [pricing](https://chanto.sh/pricing) for what each plan includes. Free includes no usage, but your personal org gets a one-time $2 trial credit, kept until it's spent, for models on our keys (or hosted time, once you add a card). It arrives when you sign in with a GitHub, GitLab or Bitbucket account at least 30 days old (a newer one gets it at a later sign-in), or, with Google, which doesn't say how old an account is, when you add a card. Each card backs one person's trial, and only a few trials start from one network address a day.
