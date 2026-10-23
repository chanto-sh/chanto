# Changelog

## v0.1.1 (October 23, 2026)

- **Security: commands could be hidden from the permission check.** A quote that bash reads as plain text, in a comment (`# don't`), a heredoc body or after a backslash, made Chanto's safe-command check misjudge where a quote ended, so the commands after it skipped the check and ran without a prompt. The check now reads comments, heredoc bodies and escapes the way bash does. Please update: `chanto upgrade`, `brew upgrade chanto`, `npm i -g @chanto/cli` or `cargo install chanto --locked`.

## v0.1.0 (October 23, 2026)

Initial release.
