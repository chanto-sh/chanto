# Chanto reference

Full reference for every feature. The [README](../README.md) is the overview.

**Contents:** [Setup](#setup) · [Install](#install) · [Usage](#usage) · [Configuration](#configuration) · [Outcome mode](#outcome-mode) · [Goals](#goals) · [Verified mode (guardrails)](#verified-mode-guardrails) · [Learning from your corrections](#learning-from-your-corrections) · [Usage insights](#usage-insights) · [Secrets](#secrets) · [Resource limits](#resource-limits) · [Computer use](#computer-use) · [Background agents and worktrees](#background-agents-and-worktrees) · [Background and remote runs](#background-and-remote-runs) · [Teams](#teams) · [Voice input](#voice-input) · [Server](#server) · [Claude Code and Codex compatibility](#claude-code-and-codex-compatibility) · [Benchmarks](#benchmarks) · [Architecture](#architecture) · [License](#license) · [ACP client](#acp-client) · [Managing spend](#managing-spend) · [Costs, budgets and telemetry](#costs-budgets-and-telemetry) · [Durable memory](#durable-memory) · [Permission modes](#permission-modes) · [Tools](#tools) · [Sandbox](#sandbox) · [Sessions and checkpoints](#sessions-and-checkpoints) · [Context management](#context-management)

A fast, local-first agentic coding CLI written in Rust, built around one rule: it has to prove its work before it calls the job done. It runs against **local models** through Ollama's native API or any OpenAI-compatible server (llama.cpp `llama-server`, LM Studio, vLLM, MLX, …), and against hosted models: the native **Anthropic Messages** and **OpenAI Responses** APIs, OpenAI-compatible services (OpenRouter, Gemini, DeepSeek, Groq and others; `chanto providers` lists them), and Amazon Bedrock, Google Vertex AI and Azure.

- **Single binary** (about 20 MB), starts instantly, uses little memory
- **Full-screen or [inline](#inline-mode) TUI**: streaming markdown, collapsible tool, diff and thinking blocks (click or `ctrl+o`), mouse scrolling, permission dialogs, slash and `@file` completion (gitignored files are offered last, marked, and `.git` never), todo panel, and a status bar with context usage and tokens/s
- **Reads the setup you already have**: existing Claude Code and Codex settings, permission rules, hooks, MCP servers, instruction files, skills, commands/prompts and subagents carry over, so switching doesn't mean starting from an empty config (see [Compatibility](#claude-code-and-codex-compatibility))
- **Multi-model**: named providers (`[providers.lmstudio]`, `[providers.llamacpp]`, …) referenced as `provider/model`, so the main agent, subagents (`subagent_model`) and the auto-mode classifier (`classifier_model`) can each use a different model or server. Switch live with `/model`.
- **Folder trust**: project settings that could run code or loosen safety (hooks, MCP servers, status line, permission allow rules, mode, endpoints, `classifier_model`, `optimize.token_classifier`) are ignored until you trust the folder (prompt on first run, or `chanto --trust`). "Don't ask again" rules are stored in `~/.chanto`, not in the repo, so a cloned project can't forge them.
- **Language servers**: after each edit, errors from the project's language server (rust-analyzer, typescript-language-server, pyright/basedpyright/pylsp, gopls, clangd: whichever is installed) are appended to the tool result so the model fixes them in the same turn. An `Lsp` tool (diagnostics, definition, references, hover, document/workspace symbols) is offered when a server is available for the project. See [Language servers](#language-servers).
- **Local-model hardening**: parses text-format tool calls (`<tool_call>` / bare JSON) when the model doesn't use native tool calls, repairs truncated JSON arguments, tolerates whitespace and indentation drift in `Edit`, retries transient server errors, and keeps a stable prompt prefix so the server's KV cache is reused

## Permission modes

Cycle with `shift+tab`: `default` → `acceptEdits` → `plan` → `auto`, plus `bypassPermissions`.

- **Auto mode** runs autonomously. Read-only and in-project actions run immediately, known-dangerous commands are hard-blocked, and anything else is screened by an LLM safety classifier. The classifier sees only what you typed and the proposed action, never tool output or the files you mention, so prompt injection can't talk it into approving. Only what you type into a frontend (the TUI, the REPL, the web UI or phone, an ACP client, `-p`) counts as your request. Every other prompt is shown to it as the agent's task, not as your request: a subagent's task, a goal, team or outcome step, a background run, a scheduled task, a slash command's expansion, a request from an MCP client. Approving an edit preserves auto mode; the approval does not remove protected-path or workspace checks. The classifier receives up to 16 recent user messages, so short follow-ups retain more of the original request. Explicit requests to publish, merge or deploy authorize necessary steps within that scope; later restrictions override earlier requests. A classifier refusal or failure becomes a prompt immediately in interactive sessions; known-dangerous command patterns fall back to you after 3 consecutive blocks. Prompts explain the applicable boundary, such as a sandbox escape, protected configuration, an outside-workspace edit, or uninspectable patch/archive paths. Optional, experimental: `permissions.fast_path = true` clears routine shell commands with a small built-in model in microseconds instead of a model call (it can only allow, never block, and never clears inline code such as `node -e` or `python -c`, packages fetched to run such as `npx github:…` or `go run …@latest`, or `curl -K`/`wget -e` option files; see [classifier/](../classifier/README.md) for how it's measured). Shell writes to protected paths (`.git/`, `.chanto/`, `.claude/` settings and hooks, `.mcp.json`, `.husky/`, `.vscode/` settings and tasks) ask in every mode but bypass, as edits of them do, and so does a write whose target the shell computes (`$var`, `$(…)`). In auto mode, extracting an archive or applying a patch asks too.
- **Always a human's call**, in every mode but `bypassPermissions`: reading secret files (`.env`, keys, `~/.ssh`, `~/.aws`), writing files outside the project or ones that make git or Chanto run commands (`.git/hooks`, hooks config), turning the command sandbox off, and fetching cloud instance metadata, this machine's own services (`localhost`, `127.0.0.1`, `::1`) or local files outside the project. An allow rule you write still covers its case, for example `WebFetch(domain:localhost)` for your dev server.

## Tools

`Read` (text, images, PDF, notebooks), `Write`, `Edit`, `MultiEdit` (a file that isn't UTF-8 is refused with its likely encoding, UTF-16 or Latin-1, and an `iconv` command to convert it; a `Write` or `Edit` whose arguments arrived as broken JSON, likely cut off, isn't run on a repaired guess: the model is asked to resend it in smaller pieces), `NotebookEdit`, `Glob`, `Grep` (ripgrep), `Ls`, `RepoMap` (a ranked, token-budgeted map of the project's symbols (functions, types, classes, modules), most-referenced first, for getting oriented in a large or unfamiliar tree before grepping; `path` limits it to a subdirectory, `tokens` sets the budget, default 1024), `SemanticSearch` (vector search over the codebase: files are split into overlapping chunks and embedded through the `[embeddings]` provider, the vectors are cached under `~/.chanto/cache/embeddings/` and refreshed when a file changes, and results are ranked by cosine similarity; `query` is required, `k` (default 8) and `path` are optional), `Bash` (persistent cwd inside the project and `--add-dir`/`additionalDirectories` directories: a command that ends outside them, like `cd /`, leaves the shell back at the project root and the output says `Shell cwd was reset to <root>`; `CLAUDE_BASH_MAINTAIN_PROJECT_WORKING_DIR=1` (or `CHANTO_BASH_MAINTAIN_PROJECT_WORKING_DIR`) starts every command at the project root, timeouts, background shells via `BashOutput`/`KillShell`; runs through bash on Unix, with your shell's PATH, variables, aliases and functions (see [Your shell setup in Bash](#your-shell-setup-in-bash)), and through PowerShell (then cmd) on Windows while the sandbox is on, which is the default, or with it off through Git Bash if present, then PowerShell, then cmd (override with `CHANTO_SHELL`, though Git Bash can't start inside the sandbox); files a command changed show as a diff), `Monitor` (runs a command in the background and hands each line it prints to the model as it arrives, so it reacts to a log, a file change or a CI status without polling; ends when the command exits or at `timeout_ms`, default 5 minutes, at most 30 (10 in `-p` runs, which wait for the watch to finish); it is a background shell, so `/bashes`, `BashOutput` and `KillShell` see it, and `Bash(...)` permission rules apply to its command), `CronCreate`/`CronList`/`CronDelete` and `ScheduleWakeup` (in-session scheduled prompts and the self-paced `/loop`; offered in the TUI and the `--plain` REPL only, and not at all with `CLAUDE_CODE_DISABLE_CRON=1`), `WebSearch`, `WebFetch`, `Browser` (Chrome over CDP: navigate, snapshot with element refs, click, type, keys, screenshots for vision models, tabs, JS eval), `Computer` (opt-in desktop control: screenshots, mouse, keyboard, apps; see [Computer use](#computer-use)), `TodoWrite`, `Task` (subagents with their own context), `AskUserQuestion`, `ExitPlanMode`, `Skill`, `Memory` (see [Durable memory](#durable-memory)), `Lsp` (see [Language servers](#language-servers)), `ToolSearch` (see [MCP servers](#mcp-servers)), `ChantoCloud` (cloud runs and agents, once you've signed in with `chanto cloud login`)

## Sandbox

The sandbox is on by default (macOS Seatbelt, Linux Landlock, and on Windows a write-restricted token that confines writes only, see below). Shell commands can write only inside the project, temp dirs and the package managers' download caches: `~/.cargo/registry` and `git`, `~/.npm` (`_cacache`, `_npx`), `~/.m2/repository`, `~/.gradle/caches`, `go/pkg`, `.bun/install/cache`, `.pnpm-store`, and named entries of `~/.cache` and `~/Library/Caches` (pip, uv, pnpm, Yarn, deno, node-gyp, go-build, Homebrew, Playwright, …), not the rest of those two. Dependency downloads are writable on purpose, like the project's own `node_modules`; toolchains, installed tools and settings are not.
- **What stays read-only inside those trees (macOS):** the project's `.git` (pinned, also when missing, so it can't be swapped or created) with its hooks, config and `commondir`, submodule and worktree git dirs (pinned too), nested repositories' `.git/hooks` and `.git/config`; agent and editor settings that carry hooks (`.chanto/`, `.claude/settings*.json` and `hooks/`, `.mcp.json`, `.husky/`, `.githooks/`, `.vscode/{settings,tasks}.json`, with `.claude` and `.vscode` pinned against rename); Chanto's own home and shell startup files; toolchain bins and settings (`~/.cargo/bin` and `config.toml`, `~/.rustup`, pyenv, uv's Pythons, `~/.local/bin`, Gradle wrappers, `init.d` and `gradle.properties`, Maven settings).
- **What needs an unsandboxed run (approval):** installing a toolchain or tool, a first dependency fetch on Linux when its cache directory doesn't exist yet, `git init` and `git clone` inside the project, `git worktree add`, `git submodule add`/`update`, `git remote add`, `git config` and `git branch -u` (they write `.git/config`), and git over SSH unless `sandbox.allow_ssh_agent = true`. The blocked-command hint says so.
- **Reads:** on macOS and Linux commands can't read SSH private keys, AWS/gcloud/GitHub/Docker/Kubernetes/npm/crates.io/1Password credentials, `.netrc`, git credentials, GPG private keys or Chanto's own token files (add more with `sandbox.deny_read`). A private npm registry or `docker pull` that needs those logins fails quietly inside the sandbox. On Linux, read denials cover files that exist when the command starts. On macOS the keychain is still reachable, so `git credential-osxkeychain`, `gh auth token` and Docker's credential helper can return tokens.
- **Local IPC (macOS):** commands start in a new session without a controlling terminal, can't read, write or `ioctl` Chanto's terminal or the other terminal windows' ttys (ptys they open themselves, as `script` does, work), can't send Apple Events, use LaunchServices (`open`, `osascript`), the clipboard or Shortcuts, and can connect to Unix sockets only inside the project and their own `$TMPDIR` (so not `docker.sock`, SSH or GPG agents, tmux, or other apps' sockets in the shared temp dirs). The SSH, GPG, D-Bus, X11/Wayland, Docker and `XDG_RUNTIME_DIR` environment variables are removed.
- **Linux status:** Landlock grants the project, temp dirs and `/dev` whole, because carving a path out of a tree takes away the right to create files next to it (`target/`, `.git/index.lock`). So the in-project guards above (`.git`, `.chanto`, `.claude`, …) are not enforced by the sandbox on Linux; the permission layer still asks before the file tools touch them, but a shell command can write them. The home guards apply where a configured `allow_write` or `--add-dir` covers them. Unix sockets aren't scoped: removing the environment variables is advisory, and a command that knows a socket's path (`/var/run/docker.sock`, `/run/user/<uid>/bus`) can connect to it. Seccomp refuses TIOCSTI and TIOCLINUX, and UDP/raw sockets when the network is restricted.
- **Network** stays allowed by default. Set `sandbox.network = "deny"` to block it, or `"allowlist"` to use the domain filtering proxy (`deny_network = true` remains a legacy alias; an explicit `network` wins); neither is enforced on Windows.
- **Windows status:** commands run under a restricted copy of your own token (`WRITE_RESTRICTED`) inside a job object. They run in PowerShell, because Git Bash can't start under this token. No administrator, driver, extra account or AppContainer is needed. What's enforced: writes are limited to the project, the temp dir, `allow_write` and `--add-dir` paths, the package managers' download caches (`.cargo\registry`, `.cargo\git`, `.gradle\caches`, `.m2\repository`, `.bun\install\cache`, `go\pkg` and `.nuget\packages` in your profile; the npm, pnpm, Yarn, pip, uv, Go build and Poetry caches under `%LOCALAPPDATA%`), plus the rare locations that grant write access to Everyone; and the job kills every descendant when the shell or the launcher exits (the launcher also watches the Chanto process). What isn't: none of the read-only guards above apply, and reads are not restricted at all (Windows only consults a write-restricted token's extra SIDs for writes), so `deny_read` and the built-in credential denials don't apply and commands can read anything you can. The network isn't restricted either: `network = "deny"` gets a startup warning and commands keep network access, and `network = "allowlist"` fails closed with an error. With `sandbox.auto_allow`, Chanto warns at startup that auto-allowed commands can read credentials and use the network.
- On Windows each writable root gets its own write SID (derived from its path) and an inheritable allow ACE for it. The ACE stays on the directory after the command: the first grant on a big tree waits while Windows propagates it to existing files, later commands only check it. The tool caches are granted in the background when Chanto starts, so commands don't wait on them. It grants nothing to ordinary processes, since that SID can only narrow a token's access. Files with protected ACLs, or moved in from elsewhere with their own ACL, stay read-only inside the sandbox. Grant paths must be on an ACL-capable filesystem, and setup failures stop the command rather than running it without isolation. WSL shells can't be contained and are refused. If the startup probe fails, Chanto warns and runs commands unsandboxed, as on any system without a sandbox.
- When the sandbox blocks a command, the model can retry it with `dangerouslyDisableSandbox: true`. That is never automatic and always needs a human: you're asked in default, acceptEdits and plan modes, auto mode asks too (the classifier does not get to hand the sandbox back), and only bypass mode allows it. An explicit `Bash(...)` allow rule you wrote also covers it.
- `/sandbox` shows the policy and `/sandbox on|off` toggles it for the session; `[sandbox] enabled = false` turns it off. `--sandbox read-only | workspace-write | danger-full-access` work as in Codex; `[sandbox] read_only = true` sets the read-only policy in config, whatever the permission mode.
- In allowlist mode, `allowed_domains` accepts exact names and `*.example.org` (subdomains only); `denied_domains` wins over allowed entries. Omit `allowed_domains` to use package registry defaults for GitHub, npm/Yarn, PyPI, crates.io, Go modules, Maven Central and RubyGems. An explicit `[]` permits nothing. IP literals, including loopback, require an exact entry.
- Commands receive `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` and their lowercase equivalents, with `NO_PROXY=localhost`. HTTPS uses CONNECT; plain HTTP checks Host and the request target. A lazy loopback proxy uses separate credentials and policies for each command. Tools that ignore proxy settings cannot connect directly to normal service ports.
- macOS Seatbelt restricts outbound traffic to the proxy's loopback endpoint. Linux requires Landlock ABI v4 for `deny` or `allowlist`, restricts TCP connects to the proxy port, and uses seccomp to block UDP/raw/packet sockets and io_uring bypasses. Landlock port rules apply to every destination address, so Linux also permits TCP connections to that same port on other hosts; it cannot enforce the loopback address. Unix sockets remain available for local DNS services. Unsupported kernels fail closed with an actionable error; `network = "allow"` retains unrestricted networking.
- A rejected domain receives HTTP 403 and appears in the tool result even if the command hides proxy output. Interactive commands offer permission to allow it for the next sandboxed command or always for this project (saved in `.chanto/config.toml`, preserving other settings). Retry the command after approving; commands are never rerun automatically. Background shells offer this when you poll `BashOutput`. An explicit denied-domain rule must be removed before a domain can be approved.
- Systems without a working sandbox (including Linux without Landlock) get a startup warning. In auto mode, sandboxed commands can skip the classifier (`sandbox.auto_allow`).

```toml
[sandbox]
network = "allowlist"
# Omit allowed_domains for package registry defaults, or supply your own list:
allowed_domains = ["github.com", "*.npmjs.org", "pypi.org", "files.pythonhosted.org"]
denied_domains = ["untrusted.npmjs.org"]
```

## Sessions and checkpoints

- **Session forks and sharing**: `/fork [name]` switches to an independent copy, preserving messages, model, permission/thinking settings, tool policy, output style, and checkpoint metadata. In the TUI, `/fork` opens the same prompt list as `/rewind`, with a current-conversation option; plain mode uses `/fork [name] --at <checkpoint index>` to fork before a prompt. `chanto -r <id> --fork-session` forks on startup. Parent IDs and fork message indexes are saved; `/resume` and `chanto sessions` indent children beneath their parents. Forking does not restore working files.
- **Sessions and pull requests**: when a command in a session prints a pull request's URL (GitHub, GitLab or Bitbucket, as `gh pr create` does), the session remembers it. `chanto --from-pr 42` (or the PR's URL) resumes the newest session linked to it, looking in this project first, then in any project.
- **Portable transcripts**: `/share [--no-tool-output]` or `chanto share [id] [--no-tool-output]` writes `~/.chanto/shares/<id>.html` and prints its path. The self-contained HTML includes light/dark themes, collapsible calls and tool output/diffs, model/date/token counts, and an embedded JSON transcript. Cost is marked unavailable because provider billing is not recorded. Secrets are redacted even when local redaction is disabled, home paths become `~`, and images and local checkpoint snapshots are omitted. `chanto import-session <file.html|file.json>` saves a fresh local session; continue with the printed `chanto -r <id>` command. An import keeps the transcript, title, model and usage, but not the file's settings (permission mode, thinking, tool policy, output style), linked pull requests, parent session or dates. A resumed session restores its saved mode, except bypass, which you choose again (`--dangerously-skip-permissions` or shift+tab); a managed `defaultMode` outranks the saved one.

`chanto share [id] --gist` optionally uploads through your installed, logged-in `gh` CLI. It shows the exact HTML bytes and requires typing `yes` before invoking `gh`. Gists are secret by default, but anyone with the URL can access them. No upload occurs without this flag and confirmation.

- **Checkpoints**: `/rewind` (or `esc esc`) restores a prompt checkpoint, including file changes made by Bash. Checkpoints survive session resume. The TUI shows how many files changed, were created, or were deleted and offers code and conversation, conversation only, or code only. In plain mode use `/rewind <n> [both|conversation|code]`. `/redo` puts back what the last `/undo` or `/rewind` took back (files and turns; repeat to step forward again) until you send the next prompt.
- **Editing an earlier prompt branches the conversation** (as in Codex): in the TUI, `/rewind` (or `esc esc`), pick the prompt, then *Edit this prompt in a new branch* (restore code too, or keep files as they are). The prompt's text goes into the input; edit it and press Enter (`esc` cancels). Chanto forks a new session from just before that prompt, as `/fork` does, restores the files to that point when you chose code too (as `/rewind` does; `/rewind undo` in the branch brings them back), and sends the edited prompt there. The original conversation is untouched under its own id: `/resume` lists the branch indented under it. Elsewhere (plain mode, scripts): `/edit-prompt <n> [both|conversation] [new prompt]`, numbered as `/rewind` lists the prompts; without a new prompt it just branches.

Snapshots use a private Git object store in `~/.chanto/history/<project-hash>/`; the project's own Git index, HEAD, refs, and stash are untouched. Snapshots include working-directory files, including untracked files, while honoring `.gitignore`. Ignored files, `.git`, `node_modules`, `target`, `.venv`, and files over the size cap are excluded. Restoring removes newly created files and recreates deleted files within that scope. External side effects (databases, network calls, package caches outside the project, and Git pushes) are not undone. Snapshots run at each prompt and before mutating Bash calls; read-only shell calls skip the extra snapshot. A project's first snapshot stores every file, written as a single pack (a few seconds for 30,000 small files), and may take up to 30 s; later ones only re-read files whose size or modification time changed (well under a second for the same project).

If Git is unavailable or a snapshot exceeds its budget, a warning appears once and that checkpoint uses file-edit snapshots. This fallback covers Write/Edit/MultiEdit/NotebookEdit/apply_patch; Bash changes cannot be recovered by the fallback. Web UI file snapshots remain supported.

Configure snapshot limits in TOML:

```toml
[history]
max_file_bytes = 5242880       # 5 MiB per file
max_snapshot_bytes = 104857600 # 100 MiB total
timeout_ms = 2000            # deadline per snapshot
retention_days = 30           # keep objects referenced by recent saved sessions
```

Checkpoint metadata is stored with the session. Daily best-effort maintenance drops shadow refs belonging only to deleted or expired sessions and runs `git gc --auto`. Old unused project stores are removed during maintenance; checkpoints older than retention may no longer have code available for restoration.

## Context management

token tracking, auto-compaction near the limit, `/compact`, a message bigger than the whole context window refused with its estimated size before anything is sent (attach it as a file instead; `-p` exits 1), each tool result capped at half the window (60 KB at most), and sessions saved with `--continue`/`--resume` (`--resume <id>` works from any directory: a session from another project is resumed in that project)

## Setup

Run `chanto` in a terminal to choose a model or log in. The TUI offers arrow keys and Enter; `--plain` uses a numbered prompt. API keys are masked and stored in `~/.chanto/auth.json` with private permissions (set `CHANTO_KEYCHAIN=1` to keep them in the macOS login keychain instead, leaving only a placeholder in that file; toggling the setting back still resolves saved logins). Codex credentials are never reused: run `chanto login chatgpt` for a separate login.

Detection prefers an explicit Chanto default, then a Codex model with Chanto credentials, hosted credentials in catalog order, local coder models, and other local models. Local probes run concurrently with 300 ms limits: Ollama on 11434, LM Studio on 1234, and llama.cpp/vLLM on 8080. ChatGPT setup lists the models available to your logged-in account. Other recommended hosted models are bundled suggestions; provider access and model availability still depend on your account.

**Free options.** The picker's *Free* section lists three ways to start without paying:
- **OpenRouter's free models:** sign in, then pick from the models OpenRouter currently offers free. Only those that support tool calls and have at least 64k of context are listed, with their context size and how long they're free. Models Chanto has benchmarked come first, marked *tested*, then coding models. They're rate-limited (about 50 requests a day, 1,000 once the account has $10 of credit), and their providers may log or train on prompts.
- **Gemini's free tier:** paste a free key from [Google AI Studio](https://aistudio.google.com/apikey) and pick a model, Flash models first. Google may use free-tier prompts to improve its products.
- **A local model:** free and private (Ollama, LM Studio, llama.cpp).

Don't use the free hosted options on code you can't share.

```sh
chanto setup                         # rerun the picker
chanto setup --plain                 # numbered prompt
chanto setup --model openrouter/anthropic/claude-sonnet-4.5
chanto setup --model qwen3-coder --provider openai --base-url http://localhost:1234/v1
chanto login openrouter              # browser login
chanto login chatgpt --as work       # a second account (`account = "work"` or `/login chatgpt work` to use it)
chanto login openai --with-api-key < /path/to/key-file
chanto login kimi-coding             # a coding plan's key (also zai, minimax-coding)
chanto doctor                        # settings, origins, reachability and fixes
chanto doctor --json                 # stable redacted diagnostic report
chanto models                        # available and recommended models
chanto models openrouter
```

Setup preserves existing config keys and comments. Non-interactive runs (`-p`, pipes, CI) never ask setup questions: exactly one usable candidate is selected temporarily with a stderr notice; otherwise pass `-m provider/model` or run setup in a terminal. `/doctor` uses the same report as the CLI. `/model` in the TUI searches all models with fuzzy matching; Enter switches and saves the choice as the default for new sessions (as in Claude Code); Ctrl+S, or `/model --session <model>`, switches this session only. The list is remembered in `~/.chanto/cache/models.json`, so the picker opens at once and refreshes in the background; `/models` always fetches a fresh list.

To disable reading another agent's settings in place, put this in `~/.chanto/config.toml`:

```toml
[compat]
claude = false
codex = false
```

Or set `CHANTO_NO_CLAUDE_CONFIG=1` / `CHANTO_NO_CODEX_CONFIG=1`. Malformed foreign settings are skipped and reported by doctor with their location; they never prevent startup.

## Install

macOS and Linux (arm64 or x86_64):

```sh
curl -fsSL https://chanto.sh/install.sh | sh
# Pin a release or change the install prefix (binary goes in PREFIX/bin):
curl -fsSL https://chanto.sh/install.sh | sh -s -- --version v0.1.12 --prefix "$HOME/.local"
```

Windows x64, in PowerShell:

```powershell
irm https://chanto.sh/install.ps1 | iex
# For options, download install.ps1, then:
./install.ps1 -Version v0.1.12 -Prefix "$env:LOCALAPPDATA/chanto"
```

The shell installer defaults to `~/.local/bin`; PowerShell defaults to
`$env:LOCALAPPDATA/chanto/bin`. Add that directory to PATH as advised.
Both check the release signature (see [Verifying a release](#verifying-a-release))
and then the archive against the release's `SHA256SUMS` before replacing the
binary. The shell installer supports curl or wget and requires tar and either
sha256sum or shasum. Linux binaries use static musl. Windows uses ZIP archives;
macOS and Linux use `chanto-vX.Y.Z-<target>.tar.gz`. Each `v*` tag publishes them,
with `SHA256SUMS`, as a [GitHub release](https://github.com/chanto-sh/chanto/releases);
`https://chanto.sh/releases/download/<version>/<file>` redirects there, and the
[download page](https://chanto.sh/download) links them. Release archives also
carry build provenance attestations (`gh attestation verify <file> -R chanto-sh/chanto`).

### Container image

Releases after v0.1.5 also publish a container image for linux/amd64 and
linux/arm64 to GitHub's registry, as `ghcr.io/chanto-sh/chanto:<version>`
(for example `0.1.6`) and `:latest`. It isn't a separate build: it holds the
release's own static Linux binary, checked against the signed `SHA256SUMS`
first, on Debian stable-slim with git, ripgrep, bash, coreutils and CA
certificates. It runs as the non-root user `chanto` (uid 1000) in `/work`,
and its entrypoint is `chanto`:

```sh
# A print run on the current directory
docker run --rm -it -v "$PWD:/work" -e OPENAI_API_KEY ghcr.io/chanto-sh/chanto -p "fix the failing test"
# The TUI, keeping config, sessions and memory in a volume between runs
docker run --rm -it -v "$PWD:/work" -v chanto-home:/home/chanto/.chanto \
  -e ANTHROPIC_API_KEY ghcr.io/chanto-sh/chanto
```

Only what you pass reaches the container: forward provider keys with `-e`,
and without a volume at `/home/chanto/.chanto` each `--rm` run starts with no
config or sessions. On Linux, files written into `/work` belong to uid 1000;
if that isn't your uid, add `--user "$(id -u):$(id -g)" -e HOME=/tmp`. The
update check is off in the image (`CHANTO_NO_UPDATE_CHECK=1`): pull a newer
tag rather than running `chanto upgrade`. To add a project's toolchain, build
`FROM ghcr.io/chanto-sh/chanto:<version>` and install it as root, then switch
back with `USER chanto`.

**The sandbox inside a container.** On Linux the OS sandbox is Landlock (see
[Sandbox](#sandbox)), and in a container it may be limited or missing: it
needs the host kernel to support Landlock (5.13+, ABI v4 for
`network = "deny"` or `"allowlist"`) and the engine's seccomp profile to allow
its system calls. Where it can't be applied, Chanto warns at startup and runs
commands unsandboxed (a `deny` or `allowlist` network setting fails closed
instead), so the container itself is the boundary: mount only the project and
give it only the keys and network it needs. To run Chanto on the host and
only its commands in a container, see [Container isolation](#container-isolation).

**Verifying the image.** Each image carries build provenance and a keyless
[cosign](https://docs.sigstore.dev/) signature, both made by the release
workflow for a `v*` tag:

```sh
gh attestation verify oci://ghcr.io/chanto-sh/chanto:0.1.6 --owner chanto-sh
cosign verify ghcr.io/chanto-sh/chanto:0.1.6 \
  --certificate-identity-regexp '^https://github\.com/chanto-sh/chanto/\.github/workflows/release\.yml@refs/tags/v' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

Both check the image's digest; pin `ghcr.io/chanto-sh/chanto@sha256:<digest>`
to keep using exactly the one you verified. The Dockerfile is
[`packaging/docker/Dockerfile`](../packaging/docker/Dockerfile).

### Verifying a release

From v0.1.2 on, every release's `SHA256SUMS` is signed with the project's
Ed25519 release key, and the signature is published next to it as
`SHA256SUMS.sig` (base64). From v0.1.5 on, the same key's signature is also
published in SSH form as `SHA256SUMS.sshsig`, which `ssh-keygen` can check
where no OpenSSL 3 is installed (stock macOS ships LibreSSL, which can't verify
Ed25519; Windows ships the OpenSSH client). `chanto upgrade` refuses a release
whose signature is missing or doesn't verify. The install scripts check the
signature before they use anything in `SHA256SUMS`, including the version
"latest" names: with OpenSSL 3 if there is one (on macOS, also Homebrew's
`openssl@3`; on Windows, also the one in Git for Windows), otherwise with
`ssh-keygen` (OpenSSH 8.1+). They stop if the signature is missing or wrong, or
if neither tool is there; on stock macOS or Windows without OpenSSL 3, that
means releases before v0.1.5, which have no `SHA256SUMS.sshsig`, can't be
installed by script. Releases before v0.1.2 aren't signed and can't be
installed by script at all. A signature can't show that a release is the
newest, so the scripts also refuse a "latest" older than the floor they carry
(currently v0.1.6, raised with each security release), in case the download
host serves an old release; an older release asked for with `--version` /
`-Version` still installs, with a warning. The public key is
[`packaging/release-signing-key.pub`](../packaging/release-signing-key.pub):

```text
-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAHk8ypq/plTuzCUZ4P9+bmwesZLkwM18CBO7AcUQLGd0=
-----END PUBLIC KEY-----
```

and, for `ssh-keygen`, [`packaging/release-allowed-signers`](../packaging/release-allowed-signers):

```text
chanto-release ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIB5PMqav6ZU7swlGeD/fm5sHrGS5MDNfAgTuwHFECxnd
```

To check a download by hand, put `SHA256SUMS`, its signature, the archive and
the key file in one folder, then, with OpenSSL 3 or later:

```sh
openssl base64 -d -A -in SHA256SUMS.sig -out SHA256SUMS.sig.bin
openssl pkeyutl -verify -pubin -inkey release-signing-key.pub -rawin \
  -in SHA256SUMS -sigfile SHA256SUMS.sig.bin    # prints "Signature Verified Successfully"
shasum -a 256 -c SHA256SUMS --ignore-missing    # or: sha256sum -c SHA256SUMS --ignore-missing
```

or with `ssh-keygen` (v0.1.5 and later):

```sh
ssh-keygen -Y verify -f release-allowed-signers -I chanto-release -n chanto-release \
  -s SHA256SUMS.sshsig < SHA256SUMS             # prints 'Good "chanto-release" signature ...'
shasum -a 256 -c SHA256SUMS --ignore-missing
```

The private key never leaves the release workflow's secrets and an offline
backup.

To build from a checkout (needs Rust 1.95+), run `./install.sh --from-source`; it
builds a release binary and installs it like the installer. Cargo can also
build a release from crates.io (installs to Cargo's bin directory):

```sh
cargo install chanto --locked
```

With npm (the package is `@chanto/cli`, which installs the `chanto` command and
pulls in only your platform's binary):

```sh
npm i -g @chanto/cli
```

With Homebrew (macOS and Linux):

```sh
brew install chanto-sh/tap/chanto
```

To embed the agent in a web page, `npm install @chanto/web`: the agent loop and its file tools as WebAssembly, working on a project held in memory, with the model supplied by the page. There's no shell. It's experimental, and its interface may change in any release; see [`web/README.md`](../web/README.md).

Each release renders the formula from `packaging/homebrew/chanto.rb` (version
and archive checksums), uploads it as a `homebrew-formula` artifact, and pushes
it to the [chanto-sh/homebrew-tap](https://github.com/chanto-sh/homebrew-tap)
repository with a deploy key that can write to that repository only
(`HOMEBREW_TAP_DEPLOY_KEY`). A fork without that secret skips the push.

`chanto upgrade` moves to the latest release the way Chanto was installed, and
never without asking (`--yes` skips the question; `--check` only reports; `--force`
reinstalls the latest release):

- **Installer** (install.sh / install.ps1): downloads the release archive for your
  platform, verifies it against the release's `SHA256SUMS`, checks that the new
  binary runs, then replaces the old one (removed, then the new one moved into
  place). On a mismatch or any failure the old binary stays.
- **Homebrew**: offers to run `brew update` and `brew upgrade chanto`, then checks the new version arrived and says so if it didn't.
- **npm and Scoop**: prints the command to run (`npm install -g @chanto/cli@latest` or `scoop update chanto`).
- **Cargo**: prints the `cargo install` command; a build in a source checkout is
  told to `git pull` and rebuild. If the install method can't be told, nothing
  is replaced and Chanto says how to install a release.

`/upgrade` in a session reports whether a newer release exists and the command
to run.

Interactive and print sessions check for stable release updates in the background,
at most once per day, with a two-second timeout. The single request is
`GET https://chanto.sh/releases/latest`, with a Chanto
version User-Agent; it sends no prompts, keys, or project data. Results and failed
attempt timestamps are cached in `~/.chanto/update-check.json` (or
`$CHANTO_HOME/update-check.json`). A newer release produces one dim TUI line or
an stderr line after a print run, with the appropriate Homebrew, Cargo, or
`chanto upgrade` command. Very short runs may exit before a notice is ready.
Checks never run when `CI` is set. Disable them with `CHANTO_NO_UPDATE_CHECK=1`
or in your configuration:

```toml
[updates]
check = false
```

Requirements: a model, either a hosted provider (a key or login) or a local server such as Ollama (`ollama serve`) or another OpenAI-compatible one. `rg` (ripgrep) is used when present. `pdftotext` is needed for PDF reads, and Chrome/Chromium for the Browser tool.

## Usage

```sh
chanto                                  # interactive TUI
chanto "fix the failing tests"          # TUI with an initial prompt
chanto -p "explain src/main.rs"         # print mode (non-interactive; also --print); `chanto exec "…"` is the same
chanto -p --max-turns 20 "…"            # stop after 20 model turns (default 500; also `max_turns`)
chanto -C ../other-repo                 # work in another directory (also --cwd, --cd)
chanto --no-think                       # turn model reasoning off (also `think = false`)
chanto -p --mode auto "add a --json flag and tests"
chanto -p --output-format json "…"      # machine-readable result (usage, duration, num_turns and tool_calls for the whole session, compaction included)
chanto -c                               # continue the last session here (an unreadable one is reported, with the newest readable one resumed instead)
chanto --model qwen3-coder:30b --num-ctx 65536
chanto --provider openai --base-url http://localhost:8080/v1 --model local
chanto --plain                          # line-based REPL instead of the TUI
chanto --worktree fix-auth              # work in a new checkout and branch (see below)
chanto -r                               # pick a previous session to resume
chanto --completions zsh > ~/.zfunc/_chanto
chanto goal "make the importer handle CSV files"   # work until agreed criteria pass
chanto team "add OAuth login and a settings page" --agents 3
chanto batch "fix the lint errors in {}" --from files.txt --jobs 4 -- --mode acceptEdits  # once per line; a rerun skips what succeeded; runs share this folder, so keep items from editing the same files
```

Text output (`-p` without `--output-format`, or with `--verbose`) drops terminal escape sequences and control characters from the model's text, whether or not stdout is a terminal, so a reply can't retitle the window or write to the clipboard; `json` and `stream-json` carry the text unchanged.

SIGTERM and SIGHUP stop Chanto like an interrupt, in print mode, the TUI and `--plain`: the running turn ends, the session is saved with your message (and any partial reply) so `-c` picks it up, and Chanto exits with 143 (SIGTERM) or 129 (SIGHUP). A second signal exits at once.

To drive a multi-turn session from another program, use Claude Code's
`--input-format stream-json` (it needs `-p --output-format stream-json`, and
no prompt argument). Each stdin line is a user message,
`{"type":"user","message":{"role":"user","content":"…"}}`, where `content` is a
string or an array of `text` blocks and base64 `image` blocks. Messages run in
order as turns of one session: their events stream to stdout, followed by a
`result` event per message. A line that isn't a valid user message gets an
`{"type":"error",…}` event and is skipped. Claude Code's control requests
(`{"type":"control_request",…}`) aren't supported: each gets a
`control_response` with an error, and the session carries on. EOF (or Ctrl-C)
ends the session; the exit code is 1 if any message was rejected or any turn
failed.

### Claude Code's flags

Scripts written for `claude` work with these flags, which behave as in Claude Code:

| Flag | What it does |
|---|---|
| `--system-prompt <text>`, `--system-prompt-file <path>` | Replace Chanto's own instructions. Tools keep working (their definitions are sent separately), and the environment, skills, subagents, MCP servers and instruction files (`AGENTS.md`, `CLAUDE.md`) are still added, as Claude Code adds them outside its system prompt |
| `--append-system-prompt <text>`, `--append-system-prompt-file <path>` | Add text to the end of the system prompt (both can be given) |
| `--mcp-config <file-or-json>` | MCP servers for this session, as `{"mcpServers": {…}}` (or just the servers object). Repeat the flag for several; unlike Claude Code, one value per flag, so a prompt after it isn't taken for a file. These servers win a name clash |
| `--strict-mcp-config` | Use only the `--mcp-config` servers: none from `.mcp.json`, `~/.claude.json`, config files or plugins |
| `--settings <file-or-json>` | Settings for this session, over the same keys in every settings file (managed settings still win). JSON is read as Claude Code's `settings.json` (`permissions`, `hooks`, `env`, `model`, `mcpServers`, …); a `.toml` file as a Chanto config layer |
| `--setting-sources user,project,local` | Read only these config files. `user`: `~/.chanto/config.toml`, `~/.claude/settings.json`, `~/.claude.json`, `~/.codex/config.toml`, `~/.chanto/mcp.json`. `project`: `.chanto/config.toml`, `.claude/settings.json`, `.codex/config.toml`, `.mcp.json`. `local`: `.chanto/config.local.toml`, `.claude/settings.local.json`. Managed settings are always read |
| `--session-id <uuid>` | Use this id for the new session (it must be a UUID not in use; not with `-c`/`-r`) |
| `-n`, `--name <name>` | Name the session: the name is its title in `chanto sessions` and the resume picker, and `chanto -r <name>` resumes it |
| `--no-session-persistence` | Don't write the session to disk, so it can't be resumed |
| `--json-schema <schema>` | Print mode: the model finishes with JSON matching the schema. The answer is checked (`type`, `properties`, `required`, `additionalProperties`, `items`, `enum`, `const`, `anyOf`/`oneOf`/`allOf`, bounds, local `$ref`s; `format` is only an annotation), and if it doesn't match the model gets one more turn with the errors. `--output-format json` puts the value in `structured_output`; text output prints just the JSON. Still invalid: exit code 1. An invalid schema stops Chanto before it starts |
| `--agent <name>` | Run the main conversation as that subagent: its prompt replaces Chanto's instructions (`--system-prompt` wins if given, with the subagent's prompt appended), and its `tools`, `disallowedTools` and `model` apply (`--model` still wins) |
| `--agents <json>` | Subagents for this session: `{"reviewer": {"description": "…", "prompt": "…", "tools": ["Read"], "disallowedTools": [], "model": "inherit"}}`, or a path to a JSON file holding that. They replace subagents with the same name. **A number keeps its Chanto meaning**: the team size for `chanto team` (`--agents 3`) |
| `--tools <list>` | Limit the built-in tools: `"Bash,Edit,Read"`, `""` for none, `"default"` for all. MCP tools aren't affected (name them in `--disallowed-tools`, or use `--strict-mcp-config`) |
| `--plugin-dir <path>` | Load a plugin for this session only: a directory with `.claude-plugin/plugin.json` (or `commands/`, `skills/`, `agents/`, `hooks/`), or a folder of such plugins. Repeatable. `.zip` archives aren't supported |
| `--bare` | Minimal, reproducible start: no hooks, skills, commands, subagents, plugins, MCP servers, memory, lessons or instruction files from disk, and only the Bash, Read and Edit tools (unless `--tools` says otherwise). What you pass still loads: `--settings`, `--mcp-config`, `--agents`, `--plugin-dir`, `--append-system-prompt`. Sets `CLAUDE_CODE_SIMPLE=1` for hooks and commands |
| `--safe-mode` | Claude Code's troubleshooting start (also `CLAUDE_CODE_SAFE_MODE=1`): the same discovery as `--bare` is skipped (hooks, skills, commands, subagents, plugins, MCP servers, memory, lessons, instruction files), and so are your status line, output style, custom theme and keybindings and language servers. Unlike `--bare`, every built-in tool stays, and your model, provider, login and permission settings apply as usual. What you pass on the command line still loads. Sets `CLAUDE_CODE_SAFE_MODE=1` for hooks and commands |
| `--restricted` | Claude Code's locked-down start (also `CLAUDE_CODE_RESTRICTED=1`): the built-in tools that run commands or code (`Bash`, `BashOutput`, `KillShell`, `Monitor`, `Browser`, `Computer`, `ChantoCloud`) and `WebFetch` are removed unless `--tools` names them; file tools (`Read`, `Edit`, `Write`, `MultiEdit`, `NotebookEdit`, `apply_patch`, `Glob`, `Grep`, `Ls`) are refused outside the working directories (the project, `--add-dir` folders and the temp directory) whatever rules, hooks or mode say; only managed settings and `--settings` are read (no user, project or local settings files, so pass `--model`/`--provider` or `--settings` for anything your config would set); bypass mode is refused (`--dangerously-skip-permissions` is an error); and `chanto cloud` and `--cloud` runs are refused. MCP servers from `--mcp-config` still load. Sets `CLAUDE_CODE_RESTRICTED=1` for hooks and commands |
| `--include-partial-messages` | Accepted and ignored: Chanto's `stream-json` output already streams text (`{"type":"text"}` events) as it arrives |
| `--permission-prompt-tool <mcp__server__tool>` | Print mode: when a tool call needs approval, call this MCP tool with `{tool_name, input, tool_use_id}`. It answers with JSON text, `{"behavior": "allow", "updatedInput": {…}}` or `{"behavior": "deny", "message": "…"}`. Without an answer the call is denied |
| `--ax-screen-reader` | Run for a screen reader: the line-based interface, no animation, and a bell when a prompt waits (see [Accessibility](#accessibility); also `screen_reader = true` or `CLAUDE_AX_SCREEN_READER=1`) |

Note that Chanto's `--agents` takes either form: a number is the team size, as before, and anything else is read as Claude Code's JSON.

### Keys

| key | action |
|---|---|
| `shift+tab` | cycle permission mode |
| `esc` | interrupt the model (also cancels running tools); stops a `/loop` |
| `ctrl+c` | clear input or stop the turn; twice to quit |
| `ctrl+o` | expand/collapse all tool output and thinking; on a `[Pasted text #n]` tag in the input, expand that paste |
| `?` | show the shortcuts panel (on an empty prompt; any key closes it) |
| click | with Chanto's mouse handling enabled, open an HTTP(S) link in your browser, or expand/collapse a tool or thinking block |
| `ctrl+r` | search prompt history |
| `ctrl+f` | search the transcript (`ctrl+f`/`↑` older match, `↓` newer) |
| `ctrl+g` | edit the prompt in `$VISUAL`/`$EDITOR` |
| `e` (Edit/Write permission prompt) | open the proposed file content in `$VISUAL`/`$EDITOR`; saving and quitting approves the change with your version, and the model is told you edited it |
| `/theme [name\|auto]` | switch colors (see [Themes](#themes); set `theme` in config to keep it) |
| `ctrl+t` | toggle todo list |
| drag | select text anywhere on screen, including permission prompts, question dialogs and `/btw`; it's copied when you let go unless `copy_on_select = false`, which keeps it highlighted for `ctrl+shift+c` |
| `ctrl+s` / `F2` | hand the mouse to the terminal for its own text selection (or hold shift/option while dragging) |
| `ctrl+v` | paste an image from the clipboard (inserts an `[Image #n]` tag; the image is attached when you send) |
| `ctrl+space` / `/voice` | [dictate](#voice-input): record, press again to transcribe into the prompt (`esc` discards) |
| `esc esc` | rewind to an earlier prompt |
| `enter` while it works | the model gets the message at its next step (between tool calls); slash commands wait for the turn to end. If the turn ends before the message is delivered, it starts the next turn as plain text (a leading `!`, `/` or `#` isn't run, and `@file` isn't attached) |
| `ctrl+x enter` | queue the message to run after the current turn |
| `ctrl+x ctrl+s` / `ctrl+enter` | send now: cut the current turn short and run the message next |
| `↑` on an empty prompt while working | pull messages the model hasn't received yet (queued ones, and ones sent with Enter that are still waiting for its next step) back into the prompt to edit |
| `ctrl+x ctrl+e` | edit the prompt in your editor (same as `ctrl+g`) |
| `shift+enter` / `alt+enter` / `ctrl+j` / `\` + enter | newline |
| `↑`/`↓` | history (or move between input lines) |
| `pgup`/`pgdn`, mouse wheel | scroll transcript |
| `@path` | attach a file or image (fuzzy completion) |
| `!cmd` | run a shell command; its output is added to the context. Like Claude Code, this is your own escape hatch: it runs directly as your user, *not* through the Bash tool's sandbox or permission rules. |
| `#note` | save a note to project memory (`#--agents note` appends to `./AGENTS.md` instead) |
| `&task` | start `task` as a [background run](#background-and-remote-runs) (same as `/bg task`) |

Messages you type while the model is working are delivered at its next step, as in Claude Code. Large pastes collapse to `[Pasted text #n +N lines]`. A message too large for the model's context stays in the box when you press enter, and enter again saves the paste to a private file under `~/.chanto/attachments` for the model to read from; ctrl+o with the cursor on or just after the tag expands it in place so you can edit it, and `collapse_pastes = false` in config inserts every paste as plain text.

### Prompt extras

- **Emoji shortcodes**: `:tada:`, `:+1:`, `:rocket:` and other common ones become emoji when you send a prompt (not in pasted text, commands or `!shell`).
- **Pull request status**: with the GitHub CLI (`gh`) signed in, the status line shows the current branch's pull request and what it's waiting on (`PR #12 checks failing`, `approved`, `changes requested`), checked once a minute.
- **Prompt suggestions** (`prompt_suggestions = true`, off by default since each one is an extra model call): after a turn, a likely next prompt appears greyed out in the empty prompt; `tab` takes it.

### Background command output

A `Bash` call with `run_in_background: true`, or a foreground command that outlives its timeout, keeps a shell id such as `bash_6`. The model uses `BashOutput` to read only new output and `KillShell` to stop the job. `/bashes` lists the jobs.

Output polls show the originating command’s short description when supplied, falling back to the command itself:

```text
BashOutput(Build release binary · bash_6)
  ↳ Waiting for new output (up to 30s)
```

`wait_secs` waits for new output or exit instead of repeatedly polling; its maximum is 600 seconds. A `filter` regex waits for matching lines. Results distinguish **Still running**, **Completed · exit 0**, **Failed · exit N** and **Stopped**, and preview up to three recent lines. Long lines are shortened in the preview; expand the result to inspect more output. A quiet running job says **no new output** (or **no new matching output** with a filter). Nonzero exits are marked as errors. For an ongoing stream of notifications instead of explicit polls, use `Monitor`.

### Background work panel

While subagents run (foreground `Task` calls, including parallel ones, and background agents), the agents panel under the prompt lists a `main` row and one row per agent: its type, what it's doing now (its latest tool call or line of text), how long it has run and its tokens so far (input plus output). Background shells follow, using their short description when available and a bounded command preview otherwise. `/bashes` shows the full commands.

The footer shows the model name without repeating the provider prefix. PR status takes the branch label's place when available; otherwise the branch remains visible. Fully connected MCP servers stay out of the footer; incomplete connections and failures remain visible, with details in `/mcp`. `/model` shows the full provider/model selection.

```
 ● main
 ○ general-purpose  Writing candidates to cand_english.txt     1m 35s · ↓ 36.2k tokens
 ○ Explore          Grep(fn parse_args)                         42s · ↓ 12.1k tokens
 ⚙ npm run dev · bash_1
   ↓ 2 more
```

At most five lines are shown (three on a short terminal); the rest collapse into `↓ N more` and scroll into view with the selection. From an empty prompt, `↓` selects a row (`↑`/`↓` or `ctrl+p`/`ctrl+n` move, `↑` past the top goes back to the prompt):

- `enter` (or a click) on an agent shows its live transcript in place of the main one, and `●` moves to it; `enter` on `main`, or `esc`, returns to the main conversation. A background agent started outside a `Task` call (a `/btw` fork) shows its latest message instead.
- While you view a background agent, the prompt says `Message <type> (<id>)…` and what you send goes to that agent rather than the main conversation: a running agent reads it after its current step, and a finished one is resumed with it (keeping its context), as `Task` with `resume` would. A foreground agent can't take messages mid-run: the text stays in the prompt with a hint to go back to `main`. `/` commands and `!` shell commands still run in the main session.
- `x` stops a background agent or shell, or dismisses a failed row. A foreground agent stops with the turn (`esc`).
- `esc` returns focus to the prompt.

A finished agent leaves the panel at once; a failed or stopped one stays 30 seconds. Each agent keeps its last 400 transcript entries for viewing. In inline mode the transcript shows in the live area above the prompt. `/tasks` and `/bashes` list background work too.

### Custom keybindings

`/keybindings` creates `~/.chanto/keybindings.json` (or shows where it is). It uses Claude Code's format, so a file written for Claude Code works as is, and with Claude Code compatibility on Chanto also reads `~/.claude/keybindings.json` (its own file wins where both bind a key):

```json
{
  "bindings": [
    { "context": "Chat", "bindings": { "ctrl+e": "chat:externalEditor", "ctrl+s": null, "ctrl+k ctrl+t": "app:toggleTodos" } },
    { "context": "Confirmation", "bindings": { "y": "confirm:yes", "n": "confirm:no" } }
  ]
}
```

Keys are `ctrl`, `shift`, `alt`/`meta`/`option` and `cmd` with a key name, case-insensitive (`shift+k` for Shift+K); a chord is keys separated by spaces, each within 3 seconds; `null` unbinds a key. Saved changes apply right away, and problems (an unknown action, a reserved key such as `ctrl+c`, a keystroke Chanto can't read) are listed when the file loads. Contexts Chanto applies: `Global`, `Chat`, `Autocomplete`, `Confirmation` (permission prompts and questions), `HistorySearch` and `Select`. Actions with a Chanto equivalent work (submit, newline, queue and send-now, external editor, image paste, cycle mode, model picker, thinking toggle, history search and navigation, todos, transcript expand, selection copy, stop background agents, exit, and the dialog and list keys); Claude Code actions for screens Chanto doesn't have (its diff panel, agent view, mods' panes, fast mode) are accepted and do nothing, so a shared file stays valid.

### Notifications

When a turn that took longer than `notify_after_secs` (default 30) finishes, or a permission prompt or question is waiting for you, Chanto sends a desktop notification through the terminal. Terminals that report focus (most do) only get one while the window is in the background. A waiting prompt notifies at once when you're away, and when you switch away later if it's still unanswered. If the terminal doesn't report focus, a prompt notifies after 6 seconds unanswered and a finished turn always notifies. This works in full screen and inline mode.

`notifications` in config picks how:

| value | sends |
|---|---|
| `auto` (default) | OSC 9 in iTerm2, WezTerm, Ghostty, kitty and Windows Terminal; OSC 777 in foot, urxvt and VTE terminals (GNOME Terminal, Tilix); `system` in Terminal.app, which can't raise a notification itself; the bell anywhere else |
| `osc9` | `ESC ] 9 ; message BEL` |
| `osc777` | `ESC ] 777 ; notify ; title ; message BEL` |
| `system` | the OS posts it and the bell rings. On macOS it uses [`terminal-notifier`](https://github.com/julienXX/terminal-notifier), whose click focuses your terminal (or opens a run's link); allow the app in System Settings → Notifications. If it isn't installed, only the bell rings. On Linux it uses `notify-send` |
| `bell` | the terminal bell (most terminals flag the tab or bounce the dock icon) |
| `off` | nothing |

In tmux, OSC notifications are wrapped in a DCS passthrough, which needs `set -g allow-passthrough on` (tmux 3.3+). Messages are reduced to one line with control characters removed, so text from the model can't inject escape sequences. `notify_after_secs = 0` turns off the finished-turn notification only.

`notify_sound` also plays a sound when a finished-turn notification goes out (same timing and focus rules): `off` (default), `default`, a system sound name (`Glass`, `Ping`… from `/System/Library/Sounds` on macOS; `complete`, `bell`… from `/usr/share/sounds/freedesktop/stereo` on Linux; a `C:\Windows\Media` `.wav` name on Windows) or the path of a sound file. It plays with `afplay` (macOS), `paplay` or `aplay` (Linux) or PowerShell's `SoundPlayer` (Windows, `.wav` only).

#### Status on the terminal tab

Besides the `✓`/spinner tab title (OSC 0), Chanto reports what the TUI session is doing through the [Program Status Protocol](https://www.superlogical.com/rex/docs/build/program-status) (OSC 7501), as Claude Code does, so terminals that implement it can show it on the tab: `working` while a turn runs, `blocked` (kind `permission` or `question`, with the prompt as the message) while it waits on you, then `done`, or `error` when the turn failed. Each report is `ESC ] 7501 ; state=…:app=chanto[:kind=…][:msg=<base64>] ESC \`, sent only when the state changes; the terminal drops `working` and `blocked` when Chanto exits.

`terminal_status` in config picks when: `auto` (default) sends it only to terminals known to implement it (Ghostty: `TERM_PROGRAM=ghostty`, `TERM=xterm-ghostty`) and never inside tmux or screen, so other terminals never see a sequence they might print; `on` always sends it; `off` never does. `CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1` turns it off too, as in Claude Code.

### Keeping the machine awake

A long turn, a `/loop` or a background run stops when the computer idles into sleep. By default Chanto prevents idle sleep while it's working, and releases it as soon as it's waiting for you. It starts after 20 seconds of work, since no system idles into sleep sooner, so quick answers never take it. At an idle prompt, the status line shows `awake` if a background job or the `always` setting is still preventing sleep.

```toml
keep_awake = "working"   # default; or "never", or "always" (the whole session)
```

It takes a power assertion on macOS (what `caffeinate -i` takes, without starting a process), `systemd-inhibit` on Linux (skipped where that isn't installed) and `SetThreadExecutionState` on Windows. None of them can stop a closed laptop lid from sleeping (unless a Mac is on power with an external display), so for overnight work use a machine that stays on, `chanto --bg` on a server, or cloud runs.

### Themes

`/theme` lists the themes and `/theme <name>` switches for the session (in the TUI, a bare `/theme` opens a picker). Set `theme = "<name>"` in config to keep one. Built-in themes: `dark`, `light`, `dracula`, `nord`, `solarized-dark`, `gruvbox` and `colorblind`. The default, `auto`, picks `dark` or `light` from the terminal background: the terminal's own answer to a background-colour query (OSC 11), else `COLORFGBG`, else `dark`. The system appearance is not used, since it says nothing about the terminal's colours; under tmux or ssh, where the query goes unanswered, set `theme = "light"` if your terminal is light. Plain code tokens always use the terminal's own foreground colour. With `NO_COLOR` set, the TUI uses no colors at all and keeps only bold, italics and reverse video (inline code keeps its backticks). `colorblind` is the Okabe–Ito palette, whose colors stay distinct under the common forms of color blindness: its `+`/`-` diffs use the blue/orange pair, the safest two, so a diff still reads without color.

Add your own as `~/.chanto/themes/<name>.toml`. Start from a built-in with `base` (default `dark`) and set only what you change:

```toml
base = "nord"            # built-in to start from
background = "dark"      # dark | light: picks the code highlighting (default: the base's)
accent = "#e5a50a"
error = "light-red"
```

Colors are `#rrggbb`, `#rgb`, an ANSI name (`black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `gray`, `dark-gray`, `light-red` … `light-cyan`, `white`) or `default` (the terminal's own color). Keys:

| key | colors |
|---|---|
| `accent` | spinner, list selections, the welcome box, `#` and `##` headings |
| `dim` | secondary text: tool output, hints, the footer |
| `faint` | barely-there hints ("ctrl+o to expand") |
| `border` | the prompt's frame in default mode |
| `error`, `success`, `warning` | errors and failed tools, finished tools, warnings |
| `info` | plan mode, diff hunk headers |
| `accept_edits`, `auto_mode` | the prompt frame and footer in those modes |
| `code`, `code_block`, `link` | inline code, code without syntax highlighting (and the `!` prompt), links |
| `panel_bg` | the `/` and `@` menus (keep it distinct from your terminal's background) |
| `selection_bg`, `selection_fg` | selected text, search hits, and the highlighted row of the `/` and `@` menus |
| `diff_add`, `diff_remove` | `+`/`-` lines in diff code blocks |
| `diff_add_bg`, `diff_remove_bg` | added and removed lines in edit previews |

An unknown key or a value that isn't a color is an error naming the key, and Chanto falls back to the default theme with a warning.

### Accessibility

`chanto --ax-screen-reader` (or `screen_reader = true` in config, or any of `CHANTO_AX_SCREEN_READER`, `CLAUDE_AX_SCREEN_READER`, `CHANTO_SCREEN_READER` in the environment) runs Chanto for a screen reader: it uses the line-based interface (`--plain`) rather than the full-screen one, so output arrives in order with no in-place redraws, and it rings the terminal bell instead of the usual notification when a prompt is waiting. Subagents and hooks it starts inherit the mode. Claude Code's `axScreenReader` setting is read too.

Animations are off whenever a screen reader is on, and otherwise follow the `animations` config key (`--no-animations` forces them off): with them off the spinner holds one steady frame instead of cycling, and the line-based frontend prints the working label once instead of repainting it. Left unset, `animations` follows the macOS "Reduce motion" setting.

### Several sessions in one project

Sessions in different terminals working in the same project know about each other. Each keeps a small record (its latest prompt, the files it edited recently, and a heartbeat) under `~/.chanto/presence/`. At the start of every turn the agent is told which other sessions are here and which files they've touched, so it can steer clear. A peer's prompt text is never put in another session's context; only `/peers` shows the tasks, to you. If it goes to edit a file another live session changed in the last ten minutes, the edit stops once with who has it and why; making the same edit again goes ahead. With the existing check that a file hasn't changed since the agent read it, neither can silently overwrite the other. `/peers` lists the other sessions. `CHANTO_NO_PRESENCE=1` keeps a session out of this entirely (nothing shared, nothing read), for runs that shouldn't be steered by, or appear to, interactive sessions. For work that has to touch the same files at once, start each session with `chanto --worktree [name]`. It works in a new checkout and branch (`chanto/<name>`, under `.chanto/worktrees/`), leaving yours untouched. On exit, a worktree with no changes is removed, otherwise Chanto says how to review, merge or discard the branch. `chanto run --bg` and `/team` isolate their agents the same way.

### Inline mode

`chanto --inline` (or `tui = "inline"` in config) draws in your terminal's normal screen instead of a full-screen view. Finished output (answers, tool results) is printed once into the terminal's scrollback, so your terminal's own scrolling, search and copy work, including over SSH and in tmux. Only what is still changing is drawn, in a region at the bottom: the end of the answer being streamed, running tools, the to-do list, prompts and dialogs, and the input box.

Differences from full screen:
- `ctrl+o` prints the most recent finished tool or thinking block again, expanded, instead of opening blocks in place.
- Search with your terminal's find instead of `ctrl+f`. The mouse belongs to your terminal, so there is no click-to-expand.
- When the window is resized, Chanto clears the screen and reprints the latest output at the new width; older scrollback stays as the terminal rewrapped it. A few lines either side of that point may appear twice. The full transcript is always in the session (`/export`).
- `/clear`, `/rewind` and `/resume` print the new state below what's already there, since scrollback can't be rewritten.

### Slash commands

`/customize /reload /plugin /plugins /help /clear /compact /mode /model /models /think /effort /context /status /cost /usage /skill-doctor /insights /stats /resume /sessions /edit-prompt /rename /title /loop /autocompact /plan /recap /cd /background /rewind /undo /redo /init /review /code-review /simplify /debug /batch /fewer-permission-prompts /security-review /pr-comments /diff /output-style /memory /learned /permissions /allow /deny /sandbox /tools /mcp /hooks /skills /agents /peers /todos /tasks /bashes /fork /share /export /copy /btw /verify /outcome /goal /team /bg /runs /schedule /ide /add-dir /vision /vim /quiet /transcript /voice /theme /statusline /terminal-setup /login /logout /privacy-settings /doctor /feedback /bug /diagnostics /release-notes /upgrade /config /exit`, plus your custom commands and skills (`/skill-name [args]`).

Bundled skills are prompt-based commands shipped in Chanto. A skill or command of your own with the same name replaces one (its alias doesn't: `/review` always runs the bundled review):
- `/code-review [low|medium|high|xhigh|max] [--fix] [--comment] [--max-findings n|all|default] [pr#|branch|range|path]` (alias `/review`): reviews the current branch (commits ahead of upstream, or of `main`, plus uncommitted changes), or the target you give, for correctness bugs; from `high` up it also covers duplication, complexity and waste cleanups. `low` reports a few high-confidence findings; higher levels broaden coverage. Without a level it reuses the last one you typed (stored in `~/.chanto/code-review.json`, as is `--max-findings`), else the session's effort. `--fix` applies the findings; `--comment` posts them on the GitHub PR (`gh`) or GitLab MR (`glab`). In the interactive UI the review runs as a background agent and its findings arrive when it finishes; in print mode it runs in the foreground. `ultra` (Claude Code's cloud review) runs a local `max` review instead.
- `/simplify [target]`: four parallel read-only reviewers (duplication, complexity, waste and placement) look at the changed code, then the fixes are applied and the tests run. It doesn't hunt for bugs; correctness belongs to `/code-review`.
- `/batch <instruction>`: researches the codebase, splits the change into the smallest independent units that can run in parallel (a handful, up to about 30 for a very large change) and presents the plan; once you approve, it runs one background agent per unit, each in its own git worktree (needs a git repository or a `WorktreeCreate` hook). Each agent implements and tests its unit, commits on its branch, and opens a pull request when the repo has a GitHub remote and `gh` is signed in; otherwise Chanto offers to merge the branches for you at the end.
- `/debug [description]`: troubleshoots a problem by reading this session's entries in `~/.chanto/logs/chanto-<date>.log`, reproducing it first, and testing hypotheses before fixing the root cause.
- `/fewer-permission-prompts`: tallies the Bash commands and MCP tools used in this project's saved sessions, picks the read-only ones, and adds them as allow rules to `.chanto/config.toml`.

`/statusline [description]` writes a status line script (`~/.chanto/statusline.sh`, or `.ps1` on Windows) from your description, or from your shell prompt (`PS1`) without one, and sets `status_line` in `~/.chanto/config.toml` (new sessions pick it up). The command gets `{"cwd", "model", "mode", "context_used", "context_max"}` as JSON on stdin and its first output line is shown.

- `/status`: one screen with the version, model, provider and where its credential comes from, mode, sandbox, directory and branch, context use, session id and title, this session's tokens and cost, and each MCP server's state. `/context` breaks the context down, and `/context skills` (alias `/skill-doctor`, the name Claude Code uses) lists each skill, subagent definition and connected MCP server with two estimated token costs: its *listing*, sent with every request (a skill's or subagent's line in the system prompt, an MCP server's instructions and tool definitions, or just a one-line hint per tool that tool search defers), and its *full* cost once used (a skill's body when loaded, a subagent's prompt in its own context, every MCP tool definition). Rows are sorted by listing cost, so what bloats every request comes first; skills with `disable-model-invocation: true` cost nothing until run. `/doctor` checks the setup, including whether the provider is reachable.
- `/stats`: sessions, prompts, tokens and cost for this project and for all projects over the last 7 and 30 days, then by day (last 7) and by model (last 30), from the saved sessions. Each session counts on the day it was last active, under the model it last used. `/cost` covers the current session and `/insights` where its tokens went.
- `/rename <title>` (or `/title`): names the session; `/resume`, its picker and `chanto sessions` list it by that name. Without a title it shows the current one. Sessions are otherwise named after their first prompt.
- **Finding, archiving and deleting sessions**: in the TUI, `/resume` opens a picker you filter by typing (every word must appear in the session's title, first prompt or id). In it, `ctrl+d` deletes the selected session after asking, `ctrl+a` archives it (or unarchives an archived one), and `tab` shows or hides archived sessions (`/resume --all` opens it with them shown). From the command line, `chanto sessions [--search <text>] [--all]` lists them, and `chanto sessions delete <id>`, `chanto sessions archive <id>` and `chanto sessions unarchive <id>` manage one (ids may be shortened to a unique prefix); `/sessions delete|archive|unarchive <id>` does the same inside a session, asking before a delete, and `/sessions --search <text>` or `/sessions --all` list. Archived sessions are hidden from the lists (unless `GET /sessions?all=true`) and `chanto -c`, but still resume by id, which unarchives them. Deleting removes the transcript and the session's `/rewind` snapshots (refs in the shadow history no other session uses); it never touches project files. The open session can't be deleted.
- `/loop [interval] [prompt]`: runs the prompt now and then again in this session (the TUI and the `--plain` REPL), e.g. `/loop 5m check the CI and fix failures`. As in Claude Code, the interval leads as a bare token (`30m`) or trails as `every 2 hours`; units are `s`, `m`, `h` and `d`, and seconds round up to whole minutes. Without an interval the loop is self-paced: at the end of each iteration the model calls `ScheduleWakeup` to pick the next run (1 minute to 1 hour, with a reason shown in the transcript) or to stop the loop when the task is done; an iteration that doesn't reschedule gets one fallback run 20 minutes later, and the loop ends if that one doesn't either. Without a prompt it runs `loop.md` (`.chanto/loop.md` or `.claude/loop.md` in the project, else `~/.chanto/loop.md` or `~/.claude/loop.md`, read again each iteration) or a built-in maintenance prompt: continue unfinished work, tend the branch's pull request, then a cleanup pass. The prompt can be a slash command (`/loop 20m /review`). A run that comes due while a turn is running or a dialog is open runs once when it ends; missed runs aren't caught up. The footer shows the next run time and the machine is kept awake while a loop lasts (`keep_awake`). `/loop status` shows the loop, `/loop stop` or `esc` ends it (ctrl+c while the REPL waits), and loops end on their own after 7 days. There is one loop per session.
- **Scheduled tasks**: the model can schedule prompts with `CronCreate` (a 5-field cron expression in local time, recurring or once, e.g. "remind me at 3pm"), list them with `CronList` and cancel them with `CronDelete`. Creating one asks for approval in default and accept-edits mode (auto mode's classifier judges it). A scheduled prompt goes to the model as plain text: it can't start with `!`, `/` or `#`, and never runs a shell command, slash command or memory note (your own `/loop 5m /command` still runs the command; the default loop prompt doesn't). They fire only while the session is open and idle, at most 50 per session, and recurring ones end after 7 days with a final run.
- `/autocompact [<tokens>|auto|off]`: when this session compacts on its own: a token count (`120k`, capped at 95% of the context), `off` (only `/compact` compacts), or `auto` (back to `compact_threshold`). Bare `/autocompact` shows the current setting.
- **Compacting for a smaller context window**: `/model` to a model whose context window can't hold the conversation (it has reached `compact_threshold` there) asks whether to compact first with the current model, which can still read all of it, switch anyway, or cancel; without a terminal to ask (print mode, `--plain` scripts) it compacts first. A switch made mid-turn, or one you let through, is handled before the next request: the model that served the last request writes the summary, as Claude Code does. A fallback model with a smaller window is covered too (see [Fallback models](#fallback-models)).
- **Idle compaction** (`idle_compaction = true`; off by default, Claude Code's `idleCompaction` is honoured): when the TUI has sat idle after a turn for `idle_compact_minutes` (default 54, just under a one-hour prompt cache) and the conversation fills at least `idle_compact_percent` of the context window (default `0.5`; `CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS` sets a token count instead), Chanto compacts it in the background so your next prompt starts on a short conversation. It runs at most once per idle spell, not while a dialog, background agent or monitor is active, and not when `/autocompact off` is set; a prompt you send meanwhile waits for it to finish.
- `/plan [task]`: switches to plan mode, and starts planning the task if one is given.
- `/recap`: a one-line summary of the session so far from the session's model; nothing is added to the conversation. The TUI also shows one when you come back after 3 minutes away with the terminal unfocused (at least 3 prompts in, never twice in a row); `away_summary = false` (or `CLAUDE_CODE_ENABLE_AWAY_SUMMARY=0`, or Claude Code's `awaySummaryEnabled`) turns that off.
- `/cd <path>`: moves the session to another directory and keeps the conversation. That directory's instructions, commands, skills, agents, hooks, permission rules and env apply at once (after a trust prompt for an untrusted directory), `CwdChanged` hooks run, `Cd(...)` deny rules can forbid destinations, and the session file moves so `--resume` finds it there. MCP servers from the new `.mcp.json` start the next time you run Chanto there. Allow rules and extra directories are replaced, not merged: the old project's settings, its saved "always allow" answers, and this session's `/allow` and `/add-dir` grants stop applying; your user-level settings, `--add-dir` directories and the new project's settings apply (its allow rules, `additionalDirectories`, hooks and env only when the folder is trusted). Deny and ask rules from before are kept, since they only tighten. The auto-mode classifier's `auto_mode_rules` and `fast_path` switch the same way (the new project's only when trusted). If the session is in accept-edits mode only because you answered "always allow" to an edit, `/cd` puts it back in the mode it was in before and says so; accept edits you chose yourself (shift+tab, `--permission-mode`, the `mode` setting) stays.
- `/background [prompt]`: hands this conversation to a [background run](#background-and-remote-runs) in its own git worktree (with your uncommitted changes) and frees the terminal; Chanto prints the run id to follow with `chanto attach <id>`. The prompt, if given, is the run's next instruction; without one it continues the work in progress.

`/btw <question>` answers a side question from the conversation so far, including mid-turn: it runs alongside the work without interrupting it, and neither the question nor the answer enters the conversation, so tangents don't cost context later. It has no tools. Side questions form a thread (each sees the newest 20 earlier ones) until you clear them. In the TUI the overlay opens as soon as you ask, showing the question with an `Asking the side question…` progress line while the answer is on its way (a failure replaces it with the provider's reason); the answer then takes its place: `←`/`→` (or `[`/`]`) step through earlier answers, `f` forks the shown one into a background agent that inherits the conversation plus that question and answer, with full tool access (you keep working; its report comes back as a message, as with any background agent), `x` clears the thread, and `esc` closes it. `f` and `x` wait until an answer has landed. Bare `/btw` reopens the overlay; `/btw fork [n]` forks from the command line.

## Configuration

`~/.chanto/config.toml` (global), `./.chanto/config.toml` (project), `./.chanto/config.local.toml` (personal). Run `chanto --example-config` for a template.

The Chanto directory (`~/.chanto` throughout these docs: config, sessions, logs, memory) is `$CHANTO_HOME` when set. Otherwise an existing `~/.chanto` is used; if there is none and `XDG_CONFIG_HOME` is set (to an absolute path), Chanto uses `$XDG_CONFIG_HOME/chanto` instead.

```toml
provider = "ollama"
model = "qwen3-coder:30b"
num_ctx = 32768
show_thinking = true                # false leaves streamed reasoning out of the TUI
tool_protocol = "native"            # native (falls back to text when needed) | text (always use the text tool-call protocol)
apply_patch_tool = false            # offer the apply_patch tool to models (it's accepted when called either way)
primer = true                       # first turn: list the project's files and attach small files the request names
think = true                        # if reasoning starts repeating itself, that step is retried without thinking
effort = "high"                     # reasoning effort: low | medium | high | xhigh | max | ultra (default: the provider's). Also --effort, /effort, /think <level>
mode = "default"
input_border = "block"              # TUI frames: block (default; thin, can't leave gaps) | line (rounded outline) | solid
theme = "auto"                      # auto | dark | light | dracula | nord | solarized-dark | gruvbox | colorblind | ~/.chanto/themes/<name>.toml
screen_reader = false               # screen-reader mode: line-based output, no animation, a bell when a prompt waits (also --ax-screen-reader)
animations = true                   # TUI animation (spinner, live counters); false skips it (also --no-animations); unset follows macOS Reduce motion
notifications = "auto"              # long turns and waiting prompts: auto | osc9 | osc777 | system | bell | off
shell_snapshot = true               # Bash gets your shell's PATH, exported variables, aliases and functions (see below)
bash_diff = true                    # show files a Bash command changed as a diff (ctrl+o expands)
away_summary = true                 # one-line recap when you return to the TUI after 3+ minutes away
keep_awake = "working"              # no idle sleep while chanto works: working | never | always
notify_after_secs = 30              # a turn this long notifies when it ends (0: never)
classifier_model = "qwen3:4b"       # optional faster model for auto-mode checks
fallback_model = ["ollama/qwen3:8b"] # when the model is overloaded or unavailable (see Fallback models)
tool_search = "auto"                # defer MCP tool schemas behind ToolSearch: auto | auto:N | true | false
stream_idle_timeout = 120           # a started model response silent this many seconds fails and is retried (0: no limit)
mcp_tool_timeout = 600              # seconds an MCP tool call may take (a server's tool_timeout_sec wins; else MCP_TOOL_TIMEOUT, in ms)

[permissions]
allow = ["Bash(cargo test:*)", "Bash(npm run lint:*)"]
deny  = ["Bash(git push:*)"]
auto_mode_rules = ["Never modify files under migrations/"]
auto_deny_after_secs = 75           # auto mode: deny a prompt nobody answers after this long (0: wait)
additional_directories = ["../shared"] # directories outside the project treated as part of the workspace

[web]
search_backend = "auto"             # duckduckgo by default; brave/tavily/searxng with keys
search_min_gap_ms = 1500            # key-less searches are paced per engine; a blocked engine cools down (0 disables pacing)
# brave_api_key, tavily_api_key     # keys for those backends (or BRAVE_API_KEY, TAVILY_API_KEY)
# searxng_url = "http://localhost:8888"
fetch_max_chars = 30000             # page content WebFetch returns
condense_with_model = false         # true: the model condenses long pages against the prompt (slow on local models)

[browser]
headless = false
# chrome_path = "/path/to/chrome"    # default: found automatically
# port = 9222                       # attaches if something already listens there
# user_data_dir = "/path/to/profile" # keeps logins between runs

[embeddings]                        # SemanticSearch's embedding endpoint
provider = "ollama"                 # ollama (default) | openai
model = "nomic-embed-text"          # ollama: nomic-embed-text; openai: text-embedding-3-small
base_url = "http://localhost:11434" # override the endpoint (Ollama's /api/embed, or /v1/embeddings when provider = "openai")
api_key = "${OPENAI_API_KEY}"       # openai provider only
batch = 16                          # chunks embedded per request

[providers.lmstudio]                # use as model = "lmstudio/<model-id>"
kind = "openai"
base_url = "http://localhost:1234/v1"

[providers.claude]                  # model = "claude/claude-opus-5-5"
kind = "anthropic"                  # key from api_key or ANTHROPIC_API_KEY

[sandbox]
enabled = true                      # on by default: Bash writes limited to project + temp/caches
auto_allow = true                   # auto mode: sandboxed commands skip the classifier

[mcp_servers.github]
command = "github-mcp-server"
args = ["stdio"]

[mcp_servers.remote]                # streamable HTTP transport
url = "https://example.com/mcp"
headers = { Authorization = "Bearer ${MY_TOKEN}" }

[mcp_servers.linear]                # OAuth: log in with /mcp login linear
url = "https://mcp.linear.app/mcp"
# oauth = { client_id = "..." }     # only for servers without dynamic client registration

[[hooks.PostToolUse]]
matcher = "Edit|Write|MultiEdit"
command = "cargo fmt"
```

### Your shell setup in Bash

Bash commands run under `bash -c`, which reads no startup files. So that tools that work in your terminal work for the agent too (nvm, pyenv, asdf, Homebrew's `shellenv`, your aliases), Chanto takes a *shell snapshot* once per session, the first time a command runs: it starts your `$SHELL` (zsh or bash) as an interactive login shell, so it reads `~/.zprofile`/`~/.zshrc` or `~/.bash_profile`/`~/.bashrc` as a terminal would (a login bash whose profile doesn't mention `.bashrc` gets `~/.bashrc` too), and records what they set up in a private file (mode 600, in a new mode-700 directory under the temp dir) that every command sources first:

- exported variables your startup files added or changed. `PATH` is merged: directories only Chanto's own environment has (an activated virtualenv) stay in front, then your login shell's `PATH`. Left out: secret-named variables (the ones `[secrets] scrub_env` keeps from commands, unless `pass_env` lets them through), values that look like secrets, terminal and prompt state (`PS1`, `TERM`, `TMUX`, history settings, …), proxy variables, the ones Chanto sets for commands itself (`PAGER`, `NO_COLOR`, …), and ones the config's `env` (or Claude Code's settings `env`) sets, which win;
- functions, except completion, prompt and hook functions (names starting with `_`, `prompt_`, `precmd`, `preexec`, …) and ones that would replace builtins Chanto relies on (`cd`, `pwd`, `exit`, …). Each is defined through its own `eval`, so one bash can't parse (zsh-only syntax) is skipped; functions and aliases whose text looks like it holds a secret are left out too;
- aliases, with `expand_aliases` on.

The startup files run with no terminal, stdin from `/dev/null` and their output thrown away, so ones that print, fail or wait for input don't matter, and with `CHANTO_SHELL_SNAPSHOT=1` set, which they can test to skip slow work. If they take more than 5 seconds, Chanto warns once and commands run with Chanto's own environment. Commands still run under bash (not your shell) so quoting and the tool's behavior are the same for everyone; variables always carry over from zsh, and functions do when bash can parse them. fish isn't captured. The snapshot is sourced inside the command's process, so the sandbox applies to it, and `CLAUDE_ENV_FILE` files run after it, so they can build on it. Not in a background run's container, and not on Windows.

Turn it off with `shell_snapshot = false`, `CHANTO_SHELL_SNAPSHOT=0`, or Claude Code's `CLAUDE_BASH_NO_LOGIN=1`.

After a Bash command changes files in the project, its result shows `Changed N files: …` with each file's diff (collapsed; ctrl+o expands it), as an edit would. The model gets only the list of changed paths. Ignored files and build outputs (the same ones `/rewind` leaves out) aren't counted; binary files and files over 256 KB are listed without a diff. Only the main conversation's foreground commands in the session's project are tracked, and commands Chanto knows are read-only skip it. Turn it off with `bash_diff = false` (Claude Code's `bashEditDiffEnabled: false` maps to it) or `CHANTO_BASH_DIFF=0`.

### Fallback models

`fallback_model` (a spec, a comma-separated list or an array; `--fallback-model a,b` overrides it for one session) names models to switch to when the session's model can't serve a request. They're tried in order, at most three, and the session's own model is skipped if listed.

- **What switches:** an overloaded or failing server (HTTP 500, 502, 503, 504, 529, or an `overloaded` error), a model the server doesn't have (404, Ollama's "model not found"), and a server that isn't running (connection refused, typical of a local Ollama or LM Studio that was closed). Hosted providers retry 429/5xx a few times first; with a fallback waiting, Chanto adds just one retry of its own before switching.
- **What doesn't:** request errors (400, 413), authentication (401, 403), billing and quota errors, rate limits (429), timeouts, and failures after the response started streaming. These fail as usual.
- **How long:** a warning says which model took over. The fallback serves the rest of that turn, tool steps included, and subagents use the same chain. The next message tries the session's model first again. If a fallback fails the same way, the next entry takes over.
- **Smaller windows:** if the conversation has reached `compact_threshold` of the fallback's context window, it is compacted first (by the fallback, since the model it replaces is the one failing) instead of failing on the request.

**Stalled streams.** A response that starts and then goes silent for `stream_idle_timeout` seconds (default 120; 0 turns the limit off) fails with a message saying so. If nothing had been shown yet it's retried like a dropped connection; otherwise the turn ends with that error. The wait before the first byte isn't limited, since a local server can spend minutes reading a long prompt before it answers. Esc interrupts the wait either way.

### Customizing Chanto

Ask `/customize <request>` in a session, or run `chanto customize "<request>"` from the shell. The agent gets a guide to Chanto's extension formats, chooses a small extension, asks about user (`~/.chanto/`) versus project (`.chanto/`) scope when needed, writes it with normal approval, and validates it with `chanto check-extensions`. For example: `/customize add a /release-check command that reviews the changelog and runs tests in this project`.

`chanto check-extensions [-C <project>]` checks config and Markdown extensions and lists loaded commands, skills, agents, output styles and hooks. It does not execute hooks or connect servers. `/reload` refreshes commands, skills, agents, output styles, instructions and hooks without losing the conversation. Other configuration, provider/tool settings, MCP and LSP servers require a restart. Plugin Markdown components and hooks are refreshed by `/reload`. Project hooks and other risky settings still require folder trust.

For behavior that extensions cannot express, use `chanto customize --source "<request>"`. It uses the source checkout recorded at build time, or offers to clone the repository into `~/.local/share/chanto/src`. The checkout must be clean. It creates a `custom/<slug>-<id>` branch, enables verified mode, requires `cargo test` to pass, then asks before building release and installing into `${PREFIX:-$HOME/.local}/bin`. Installation removes the old binary before copying the new one, as `install.sh --from-source` does. Source edits use the same approval rules as other agent work; other repositories are not used as the checkout.

### Plugins

Chanto manages Claude Code-compatible plugins without requiring Claude Code:

```sh
chanto plugin marketplace add owner/repo
chanto plugin marketplace add https://example.com/team/plugins.git
chanto plugin marketplace add /path/to/local-marketplace
chanto plugin marketplace list
chanto plugin marketplace update [name]
chanto plugin marketplace remove <name>
chanto plugin list
chanto plugin install <plugin>@<marketplace> [--yes]
chanto plugin uninstall <plugin>
chanto plugin enable <plugin>
chanto plugin disable <plugin>
```

Marketplaces contain `.claude-plugin/marketplace.json` with `name`, `owner`, and
`plugins[]` entries (`name`, `source`, optional `description` and `version`). A
plugin source can be a path relative to the marketplace, a GitHub object
(`{"source":"github","repo":"owner/repo","ref":"tag"}`), or a git URL object
(`{"source":"url","url":"https://example.com/plugin.git"}`). Git marketplaces
update with `git pull --ff-only`; local marketplaces are copied snapshots (remove
and add again to refresh). Local copies reject symlinks, and relative plugin
sources must stay inside the marketplace.

Checkouts live in `~/.chanto/plugins/marketplaces/`, installed copies in
`~/.chanto/plugins/installed/`, and the install registry in
`~/.chanto/plugins/installed.json`. Installed copies retain their marketplace
manifest, so removing a marketplace does not uninstall its plugins. These
user-level installs load alongside existing Claude Code plugins; project
`enabledPlugins` overrides are read only for trusted folders. Use
`plugin@marketplace` to disambiguate uninstall, enable, or disable.

Install reports commands, skills, agents, hooks, MCP servers and LSP servers.
Hooks and server definitions can run code and require explicit confirmation,
and so do skills and commands with `` !`cmd` `` blocks, frontmatter `hooks` or
`allowed-tools` (the summary lists each one under "runs when invoked");
non-interactive installs require `--yes` when those components are present.
Plugins cloned from git are refused if they contain symlinks.
`/plugin` lists plugins and accepts the same subcommands, including interactive
confirmation. Changes apply after `/reload` for Markdown components and hooks;
restart for MCP/LSP connections. `/plugins` lists active plugins.

To undo source changes, commit or stash your work, switch to the previous branch (printed when customization starts), and run `./install.sh`; alternatively commit the customization, `git revert` that commit, and rebuild with `./install.sh`. Restart Chanto after installing.

### Profiles, output styles, API key helper

- **Profiles** (like Codex's `--profile`): define `[profiles.<name>]` sections in any config file and select one with `chanto --profile <name>` or `profile = "<name>"`. A profile is applied on top of every other layer. A profile in an untrusted project can't set risky keys.
  ```toml
  [profiles.local]
  model = "qwen3:8b"
  num_ctx = 32768
  [profiles.hosted]
  provider = "openai"
  base_url = "https://openrouter.ai/api/v1"
  model = "qwen/qwen3.5-9b"
  ```
- **Output styles** (like Claude Code's `/output-style`): `Concise` keeps replies short, result first; `Proactive` does the obvious follow-up work (callers, tests, docs) without asking, except for anything destructive or outward-facing; `Explanatory` adds short notes on why choices were made; and `Learning` leaves small `TODO(human)` pieces for you to write.
  - Add your own as Markdown files in `~/.chanto/output-styles/` or `.chanto/output-styles/`, with `name` and `description` front matter. Claude Code's `output-styles` folders are read too.
  - Switch with `/output-style <name>`, or set `output_style` in config (Claude Code's `outputStyle` is imported).
- **`api_key_helper`**: a command that prints the API key, run at startup when no key is set, for example `op read op://dev/openai/key`. Claude Code's `apiKeyHelper` is imported. Only user or trusted config can set it.

### MCP servers

- **Managing servers:** from the shell, like `claude mcp`:
  - `chanto mcp add <name> -- <command> [args]` adds a stdio server; `chanto mcp add <name> <url>` adds an HTTP one. Use `-e KEY=VALUE` for env vars and `-H 'Name: value'` for headers.
  - `-s local` (default, `./.chanto/config.local.toml`), `-s project` (`./.chanto/config.toml`, shared via git) or `-s user` (`~/.chanto/config.toml`).
  - `chanto mcp add-json <name> '<json>'` adds a server from a JSON entry in Claude Code's format (`{"type":"http","url":...,"headers":{...}}`, `{"command":...,"args":[...],"env":{...}}`, `oauth.clientId`/`callbackPort`; `--client-secret` reads `MCP_CLIENT_SECRET` or asks). Pass the server's object, not the `mcpServers` wrapper. A `url` must start with `http://` or `https://` (or a `${VAR}` that supplies it).
  - `chanto mcp add-from-claude-desktop` lists the servers in Claude Desktop's config (macOS `~/Library/Application Support/Claude/`, Windows `%APPDATA%\Claude\`, Linux `~/.config/Claude/`, and a Windows install from WSL) and imports the ones you pick (`--all`, or names, to skip the question). Names with characters other than letters, digits, `-` and `_` are reported and skipped; a name already in use gets a suffix (`server_1`).
  - `chanto mcp list`, `chanto mcp get <name>` and `chanto mcp remove <name>`. In a folder you haven't trusted, `list` still shows the servers its own config files define, marked "(untrusted folder: not loaded until you trust it)". Edits keep the rest of the file, comments included. Names must be letters, digits, `-` and `_`.
- **In a session:** `/mcp` opens the server panel at the bottom (or, in print/plain mode, prints the same list). Each server shows its state (connected, needs login, failed with the reason, disabled, blocked by policy, or a `.mcp.json` server pending approval); ↑/↓ moves, enter opens the server's actions (authenticate, reconnect, enable/disable, clear authentication), and esc closes. Like `/status`, it runs immediately even while a turn is running, it is never queued behind the model. A server that drops mid-session stops counting as connected: the panel shows it failed, and the tool call that hit it says so and names `/mcp reconnect <server>`. `/mcp reconnect <server>` reconnects one; `/mcp reconnect all` retries every server that failed or needs a login. `/mcp disable <server>|all` disconnects servers and `/mcp enable <server>|all` connects them again; the choice is remembered per project (`disabledMcpServers` in `~/.chanto/mcp-projects.json`, Claude Code keeps the same list in `~/.claude.json`, which Chanto also honors). The footer shows `mcp <connected>/<configured>` and turns amber when a server needs attention.
- **Tool approval:** MCP tools ask before running in default and accept-edits mode and go to the classifier in auto mode, even when a local server marks a tool read-only (`readOnlyHint`): the hint is the server's own word, and a read-only fetch can still send data out. In plan mode a tool marked read-only asks and the others are refused. An allow rule (`mcp__server__tool`, or `mcp__server` for all of a server's tools) lets a tool run without asking, and a tool marked read-only in plan mode too.
- **Project `.mcp.json` approval:** trusting the folder in Chanto approves all of its servers. Otherwise a server loads once approved, as in Claude Code: by `enabledMcpjsonServers` or `enableAllProjectMcpServers` in `~/.claude/settings.json`, managed settings, `~/.claude.json`'s entry for the project, or `/mcp enable <name>` (which records it in `~/.chanto/mcp-projects.json`); approvals in the project's own `.claude/settings*.json` count only once the folder is trusted in Claude Code. `disabledMcpjsonServers` from any of those rejects a server.
- **Organization policy:** `allowedMcpServers` and `deniedMcpServers` from managed settings and Claude Code settings are enforced, with Claude Code's matching: `serverName` (exact), `serverCommand` (exact command and arguments) and `serverUrl` (`*` wildcards; a host written in full matches only the default port). The denylist always wins and merges from every scope; the allowlist merges too unless managed settings set `allowManagedMcpServersOnly`. With an allowlist, a remote server must match a `serverUrl` entry and a stdio server a `serverCommand` entry (a `serverName` counts only when the list has none of that kind). An untrusted project's settings can add denylist entries but not allowlist ones. Blocked servers are listed in `/mcp` and never started.
- **Transports:** stdio (`command`), streamable HTTP (`url`) and the legacy HTTP+SSE transport (`url` plus `transport = "sse"`, or `"type": "sse"` in `.mcp.json`; `chanto mcp add -t sse`). An HTTP server that refuses the streamable POST with a 4xx is retried as SSE automatically.
- **Sampling:** a server can ask to run a prompt on your model, for example to summarize what it fetched. You see the prompt and choose **Allow** or **Deny** each time; allowed requests run on the session's current model with no tools, count toward your budget, and are limited to the server's `maxTokens` (at most 16k). Only text is supported. In print mode they're refused.
- **Elicitation:** a server can ask you for input mid-call. Each field of its form is asked in turn as a question, required fields first (pick an option, type a value, or skip optional fields). Then you see exactly what would be sent and choose **Send** or **Decline**. Esc cancels at any point. While a question is open, the tool call waiting on it gets up to an hour instead of its usual limit. In print mode there's nobody to ask, so the request is declined.
- **OAuth:** a remote server that answers 401 shows as "needs login" in `/mcp`; startup isn't blocked. To log in:
  - Run `/mcp login <name>`. It opens your browser, waits for the redirect back to a local callback, then connects the server immediately.
  - Discovery follows the MCP authorization spec (protected-resource and authorization-server metadata), with dynamic client registration, PKCE and the `resource` parameter.
  - Tokens are stored in `~/.chanto/mcp-auth/` (mode 0600) and refreshed automatically. `/mcp logout <name>` deletes them.
- **Resources:** when a connected server offers resources, the model gets `ListMcpResources` and `ReadMcpResource` tools. You can attach one to a prompt with `@server:uri`, e.g. `@docs:file:///guide.md`.
- **Prompts:** server prompts become slash commands named `/mcp__<server>__<prompt> [args]`. Arguments fill the prompt's declared parameters in order. They're listed in `/help` and slash completion.
- **Updates:** when a stdio server reports that its tool list changed, the list is refreshed before the next model call.
- **Environment variables:** `${VAR}` and `${VAR:-default}` are expanded in `command`, `args`, `env`, `cwd`, `url` and `headers`, in Chanto's config and in imported `.mcp.json` / Codex servers. `${VAR:-default}` uses the default when `VAR` is unset or empty. A server that references an unset variable with no default is skipped with a warning naming the variable; the literal `${VAR}` is never sent. (Claude Code instead starts the server with the text unexpanded.)
- **Secrets:** stdio servers don't inherit secret-looking variables (`*_TOKEN`, `*_API_KEY`, ...) from Chanto's environment, except ones the server's config references as `${VAR}` or lists in `env_vars = ["GITHUB_TOKEN"]`, and ones set in its `env`.
- **Tool search:** when MCP tool definitions would take 10% or more of the context window, Chanto stops sending their schemas. The model sees each tool's name and a one-line hint in a `<system-reminder>` attached to the next message, and loads full definitions with the `ToolSearch` tool: `select:A,B` for exact names, keywords (`github issue`) for the best matches (`max_results`, default 5), or `+slack send` to require `slack` in the name. Loaded tools are added to the end of the tool list and stay callable for the rest of the session. Earlier tool definitions and messages are never rewritten, so a local server's prompt cache stays valid. Newly connected servers' tools are announced the same way. With the text tool-call protocol, the definitions come only in the `ToolSearch` result, so the system prompt doesn't change.
  - `tool_search = "auto"` (default), `"auto:N"` (defer at N% of the context), `"true"` (always defer) or `"false"` (always send every schema). The `ENABLE_TOOL_SEARCH` environment variable (or `env` in settings) takes the same values and overrides the config.
  - `always_load = true` on a server (`alwaysLoad` in `.mcp.json`) keeps its tools upfront; `always_load = false` defers all of them. A tool the server marks `_meta["anthropic/alwaysLoad"]: true` loads upfront unless its server sets `always_load = false`.
  - A deny rule for `ToolSearch` turns tool search off.
- **Per-server options:** `cwd` (working directory of a stdio server), `startup_timeout_sec` (how long a server may take to connect; default 30, 10 for imported servers. The interactive TUI and REPL don't wait for servers at launch: `/mcp` shows them as connecting, and the first request waits for any still connecting, so it has their tools. A run with a prompt, such as `-p`, waits at launch), `tool_timeout_sec` (how long one tool call may take; default `mcp_tool_timeout` from the top level of the config, else `MCP_TOOL_TIMEOUT` in milliseconds as in Claude Code, else 10 minutes. A call that runs out comes back to the model as an error naming the server and tool; esc stops the wait at any time. Either way the server is sent `notifications/cancelled` for the request, over stdio, streamable HTTP or legacy SSE), `enabled_tools = [...]` (only expose these) and `disabled_tools = [...]` (hide these; applied after `enabled_tools`). Names are the server's own tool names, without the `mcp__server__` prefix.

## Outcome mode

Outcome mode reviews what was built, not the code. `/outcome <what you want>` (or `chanto -p --outcome "…"`):
1. **Criteria:** the agent explores read-only, then writes executable acceptance criteria to `.chanto/outcome/<id>/criteria.json`, e.g. "signup rejects duplicate emails: `pytest .chanto/outcome/<id>/checks/test_signup.py -k duplicate`". It can only write there in this phase.
2. **Baseline:** every criterion runs before any change. One that already passes is flagged, because it proves nothing about the work.
3. **Approval:** you approve the criteria (plain-language titles plus commands), not code.
4. **Build:** verified mode with the criteria as checks. The criteria files are locked: loosening them fails the gate, and existing tests stay protected.
5. **Report** (`report.md`): each criterion with its evidence (command output), plus **"Review these by hand"**. Those are changes that passing checks can't vouch for: auth and security code, schema migrations, dependency changes, secrets, CI and infrastructure, public API surface, deleted files, and risky constructs (shell or eval, destructive data operations, concurrency, injection-prone queries).

## Goals

A goal keeps going until it's verifiably done. `/goal <objective>` (or `chanto goal "<objective>"`) works toward an objective over as many turns as it takes, not just one:

1. **Criteria:** the same first steps as [outcome mode](#outcome-mode). The agent explores read-only and writes executable acceptance criteria, they run once as a baseline, and you approve them. From then on they're **locked**: Chanto runs the copy recorded when you approved them, and if the agent edits the criteria files, Chanto puts them back and tells it so. That iteration doesn't count as progress.
   - **How criteria run.** Every criterion runs the same way: in the baseline, after every iteration, and in the instructions the agent gets. Its `check` is a shell command run **from the project root** (a background goal's worktree). `CHANTO_GOAL_DIR` (the goal directory) and `CHANTO_PROJECT_ROOT` are set, and the project root is on `PYTHONPATH`, so check scripts are written as `python3 "$CHANTO_GOAL_DIR/checks/test_x.py"` and can `import` project modules. The agent is shown the exact line to reproduce a run (`cd <root> && CHANTO_GOAL_DIR=… <check>`).
   - **Checks that can't run.** The baseline tells "fails because the behavior is missing" apart from "fails because the check can't run". The second covers: a `checks/...` path that names the goal directory relative to the project, a command that isn't installed, an import of a project module that fails, a syntax error in a check script, and a timeout. Those criteria are flagged `⚠ can't run` at approval, and you can let the agent fix them before anything is locked. Unattended, the agent gets two fix rounds, and if the criteria still can't run the goal stops and says why.
2. **Iterations:** each iteration is one agent turn. The agent plans the next step for the failing criteria, does it and runs the checks. Then Chanto runs every criterion itself and appends the result and the agent's summary to the goal's progress log. The next iteration starts from that log and the latest failure output. Existing tests are protected as in verified mode: weakening one doesn't count either.
3. **Done** when every criterion passes and no project check (verified mode's `checks`, or the detected ones) that passed when the work began fails now. A check that already failed then, or can't run in this project, doesn't hold the goal back.

It stops early, with a report saying why, when:
- it hits the **iteration limit**;
- it hits **no progress**: several iterations in a row without a new criterion passing (the report lists what still fails);
- it hits the **budget** or **time limit**;
- you **pause** or **stop** it;
- it's **blocked**: the agent ends a turn with `BLOCKED: <what it needs>` (a credential, a decision, access). Interactively, Chanto asks you, and your answer goes into the next iteration. Unattended, the goal stops as `blocked`.
- a **criteria problem** comes up: after an iteration a locked criterion fails because it can't run, or the agent ends with `CRITERIA PROBLEM: <id and why>`. Then the project may already be right and only the criteria are wrong, so Chanto doesn't keep iterating. Interactively it offers to unlock the criteria: the agent fixes them, you approve them again, and they're locked again. You can also keep going or stop. Unattended, the goal stops as `blocked`, and `/goal resume` starts with that fix-and-approve step.

The status line shows `◎ goal 2/5 · iteration 3` (criteria passing / total). `/goal status` shows the objective and each criterion. `/goal pause` stops after the current iteration, and `/goal stop` stops at once (both work while it runs). `/goal resume [id]` continues the newest unfinished goal, or the one you name. `/goal list` lists this project's goals. From another terminal, run `chanto goal pause|stop [id]`. Esc interrupts and pauses the goal. Limits count from the start or from the last resume.

**Where its state lives.** `~/.chanto/goals/<id>/` holds `goal.json` (state, the locked criteria, counters), `criteria.json` and its check scripts, `progress.md` and `report.md`. They live outside the project, unlike outcome mode's `.chanto/outcome/`. That keeps the bookkeeping out of `git status` and out of a background run's commits, it survives `git clean` and worktree removal, and the agent can't edit its own criteria without a permission prompt. When the conversation is compacted, the objective, the criteria and the tail of the progress log are put back into the conversation. After a restart, `/goal resume` reloads everything from that directory.

**In the background.** `chanto goal --bg "<objective>"` (or `/goal --bg <objective>`) runs it as a [background run](#background-and-remote-runs) with the run id as the goal id. Approving the criteria is a needs-input question, answered with `chanto attach` or `/runs attach`. The run's own budget and wall-clock limit apply too. `chanto runs resume <id>` continues an interrupted goal run from its goal directory. A [scheduled job](#scheduled-and-triggered-jobs) with `kind = "goal"` starts goal runs the same way.

```toml
[goal]
max_iterations = 20          # work iterations (also `chanto goal --max-iterations N`)
max_stale_iterations = 3     # in a row without a new passing criterion
# max_cost_usd = 5.0         # default: no goal budget (the session budget still applies)
max_minutes = 120
```

## Verified mode (guardrails)

`chanto --verify` (or `[guardrails] enabled = true`) stops the agent from declaring victory until the evidence agrees:

- **Done-gate:** when the model gives its final answer after changing the project, Chanto runs the project's checks. They're auto-detected (`cargo test`, `go vet && go test`, `npm test`, `pytest` through the first of `python3` and `python` that has it, else `unittest`, `make test`) or set with `checks = [...]`. By default it runs only the tests related to the change (see below). Failures go back to the model, up to `max_attempts` (default 3). An answer that changed nothing is checked once, with the full suite, so "done" without doing the work doesn't slip through.
- **Test protection:** existing test files are snapshotted when the turn starts. Deleting them, removing or changing assertions, changing an existing test's body, or adding skip markers (`@skip`, `it.skip`, `#[ignore]`, `t.Skip`, …) is reported as soon as it happens and then fails the gate with "fix the code, not the tests". New tests are fine. Outside `--verify` the same check applies without a gate: the agent is told to restore the test and fix the code, and a finish that still stands on the change is sent back. If the requested behaviour genuinely conflicts with an existing test, the agent keeps the code, leaves the test and explains the conflict.
- **Finishing on a failed run:** outside `--verify`, a final answer after the agent's own last check failed is sent back with the failure output (twice at most), and a failure that was only re-run at a narrower scope (a full suite that failed, then one file that passed) is sent back as unverified unless a later passing run covers the same tests.
- **No redundant runs:** if the agent's own last run of the same checks passed with no edits since, that run is the evidence and isn't repeated. Only a plain run counts: not piped (`| tail` hides failures), not filtered to some tests (`-k`, a test name, `--collect-only`), and covering at least what the gate would run. A run of more files or packages than the related selection counts (`pytest tests/` covers `pytest tests/test_a.py`); a run of fewer doesn't.
- **Baseline:** failures are reported as new or already failing; if only already-failing ones remain after one push, the turn ends with **⚠ Not verified** naming them instead of retrying. When the gate runs the full suite, the baseline is recorded before any change, alongside the first model call. When it runs related tests, the baseline is taken only if they fail: the same selection runs once more with the files changed this turn temporarily put back as they were (then restored), so passing turns pay nothing for it. The changed versions are saved under `~/.chanto/history/putback/` while that run is going; if Chanto is killed meanwhile (even with `kill -9` or by closing the terminal), a small watcher process puts them back right away; failing that, the next start in that project restores them and says so.
- **Evidence:** the final answer ends with what passed and how much of the suite that was, e.g. `_Verified: `pytest -q -x tests/test_x.py tests/test_y.py` (3s) passed, related to your changes; full suite not run._`, or `_Verified: `cargo test --quiet` (40s) passed (full suite: `Cargo.toml` changed)._`, or an explicit **⚠ Not verified** with the failing output. Checks that find no tests to run verify nothing, so that's **⚠ Not verified** too, with a pointer to `checks = [...]`.
- **Exit code:** with `-p`, a run that ends **⚠ Not verified** exits 3 (0 = verified or nothing to verify, 1 = error or interrupted), so scripts and CI can tell them apart.
- **Tests that don't finish:** a check that hits its time limit (`timeout_secs` under `[guardrails]`, default 600) ends the turn **⚠ Not verified**, without a retry, a full-suite fallback or a baseline run, since each would hang the same way. If the agent's own run of the same tests just timed out, with no edits since, the gate doesn't run them again.
- **Your instructions come first:** if a check fails in code you told the agent to leave alone, it leaves it and says so; the run then ends **⚠ Not verified**.

**Related tests.** From the files changed this turn (by edits, shell commands or subagents), each detected test command is narrowed:

| Runner | Related selection |
|---|---|
| pytest | changed test files; test files named after a changed module (`test_<mod>.py`, `<mod>_test.py`, under a matching package directory when nested); test files that import it (`import pkg.mod`, `from pkg.mod import …`, `from pkg import mod`), up to 50 files: `pytest -q -x <files>` |
| unittest (`discover`) | the same files as modules: `python3 -m unittest -q tests.test_mod …` |
| cargo | in a workspace, the crates holding changed files plus the workspace crates that depend on them: `cargo test -p a -p b`; a single crate runs its whole suite |
| go | the changed packages with their subpackages, plus packages that import them: `go vet ./pkg/x/... ./pkg/y && go test ./pkg/x/... ./pkg/y` |
| jest / vitest (from `npm test`'s script) | `npx jest --findRelatedTests <files>` / `npx vitest related --run <files>` |

A new file nothing imports (a reproduction script) doesn't widen the selection. Other checks (`make test`, `npm run typecheck`, your own commands) always run in full.

**When it runs the full suite instead:** shared test or build setup changed (`conftest.py`, `pytest.ini`, `pyproject.toml`, `setup.py`/`setup.cfg`, `tox.ini`, `requirements*.txt`, `Cargo.toml`/`Cargo.lock`, `build.rs`, `go.mod`/`go.sum`, `package.json` and lockfiles, jest/vitest/tsconfig config, `Makefile`, CI files); more than 20 files changed; a changed file has no related tests or isn't the runner's language; the changed files can't be determined; or the related run itself can't run (a path or package it names isn't found, no tests collected). It also just runs everything when that's cheap: a project with 20 or fewer test files, or a full suite this session already measured at 30s or less. The evidence line says which, and why.

**Forcing the full suite:** `chanto --verify-full`, `scope = "full"` in config, or `/verify full` for the rest of the session (`/verify related` switches back; `/verify` shows the current setting).

```toml
[guardrails]
enabled = true
checks = ["cargo clippy -- -D warnings", "cargo test"]   # default: detected
scope = "related"     # or "full"
max_attempts = 3
protect_tests = true
oracle = false        # true: the oracle reviews the diff once before the checks run
```

Project-level `[guardrails]` runs commands, so it only applies in trusted folders.

Checks (configured or detected), goal and outcome criteria, team checks and formatters run the way the agent's Bash commands do: in the [sandbox](#sandbox) when it's on, with the same writable places (the project, temporary directories, tool caches), and without secret-named environment variables. A check that needs a token gets it through `[secrets] pass_env`. Under a network allowlist or `network = "deny"`, a check that needs the network fails as a Bash command would. Language servers start without secret-named variables too. Detecting pytest never imports code from the project.

**The oracle.** `oracle` is a built-in subagent type, next to `Explore` and `general-purpose`: a read-only reviewer (Read, Grep, Glob, read-only Bash, web) prompted to find bugs, wrong assumptions and broken callers in a plan or diff, and to answer `LGTM` when there's nothing to fix. It runs on `oracle_model` (top-level setting, e.g. a stronger hosted model), else the main model. The model can consult it any time through the Task tool (`subagent_type: "oracle"`). With `oracle = true` under `[guardrails]` and verified mode on, when the model first gives its final answer after changing files, the oracle reviews that turn's diff; its findings go back to the model once (it fixes what's real or says why not), and then the done-gate runs as usual. An `LGTM`, a failed review or an empty diff goes straight to the gate. A custom agent named `oracle` replaces the built-in.

## Learning from your corrections

Chanto can improve over time without quietly drifting:
- **Signals:** it records your own corrections locally in `~/.chanto/learned/` (outside the repo):
  - denials with feedback, and messages like "no, use X"
  - edits you make to files it just wrote
  - rewinds, `!commands` you run by hand, and done-gate failures

  Repo content and tool output never count, so a planted file can't "teach" it anything.
- **Proposals:** `/learned review` condenses signals into short lessons, each citing its evidence. Commands you keep running by hand (`!cargo clippy` three times) become proposed verification checks.
- **Approval:** nothing applies until you `/learned accept <id>`. Lessons about permissions, or ones that skip or weaken tests or checks, are refused outright.
- **Visibility:** accepted rules become feedback memories, indexed on the first turn within the memory budget. Accepted checks join verified mode. `/learned` lists everything; `disable`, `reject`, `undo` and `add` manage it.
- **Measurability:** `--no-lessons` runs without lessons, so a benchmark can compare with and without them before you trust them.

Turn it off with `[learning] enabled = false`.

## Usage insights

`/insights` shows where the current session's tokens and time went, ranked by what a change would save, each with a concrete fix. `chanto insights` does the same for saved sessions in this directory: the last 10 by default, `--last N`, or `--session <id>`.

It flags:
- **Context bloat:** single tool outputs over 10% of the context, large files read whole when only a small part was edited, files read again without changing in between, and commands with long output.
- **Prompt-cache breaks:** calls where the start of the prompt changed (system prompt, tool list, cleared or edited history), so a local model had to re-process its whole context. Each break is attributed to its cause.
- **Wasted turns:** the same failing command rerun with no edits in between, loop-guard hits, verification retries, empty or truncated responses, and compactions.

Model time (including prompt processing), tool time and token counts are recorded per session and saved with it. When one tool output takes over a quarter of the context, Chanto shows a one-time tip. Token savings are estimates.

## Secrets

Secrets are kept away from the model, transcripts and model-written commands by default. There are three layers.

1. **Masking known values.** At startup Chanto collects real secret values from three places: secret-named environment variables (`*_TOKEN`, `*SECRET*`, `*PASSWORD*`, `*API_KEY*` and similar, plus a random-looking value in any other `*_KEY`; `*PUBLIC_KEY` and names like `CACHE_KEY` and `SORT_KEY` don't count), its own API keys, and the project's `.env`, `.env.*` and `.dev.vars` files. Example files like `.env.example` are skipped, and changed files are re-read each turn. Sign-in tokens Chanto gets while running (`chanto login` OAuth tokens, MCP servers' OAuth tokens) are added as they're used, and a background run also masks the values in its source checkout's env files, which its worktree doesn't have. Those exact values, plus their base64 and URL-encoded forms, are replaced with `[REDACTED:NAME]` in everything sent to the model (every request: turns, compaction summaries, hook and classifier checks and other side calls) and in saved sessions. That covers tool output, file contents, your own messages and tool-call arguments. What leaves the session is masked too: `--output-format stream-json` events, the web UI and `/remote` (events, permission prompts and questions, transcripts, checkpoints and the message queue), background runs' event logs, and what a runner sends its server (including a run's script output). Streamed answer text is masked one piece at a time, so a known value the model writes split across two pieces could show; the model only ever sees masked values, so it can't repeat one it wasn't shown. If you paste a secret into a message, Chanto warns you that it was masked.
2. **Masking known formats.** Common token formats are masked even when Chanto doesn't know the value:
   - AWS, GitHub, GitLab, Slack, Stripe, OpenAI, Anthropic, Google, npm, PyPI, Hugging Face, SendGrid and DigitalOcean tokens;
   - private keys and JWTs;
   - passwords in URLs and bearer tokens;
   - random-looking values assigned to secret-named keys.
3. **Prevention:**
   - **Secret files need your approval.** Reading or editing `.env` files, keys and certificates (`*.pem`, `*.key`, `id_rsa`), `~/.ssh`, `~/.aws`, `~/.netrc`, `~/.docker/config.json`, `~/.kube/config` and similar asks first in every mode except bypass, and so does a shell command that mentions one.
   - **Secret variables are withheld from commands.** Secret-named environment variables aren't passed to shell commands the model runs (any `*_KEY` counts here, whatever its value; list one in `pass_env` if a tool needs it). On Linux, Chanto also marks its own process non-dumpable, so a command can't read them back from `/proc/<chanto's pid>/environ` or attach to Chanto. This also means Chanto leaves no core dump, and `gdb -p`, `strace -p` and similar tools can't attach to a running Chanto without root.

```toml
[secrets]
redact = true          # layers 1 and 2
protect_files = true   # ask before secret files are read or changed
scrub_env = true       # withhold secret-named variables from shell commands
pass_env = ["GH_TOKEN", "AWS_*"]   # ...except these (names or prefix*), if your tools need them
```

To let the agent read one specific file without asking, add an allow rule such as `Read(./.env.test)`. Project config can't weaken these settings unless you trust the project.

## Resource limits

Agents can't take the machine down:
- **Concurrency:** at most 6 subagents (`max_parallel_agents`) and 4 shell commands (`max_parallel_commands`) run at once across all agents; the rest wait their turn. At most 8 background agents can be alive (`max_background_agents`), and by default subagents can't start subagents; `max_agent_depth` 2 or 3 allows it, with at most 20 nested agents at once.
- **Memory-aware starts:** new subagents and commands wait while free memory is below `min_free_memory_mb` (default 5% of RAM, at least 1 GB), then fail with a message telling the model to work sequentially. On macOS this uses the kernel's memory-pressure figure, not resident size.
- **Watchdog:** if Chanto's child processes use more than `memory_limit_percent` of RAM (default 70%), or the system is nearly out of memory, Chanto kills the largest one's process group. That's the runaway build or test, never Chanto itself, and the model is told why. On macOS it measures physical footprint, so compressed and swapped memory count.
- **Bounded output:** command output kept in memory is capped (head and tail), so a command that floods output can't exhaust RAM.
- **Disk space:** below 2 GB free on the project's volume the model is told once and asked to clean up; below 256 MB, writes and commands that would add data are refused with the free space in the error (cleanup commands such as `rm`, `cargo clean` and `git clean` still run), rather than failing halfway through a write on a full disk.

```toml
[resources]
max_parallel_agents = 6
max_agent_depth = 1                 # 2 or 3 lets subagents start their own, at most 20 of those at once
max_parallel_commands = 4
max_background_agents = 8
# min_free_memory_mb = 2048
memory_limit_percent = 70        # 0 = watchdog off
```

## Computer use

The `Computer` tool lets the model work in any desktop app, not just web pages: it takes screenshots, clicks, drags, scrolls, types, presses keys and opens or focuses apps. It's off by default. Turn it on for one session with `chanto --computer`, or always with:

```toml
[computer]
enabled = true
# background = false   # also offer it to background runs (off: runs never get it)
# max_width = 1280     # screenshot size the model sees (aspect kept)
# max_height = 800
```

**Actions.** `screenshot`; `left_click`, `right_click`, `middle_click`, `double_click`, `triple_click` and `mouse_move` at `coordinate: [x, y]`; `left_click_drag` from `start_coordinate` to `coordinate`; `scroll` (`scroll_direction`, `scroll_amount` wheel clicks, optional `coordinate`); `type` (`text`); `key` (`text`: `Return`, `Escape`, `cmd+shift+t`, `ctrl+a`, `F5`, …); `open_app` and `focus_app` (`app`); `list_windows`; `cursor_position`; `wait`. Coordinates are pixels in the latest screenshot. Screenshots of the main display are scaled to fit 1280x800, and Chanto maps them to screen points. On a Retina display the raw capture has twice as many pixels as points, and the mapping accounts for that. Screenshots need a vision model.

**Platforms.** On macOS it uses only built-in tools: `screencapture` and `sips` for screenshots, and `osascript` running JavaScript for Automation for input. Its ObjC bridge posts CoreGraphics mouse and scroll events, and System Events types text and presses keys. That means no compiler (a Swift helper would need the Xcode tools), no download (`cliclick`) and no new dependencies. Each action is one short-lived process. Linux works on X11 with `xdotool` and ImageMagick's `import` installed (`open_app` goes through `gtk-launch`).

On Wayland, which keeps apps from reading the screen or faking input, it uses the tools each desktop allows:
- **Screenshots:** `grim` on wlroots compositors (Sway, Hyprland, river), `gnome-screenshot` on GNOME, `spectacle` on KDE; the last two need ImageMagick to resize.
- **Mouse and keyboard:** `ydotool`, which works on any compositor through `/dev/uinput`; its daemon `ydotoold` must be running. ydotool moves the pointer with relative motion, so turn off pointer acceleration for clicks to land where intended. On scaled (HiDPI) displays, clicks can be off by the scale factor.
- **Windows:** `list_windows` and `focus_app` work on Sway and Hyprland. Other compositors don't let apps list or focus windows, and the cursor position can't be read on any; the tool says so.

Windows isn't supported yet.

**macOS permissions.** The app Chanto runs in (Terminal, iTerm, your editor) needs **Screen & System Audio Recording** to take screenshots and **Accessibility** to click and type. Typing and keys also need **Automation** permission to control System Events, which macOS asks for the first time. When a permission is missing, the tool returns an error that names the setting and the app. You turn it on in System Settings → Privacy & Security, then reopen the app.

**Safety.**
- Every action asks for approval in the default and acceptEdits modes, screenshots included, since they send your screen to the model provider. "Yes" applies to that action only, and allow rules don't skip the prompt.
- In auto mode, screenshots and other look-only actions go ahead, and everything else goes to the auto-mode classifier. Plan mode only allows looking, and bypass mode allows everything.
- Background runs never get the tool unless `[computer] background = true`. Then each action becomes a `needs-input` prompt in acceptEdits.
- While computer use is on, the TUI status line shows **◉ computer use on** (**active** right after an action), and the session starts with a warning.
- `[computer]` is ignored in projects you haven't trusted.

**Risks.** Computer use acts with your login: the model can click, type and see anything on your screen, in any app and any signed-in account. Anything on screen can contain text written to hijack the model: a web page, an email, a document or a notification. This is prompt injection, and it can make the model try things you didn't ask for, like sending messages, changing settings or copying data somewhere. The approval prompt is your defense, so read each action before you allow it. Close windows with sensitive content first, and don't use auto or bypass mode with computer use unless the machine is a disposable VM. The tool tells the model to treat screen content as untrusted, but that doesn't make injection impossible.

## Background agents and worktrees

The model can run subagents in the background (`Task` with `run_in_background: true`) and keep working. Each report is delivered when that agent finishes: mid-turn, or by starting a new turn if the model is idle. With `-p`, Chanto waits for background agents before exiting.

`isolation: "worktree"` gives a subagent its own git worktree and branch (`.chanto/worktrees/<id>`, branch `chanto/<id>`), so several agents can edit in parallel without touching your checkout. The report says what changed on the branch, and a worktree with no changes is removed. `resume: "<id>"` sends a finished agent follow-up instructions, and it keeps its context. `/tasks` lists background agents and `/tasks stop <id>` cancels one.

## Background and remote runs

Hand a task off and keep working: a background run works by itself in its own git worktree and branch, survives closing the terminal, and waits for you if it needs a decision.

```bash
chanto run --bg "add retries to the HTTP client"   # returns at once with the run id
chanto runs                                         # id, state, duration, cost, verify result, branch, task
chanto attach 3f2a9c1d                              # follow it live, answer prompts; Ctrl-C detaches
chanto runs show 3f2a9c1d                           # summary, diff stat, last messages
chanto runs apply 3f2a9c1d                          # bring its commits onto your current branch
chanto runs apply 3f2a9c1d --pr                     # or push the branch and open a PR with gh
chanto runs stop 3f2a9c1d | resume 3f2a9c1d | rm 3f2a9c1d
```

In the TUI, start a prompt with `&` (or use `/bg <task>`); `/runs` lists runs, `/runs <id>` shows one, `/runs attach <id>` mirrors its messages into your session and shows its permission prompts as the usual dialog (`/runs detach` stops following). Ids can be shortened to a unique prefix. `-m/--model`, `--provider`, `--base-url`, `--mode`, `--max-cost` and `--no-verify` apply to the run.

**The daemon.** Runs are hosted by the Chanto server (`chanto serve`), started on demand in the background (detached from the terminal; log in `~/.chanto/daemon.log`) and found through `~/.chanto/server.json`, like any `chanto serve`. `chanto daemon status` shows it and its runs, `chanto daemon stop` stops it and `chanto daemon start` starts it. Each run is its own worker process, so runs in different repositories don't share state. Run records and their event logs live in `~/.chanto/runs/<id>/` (`run.json`, `events.jsonl`, `worker.log`), readable only by you (on Unix, `runs/` and its directories are mode 700, the files 600), with secrets masked in the event log. Stopping the daemon (or a crash) leaves active runs `interrupted`: their sessions are saved, and `chanto runs resume <id>` continues one from where it stopped. A server that listens beyond this machine (`--host`, `--tailscale`) hosts runs only with `--runs`.

**Isolation.** A run works in its own git worktree, `~/.chanto/runs/worktrees/<project>/<id>`, on a new branch `chanto/run/<id>` created from your current HEAD. The worktree is outside your project, so test runners, build globs, indexers and file watchers never see a second copy of it. It is trusted (project settings, hooks, MCP servers) exactly when the source repository is, and its own `.chanto/` settings stay protected from the agent. Your uncommitted changes (staged, unstaged and untracked files, minus ignored ones) come along as a first commit on that branch, "Uncommitted changes at the start of background run <id>", built with a throwaway git index (`git add -A` into a temporary `GIT_INDEX_FILE`, then `write-tree` and `commit-tree`), so your working copy, index and stash are never touched. When the run finishes, whatever it left uncommitted is committed to its branch. `chanto runs rm` removes the worktree and branch (and prunes git's worktree list). Directories that aren't in a git repository (or have no commits) are refused with a message saying so.

**Applying.** `chanto runs apply <id>` cherry-picks the run's own commits (everything after the snapshot) onto the branch you have checked out. If your working tree has uncommitted changes, it applies the run's diff to the working tree instead and leaves it uncommitted; if that doesn't apply cleanly, nothing changes and you're told to commit or stash first. A conflicting cherry-pick is aborted, leaving the exact `git cherry-pick` range to run by hand. `--pr` pushes the branch to `origin` and runs `gh pr create` (it only happens when you ask for it).

**Autonomy and limits.** Runs default to `acceptEdits` with shell commands in the OS sandbox: edits inside the worktree and commands that stay in the sandbox go ahead; anything else (leaving the sandbox, secret files, protected paths, dangerous commands) pauses the run in state `needs-input` until someone answers with `chanto attach`, `/runs attach` or the API. Prompts nobody answers are denied after `input_timeout_minutes`, so a run never waits forever. Verified mode is on (its result shows in `chanto runs` as passed, failed, skipped or off); the run stops at its cost budget or its wall-clock limit (time spent waiting for input doesn't count), and at most `max_parallel` runs work at once while the rest wait as `queued`.

```toml
[background]
mode = "acceptEdits"              # permission mode for runs
allow_sandboxed_commands = true   # acceptEdits: sandboxed commands run without asking
sandbox = true
verify = true                     # `--no-verify` per run
max_cost_usd = 2.0
max_minutes = 60
input_timeout_minutes = 1440
max_parallel = 2
# model = "qwen3-coder"           # default: your configured model
```

**Notifications.** When a run finishes, fails or needs input, Chanto can tell you:

```toml
[notify]
ntfy = "https://ntfy.sh/my-long-random-topic"   # plain HTTP POST; subscribe in the ntfy phone app
webhook = "https://example.com/chanto-hook"    # JSON POST: {event, run_id, state, title, message, url}
desktop = true                                   # macOS Notification Center / Linux notify-send; default on for the local daemon
link = "http://my-mac.tailnet-name.ts.net:4141" # where notification links point (default: the server)
```

A notification has only a title, the run id, its state, the verify verdict and a link to the run (`/#run=<id>`). It never includes the task, code, output or error messages, because those don't belong in notification services or on lock screens. Treat an ntfy topic name like a password, or use your own ntfy server.

**API.** The same server, behind its bearer token (see [Server](#server)), so schedulers and scripts can drive runs:

| Route | |
|---|---|
| `POST /runs` | `{task, cwd, model?, provider?, base_url?, mode?, verify?, sandbox?, max_cost_usd?, max_minutes?, start?, kind?, team?}` → the run (201). `source: {"kind": "local", "path": ...}` may replace `cwd`. `start`: a commit to begin at; `kind: "goal"`; `team`: the team it belongs to. |
| `GET /runs` | `{runs: [...]}`, newest first |
| `GET /runs/{id}` | the run plus `diff_stat`, `commits`, `last_messages`, `last_seq` |
| `GET /runs/{id}/events?since=<seq>` | server-sent events: the logged events after `since`, then live ones; ends once the run reaches a final state |
| `POST /runs/{id}/respond` | `{request_id?, decision: "allow" \| "allow_always" \| "deny", feedback?}` or `{request_id?, answer}` for a question |
| `POST /runs/{id}/stop`, `POST /runs/{id}/resume`, `DELETE /runs/{id}` | stop, continue an interrupted run, remove its worktree, branch and record |
| `POST /runs/{id}/apply` | `{pr?}`: apply a run of a local checkout onto that checkout's current branch, or push and open a PR |
| `POST /runs/bundles`, `GET /runs/{id}/bundle` | upload a git bundle as a run's source (`source: {kind: "bundle", bundle_id, ref, …}`; a `{kind: "git", url, ref}` source is fetched instead), and download a run's commits |

A run's `state` is `queued`, `running`, `needs-input`, `done`, `failed`, `cancelled` or `interrupted`; `pending` holds the open permission request or question. Events are the session events (`text`, `tool_start`, `permission_request`, …) with a `seq` and `run_id`, plus `run_state` (with the whole run record) on every state change and `request_done` when a prompt is answered or times out.

**Starting point.** `start` (API) makes a run begin at a given commit of the local checkout instead of HEAD plus your uncommitted changes. [Teams](#teams) use it to start every sub-task from the same base, and to start a re-delegated one from the merged work. `kind: "goal"` runs a [goal](#goals), and `team` tags a run as part of a team.

### Container isolation

By default a run is isolated by its own worktree and the OS sandbox (Seatbelt, Landlock, or a write-restricted token on Windows). With `isolate = "docker"` (or `"podman"`) its shell commands run in a container instead:

```toml
[background]
isolate = "docker"            # or "podman"; "none" (default) keeps the OS sandbox
image = "rust:1-bookworm"     # default: by project type (see below)
network = "none"              # none | allowlist | full
cpus = 2                      # optional limits
memory = "4g"
```

**How it works.** The agent loop stays on the host, and each run gets one long-lived container (`chanto-run-<id>`) with its worktree bind-mounted at the same path. Every command the model writes (`Bash`, background shells, and verified mode's checks) is a `docker exec` into it, so `cd`, paths in output and paths in error messages mean the same thing on both sides. Chanto's own file tools (Read, Edit, Write) work on the host worktree, and the bind mount makes their changes visible in the container. The alternative, running the whole worker inside the container, would need a Linux build of Chanto (not available on a macOS host) and would have to put your model credentials into the container. With this design they never enter it: a command sees only the image's environment plus `CHANTO=1`, the pager/color settings Chanto always sets and, with `allowlist`, its proxy variables. Nothing from your shell environment is passed, so no provider keys and no secret-named variables. Nothing else from the host is mounted: no home directory, no SSH keys, no Docker socket. The repository's `.git` directory is mounted read-only, so the container can't plant hooks or config that your own git would later run. Only the worktree's own git metadata (index, HEAD) is writable. Git commands that read work if the image has git, but `git add`/`git commit` inside the container fail on the read-only object store. The run's changes are committed on the host when it finishes, as usual. On Linux the container runs as your user (so files in the worktree stay yours, with `HOME` set to a scratch directory). With Docker Desktop on macOS it runs as root, and file ownership is mapped by Docker Desktop. Containers get `--pids-limit 4096` and `no-new-privileges`.

**Images.** Without `image`, Chanto picks one by project type: `rust:1-bookworm` (Cargo.toml), `golang:1-bookworm` (go.mod), `node:22-bookworm` (package.json), `python:3.12-bookworm` (pyproject.toml, requirements.txt, setup.py), `ruby:3-bookworm` (Gemfile), `eclipse-temurin:21-jdk` (Maven/Gradle), else `buildpack-deps:bookworm`. The engine pulls an image the first time it's used, which can take a while. These images are large, so set `image` to something smaller you already have if disk space matters.

**Network.** `none` (default): no network at all. `full`: the engine's default bridge. `allowlist`: the container sits on an internal network (`--internal`, no route out) whose only other member is a small relay container (`alpine:latest` running busybox `nc`). The relay forwards to Chanto's filtering proxy on the host, and commands get `HTTP(S)_PROXY` pointing at it, so only `[sandbox] allowed_domains` (default: the package registries) minus `denied_domains` are reachable. A blocked domain is offered for approval like any sandboxed command's. Tools that ignore proxy variables simply get no network. With Docker Desktop the relay reaches the host's loopback proxy through `host.docker.internal`. On Linux Chanto also serves the proxy on the bridge's gateway address.

**Permissions.** In `acceptEdits` (the default for runs), commands that run in the container go ahead without asking when `allow_sandboxed_commands = true`, except for the usual dangerous ones. `dangerouslyDisableSandbox` has no effect: the container is the boundary.

**Lifecycle.** The worker creates the container when the run starts (or resumes) and the daemon removes it, together with the relay and network, when the run ends, is stopped or is removed. On start, the daemon also removes containers that a crashed daemon left behind, which it finds by their `chanto.store` label. Run creation fails at once with a clear message if the engine isn't installed or isn't running ("start Docker Desktop"). `chanto runs show` shows the isolation in use (`Isolation: container (docker · rust:1-bookworm · network none; chanto-run-3f2a9c1d)`), and `docker ps --filter label=chanto.run` lists the live containers.

These settings live in `~/.chanto/config.toml`, or in a trusted project's `.chanto/config.toml` (`[background]` is ignored in projects you haven't trusted).

### Remote runs: your own server

Run tasks on a machine you control (a VM, a desktop with a GPU) and keep your laptop free:

```bash
# on the server
chanto serve --runs --tailscale          # or --host 0.0.0.0 behind your own TLS proxy
cat ~/.chanto/server.json                # url and token (or pin the token with CHANTO_SERVER_TOKEN)

# on your laptop
chanto remote add box http://box.tailnet-name.ts.net:4141 --token <token>   # saved in ~/.chanto/auth.json (0600)
chanto run --bg --on box "add retries to the HTTP client"
chanto runs --on box
chanto attach 3f2a9c1d --on box
chanto runs apply 3f2a9c1d --on box      # or --pr
chanto remote list | chanto remote rm box
```

All run commands take `--on <server>`. How the code gets there:

- **Git bundle (default).** Chanto bundles your HEAD plus a snapshot of uncommitted changes (the same snapshot commit as local runs) and uploads it (`POST /runs/bundles`, limit `[background] max_bundle_mb`, default 1024). Nothing is pushed anywhere.
- **Shared git remote.** With `[remote.box] git_remote = "origin"` and a clean working tree, Chanto pushes your current branch to that remote (a normal, non-force push) and the server fetches it from the same URL, so the server needs read access to it. With uncommitted changes or a detached HEAD, it falls back to a bundle.

The server fetches into a bare repository under its own `~/.chanto/runs/repos/`, adds the run's worktree under `~/.chanto/runs/worktrees/`, and runs it exactly like a local run. Code that came from another machine is never trusted there: its project settings, hooks and MCP servers are ignored, and the server's own `[background]` settings apply. `chanto runs apply --on` downloads the run's commits as a git bundle (`GET /runs/{id}/bundle`) into your checkout as the branch `chanto/run/<id>`, then applies them like a local run. The server's answer is checked first: the branch must be the run's own `chanto/run/<id>`, its base a full commit hash that your checkout has below the run's work, and an existing branch of that name is never moved (delete it to fetch the run again); `--pr` pushes that branch from your machine with your credentials and opens the PR with `gh`.

**Deploying on a VM.** Install Chanto, configure a model (for example `[providers]` pointing at a hosted API, or Ollama on the same box), and run it as a service:

```ini
# /etc/systemd/system/chanto.service
[Unit]
Description=chanto run server
After=network-online.target tailscaled.service
Wants=network-online.target

[Service]
User=chanto
Environment=CHANTO_SERVER_TOKEN=change-me-to-a-long-random-string
ExecStart=/usr/local/bin/chanto serve --runs --tailscale --port 4141
Restart=on-failure
# Runs need git, and the OS sandbox needs Landlock (Linux 5.13+).

[Install]
WantedBy=multi-user.target
```

`systemctl enable --now chanto`. Pin the token with `CHANTO_SERVER_TOKEN` so clients survive restarts. If the service restarts while runs are active, they come back `interrupted`; `chanto runs resume <id> --on box` continues them. Reach the server privately over Tailscale: `--tailscale` binds only to the tailnet address, so nothing outside your tailnet can connect, and the bearer token is a second layer. Use `--host 0.0.0.0` only behind a TLS-terminating proxy you trust.

**Stronger isolation.** Runs on a server can use [container isolation](#container-isolation) like local ones; for more, run the server itself in a VM.

### Queue runners: taking runs from Chanto cloud

`chanto runner connect` turns a machine into a runner for a run queue (Chanto cloud, or any server that implements the runner protocol in `proto/src/runs.rs`). The runner only makes outbound HTTPS requests, so it works behind a firewall.

```bash
chanto runner connect https://cloud.example --registration-token hrr_…   # once: registers, saves its token
chanto runner connect https://cloud.example                              # afterwards
chanto runner connect https://cloud.example --label gpu --label linux     # only runs that ask for these
CHANTO_CLOUD_URL=… CHANTO_RUNNER_REGISTRATION_TOKEN=… chanto runner connect --ephemeral   # in a throwaway VM
chanto runner connect https://cloud.example --own-model                  # always use this machine's model setup
chanto runner connect https://cloud.example --allow-scripts              # also run the scripts jobs come with
```

**Scripts.** A job can come with a shell script that runs before (or instead of) the model. It is the server's code, so a runner only runs it with `--allow-scripts` (`CHANTO_RUNNER_ALLOW_SCRIPTS=1`); without that, such a job is reported failed with a note saying so. Allowed scripts run like the agent's Bash commands: in the [sandbox](#sandbox) under the machine's `[sandbox]` settings, without its secret-named environment variables (the run's own org secrets are passed), in a fresh checkout whose git directory is kept under `~/.chanto/runner/pulled/scripts/`, out of the script's reach. A script that runs `git commit` itself therefore fails in the sandbox; its changes are committed for it.

**Credentials.** For each job the runner asks the server for the run's credentials. If the server sends a git credential (Chanto cloud does for connected GitHub, GitLab and Bitbucket repositories), the runner clones and pushes with it; otherwise it uses the machine's own git credentials. If the server offers a model endpoint (Chanto cloud's model gateway, or one the org set up), the run uses it; otherwise, or with `--own-model` (`CHANTO_RUNNER_OWN_MODEL=1`), it runs with the machine's Chanto configuration, so a local or private model's key never reaches the server. The org's secrets are passed to the run's shell commands as environment variables and masked like any other secret; names starting with `CHANTO_` are skipped. `--name` sets the name the runner registers with, and `--mode` the permission mode for its runs.

The registration token comes from the org's runner settings; the runner's own token is saved in `~/.chanto/runner/connections.json` (0600). Each job is cloned into a cached bare repository under `~/.chanto/runner/pulled/` and run as an ordinary background run of a remote source (untrusted, like [remote runs](#remote-runs-your-own-server)), with `[background]` settings, verified mode and container isolation as configured. Its events stream to the server as they happen. When it finishes, its branch `chanto/run/<id>` is pushed to the repository and reported with the run's summary, and the worktree is removed.

Nobody watches these runs, so permission prompts are denied with an explanation and questions are answered with "use your best judgment"; choose `[background] mode` (or `--mode`) with that in mind. The runner keeps its job with a heartbeat. If it stops (Ctrl-C, crash, lost network), the server gives the run to another runner once the lease runs out; a cancel from the server stops the run at the next heartbeat. `--ephemeral` takes one job and deregisters, for runners in VMs or containers that are thrown away afterwards; `--once` takes one job and exits.

### Cloud runs from the CLI

Sign in to [Chanto cloud](https://chanto.sh) with GitHub, then send a task from your terminal, follow it there, and get its branch back:

```bash
chanto cloud login                                      # browser sign-in; the token goes in ~/.chanto/auth.json (0600)
chanto cloud status                                     # who you are, your orgs, where runs go
chanto cloud org acme                                   # make acme the default org (--org picks one per command)
chanto run --cloud "add retries to the HTTP client"     # queue it, then follow it here; Ctrl-C detaches
chanto run --cloud --placement hosted --label gpu "…"   # placement auto | hosted | self-hosted; labels pick runners
chanto runs --cloud                                     # the org's recent runs (--status, --org, --limit)
chanto attach 0b9e6f1c --cloud                          # follow one again (ids can be shortened)
chanto runs show 0b9e6f1c --cloud | chanto runs cancel 0b9e6f1c --cloud
chanto cloud logout                                     # revokes the token on the server and forgets it
```

**From the agent.** Signed in, the agent also gets a `ChantoCloud` tool, so you can ask in plain words ("send this to the cloud", "how's my cloud run doing?", "run the docs agent on issue 42"). It lists runs and agents and shows a run's result freely; starting a run (of the current branch, which must already be pushed), cancelling one or starting an agent asks you first, like any other action with outside effects. Signed out, the tool isn't offered.

**What the run starts from.** A runner clones your repository from its `origin` remote (an SSH remote is sent as its HTTPS URL, and credentials in a remote URL are never sent), on the branch you have checked out. Work it can't see is refused rather than silently left out: with uncommitted changes Chanto asks you to commit or stash them, and with commits that aren't on `origin` (or a branch that was never pushed) it asks you to push. `--push` pushes the branch for you (a normal, non-force `git push -u origin <branch>`) when the working tree is clean. A detached HEAD is refused too.

**Following.** `chanto run --cloud` prints the run's id and follows it: the model's text, its tool calls and failures as they reach the server, whether it's waiting for a runner or running on a hosted or self-hosted one, and finally its summary. Cloud runs are unattended, so permission prompts are denied and questions answered by the runner (see above); you see them in the stream. `--detach` returns as soon as the run is queued. The exit code is 0 when the run succeeded.

**Getting the result.** When the run succeeds, its runner pushes the branch to the same repository, and Chanto prints how to fetch it, plus a link to open a pull request for GitHub repositories:

```bash
git fetch origin chanto/run/<id> && git switch chanto/run/<id>
```

**Errors.** What the server refuses is shown as it says it, with what to do: a hosted run without a card on file or past the org's spending limit (`--placement self-hosted` keeps it on your runners), an expired or revoked sign-in (`chanto cloud login` again), an org you're not in. A run that no runner takes in time fails with "no runner took the run in time"; connect one with `chanto runner connect` or allow hosted runners.

In CI, set `CHANTO_CLOUD_URL` and `CHANTO_CLOUD_TOKEN` (an account token, such as the one `chanto cloud login` saves) instead of logging in, and `CHANTO_CLOUD_ORG` to pick the org.

**Staying signed in.** The dashboard's sign-in lasts a year and is pushed forward on every visit, so it only asks again after a long absence or once the token is revoked. The token lives in an `HttpOnly` cookie that page scripts can't read. **Devices** in the dashboard lists every browser and CLI sign-in on your account with when it signed in and when it was last used, and revoking one signs that device out; logging out of the browser revokes its token on the server too.

### Scheduled and triggered jobs

`chanto runner` starts agent tasks on a schedule, from a webhook, or on demand. Each one becomes an ordinary background run, with the same worktree isolation, `[background]` budgets and permission mode, verified mode, `needs-input` prompts, and notifications as `chanto run --bg`. Runs started by a job are marked with it (`[nightly] …` in `chanto runs` and "job nightly" in notification titles).

Jobs live in `.chanto/runner.toml` in a project, or in `~/.chanto/runner.toml` for jobs of your own:

```toml
[[job]]
name = "nightly-review"             # letters, digits, - _ .; unique within a file
schedule = "0 3 * * *"              # 5-field cron (local time), or an interval: 30m, 2h, 1d
prompt = "Review yesterday's commits and write up the risks in REVIEW.md."
verify = true                       # the rest default to [background]
max_minutes = 30
max_cost_usd = 1.0
# mode = "acceptEdits"  model = "…"  provider = "…"  base_url = "…"
# cwd = "services/api"              # project jobs: relative to the project (default: its root)
# enabled = false                   # pause the schedule; triggers still work
# kind = "goal"                     # work on the prompt as a goal (see Goals)

[[job]]
name = "triage"                     # no schedule: runs only when triggered
prompt = "Triage the newest failing CI run and propose a fix."
```

A job with `kind = "goal"` runs its prompt as a [goal](#goals): the first run waits as `needs-input` until you approve its criteria, and `chanto runs` and notifications label it as both the job and a goal run.

Jobs in `~/.chanto/runner.toml` need `cwd`, the absolute path of the checkout to run in. A project's jobs are scheduled once you run `chanto runner enable` in it; the project must be trusted, because its jobs run unattended. A project job overrides a global one with the same name. Unknown keys are errors, so a typo never silently drops a setting.

**When jobs run.** The scheduler lives in the Chanto daemon (the process that hosts background runs) and rereads the job files every 30 seconds, so edits apply without a restart. `chanto runner enable` starts the daemon; `chanto daemon start` does too. An interval job runs as soon as it is first seen and then every interval after it last started; a cron job waits for its next slot. A slot missed while the daemon was down runs once when it comes back. A job never overlaps itself: while its last run is still going (including waiting for input), it is skipped.

```bash
chanto runner                     # jobs: schedule, next run, last run and its state
chanto runner status              # plus whether the daemon is up, enabled projects, problems
chanto runner enable | disable    # schedule (or stop scheduling) this project's jobs
chanto runner trigger triage      # start a job now (--wait to wait for its result)
chanto runner logs nightly-review # the job's runs, and the latest in detail
```

In the TUI, `/schedule` lists the jobs and `/schedule run <job>` starts one.

**From cron, launchd, systemd or CI.** `chanto runner --once` asks the daemon (starting it if needed) to start whatever is due, including the current project's jobs, waits for those runs, prints how each ended, and exits non-zero if one failed (a run that needs input is reported and not waited for). `--job <name>` starts that job regardless of its schedule, `--dry-run` only reports, and `--no-wait` returns once the runs have started. To keep schedules running across reboots, start the daemon at login, for example with a crontab line `@reboot chanto daemon start`, a launchd agent, or a systemd user unit running `chanto daemon start`.

**Webhooks.** `POST /runs/jobs/<job>/trigger` starts a job. It accepts the daemon's bearer token, or a per-job token from `chanto runner token <job>` (`--rotate` replaces it), so a CI system, a Slack workflow or GitHub can trigger exactly that job and nothing else:

```bash
curl -X POST -H "Authorization: Bearer $JOB_TOKEN" "$CHANTO_URL/runs/jobs/triage/trigger"   # chanto runner token prints both
```

The job token is also accepted as an `X-Chanto-Token` header or a `?token=` query parameter, or as a GitHub webhook secret: GitHub signs each delivery (`X-Hub-Signature-256`) and Chanto checks the signature. A GitHub `ping` is acknowledged without starting a run. The reply is the run (201), or 409 while the job's last run is still going. Tokens are kept in `~/.chanto/runner/tokens.json` (owner-only). The local daemon listens on loopback only; for callers on other machines, put it behind a tunnel or reverse proxy, or use a run server (`chanto serve --runs`), whose own `~/.chanto/runner.toml` jobs it schedules.

Behind the bearer token, `GET /runs/jobs` lists the jobs with their next and last runs, and `POST /runs/jobs/due` `{job?, project?, dry_run?}` is what `--once` calls. Scheduler state (when each job last ran, and which run) is in `~/.chanto/runner/state.json`; the enabled projects are in `~/.chanto/runner/projects.json`.

## Chanto cloud

> Chanto cloud integration is in development. The commands below describe the client interface and do not imply that a public hosted service is available. Nothing else in this guide needs it.

Chanto cloud runs agents for you on hosted runners or your own, and offers models through its gateway. Sign in once and the CLI uses it for [cloud runs](#cloud-runs-from-the-cli), models and agents:

```bash
chanto cloud login                      # browser sign-in (asks which one if the server offers several; --provider github picks)
chanto login cloud                      # the same
chanto cloud login --url https://cloud.example   # another server (default: CHANTO_CLOUD_URL, else https://api.chanto.sh)
chanto cloud status                     # who you are, your orgs, and the one in use
chanto cloud org acme                   # use acme from now on (CHANTO_CLOUD_ORG or --org override it)
chanto cloud logout                     # or `chanto logout cloud`: revokes the token and forgets it
```

The token is kept in `~/.chanto/auth.json` (0600). Agents, runs and model usage belong to an org; without one picked, your personal org.

### Models through Chanto cloud

Once signed in, the built-in provider `chanto` uses the cloud's model gateway, billed to the selected org:

```bash
chanto cloud models                     # what the gateway offers, with prices per million tokens
chanto models chanto                   # the same names as chanto/<model>
chanto -m chanto/claude-sonnet-4-5
```

`/model chanto/<model>` works too, and `chanto providers` and `chanto doctor` show the sign-in. Claude models go to the gateway's Anthropic Messages API (`/v1/model/anthropic`), others to its OpenAI-compatible API (`/v1/model/openai/v1`). Requests carry your token as `x-api-key` (or a bearer token) and the org as `x-chanto-org` when one is selected. A `402` means the org can't spend more: it reached its monthly spending limit or has no trial credit left, so an org admin needs to raise the limit or turn on billing. A `401` means the token was revoked or expired: run `chanto cloud login` again.

### Agents

An agent is a standing job in an org: instructions plus a repository, started by hand, on a schedule, or by events in the repository. Each start is a run on a runner ([queue runners](#queue-runners-taking-runs-from-chanto-cloud)).

```bash
chanto agents                           # list: what starts each one, last and next run
chanto agents create triage --repo https://github.com/acme/api.git \
  --instructions "Label new issues and ask for missing details." --on issue_opened --on issue_labeled:needs-triage
chanto agents create deps --file deps.toml
chanto agents show triage               # settings, instructions, last run (--json for all fields)
chanto agents edit triage --schedule "0 9 * * 1-5" --placement hosted
chanto agents edit triage --on none --disable   # no event starts; only by hand
chanto agents run triage --input "look at #42" --wait
chanto agents runs triage               # its runs, newest first
chanto agents delete triage             # asks first; --yes in scripts. Its runs are kept
```

Settings, as flags or in `--file` (TOML or JSON; flags override the file, and `chanto agents show --json` output is a valid file):

| Flag | File key | |
|---|---|---|
| `--repo` | `repo` | Git URL the runs clone (required) |
| `--instructions`, `--instructions-file` | `instructions` | The task given to every run (required) |
| `--description` | `description` | |
| `--branch` | `branch` | Branch to start from; default: the repository's default branch |
| `--schedule` | `schedule` | Five-field cron in UTC, at most every 10 minutes; `none` removes it |
| `--on` | `triggers` | `issue_opened`, `issue_labeled[:label]`, `pull_request_opened`, `pull_request_updated`, `mention`; repeat for several |
| `--placement` | `placement` | `auto` (your runners if one is online, else hosted), `hosted` or `self-hosted`; default: the org's |
| `--label` | `labels` | Runner labels a run needs; repeat for several |
| `--disable`, `--enable` | `enabled` | A disabled agent only starts by hand |

On `edit`, only what you pass changes; `--on` and `--label` replace the whole list (`none` empties it), and `--rename` renames. Events only count from people with write access to the repository.

```toml
# deps.toml
name = "deps"
repo = "https://github.com/acme/api.git"
instructions = "Update outdated dependencies, run the tests, and open a pull request."
schedule = "0 6 * * 1"
placement = "hosted"
triggers = ["mention"]
```

`run` starts a run and prints its id (`chanto attach <id> --cloud` follows it later). With `--wait` it follows the run like [`chanto run --cloud`](#cloud-runs-from-the-cli): the model's messages and tool calls, then the summary, branch and pull request URL. It exits 0 if the run succeeded and 1 if it failed or was cancelled. Ctrl-C asks whether to cancel the run (yes cancels it; no stops following and leaves it running, exit 130); without a terminal it cancels. With `--json`, it prints only the finished run (`list`, `show` and `runs` take `--json` too).

### CI

`chanto cloud run --agent <name>` (the same as `chanto agents run <name>`) starts an agent's run without a login: set `CHANTO_CLOUD_TOKEN` to an account token (such as the one `chanto cloud login` saves), and `CHANTO_CLOUD_URL` and `CHANTO_CLOUD_ORG` as needed. With `--wait`, the job passes only if the run succeeded. The token also works for every other command here, including the `chanto` provider.

GitHub Actions:

```yaml
jobs:
  release-notes:
    runs-on: ubuntu-latest
    steps:
      - run: curl -fsSL https://chanto.sh/install.sh | sh
      - run: ~/.local/bin/chanto cloud run --agent release-notes --ref "$GITHUB_REF_NAME" --input "Write notes for $GITHUB_SHA" --wait
        env:
          CHANTO_CLOUD_TOKEN: ${{ secrets.CHANTO_CLOUD_TOKEN }}
          CHANTO_CLOUD_ORG: acme
```

GitLab CI:

```yaml
release-notes:
  image: debian:stable-slim
  script:
    - apt-get update -qq && apt-get install -yqq curl ca-certificates
    - curl -fsSL https://chanto.sh/install.sh | sh
    - ~/.local/bin/chanto cloud run --agent release-notes --ref "$CI_COMMIT_REF_NAME" --wait
  variables:
    CHANTO_CLOUD_ORG: acme   # CHANTO_CLOUD_TOKEN: a masked CI/CD variable
```

The same works in any CI system that can run a shell script.

## Teams

A team runs parallel agents on one task. `/team <task>` (or `chanto team "<task>" [--agents N]`) puts a lead agent in charge of several worker agents:

1. **Plan:** the lead explores read-only and splits the task into independent sub-tasks (up to twice the agent count, at most 8). Each sub-task owns some files or directories, so the workers don't edit the same files. You see the plan, with any overlapping ownership flagged, and approve it.
2. **Run in parallel:** each sub-task becomes a [background run](#background-and-remote-runs) in its own worktree and branch, at most `agents` at a time. All of them start from the same base: HEAD plus a snapshot of your uncommitted changes. Each run has its share of the budget, verified mode and needs-input prompts. They show up in `chanto runs`.
3. **Review:** as each run finishes, the lead reads its diff, verification result and final message, then accepts it or rejects it with a reason. A rejected sub-task is re-delegated once with that feedback. A failed run (error, budget, time limit) is reported, not retried.
4. **Merge:** accepted results are cherry-picked onto the team branch `chanto/team/<id>` in plan order. If one conflicts with the work merged before it, the cherry-pick is aborted and the sub-task is re-delegated once. The new run starts from the merged work and is told which files conflicted. If that also fails, the sub-task is reported as a conflict.
5. **Check and report:** the project's checks (verified mode's `checks`, or the detected ones) run on the merged result. The report lists each sub-task with its runs, state, notes and the check results.

Your checkout is never touched. `chanto team apply <id>` brings the team branch onto your current branch the way `chanto runs apply` does (cherry-picked, or applied uncommitted if you have local changes). You can also `git merge chanto/team/<id>`.

While it runs, the TUI shows `⧉ team 1/3 merged · 2 running` in the status line and a line per sub-task above the prompt. A sub-task waiting for input says so: answer it with `/runs attach <id>`. `/team status` shows the list, and `/team stop` stops the team and its running sub-tasks. Esc detaches the lead but leaves the sub-tasks running. `chanto team resume <id>` picks the team up again (the record is in `~/.chanto/teams/<id>/team.json`). `chanto team list` and `chanto team show <id>` show past teams.

The budget is the total for the sub-task runs, split evenly. A re-delegation only happens if the total still has room for another share. `--max-cost` overrides it for `chanto team`. Model, provider, mode and `--no-verify` apply to the sub-task runs.

```toml
[team]
agents = 3            # sub-task runs at once (`--agents N`); the daemon's `[background] max_parallel` also applies
max_cost_usd = 6.0    # total, split evenly across sub-tasks
```

## Voice input

Dictate a prompt instead of typing it. In the TUI press **ctrl+space** (or run
`/voice`): the footer shows a red recording indicator with the elapsed time.
Press ctrl+space again, or just stop talking for two seconds, and the
transcript is inserted into the prompt for you to review and edit; it is never
sent on its own. `esc` discards the recording. (On macOS,
ctrl+space may be bound to "Select the previous input source" in System
Settings → Keyboard → Keyboard Shortcuts; turn that off or use `/voice`.)

Recording uses `ffmpeg` (AVFoundation on macOS, PulseAudio or ALSA on Linux,
DirectShow on Windows), or sox's `rec`; install one (`brew install ffmpeg`).
Your terminal needs microphone access (macOS asks the first time). Recordings
are 16 kHz mono WAV temp files, readable only by you and deleted as soon as
they are transcribed.

Transcription is **local by default**: [whisper.cpp](https://github.com/ggml-org/whisper.cpp)
(`brew install whisper-cpp`, which provides `whisper-cli`) with a model in
`~/.chanto/voice/`:

```sh
mkdir -p ~/.chanto/voice
curl -L -o ~/.chanto/voice/ggml-base.en.bin \
  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin
```

OpenAI's Python `whisper` command works too (`whisper_bin = "whisper"`;
`whisper_model` is then a model name such as `base`).

Audio is sent to a **hosted** service only if you configure one: any
OpenAI-compatible `/audio/transcriptions` endpoint, with the credentials Chanto
already uses for that provider (`chanto login`, or its key variable):

```toml
[voice]
backend = "auto"        # auto: whisper.cpp if installed, else the hosted endpoint below (if set)
                        # local | hosted | off
provider = "groq"       # or "openai", or a [providers.<name>] entry
# base_url = "https://api.example.com/v1"   # any OpenAI-compatible server
# api_key = "..."
# model = "whisper-large-v3-turbo"          # default: whisper-1 (OpenAI), whisper-large-v3-turbo (Groq)
language = "en"         # default: detected
# whisper_bin = "/opt/homebrew/bin/whisper-cli"
# whisper_model = "~/.chanto/voice/ggml-small.en.bin"
# device = ":1"         # ffmpeg input (macOS ":default", Linux "default", Windows: the DirectShow name)
silence_secs = 2.0      # stop after this much silence following speech; 0 = only on the key
max_secs = 120
```

`[voice]` is read only from your own config (and trusted projects): a
repository can't redirect your audio. The server's `GET /voice` reports whether
dictation is available and `POST /voice/transcribe` (bearer token; the body is
the audio, any format ffmpeg reads) returns `{"text": ...}`.

## Server

`chanto serve` exposes sessions and [background runs](#background-and-remote-runs) over HTTP with server-sent events, so other machines and scripts can drive Chanto.

The API is bound to `127.0.0.1` by default and every route except `/health` requires `Authorization: Bearer <token>` (the scheme name is case-insensitive, as for every Chanto server: `bearer` works too) (the token is written to `~/.chanto/server.json`, mode `600`). Set `CHANTO_SERVER_TOKEN` to pin it, or `CHANTO_SERVER_TOKEN_FILE` to a file that holds the token (for systemd `LoadCredential=` and Docker/Compose secrets, so it never sits in the environment).

### As an MCP server

`chanto mcp-serve` runs Chanto as an MCP server over stdio, so Claude Code, Codex, Cursor or any MCP client can hand it coding tasks. It offers two tools: `chanto` (run a task; returns the answer and a `sessionId`) and `chanto_reply` (continue that session). Verified mode is on by default, so a task only comes back "done" once the project's checks pass. Approvals can't be asked for over MCP: actions needing one are refused unless the call's `mode` (default `acceptEdits`) or your allow rules permit them. Tool activity is sent as progress notifications, and cancelling the call stops the agent.

```bash
claude mcp add chanto -- chanto mcp-serve --model qwen3:8b
```

## Claude Code and Codex compatibility

Chanto picks up what you've already configured for Claude Code and Codex, so a project set up for either works without changes. This compatibility runs one way: Chanto reads their files and leaves them as they are. Run `/config` to see which files were loaded, and `chanto --dump-config` to print the merged result.

| | Claude Code | Codex |
|---|---|---|
| Settings | `~/.claude/settings.json`, `.claude/settings.json`, `.claude/settings.local.json`: `permissions` (`allow`/`deny`/`ask`, `defaultMode`, `additionalDirectories`), `hooks`, `disableAllHooks`, `env`, `statusLine`, `bashEditDiffEnabled` (`bash_diff`); the env variable `CLAUDE_BASH_NO_LOGIN=1` turns off the [shell snapshot](#your-shell-setup-in-bash) | `~/.codex/config.toml`, `.codex/config.toml`: `mcp_servers`, `model_providers`, `[hooks]`; `hooks.json` beside them; trusted projects |
| MCP servers | `.mcp.json`, `~/.claude.json` (user and per-project, including `disabledMcpServers`), with `${VAR}` / `${VAR:-default}` expansion; `.mcp.json` approval keys `enabledMcpjsonServers`, `disabledMcpjsonServers`, `enableAllProjectMcpServers`; `allowedMcpServers`/`deniedMcpServers`; `chanto mcp add-json` and `add-from-claude-desktop` | `[mcp_servers.*]` (stdio and HTTP: `command`, `args`, `env`, `env_vars`, `cwd`, `url`, `http_headers`, `env_http_headers`, `bearer_token_env_var`, `startup_timeout_sec`/`_ms`, `tool_timeout_sec`, `enabled_tools`, `disabled_tools`, `enabled`) |
| Managed settings | `managed-settings.json` and `managed-settings.d/*.json` (alphabetical) in `/Library/Application Support/ClaudeCode/` (macOS), `/etc/claude-code/` (Linux, WSL), `C:\Program Files\ClaudeCode\` (Windows): `permissions` (`allow`/`deny`/`ask`, `additionalDirectories`, `defaultMode`, `disableBypassPermissionsMode`), `env`, `allowManagedPermissionRulesOnly`, `allowedMcpServers`/`deniedMcpServers`, `allowManagedMcpServersOnly` | — |
| Instructions | `CLAUDE.md`, `.claude/CLAUDE.md`, `CLAUDE.local.md`, `~/.claude/CLAUDE.md`, `@path` imports, `.claude/rules/**/*.md` (including `paths:`-scoped rules) | `AGENTS.md`, `AGENTS.override.md`, `~/.codex/AGENTS.md` (Gemini, Cursor, Copilot, Aider and opencode files: [below](#other-tools-instruction-files)) |
| Skills, commands, agents | `.claude/skills`, `.claude/commands` (subfolders namespace them: `commands/frontend/test.md` is `/frontend:test`), `.claude/agents` (subfolders allowed), in home and in every directory from the repository root down to the working directory | `~/.codex/skills`, `~/.codex/prompts` (`/name` or `/prompts:name`, `$1`..`$9`, `$NAME`), `.agents/skills`, `.codex/skills` |
| Plugins | chanto-managed installs and Claude Code plugins enabled in `enabledPlugins`: commands (`/plugin:cmd`, or `/cmd` when unambiguous), agents, skills, hooks (`${CLAUDE_PLUGIN_ROOT}`, `${CLAUDE_PLUGIN_DATA}`), MCP servers and LSP servers, including components defined in the marketplace entry. `/plugins` lists them | — |
| Tools | — | `apply_patch` patches (as a tool call or `apply_patch <<EOF` in the shell), `shell` argv calls, `update_plan` |
| CLI | `-p`, `-c`, `-r`, `--add-dir`, `--dangerously-skip-permissions`, `--fallback-model`, `--permission-mode`, `--allowed-tools`/`--disallowed-tools`, `--output-format`, `--input-format stream-json`, `--system-prompt[-file]`, `--append-system-prompt[-file]`, `--mcp-config`, `--strict-mcp-config`, `--settings`, `--setting-sources`, `--session-id`, `-n`/`--name`, `--no-session-persistence`, `--json-schema`, `--agent`, `--agents` (JSON; a number is Chanto's team size), `--tools`, `--plugin-dir`, `--bare`, `--permission-prompt-tool`, `--include-partial-messages` (no-op); see [Claude Code's flags](#claude-codes-flags) | `exec`, `--full-auto`, `--yolo`, `-s read-only\|workspace-write\|danger-full-access`, `-C`, `--json` |

How it works:
- Instruction files are read from your home directory and from every directory between the repository root and the working directory. Files in deeper subdirectories are added to the context the first time the agent reads or edits something there.
- A skill, command or subagent nearer the working directory replaces one with the same name further up, and project ones replace your home ones.
- Skills follow Claude Code's format:
  - Every skill also runs as `/name [args]`. When a skill and a command share a name, the skill wins (built-in commands always win). `user-invocable: false` hides a skill from `/`; `disable-model-invocation: true` hides it from the model and the `Skill` tool, so only `/name` runs it.
  - In `SKILL.md`, `$ARGUMENTS` is all the arguments, and `$ARGUMENTS[N]` or `$N` is one of them, counting from 0 (`$0` is the first; command files keep `$1` as the first). `${CLAUDE_SKILL_DIR}` is the skill's directory. Arguments that no placeholder takes are appended as `ARGUMENTS: <value>`. `` !`cmd` `` at the start of a line or after a space is replaced by the command's output before the model sees it, in skills and commands alike; project skills and commands don't run them in a folder you haven't trusted. Arguments are filled in after the blocks run, so they can't add a block or a command. Inside a block they are data: all of them are in the `ARGUMENTS` environment variable (`"$ARGUMENTS"`, or `$env:ARGUMENTS` in PowerShell); in a command file's blocks run by a POSIX shell, the words are also `"$1"`..`"$9"`, and a `NAME=value` argument is `$NAME` when the block uses it and the environment has no `NAME` already. Quote them, as in `` !`git log "$1"` ``.
  - `context: fork` runs the skill in a subagent (`agent:` picks the type, default `general-purpose`). It runs in the background and its report arrives when it finishes; `background: false` waits for it in the turn that invoked the skill, and so do non-interactive runs (`-p`).
  - Skills and command files also read `argument-hint` (shown next to `/name` in completion and `/help`), `allowed-tools` (permission rules, space- or comma-separated or a list, that are pre-approved during the turn that invokes it; deny and ask rules still win, and the grant clears with your next message), `model` and `effort` (for the rest of that turn; with `context: fork`, for the subagent), `shell` (`bash` or `powershell` for its `` !`cmd` `` blocks) and `hooks` (registered when it's invoked and kept for the rest of the session; `once: true` removes a hook after its first successful run; a project skill's hooks need folder trust).
  - Boolean fields accept `true`/`false`, `yes`/`no`, `on`/`off` and `1`/`0` in any case.
- Subagent files read Claude Code's frontmatter; unknown keys are ignored:
  - `tools` and `disallowedTools` are a comma-separated string or a YAML list. A `disallowedTools` entry like `Bash(git push *)` removes the whole tool. If Chanto can't read either value, the subagent gets no tools rather than all of them.
  - `model: inherit` uses the main conversation's model. `sonnet`, `opus`, `haiku` and `fable` become the newest Claude model of that family when the subagent's provider is the Anthropic API (`anthropic/…` or a `kind = "anthropic"` provider). On other providers, including Bedrock, Vertex AI and Foundry, the subagent keeps its usual model and Chanto warns once. Any other value is a model spec.
  - `permissionMode` (`default`/`manual`, `acceptEdits`, `auto`, `dontAsk`, `plan`, `bypassPermissions`) applies while the main conversation is in default or plan mode; in bypass, acceptEdits or auto mode the subagent runs in that mode, and it never gets bypass on its own. `dontAsk` denies anything that would prompt.
  - `maxTurns` caps its model calls; at the limit its last message comes back marked partial, and it can be resumed.
  - `skills` preloads up to 32 skills' full content into its prompt (skills with `disable-model-invocation` can't be preloaded; missing ones are skipped).
  - `mcpServers` lists configured server names (kept reachable through a `tools` list) and inline definitions (`- name: {type, command, args, url, ...}`), which connect when the subagent starts and close when it's done. A project agent's inline servers and its `hooks` need folder trust.
  - `hooks` (same layout as settings) run only while the subagent does; `Stop` hooks fire as `SubagentStop`.
  - `memory: user|project|local` gives it a directory (`~/.claude/agent-memory/<name>/`, `.claude/agent-memory/<name>/`, `.claude/agent-memory-local/<name>/`) with instructions and the first 200 lines of its `MEMORY.md` in its prompt, and adds Read, Write and Edit to its tools. Off when `[memory] auto = false`.
  - `background: true` always runs it in the background, `isolation: worktree` in its own git worktree, `effort` sets its effort level, `color` colors its name in the result line, and `omitClaudeMd: true` leaves the instruction files out of its prompt.
  - With `--agent <name>` the definition runs as the main session: its tools, model, prompt, `permissionMode`, `maxTurns`, `effort`, `skills`, `mcpServers`, `hooks` and `memory` apply, and `initialPrompt` becomes the first message of a new session (ahead of any prompt you pass). `--agents '<json>'` takes the same fields under the same keys.
- Frontmatter is read as YAML: quoted strings, `>` and `|` blocks, lists, nested maps (`hooks:`, `mcpServers:`), flow `[...]`/`{...}` collections and comments.
- Permission rules follow Claude Code's rules:
  - `Edit(...)` covers every tool that writes files and `Read(...)` every tool that reads them.
  - In paths, `//` means absolute, `~/` means home, a leading `/` is relative to the project and `./` is relative to the working directory.
  - `mcp__server` matches all of that server's tools.
  - A bare `Bash` (or `Bash(*)`) allow rule covers every command, including ones with `$(…)`, backticks or `<(…)`. A prefix rule such as `Bash(git:*)` doesn't cover those, since the substituted command isn't visible in the text: `git $(rm -rf ~)` still needs approval. Deny and ask rules win over either.
  - When a non-interactive run (`-p`) refuses a call that needs approval, the message suggests the allow rule that "don't ask again" would add for it, such as `Bash(npm run test:*)` or `Bash(git push:*)`.
- Imported MCP servers that need an OAuth login show as "needs login" (`/mcp login <name>`); ones that fail to connect for other reasons are skipped with a single warning.
- Project-level files go through the same [folder trust](#configuration) check as `.chanto/config.toml`. If you already trusted a folder in Claude Code or Codex, Chanto trusts it too.
- To turn imports off, set `compat = { claude = false, codex = false }` in `~/.chanto/config.toml`.
- Claude Code managed settings (your organization's policy) are read whatever `compat` says and applied above every other layer, profiles included: their deny and ask rules can't be removed by any other file, and their `env` wins. With `allowManagedPermissionRulesOnly: true`, only the policy's own allow rules count (deny and ask rules from your files still apply). `permissions.defaultMode` sets the mode every session starts in, over your config and command-line flags (you can still switch modes during a session). `permissions.disableBypassPermissionsMode: "disable"` rules out bypass mode entirely: `--dangerously-skip-permissions`, a config `mode`, `/mode bypass`, ACP and the HTTP API all fall back to default mode with a warning. Other managed keys are ignored. A managed file that isn't a JSON object stops Chanto from starting, as it does Claude Code; `chanto doctor` lists the managed files among the config layers. Debug builds accept `CHANTO_MANAGED_SETTINGS_DIR` to test a policy from another directory; release builds always read the real one.
- `MCP_TOOL_TIMEOUT` (milliseconds) limits MCP tool calls as in Claude Code, for servers without `tool_timeout_sec` when `mcp_tool_timeout` isn't set; unset, the limit is 10 minutes.
- MCP tool search follows Claude Code's `ToolSearch`: the same query forms, `ENABLE_TOOL_SEARCH` values (`true`, `false`, `auto`, `auto:N`), `alwaysLoad` server setting and `anthropic/alwaysLoad` tool metadata. Two differences: unset means `auto` (Claude Code always defers on its own API), since small local models do better with tools in view while the list is small; and loaded definitions are sent as ordinary tools appended to the list (and in the `ToolSearch` result) rather than as `tool_reference` blocks, which only Anthropic's API supports. See [MCP servers](#mcp-servers).
- `--fallback-model` and `fallback_model` work like Claude Code's `--fallback-model` and `fallbackModel` (a chain of up to three, tried in order, for the current turn only, with a notice when it switches). Chanto also switches when a local model server refuses the connection, which Claude Code treats as a transport error. Claude Code's `fallbackModel` setting isn't imported, since its model names don't apply to other providers. See [Fallback models](#fallback-models).
- `compat.codex_rules = true` also imports the command approvals in `~/.codex/rules/*.rules` (`prefix_rule`) as Bash allow, ask and deny rules.

`chanto migrate <tool> [--path <file>] [--apply]` translates another agent's configuration (`opencode`, `continue`, `cline`, `cursor`, `windsurf`, `aider`, or any file with `--path`) into Chanto's, using your configured model. It shows the result for review; `--apply` backs up your config and merges it into `~/.chanto/config.toml`.

### Other tools' instruction files

Chanto also reads the instruction files other coding tools keep, in each directory from the repository root down to the working directory:

| Tool | Files | Switch |
|---|---|---|
| Gemini CLI | `~/.gemini/GEMINI.md`, `GEMINI.md` (hierarchical: subdirectory files load once the agent works there, like AGENTS.md) | `compat.gemini` |
| Cursor | `.cursor/rules/**/*.mdc` (and `.md`): `alwaysApply: true` always loads; `globs` loads the rule when the agent touches a matching file; a rule with only a `description` is listed for the model to read when relevant; manual rules aren't loaded. Legacy `.cursorrules` always loads | `compat.cursor` |
| GitHub Copilot | `.github/copilot-instructions.md`; `.github/instructions/**/*.instructions.md` by `applyTo` (`**` = always, other globs = when a matching file is touched) | `compat.copilot` |
| Aider | the files `.aider.conf.yml` lists under `read:` (e.g. `CONVENTIONS.md`); nothing else | `compat.aider` |
| opencode | `~/.config/opencode/AGENTS.md`; `instructions` paths and globs in `opencode.json`; subagents in `.opencode/agent/*.md` and `~/.config/opencode/agent/` (`mode: primary` agents are skipped; `tools: { write: false }` entries become disallowed tools) | `compat.opencode` |

Project files from these tools load only in a [trusted folder](#configuration); user-level files always load. Paths they reference must stay inside the project. Files with identical content (an `AGENTS.md` symlinked as `GEMINI.md`, the same text in two tools' files) load once. All are on by default; turn one off in `~/.chanto/config.toml`, e.g. `compat = { cursor = false }`. `/config` lists the instruction files loaded and the path-scoped rules waiting for a match.

### Hooks

Hooks follow Claude Code's protocol for its events and handler types. Hooks written for Claude Code or Codex work unchanged.
- **Events:** all of Claude Code's events are accepted. Each fires at the matching moment in Chanto, with Claude Code's input fields, matcher and decision handling:
  - `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionRequest`, `UserPromptSubmit`, `Stop`, `SubagentStart`, `SubagentStop`, `SessionStart`, `SessionEnd`: as in Claude Code. `Notification` fires for permission prompts and, with `notification_type: "idle_prompt"`, a minute after a turn ends while you seem to be away (nothing typed, nothing running in the background; the TUI also needs the terminal unfocused).
  - `PreCompact` / `PostCompact`: around `/compact` (`manual`) and automatic compaction (`auto`). Exit 2 or `decision: "block"` on `PreCompact` skips the compaction. `PostCompact` gets `compact_summary`.
  - `PermissionDenied`: auto mode refused a tool call (the classifier or a dangerous-command check). `hookSpecificOutput.retry: true` tells the model it may retry, except when the classifier was unavailable or timed out.
  - `PostToolBatch`: after every call of a batch has its result, before the next model call. `tool_calls` lists each call's `tool_name`, `tool_input`, `tool_use_id` and `tool_response` (the text the model receives). `additionalContext` reaches the model; `decision: "block"` or `continue: false` ends the turn.
  - `StopFailure`: the turn ended on a model request error. `error` is Claude Code's error type, guessed from the provider's error text (`rate_limit`, `authentication_failed`, `billing_error`, `model_not_found`, `server_error`, ...), plus `error_details` and `last_assistant_message`. Output is ignored.
  - `MessageDisplay`: the assistant's text passes through the hook before it's shown, in batches of whole lines (the whole message at once in `-p` and other non-interactive runs). `hookSpecificOutput.displayContent` replaces what's displayed; the conversation keeps the original. Default timeout 10 seconds.
  - `UserPromptExpansion`: a typed `/command` expands into a prompt: custom commands and skills (`expansion_type: "slash_command"`, `command_source` `project` or `user`) and MCP prompts (`mcp_prompt`). Exit 2 or `decision: "block"` stops it; `additionalContext` goes along with the prompt. Built-in commands such as `/review` don't fire it.
  - `TaskCreated` / `TaskCompleted`: Chanto's tasks are the `TodoWrite` list. A new item is a task being created; an item turning `completed` is a task being completed. `task_id` is the item's position, `task_subject` its text. A block keeps that item out of the list, or keeps it from being marked completed, and the hook's message goes back to the model as the tool's error.
  - `TeammateIdle`: a `/team` sub-task's run finished, before the lead reviews it (`teammate_name` is the sub-task id, `team_name` the team id). Exit 2 runs the sub-task again with the hook's message, told to pick up its earlier branch first (up to five runs per sub-task); `continue: false` stops it.
  - `InstructionsLoaded`: instruction files (`AGENTS.md`, `CLAUDE.md`, `CHANTO.md`, `.claude/rules/*.md`) loaded at session start (`load_reason: "session_start"`, `compact` after a compaction) and nested ones loaded when the agent touches a subdirectory (`nested_traversal`, or `path_glob_match` for rules with `paths:`). Unlike Claude Code it also fires for `AGENTS.md`, Chanto's own instruction file. Runs in the background.
  - `ConfigChange`: Chanto applies changed settings when you run `/reload`, so that's when it fires, once per settings file changed since they were last loaded (`user_settings`, `project_settings`, `local_settings`, `policy_settings`, `skills`; Chanto's `config.toml` files count with the matching Claude settings). A block keeps the current settings, except for `policy_settings`. Unlike Claude Code, `/reload` says it was blocked.
  - `CwdChanged`: a Bash command in the main conversation changed the session's working directory (`old_cwd`, `new_cwd`). `watchPaths` sets the files `FileChanged` watches. Its hooks get `CLAUDE_ENV_FILE` (see below); what they write there lasts until the next `CwdChanged`.
  - `FileChanged`: the files named in the `FileChanged` matchers (in the working directory) and any `watchPaths` a `SessionStart`, `CwdChanged` or `FileChanged` hook returned are polled twice a second. A change, creation or deletion fires the hooks whose matcher fits the file name, with `file_path` and `event` (`change`, `add`, `unlink`). Its hooks get the same `CLAUDE_ENV_FILE` as `CwdChanged` hooks.
  - `DirectoryAdded`: `/add-dir` added a directory that wasn't already in the workspace (`source: "slash_command"`). Runs in the background (600-second default timeout); a `systemMessage` reaches the model with the next prompt. `--add-dir` doesn't fire it, as in Claude Code.
  - `WorktreeCreate` / `WorktreeRemove`: a `WorktreeCreate` hook replaces git for `--worktree` sessions and for subagents started with `isolation: "worktree"`; it gets `name` and prints the directory's path as the last line of stdout (an http hook returns `hookSpecificOutput.worktreePath`). A failure, or no usable directory, fails the creation. On leaving a `--worktree` session whose worktree a hook made, Chanto removes it when git sees no changes in it: through the `WorktreeRemove` hooks (`worktree_path`; a non-zero exit fails the removal if the directory is still there), or without one, as `git worktree remove --force` would. Subagents' hook-made worktrees are kept, as in Claude Code.
  - `PreModelSwitch` / `PostModelSwitch`: `/model <name>` (`source: "command"`) and an ACP client's model change (`sdk`). The matcher sees the model name without its `provider/` prefix. `PreModelSwitch` can refuse the switch (exit 2, `decision: "block"`, `permissionDecision: "deny"`, or not answering within its 30-second timeout) or ask (`ask`: `/model` asks you to confirm; elsewhere it refuses). `PostModelSwitch` also fires when resuming a session restores its model (`resume`); its stdout or `additionalContext` reaches the model with the next prompt. `context_tokens` is an estimate, `prompt_cache_warm` means a model reply arrived in the last five minutes, and `estimated_cache_write_usd` uses Chanto's price list (0 with `pricing: "default"` for models it can't price).
  - `Elicitation` / `ElicitationResult`: around an MCP server's `elicitation/create`, matched on the server name. An `Elicitation` hook can answer for you (`hookSpecificOutput.action` `accept` with `content`, `decline` or `cancel`); exit 2 or `decision: "block"` declines. Otherwise you answer, and `ElicitationResult` hooks can change or block your answer the same way. Hooks also run in `-p` mode, where nobody is there to answer and requests are otherwise declined.
  - `Setup`: fires only for `chanto --init-only` (runs the `Setup` hooks with trigger `init`, then the `SessionStart` hooks, and exits without a conversation) and for `chanto -p --init` / `-p --maintenance` (trigger `init` / `maintenance`, before the prompt runs; without `-p` these flags are an error). The matcher and the `trigger` input field are the trigger. Only `command` hooks run; Setup can't block, any exit code continues, and its JSON output is discarded.

  Codex's `Interrupt` isn't fired. Hooks configured for unknown events produce a startup warning.
- **Handler types:** all of Claude Code's: `command`, `http`, `mcp_tool`, `prompt` and `agent`, on the events Claude Code allows them (others are skipped with a single warning). A handler of an unknown type, or without the fields its type needs, is reported at startup and by `chanto doctor`.
  - `command`: `args` switches to exec form (`command` is spawned directly with `args`, no shell; `${CLAUDE_PROJECT_DIR}`, `${CLAUDE_PLUGIN_ROOT}` and `${CLAUDE_PLUGIN_DATA}` are substituted). `shell: "powershell"` runs it with PowerShell (`pwsh` where installed outside Windows). `async: true` runs it in the background without blocking or a timeout; when it exits, its `additionalContext` and `systemMessage` reach the model on its next turn. `asyncRewake: true` does the same within its timeout, and exit code 2 also wakes an idle session with the hook's stderr (or stdout).
  - `mcp_tool`: `server`, `tool` and `input`, whose strings take `${path}` values from the hook input (`"${tool_input.file_path}"`). The tool's text is read like a command's stdout; an error result, or a server that's missing or not connected, is a non-blocking error. Not run for `Setup` or `SessionStart` at launch or resume.
  - `prompt`: one model call (the hook's `model`, or the session's) with the prompt, where `$ARGUMENTS` is the hook input JSON (appended when absent; `\$` is a literal `$`). The model answers `{"ok": true}` or `{"ok": false, "reason": "..."}`. `ok: false` keeps a `Stop`/`SubagentStop` agent working (unless it also says `"impossible": true`), ends the turn on `PreToolUse` and `PostToolUse` with the reason as a warning (`continueOnBlock: true` instead returns it to the model: a denied tool call, or feedback), blocks `UserPromptSubmit`, `UserPromptExpansion`, `PostToolBatch`, `PostToolUseFailure`, `TaskCreated` and `TaskCompleted`, and does nothing on `PermissionRequest` and `PermissionDenied`. Default timeout 30 seconds.
  - `agent`: like `prompt`, but a read-only subagent (Read, Grep, Glob, Ls; up to 50 model calls, no hooks of its own) investigates before answering, and `ok: false` acts as `continueOnBlock: true`. Not on `PermissionRequest`. Default timeout 60 seconds.
- **`onFailure: "block"`** (command and http hooks): a hook that can't start, exits with a code other than 0 or 2, times out or prints invalid JSON does what exit 2 does on that event (denies on `PermissionRequest`), with `failed; blocking because onFailure is "block"` (or `timed out`) in the message. No effect on `Stop`, `SubagentStop`, `TaskCompleted`, `TeammateIdle` and background hooks.
- **`disableAllHooks: true`** (or `disable_all_hooks = true`) turns all hooks off, skill and subagent hooks included. A project setting needs folder trust.
- **Hooks in skills and subagents:** see their frontmatter above. `once: true` is honored for skill hooks only.
- **Matchers:** empty or `*` matches everything. A value made only of letters, digits, `_`, `-`, spaces, `,` and `|` is a list of exact names (`Edit|Write` or `Edit, Write`). Anything else is an unanchored regex (`^Notebook`, `mcp__memory__.*`). Matchers that name `Edit`, `Write` or `MultiEdit` also match `apply_patch`.
- **`if`:** a handler can carry one permission rule, such as `"if": "Bash(git *)"` or `"if": "Edit(**/*.ts)"`. The hook runs only when the tool call matches that rule, using the same matcher as permission rules. For Bash, every subcommand is checked, including commands inside `$(...)` and backticks, with leading `VAR=value` assignments ignored. When Chanto can't tell what a command runs (nested substitutions), the hook runs, as in Claude Code. `if` applies to tool events only. A hook whose `if` is malformed or set on another event is skipped, with a single warning.
- **Input:** a command hook gets JSON on stdin with `hook_event_name`, `session_id`, `transcript_path`, `cwd` and `permission_mode`, plus the event's own fields:
  - Tool events: `tool_name` and `tool_input`.
  - `PostToolUse`: a `tool_response` object. Bash's is `stdout`, `stderr`, `interrupted` and `isImage`, with stdout and stderr interleaved in `stdout`. Write's is `type`, `filePath` and `content`. Edit's is `filePath`, `oldString`, `newString` and `replaceAll`. Read's is `type` and `file.filePath`/`file.content`. Other tools get `{"output": ...}`. The plain text the model saw is also sent as `tool_output`.
  - `PostToolUseFailure`: `error` and `is_interrupt`.
  - `Stop`/`SubagentStop`: `last_assistant_message` (also sent as `last_message` for older hooks) and `stop_hook_active`.

  The environment has `CLAUDE_PROJECT_DIR` and `CHANTO_PROJECT_DIR`. Payload fields Claude Code sends that Chanto doesn't include: `tool_use_id` (sent on `PermissionDenied` and `PostToolBatch` only), `duration_ms`, `agent_id`, `background_tasks`, `prompt_id`, `effort`.
- **Persisted environment (`CLAUDE_ENV_FILE`):** `SessionStart`, `Setup`, `CwdChanged` and `FileChanged` command hooks get `CLAUDE_ENV_FILE` (also as `CHANTO_ENV_FILE`), a per-session file under `~/.chanto/session-env/` that's kept when you resume. Other events don't get it. Append `export NAME=value` lines to it, and every later Bash command in the session (background ones and subagents' too) runs the file first, so a line can use anything Bash evaluates, such as `export PATH="$PATH:./node_modules/.bin"`. What `CwdChanged` and `FileChanged` hooks write lasts until the next `CwdChanged`. A `CLAUDE_ENV_FILE` set before Chanto starts is run before each command as well. Sandboxed commands can read the files. Commands in a background run's container don't get them. Where the shell is PowerShell or cmd (Windows without Git Bash, or `CHANTO_SHELL`), nothing runs the files and hooks don't get `CLAUDE_ENV_FILE`, as in Claude Code. Variables from a launch-time `CLAUDE_ENV_FILE` are still copied into the command's environment if every line of the files is blank, a `#` comment or a plain assignment (`export NAME=value`, `declare -x NAME=value` or `NAME=value`, with unquoted `[A-Za-z0-9_@%+=:,./-]` text, single-quoted text, or double-quoted text with `$`, backticks and `"` escaped). If any line is something else (`$PATH`, `source`, `$'...'`), none are copied and the debug log says `Session environment is not all plain assignments`. On Windows, values containing `/` or `\` are left out, and sandboxed commands get none.
- **HTTP hooks:** `{"type": "http", "url": "...", "headers": {...}, "allowedEnvVars": [...], "timeout": 30}` POSTs the same JSON payload. `$VAR`/`${VAR}` in headers expand only for variables listed in `allowedEnvVars`; unlisted ones become empty. A 2xx response with an empty body counts as success. A 2xx response with a JSON object body is read like a command hook's stdout. Any other body, a non-2xx status or a connection error is a non-blocking error (or a block with `onFailure: "block"`), and a timeout cancels the hook. Like command hooks, http hooks don't run on `SessionStart` and `Setup`.
- **Blocking:** exit code 2 blocks, and stderr (or stdout, if stderr is empty) is passed back to the model. A hook that blocks or denies without a reason is named instead: `hook [<command>] exited with code 2 and gave no reason`.
- **JSON output:** a hook can print JSON on stdout to approve, deny or ask (`hookSpecificOutput.permissionDecision`, or `decision.behavior` for `PermissionRequest`). It can also rewrite the tool input (`updatedInput`), add context (`additionalContext`), make the agent keep going (`decision: "block"` on `Stop`), stop the turn (`continue: false`) or show a message (`systemMessage`). On `PostToolUse`, `updatedToolOutput` replaces what the model sees when it's shaped like the tool's `tool_response` (Bash's `stdout`/`stderr`, Read's `file.content`, other tools' `output`); `updatedMCPToolOutput` does the same for MCP tools only, which also accept text or content blocks. The transcript display keeps the original. `suppressOutput` is ignored.
- **Timeouts:** 60 seconds by default for command, http and mcp_tool hooks (Claude Code's default is longer), except `MessageDisplay` (10), `PreModelSwitch` (30) and `DirectoryAdded` (600); `prompt` 30 and `agent` 60. Hooks for one event run one after another, not in parallel.
- **Config:** hooks go in TOML or JSON. In TOML, use `[[hooks.PreToolUse]]` with `matcher`, `command` or `type = "http"` plus `url`, and optional `if` and `timeout_secs`. In JSON, use Claude's layout in `.claude/settings.json`. Codex hooks are also read: `~/.codex/hooks.json` and `[hooks]` in `~/.codex/config.toml`, plus `.codex/hooks.json` and `.codex/config.toml` in the project. Codex hooks are not read when `compat.codex` is off or Codex's `[features] hooks = false` (or `codex_hooks = false`) is set. Project hooks from any source need folder trust. Chanto doesn't check Codex's per-hook review state. `/hooks` lists configured hooks.
- **Plugins:** plugin hooks and stdio MCP servers get `CLAUDE_PLUGIN_ROOT` (the plugin directory) and `CLAUDE_PLUGIN_DATA` in their environment. `CLAUDE_PLUGIN_DATA` is a per-plugin directory, `~/.chanto/plugin-data/<plugin>`, created when first needed. `${CLAUDE_PLUGIN_ROOT}` and `${CLAUDE_PLUGIN_DATA}` are substituted in their definitions.

### Format and lint after edits

After `Write`, `Edit`, `MultiEdit` or `apply_patch` succeeds, Chanto runs each changed file's formatter (in place), then its linter, from the project root. The model is told what the formatter changed, as a small diff (or "Read it again" for large rewrites), so its view of the file stays right, and the formatter's write doesn't count as an outside change for the next edit. A linter that exits non-zero sends its output to the model as `<lint file="..." command="...">`; the transcript shows a one-line summary. A formatter that fails leaves the file as written and says why.

```toml
[format]
auto = true                    # use the project's own tools (off by default)
timeout_secs = 15
[format.commands]              # glob or extension = command; {file} is the path (else appended)
"*.rs" = "rustfmt --edition 2021"
"web/**/*.{ts,tsx}" = "npx prettier --write {file}"

[lint]
auto = true                    # ruff check (when ruff is configured), eslint (when in package.json)
timeout_secs = 60
[lint.commands]
py = "ruff check --quiet"
```

Explicit commands are always used; the most specific pattern wins, and `enabled = false` turns the section off. With `auto = true`, Chanto picks a tool only when the project clearly uses it: `rustfmt --edition <Cargo.toml's>` for Rust with `rustfmt` installed, `gofmt -w` with a `go.mod`, `ruff format` when ruff is configured (`ruff.toml` or `[tool.ruff]`), else `black` when `[tool.black]` is, `clang-format -i` with a `.clang-format`, and prettier for JS/TS/CSS/JSON/Markdown/HTML/YAML when it's in `package.json`. Only files inside the project are touched. These are your commands, run like hooks: `[format]` and `[lint]` in a project config apply only once the folder is trusted, nothing runs under the read-only sandbox policy or when an editor (ACP) owns the file, and a background run's commands run in its container. They complement the done-gate (`--verify`), which still runs the project's checks at the end.

The path that `{file}` stands for is quoted for the shell that runs the command (single quotes for bash and PowerShell; under cmd.exe it becomes `"%CHANTO_FILE%"`), so `$(…)`, backticks or `%VAR%` in a file name are never expanded. The path is also in the `CHANTO_FILE` environment variable. A format, lint or verify command is finished when its own process exits: output arriving within a short grace period after that is kept, and a daemon it left running in the background (holding the output open) no longer stalls the turn until the time limit.

### Language servers

Servers start lazily (a file of that language is read or edited), one per language per project root (nearest `Cargo.toml`, `package.json`/`tsconfig.json`, `pyproject.toml`, `go.mod`, `compile_commands.json`, …). After `Write`/`Edit`/`MultiEdit`, Chanto waits briefly for diagnostics (2.5 s by default, 8 s for rust-analyzer while it indexes; usually well under a second once warm) and reports errors only, as

```
<diagnostics file="src/main.rs">
L4:10 error: cannot move out of `v` because it is borrowed [rustc E0505]
</diagnostics>
```

```toml
[lsp]
# enabled = false              # turn off language servers
# diagnostics_wait_ms = 2500
[lsp.servers.zls]              # add a server (or override a built-in by name)
command = "zls"
extensions = ["zig"]
```

In a project with more than 20,000 files, reading a file doesn't start a language server (the first edit does), and clangd runs only with a compilation database.

Some servers run project code: rust-analyzer runs build scripts and proc macros, typescript-language-server loads the project's own TypeScript, gopls may download toolchains, and pylsp loads linter plugins. These only start in trusted folders. pyright and clangd run anywhere. Servers only start for files inside the project or an `--add-dir` directory, with at most 4 running at once. In a Cargo workspace, one rust-analyzer serves all member crates. `[lsp]` in a project config is ignored until the folder is trusted. Set `CHANTO_LSP_DEBUG=1` to log server traffic to stderr.

Environment overrides: `CHANTO_MODEL`, `CHANTO_BASE_URL`, `CHANTO_PROVIDER`, `CHANTO_API_KEY`, `CHANTO_HOME`, `BRAVE_API_KEY`, `TAVILY_API_KEY`, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`.

### Hosted models

| `kind` | API | Default `base_url` | Key |
|---|---|---|---|
| `ollama` | Ollama `/api/chat` | `http://localhost:11434` | — |
| `openai` | OpenAI-compatible `/chat/completions` (llama.cpp, LM Studio, vLLM, …) | `http://localhost:8080/v1` | `api_key` |
| `anthropic` | Anthropic Messages API | `https://api.anthropic.com` | `api_key` or `ANTHROPIC_API_KEY` |
| `openai-responses` | OpenAI Responses API | `https://api.openai.com/v1` | `api_key` or `OPENAI_API_KEY` |
| `bedrock`, `vertex`, `azure`, `foundry` | Cloud platforms, see [Cloud providers](#cloud-providers-bedrock-vertex-ai-azure) | from region, project or resource | the cloud's credentials |

A `[providers.<name>]` section without `kind` is `ollama`, unless its `base_url` ends in `/v1`: then it's `openai`. If the server answers in the other format (Server-Sent Events or `choices` to the Ollama provider, Ollama's NDJSON to the OpenAI one), the error says so and names the `provider`/`kind` to set.

Built-in providers need no `[providers]` section: `chanto providers` lists them (Anthropic, OpenAI, your ChatGPT plan, OpenRouter, Gemini, DeepSeek, xAI, Mistral, Groq, Cerebras, Together, Fireworks, Moonshot, Z.ai, MiniMax, DashScope, Hugging Face, NVIDIA, Vercel AI Gateway, OpenCode Zen, Baseten, Perplexity, Cohere, DeepInfra, Nebius, SambaNova, SiliconFlow, Upstage, AI21, Inception, Requesty, Venice, Chutes, Nano-GPT, Scaleway, DigitalOcean, Weights & Biases, OVHcloud, Crusoe, Morph, Poe, StepFun, Volcengine (Doubao), Xiaomi MiMo, Ollama Cloud, the coding plans, the cloud platforms and [Chanto cloud](#models-through-chanto-cloud)) and whether you're logged in. Each works with its key variable (`GEMINI_API_KEY`, `GROQ_API_KEY`, …) or `chanto login <provider>`. With `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` set, the provider names `anthropic` and `openai` work without any config. For example, `chanto -m anthropic/claude-opus-5-5`, `subagent_model = "anthropic/claude-haiku-5-5"` or `/model openai/gpt-6.1-sol`. To use one as the default provider, set `provider = "anthropic"` and `model = "claude-opus-5-5"`.

For a ChatGPT plan, run `chanto login chatgpt` (or `/login chatgpt`), then `/models` to discover your account's available model names. Select an exact name with `/model chatgpt/<model>` or `chanto -m chatgpt/<model>`. Logging in does not select a model, and Chanto stores its credentials separately from Codex. Model access depends on your account; a model-access rejection is different from an expired or rejected login. OpenAI API-key access remains separate as `openai/<model>`. The `chatgpt` login uses Sign in with ChatGPT and draws from your existing Codex/ChatGPT Work allowance; a separate login to the same account does not add quota. Review connected-app usage and limits in ChatGPT **Settings → Usage**. See [OpenAI’s explanation](https://learn.chatgpt.com/docs/sign-in-with-chatgpt).

### Subscription coding plans

Subscription coding plans whose vendors allow them in third-party coding tools are built in. Each takes the plan's own key, which is not the vendor's pay-as-you-go API key, through `chanto login <provider>` (or `--as <name>` for a second account) or its variable. Most speak the vendor's OpenAI-compatible endpoint (MiniMax's M Plan is Anthropic-compatible; see below), and their usage shows no per-token price. Anthropic's Claude Code subscription (Pro/Max) is intentionally absent: its terms don't permit use from other tools, so use the `anthropic` provider with an API key or [Chanto cloud](#models-through-chanto-cloud) instead.

```sh
chanto login kimi-coding   # paste a Kimi Code membership key
chanto login zai           # paste a Z.ai GLM Coding Plan key
```

Inside a session, the equivalents are `/login kimi-coding` and `/login zai`. Login saves credentials; select a model separately with `/model kimi-coding/kimi-for-coding`, or use `/models` after choosing the provider to discover its models. These logins use plan keys rather than a browser sign-in.

| Provider | Plan | Endpoint | Key | Models |
|---|---|---|---|---|
| `zai` | Z.ai GLM Coding Plan | `https://api.z.ai/api/coding/paas/v4` | `ZAI_API_KEY` | `/models` |
| `kimi-coding` | Kimi Code membership | `https://api.kimi.ai/coding/v1` | `KIMI_CODE_API_KEY`; create one in the [Kimi Code console](https://www.kimi.com/code/console) | `kimi-for-coding` (1M context), `kimi-for-coding-highspeed`, `k3`, `k3-256k` |
| `kimi-coding-cn` | Kimi Code membership (China) | `https://api.kimi.com/coding/v1` | `KIMI_CODE_API_KEY` | same as `kimi-coding` |
| `minimax-coding` | MiniMax M Plan | `https://api.minimax.io/v1` | `MINIMAX_CODING_API_KEY`; the `sk-cp-…` subscription key from the [M Plan console](https://platform.minimax.io/console/plan) | `MiniMax-M3.1-Flash-Preview` (1M context), `MiniMax-M3`, `MiniMax-M2.7` |
| `qwen-coding` | Alibaba Qwen Code plan | `https://coding-intl.dashscope.aliyuncs.com/v1` | `ALIBABA_CODING_PLAN_API_KEY` | `/models` |
| `qwen-coding-cn` | Alibaba Qwen Code plan (China) | `https://coding.dashscope.aliyuncs.com/v1` | `ALIBABA_CODING_PLAN_API_KEY` | `/models` |
| `tencent-coding` | Tencent CodeBuddy plan | `https://api.lkeap.cloud.tencent.com/coding/v3` | `TENCENT_CODING_PLAN_API_KEY` | `/models` |
| `volcengine-coding` | Volcengine (Doubao) coding plan | `https://ark.cn-beijing.volces.com/api/coding/v3` | `ARK_CODING_PLAN_API_KEY` | `/models` |
| `stepfun-plan` | StepFun Step plan | `https://api.stepfun.ai/step_plan/v1` | `STEPFUN_API_KEY` | `/models` |
| `stepfun-plan-cn` | StepFun Step plan (China) | `https://api.stepfun.com/step_plan/v1` | `STEPFUN_API_KEY` | `/models` |
| `cline` | Cline Pass | `https://api.cline.bot/api/v1` | `CLINE_API_KEY` | `/models` |

For example, `chanto login kimi-coding`, then `chanto -m kimi-coding/kimi-for-coding`. `/models` lists what the plan's API reports, or the vendor-documented models above if it has no list. Kimi's `k3` reaches a 1M window only on higher membership tiers, so Chanto budgets 256K for it. The `-cn` providers use the mainland-China endpoint with the same key, so pick whichever matches where your account was created. MiniMax's M Plan is Anthropic-compatible, so point `[providers.minimax-coding]` at `https://api.minimax.cn/anthropic/v1` (China) or `https://api.minimax.io/anthropic/v1` with `kind = "anthropic"` if your key only works there.

### Usage and provider errors

`/usage` (alias `/cost`) reports session tokens, spend and any account limits reported by the provider. The footer shows the most-used reported window, its remaining percentage, a local reset time when available, and `/usage` when space permits:

```text
5h limit: 27% left · resets at 10:09 PM · /usage
```

On a narrow terminal, reset details and the hint drop before the percentage. At **25% remaining**, the quota turns amber and a one-time heads-up appears; at **10% remaining**, it turns red and warns once more. Warnings re-arm after a reset. Weekly limits can take precedence when they are more exhausted than the 5-hour window. `/usage` lists all reported windows.

These are the provider’s last reported values, read from response headers or stream events; Chanto does not poll a separate quota endpoint or estimate missing percentages. If no limits have been reported, `/usage` says so. An elapsed reset timestamp displays **reset due** until a fresh report arrives; it does not imply that the allowance has already replenished. The separate `ctx` percentage is conversation-context usage, not your plan’s remaining allowance. Quota banners are for the user and are not automatically added to the model prompt. Limits and resets are enforced by the provider and depend on your plan.

**Provider safety notices.** Refusal text and links appear in the streamed reply and transcript when the provider supplies them. A safety-filter stop without an explanation shows a clear fallback notice. Filtered turns end without automatic retries, tool execution or continuation through verification/stop hooks. Provider-specific access-program wording is shown only when supplied; Chanto does not invent it.

**Waiting out a usage limit.** When a plan's 5-hour or weekly window runs out mid-turn (`usage_limit_reached`, with a reset time from the error, `Retry-After`, or the last reported limits), Chanto can keep the session and continue the same turn when the window resets instead of failing it. With `resume_after_limit = true` it always waits; when unset, the TUI asks "Wait and continue at 14:05?" (a reset on another day shows the day), and `-p` runs fail as before. While waiting, the status line shows `⏸ usage limit · resuming at 14:05` and the spinner counts down; `esc` stops waiting and ends the turn. When it resumes, Chanto sends a desktop notification (the `notifications` setting) and fires a `Notification` hook with `notification_type: "limit_resumed"`. Billing quotas (`insufficient_quota`) don't reset on a clock, so they still fail right away, and a reset more than eight days off isn't waited for.

```toml
resume_after_limit = true   # false: never wait; unset: ask in the TUI
```

**Several accounts per provider.** `chanto login <provider> --as <name>` saves another login for the same provider (a work and a personal ChatGPT plan, two API keys) beside the default one; `--with-api-key` and the browser logins take `--as` too, and `chanto logout <provider> --as <name>` removes one. `account = "<name>"` in the config (or a profile) uses that login for every provider that has one by that name, and the default login for the rest. In a session, `/login` lists saved logins with the one in use marked, `/login <provider> <name>` switches for the rest of the session (`default` goes back to the unnamed one), `/login chatgpt --as <name>` signs in as a new account and switches to it, and `/status` shows the account in use. Named logins are stored in `~/.chanto/auth.json` as `<provider>@<name>` entries, so older Chanto versions keep reading the default ones. Listings never show keys or tokens. A project config can set `account` only once the folder is trusted.

### Hosted provider behavior

How the hosted providers behave:
- **Anthropic:**
  - Prompt caching is on, with breakpoints on the system prompt, the tool list and the latest user turn.
  - Thinking is adaptive on current models and uses a thinking budget on older ones.
  - Thinking blocks and their signatures are sent back unchanged with the turn they belong to.
- **OpenAI Responses:**
  - Requests are stateless (`store: false`).
  - Reasoning models get `reasoning.effort` and return encrypted reasoning, which is passed back with the following turns.
- **Both:**
  - Rate-limit and overload errors (429, 529, 5xx) are retried, honoring `retry-after`.
  - The context budget defaults to the model's window (from Anthropic's Models API, or a per-model default for OpenAI) instead of `num_ctx`.

### Cloud providers (Bedrock, Vertex AI, Azure)

Organizations that must use their cloud's model endpoints can select them like any other provider:

An AWS profile or gcloud login on your machine is not taken as a choice to use it: `/model` and `chanto models` list a cloud platform's models only once you opt in, with a key saved by `chanto login <platform>`, the platform's API-key variable (`AWS_BEARER_TOKEN_BEDROCK`, …), a `[providers.<platform>]` section, a default model on that platform, or `CHANTO_USE_BEDROCK=1` / `CHANTO_USE_VERTEX=1` / `CHANTO_USE_AZURE=1` / `CHANTO_USE_FOUNDRY=1` (Claude Code's `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX` and `CLAUDE_CODE_USE_FOUNDRY` count too). `chanto setup` still offers them when it finds credentials, and `chanto providers` shows what it found.

| Spec | Platform | Wire API | Credentials |
|---|---|---|---|
| `bedrock/<model-id or inference profile>` | Amazon Bedrock | Claude: `InvokeModelWithResponseStream` (Anthropic Messages). Others: `ConverseStream` | AWS credential chain (SigV4), or a Bedrock API key |
| `vertex/<model>` | Google Vertex AI | Claude: `streamRawPredict` (Anthropic Messages). Gemini and Model Garden: OpenAI-compatible endpoint | Application Default Credentials |
| `azure/<deployment>` | Azure OpenAI (v1 API) | chat completions, or Responses for `codex` and `-pro` deployments | API key or Microsoft Entra ID |
| `foundry/<model>` | Azure AI Foundry | Claude: Anthropic Messages. Others: OpenAI-compatible chat completions | API key or Microsoft Entra ID |

Credentials are resolved on the first request to a cloud model, never at startup, and cached until five minutes before they expire. Claude on every cloud gets the same prompt caching, thinking and tool handling as the Anthropic API. The context budget for Claude on a cloud is 200k tokens. `chanto doctor` shows where credentials come from (never their values), the region, project or endpoint, and the result of a check that spends no tokens. `chanto providers` shows the same in one line per cloud. Setup suggests a cloud only when credentials and a region, project or endpoint are present, and ranks it after local models.

Optional settings, in `~/.chanto/config.toml`. A section named after a platform needs no `kind`. A section with another name needs a `kind` (`kind = "bedrock"`, for example) and can point at a separate account:

```toml
[providers.bedrock]
region = "us-east-1"        # default: AWS_REGION, AWS_DEFAULT_REGION, then the profile's region
profile = "ml-dev"          # default: AWS_PROFILE, then "default". If set, it wins over AWS_* key variables
# api = "converse"          # force the Converse API (default: Anthropic Messages for Claude model IDs and ARNs that name one)
# base_url = "https://vpce-….bedrock-runtime.us-east-1.vpce.amazonaws.com"   # PrivateLink endpoint

[providers.vertex]
project = "my-project"      # default: GOOGLE_CLOUD_PROJECT, then the service account's or gcloud's project, then the ADC quota project
region = "us-east5"         # default: CLOUD_ML_REGION, then "global"

[providers.azure]
resource = "my-openai"      # https://my-openai.openai.azure.com. Default: AZURE_OPENAI_ENDPOINT
# api = "responses"         # or "chat". Default: Responses for codex and -pro deployments only
# api_version = "preview"   # appended as ?api-version=; the v1 API needs none

[providers.foundry]
resource = "my-foundry"     # https://my-foundry.services.ai.azure.com. Default: AZURE_FOUNDRY_RESOURCE
```

Costs: Claude on Bedrock, Vertex AI and Foundry is priced like the Anthropic API, and Azure deployments named after an OpenAI model are priced like OpenAI. Regional-endpoint and data-zone premiums are not modeled. Common Bedrock models (gpt-oss, Nova, Llama 3.3, DeepSeek R1) and Vertex Gemini 2.5 models have prices. Other models show as unknown; add them under `[pricing]`.

#### Amazon Bedrock

```sh
export AWS_PROFILE=ml-dev AWS_REGION=us-east-1      # or AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY[/AWS_SESSION_TOKEN]
chanto -m bedrock/us.anthropic.claude-sonnet-4-5-20250929-v1:0
chanto -m bedrock/openai.gpt-oss-120b-1:0           # Converse
```

- **Model IDs.** Newer models are invoked through an inference profile: `us.`, `eu.` or `apac.` for a geography, or `global.`. Use one of these instead of the base model ID (`anthropic.claude-…`). Application inference profile ARNs also work. Set `api = "anthropic"` when the ARN doesn't name a Claude model. Enable each model under *Model access* in the Bedrock console for the region. `/models` lists the profiles and on-demand models the account can call, if `[providers.bedrock]` is configured.
- **Credentials**, in order:
  1. A Bedrock API key: `AWS_BEARER_TOKEN_BEDROCK`, `chanto login bedrock`, or `api_key` in the section. It is sent as a bearer token.
  2. The profile named in Chanto config.
  3. `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`/`AWS_SESSION_TOKEN`.
  4. The `AWS_PROFILE` (or `default`) profile in `~/.aws/credentials` and `~/.aws/config`: static keys, `credential_process`, or an IAM Identity Center session from `aws sso login`.
  5. Container credentials (ECS/EKS) and EC2 instance profiles (IMDSv2).

  Profiles that need anything else (assume-role chains, web identity) are resolved by running `aws configure export-credentials`, which needs AWS CLI 2.9 or later. `AWS_CONFIG_FILE`, `AWS_SHARED_CREDENTIALS_FILE`, `AWS_ENDPOINT_URL_BEDROCK_RUNTIME` and `AWS_ENDPOINT_URL_BEDROCK` are honored.
- **IAM permissions:** `bedrock:InvokeModelWithResponseStream` on the foundation models and inference profiles you use. Cross-region profiles need it on the profile ARN and on the foundation model in each destination region. Optional: `bedrock:ListInferenceProfiles` and `bedrock:ListFoundationModels`, for `/models` and the `chanto doctor` check. Without them doctor reports a permission note, and requests still work.

```json
{"Version": "2012-10-17", "Statement": [
  {"Effect": "Allow", "Action": ["bedrock:InvokeModelWithResponseStream", "bedrock:InvokeModel"],
   "Resource": ["arn:aws:bedrock:*::foundation-model/*", "arn:aws:bedrock:*:<account-id>:inference-profile/*"]},
  {"Effect": "Allow", "Action": ["bedrock:ListFoundationModels", "bedrock:ListInferenceProfiles"], "Resource": "*"}]}
```

#### Google Vertex AI

```sh
gcloud auth application-default login                # or GOOGLE_APPLICATION_CREDENTIALS=/path/to/key.json
export GOOGLE_CLOUD_PROJECT=my-project CLOUD_ML_REGION=global
chanto -m vertex/claude-sonnet-4-5@20250929
chanto -m vertex/gemini-2.5-pro                     # sent as google/gemini-2.5-pro
chanto -m vertex/meta/llama-4-maverick-17b-128e-instruct-maas
```

- **Model names.** Claude models use Vertex's names (`claude-sonnet-4-5@20250929`) and must be enabled in Model Garden for the project. A bare Gemini name gets the `google/` publisher prefix. Other Model Garden models are given as `publisher/model`. For `global`, requests go to `aiplatform.googleapis.com`. For any other region, they go to `<region>-aiplatform.googleapis.com`. Claude is available in `global` and in specific regions such as `us-east5` and `europe-west1`.
- **Credentials:** Application Default Credentials. User credentials (`authorized_user`) are refreshed with their refresh token. Service account keys sign an RS256 JWT locally. Other types (workload identity federation, impersonation) and machines without an ADC file run `gcloud auth application-default print-access-token` or `gcloud auth print-access-token`. `chanto login vertex` doesn't apply, since there are no keys.
- **IAM:** `roles/aiplatform.user` (Vertex AI User) on the project, which includes `aiplatform.endpoints.predict`. The project needs the Vertex AI API (`aiplatform.googleapis.com`) enabled. The doctor check reads the project's location (`GET …/projects/<project>/locations/<region>`). If it reports a permission note while requests work, the role is narrower than Vertex AI User.

#### Azure OpenAI and Azure AI Foundry

```sh
export AZURE_OPENAI_ENDPOINT=https://my-openai.openai.azure.com AZURE_OPENAI_API_KEY=…
chanto -m azure/my-gpt-5-deployment                 # the deployment name, not the model name

export AZURE_FOUNDRY_RESOURCE=my-foundry AZURE_FOUNDRY_API_KEY=…
chanto -m foundry/claude-sonnet-4-5                 # the deployment name in Foundry
```

- **Azure OpenAI** uses the v1 API (`https://<resource>.openai.azure.com/openai/v1/`) with the `api-key` header. `AZURE_OPENAI_DEPLOYMENT` adds your deployment to `/model`. Requests send `max_completion_tokens`, and optional parameters a deployment rejects (such as `temperature` on reasoning models) are dropped and the request is retried.
- **Foundry** sends Claude to `https://<resource>.services.ai.azure.com/anthropic/v1/messages` with `x-api-key`. Other models go to `…/openai/v1/chat/completions`. `ANTHROPIC_FOUNDRY_RESOURCE`, `ANTHROPIC_FOUNDRY_API_KEY` and `ANTHROPIC_FOUNDRY_BASE_URL` are accepted too.
- **Without an API key**, Chanto uses a Microsoft Entra ID token for `https://cognitiveservices.azure.com`. It comes from a service principal (`AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`), or else from the Azure CLI (`az login`, then `az account get-access-token`). The Azure CLI is optional and only needed for this path.
- **Roles:** with Entra ID, the identity needs **Cognitive Services OpenAI User** (Azure OpenAI) or **Azure AI User** (Foundry) on the resource. With an API key, no role is needed.

#### Live smoke test

The test suite never contacts a cloud and never reads your real credentials. An opt-in test makes one small real request per configured cloud, using your environment:

```sh
CHANTO_LIVE_CLOUD=1 CHANTO_LIVE_BEDROCK_MODEL=us.anthropic.claude-haiku-4-5-20251001-v1:0 \
  CHANTO_LIVE_VERTEX_MODEL=gemini-2.5-flash CHANTO_LIVE_AZURE_MODEL=<deployment> CHANTO_LIVE_FOUNDRY_MODEL=<deployment> \
  cargo test live_cloud_smoke -- --nocapture
```

## Benchmarks

The benchmark suite lives in its own repository, [chanto-sh/benchmarks](https://github.com/chanto-sh/benchmarks): coding tasks in three tiers, each with an automated check: `core` (small exercises in several languages, plus trap tasks), `medium` (hand-written bug fixes, a feature, a cross-file refactor, test writing checked with mutation testing, codebase Q&A, a CSV CLI) and `hard` (real GitHub issues from SWE-bench Lite). Clone it next to this repository and run it against several agents on the same local model, or on a hosted model through the benchmark proxy:

```sh
cargo build --release                    # in this repository
git clone https://github.com/chanto-sh/benchmarks.git ../benchmarks
cd ../benchmarks
python3 run.py --agents chanto,opencode,claude --model qwen3.8-bench
./proxy.sh deepseek/deepseek-flash       # in another terminal
python3 run.py --api http://localhost:4000 --agents chanto,opencode,claude,codex --tier hard
```

`--tier` picks tiers (default all), `--tasks` narrows by name, and `--estimate` prints the projected cost and time without running anything.

See [RESULTS.md](https://github.com/chanto-sh/benchmarks/blob/main/RESULTS.md) for the latest results, per-task numbers, methodology and caveats.

To measure the agent’s own CPU and memory overhead with an instant mock model, run the published harness from the benchmark repository:

```sh
python3 reproduce.py --agents chanto --no-install --chanto ../chanto/target/release/chanto
```

The default workload is 40 tool turns reading 40 KB files, repeated 10 times. It reports median process-tree CPU time per model request and median peak process-tree resident memory sampled every 60 ms; the final answer is also a model request. Results record versions, machine details and every run. Component microbenchmarks are separate measurements and do not establish a whole-agent CPU or memory improvement.

Every pull request that changes Rust also passes a performance gate on Linux, macOS and Windows (`tests/perf/gate.py`, workflow `perf`). It measures the same two things in this repository: process CPU and peak memory of one `-p` request, and CPU per request and peak memory of the 40-turn workload. The pull request and its base commit are both built and run alternately on the same runner, so the comparison is never between two machines; each platform also has its own ceilings, because process startup and peak memory differ between them. A metric fails when it is over the fixed ceiling in `tests/perf/budgets.json`, when it is significantly slower than the base by a one-sided Mann-Whitney U test (α = 0.01) and outside the tolerance, or when it is more than 10% slower whatever the noise. The ceilings are set from the slowest GitHub runner measured, so they do not fail on hardware variation; the comparison with the base is what holds a small change. The Linux job also re-runs the workload pinned to one core, against its own wall-time and 256 MB memory ceilings: a phone or a cheap laptop has no spare cores and little memory, and nothing else in the gate says whether Chanto still runs there. The table goes on the run's summary page.

To measure a local build against the ceilings, and against another binary you have:

```sh
cargo build --release
python3 tests/perf/gate.py --head target/release/chanto
python3 tests/perf/gate.py --head target/release/chanto --base path/to/older/chanto
python3 tests/perf/gate.py --head target/release/chanto --device
```

`--device` adds the small-device run (one core, via `taskset` on Linux).

Benchmark variants turn on the experimental optimizations below: `chanto+concise`, `chanto+quiet`, `chanto+provenance`, `chanto+all`, or any combination joined with `+`. They run the same command as `chanto` with `CHANTO_OPTIMIZE` set, and each result and the run's metadata record which switches were on. `--chanto-effort LEVEL` passes `--effort LEVEL` to every Chanto agent in the run.

## Optimizations (experimental)

Speed and token optimizations, each with its own switch so it can be measured on its own against the benchmark. Most are **off by default**; `adaptive_effort` and `stable_prefix` are on. Set them in config, or for one run with `CHANTO_OPTIMIZE=concise,quiet,provenance,spec,defer,token,noprefix,noadaptive` (`all` turns on the first three):

```toml
[optimize]
concise_prose = true      # brief prose replies
quiet = true              # quiet mode: no narration, one short summary, a quiet view (see below)
provenance = true         # flag numeric claims and identifiers not found in this turn's tool outputs
spec_check = true         # treat a written spec as the contract and exercise the result against it
defer_tools = true        # keep rarely used built-in tools behind ToolSearch
token_classifier = true   # auto mode: settle ALLOW from a one-token answer's probabilities
adaptive_effort = false   # on by default: low effort once a turn is under way
stable_prefix = false     # on by default: per-project context goes in the first user message
```

- **`concise_prose`** adds a short instruction to the system prompt: lead with the result, no recaps of what was done or restating the task, short status updates between tool calls. It says explicitly that it never applies to code, tool arguments, file contents or reasoning. It's part of the session's fixed system prompt, so the prompt prefix (and a local server's cache) stays the same from request to request. It's skipped when the Concise output style is already selected, which says the same.
- **`quiet`** (quiet mode) minimizes output tokens and screen noise. It's off by default and being measured (`chanto+quiet`).
  - *Model side* (`quiet = true`, `CHANTO_OPTIMIZE=quiet`): a fixed system-prompt instruction not to write narration or status text between tool calls and to call tools directly, then, when the task is done, to write one short summary: what changed (the files), what was verified and how, and anything left or uncertain. It says explicitly that it never applies to code, tool arguments, file contents or reasoning, or to questions the model genuinely needs to ask you. It's part of the session's fixed system prompt, so the prefix stays the same from request to request; `/quiet` rebuilds it once, from the next turn.
  - *`--quiet`* (or `/quiet on|off` during a session) turns on the model side and a quiet view. In the TUI, while the agent works you see only the spinner and the status line (the tool running now, how many tool calls so far, elapsed time and tokens), plus permission prompts and questions. The turn's intermediate text, thinking and tool blocks stay out of the transcript but are still recorded: ctrl+o or `/transcript` shows them (inline mode prints the last turn's steps), and the session history is intact, so "what did you do?" works as usual. The final summary shows normally, and spend-guard notices, warnings, errors and verification results always show. `--plain` prints the same way (`/transcript` lists the last turn's steps), and `-p` text prints only the final summary even with `--verbose`; `--output-format json` and `stream-json` are unchanged.
  - *Preset*: `--quiet` without `--mode` starts in auto mode, so safety is auto mode's: risky actions still ask, and the sandbox still applies. An explicit `--mode` (or `--dangerously-skip-permissions`, `--full-auto`, `--sandbox`) wins. `quiet = true` in config (or `CHANTO_OPTIMIZE=quiet`) turns on the model side and the quiet view but leaves the mode alone.
- **`provenance`** (`chanto+provenance`) checks the final answer's numbers, environment-variable names, file paths and function-call identifiers against tool output from the current turn. When a claim isn't found, the model is told once more and asked to re-check it against the code or the tool output (a single bounded bounce, so it never loops); the warning is always shown, and nothing is blocked or re-asked again. This deterministic prototype can miss unsupported prose or flag a valid claim expressed differently; user messages, earlier turns and answers given without any tool call are not checked.

- **`adaptive_effort`** (on by default): from the first reply in a turn that only called tools, later calls in that turn ask for `low` reasoning effort, so the effort changes at most once a turn. `false` keeps the provider's default on every call.
- **`stable_prefix`** (on by default): only Chanto's fixed instructions go in the system message; the environment, git status, skills, subagents, MCP servers and instruction files go at the top of the first user message, so the system message and tool list are cached across sessions and directories.

OpenAI's Predicted Outputs aren't included: the `prediction` parameter is a Chat Completions feature of GPT-4o and GPT-4.1 models only, it can't be combined with `tools` (function calling), and the file content a tool call is about to write isn't known before the model writes it, so there's no sound way to use it in a tool-calling agent loop.

## Architecture

```
src/
  main.rs         CLI, setup, print mode
  agent.rs        agent loop, permission gating, parallel tool execution, compaction, subagents
  llm.rs, llm/    streaming clients: Ollama, OpenAI-compatible, Anthropic, OpenAI Responses, Bedrock/Vertex/Azure; text tool-call parsing
  cloud/          cloud credentials (AWS SigV4, Google ADC, Azure Entra ID)
  permissions.rs  modes, rules, safe-command analysis, auto-mode classifier
  tools/          Read/Write/Edit/MultiEdit, Glob/Grep/Ls, Bash, Web, Browser (CDP), Computer, Todo/Ask/Plan/Skill, Task
  lsp.rs          language server client (post-edit diagnostics, Lsp tool)
  mcp.rs          MCP client (stdio + HTTP), resources, prompts
  mcp_auth.rs     MCP OAuth (discovery, registration, PKCE, tokens)
  hooks.rs        user hooks
  context.rs      system prompt, AGENTS.md/CLAUDE.md, skills, commands, agents
  session.rs      persistence
  server.rs       HTTP API
  runs/           background, remote and queue runs (daemon, worktrees, containers)
  tui/            ratatui frontend (markdown renderer, input editor)
  plain.rs, repl.rs  line-based frontends
```

### Visual UI tests

The `visual` workflow checks 72 deterministic terminal frames: 12 UI states at three widths in dark and light themes. Baselines include cell colors and modifiers as well as text. Covered states include a successful exchange, streaming, background jobs, quota warnings, dialogs, completion and scrolling.

```sh
cargo test --locked --lib visual_snapshots -- --ignored --test-threads=1
```

The separate invocation keeps process-global theme changes isolated from normal unit tests. CI uploads `chanto-ui-previews`, a gallery of PNGs/GIFs plus captured text, styles and pending snapshot diffs. Download the artifact and open `index.html`. Snapshots are the regression check; the previews show those captured frames and do not replace live-terminal or behavior tests. [Visual-test instructions](https://github.com/chanto-sh/chanto/blob/main/tests/visual/README.md) explain local previews and intentional baseline updates.

## License

Chanto is open source under the [Apache License 2.0](../LICENSE): the CLI, the server and the [benchmark suite](https://github.com/chanto-sh/benchmarks). Anything that runs on one machine for one person stays open source.

## ACP client

`chanto acp-client <command...>` runs an external agent under Chanto's own UI. The command must speak [Agent Client Protocol](https://agentclientprotocol.com) version 1 over stdio, which is what the Claude Agent SDK adapter (`claude-agent-acp`), OpenCode and others do:

```sh
chanto acp-client claude-agent-acp
chanto acp-client opencode acp
```

Chanto is the ACP client here. The agent's session starts in the current directory, so it reads the project instructions it normally uses (`CLAUDE.md`, `AGENTS.md`, its own configuration). Its text, reasoning, tool calls, diffs and plans stream into the TUI exactly like Chanto's own, its permission requests become the same permission dialogs, and `/clear` starts a fresh agent process and session.

The agent runs its own loop, so Chanto's `/model`, tools, hooks, subagents, context compaction, session files and usage accounting do not apply. Chanto answers `fs/read_text_file` and `fs/write_text_file` requests, and the `terminal/*` methods: an agent that asks for a command gets it run through Chanto's own environment, with its output available to the agent and the terminal killed when the session ends. Your configured MCP servers are handed to the agent at session start (variables expanded, and disabled or gated servers left out), except HTTP or SSE servers when the agent doesn't advertise those transports. A form elicitation becomes the normal question dialog, and a URL elicitation shows the address and asks before confirming. The agent's `config_option_update` and `available_commands_update` notifications update what Chanto knows and appear as a one-line note. If the agent process restarts, Chanto restores its previous session with `session/load` or `session/resume` (history the agent replays is not shown twice), and `/clear` closes it explicitly when the agent supports `session/close`. The agent's session id is saved with the Chanto session, so `/resume` (and `--continue`) restores the agent's side of the conversation too. Modes and settings the agent advertises when the session starts are listed once: `/agent-mode <id>` and `/agent-config <id> <value>` change them, and without arguments they list the available modes and settings with their current values. `/agent-login [method]` and `/agent-logout` use the agent's own sign-in methods; when it reports that authentication is required, Chanto points at `/agent-login`. When the agent advertises slash commands, they appear in completion and `/help` and are sent to it as prompts; Chanto commands of the same name win. The agent's stderr is kept only for error messages (the last 20 lines) and is not shown in the transcript. If a turn is cancelled, Chanto asks the agent to stop (`session/cancel`) and terminates it if it has not stopped within 10 seconds.

## Managing spend

The spend guard watches for work that multiplies cost, says so before and while it happens, and explains it afterwards. It never changes anything on its own: every mitigation is something you pick.

**Before fan-out.** When a turn is about to start several subagents (Task calls in one batch, or one next to agents already running, e.g. from `/batch`), when you start a `/team`, or when you start a `/loop`, Chanto estimates the cost first:

> estimate = the context each agent starts with × the calls it's expected to make × agents × the model's price

Each agent starts with the system prompt and tools plus its task, and grows by about 3k tokens a call. Its first call writes that prompt to the provider's cache and later calls read it back, at the cache-read and cache-write rates where the provider has them (input rates otherwise). Calls per agent default to 15, and once you've run a few subagents Chanto uses the average of your own (kept in `~/.chanto/cache/spend.json`). Agents run on the subagent model: the agent definition's `model`, `subagent_model`, or the main model. A `/loop` is priced as six calls on the current context per run, times the runs in a day; a self-paced loop is assumed to run every 10 minutes.

Prices that aren't known are never guessed. Those estimates show tokens, and on a plan that reports usage windows (ChatGPT) the share of the window. Chanto learns that share from how fast the window fills as the session uses tokens: e.g. `~2.1M tokens, ~14% of your 5h window`. Local models are free and never asked about.

Above `confirm_over_usd` (default $2), or `confirm_over_plan_percent` of a plan window (default 10%), or `confirm_over_tokens` when neither is known (default 5M), the TUI asks:

```
Spend check: starting 4 subagents on anthropic/claude-opus-5-5 (~15 calls each): ~$3.10 (2.3M tokens), over your $2.00 confirm threshold.
❯ Continue (~$3.10 (2.3M tokens))
  Use anthropic/claude-sonnet-5-5 for the agents (~$1.40 (2.3M tokens))
  Run at most 2 at a time (same total, slower to burn, easier to stop)
  Cancel
```

- **Use a cheaper model** is offered when `[spend] cheap_model`, or the next tier down of the same family (Opus → Sonnet → Haiku, GPT-6 Astra → Sol → Luna, …), has a known, lower price. Subagents then run on it for the rest of the session.
- **Run at most N at a time** caps how many agents run at once for the rest of the session (`limit_agents`, default 2). The total is the same; it burns slower and is easier to stop part-way.
- **Cancel** doesn't start them; the model is told why and does the work directly or with fewer agents.

After you continue, a similar fan-out within 10 minutes isn't asked about again unless it's more than twice as big. `-p` and background runs follow `non_interactive`: `"warn"` (default) prints the estimate as a warning and continues, and `"stop"` refuses to start the fan-out and says so. Neither is silent. `/team` shows the estimate in its plan approval, next to the team budget that caps it.

**While it runs.** The guard tracks $/min (or plan-% an hour) for the session and each agent, over a rolling 5-minute window. It flags each of these once per session, with a one-line note in the transcript and a `$ …` marker in the footer:

| Pattern | Example notice |
|---|---|
| many agents at once (`parallel_agents`, default 4) | `Spend: 5 agents running in parallel; each resends its own context on every call, so cost grows with the count. /cost limit 2 caps how many run at once.` |
| the same large file (4k+ tokens) read again and again (`repeated_reads`, default 3) | `Spend: src/parser.rs (~12k tokens) has been read 3 times; every copy stays in context and is resent on each call. Read the part you need (offset/limit) or keep one summary.` |
| a context above `resent_context_percent` of the window (default 50%) resent on five calls in a row | `Spend: the main agent's context is ~150k tokens (75% of the window) and is resent on every call, ~$0.05 a call even with most of it cached. /compact or a fresh session cuts that.` |
| the same tool call with the same result four times in an agent's last dozen | `Spend: the same Grep call ran 4 times in the Explore: map the parser with the same result: no progress, but each repeat is another model call over the whole context.` |
| spend above `burn_usd_per_min` (default $0.50) or `burn_plan_percent_per_hour` (default 25%) | `Spend: ~$0.62/min over the last 3m across 6 agents (above $0.50/min). /cost shows where it goes.` |

`notices = false` turns them off. Detection works on counters the agent loop already keeps, so it adds no model calls.

**Afterwards: `/cost`.** Besides the totals, budgets and plan limits, `/cost` breaks the session down by agent (model, calls, tokens in and cached, out, cost, $/min), by model (including cache writes), by tool (tokens their results added to the context) and by the large files read more than once. It ends with suggestions from what it saw:

- `Subagents ran on anthropic/claude-opus-5-5 for $4.12 (63% of this session's spend); anthropic/claude-sonnet-5-5 would have cut that by ~50%.`
- `Context was resent 340 times at ~180k tokens (90%+ of the window): /compact or start fresh when a task is done.`
- `Up to 8 agents ran in parallel: /cost limit 2 caps that.`

`/cost today` adds up every session's model calls today, by model, from the log. `/cost cheaper [model]` runs subagents on a cheaper model for the rest of the session (`/cost cheaper off` undoes it), and `/cost limit [N]` caps agents at once (`/cost limit off`). `/compact` shrinks the context.

**Budgets.** `session_usd`, `day_usd` and `month_usd` cap spend per session, per local calendar day (from the model-call log) and per calendar month (saved sessions last active this month, plus this one). They're the same budgets as `max_cost_usd` and `max_cost_usd_per_day` (which win when both are set), and the same checks: `/goal`'s `max_cost_usd`, background runs' and the runner's budgets all count against them too. A note at 50% and 80% of the tightest budget, and at 100% the turn stops before its next model call: interactively you're asked whether to continue, and `-p` and background runs stop.

```toml
[spend]
confirm_over_usd = 2.0
confirm_over_plan_percent = 10
confirm_over_tokens = 5000000
non_interactive = "warn"        # or "stop"
# calls_per_agent = 15          # default: learned
# cheap_model = "anthropic/claude-haiku-5-5"
limit_agents = 2
notices = true
burn_usd_per_min = 0.5
burn_plan_percent_per_hour = 25
parallel_agents = 4
repeated_reads = 3
resent_context_percent = 50
# session_usd = 5.0
# day_usd = 20.0
# month_usd = 200.0
```

## Costs, budgets and telemetry

Chanto prices model calls in USD, enforces budgets, writes local logs and can
export telemetry.

`/cost` (also `/usage`), the TUI footer, and `chanto insights` show USD totals.
JSON/stream-json results and server usage and turn events expose
`total_cost_usd`; `price_unknown` marks totals containing unpriced calls.
Provider-reported cost (including OpenRouter `usage.cost`) takes precedence.
The bundled table uses exact model IDs and standard token rates; unknown IDs
show tokens and **price unknown**. Local Ollama, LM Studio and llama.cpp calls
are free. Subscription/custom endpoints without token prices need overrides.
Prices also come from a list published at
https://chanto.sh/prices.json, so they can change without a new
release: Chanto keeps the last copy in `~/.chanto/cache/prices.json` and
fetches it at most once a day in the background (a plain GET that sends
nothing about you; off with `[updates] check = false` or
`CHANTO_NO_UPDATE_CHECK=1`). Your `[pricing]` overrides come first, then that
list, then the built-in table.
Cached input and cache writes are deducted from ordinary input before pricing.
Rates are USD per million tokens; overrides without cache rates use the input
rate for those tokens.

```toml
max_cost_usd = 2.0              # per session; --max-cost overrides this
max_cost_usd_per_day = 10.0     # across sessions under CHANTO_HOME

[pricing."deepseek/deepseek-flash"]
input = 0.14
output = 0.28
cached_input = 0.014
# cache_write = 0.14

# Omit this section to keep telemetry off.
[telemetry]
otlp_endpoint = "http://localhost:4318"
service_name = "chanto"
[telemetry.headers]
Authorization = "Bearer collector-token"
```

Budgets note at 50% and 80% and stop at 100% (see [Managing spend](#managing-spend)
for `[spend]`'s session, day and month budgets). Interactive sessions ask whether to
continue beyond the configured limits for that session. A call already in
flight may exceed a limit; checks run before calls and after completed calls,
before running requested tools. Concurrent calls already in flight can also
overshoot. Unknown prices do not count as zero-priced calls: the partial total
is explicitly marked unknown and cannot enforce a complete dollar budget.
Daily spend uses the current local calendar date and the model-call log.

`chanto logs` prints `~/.chanto/logs/chanto-YYYY-MM-DD.log`;
`chanto logs --tail` follows it across daily rotation. `CHANTO_HOME` relocates
all Chanto data. JSON lines contain time, level, session ID, event and data:
model calls record provider, model, input/output/cache tokens, cost, latency
(seconds), and status; tool calls record name, duration (seconds), and ok;
permission decisions record tool name and allow/deny/stop; errors record status.
Prompts, responses, tool arguments and error bodies are excluded. Values pass
through secret redaction even when transcript redaction is disabled. Files are
created at mode 0600 on Unix. Writes remove Chanto log files older than the
last 14 calendar days. `--debug` or `CHANTO_LOG=debug` mirrors structured
records to stderr. Logs are local and independent of telemetry.

Opt-in export sends OTLP/HTTP JSON to `/v1/traces` and `/v1/metrics` under the
configured endpoint. **Exactly these fields are sent:** service name, event name
(`model_call` or `tool_call`), random trace/span IDs, start/end timestamps,
provider, model, input/output/cached/cache-write token counts, USD cost when
known, latency or duration, status or ok, and tool name. Model/token/cost metrics
and duration metrics carry those same attributes. No session IDs, filesystem
paths, prompts, responses, tool arguments, errors or credential values are
exported. Configured headers authenticate requests; they are never included in
payloads or logs. Export failure logs only the signal name and does not fail a
turn; requests time out after two seconds per signal. `/privacy-settings` shows
whether export is enabled. No OpenTelemetry dependencies are required.

Bundled prices are a snapshot, not an invoice; use overrides when your contract,
region, routing, batch/service tier, long-context or caching policy differs.
Primary price references: [OpenAI](https://platform.openai.com/pricing),
[Anthropic](https://docs.anthropic.com/en/docs/about-claude/pricing),
[Gemini](https://ai.google.dev/gemini-api/docs/pricing),
[DeepSeek](https://api-docs.deepseek.com/quick_start/pricing/),
[xAI](https://docs.x.ai/developers/models),
[Mistral](https://docs.mistral.ai/models),
[Groq](https://console.groq.com/docs/models),
[Cerebras](https://www.cerebras.ai/pricing),
[Together](https://www.together.ai/pricing),
[Fireworks](https://docs.fireworks.ai/serverless/pricing),
[MiniMax](https://platform.minimax.io/docs/pricing/overview),
[DashScope](https://www.alibabacloud.com/help/en/model-studio/qwen-plus),
[OpenCode](https://opencode.ai/docs/zen),
[Moonshot](https://forum.moonshot.ai/t/an-update-on-new-k2-models-and-new-pricing/104),
[Baseten](https://www.baseten.co/pricing/),
[Vercel](https://vercel.com/ai-gateway/models/gpt-4.1).

ChatGPT plans, the Z.ai, Kimi Code and MiniMax M Plan coding plans, and routed/deployment-specific
Hugging Face, NVIDIA and other models without a bundled exact rate
use returned cost or explicit overrides; otherwise they show price unknown.
The table never borrows another provider's price for these endpoints.
Anthropic Sonnet 4/4.5 requests above 200K input tokens apply the published
long-context multiplier. Anthropic cache writes use the default five-minute
cache rate; custom one-hour policies require a pricing override.

## Feedback and error reports

`/feedback <message>` (also `/bug <message>`, or `chanto feedback "<message>"`
from a shell) sends a note or bug report to the Chanto developers. It works
for everyone; nothing needs turning on. Before anything is sent you see the
exact JSON and choose **Send**, **Send without log lines** or **Cancel**
(`chanto feedback` asks `[y/N]`; `--yes` skips the question, `--no-log` leaves
out the log lines). The report holds:

- your message,
- Chanto version, OS and CPU architecture, terminal (`TERM_PROGRAM`),
- provider type (`ollama`, `openai`, `anthropic`, …), model name and permission mode,
- up to 20 recent error/warning lines from the diagnostic log (`chanto logs`).

**Automatic error reports** are off unless you opt in. On the first
interactive run (and in `chanto setup`) Chanto asks once whether to send
them, and records the answer in `~/.chanto/config.toml`:

```toml
[feedback]
share_errors = true   # or false
```

`CHANTO_NO_ERROR_REPORTS=1` turns them off whatever the config says, and
`chanto doctor` and `/privacy-settings` show whether they're on. When on,
Chanto reports these known-bad events: a crash (a panic writes
`~/.chanto/crashes/<time>.json`, sent at the next start and then deleted;
with reports off the files stay local, at most ten), a model request that
fails and ends a turn (not a server that simply isn't running), the auto-mode
safety classifier being unavailable, and the shell sandbox failing to start.
Each report holds only: the kind of error, its message (first 500
characters), Chanto version, OS and architecture, provider type, a timestamp
and a random install id generated once and kept in `~/.chanto/feedback.json`.
The same kind is sent at most once an hour, and at most ten reports a day.
Reports go out in the background with a five-second timeout and never slow
or stop your session; a failure is only noted in
`~/.chanto/logs/feedback.log`.

**Diagnostic mode** is for people helping test Chanto who want everything
that goes wrong sent without being asked:

```
chanto diagnostics on --name sam     # for 24 hours; --hours N up to 168
chanto diagnostics status
chanto diagnostics off
```

(`/diagnostics on|off|status` does the same inside a session; it takes effect
the next time Chanto starts.) While it's on, every kind of error report above
goes out whatever `share_errors` says, without the hourly limit (at most 300 a
day), and every two minutes Chanto sends the new warning and error lines from
the diagnostic log (up to 50 at a time, redacted the same way). Each report
carries the `--name` you gave, so the developers know whose it is. It turns
itself off when the time runs out; `CHANTO_NO_ERROR_REPORTS=1` still wins.

**Privacy.** Every field is redacted before you see it and before it is sent:
your home directory becomes `~`, secrets are masked as in transcripts, and
URLs keep only their scheme and host (no credentials, paths or query strings).
Prompts, responses, code, file contents, the conversation, tool arguments, API
keys and base URLs are never part of a report. Reports go to
`https://chanto.sh/api/feedback` and are stored privately by the
Chanto developers; project config can't turn error reports on (the
`[feedback]` table is ignored in untrusted folders). `CHANTO_FEEDBACK_URL`
points both at another endpoint.

## Durable memory

Memory lives outside the checkout: user facts in `~/.chanto/memory/` and project facts in `~/.chanto/projects/<project-key>/memory/`. `CHANTO_HOME` overrides the Chanto directory. Each markdown file has name, description, type (`user`, `feedback`, `project`, `reference`), created and updated frontmatter. `MEMORY.md` is regenerated from the files.

The first user turn includes the user and project indexes in a `<system-reminder>` capped at 4096 bytes. Bodies are read on demand with the Memory tool. Automatic saves remember durable preferences, corrections and non-obvious facts; secrets are refused even when display redaction is disabled. Repository-derived information and temporary task state should not be saved. Writes show a dim saved/deleted line with the scope and description, and can be undone. Saves, updates and deletes in user memory (which every project loads) ask for approval outside bypass mode, showing the description and body; project memory writes don't ask.

```toml
[memory]
auto = true
scope = "project" # or "user"
```

`auto = false` disables model writes while keeping recall and manual commands available. `/memory` or `/memory list` lists both scopes. `/memory open <name>`, `edit <name> <text>`, `delete <name>`, `forget all`, and `undo [scope]` manage files. Add `--user` before the name to manage user memory. `/memory <note>` and `#note` save project memory by default; `/memory --agents <note>` (or `#--agents <note>`) still appends to AGENTS.md. `chanto memory list|show|rm` works without a model connection.

Accepted `/learned` rules migrate to feedback memories; checks retain the existing verified-mode workflow. Disabling or undoing lessons reconciles their feedback memories. Explicit edits or deletions of a migrated memory are preserved on subsequent session starts. Forget-all deletes both scopes; undo restores one deletion at a time.

### Diagnosing a slow start

`CHANTO_TIMING=1 chanto -p "hi"` prints how long each startup phase took (config, MCP connections, setup, each model and tool call) to stderr. MCP servers that needed a login are remembered for a day, so they don't cost a network round trip on every start; `/mcp login` clears that.
