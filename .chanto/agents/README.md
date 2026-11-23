# Quality and work agents for this repository

Standing jobs in Chanto cloud, defined as agent files so they are reviewable and
re-appliable:

| file | starts | what it does |
| --- | --- | --- |
| `review.toml` | a pull request is opened | reports bugs it can point at a line for; does not push |
| `integration-gaps.toml` | Mondays 06:00 UTC | finds one untested integration path and opens the test that covers it |
| `security.toml` | daily 03:00 UTC | dependency advisory, committed secret or unsafe parsing; opens the smallest fix |
| `work.toml` | `@chanto work …` in a comment | does the task the comment asks for and leaves it committed on a branch |

All four use `placement = "self-hosted"` and `model_keys = "own"`: runs execute
on your own runner, so the checkout, the filesystem and the model key never
reach the cloud. The cloud queues runs, stores their events and shows them in the
dashboard; the only traffic off the runner is the prompt to the model provider.

The three quality agents also ask for `labels = ["gha"]`, so only the GitHub
Actions runner takes them. `work.toml` has no labels: whichever runner is online
takes it, a laptop or a GitHub Actions VM.

`.github/workflows/quality-runner.yml` offers one for free: this repository is
public, so GitHub Actions minutes cost nothing, and those VMs can reach OpenCode
Go, which the cloud's hosted runners cannot. A job holds the runner for 24
minutes and the next starts when it ends, so the queue drains continuously.

## Where runs execute

A run is taken by a runner that carries every label the run asks for, so the
labels choose the machine. Three kinds of runner work here:

| runner | labels | cost | notes |
| --- | --- | --- | --- |
| a GitHub Actions VM (`quality-runner.yml`) | `gha` | free, this repository is public | holds runs for 24 minutes at a time; the next job starts when one ends |
| your own machine, as a service (`scripts/runner-service.sh`) | `local` | free | keeps taking runs after the terminal closes and the machine reboots; a laptop can sleep and the run waits |
| a hosted cloud VM (`placement = "hosted"`) | — | billed | the code leaves your machine, and the hosted runners cannot reach OpenCode Go |

The files here stay on the self-hosted path. A hosted run also needs a card on
file and a model key for a provider the hosted runners reach (Anthropic, OpenAI,
Together, Fireworks, OpenRouter), which is why the agents ask for their own key.

## Working from a phone

Two ways in, both of them fine from a phone browser or the GitHub app:

- **The dashboard's runs page**, `https://chanto.sh/app/<org>/runs`: a repo, a
  task and, optionally, labels. A run labelled `local` goes to your laptop, one
  labelled `gha` to the next GitHub Actions window; with no labels whichever
  runner is online takes it. This path needs no GitHub App: a public repository
  clones, and the run's branch is pushed with the org's git connection when
  there is one.
- **A mention on an issue or pull request**, `@chanto work …`:
  `work.toml` does what the comment asks and leaves it committed. The comment
  needs write access, so only maintainers can spend a run.

Whichever runner is online claims the run (the laptop, the GitHub Actions
window, or both racing), the branch is pushed when it ends, and the dashboard or
`chanto agents runs work` shows what happened. With nothing online the run waits
in the queue instead of failing, so it starts when a machine comes back.

Mentions only reach the agent once this repository is connected in the dashboard
at `https://chanto.sh/app/<org>/git`, where the GitHub app is installed on the
repository's owner. That is the same connection `review` needs, and it is what
lets any run push its branch.

## One-time setup

```sh
chanto cloud login                        # account and org
scripts/cloud-demo.sh                     # create or update the four agents
```

The org also connects this repository at `https://chanto.sh/app/<org>/git` (the
GitHub app): that is what lets a run clone it and push its branch, and what
queues `review` when a pull request opens.

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
scripts/cloud-demo.sh --run work --input "add a --dry-run flag to scripts/cloud-demo.sh"
chanto agents runs integration-gaps      # what it did, newest first
```

`review` also starts by itself when a pull request opens; `@chanto review` in a
comment starts it on any issue or pull request of this repository. A queued run
starts within about a quarter of an hour, when a window takes it.

To take runs on your own machine instead, install the runner as a service, which
keeps taking them after the terminal closes:

```sh
scripts/runner-service.sh install --label local --own-model
```

That is the `local` label: it takes runs that ask for `local`, and `work` runs,
which ask for nothing, while the `gha`-labelled quality agents stay with the
GitHub Actions windows.

## Editing

The file name is the agent name. Change the file, run
`scripts/cloud-demo.sh --dry-run` to check it, then `scripts/cloud-demo.sh` to
apply it: an existing agent is updated, a new one created. The `checks` list is
what "done" means — a run that cannot pass them opens no pull request.
