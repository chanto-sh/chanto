# Terminal visual regressions

The `visual` GitHub Actions workflow compares **72 full-frame baselines**:
12 scenes × 40/80/120 columns × dark/light themes. It also uploads
`chanto-ui-previews`, containing a PNG for each scene, six short GIF tours,
ANSI captures, text/cell styles, and a browsable `index.html` gallery.
Download the artifact and open the gallery locally. A small fixture caption
below each preview identifies its theme, size and state.

Scenes cover welcome/setup, a successful conversation with Markdown and code,
a streaming code block, waiting/successful/failed background jobs, both quota
warning levels, permission/question dialogs, slash completion and scrolling.
They render the real App with Ratatui's TestBackend: no model, login, API key
or external service is called.

## Check and review

```sh
cargo test --locked --lib visual_snapshots -- --ignored --test-threads=1
```

This suite runs separately because themes are process-global. The ordinary
unit suite ignores it; the dedicated workflow always runs it explicitly.
Text **and every cell's style** are compared, so color/modifier regressions
are caught as well as layout changes. Time and spinner phase are frozen,
fixture paths and responses are fixed, and reset times use UTC in visual
fixtures only. Normal runtime rendering retains local times and real clocks.
The version on the welcome screen is intentionally captured: review that
snapshot when bumping the package version.

For intended changes, install `cargo-insta`, run the suite, and review the
pending snapshots with `cargo insta review`. Commit reviewed `.snap` files;
never enable automatic snapshot acceptance in CI. CI writes `.snap.new`
files on differences and uploads them with the newly rendered previews.
All scenes are captured even when an earlier snapshot fails.

## Generate previews locally

```sh
CHANTO_VISUAL_OUT="$PWD/target/visual" \
  cargo test --locked --lib visual_snapshots -- --ignored --test-threads=1
python3 tests/visual/previews.py
```

The second command needs VHS **v0.12.1**, its ttyd/FFmpeg/browser dependencies,
and the **JetBrains Mono** font (the pinned official VHS action supplies
these in CI). It waits for a unique fixture caption before capturing each
PNG. Generated media stays under `target/visual`; do not commit it.

Snapshots are the regression gate. VHS presents those captured cells for
human review; it does **not** exercise the live TUI event loop, terminal
setup/teardown or a real model. Keep ordinary event/behavior tests and
platform-specific terminal smoke tests alongside this suite.
