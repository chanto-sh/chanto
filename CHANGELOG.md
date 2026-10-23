# Changelog

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
