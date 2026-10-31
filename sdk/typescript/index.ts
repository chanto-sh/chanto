// SPDX-License-Identifier: Apache-2.0
// TypeScript client for `chanto serve` (no dependencies; Node 18+ or Bun/Deno with fetch).
//
//   import { Chanto } from "./index";
//   const h = await Chanto.connect();
//   const s = await h.session({ mode: "acceptEdits", onPermission: () => "allow" });
//   for await (const ev of s.send("fix the failing test")) if (ev.type === "text") process.stdout.write(ev.text);
//
// Background runs (a task in its own git worktree, see sdk/README.md):
//   const run = await h.startRun({ task: "add retries", cwd: "/path/to/repo" });
//   for await (const ev of h.runEvents(run.id)) if (ev.type === "permission_request") await h.respondRun(run.id, { decision: "allow" });

import type { Decision, Mode, PermissionRequestEvent, QuestionEvent, StreamEvent } from "./types.js";

// The API's types, generated from sdk/schema.json (the server's own wire types).
export type * from "./types.js";
/** An event on a session's stream. */
export type ChantoEvent = StreamEvent;
/** An event on a background run's stream (its own format: see sdk/README.md). */
export type RunEvent = { type: string; [k: string]: any };

export interface Tool {
  name: string;
  description: string;
  inputSchema: Record<string, unknown>;
  handler: (input: any) => unknown | Promise<unknown>;
}

export interface SessionOptions {
  systemPromptAppend?: string;
  allowedTools?: string[];
  disallowedTools?: string[];
  maxTurns?: number;
  verify?: boolean;
  cwd?: string;
  tools?: Tool[];
  model?: string;
  mode?: Mode;
  resume?: string;
  /** Default: deny, so nothing runs unattended unless you opt in. */
  onPermission?: (ev: PermissionRequestEvent) => Decision | Promise<Decision>;
  onQuestion?: (ev: QuestionEvent) => string | Promise<string>;
}

export interface RunOptions {
  task: string;
  /** A checkout on the server's machine... */
  cwd?: string;
  /** ...or code from elsewhere: {kind: "git", url, ref, commit?} | {kind: "bundle", bundle_id, ref, head?, snapshot?}. */
  source?: Record<string, unknown>;
  model?: string;
  provider?: string;
  base_url?: string;
  mode?: string;
  verify?: boolean;
  sandbox?: boolean;
  max_cost_usd?: number;
  max_minutes?: number;
}

export class Chanto {
  constructor(public url: string, public token: string) {
    this.url = url.replace(/\/$/, "");
  }

  /** Defaults come from ~/.chanto/server.json (Node only). */
  static async connect(url?: string, token?: string): Promise<Chanto> {
    if (!url || !token) {
      const fs = await import("node:fs");
      const os = await import("node:os");
      const path = await import("node:path");
      const home = process.env.CHANTO_HOME ?? path.join(os.homedir(), ".chanto");
      const info = JSON.parse(fs.readFileSync(path.join(home, "server.json"), "utf8"));
      url ??= info.url;
      token ??= info.token;
    }
    return new Chanto(url!, token!);
  }

  async request(method: string, p: string, body?: unknown): Promise<any> {
    const r = await fetch(this.url + p, {
      method,
      headers: { Authorization: `Bearer ${this.token}`, ...(body ? { "Content-Type": "application/json" } : {}) },
      body: body ? JSON.stringify(body) : undefined,
    });
    if (!r.ok) throw new Error(`${method} ${p}: ${r.status} ${await r.text()}`);
    return r.json();
  }

  private async raw(method: string, p: string, body?: Uint8Array): Promise<Response> {
    const r = await fetch(this.url + p, {
      method,
      headers: { Authorization: `Bearer ${this.token}`, ...(body ? { "Content-Type": "application/octet-stream" } : {}) },
      // A copy is backed by a plain ArrayBuffer, which fetch's body type requires.
      body: body?.slice(),
    });
    if (!r.ok) throw new Error(`${method} ${p}: ${r.status} ${await r.text()}`);
    return r;
  }

  // ---------------------------------------------------------- background runs

