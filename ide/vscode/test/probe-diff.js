// Leave a proposed diff open for a few seconds (for a screenshot), then close it.
const fs = require("fs");
const path = require("path");
const { WebSocket } = require("ws");
const { AUTH_HEADER } = require("../out/server");

const [lockDir, file, holdMs = "6000"] = process.argv.slice(2);
const lockName = fs.readdirSync(lockDir).find((f) => f.endsWith(".lock"));
const lock = JSON.parse(fs.readFileSync(path.join(lockDir, lockName), "utf8"));
const ws = new WebSocket(`ws://127.0.0.1:${lockName.replace(".lock", "")}`, { headers: { [AUTH_HEADER]: lock.authToken } });
let id = 0;
const call = (method, params) =>
  new Promise((resolve) => {
    const my = ++id;
    ws.on("message", (d) => {
      const m = JSON.parse(d.toString());
      if (m.id === my) resolve(m.result ?? m.error);
    });
    ws.send(JSON.stringify({ jsonrpc: "2.0", id: my, method, params }));
  });
ws.on("open", async () => {
  console.log("diagnostics:", JSON.stringify(await call("getDiagnostics", { path: file })));
  const diff = call("openDiff", { path: file, newContents: fs.readFileSync(file, "utf8").replace("greeting;", "greeting.length;") });
  await new Promise((r) => setTimeout(r, Number(holdMs)));
  await call("closeDiffs", {});
  console.log("diff:", await diff);
  process.exit(0);
});
