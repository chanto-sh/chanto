# Changelog

## Unreleased

- **Usage limits.** The footer shows the most-used reported quota window, its local reset time and `/usage`, dropping extra details on narrow terminals. A heads-up appears once at 25% and 10% remaining and re-arms after a window resets. Missing quota data is never estimated.

- **Background command status.** Output polls show the originating command’s description alongside its id, explain when they are waiting for new output, distinguish quiet/running/completed/failed jobs, and preview the latest three short lines. Failed background exits are marked as errors.

- **Todo panel.** A long todo is cut at a word with an ellipsis instead of being clipped mid-word by the frame, the panel says `… N more` when more than eight are hidden, its height reserves every row it draws, a blank row separates the list from a running turn's status line, and the header lines up with the transcript.
- **Interrupted sessions.** The notice shown when a session resumes after an interrupted turn lists the calls it was running one per line, instead of joining a batch of long commands into one run-on sentence.
- **Provider safety messages.** Refusal explanations and links now appear in streamed replies and transcripts. A provider safety filter that supplies no explanation shows a clear notice, and filtered turns end without retries or tool execution.
- **Streaming and large requests.** Stream framing borrows complete lines and compacts once per network read, avoiding per-line allocations and repeated buffer shifts. Buffered model request retries share the serialized body instead of copying it. The standard 40-turn overhead benchmark shows no measurable CPU or peak-memory improvement from these changes.

