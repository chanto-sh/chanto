# Chanto IDE bridge protocol (v1)

An editor plugin lets Chanto read the user's selection and diagnostics and show proposed edits as diffs. Two implementations exist: the VS Code extension in `ide/vscode` (`src/server.ts` is editor-independent) and the JetBrains plugin in `ide/jetbrains`. Other editors only need to implement the server side described here. (Editors that speak the Agent Client Protocol can instead run Chanto as their agent with `chanto acp`.)

## Discovery

The plugin listens on `127.0.0.1` on a random port and writes a lockfile at `$CHANTO_HOME/ide/<port>.lock` (`CHANTO_HOME` defaults to `~/.chanto`). Create the directory with mode `0700` and the file with mode `0600`:

```json
{
  "pid": 12345,
  "workspaceFolders": ["/path/to/project"],
  "ideName": "Visual Studio Code",
  "transport": "ws",
  "authToken": "<random secret>",
  "protocolVersion": 1
}
```

- Rewrite the file when the workspace folders change, and delete it on shutdown.
- Chanto ignores lockfiles whose `pid` is no longer running, and deletes them.
- Chanto connects to the IDE whose workspace folder contains its working directory; the deepest match wins. A terminal the plugin opens should set `CHANTO_IDE_PORT=<port>` so Chanto picks that IDE first.

## Transport

A WebSocket connection to `ws://127.0.0.1:<port>`.

- The handshake must carry the header `x-chanto-ide-authorization: <authToken>`. Reject the upgrade with HTTP 401 otherwise.
- Messages are JSON-RPC 2.0 text frames.
- Chanto sends requests; the IDE answers them and may push notifications.
- Requests are answered concurrently, not in lockstep: while an `openDiff` waits for the user, Chanto still sends `closeDiffs` on the same connection to settle it. Replies may arrive out of order; match them by `id`.
- Errors use the standard JSON-RPC shape: `-32601` for an unknown method, `-32000` for a handler failure.

## Methods (Chanto → IDE)

Lines are 1-based. Paths are absolute.

| method | params | result |
|---|---|---|
| `getSelection` | `{}` | `{file, text, startLine, endLine, isEmpty}` or `null` when no file editor is active |
| `getOpenEditors` | `{}` | `[{file, isActive, isDirty}]` |
| `getWorkspaceFolders` | `{}` | `[path]` |
| `getDiagnostics` | `{path?}` | `[{file, diagnostics: [{line, character, severity: "error"\|"warning"\|"info"\|"hint", message, source?, code?}]}]`; all files when `path` is omitted |
| `openFile` | `{path, line?, endLine?}` | `{ok: true}` |
| `openDiff` | `{path, newContents, title?}` | `{accepted: bool, contents?: string}` |
| `closeDiffs` | `{path?}` | `{ok: true}` |

`openDiff` shows the current file (empty if it doesn't exist yet) against `newContents` and **stays pending until the user decides**:

- If the user accepts, return `accepted: true` with `contents` set to the proposed side as it is now. The user may have edited it; Chanto writes these contents.
- Rejecting, or closing the diff, returns `accepted: false`.
- `closeDiffs` means the user already answered in the terminal: close the open diff for `path` (every open diff if `path` is omitted) and resolve its pending `openDiff` with `accepted: false`.

## Notifications (IDE → Chanto)

| method | params |
|---|---|
| `selectionChanged` | same shape as the `getSelection` result |

Chanto caches the last pushed selection and uses it when `getSelection` doesn't answer within 400 ms.
