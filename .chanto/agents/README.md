# Quality agents for this repository

Three standing jobs in Chanto cloud, defined as agent files so they are
reviewable and re-appliable:

| file | starts | what it does |
| --- | --- | --- |
| `review.toml` | a pull request is opened | reports bugs it can point at a line for; does not push |
| `integration-gaps.toml` | Mondays 06:00 UTC | finds one untested integration path and opens the test that covers it |
| `security.toml` | daily 03:00 UTC | dependency advisory, committed secret or unsafe parsing; opens the smallest fix |

All three use `placement = "self-hosted"`, `model_keys = "own"` and
`labels = ["gha"]`: runs execute on the runner they ask for, so the checkout,
the filesystem and the model key never reach the cloud. The cloud queues runs,
stores their events and shows them in the dashboard; the only traffic off the
runner is the prompt to the model provider.

`.github/workflows/quality-runner.yml` offers one for free: this repository is
public, so GitHub Actions minutes cost nothing, and those VMs can reach OpenCode
Go, which the cloud's hosted runners cannot. A job holds the runner for 24
minutes and the next starts when it ends, so the queue drains continuously.

## One-time setup

```sh
chanto cloud login                        # account and org
scripts/cloud-demo.sh                     # create or update the three agents
```

The org also connects this repository in the dashboard (the GitHub App): that is
what lets a run clone it and push its branch, and what queues `review` when a
pull request opens.

Runs go to a runner labelled `gha`, which the workflow offers. Register that
once and give the workflow its saved sign-in:

```sh
chanto runner connect https://api.chanto.sh \
  --registration-token hrr_...            # from the dashboard's Runners page
  --label gha --name gha                  # ctrl-c once it's waiting
gh secret set CHANTO_RUNNER_CONNECTIONS \
  < ~/.chanto/runner/connections.json     # the token it just saved
```

The workflow thinks with OpenCode Go, so the repository's `OPENCODE_GO_API_KEY`
secret holds a Console key from that subscription (the provider reads it as
`OPENCODE_API_KEY`).

The workflow reads `CHANTO_MODEL` from a repository variable, defaulting to
`opencode-go/deepseek-v4.1-flash`. `--mode bypassPermissions` is the runner's
default here because nobody answers a prompt on a throwaway VM; the model key is
still withheld from anything the agent's shell runs.

The scheduled workflow only runs from the default branch, so merge the workflow
before the first window (or dispatch it once it's there).

## Demo a run

```sh
scripts/cloud-demo.sh --run integration-gaps --wait
scripts/cloud-demo.sh --run review --input "review pull request #299"
chanto agents runs integration-gaps      # what it did, newest first
```

`review` also starts by itself when a pull request opens; `@chanto review` in a
comment starts it on any issue or pull request of this repository. A queued run
starts within about a quarter of an hour, when a window takes it.

To take runs on your own machine instead, `scripts/cloud-demo.sh --runner` (it
registers with the same `gha` label).

## Editing

The file name is the agent name. Change the file, run
`scripts/cloud-demo.sh --dry-run` to check it, then `scripts/cloud-demo.sh` to
apply it: an existing agent is updated, a new one created. The `checks` list is
what "done" means — a run that cannot pass them opens no pull request.
