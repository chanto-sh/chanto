// Chanto's agent (WebAssembly) driving a model that runs on the GPU through
// WebLLM, on a sample project held in memory. Every event is timestamped, so
// each step splits into the model's time, tool time and Chanto's own work.
import init, { Session } from "../pkg/chanto_web.js";
import { CreateMLCEngine } from "https://esm.run/@mlc-ai/web-llm@0.2.85";

const $ = (id) => document.getElementById(id);
const ROOT = "/project";
const PROJECT = {
  "README.md": "# greeter\n\nA tiny app that greets people. Run `python -m src.app NAME`.\n",
  ".gitignore": "__pycache__/\n*.pyc\n",
  "src/__init__.py": "",
  "src/greet.py":
    'def greet(name: str) -> str:\n    """A friendly greeting for `name`."""\n    return f"Helo, {name}!"\n\n\ndef farewell(name: str) -> str:\n    return f"Goodbye, {name}."\n',
  "src/app.py":
    'import sys\n\nfrom src.greet import greet\n\n\ndef main() -> None:\n    name = sys.argv[1] if len(sys.argv) > 1 else "world"\n    print(greet(name))\n\n\nif __name__ == "__main__":\n    main()\n',
  "tests/test_greet.py":
    'from src.greet import farewell, greet\n\n\ndef test_greet():\n    assert greet("Ada") == "Hello, Ada!"\n\n\ndef test_farewell():\n    assert farewell("Ada") == "Goodbye, Ada."\n',
};
const SYSTEM = `You are Chanto, a coding agent. You work on the user's project in ${ROOT} using the tools below: read files before changing them, change them with Edit or Write, and use Grep, Glob and Ls to find things. There is no shell, so you can't run code or tests. Keep replies short. When the task is done, say briefly what you found or changed.`;

const fmt = (ms) => (ms >= 1000 ? `${(ms / 1000).toFixed(2)} s` : `${ms.toFixed(0)} ms`);
let session, engine, shown;

// --- startup: the agent ------------------------------------------------------
async function startAgent() {
  const url = new URL("../pkg/chanto_web_bg.wasm", import.meta.url);
  const t0 = performance.now();
  const resp = await fetch(url);
  const bytes = await resp.arrayBuffer();
  await init({ module_or_path: bytes });
  session = new Session("http://webllm.local/v1", "local", null, SYSTEM, true, ROOT);
  for (const [path, text] of Object.entries(PROJECT)) session.write_file(`${ROOT}/${path}`, text);
  const ms = performance.now() - t0;
  // What a visitor downloads: the wasm compressed (servers send it gzipped).
  const gz = await gzippedSize(bytes);
  $("s-wasm").textContent = `${(gz / 1e6).toFixed(2)} MB`;
  $("s-init").textContent = fmt(ms);
  showFiles();
}

async function gzippedSize(bytes) {
  const stream = new Blob([bytes]).stream().pipeThrough(new CompressionStream("gzip"));
  return (await new Response(stream).arrayBuffer()).byteLength;
}

function showFiles() {
  const list = $("files");
  list.textContent = "";
  for (const path of session.files()) {
    const b = document.createElement("button");
    b.textContent = path.slice(ROOT.length + 1);
    b.className = path === shown ? "on" : "";
    b.onclick = () => showFile(path);
    list.append(b);
  }
  if (shown) showFile(shown);
}

function showFile(path) {
  shown = path;
  $("file-name").textContent = path.slice(ROOT.length + 1);
  $("file-view").textContent = session.read_file(path) ?? "";
  for (const b of $("files").children) b.className = `${ROOT}/${b.textContent}` === path ? "on" : "";
}

// --- startup: the model ------------------------------------------------------
async function loadModel() {
  $("load").disabled = true;
  const t0 = performance.now();
  try {
    engine = await CreateMLCEngine($("model").value, {
      initProgressCallback: (p) => {
        $("progress").style.width = `${Math.round((p.progress ?? 0) * 100)}%`;
        $("progress-text").textContent = p.text ?? "";
      },
    });
    $("s-model").textContent = fmt(performance.now() - t0);
    $("progress-text").textContent = "Ready.";
    $("run").disabled = false;
  } catch (e) {
    $("progress-text").textContent = `Couldn't load the model: ${e.message ?? e}`;
    $("load").disabled = false;
  }
}

// The agent POSTs an OpenAI chat-completions request; WebLLM answers it on
// the GPU, and its chunks go back as the Server-Sent Events Chanto decodes.
function transport(url, headersJson, body) {
  const req = JSON.parse(new TextDecoder().decode(body));
  const enc = new TextEncoder();
  const chunks = engine.chat.completions.create({
    messages: req.messages,
    stream: true,
    stream_options: { include_usage: true },
    stop: req.stop,
    temperature: 0.2,
    max_tokens: 1024,
  });
  let it, done = false;
  return {
    async read() {
      if (done) return { done: true };
      it ??= (await chunks)[Symbol.asyncIterator]();
      const n = await it.next();
      if (n.done) {
        done = true;
        return { done: false, value: enc.encode("data: [DONE]\n\n") };
      }
      return { done: false, value: enc.encode(`data: ${JSON.stringify(n.value)}\n\n`) };
    },
  };
}

