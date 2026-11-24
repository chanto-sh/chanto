---
title: 'Introduction'
section: Getting started
order: 0
description: 'What Chanto is and where to start.'
---

Chanto is a coding agent for your terminal: one Rust binary, local or hosted models.

**Local:** Ollama, llama.cpp, vLLM, any OpenAI-compatible server. **Hosted:** Anthropic, OpenAI, your ChatGPT login, Amazon Bedrock, Google Vertex AI, Azure.

In our [benchmark](https://github.com/chanto-sh/benchmarks#overhead-latest-results) of the 23 coding agents with 5,000+ GitHub stars, it used the least startup time, CPU per turn and memory ([reproduce it](https://github.com/chanto-sh/benchmarks#reproduce-the-overhead-numbers)).

## What makes it different

- **Fast.** 1.8–2.8 ms of CPU per turn, 11–14 ms to start, 17–23 MB of peak memory (0.1.21, macOS and Linux). [Numbers](https://github.com/chanto-sh/benchmarks#overhead-latest-results) · [Reproduce](https://github.com/chanto-sh/benchmarks#reproduce-the-overhead-numbers)
- **Works with your setup.** Reads Claude Code and Codex settings, permission rules, hooks, MCP servers, instruction files, skills and subagents. Nothing to migrate.
- **Checks before it finishes.** `--verify` runs the tests related to the change, or the full suite when shared setup changed. It separates failures it caused from ones that were already there, and won't weaken a test to pass.
- **Drives another agent.** `chanto acp-client` runs Claude Code, OpenCode or any ACP v1 agent under Chanto's UI and permissions.
- **Safe to leave alone.** Shell commands run in an OS sandbox; risky actions ask first; API keys and `.env` values stay out of the model and its logs; the spend guard warns before a costly fan-out.
- **Runs where you want.** In the terminal, in a background git worktree, or on your own server.

## Where to go next

- [Quickstart](./quickstart.md): install, first run and the basics.
- [Models](./models.md): local models, hosted APIs, your ChatGPT plan and cloud providers.
- [Reference](./reference.md): every feature in detail.

Chanto is open source under Apache-2.0. Releases are on the [download page](https://chanto.sh/download); the [changelog](https://chanto.sh/changelog) lists each release.
