# Chanto server API and SDKs

> [!WARNING]
> **Not ready yet.** The SDKs are a work in progress: they aren't documented or
> supported, and they may change or break without notice. For now, use Chanto
> from the terminal.

`chanto serve` starts an HTTP server (default `127.0.0.1:4141`) that other programs can use to drive Chanto sessions: web or desktop UIs, editor plugins, scripts and CI. Each session is a full Chanto agent in its own working directory, with the same tools, permission modes, hooks and MCP servers as the terminal UI.

```sh
chanto serve                        # prints the URL and a token; also written to ~/.chanto/server.json
CHANTO_SERVER_TOKEN=... chanto serve --port 0 --model qwen3-coder:30b
```

## Clients

- **Python** (`sdk/python/chanto_sdk.py`, standard library only)
  ```python
  from chanto_sdk import Chanto
  s = Chanto.connect().session(mode="acceptEdits", on_permission=lambda ev: "allow")
  print(s.run("add a --verbose flag to the CLI and update the tests"))
  ```
- **TypeScript** (`sdk/typescript/index.ts`, uses `fetch`; works on Node 18+, Bun and Deno)
  ```ts
  const s = await (await Chanto.connect()).session({ onPermission: () => "allow" });
  for await (const ev of s.send("explain src/agent.rs")) if (ev.type === "text") process.stdout.write(ev.text);
  ```

By default, permission prompts are **denied**. Pass a callback to approve actions, or create the session with `mode: "acceptEdits" | "auto" | "bypassPermissions"`.

## Protocol

In `chanto web` mode, `/`, `/app.js` and `/style.css` serve public embedded UI assets.
Every API route except `/health` needs `Authorization: Bearer <token>`.

| Method and path | Body | Result |
|---|---|---|
| `GET /health` | | `{ok, version}` |
| `POST /sessions` | `{model?, mode?, resume?, cwd?, system_prompt_append?, allowed_tools?, disallowed_tools?, max_turns?, verify?, tools?}` | `{id, model, warnings}` |
| `GET /sessions` | | `{sessions: [{id, busy, model, title?, saved?}]}` (includes saved sessions for the project) |
| `GET /sessions/{id}` | | transcript: `{messages, todos, mode, model}`; prompts that start a checkpoint carry its `checkpoint` index (409 while a turn runs) |
| `POST /sessions/{id}/fork` | `{name?, point?}` (checkpoint index; omitted means current conversation) | `{id, model, warnings}` for an independent live child; 409 if busy |
| `GET /sessions/{id}/checkpoints` | | `{checkpoints: [{index, prompt, timestamp, code, conversation}], undo}`: one per prompt, oldest first; `code`/`conversation` say what a rewind can restore, `undo` whether a code restore can be undone; 409 if busy |
| `POST /sessions/{id}/rewind` | `{checkpoint, mode?: "both"\|"conversation"\|"code", preview?}` or `{undo: true, preview?}` | like `/rewind`: `{files_restored, prompt, checkpoints, undo}`; with `preview`, only `{summary}` (files a code restore would write or delete, or null); 409 if busy |
| `POST /sessions/{id}/messages` | `{text}`: a prompt or `/command` | 202 `{started}`; while a turn runs, `{queued}` (delivered at the next step) |
| `GET /sessions/{id}/events` | | server-sent events, one JSON object per `data:` line |
| `POST /sessions/{id}/respond` | `{request_id, decision: "allow"\|"allow_always"\|"deny", feedback?}` or `{request_id, answer}` | answers a `permission_request` or `question` |
| `POST /sessions/{id}/interrupt` | | cancels the running turn |
| `DELETE /sessions/{id}` | | Stops the session; `?purge=true` also deletes its saved transcript |

`usage` events include `total_cost_usd` and `price_unknown`.

