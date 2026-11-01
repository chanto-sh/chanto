# SDKs

Use Chanto from your own programs, in Python or TypeScript. The SDKs are experimental: their interface may change in any release, so pin a version.

## Install

You need [Chanto](quickstart.md) itself, since the SDKs drive it through `chanto serve`.

```sh
pip install chanto-sdk
npm install @chanto/sdk
```

## Start

`start` runs a private server for your program on a free port, and stops it when you're done. It uses your normal Chanto config and logins, and doesn't affect a `chanto serve` you already have running.

```python
from chanto_sdk import Chanto

with Chanto.start() as h:
    s = h.session(mode="acceptEdits")
    print(s.run("add a --verbose flag to the CLI and update the tests"))
```

```ts
import { Chanto } from "@chanto/sdk";

const h = await Chanto.start();
const s = await h.session({ mode: "acceptEdits" });
for await (const ev of s.send("explain src/agent.rs")) if (ev.type === "text") process.stdout.write(ev.text);
h.close();
```

To use a server you started yourself, call `Chanto.connect()`, which reads `~/.chanto/server.json`.

Both check the server's version and stop with a message if it's older than the SDK needs. Run `chanto upgrade` if you see that.

## Permissions

Permission prompts are denied by default, so nothing runs unattended unless you opt in. Pass a callback, or a mode such as `acceptEdits` or `auto`:

```python
s = h.session(on_permission=lambda ev: "allow" if ev["tool"] == "Read" else "deny")
```

## More

Sessions, client tools and background runs are covered in the [SDK README](https://github.com/chanto-sh/chanto/blob/main/sdk/README.md).
