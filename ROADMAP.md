# Roadmap

Where Chanto is heading over the next year (to late 2027). These are
intentions, not promises: priorities change with what users need, and
[discussions](https://github.com/chanto-sh/chanto/discussions) are the place to
suggest or argue for something. Larger items get an issue when work starts.

## Now (late 2026)

- **Security hardening.** Finish the fixes from the October 2026 security
  review: stricter rules for which shell commands count as read-only, tighter
  sandbox write rules, and least-privilege CI tokens.
- **Release integrity.** Make builds reproducible (releases are signed, and
  `chanto upgrade` and the install scripts verify the signature).
- **Testing.** Measure test coverage and raise it to at least 80%; fuzz the
  shell-command parser that the permission checks rely on.
- **Windows.** Code-signed Windows binaries.

## Next (first half of 2027)

- **More ways to use Chanto.** Bring the surfaces that exist but aren't ready
  yet to a supported state, one at a time: the web UI and `/remote`, the editor
  extensions (VS Code, JetBrains), the SDKs and the GitHub Action.
- **Distribution.** Homebrew core (no tap needed), winget and Scoop.
- **Speed and footprint.** Keep the per-turn overhead and memory lowest among
  coding agents as features land, tracked by the
  [benchmarks](https://github.com/chanto-sh/benchmarks).

## Later (second half of 2027)

- **1.0.** Stable configuration, command-line interface and wire protocol, with
  a compatibility promise for 1.x.
- **Governance.** A second maintainer with release and organization access, so
  the project doesn't depend on one person (see [GOVERNANCE.md](GOVERNANCE.md)).
- **The desktop app,** once the web UI it builds on is supported.
