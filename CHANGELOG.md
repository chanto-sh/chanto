# Changelog

## Unreleased

- **Ghost text can't stay in the transcript or composer.** A restored window, a multiplexer, or a program writing to the tty can leave its glyphs in rows Chanto drew: ratatui only rewrites cells whose content changed. The session now repaints every row after regaining focus and on the first input after a pause, so it looks clean again.
- **The mode hint no longer crowds out status.** `shift+tab changes mode` beside the mode name now appears only when it fits whole; on a narrow terminal the context meter, model and branch keep their place instead of the hint being cut to `shift+tab…`.
- **Window titles name the project.** The terminal tab title is `project — chanto` (or just `chanto` for a project with that name), so several tabs are told apart at a glance.
- **Semantic search no longer crashes on a first build.** Building the embedding index queued each chunk's slot as `entries + pending`, counting the queued chunks twice, so the vectors were written past the end of the list as soon as more than one chunk needed embedding (`index out of bounds` in `semantic.rs`). Each chunk now records the slot it was pushed into.
- **URLs read and wrap like links.** A bare `http(s)://` URL in a model reply is drawn in the link colour and underlined, and trailing punctuation (or a `)` the URL did not open) stays outside it. A URL too long for the line now breaks before `&`, `?` and `#` and after `/`, so an OAuth URL splits into readable parameters instead of mid-word (`wo`/`rkers`); it never breaks inside a `%XX` escape. Links carry the terminal's OSC 8 hyperlink, so cmd/ctrl-click opens them (where the terminal supports it), and clicking any fragment of a wrapped URL opens the whole URL. A fragment cut off by the viewport is left unlinked rather than opening a truncated URL.

## v0.1.12 (October 19, 2026)

- **An interrupted turn no longer wedges the conversation.** A tool call that never received its result (the process died, or the turn was interrupted) is repaired, with a note that the call may have run, before the next model request, both when a session is reloaded from disk and mid-session. Without it, OpenAI-style providers reject the whole history once a user message follows the call instead of its result.
- **The low-disk warning can't split a call from its results.** It is emitted before the tool calls are recorded, so it never lands between them.

## v0.1.11 (October 19, 2026)

- **Drive other agents from Chanto.** `chanto acp-client <command>` runs any ACP v1 agent (Claude Code through `claude-agent-acp`, OpenCode, …) with Chanto's own TUI, permission dialogs and tool display. The agent reads the project's own instructions from the current directory.
- **Two new ways to get oriented in a large tree.** `RepoMap` returns a ranked, token-budgeted map of the project's symbols and where they live. `SemanticSearch` embeds the codebase through the new `[embeddings]` section (a local Ollama `nomic-embed-text` by default, or any OpenAI-compatible `/v1/embeddings`), caches the vectors under `~/.chanto/cache/embeddings/`, and refreshes them as files change. Both are read-only, so they run without a prompt.
- **Verification that the tests can't be edited around.** Existing test files are snapshotted when the turn starts: deleting them, changing an assertion or a test body, or adding a skip marker is reported and fails the verified gate with "fix the code, not the tests"; new tests are fine.
- **Finishing on a failed run is caught outside verified mode too.** A final answer after the agent's own last check failed is sent back with the failure output. So is a failure it only re-ran at a narrower scope (a full suite that failed, then one file (or one other test) that passed) unless a later passing run covers the same tests.
- **Fewer phantom loops.** The identical-tool-call guard now also requires identical output, so polling a background shell or running a command whose output changes is no longer a loop. Edits within one batch that touch the same file, and no-op edits, no longer look like a file being reverted to an earlier version.
- **No more full disks.** Below 2 GB free the model is told once and asked to clean up; below 256 MB, writes and commands that would add data are refused with the free space in the error, while cleanup commands (`rm`, `cargo clean`, `git clean`, …) still run.
- **TUI.** Double-click keeps the whole word when the release lands on the same cell (it used to collapse to the click point); themes gained an accent colour; and resuming a session that died mid-tool says the last turn was interrupted instead of pretending it ended cleanly.
- **Providers.** Built-in coding plans for Qwen (international and China), Tencent CodeBuddy, Volcengine (Doubao), StepFun (international and China), Cline Pass and Kimi Code (China), each with its own key variable and login.
- **Remote control.** Commands are now numbered and authenticated per reader, and a command that is replayed, reordered or rewritten by the relay is ignored. The conversation stays end-to-end encrypted, with the link's key forgotten by the terminal once a reader joins.
- **Sharper agent guidance.** The prompt now says to reproduce a reported bug before fixing it, prefer the smallest fix an existing option or fallback already supports, treat existing behaviour and tests as the contract, and fix the cause rather than special-casing symptoms.
- **Benchmark.** The benchmark results and runner carry this round's measurements and the extra shard tooling.

