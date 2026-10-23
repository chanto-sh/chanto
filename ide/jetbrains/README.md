# Chanto for IntelliJ

> [!WARNING]
> **Not ready yet.** The JetBrains plugin is a work in progress: it isn't documented or
> supported, and it may change or break without notice. For now, use Chanto
> from the terminal.

Connects the [Chanto](../../) coding agent to IntelliJ-based IDEs (IDEA, PyCharm,
WebStorm, GoLand, …). The plugin serves the Chanto IDE bridge — the same
[jSON-RPC-over-WebSocket protocol](../PROTOCOL.md) the VS Code extension uses —
on a loopback port.

What you get:

- **Selection as context:** whatever you have selected (or the file you have open)
  is added to your next prompt.
- **Edits as diffs:** when Chanto asks permission for an edit, the change also
  opens as a diff. Accept it, or edit the proposed side first and Chanto writes
  your version. Reject, or close the diff, and the turn stops. Answering in the
  terminal works too and closes the diff.
- **Diagnostics:** after an edit, Chanto reads the IDE's errors and warnings for
  the file, so it sees problems from inspections Chanto has no server for.

The bridge only listens on `127.0.0.1` and requires the per-session auth token
from the lockfile.

## Building

```sh
cd ide/jetbrains
./gradlew buildPlugin          # build/distributions/chanto-jetbrains-0.1.0.zip
./gradlew runIde               # launch a sandbox IDE with the plugin
```

Requires JDK 21; an IDE's bundled runtime works, e.g.
`JAVA_HOME="/Applications/IntelliJ IDEA.app/Contents/jbr/Contents/Home"`. The Gradle
wrapper resolves the IntelliJ Platform (`platformVersion` in `gradle.properties`,
default 2025.3.1), or builds against an installed IDE with
`-PlocalIde="/Applications/IntelliJ IDEA.app"`.

This plugin is for running Chanto in the IDE's terminal with the IDE showing its
diffs, selection and diagnostics (as the VS Code extension does). To use Chanto as
the agent in JetBrains AI Assistant's own chat instead, no plugin is needed: add it
as an ACP agent (`chanto acp`, see docs/reference.md, "Editors (ACP)").

Without Gradle you can type-check the sources against a locally installed IDE:

```sh
./selftest.sh                  # the editor-independent bridge core, runs anywhere
./typecheck.sh                 # the IntelliJ-facing sources, needs a local IDE
```

`selftest.sh` also covers the concurrency rule from the protocol: a pending
`openDiff` must not stop `closeDiffs` from being answered.

Install the built zip with **Settings → Plugins → ⚙ → Install Plugin from Disk…**.

## Using it

1. Open a project in the IDE. The bridge starts on its own for each open project.
2. **Tools → Chanto: Open in Terminal** (or the same action from the search
   everywhere popup) starts Chanto in a terminal tagged with the bridge's port.
3. `cmd+alt+K` (**Chanto: Insert @-mention of Current File**) types `@path ` into
   that terminal.

`/ide` inside Chanto shows the connection; `/ide connect [port]` picks another
window and `/ide disconnect` turns it off. `/ide autoConnect false` stops it
connecting on startup.

## How the plugin runs

| | |
|---|---|
| `Json.kt` | dependency-free JSON parser/serializer |
| `WebSocket.kt` | RFC 6455 framing and the HTTP upgrade |
| `IdeApi.kt` | the editor operations the bridge needs |
| `IdeServer.kt` | the loopback server, auth, JSON-RPC dispatch, lockfile |
| `IntelliJIde.kt` | `IdeApi` on the IntelliJ platform (EDT-marshalled) |
| `ChantoIdeService.kt` | starts/stops the bridge per project, pushes selection changes |
| `ChantoActions.kt` | the four actions |

`openDiff` is a modal diff with **Accept**/**Reject**; the request stays pending
until you decide, so Chanto applies exactly what you saw. The server answers
requests on worker threads for that reason.
