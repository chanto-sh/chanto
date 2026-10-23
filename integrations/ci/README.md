# Starting Chanto cloud agents from CI

> [!WARNING]
> **Not ready yet.** These CI templates are a work in progress: they aren't documented or
> supported, and they may change or break without notice. For now, use Chanto
> from the terminal.

Each file here starts an [agent](../../docs/reference.md#chanto-cloud) in Chanto cloud and waits for it:

```sh
curl -fsSL https://chanto.sh/install.sh | sh
chanto cloud run --agent reviewer --input "Merge request !42" --wait
```

The agent does its work on a Chanto cloud runner (yours or a hosted one), not on the CI machine, so the CI job only needs a few seconds of setup and then waits. `--wait` prints the run's progress and its summary and pull request, and exits non-zero unless the run succeeded. Leave it out to start the agent and move on.

Set these in the CI system's secret settings:

| Variable | |
|---|---|
| `CHANTO_CLOUD_TOKEN` | An API token: `chanto cloud login` saves one, or create one in the dashboard |
| `CHANTO_CLOUD_ORG` | The org the agent is in (optional: the token's personal org otherwise) |

| File | CI system | Starts the agent |
|---|---|---|
| `github-actions.yml` | GitHub Actions | On pull requests, with the `chanto-sh/chanto/action` step |
| `gitlab-ci.yml` | GitLab CI/CD | On merge request pipelines |
| `bitbucket-pipelines.yml` | Bitbucket Pipelines | On pull requests |
| `azure-pipelines.yml` | Azure Pipelines | On pull requests |
| `Jenkinsfile` | Jenkins | On a nightly schedule |
| `circleci-config.yml` | CircleCI | On a nightly schedule |

On GitHub you can also skip CI entirely: an agent with triggers starts on its own when an issue or pull request event happens (see the reference).

Runs can take a while. If your CI system bills by the minute or has a short job timeout, start the agent without `--wait` and follow it in the dashboard or with `chanto agents runs <name>`.