Event types are the `--output-format stream-json` events (`text`, `reasoning`, `tool_start`, `tool_end`, `tool_progress`, `usage`, `todos`, `warning`, `error`, `info`, `busy`, `mode`, `compacted`, `background_agent`) plus (tool results also include human-facing `display`, with edit diffs):
- `permission_request`: `{request_id, tool, label, detail, reason}`
- `question`: `{request_id, question, options}`
- `tool_call`: `{id, name, input}`. Answer with `POST /sessions/{id}/respond` and `{id, result: {content: "...", is_error: false}}`. Calls time out after ten minutes and are cancelled by interrupt.
- `turn_done`: `{result?, output?, error?, interrupted?, total_cost_usd, price_unknown}`. `output` is set for slash commands that only print text.

Subscribe to `/events` before posting a message so you don't miss the start of the turn. Sessions are saved like terminal sessions and can be resumed with `{resume: "<id>"}` or `chanto --resume <id>`.


## Build your own agent

`query` returns the final answer and cleans up its session. It connects using
`$CHANTO_HOME/server.json` (default `~/.chanto/server.json`), or starts a temporary
`chanto serve --port 0` subprocess if no server is reachable. Put `chanto` on
PATH and configure a model first. Only a server started by the query is stopped.
Python uses only the standard library; automatic startup in TypeScript requires Node.

```python
from chanto_sdk import query, tool

@tool("lookup_ticket", "Look up a ticket", {
    "type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]})
def lookup_ticket(args):
    return {"id": args["id"], "title": "Fix login validation"}

print(query("Look up ticket 42 and propose a fix", tools=[lookup_ticket],
    allowed_tools=["lookup_ticket"], mode="bypassPermissions"))
print(query("Fix the failing tests", cwd="/path/to/repo", verify=True,
    mode="bypassPermissions", max_turns=50))
```

For persistent agents, use `Chanto.connect().session(...)` and `Session.run()`
or `Session.send()` as before. Close the session when finished. Python also accepts
`Tool(name, description, schema, handler)`. Handlers receive the input object;
string results are passed through and other results are JSON encoded. Exceptions
become tool errors. TypeScript tools are `{name, description, inputSchema, handler}`;
handlers may return a value or a promise.

Python session/query keyword arguments match the protocol names. TypeScript uses
`systemPromptAppend`, `allowedTools`, `disallowedTools`, `maxTurns`, `verify`,
`cwd`, `model`, `mode`, `resume`, and `tools` in its options object.

Session options:

| Option | Meaning |
|---|---|
| `cwd` | Existing directory; resolved once for this session without changing the server process directory |
| `system_prompt_append` | Text appended to the system prompt, preserved across prompt refreshes |
| `allowed_tools` | List of permitted tool names; an empty list disables tools |
| `disallowed_tools` | Tool names to exclude; exclusions take precedence |
| `max_turns` | Positive 32-bit integer limiting model calls per user turn |
| `verify` | Boolean enabling/disabling verified mode (the done-gate checks the project before accepting completion) |
| `tools` | Custom definitions: `[{name, description, input_schema}]`; schemas must describe an object |

Invalid options return HTTP 400 with an `error` message. Tool names use 1–64
letters, digits, underscores or hyphens and must be unique, including built-in
names (case insensitive). Custom tools use External permissions; use a permission
callback or an appropriate permission mode. Tool results require string `content`
and boolean `is_error`; malformed responses return 400 without consuming the
pending request. Permission/question response formats remain unchanged.

Examples: `sdk/examples/fix_tests.py`, `sdk/examples/ticket_agent.py`, and
`sdk/examples/ticket_agent.ts`; each includes its run command. Python tests:
`python3 -m unittest discover -s sdk/python -p 'test_*.py'`.

Fork with `await session.fork(name?, point?)` in TypeScript or `session.fork(name=None, point=None)` in Python. Saved transcripts can also be forked by ID through the endpoint.

Rewind with `session.checkpoints()`, then `session.rewind(index, mode)` (`mode` is `"both"`, `"conversation"` or `"code"`; pass `preview` to see what a code restore would write first) and `session.undo_rewind()` / `undoRewind()` to undo the last code restore.

