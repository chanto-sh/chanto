# Chanto: agent notes

## Commands
- Build: `cargo build` (release: `cargo build --release`, binary at `target/release/chanto`)
- Test: `cargo test` (single test: `cargo test <name>`); agent-loop tests use a mock OpenAI server in `src/agent/tests.rs`
- Format: `cargo fmt --all` (CI runs `cargo fmt --all --check`); enable the hooks (pre-commit checks, commit-msg format) once with `git config core.hooksPath .githooks`, and don't bypass them with `--no-verify`
- Lint: `cargo clippy --workspace --all-targets` must stay warning-free (CI runs it with `-D warnings`)
- Debug a tool without a model: `chanto --run-tool <Tool> '<json args>'`
- Inspect prompt size: `chanto --dump-prompt`; classifier check: `chanto --classify <Tool> '<json>' "<request>"`
- Fuzz: `fuzz/` is its own crate (nightly + `cargo install cargo-fuzz`); from there, `cargo +nightly fuzz run <target> corpus/<target> seeds/<target>` with target `shell_permission`, `redact`, `url_checks` or `path_checks`. CI fuzzes pull requests with ClusterFuzzLite (`.clusterfuzzlite/`). Add a minimized crash to `fuzz/seeds/<target>/` and a unit test next to the fix
- Benchmarks: a separate repository, [chanto-sh/benchmarks](https://github.com/chanto-sh/benchmarks); clone it next to this one and run `python3 run.py --agents chanto,opencode,claude` there (it uses `../chanto/target/release/chanto`)

## Releases
- Use an isolated worktree from current `origin/main`; preserve other agents' uncommitted work. Prepare a release PR and wait for every required check, including CodeQL, fuzz and visual, before merging. Do not bypass checks or force-push a published tag.
- Align `Cargo.toml`, `desktop/Cargo.toml`, their `Cargo.lock` entries, `ide/vscode/package.json` and `ide/jetbrains/build.gradle.kts`. Keep `proto` at its existing version unless its wire types changed.
- Keep an empty **Unreleased** heading and move its entries into `## vX.Y.Z (Month D, YYYY)`. Update the six welcome snapshots for the displayed version. For security releases raise `min_version` in `install.sh`, `$minVersion` in `install.ps1`, and the documented floor in `docs/reference.md`.
- Before pushing, run `python3 packaging/check-release.py vX.Y.Z`, `bash tests/install.sh`, and `cargo test --locked --lib visual_snapshots -- --ignored --test-threads=1`; review any snapshot differences. Metadata validation also runs once before all release builds.
- After the PR merges, fetch main and tag its exact merged commit with an annotated `vX.Y.Z` tag; push only that tag. The tag starts `.github/workflows/release.yml`. Releases serialize without canceling an active run because they update shared latest pointers. Cut one version at a time; GitHub keeps only one pending run in this concurrency group.
- Keep release actions pinned by SHA, Rust and `cross` pinned to tested exact versions, dependencies locked and release builds uncached. Upgrade tool pins in reviewed PRs. Hosted OS images, cross container tags and signing timestamps still vary; do not claim byte-for-byte reproducibility.
- Wait for all five archives, signed checksums, provenance, Homebrew, npm, crates.io and container jobs. Verify a downloaded archive against the signed checksum file and run its `--version`. Report publication complete only after checking the package channels, including any npm staging/approval state.
- On failure, inspect the specific job and retry only what is safe. Reuse already published immutable artifacts; never overwrite a released version, move its tag, or rebuild assets behind existing checksums. A successful GitHub release job alone does not prove every package channel is available.

## Architecture
- `agent/` owns the loop: `turn.rs` (`run_turn` → `call_model` → `execute_calls` → repeat), `model.rs` (one streaming call), `calls.rs` (executing calls, spend guard), `authorize.rs` (permission decisions), `compact.rs`, `session.rs` (checkpoints, rewind, forks), `verify.rs` (guardrails, oracle), `subagent.rs`, `protocol.rs` (text tool protocol, argument normalization), `background.rs`. `agent.rs` keeps the `Env` and `Agent` state and its accessors. Tests live in `agent/tests.rs`; `#[cfg(test)]` modules are separate `*_tests.rs` files.
- Frontends implement `agent::Frontend` (emit events, permission prompts, questions): `tui/` (ratatui; `draw.rs`, `keys.rs`, `mouse.rs`, `events.rs`, `pickers.rs`, `render.rs`, `stream.rs`, `run.rs`), `plain.rs` (print/REPL).
- `remote.rs` (`/remote`): `Mirror` wraps the TUI's frontend to also serve the session to the web UI over the tailnet; prompts are raced (first answer wins) and phone input reaches the TUI as `AppMsg::Remote`.
- Tools implement `tools::Tool`; `kind_for(args)` drives permission decisions in `permissions::static_decision`. Auto mode falls back to `permissions::classify`.
- `llm.rs`: `Provider` speaks Ollama `/api/chat` (NDJSON) or OpenAI `/chat/completions` (SSE); `Env::resolve` maps `provider/model` specs to named providers.
- Large modules keep a thin root `.rs` and move cohesive subsystems into a same-named directory of submodules (as `agent.rs`/`agent/` does): `guardrails.rs` + `guardrails/select.rs` (related-test selection), `hooks.rs` + `hooks/verdict.rs` (prompt/agent model-verdict hooks).

## Wire types (`proto/`)
- Test: `cargo test -p chanto-proto`. Root `cargo build`/`cargo test` skip it.
- `proto/` is wire types only, shared by the CLI and the hosted service; additive changes only, since old CLIs must keep parsing responses.

## Conventions
- Keep the system prompt prefix stable within a session (KV-cache reuse on local servers); put per-turn context in user messages as `<system-reminder>`.
- Tool output for the model goes in `ToolOutput.content`; human-facing summaries in `.display` (may contain ANSI).
- Public URLs come from `site_url!()` / `api_url!()` in `src/site.rs`.
- Commit messages follow Conventional Commits, checked by `.githooks/commit-msg` and on PR titles in CI: `<type>(<scope>): <summary>`, with type one of feat, fix, perf, refactor, docs, test, build, ci, chore, revert, style. The scope is optional, lowercase and names the area touched (`tui`, `hooks`, `sandbox`, `classifier`, `deps`). The summary is imperative, lowercase, has no trailing period, and the whole subject fits in 72 characters. The body says why. Breaking changes get `!` plus a `BREAKING CHANGE:` footer. No AI attribution lines. Full rules are in CONTRIBUTING.md#commit-messages.
- Comments describe Chanto's own behaviour. Name another agent only where the code really interoperates with its files or protocol (`.claude/`, `CLAUDE_*` aliases, hook payloads).
- Keep modules to a readable size: when a `*.rs` file grows past ~2.5k lines, split a self-contained subsystem into a `module/submodule.rs` rather than letting it sprawl. A submodule starts with `use super::*;`; items the parent still calls are `pub(super)`, everything else stays private. Move a function's doc comment with it (an orphaned `///` left behind trips clippy's `empty_line_after_doc_comments`).
