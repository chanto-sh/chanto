---
title: 'Quickstart'
section: Getting started
order: 2
description: 'Install Chanto and make your first verified change.'
---

## Install

```sh
curl -fsSL https://chanto.sh/install.sh | sh
```

On Windows, run this in PowerShell:

```powershell
irm https://chanto.sh/install.ps1 | iex
```

The installer checks the download's checksum and puts `chanto` in `~/.local/bin` (on Windows, `%LOCALAPPDATA%\chanto\bin`), and tells you if that isn't on your `PATH`. Update later with `chanto upgrade`. The [download page](https://chanto.sh/download) has the release files, and [Install](./reference.md#install) covers the options.

## First run

Run `chanto` in a project. It finds your API keys, a running Ollama, or your Codex setup, and walks you through login if it finds nothing.

```sh
chanto                               # start in the current project
chanto -p "fix the failing tests"    # one-off task
chanto --verify "add a login page"   # only finishes when the tests pass
chanto -c                            # continue the last session here
```

No API key? The model picker's _Free_ section offers OpenRouter's free models, Gemini's free tier, or a model on your own machine. The hosted free options may log or train on your prompts, so don't use them on code you can't share.

`chanto setup` changes the default model, and `chanto doctor` shows what was detected and where each setting came from.

## Permission modes

Cycle modes with `shift+tab`: `default` → `acceptEdits` → `plan` → `auto`, plus `bypassPermissions`.

- **default** asks before anything that isn't read-only.
- **acceptEdits** applies file edits without asking.
- **plan** reads and plans without changing anything.
- **auto** runs on its own: read-only and in-project actions run immediately, known-dangerous commands are blocked, and everything else is screened by a safety classifier that never sees tool output.

## Verified runs

`chanto --verify` (or `[guardrails] enabled = true`) runs the project's checks when the model says it's done. They're detected automatically (`cargo test`, `go test`, `npm test`, `pytest`, `make test`, …) or set with `checks = [...]`. Failures go back to the model, up to `max_attempts` (default 3). See [Verified mode](./reference.md#verified-mode-guardrails).

## Background runs

```sh
chanto run --bg "add retries to the HTTP client"
chanto runs                 # list them
chanto attach <id>          # follow one and answer its prompts
chanto runs apply <id>      # bring the result into your checkout (or --pr)
```

Each background run works in its own git worktree and branch while you keep going. See [Background and remote runs](./reference.md#background-and-remote-runs).

## Configuration

Settings live in `~/.chanto/config.toml` (global), `./.chanto/config.toml` (project) and `./.chanto/config.local.toml` (personal). `chanto --example-config` prints a commented template of the common options.

```toml
provider = "ollama"
model = "qwen3-coder:30b"
mode = "default"

[permissions]
allow = ["Bash(cargo test:*)"]
deny  = ["Bash(git push:*)"]
```
