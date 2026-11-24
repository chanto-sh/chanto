# @chanto/sdk

TypeScript client for `chanto serve`, with no dependencies (Node 18+, Bun or Deno). [Install Chanto](https://chanto.sh/download) first.

```ts
import { Chanto } from "@chanto/sdk";

const h = await Chanto.start();
const s = await h.session({ mode: "acceptEdits" });
for await (const ev of s.send("explain src/agent.rs")) if (ev.type === "text") process.stdout.write(ev.text);
h.close();
```

Experimental: the interface may change in any release. See the [SDK docs](https://chanto.sh/docs/sdk).