## v0.1.10 (October 18, 2026)

- **Parallel sessions stay in their own lanes.** Sessions sharing a project told each other their latest prompt: the agent in one terminal read what you typed in another, both as turn-start context and in the file-conflict warning. Only the files a peer is touching now reach the model; the prompt stays for you, shown by `/peers`. `CHANTO_NO_PRESENCE=1` keeps a session out of presence entirely: nothing shared, nothing read.
- **Auto mode shows its countdown.** Before auto mode denied an unanswered permission prompt, the dialog now counts it down ("auto-deny in 12s") and redraws each second, so a prompt can't vanish without warning. `[permissions] auto_deny_after_secs` (default 75) still sets the window; `0` waits indefinitely.
- **A repository can't shorten that window.** The auto-deny timeout joined the settings an untrusted project may not set, along with the other permission keys.
- **A command that outlives its timeout keeps running.** A foreground command past its timeout used to be killed, throwing away work in progress. It now hands the running process to a background shell and returns the output so far, readable with `BashOutput` and stoppable with `KillShell`.
- **API keys can live in the macOS login keychain.** Set `CHANTO_KEYCHAIN=1` and `~/.chanto/auth.json` keeps only a placeholder; saved logins still resolve, and toggling the setting back leaves them working.
- **TUI.** Double-click selects a word and triple-click a row; Monitor watches get their own ◉ in the agents panel, distinct from one-shot commands; the busy window title is a single steady glyph instead of a title that nudged itself every frame; and a reply that merely mentions `<tool_call>` in prose keeps its text.
- **Smaller, tidier plumbing.** Hot-path regexes are compiled once instead of per call, `/btw` reuses its snapshot without a fresh allocation each model call, and the license and third-party notices are filled in (tldr-pages, syntect's bundled themes, SWE-bench; GTFOBins is excluded).

## v0.1.9 (October 17, 2026)

- **Adaptive effort.** When nothing pins an effort, a turn whose previous assistant message only called tools and said nothing is sent at low effort; reasoning is reserved for the turns that plan or answer, not the mechanical follow-ups that just run a call. An explicit session or skill effort still wins.
- **Old tool results are only cleared when it actually helps.** Clearing them needs no model call, but it rewrites history and so invalidates the prompt-prefix cache. Auto-compaction now estimates what clearing would free before mutating: if that wouldn't get under the trigger, it goes straight to summarizing instead of clearing and then compacting anyway (a second cache-busting rewrite for nothing).
- **No more `osascript` notifications.** macOS system notifications now go through `terminal-notifier` only; with it not installed, the bell is all you get. The bare `osascript` fallback posted its notification under Script Editor and opened Script Editor on a click, which was more annoying than no notification. Terminals that raise their own notification (iTerm2, Ghostty, WezTerm, kitty, foot, …) are unaffected.
- **The browser shows it's under Chanto control.** While Chanto drives Chrome, its dock/taskbar icon carries an "AI" badge (`Browser.setDockTile`), cleared when Chanto exits, so an automated browser isn't mistaken for one you're using by hand. It works whether Chanto launched Chrome or attached to a running one, and, unlike `--enable-automation`, it doesn't set `navigator.webdriver`. Set the label with `[browser] badge`, or `""` to turn it off.

## v0.1.8 (October 17, 2026)

- **Copy anywhere, not just the transcript.** Mouse selection now spans the whole frame: the prompt box and everything below it (status line, todos, the agents/work panel and the footer's badges and hints), as well as the transcript and an open agent view. The highlight is painted over the finished frame, so every widget follows the drag, and a quick press-and-release still copies.
- **The prompt wraps at word boundaries.** A line that gets too long now breaks at the last space instead of mid-word, so a wrapped paragraph stays readable; a single word wider than the box still breaks on the character rather than disappearing.
- **The free-text prompt cursor sat one row too high.** In a question dialog, choosing "type something else" put the cursor on the hint line above the input instead of in it.
- **macOS system notifications no longer open Script Editor when clicked.** `osascript` is a bare binary with no app bundle, so a click on its notification had nothing to activate and macOS opened Script Editor instead. Where [`terminal-notifier`](https://github.com/julienXX/terminal-notifier) is installed, Chanto now posts through it and passes `-activate` so a click focuses your terminal (background-run notifications open their web link). Without it, the `osascript` fallback is unchanged.
- **Fast path: broader routine coverage, rebuilt from public data.** The training set now mixes the synthetic generator with routine examples mined from a vendored [tldr-pages](https://github.com/tldr-pages/tldr) snapshot (CC BY 4.0); `classifier/mine.py` keeps only read-only local tools and drops anything carrying a risky token or a home/system path (`classifier/sources/README.md`). Read-only inspectors, shell tests, subshells and read-only `gh` calls join the routine examples. On the labelled real set it clears 75% of routine commands (was 74%) at 7 leaks (was 8), so ~99% of what it clears fast is genuinely routine. Shapes that would have taught it to flag `python3 -c`, `curl -k` or `sed -i` on a manifest for one leak each were dropped: they cost more routine coverage than they bought, and those calls belong to the model classifier.
- **Fast path: interpreter one-liners that write defer.** `python3 -c`, `node -e` and friends that write a file or launch a subprocess (`json.dump(…, open(f,'w'))`, `fs.writeFileSync(…)`, `subprocess.run(…)`) now hit a hard guard: their writes are invisible to the project-boundary check, so they go to the model classifier instead of being cleared. Reading (`open('f')`, `json.load`, `json.dumps`) still clears.
- **Removed two experimental optimizations.** `effort_by_step` and `outline_reads` didn't pay off in the benchmark (`effort_by_step` came out level with the baseline, `outline_reads` clearly worse), so they're gone: the `[optimize]` switches, the `CHANTO_OPTIMIZE=effort`/`outline` names and the `chanto+effort`/`chanto+outline` benchmark variants no longer exist. `concise_prose`, `quiet` and `provenance` remain.

## v0.1.7 (October 17, 2026)

- **Experimental optimizations, off by default.** New `[optimize]` switches cut tokens without changing behavior: `concise_prose` (brief prose, never code, tool arguments or reasoning), `effort_by_step` (full effort to plan and after a failed tool result, a step less for calls that follow a success), `outline_reads` (an unranged read of a long Rust, Python, TS/JS or Go file returns its imports and signatures first), `quiet` (skip narration between tool calls) and `provenance` (a bounded re-check of a turn's numeric, path and identifier claims against its own tool output). Turn them on per run with `CHANTO_OPTIMIZE` (`quiet,provenance`, or a `+`-combination such as `all`); `/quiet` or `--quiet` also hides a turn's steps in the TUI (`ctrl+o` or `/transcript` shows them) and `-p text` prints only the summary. The benchmark results report how each variant fared; none is enabled by default.
- **Calmer auto mode.** The safety classifier now allows ordinary development by default and blocks only named, concrete harms: data loss outside the project, forced or published-history pushes, credentials and system config, exfiltration, deploys and purchases, prompt injection. It interrupts less without loosening what it stops.
- **A tidier TUI.** Long tool output wraps under its `⎿` and stays dimmed on every row; commands and paths are highlighted in the inline-code colour; permission dialogs use a clean rule-above-and-below frame instead of vertical side bars and are sized to the rows they show; a permission's reason renders as markdown above the command; question dialogs show their key hints; the agents panel never wraps a row; the spinner no longer shifts the label; and copying after a quick mouse drag now works.
- **Fixes.** A heredoc body no longer becomes a `Bash(<line>:*)` permission rule; a Memory name may drop its `.md`; a mistyped path suggests near matches; the prompt tells the model to watch long-running work rather than launch it and move on; and a test no longer fails when `NO_COLOR` is set.

## v0.1.6 (October 17, 2026)

- **Spend guard.** Before a turn starts several agents, Chanto estimates the cost (or the share of your plan's window) and asks above a threshold: continue, use a cheaper model, run fewer at once, or cancel. Live notices name what's driving spend (many agents, a big file read again and again, a huge context resent every call, a loop making no progress), `/cost` breaks spend down with suggestions, and `[spend]` adds session, day and month budgets.
- **Your shell setup in Bash.** Commands see your login shell's PATH, functions and aliases (nvm, Volta, pyenv, asdf, Homebrew), without secrets; `shell_snapshot = false` turns it off.
- **Diffs for Bash changes.** Files a command changed (sed, scripts, codegen) show as a diff, like an edit.
- **Sessions.** `chanto sessions` searches, archives and deletes past sessions; `/resume` filters as you type (ctrl+d delete, ctrl+a archive).
- **Branch from an earlier prompt.** In `/rewind`, edit an old prompt and resend it as a new branch; the original stays.
- **Context.** `/context skills` (`/skill-doctor`) shows what each skill, agent and MCP server costs; switching to a model with a smaller window compacts first; optional idle compaction.
- **`--safe-mode` and `--restricted`**, as in Claude Code; tab status (working, waiting, done) in Ghostty.

## v0.1.5 (October 16, 2026)

- **More coding plans.** Kimi Code (`chanto login kimi-coding`) and MiniMax M Plan (`chanto login minimax-coding`) join ChatGPT and Z.ai; the Z.ai and Gemini endpoints now reach the right URL. Several accounts per provider (`chanto login <provider> --as work`).
- **Agents panel** as in Claude Code: each running agent's activity, time and tokens under the prompt; open one to watch its transcript or type to message it.
- **MCP and skills.** `chanto mcp add-json`, import from Claude Desktop, `/mcp reconnect|enable|disable`, allow/deny policy; bundled `/code-review`, `/simplify`, `/batch`, `/debug`, `/fewer-permission-prompts`; `/statusline`.
- **Session tools.** The Monitor tool, scheduled tasks (`/loop` self-paced, CronCreate/List/Delete), `/background`, `/cd`, `/plan`, `/recap`, `/autocompact`, `/redo`, and `e` on an edit prompt to change the proposed file in your editor first.
- **Quality checks.** Format and lint after edits (`[format]`, `[lint]`), an `oracle` reviewer agent, and waiting out a usage limit instead of failing (`resume_after_limit`).
- **Other tools' instructions.** GEMINI.md, Cursor rules, Copilot instructions, Aider and opencode files load in trusted folders; XDG config locations.
- **Sturdier.** Fixes from a chaos-testing pass, among them: a full disk could leave an edited file empty; a cut-off tool call could write a truncated file; stalled model streams and MCP calls now time out; inputs bigger than the context window are refused with a hint; `/cd` no longer carries the old project's permissions; Bash `cd` outside the project resets; SIGTERM saves the session.
- **Looks.** Code stays readable on dark terminals when the background can't be detected; menus get their own panel and a highlighted row; narrow terminals no longer cut words or collide.
- **Faster big repos.** The first rewind snapshot of a 30,000-file project takes seconds instead of half a minute.
- **Windows and Linux.** Bash tracks `cd` on Windows; the Linux test suite runs in CI.

## v0.1.4 (October 16, 2026)

- **Your model choice sticks.** `/model` and the model picker save the choice as your default, as in Claude Code; `/model --session` (Ctrl+S in the picker) switches this session only.
- **Every Claude Code hook event.** All 33 events are accepted and fire where Chanto has the moment (PostToolBatch, PermissionDenied, StopFailure, MessageDisplay, Task*, ConfigChange, CwdChanged, FileChanged, WorktreeCreate/Remove, model switches, elicitation and more), plus `CLAUDE_ENV_FILE` and the `Setup` event (`--init-only`, `-p --init`, `-p --maintenance`).
- **Feedback and diagnostics.** `/feedback` (or `/bug`) shows the exact redacted report before sending; opt-in automatic error reports; `chanto diagnostics on --name you` sends errors and warnings automatically for a while, for people helping test.
- **Keybindings** in Claude Code's `keybindings.json` format, applied live (`/keybindings`).
- **Background work panel** under the prompt for running agents and shells (↓ to open or stop them).
- **Concise and Proactive output styles; `--from-pr`** resumes the session that opened a pull request, whose status shows in the status line.
- **Smaller things.** Emoji shortcodes, opt-in prompt suggestions, `/login chanto`, the auto-mode classifier retries after an unexplained 400, big projects get their first rewind snapshot, and Windows error reports redact the home directory in log lines.

## v0.1.3 (October 16, 2026)

- **Chanto cloud from the agent.** Signed in, the agent gets a `ChantoCloud` tool: it lists and shows cloud runs and agents, and (asking first) starts or cancels runs and starts agents, so "send this to the cloud" just works.
- **Notifications in Terminal.app.** It can't raise one itself, so Chanto asks macOS to post it (`notifications = "system"`, the new default there); Linux uses `notify-send`.
- **Status line.** Shows the reasoning effort (or "thinking off") next to the model, and cost only when there is some (`$?` for unknown prices); ChatGPT plan calls no longer count as unpriced.
- **Windows.** Cloud run scripts use Git Bash, never the WSL launcher.

## v0.1.2 (October 16, 2026)

- **DeepSeek thinking mode works with tools.** Its `reasoning_content` goes back with each assistant turn, as the API requires; sessions saved before this retry once without thinking instead of failing with a 400.
- **`/btw` like Claude Code.** Side questions run mid-turn without interrupting, form a thread, and open in an overlay (←/→ to step, `x` to clear); `f` forks one into a background agent that inherits the conversation, with full tools.
- **Edit queued messages.** ↑ on an empty prompt pulls messages the model hasn't received yet back into the prompt.
- **Readable on dark terminals.** Light or dark now comes from the terminal's own background, not the macOS appearance; diff highlights fill the line.
- **Prompt box** is a line above and below, without side bars or blank rows.
- **Fixes.** The status line shows the new model right after `/model`; long dialog options wrap instead of being cut off; Bedrock, Vertex, Azure and Foundry models are listed only once you opt in, not because AWS or gcloud credentials are on the machine.
- **Windows:** sandboxed commands can write to the package managers' caches.

## v0.1.1 (October 15, 2026)

- **Windows sandbox works.** Shell commands on Windows run under a write-restricted token: writes are limited to the project, temp and the package managers' caches. Reads and network aren't restricted there; Chanto says so.
- **Verified mode is sturdier.** If Chanto is killed while a check runs on the original files, a watcher puts your changes back at once. Tests that hang end verification as ⚠ Not verified instead of costing another time limit. `-p` exits 3 when the answer isn't verified.
- **Clearer when things go wrong.** An unreachable model server gives one line instead of three retries; mistyped commands and config keys get "did you mean"; `-p` with an unknown session id fails instead of starting fresh.
- **Polish.** `/vim` and the `/config`, `/permissions`, `/hooks` and `/agents` editors work when idle; `/config <key> <value>` sets a setting; Markdown keeps single line breaks; fixed choices no longer offer "Type something else"; pickers open on the current choice; `/release-notes` shows these notes.
- **Prices.** Model prices come from a published list (checked against each vendor's pricing page) and update without a release; GPT-6 long-context and Haiku 5.5 long-prompt rates are applied.
- **Organization policy.** Claude Code managed `defaultMode` and `disableBypassPermissionsMode` are honoured.
- **Context windows.** OpenAI-compatible servers (vLLM, llama.cpp, LM Studio, LiteLLM) report their context size, so sessions don't compact at 32k.

## v0.1.0 (October 15, 2026)

First public release. Highlights:

- **Checks its own work.** `--verify` runs the tests related to a change (pytest, unittest, cargo, go, jest, vitest; the full suite when shared setup changed, or with `--verify-full`) before the agent can finish, separates new failures from ones that were already there, and blocks weakening or skipping tests. Outcome mode, goals and teams build on the same checks.
- **Any model.** Ollama, llama.cpp, vLLM and LM Studio; OpenAI, Anthropic, OpenRouter, DeepSeek and 20 other hosted providers; a ChatGPT plan login; Amazon Bedrock, Google Vertex AI and Azure with your cloud credentials. `--effort low|medium|high|xhigh|max` (or `/effort`) sets reasoning effort for each provider's API.
- **Safe by default.** Shell commands run in an OS sandbox on macOS, Linux and Windows; permission modes from ask-everything to auto mode, where a classifier screens actions; secrets are masked before anything reaches the model.
- **Undo anything.** `/rewind` restores files, including shell side effects, and the conversation to any earlier prompt.
- **Two terminal views.** A full-screen view, or `--inline` to print into the terminal's own scrollback. Markdown renders while it streams, without reflowing when the answer finishes; scrolling up holds the view still while output arrives.
- **Works with your setup.** Reads Claude Code and Codex settings, MCP servers (stdio, streamable HTTP and legacy SSE, with OAuth, and elicitation so servers can ask you questions), hooks, skills, commands, agents and plugin marketplaces; AGENTS.md and CLAUDE.md instructions.
- **Sessions.** Resume from any directory, fork, browse session trees, share as redacted HTML.
- **Automation.** Print mode with JSON and stream-json output (and `--input-format stream-json` to drive a multi-turn session over stdin), an HTTP API, an MCP server, background and remote runs, and scheduled jobs.

Install with `curl -fsSL https://chanto.sh/install.sh | sh` (or the PowerShell script on Windows); every download is on [chanto.sh/download](https://chanto.sh/download).