  /** Background runs on this server, newest first. */
  async runs(): Promise<any[]> {
    return (await this.request("GET", "/runs")).runs;
  }
  /** One run, with diff_stat, commits, last_messages and last_seq. */
  async run(id: string): Promise<any> {
    return this.request("GET", `/runs/${id}`);
  }
  async startRun(opts: RunOptions): Promise<any> {
    return this.request("POST", "/runs", opts);
  }
  /** Upload a git bundle; returns the bundle_id for a {kind: "bundle"} source. */
  async uploadBundle(bytes: Uint8Array): Promise<string> {
    return (await (await this.raw("POST", "/runs/bundles", bytes)).json()).bundle_id;
  }
  /** Logged events after `since`, then live ones; ends when the run finishes. */
  async *runEvents(id: string, since = 0): AsyncGenerator<RunEvent> {
    const r = await this.raw("GET", `/runs/${id}/events?since=${since}`);
    const reader = r.body!.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) return;
      buffer += decoder.decode(value, { stream: true }).replace(/\r\n/g, "\n");
      let end;
      while ((end = buffer.indexOf("\n\n")) >= 0) {
        const frame = buffer.slice(0, end);
        buffer = buffer.slice(end + 2);
        const data = frame.split("\n").filter(l => l.startsWith("data:")).map(l => l.slice(5).trimStart()).join("\n");
        if (data) yield JSON.parse(data);
      }
    }
  }
  /** Answer the open permission request ({decision}) or question ({answer}). */
  async respondRun(id: string, reply: { decision?: Decision; answer?: string; feedback?: string; request_id?: string }): Promise<any> {
    return this.request("POST", `/runs/${id}/respond`, reply);
  }
  async stopRun(id: string): Promise<any> {
    return this.request("POST", `/runs/${id}/stop`, {});
  }
  async resumeRun(id: string): Promise<any> {
    return this.request("POST", `/runs/${id}/resume`, {});
  }
  /** Apply a local run onto its checkout's current branch, or push and open a PR. */
  async applyRun(id: string, pr = false): Promise<string> {
    return (await this.request("POST", `/runs/${id}/apply`, { pr })).output;
  }
  /** The run's commits as a git bundle. */
  async runBundle(id: string): Promise<Uint8Array> {
    return new Uint8Array(await (await this.raw("GET", `/runs/${id}/bundle`)).arrayBuffer());
  }
  async deleteRun(id: string): Promise<void> {
    await this.request("DELETE", `/runs/${id}`);
  }

  async sessions(): Promise<any[]> {
    return (await this.request("GET", "/sessions")).sessions;
  }

  async session(opts: SessionOptions = {}): Promise<Session> {
    const { model, mode, resume, verify, cwd } = opts;
    const info = await this.request("POST", "/sessions", {
      model, mode, resume, verify, cwd, system_prompt_append: opts.systemPromptAppend,
      allowed_tools: opts.allowedTools, disallowed_tools: opts.disallowedTools, max_turns: opts.maxTurns,
      tools: opts.tools?.map(t => ({ name: t.name, description: t.description, input_schema: t.inputSchema })),
    });
    const s = new Session(this, info.id, opts);
    await s.open();
    return s;
  }
}

export class Session {
  lastResult?: string;
  private queue: ChantoEvent[] = [];
  private wake?: () => void;
  private abort = new AbortController();

  constructor(private client: Chanto, public id: string, private opts: SessionOptions) {}

  async fork(name?: string, point?: number): Promise<Session> {
    const info = await this.client.request("POST", `/sessions/${this.id}/fork`, { name, point });
    const child = new Session(this.client, info.id, this.opts);
    await child.open();
    return child;
  }

  async open(): Promise<void> {
    const r = await fetch(`${this.client.url}/sessions/${this.id}/events`, {
      headers: { Authorization: `Bearer ${this.client.token}` },
      signal: this.abort.signal,
    });
    if (!r.ok || !r.body) throw new Error(`events: ${r.status}`);
    const reader = r.body.getReader();
    const dec = new TextDecoder();
    (async () => {
      let buf = "";
      for (;;) {
        const { value, done } = await reader.read().catch(() => ({ value: undefined, done: true }));
        if (done) {
          // Ends the turn like the server's own turn_done; the cost so far is unknown.
          this.queue.push({ type: "turn_done", error: "event stream closed", total_cost_usd: 0, price_unknown: true });
          this.wake?.();
          return;
        }
        buf += dec.decode(value, { stream: true });
        let i: number;
        while ((i = buf.indexOf("\n\n")) >= 0) {
          const chunk = buf.slice(0, i);
          buf = buf.slice(i + 2);
          const data = chunk
            .split("\n")
            .filter((l) => l.startsWith("data:"))
            .map((l) => l.slice(5).trimStart())
            .join("\n");
          if (data) {
            this.queue.push(JSON.parse(data));
            this.wake?.();
          }
        }
      }
    })();
  }