## Background runs

A run is a task Chanto works on by itself in its own git worktree and branch
(see "Background runs" in `docs/reference.md`). The local daemon (`chanto
serve`, started by `Chanto run --bg`) hosts runs for checkouts on its machine;
`chanto serve --runs` hosts them for other machines too. Same bearer token as
sessions. Run ids may be shortened to a unique prefix.

| Route | |
|---|---|
| `POST /runs` | Start a run: `{task, cwd \| source, model?, provider?, base_url?, mode?, verify?, sandbox?, max_cost_usd?, max_minutes?}` → the run (201) |
| `POST /runs/bundles` | Body: a git bundle (`application/octet-stream`) → `{bundle_id}` |
| `GET /runs` | `{runs: [...]}`, newest first |
| `GET /runs/{id}` | The run plus `diff_stat`, `commits`, `last_messages`, `last_seq` |
| `GET /runs/{id}/events?since=<seq>` | SSE: logged events after `since`, then live ones; ends when the run reaches a final state |
| `POST /runs/{id}/respond` | `{request_id?, decision: allow \| allow_always \| deny, feedback?}`, or `{request_id?, answer}` for a question |
| `POST /runs/{id}/stop` / `resume` | Stop (worktree kept) / continue an interrupted run from its saved session |
| `POST /runs/{id}/apply` | `{pr?}`: apply a run of a local checkout onto that checkout's current branch, or push and open a PR → `{output}` |
| `GET /runs/{id}/bundle` | The run's commits after its base, as a git bundle |
| `DELETE /runs/{id}` | Remove the run's worktree, branch and record |

Sources: `cwd` (or `{"kind": "local", "path"}`) is a checkout on the server's
machine. From elsewhere, either `{"kind": "git", "url", "ref", "commit"?}` (the
server fetches the branch) or upload a bundle and pass `{"kind": "bundle",
"bundle_id", "ref", "head"?, "head_ref"?, "snapshot"?}` where `ref` is the ref
inside the bundle (`git bundle create file refs/heads/main`). Code from
elsewhere is checked out under the server's data directory and never trusted
(its project settings, hooks and MCP servers are ignored).

A run's `state` is `queued`, `running`, `needs-input` (see `pending`: the open
`permission_request` or `question`), `done`, `failed`, `cancelled` or
`interrupted`. `verify` is `{status: passed | failed | skipped | off, detail}`.
Events are the session events with `seq` and `run_id`, plus `run_state` (the
whole record, on every state change), `run_started` and `request_done`.

Schedulers and scripts start runs with `POST /runs` and follow them with the
events stream or by polling `GET /runs/{id}`.

```python
h = Chanto.connect()
run = h.start_run("add retries to the HTTP client", cwd="/path/to/repo")
for ev in h.run_events(run["id"]):
    if ev["type"] == "run_state" and ev["run"].get("pending"):
        h.respond_run(run["id"], decision="deny", feedback="don't touch the network")
print(h.run(run["id"])["verify"])
print(h.apply_run(run["id"]))      # or the CLI: chanto runs apply <id>
```

```ts
const h = await Chanto.connect();
const run = await h.startRun({ task: "add retries to the HTTP client", cwd: "/path/to/repo" });
for await (const ev of h.runEvents(run.id)) {
  if (ev.type === "run_state" && ev.run.pending) await h.respondRun(run.id, { decision: "allow" });
}
console.log(await h.applyRun(run.id));
```

Other helpers: `runs()`, `run(id)`, `upload_bundle(path)` / `uploadBundle(bytes)`,
`stop_run`/`stopRun`, `resume_run`/`resumeRun`, `run_bundle`/`runBundle`,
`delete_run`/`deleteRun`.

## License

The SDKs and examples are licensed under [Apache-2.0](LICENSE), the same license as the rest of Chanto, so you can copy them into your own projects.
