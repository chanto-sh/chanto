// IDE bridge server: a localhost WebSocket speaking JSON-RPC 2.0, plus the
// lockfile chanto uses to find it. Independent of the VS Code API so it can
// be tested (and reused by other editors) under plain Node. See ../../PROTOCOL.md.

import * as crypto from "crypto";
import * as fs from "fs";
import * as http from "http";
import * as os from "os";
import * as path from "path";
import { WebSocket, WebSocketServer } from "ws";

export const AUTH_HEADER = "x-chanto-ide-authorization";
export const PROTOCOL_VERSION = 1;

/** Methods the editor implements. Each returns a JSON-serializable result. */
export interface Handlers {
  [method: string]: (params: any) => Promise<unknown> | unknown;
}

export interface ServerOptions {
  ideName: string;
  workspaceFolders: () => string[];
  handlers: Handlers;
  /** Directory for lockfiles (default: $CHANTO_HOME/ide or ~/.chanto/ide). */
  lockDir?: string;
  log?: (msg: string) => void;
}

export function defaultLockDir(): string {
  const home = process.env.CHANTO_HOME || path.join(os.homedir(), ".chanto");
  return path.join(home, "ide");
}

export class IdeServer {
  readonly authToken = crypto.randomBytes(24).toString("hex");
  port = 0;
  private wss?: WebSocketServer;
  private http?: http.Server;
  private clients = new Set<WebSocket>();
  private lockPath?: string;

  constructor(private opts: ServerOptions) {}

  async start(): Promise<number> {
    this.http = http.createServer((_req, res) => {
      res.writeHead(426, { "content-type": "text/plain" });
      res.end("chanto IDE bridge: WebSocket only\n");
    });
    this.wss = new WebSocketServer({
      server: this.http,
      verifyClient: (info: { req: http.IncomingMessage }) => {
        const token = info.req.headers[AUTH_HEADER];
        return typeof token === "string" && timingSafeEqual(token, this.authToken);
      },
    });
    this.wss.on("connection", (ws) => this.onConnection(ws));
    await new Promise<void>((resolve, reject) => {
      this.http!.once("error", reject);
      this.http!.listen(0, "127.0.0.1", () => resolve());
    });
    this.port = (this.http.address() as { port: number }).port;
    this.writeLock();
    // Async deactivate doesn't always run (editor killed); remove the lockfile on exit.
    process.once("exit", () => this.removeLock());
    return this.port;
  }

  /** Rewrite the lockfile (e.g. after workspace folders change). */
  writeLock(): void {
    if (!this.port) return;
    const dir = this.opts.lockDir ?? defaultLockDir();
    fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
    this.lockPath = path.join(dir, `${this.port}.lock`);
    const lock = {
      pid: process.pid,
      workspaceFolders: this.opts.workspaceFolders(),
      ideName: this.opts.ideName,
      transport: "ws",
      authToken: this.authToken,
      protocolVersion: PROTOCOL_VERSION,
    };
    fs.writeFileSync(this.lockPath, JSON.stringify(lock), { mode: 0o600 });
  }

  get connected(): number {
    return this.clients.size;
  }

  /** Push a notification to every connected client. */
  notify(method: string, params: unknown): void {
    const msg = JSON.stringify({ jsonrpc: "2.0", method, params });
    for (const c of this.clients) {
      if (c.readyState === WebSocket.OPEN) c.send(msg);
    }
  }

  private removeLock(): void {
    if (this.lockPath) {
      try {
        fs.unlinkSync(this.lockPath);
      } catch {
        /* already gone */
      }
    }
  }

  async stop(): Promise<void> {
    this.removeLock();
    for (const c of this.clients) c.terminate();
    await new Promise<void>((r) => (this.wss ? this.wss.close(() => r()) : r()));
    await new Promise<void>((r) => (this.http ? this.http.close(() => r()) : r()));
  }

  private onConnection(ws: WebSocket): void {
    this.clients.add(ws);
    this.opts.log?.(`client connected (${this.clients.size})`);
    ws.on("close", () => this.clients.delete(ws));
    ws.on("message", async (data) => {
      let req: any;
      try {
        req = JSON.parse(data.toString());
      } catch {
        return ws.send(JSON.stringify({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "parse error" } }));
      }
      if (req.id === undefined || req.id === null) return; // notifications from the client are ignored
      const handler = this.opts.handlers[req.method];
      let reply: object;
      if (!handler) {
        reply = { jsonrpc: "2.0", id: req.id, error: { code: -32601, message: `unknown method ${req.method}` } };
      } else {
        try {
          const result = await handler(req.params ?? {});
          reply = { jsonrpc: "2.0", id: req.id, result: result ?? null };
        } catch (e: any) {
          reply = { jsonrpc: "2.0", id: req.id, error: { code: -32000, message: String(e?.message ?? e) } };
        }
      }
      if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(reply));
    });
  }
}

function timingSafeEqual(a: string, b: string): boolean {
  const x = Buffer.from(a);
  const y = Buffer.from(b);
  return x.length === y.length && crypto.timingSafeEqual(x, y);
}
