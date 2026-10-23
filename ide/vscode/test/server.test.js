const test = require("node:test");
const assert = require("node:assert");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { WebSocket } = require("ws");
const { IdeServer, AUTH_HEADER } = require("../out/server");

function connect(port, token) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(`ws://127.0.0.1:${port}`, { headers: token ? { [AUTH_HEADER]: token } : {} });
    ws.once("open", () => resolve(ws));
    ws.once("error", reject);
    ws.once("unexpected-response", (_req, res) => reject(new Error(`HTTP ${res.statusCode}`)));
  });
}

function call(ws, id, method, params) {
  return new Promise((resolve) => {
    const onMsg = (data) => {
      const m = JSON.parse(data.toString());
      if (m.id === id) {
        ws.off("message", onMsg);
        resolve(m);
      }
    };
    ws.on("message", onMsg);
    ws.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
  });
}

test("lockfile, auth, dispatch, notifications", async () => {
  const lockDir = fs.mkdtempSync(path.join(os.tmpdir(), "chanto-ide-test-"));
  const server = new IdeServer({
    ideName: "Test",
    lockDir,
    workspaceFolders: () => ["/w"],
    handlers: {
      echo: (p) => p,
      boom: () => {
        throw new Error("nope");
      },
    },
  });
  const port = await server.start();
  const lock = JSON.parse(fs.readFileSync(path.join(lockDir, `${port}.lock`), "utf8"));
  assert.deepStrictEqual(lock.workspaceFolders, ["/w"]);
  assert.strictEqual(lock.authToken, server.authToken);
  assert.strictEqual(lock.pid, process.pid);
  assert.strictEqual((fs.statSync(path.join(lockDir, `${port}.lock`)).mode & 0o777).toString(8), "600");

  await assert.rejects(connect(port, "wrong"), /401/);
  await assert.rejects(connect(port), /401/);

  const ws = await connect(port, server.authToken);
  assert.deepStrictEqual((await call(ws, 1, "echo", { a: 1 })).result, { a: 1 });
  assert.strictEqual((await call(ws, 2, "missing", {})).error.code, -32601);
  assert.match((await call(ws, 3, "boom", {})).error.message, /nope/);

  const note = new Promise((r) => ws.once("message", (d) => r(JSON.parse(d.toString()))));
  server.notify("selectionChanged", { text: "x" });
  assert.deepStrictEqual(await note, { jsonrpc: "2.0", method: "selectionChanged", params: { text: "x" } });

  ws.close();
  await server.stop();
  assert.ok(!fs.existsSync(path.join(lockDir, `${port}.lock`)), "lockfile removed on stop");
});
