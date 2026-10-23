// A stand-in editor: the real IdeServer with scripted handlers. Used by the
// Rust integration test (src/ide.rs) and the Node tests.
//   node test/mock-ide.js <lockDir> <workspaceFolder> <diffMode: accept|edit|reject|hang>
// Prints "READY <port>" once the lockfile is written; exits when stdin closes.

const { IdeServer } = require("../out/server");

const [lockDir, folder, diffMode = "accept"] = process.argv.slice(2);
const diffs = [];

const server = new IdeServer({
  ideName: "Mock IDE",
  lockDir,
  workspaceFolders: () => [folder],
  handlers: {
    getSelection: () => ({ file: `${folder}/src/lib.rs`, text: "fn selected() {}", startLine: 3, endLine: 3, isEmpty: false }),
    getOpenEditors: () => [{ file: `${folder}/src/lib.rs`, isActive: true, isDirty: false }],
    getWorkspaceFolders: () => [folder],
    getDiagnostics: (p) => [
      { file: p.path, diagnostics: [{ line: 2, character: 5, severity: "error", message: "mock type error", source: "mockc" }] },
    ],
    openFile: () => ({ ok: true }),
    openDiff: (p) =>
      new Promise((resolve) => {
        diffs.push(resolve);
        if (diffMode === "accept") resolve({ accepted: true, contents: p.newContents });
        else if (diffMode === "edit") resolve({ accepted: true, contents: p.newContents + "// edited in IDE\n" });
        else if (diffMode === "reject") resolve({ accepted: false });
        // "hang": wait for closeDiffs
      }),
    closeDiffs: () => {
      for (const r of diffs.splice(0)) r({ accepted: false });
      console.log("CLOSED");
      return { ok: true };
    },
  },
});

server.start().then((port) => {
  // Push a selection once a client is connected: sent before that, it reaches nobody.
  server.wss.on("connection", () =>
    setTimeout(() => server.notify("selectionChanged", { file: `${folder}/src/main.rs`, text: "pushed", startLine: 7, endLine: 8, isEmpty: false }), 50));
  console.log(`READY ${port}`);
});
process.stdin.on("end", () => server.stop().then(() => process.exit(0)));
process.stdin.resume();
