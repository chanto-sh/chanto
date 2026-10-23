// Manual check against a running editor: connect via the lockfile and
// exercise the protocol.  node test/probe.js <lockDir> <file-in-workspace>
const fs = require("fs");
const path = require("path");
const { WebSocket } = require("ws");
const { AUTH_HEADER } = require("../out/server");

const [lockDir, file] = process.argv.slice(2);
const lockName = fs.readdirSync(lockDir).find((f) => f.endsWith(".lock"));
const lock = JSON.parse(fs.readFileSync(path.join(lockDir, lockName), "utf8"));
const port = lockName.replace(".lock", "");
console.log("lock:", { ...lock, authToken: "<redacted>" });

const ws = new WebSocket(`ws://127.0.0.1:${port}`, { headers: { [AUTH_HEADER]: lock.authToken } });
let id = 0;
const call = (method, params) =>
  new Promise((resolve) => {
    const my = ++id;
    const onMsg = (d) => {
      const m = JSON.parse(d.toString());
      if (m.id === my) {
        ws.off("message", onMsg);
        resolve(m.result ?? m.error);
      }
    };
    ws.on("message", onMsg);
    ws.send(JSON.stringify({ jsonrpc: "2.0", id: my, method, params }));
  });

ws.on("open", async () => {
  console.log("openFile:", await call("openFile", { path: file, line: 2 }));
  await new Promise((r) => setTimeout(r, 800));
  console.log("getSelection:", await call("getSelection", {}));
  console.log("getOpenEditors:", await call("getOpenEditors", {}));
  console.log("getDiagnostics:", JSON.stringify(await call("getDiagnostics", { path: file })));
  const diff = call("openDiff", { path: file, newContents: "proposed\n" });
  await new Promise((r) => setTimeout(r, 1500));
  console.log("closeDiffs:", await call("closeDiffs", {}));
  console.log("openDiff result:", await diff);
  ws.close();
  process.exit(0);
});
ws.on("error", (e) => {
  console.error("error:", e.message);
  process.exit(1);
});
