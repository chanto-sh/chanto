# Changelog

## Unreleased

- **Container image.** Each release now publishes `ghcr.io/chanto-sh/chanto:<version>` and `:latest` for linux/amd64 and linux/arm64, built from the release's own signed Linux binaries, with build provenance and a cosign signature. See [Container image](docs/reference.md#container-image).
- **Security: secrets could reach the model through side calls and leave through run logs.** Compaction summaries, prompt and agent hooks, the permission classifier and other side requests sent the conversation unmasked; every model request is now masked in one place. Background runs' event logs, stream-json output, the web UI and `/remote` (including permission prompts, transcripts and the queue) and what a runner sends its server (including script output) are masked too, and on Unix run records and logs under `~/.chanto/runs` are now readable only by you. Background runs also mask their source checkout's `.env` values, sign-in and MCP OAuth tokens are masked once used, and plain `*_KEY` variables are withheld from commands (list one in `[secrets] pass_env` if a tool needs it) and masked when their value looks random. On Linux, Chanto's process is no longer readable through `/proc` by the commands it starts, so `gdb -p` and `strace -p` need root.
- **Security: a prompt the model scheduled could run a shell command without asking.** A `CronCreate` prompt starting with `!` ran outside the sandbox when it fired, and one starting with `/` ran a slash command. Scheduled prompts, background reports and undelivered messages now always reach the model as plain text (no `!`, `/`, `#` or `@file`), `CronCreate` asks for approval outside bypass mode, and a command's arguments reach its `` !`cmd` `` blocks only as data.
- **Security: an imported session could turn on bypass mode.** `chanto import-session` now drops the file's settings, pull-request links, parent and dates, a resumed session never restores bypass mode, and a managed `defaultMode` wins on resume.
- **Security:** an approved call is refused if an earlier call in the same batch re-pointed its path through a symlink or made its command reach a secret file; the model's saves to user memory (loaded in every project) ask first and show what they store; installing a plugin asks when its skills or commands run `` !`cmd` `` blocks, register hooks or pre-approve tools, and git-cloned plugins with symlinks are refused.

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
