---
title: 'Introduction'
section: Getting started
order: 0
description: 'What Chanto is and where to start.'
---

Chanto is a fast, lean coding agent for your terminal. It's a single small Rust binary that starts instantly and has the lowest startup, CPU per turn and memory of the 23 coding agents with 5,000+ GitHub stars we benchmark, and it runs against local models (Ollama, llama.cpp, vLLM and other OpenAI-compatible servers) or hosted ones (Anthropic, OpenAI, your ChatGPT login, Amazon Bedrock, Google Vertex AI, Azure and more).

## What makes it different

- **It's fast and light.** Chanto 0.1.10 used 1.5–3.1 ms of CPU per turn, 13–23 ms to start, and 17–24 MB of peak memory in our [overhead benchmark](https://github.com/chanto-sh/benchmarks#overhead-latest-results) on GitHub’s macOS and Linux runners ([reproduce it](https://github.com/chanto-sh/benchmarks#reproduce-the-overhead-numbers)), so it stays responsive in long sessions and leaves your machine to the model and your tools.
- **It works with your setup.** It reads your Claude Code and Codex settings, permission rules, hooks, MCP servers, instruction files, skills and subagents, so there's nothing to migrate.
- **It checks before it finishes.** With `--verify`, Chanto runs the project's checks before it calls a task done. It runs the tests related to the change, or the full suite when shared setup changed, tells failures it caused from ones that were already there, and won't weaken tests to make them pass.
- **It can drive another agent.** `chanto acp-client` runs Claude Code, OpenCode or any other ACP v1 agent under Chanto's own UI and permissions.
- **It's safe to leave alone.** Shell commands run in an OS sandbox, risky actions need your approval, API keys and `.env` values are hidden from the model and its logs, and the spend guard warns before work that multiplies cost.
- **It runs where you want.** In your terminal, in the background in its own git worktree, or on a server you control.

## Where to go next

- [Quickstart](./quickstart.md): install, first run and the basics in five minutes.
- [Models](./models.md): local models, hosted APIs, your ChatGPT plan and cloud providers.
- [Reference](./reference.md): every feature in detail, generated from the CLI's own documentation.

Chanto is open source under Apache-2.0. Releases are on the [download page](https://chanto.sh/download), and the [changelog](https://chanto.sh/changelog) lists what changed in each.