- **Container image.** Each release now publishes `ghcr.io/chanto-sh/chanto:<version>` and `:latest` for linux/amd64 and linux/arm64, built from the release's own signed Linux binaries, with build provenance and a cosign signature. See [Container image](docs/reference.md#container-image).
- **Security: the auto-mode classifier only counts what you typed as your request.** Prompts that Chanto or another agent wrote were shown to it as yours. This covered subagent tasks, goal, team and outcome steps, background run tasks and reports, slash-command expansions, and files attached to a prompt. A model could write "the user approved this" and get its own action cleared. Those prompts are now labelled as the agent's task. Files, resources and `<system-reminder>` context are cut from what you typed.
- **Security: tighter auto-mode classifier settings.** An untrusted project can no longer set `classifier_model` or `optimize.token_classifier`. The one-token fast path allows an action only when `ALLOW` is the top token with at least 90% probability; a prefix like `All` doesn't count.
- **Security: safe-listed commands could run programs or write files without a prompt.** Examples: `fd -x`, `find -fprint`, `git fetch --upload-pack` or `git fetch <url>`, `git grep -O`, `sort -o`/`--compress-program`, `rg --pre`, `tar --checkpoint-action`/`-I`, `git rebase -x`, `go build -o`, `go env -w`, `cargo --config`. The same was possible with `$'…'` quoting or brace expansion. These options now need approval in default mode and are vetted in auto mode too.
- **Security: shell writes to protected files ask in every mode but bypass.** Writing `.git/`, `.chanto/`, `.claude/` settings, `.mcp.json`, `.husky/` or `.vscode/` from the shell now asks, as an Edit of them does. That includes writes through redirections, `tee`, `cp`, `sed -i`, `curl -o`, `git config`, `sh -c`, a symlink made in the same command, or a target the shell computes (`$var`, `$(…)`). In auto mode, extracting an archive or applying a patch (`tar x`, `unzip`, `patch`, `git apply`) asks, because the input names the files written.
- **Security:** the auto-mode fast path sends inline code (`python -c`, `node -e`), remote packages (`npx owner/repo`, `go run host/x@v`) and option files (`curl -K`) to the classifier. MCP tools marked read-only now ask or go to the classifier like other MCP tools, unless an allow rule covers them.
- **Fix:** the permission check read `$$'…'` (the shell's PID followed by a quote) as `$'…'` quoting, so it could misjudge where a quote ended. Found by fuzzing.
- **Security: a prompt the model scheduled could run a shell command without asking.** A `CronCreate` prompt starting with `!` ran outside the sandbox when it fired, and one starting with `/` ran a slash command. Scheduled prompts, background reports and undelivered messages now always reach the model as plain text (no `!`, `/`, `#` or `@file`), `CronCreate` asks for approval outside bypass mode, and a command's arguments reach its `` !`cmd` `` blocks only as data.
- **Security: an imported session could turn on bypass mode.** `chanto import-session` now drops the file's settings, pull-request links, parent and dates, a resumed session never restores bypass mode, and a managed `defaultMode` wins on resume.
- **Security:** an approved call is refused if an earlier call in the same batch re-pointed its path through a symlink or made its command reach a secret file; the model's saves to user memory (loaded in every project) ask first and show what they store; installing a plugin asks when its skills or commands run `` !`cmd` `` blocks, register hooks or pre-approve tools, and git-cloned plugins with symlinks are refused.
- **Security: checks ran outside the sandbox with your full environment.** Verified-mode, goal, outcome and team checks and formatters run commands the project defines (`npm test`, `pytest`), which the agent or a cloned repository can change. They now run in the sandbox like the agent's Bash commands, without secret-named environment variables; a check that needs a token gets it through `[secrets] pass_env`. Language servers start without secret-named variables, and detecting pytest no longer imports a `pytest.py` from the project.
- **Security: git that Chanto runs itself could run programs a repository's config names.** The startup status and log, the TUI's branch, `/diff`, and git for worktrees, background runs and teams now turn off the fsmonitor hook, hooks, signing, diff, filter and merge drivers the repository defines (Git LFS keeps working) and submodules, and never follow a worktree's `.git` file. File tools ask before writing a `.git` file, as they did for the `.git` directory.
- **Security: runners ran a server's scripts unsandboxed.** `chanto runner connect` now runs the scripts jobs come with only with `--allow-scripts`, sandboxed and without the machine's secrets, and checks the run ids it's given.
- **Security: `chanto runs apply --on` trusted the server's branch and base.** It now only creates the run's own `chanto/run/<id>` branch, never moves an existing one, and requires the base to be a commit your checkout has.

## v0.1.5 (October 25, 2026)

- **Security: the install scripts check the release signature before trusting anything.** `install.sh` read the version for "latest" from an unsigned `SHA256SUMS`, skipped the signature for v0.1.0/v0.1.1, and skipped it entirely without OpenSSL 3 (as on stock macOS); `install.ps1` never checked it. Both now verify the signature first, with OpenSSL 3 or, from this release on, with `ssh-keygen` against a new `SHA256SUMS.sshsig`, and stop if neither tool is available. They refuse releases before v0.1.2, which aren't signed, and refuse a "latest" older than v0.1.5. See [Verifying a release](docs/reference.md#verifying-a-release).
- **Terminal:** fix “The cursor position could not be read within a normal duration” on startup, focus restoration, and return from an external editor. Fullscreen redraws no longer require a cursor-position response from the terminal.

## v0.1.4 (October 23, 2026)

- **Security: commands could slip past the permission check.** A heredoc closed by a marker bash doesn't accept (such as one indented with spaces), or characters that ended a word for Chanto's check but not for bash, let a command after them run without a prompt. Chanto now ends heredocs and words exactly where bash does. Please update: `chanto upgrade`, `brew upgrade chanto`, `npm i -g @chanto/cli` or `cargo install chanto --locked`.
- **Security: the cloud metadata address could skip its approval.** `169.254.169.254` written with a tab, `%`-escapes, a trailing dot or control characters wasn't recognised, so a request to it went through without asking. Chanto now checks the address as the URL parser reads it.
- **Security:** hosts like `localhost.evil.com` no longer count as local for plain HTTP, and GitLab, Slack, Google and SendGrid tokens ending in `-` are now redacted whole. These and the fixes above were found by fuzzing, which now runs on every pull request.
- **Faster and lighter.** About a third less CPU per turn and about 15% less peak memory in our overhead benchmark: Chanto no longer starts helper processes it doesn't need (`defaults`, `caffeinate`, git outside a repository), stops rewriting and re-reading files on every model call, sends requests without building a copy of the conversation first, and skips work whose result hasn't changed.
- **Releases** now carry each build's SLSA provenance as a `.intoto.jsonl` file next to the archive.

## v0.1.3 (October 23, 2026)

- **Windows: unchanged `--worktree` sessions are cleaned up again.** Chanto exited while still inside the worktree, and a file held open for a moment could make the removal fail, so an untouched worktree was kept and reported as having changes. Chanto now leaves the worktree first and retries; if removal still fails, it says so rather than claiming there are changes.
- **`chanto migrate`:** when nothing in the other agent's configuration maps to Chanto's, the empty result is no longer misread as a setting.
- **Release process:** crates.io packages are now published without a stored token (trusted publishing), and each release workflow job gets only the permissions it needs.

## v0.1.2 (October 23, 2026)

- **Signed releases.** From this release on, each release's `SHA256SUMS` is signed with the project's release key. `chanto upgrade` refuses a release whose signature is missing or wrong, and `install.sh` checks it too whenever OpenSSL 3 is available. To verify a download yourself, see [Verifying a release](docs/reference.md#verifying-a-release).

## v0.1.1 (October 23, 2026)

- **Security: commands could be hidden from the permission check.** A quote that bash reads as plain text, in a comment (`# don't`), a heredoc body or after a backslash, made Chanto's safe-command check misjudge where a quote ended, so the commands after it skipped the check and ran without a prompt. The check now reads comments, heredoc bodies and escapes the way bash does. Please update: `chanto upgrade`, `brew upgrade chanto`, `npm i -g @chanto/cli` or `cargo install chanto --locked`.

## v0.1.0 (October 23, 2026)

Initial release.