  private async next(): Promise<ChantoEvent> {
    while (this.queue.length === 0) await new Promise<void>((r) => (this.wake = r));
    return this.queue.shift()!;
  }

  /** Send a prompt (or /command) and yield events until the turn ends. */
  async *send(text: string): AsyncGenerator<ChantoEvent> {
    await this.client.request("POST", `/sessions/${this.id}/messages`, { text });
    for (;;) {
      const ev = await this.next();
      if (ev.type === "permission_request") {
        const decision = (await this.opts.onPermission?.(ev)) ?? "deny";
        await this.client.request("POST", `/sessions/${this.id}/respond`, { request_id: ev.request_id, decision });
      } else if (ev.type === "question") {
        const answer = (await this.opts.onQuestion?.(ev)) ?? ev.options?.[0] ?? "";
        await this.client.request("POST", `/sessions/${this.id}/respond`, { request_id: ev.request_id, answer });
      }
      if (ev.type === "tool_call") {
        let result;
        try {
          const tool = this.opts.tools?.find(t => t.name === ev.name);
          if (!tool) throw new Error(`Unknown tool ${ev.name}`);
          const content = await tool.handler(ev.input);
          result = { content: typeof content === "string" ? content : JSON.stringify(content) ?? "", is_error: false };
        } catch (e) {
          result = { content: String(e), is_error: true };
        }
        await this.client.request("POST", `/sessions/${this.id}/respond`, { id: ev.id, result });
      }
      yield ev;
      if (ev.type === "turn_done") {
        if (ev.error) throw new Error(ev.error);
        this.lastResult = ev.result ?? ev.output ?? undefined;
        return;
      }
    }
  }

  /** Send a prompt and return the final answer. */
  async run(text: string): Promise<string | undefined> {
    for await (const _ of this.send(text)) {
    }
    return this.lastResult;
  }

  interrupt() {
    return this.client.request("POST", `/sessions/${this.id}/interrupt`, {});
  }
  /** The prompts this session can be rewound to, and whether the last code restore can be undone. */
  checkpoints() {
    return this.client.request("GET", `/sessions/${this.id}/checkpoints`);
  }
  /** Go back to before a prompt: restore its code, the conversation, or both. `preview` changes nothing. */
  rewind(checkpoint: number, mode: "both" | "conversation" | "code" = "both", preview = false) {
    return this.client.request("POST", `/sessions/${this.id}/rewind`, { checkpoint, mode, preview });
  }
  /** Undo the last code restore. */
  undoRewind() {
    return this.client.request("POST", `/sessions/${this.id}/rewind`, { undo: true });
  }
  transcript() {
    return this.client.request("GET", `/sessions/${this.id}`);
  }
  async close() {
    this.abort.abort();
    await this.client.request("DELETE", `/sessions/${this.id}`);
  }
}


/** One-call agent query. Automatic server startup requires Node. */
export async function query(prompt: string, options: SessionOptions = {}): Promise<string | undefined> {
  let child: import("node:child_process").ChildProcess | undefined;
  let session: Session | undefined;
  try {
    let client: Chanto;
    try {
      client = await Chanto.connect();
      await client.request("GET", "/health");
    } catch {
      const { spawn } = await import("node:child_process");
      const { randomUUID } = await import("node:crypto");
      const token = randomUUID();
      child = spawn("chanto", ["serve", "--port", "0"], {
        env: { ...process.env, CHANTO_SERVER_TOKEN: token }, stdio: "ignore",
      });
      let spawnError: Error | undefined;
      child.on("error", e => { spawnError = e; });
      const deadline = Date.now() + 30000;
      for (;;) {
        if (spawnError) throw spawnError;
        if (child.exitCode !== null) throw new Error("chanto serve exited during startup");
        try {
          client = await Chanto.connect();
          if (client.token === token) {
            await client.request("GET", "/health");
            break;
          }
        } catch {}
        if (Date.now() >= deadline) throw new Error("chanto serve startup timed out");
        await new Promise(r => setTimeout(r, 50));
      }
    }
    session = await client.session(options);
    return await session.run(prompt);
  } finally {
    try {
      await session?.close();
    } finally {
      if (child && child.exitCode === null && child.pid) {
        const stopped = new Promise<void>(r => child!.once("exit", () => r()));
        child.kill("SIGTERM");
        const timer = setTimeout(() => child!.kill("SIGKILL"), 5000);
        await stopped;
        clearTimeout(timer);
      }
    }
  }
}
