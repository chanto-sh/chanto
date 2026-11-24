# @chanto/web

[Chanto](https://chanto.sh)'s agent in the browser: the agent loop and its file
tools (Read, Write, Edit, Grep, Glob, Ls) compiled to WebAssembly, working on
a project held in memory. It's what runs on [chanto.sh/try](https://chanto.sh/try).

**Experimental.** It's released with Chanto under the same version, and its
interface may change in any release.

There is no shell and no network of its own: the page supplies the model.
`transport(url, headersJson, body)` receives each OpenAI-style
`/chat/completions` request and returns a reader whose `read()` yields the
Server-Sent Events of the reply, so the model can be one in the browser
(WebGPU), a server the page can reach, or a recording.

```js
import init, { Session } from '@chanto/web';

await init();
const session = new Session(baseUrl, model, apiKey, systemPrompt, textTools, '/project');
session.write_file('/project/src/app.py', source);
const answer = await session.run('Fix the typo in src/app.py', transport, (json) => {
  const event = JSON.parse(json); // request, text, tool_call, tool_result, reply
});
console.log(session.read_file('/project/src/app.py'));
```

For your own code, use the CLI: <https://chanto.sh/download>.
