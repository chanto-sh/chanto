# Changelog

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