// --- a turn, timed -----------------------------------------------------------
async function run() {
  $("run").disabled = true;
  $("log").textContent = "";
  $("timings").textContent = "";
  $("totals").textContent = "";
  const steps = [];
  let current, toolStart, textEl;
  const start = performance.now();
  let lastEnd = start;
  const close = (t) => {
    if (!current) return;
    current.chanto = t - lastEnd - current.model - current.tools;
    lastEnd = t;
    addRow(current);
  };
  const onEvent = (json) => {
    const t = performance.now();
    const e = JSON.parse(json);
    switch (e.type) {
      case "request":
        close(t);
        current = { step: e.step, requested: t, first: 0, model: 0, tools: 0, tokens: 0 };
        steps.push(current);
        textEl = null;
        break;
      case "reply":
        current.model = t - current.requested;
        current.tokens = e.completion_tokens;
        break;
      case "text":
        current.first ||= t - current.requested;
        textEl ??= log("reply", "");
        textEl.lastChild.textContent += e.text;
        break;
      case "tool_call":
        toolStart = t;
        log(`call · ${e.name}`, JSON.stringify(e.arguments));
        break;
      case "tool_result":
        current.tools += t - toolStart;
        log(`${e.name} · ${e.is_error ? "error" : "result"}`, e.content.slice(0, 600), e.is_error);
        showFiles();
        break;
    }
  };
  $("run-state").textContent = "Working…";
  try {
    await session.run($("prompt").value, transport, onEvent);
    $("run-state").textContent = "Done.";
  } catch (e) {
    log("error", String(e), true);
    $("run-state").textContent = "Stopped.";
  }
  close(performance.now());
  addTotals(steps, performance.now() - start);
  showFiles();
  $("run").disabled = false;
}

function log(kind, text, error) {
  const div = document.createElement("div");
  div.className = `ev${error ? " err" : ""}`;
  const k = document.createElement("div");
  k.className = "k";
  k.textContent = kind;
  const body = document.createElement("pre");
  body.textContent = text;
  div.append(k, body);
  $("log").append(div);
  $("log").scrollTop = $("log").scrollHeight;
  return div;
}

function cells(values) {
  const tr = document.createElement("tr");
  for (const v of values) {
    const td = document.createElement("td");
    td.textContent = v;
    tr.append(td);
  }
  return tr;
}

// Tokens per second while generating, after the first token: the time
// before it is the model reading the prompt.
function addRow(s) {
  const gen = s.model - s.first;
  const tps = gen > 0 && s.tokens > 1 ? ((s.tokens - 1) / (gen / 1000)).toFixed(0) : "–";
  $("timings").append(
    cells([String(s.step), s.first ? fmt(s.first) : "–", fmt(s.model), tps, fmt(s.tools), fmt(s.chanto)]),
  );
}

function addTotals(steps, total) {
  const sum = (k) => steps.reduce((a, s) => a + s[k], 0);
  const tr = cells(["Total", fmt(sum("first")), fmt(sum("model")), "", fmt(sum("tools")), fmt(sum("chanto"))]);
  tr.style.fontWeight = "600";
  $("totals").append(tr);
  const share = total > 0 ? ((sum("chanto") + sum("tools")) / total) * 100 : 0;
  $("totals").append(cells([`Turn ${fmt(total)}`, "", "", "", "", `${share.toFixed(1)}% not the model`]));
}

// Tasks a small model can carry out: precise about what to change.
const PRESETS = [
  'Read src/greet.py, then use Edit to change "Helo" to "Hello".',
  "List the project's files, then read src/app.py and say what it does.",
  'Which files mention "farewell"? Use Grep.',
  "Create docs/usage.md that explains how to run the app, based on README.md.",
];
for (const p of PRESETS) {
  const b = document.createElement("button");
  b.textContent = p;
  b.onclick = () => ($("prompt").value = p);
  $("presets").append(b);
}

// --- go ----------------------------------------------------------------------
if (!("gpu" in navigator)) {
  $("warn").textContent =
    "This browser has no WebGPU, so the model can't run here. Try desktop Chrome, Edge or Safari 26.";
}
if (/Android|iPhone|iPad/i.test(navigator.userAgent)) {
  $("warn").textContent += " Phones can run only the smallest model, slowly, and may reload the page.";
}
$("load").onclick = loadModel;
$("run").onclick = run;
startAgent().catch((e) => ($("warn").textContent = `The agent didn't start: ${e}`));
