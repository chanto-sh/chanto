# Chanto GitHub Action

Run [Chanto](https://github.com/chanto-sh/chanto) on GitHub Actions:

- **Quality checks:** check every pull request against your repository's quality profile: its rules, a secrets scan and its tests and linters. Findings show up on the changed lines. No model or API key is needed.
- **Pull request review:** a read-only review posted as a summary plus inline comments.
- **`@chanto` commands:** maintainers mention `@chanto` in an issue or pull request to get an answer, a review or a fix pushed as commits.
- **Prompts:** run a task on a schedule or by hand (`workflow_dispatch`).

`chanto github install` writes the workflows below into your repository.

## Quality checks

1. Run `chanto prime` locally and commit `.chanto/quality.toml`. It records your project's checks and holds your rules ([quality profile](https://github.com/chanto-sh/chanto/blob/main/docs/reference.md#quality-profile)).
2. Add `.github/workflows/chanto-quality.yml`:

```yaml
name: Chanto quality
on:
  pull_request:
  push:
    branches: [main]
  merge_group:
permissions:
  contents: read
jobs:
  quality:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
        with:
          fetch-depth: 0
          persist-credentials: false
      - uses: chanto-sh/chanto-action@v1
        with:
          quality: "true"
```

What gets checked depends on the event:
- **Pull request:** the lines its commits add.
- **Push:** the pushed commits.
- **Merge queue:** the group's changes.
- **Manual or scheduled run:** every file.

A check covers the profile's regex rules, the secrets scan and, unless `quality_checks: "false"`, the profile's `when = "turn"` checks. A failure fails the job. Each finding is annotated on its line, with secrets masked, and the report goes to the job summary.

Because nothing secret is involved, this also checks pull requests from forks. It refuses `pull_request_target` and comment events, which would run a pull request's checks with your secrets.

## Pull request review and `@chanto` commands

These run a model, so they need a key: add `ANTHROPIC_API_KEY` (or `OPENAI_API_KEY`, or `[providers.*]` TOML as `CHANTO_PROVIDERS`) as a repository secret. `chanto github install` writes both workflows and a review policy, `.chanto/review.toml` (strictness, inline comment cap, ignored paths, checks).

```yaml
- uses: chanto-sh/chanto-action@v1
  with:
    anthropic_api_key: ${{ secrets.ANTHROPIC_API_KEY }}
    # prompt: Focus on security and error handling.
```

**Security:**
- Comments, titles, bodies and diffs are passed to the model as untrusted input. Only users with write access can trigger commands, and that access is checked live.
- Pull requests from forks are not reviewed, because GitHub gives them no secrets.
- The agent has no web access unless `allow_network: "true"`.
- Executable project settings are ignored.

## Inputs

| Input | Default | What it does |
| --- | --- | --- |
| `quality` | `false` | Run `chanto quality check` instead of the agent |
| `quality_checks` | `true` | In quality mode, also run the profile's checks |
| `prompt` | | Task for `workflow_dispatch`/`schedule`, or extra review instructions |
| `trigger_phrase` | `@chanto` | Phrase that triggers commands in comments |
| `model` | `anthropic/claude-sonnet-5-5` | `provider/model`, or a model served at `base_url` |
| `anthropic_api_key`, `openai_api_key` | | Keys for `anthropic/*` and `openai/*` models (`openai_api_key` is also the key for `base_url`) |
| `base_url` | | A self-hosted OpenAI-compatible endpoint |
| `provider_config` | | `[providers.*]` TOML, passed from a secret |
| `mode` | `acceptEdits` | Permission mode for commands |
| `allowed_tools` | | Comma-separated tool allowlist |
| `max_turns` | | Maximum model calls per run |
| `timeout_minutes` | `30` | Stop Chanto after this long |
| `allow_network` | `false` | Let the agent use WebFetch and WebSearch |
| `dry_run` | `false` | Print the review as JSON instead of posting it |
| `version` | `latest` | Chanto release to install, e.g. `v0.1.16` |
| `github_token` | `github.token` | Token used to comment, push and open pull requests |
| `cloud_agent`, `cloud_input`, `cloud_token`, `cloud_org`, `cloud_url` | | Run a Chanto cloud agent instead, and wait for its result |

## Versions

- **`v1`** follows the latest `v1.x.y` release.
- **`v1.0.0`** stays fixed.
- **Pinning:** to pin exactly, use a full commit SHA.

The action installs the Chanto release named by `version` (default `latest`). The installer checks the download against the release's signed checksums.

This repository is published from [`action/`](https://github.com/chanto-sh/chanto/tree/main/action) in the Chanto repository, which is where to send changes and issues. Its tests run there: `python3 -m unittest discover -s tests`.
