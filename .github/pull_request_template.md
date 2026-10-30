<!--
Thanks for contributing! Please read CONTRIBUTING.md if you haven't.
Keep PRs focused on one change.
-->

## What this changes

<!-- A short description of the change and why. Required. -->

## Testing

<!-- How you checked it: tests added or run, manual steps, what you saw.
     "Not tested" with a reason is fine; an empty section isn't. Required. -->

## Checklist

- [ ] `cargo fmt --all --check` passes
- [ ] `cargo clippy --locked --workspace --all-targets -- -D warnings` is clean
- [ ] `cargo test --locked` passes
- [ ] Added or updated tests for the behavior I changed
- [ ] Added a `changelog.d/<name>.md` fragment if this is user-visible (CI requires one for `feat`, `fix` and `perf`, or a `Changelog: none (<why>)` line here)
- [ ] Updated docs / `AGENTS.md` if I changed a convention or interface

## Notes for reviewers

<!-- Anything that helps review: tradeoffs, follow-ups, areas to look at. -->
