<p align="center">
  <img src="assets/logo.svg" alt="Chanto" width="96" height="96" />
</p>

<h1 align="center">Chanto</h1>

<p align="center">
  A fast, lean coding agent for your terminal.
  Local and cloud models, including:
  Amazon Bedrock, Google Vertex AI and Azure.
</p>

<p align="center">
  <img alt="Status: alpha" src="https://img.shields.io/badge/status-alpha-f59e0b">
  <a href="https://scorecard.dev/viewer/?uri=github.com/chanto-sh/chanto"><img alt="OpenSSF Scorecard" src="https://api.scorecard.dev/projects/github.com/chanto-sh/chanto/badge"></a>
  <a href="https://www.bestpractices.dev/projects/15833"><img alt="OpenSSF Best Practices" src="https://www.bestpractices.dev/projects/15833/badge"></a>
  <img alt="License" src="https://img.shields.io/badge/license-Apache--2.0-blue">
  <img alt="Platforms" src="https://img.shields.io/badge/platforms-macOS%20·%20Linux%20·%20Windows-0078D6">
</p>

> [!NOTE]
> **Chanto is in alpha:** expect rough edges and breaking changes before 1.0.
> CI tests Linux, macOS and Windows. Read the [security model](docs/security-model.md)
> for the protections and their limits. [Issues](https://github.com/chanto-sh/chanto/issues)
> and feedback are welcome.

<p align="center">
  <a href="docs/reference.md"><b>Docs</b></a> ·
  <a href="https://chanto.sh/docs/quickstart"><b>Quickstart</b></a> ·
  <a href="https://github.com/chanto-sh/benchmarks/blob/main/RESULTS.md"><b>Benchmarks</b></a> ·
  <a href="CHANGELOG.md"><b>Changelog</b></a>
</p>

---

## Install

```sh
curl -fsSL https://chanto.sh/install.sh | sh      # macOS and Linux
brew install chanto-sh/tap/chanto                 # Homebrew
npm i -g @chanto/cli                              # npm
cargo install chanto --locked                     # from source (Rust 1.95+)
```

On Windows (PowerShell): `irm https://chanto.sh/install.ps1 | iex`. The installers check the release signature and the download's checksum and put `chanto` in `~/.local/bin`; `chanto upgrade` updates it.

## First run

```sh
cd your-project
chanto
```

Chanto detects API keys, compatible configuration and local model servers, then asks you to pick a model. Use an API provider, your ChatGPT plan, or a local model through Ollama or an OpenAI-compatible server.

Run `chanto login chatgpt` for your ChatGPT plan. Subscription coding plans such as Kimi Code (`chanto login kimi-coding`) and Z.ai (`chanto login zai`) use their own plan keys. Login saves credentials; `/model` or `/models` selects the model. See [providers and login](docs/reference.md#hosted-models).

Then just say what you want, like "fix the failing test in auth". Chanto asks before editing files or running risky commands, unless you allow them.

`chanto setup` changes the default model later, and `chanto doctor` shows what was found and where each setting came from.

## Everyday use

```sh
chanto                               # a session in this project
chanto "explain the auth flow"       # start with a prompt
chanto -p "fix the failing tests"    # run once and print the result (scripts, CI)
chanto --verify "add a login page"   # can't finish until the project's tests pass
chanto -c                            # continue the last session here
chanto --worktree                    # work in a new checkout and branch, for parallel sessions
chanto --inline                      # print into your terminal's scrollback instead of full screen
```

In a session:

| | |
|---|---|
| `esc` | stop the model |
| `enter` while it works | the model gets your message at its next step (`ctrl+x ctrl+s` sends now, `ctrl+x enter` waits for the turn to end) |
| `esc esc` or `/rewind` | go back to an earlier prompt (undoing file changes too), or edit one to start a new branch; `/redo` puts back what you undid |
| `/model`, `/effort` | switch model, or set how hard it thinks |
| `shift+tab` | cycle permission mode (ask, accept edits, plan, auto) |
| `@file`, `!cmd` | attach a file; run a shell command yourself |
| `?` or `/help` | all keys and commands |

`chanto --help` lists the command-line options and subcommands. The [reference](docs/reference.md) covers the rest.

---

## Features

| | |
|---|---|
| **Local and cloud models** | OpenAI, Anthropic, Gemini, DeepSeek, OpenRouter and more; ChatGPT and coding plans; Bedrock, Vertex AI and Azure; local OpenAI-compatible servers |
| **Permission controls** | Ask before risky actions, or use plan, accept-edits and auto modes. Shell sandboxing covers macOS and Linux; Windows restricts writes, with read/network limits described in the [security model](docs/security-model.md) |
| **Verification** | `--verify` adds a project-check gate before completion; `--verify-full` runs the full suite. The test-weakening guard also runs on ordinary turns ([reference](docs/reference.md#verified-mode-guardrails)) |
| **Sessions and undo** | Resume, fork and rewind conversations with file changes; share redacted transcripts |
| **Background work** | Run tasks in separate worktrees, monitor jobs, and apply their results while continuing your session |
| **Goals and teams** | Keep working toward agreed criteria or split a task across agents with checks and limits |
| **Extensions** | MCP tools, hooks, skills, custom commands and compatible plugin marketplaces |
| **Usage and costs** | Session token/cost totals, budgets, and provider-reported limits with low-quota warnings |

The [reference guide](docs/reference.md) also covers memory, scheduled jobs,
voice, remote runs, optional computer control and using another agent through ACP.

<details>
<summary><b>Compatibility: the existing Claude Code and Codex setup it reads</b></summary>

| Reads | From |
|---|---|
| Settings, permission rules, hooks, env, status line, output style, API key helper | `~/.claude/settings.json`, `.claude/settings*.json` |
| MCP servers | `.mcp.json`, `~/.claude.json`, Codex `config.toml` |
| Instructions | `AGENTS.md`, `AGENTS.override.md`, `CLAUDE.md`, `.claude/rules` |
| Skills, commands, subagents, plugins | `.claude/` and `~/.claude/`, Codex `prompts/` |

Project settings that could run code are ignored until you trust the folder.
</details>

---

## Benchmarks

Two things are measured, each with the same rules for every agent:

- **Overhead:** the CPU time and memory an agent spends per turn on its own work, with the model taken out (a local mock answers instantly), every agent at a pinned version.
- **Task results:** pass rates on coding tasks from public sets (SWE-bench, HumanEval, DS-1000 and others) with the same model for every agent.

Results, versions and caveats are in [RESULTS.md](https://github.com/chanto-sh/benchmarks/blob/main/RESULTS.md), with every raw result behind them. To check the overhead numbers on your own machine:

```sh
git clone https://github.com/chanto-sh/benchmarks.git && cd benchmarks
python3 reproduce.py --quick    # smaller selection; omit --quick for the full comparison
```

---

## Development

`src/` contains the agent, `proto/` shared wire types, and `docs/` the reference. Building requires Rust 1.95 or newer:

```sh
cargo build                 # add --release for target/release/chanto
cargo test
```

[AGENTS.md](AGENTS.md) has the day-to-day commands and architecture; [CONTRIBUTING.md](CONTRIBUTING.md),
[SECURITY.md](SECURITY.md) and [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) cover contributing, reporting a
vulnerability and behaviour; [GOVERNANCE.md](GOVERNANCE.md) and [ROADMAP.md](ROADMAP.md) cover how the
project is run and where it's going.

---

## License

Licensed under the Apache License, Version 2.0 ([LICENSE](LICENSE) or
<http://www.apache.org/licenses/LICENSE-2.0>). Third-party notices are in
[NOTICE](NOTICE).

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
licensed as above, without any additional terms or conditions.

---

<p align="center"><sub>Chanto is not affiliated with, endorsed by, or sponsored by Anthropic, OpenAI, or any other vendor it can talk to. "Claude", "Claude Code", "Anthropic", "OpenAI", "ChatGPT", and "Codex" are trademarks of their respective owners, named here only to describe compatibility with their configuration and APIs.</sub></p>
