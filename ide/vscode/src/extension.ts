// VS Code side of the Chanto IDE bridge: implements the protocol methods
// with the editor API and manages proposed-edit diffs.

import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";
import { mentionToken } from "./mention";
import { IdeServer } from "./server";

const EMPTY_SCHEME = "chanto-empty";

interface PendingDiff {
  target: string;
  temp: vscode.Uri;
  tempDir: string;
  done: boolean;
  resolve: (r: { accepted: boolean; contents?: string }) => void;
}

let server: IdeServer | undefined;
const diffs = new Map<string, PendingDiff>(); // key: temp uri

export async function activate(ctx: vscode.ExtensionContext): Promise<void> {
  const out = vscode.window.createOutputChannel("Chanto");
  server = new IdeServer({
    ideName: vscode.env.appName,
    workspaceFolders: () => (vscode.workspace.workspaceFolders ?? []).map((f) => f.uri.fsPath),
    handlers: {
      getSelection: () => currentSelection(),
      getOpenEditors: () => openEditors(),
      getWorkspaceFolders: () => (vscode.workspace.workspaceFolders ?? []).map((f) => f.uri.fsPath),
      getDiagnostics: (p: { path?: string; uri?: string }) => diagnostics(p.path ?? p.uri),
      openFile: (p: { path: string; line?: number; endLine?: number }) => openFile(p),
      openDiff: (p: { path: string; newContents: string; title?: string }) => openDiff(p),
      closeDiffs: (p?: { path?: string }) => closeDiffs(p?.path),
    },
    log: (m) => out.appendLine(m),
  });
  const port = await server.start();
  out.appendLine(`listening on 127.0.0.1:${port}`);

  ctx.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider(EMPTY_SCHEME, { provideTextDocumentContent: () => "" }),
    vscode.workspace.onDidChangeWorkspaceFolders(() => server?.writeLock()),
    vscode.window.onDidChangeTextEditorSelection(debounce(() => server?.notify("selectionChanged", currentSelection()), 150)),
    vscode.window.onDidChangeActiveTextEditor(() => updateDiffContext()),
    vscode.window.tabGroups.onDidChangeTabs((e) => {
      // Closing a proposed-change tab by hand rejects it.
      for (const tab of e.closed) {
        const input = tab.input;
        if (input instanceof vscode.TabInputTextDiff) {
          const d = diffs.get(input.modified.toString());
          if (d && !d.done) finish(d, false);
        }
      }
    }),
    vscode.commands.registerCommand("chanto.openTerminal", () => {
      const cwd = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
      const term = vscode.window.createTerminal({ name: "chanto", cwd, env: { CHANTO_IDE_PORT: String(port) } });
      term.show();
      term.sendText("chanto");
    }),
    vscode.commands.registerCommand("chanto.insertMention", () => insertMention()),
    vscode.commands.registerCommand("chanto.acceptDiff", () => resolveActive(true)),
    vscode.commands.registerCommand("chanto.rejectDiff", () => resolveActive(false)),
    { dispose: () => void server?.stop() },
  );
}

export async function deactivate(): Promise<void> {
  for (const d of diffs.values()) finish(d, false);
  await server?.stop();
}

function currentSelection() {
  const ed = vscode.window.activeTextEditor;
  if (!ed || ed.document.uri.scheme !== "file") return null;
  const s = ed.selection;
  return {
    file: ed.document.uri.fsPath,
    text: ed.document.getText(s),
    startLine: s.start.line + 1,
    endLine: s.end.line + 1,
    isEmpty: s.isEmpty,
  };
}

function openEditors() {
  const out: object[] = [];
  for (const g of vscode.window.tabGroups.all) {
    for (const t of g.tabs) {
      if (t.input instanceof vscode.TabInputText && t.input.uri.scheme === "file") {
        out.push({ file: t.input.uri.fsPath, isActive: t.isActive && g.isActive, isDirty: t.isDirty });
      }
    }
  }
  return out;
}

const SEVERITY = ["error", "warning", "info", "hint"];

function diagnostics(p?: string) {
  const fmt = (uri: vscode.Uri, ds: readonly vscode.Diagnostic[]) => ({
    file: uri.fsPath,
    diagnostics: ds.map((d) => ({
      line: d.range.start.line + 1,
      character: d.range.start.character + 1,
      severity: SEVERITY[d.severity] ?? "error",
      message: d.message,
      source: d.source,
      code: typeof d.code === "object" ? String(d.code.value) : d.code,
    })),
  });
  if (p) {
    const uri = p.includes("://") ? vscode.Uri.parse(p) : vscode.Uri.file(p);
    return [fmt(uri, vscode.languages.getDiagnostics(uri))];
  }
  return vscode.languages.getDiagnostics().filter(([, ds]) => ds.length > 0).map(([u, ds]) => fmt(u, ds));
}

