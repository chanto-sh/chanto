# Contributing to Chanto

Thanks for your interest in improving Chanto. This guide covers how to build,
test, and submit changes. Architecture notes and day-to-day commands live in
[AGENTS.md](AGENTS.md); this file is about the contribution workflow.

## Getting started

Chanto is a Rust workspace. You need **Rust 1.95 or newer**:

```sh
rustup update stable
git clone https://github.com/chanto-sh/chanto.git
cd chanto
cargo build                 # add --release for target/release/chanto
```

`cargo build` and `cargo test` at the root build only the `chanto` CLI. The
desktop app (`desktop/`), the WebAssembly build (`web/`) and the wire types
(`proto/`) are separate workspace members so their dependencies never reach the CLI.

## Before you open a pull request

CI runs the same three checks on every push, so run them locally first:

```sh
cargo fmt --all --check                               # formatting
cargo clippy --locked --workspace --all-targets -- -D warnings   # lints (must be clean)
cargo test --locked                                   # tests
```

Enable the repository's git hooks once per clone:

```sh
git config core.hooksPath .githooks
```

`pre-commit` checks formatting and catches conflict markers, `dbg!`/`todo!`,
credentials and oversized files; `commit-msg` checks the message format below.

CI exercises the full suite on macOS, Linux, and Windows, plus the wire types
and the editor extension. A green local run of the three commands above covers the
common cases.

Changing credentials, permissions, the sandbox or `unsafe` code? Every pull
request is scanned with CodeQL, and one that adds a new finding can't
merge. To see the findings before you push, run `scripts/codeql.sh` (needs the
CodeQL CLI, `brew install --cask codeql`; about ten minutes for the Rust code).

Changing a workflow or a dependency? The `scorecard guard` check runs the
OpenSSF Scorecard checks that only need the repository's files: known
vulnerabilities in the lockfiles (osv-scanner), workflow security (zizmor) and
Scorecard's own pinned-dependency, token-permission, dangerous-workflow and
binary-artifact checks. Run them locally with `scripts/scorecard-check.sh`
(`brew install osv-scanner zizmor scorecard`; a few seconds). The pre-commit
hook runs zizmor when a workflow is staged and osv-scanner when a lockfile is,
if they're installed. An advisory that can't affect Chanto goes, with the
reason, in the `osv-scanner.toml` next to the lockfile: that's the only list
Scorecard reads (`.cargo/audit.toml` and `deny.toml` don't count).

### Wire type changes

The wire types in `proto/` are shared with the hosted service and have their
own tests. Changes must be additive, since older CLIs keep parsing responses:

```sh
cargo test -p chanto-proto
```

See [AGENTS.md](AGENTS.md) for the store/migration conventions (every query goes
in both `MemoryStore` and `PgStore`; schema changes are new migration files,
never edits to applied ones; `proto/` is additive-only).

Public URLs come from `src/site.rs`; route new code through it.

## Pull requests

1. Fork the repo and create a branch from `main`.
2. Keep each PR focused on one change; smaller PRs review faster.
3. Add or update tests for behavior you change. The agent-loop tests use a mock
   model server (`src/agent/tests.rs`), so most changes can be tested without a
   live model.
4. When your change is user-visible, add a changelog fragment,
   `changelog.d/<branch-name>.md`, instead of editing CHANGELOG.md (see
   [changelog.d/README.md](changelog.d/README.md)). Separate files never
   conflict; the release pull request collects them.
5. Give the pull request a [Conventional Commits](#commit-messages) title. PRs
   are squash-merged, so the title becomes the commit on `main`; CI checks it.
6. Make sure the three CI checks above pass.

## Commit messages

We use [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <summary>

<body: what changed and why, wrapped at 72 columns>

<footers: Fixes #123, BREAKING CHANGE: ...>
```

| Type | For |
|---|---|
| `feat` | a new user-visible capability |
| `fix` | a bug fix |
| `perf` | faster or leaner, same behavior |
| `refactor` | restructuring with no behavior change |
| `docs` | documentation only |
| `test` | tests only |
| `build` | Cargo, dependencies, packaging, installers |
| `ci` | GitHub Actions and other automation |
| `chore` | anything else that ships nothing (tooling, housekeeping) |
| `revert` | undoing an earlier commit |
| `style` | formatting only |

- **Scope** is optional and lowercase: the area touched, usually a module or
  directory (`tui`, `hooks`, `sandbox`, `mcp`, `llm`, `classifier`, `desktop`,
  `proto`, `deps`).
- **Summary:** imperative, lowercase, no trailing period, and the whole subject
  within 72 characters: `fix(sandbox): allow writes to the cargo registry cache`.
- **Body** (optional): explain *why*. The diff already says *what*.
- **Breaking changes:** add `!` after the type or scope (`feat(config)!: ...`)
  and a `BREAKING CHANGE:` footer saying what to do about it.
- Don't add AI-assistant attribution lines (`Co-Authored-By:` an AI tool,
  "Generated with ..."); the hook rejects them.

`.githooks/commit-msg` enforces this locally, and the `pr-title` workflow runs
the same script on pull request titles.

## Signing off commits

Every commit needs a sign-off certifying the
[Developer Certificate of Origin](https://developercertificate.org): that you
wrote the change, or otherwise have the right to submit it under the
project's license. Commit with `git commit -s`, which adds a line with your
name and the email address the commit is made with:

```
Signed-off-by: Your Name <you@example.com>
```

To sign off commits you've already made: `git rebase --signoff main`. The
commit-msg hook asks for the line, and the `DCO sign-off` check rejects a pull
request with a commit that lacks it (bots such as Dependabot are exempt).

## Reporting bugs and requesting features

Open an issue using the templates under
[.github/ISSUE_TEMPLATE](.github/ISSUE_TEMPLATE). For **security
vulnerabilities, do not open a public issue**; see [SECURITY.md](SECURITY.md).

## Code of conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md). By
participating you agree to uphold it.

## Licensing of contributions

Chanto is licensed under [Apache-2.0](LICENSE). Unless you explicitly state
otherwise, any contribution you intentionally submit for inclusion in the work,
as defined in the Apache-2.0 license, shall be licensed as above, without any
additional terms or conditions. No separate CLA is required; the sign-off on
each commit (see [Signing off commits](#signing-off-commits)) is your
certification that you may contribute it.