async function openFile(p: { path: string; line?: number; endLine?: number }) {
  const doc = await vscode.workspace.openTextDocument(vscode.Uri.file(p.path));
  const ed = await vscode.window.showTextDocument(doc, { preview: false });
  if (p.line) {
    const start = new vscode.Position(Math.max(0, p.line - 1), 0);
    const end = new vscode.Position(Math.max(0, (p.endLine ?? p.line) - 1), 0);
    ed.selection = new vscode.Selection(start, end);
    ed.revealRange(new vscode.Range(start, end), vscode.TextEditorRevealType.InCenter);
  }
  return { ok: true };
}

function openDiff(p: { path: string; newContents: string; title?: string }): Promise<{ accepted: boolean; contents?: string }> {
  return new Promise((resolve) => {
    const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "chanto-diff-"));
    const tempPath = path.join(tempDir, path.basename(p.path));
    fs.writeFileSync(tempPath, p.newContents);
    const temp = vscode.Uri.file(tempPath);
    const original = fs.existsSync(p.path) ? vscode.Uri.file(p.path) : vscode.Uri.from({ scheme: EMPTY_SCHEME, path: p.path });
    const d: PendingDiff = { target: p.path, temp, tempDir, done: false, resolve };
    diffs.set(temp.toString(), d);
    const rel = vscode.workspace.asRelativePath(p.path);
    const title = p.title ?? `${path.basename(p.path)}: current ↔ proposed (chanto)`;
    void vscode.commands.executeCommand("vscode.diff", original, temp, title, { preview: false }).then(updateDiffContext);
    void vscode.window
      .showInformationMessage(`chanto wants to edit ${rel}. You can change the proposed side before accepting.`, "Accept", "Reject")
      .then((choice) => {
        if (choice && !d.done) finish(d, choice === "Accept");
      });
  });
}

// With a path, only that file's diff (other agents may have their own open).
async function closeDiffs(target?: string) {
  for (const d of [...diffs.values()]) if (!target || d.target === target) finish(d, false);
  return { ok: true };
}

function resolveActive(accepted: boolean) {
  const uri = vscode.window.activeTextEditor?.document.uri.toString();
  const d = (uri && diffs.get(uri)) || (diffs.size === 1 ? [...diffs.values()][0] : undefined);
  if (d) finish(d, accepted);
}

function finish(d: PendingDiff, accepted: boolean) {
  if (d.done) return;
  d.done = true;
  diffs.delete(d.temp.toString());
  // The user may have edited the proposed side (possibly unsaved).
  const doc = vscode.workspace.textDocuments.find((x) => x.uri.toString() === d.temp.toString());
  let contents: string | undefined;
  if (accepted) {
    contents = doc ? doc.getText() : fs.readFileSync(d.temp.fsPath, "utf8");
  }
  d.resolve(accepted ? { accepted, contents } : { accepted });
  void closeTabs(d).finally(() => fs.rmSync(d.tempDir, { recursive: true, force: true }));
  updateDiffContext();
}

async function closeTabs(d: PendingDiff) {
  const doc = vscode.workspace.textDocuments.find((x) => x.uri.toString() === d.temp.toString());
  if (doc?.isDirty) await doc.save(); // avoid a "save changes?" prompt on the temp file
  const tabs = vscode.window.tabGroups.all.flatMap((g) => g.tabs).filter((t) => t.input instanceof vscode.TabInputTextDiff && t.input.modified.toString() === d.temp.toString());
  if (tabs.length) await vscode.window.tabGroups.close(tabs);
}

function updateDiffContext() {
  const uri = vscode.window.activeTextEditor?.document.uri.toString();
  void vscode.commands.executeCommand("setContext", "chanto.diffActive", !!uri && diffs.has(uri));
}

function insertMention() {
  const ed = vscode.window.activeTextEditor;
  if (!ed) return;
  const mention = mentionToken(vscode.workspace.asRelativePath(ed.document.uri, false));
  if (!mention) {
    void vscode.window.showWarningMessage("This file's name can't be typed as an @-mention.");
    return;
  }
  const term = vscode.window.terminals.find((t) => t.name === "chanto") ?? vscode.window.activeTerminal;
  if (!term) {
    void vscode.window.showWarningMessage("No Chanto terminal open. Run 'Chanto: Open in Terminal' first.");
    return;
  }
  term.show();
  term.sendText(`${mention} `, false);
}

function debounce(fn: () => void, ms: number): () => void {
  let t: NodeJS.Timeout | undefined;
  return () => {
    if (t) clearTimeout(t);
    t = setTimeout(fn, ms);
  };
}
