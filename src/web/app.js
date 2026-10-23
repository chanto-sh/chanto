const $ = id => document.getElementById(id);
const fragment = new URLSearchParams(location.hash.slice(1));
if (fragment.has('token')) sessionStorage.setItem('chanto-token', fragment.get('token'));
// The desktop shell may reopen its last project folder this way.
if (fragment.has('project')) sessionStorage.setItem('chanto-project', fragment.get('project'));
if ((fragment.has('token') || fragment.has('project')) && location.hash) {
  history.replaceState(null, '', location.pathname + location.search);
}
const token = sessionStorage.getItem('chanto-token');
// The desktop app (desktop/) injects the token and installs `window.ipc`.
// The bridge itself is the reliable marker: the shell also uses it when it
// recovers from a failed start, where the injected init script never ran.
const desktop = typeof window.ipc?.postMessage === 'function';
const native = message => { if (desktop) window.ipc.postMessage(JSON.stringify(message)); };
// The project folder sessions start in (the server's own when unset).
let project = sessionStorage.getItem('chanto-project') || '';
let current, controller, ready, assistant, running = false, request, focusBefore, usage = '';
// Prompts sent while the turn runs: they wait for the model's next step.
let queued = [];
// Background runs: the open run, the last event seq already in the log when it
// was opened (replayed history never raises prompts), and its live state.
let runId = null, replayedTo = 0, runState = null;
// Served by `/remote` in the terminal: this page shows that one session.
let remoteMode = false;
const deepRun = fragment.get('run');
const tools = new Map(), prompts = [];
const node = (tag, text, cls) => { const n = document.createElement(tag); if (text != null) n.textContent = text; if (cls) n.className = cls; return n; };
const clean = text => String(text ?? '').replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, '');
// The header names the conversation by its first prompt, like the session list.
function nameSession(text) {
  const line = String(text ?? '').split('\n').map(l => l.trim()).find(Boolean) || '';
  $('session-name').textContent = line.length > 120 ? `${line.slice(0, 119)}…` : line || 'New session';
  $('session-name').title = line || 'New session';
  setTitle();
}
// The tab is named like the desktop window: `name — chanto`. A browser can
// have several sessions open at once, so the conversation's name leads, and
// the project stands in until the first prompt names it.
function setTitle() {
  const name = $('session-name').textContent.trim();
  const folder = project ? project.split(/[\\/]/).filter(Boolean).pop() : '';
  const label = name && name !== 'New session' ? name : folder && folder !== 'chanto' ? folder : '';
  document.title = label ? `${label.length > 40 ? `${label.slice(0, 39)}…` : label} — chanto` : 'chanto';
}
// What a request that never reached the daemon means. The desktop shell can
// start it again; a browser page can only be reloaded after someone does.
function daemonLost() {
  return desktop
    ? "The chanto daemon isn't answering. Restart it below, or reopen chanto."
    : 'Cannot reach the chanto daemon. Check that `chanto web` is still running, then reload this page.';
}
function explain(error) {
  const text = String(error?.message || error);
  if (/failed to fetch|load failed|networkerror|network error|err_connection|err_network/i.test(text)) {
    return daemonLost();
  }
  return text;
}
// Error text usually names commands and paths in backticks; render those as
// code spans (as text nodes, never HTML) so a long message stays scannable.
function fillMessage(element, text) {
  element.replaceChildren();
  String(text ?? '').split('`').forEach((part, i) => {
    if (part) element.append(i % 2 ? node('code', part) : document.createTextNode(part));
  });
}
// The action the error banner offers, when the failure has a next step.
let errorAction = null;
function report(error) {
  const text = explain(error);
  fillMessage($('error-text'), text);
  // A first-run failure can set up a model; a lost daemon can be started
  // again by the desktop shell, and a restarted one reconnected (a browser
  // only gets the command or a reload).
  const setup = /No usable model available/i.test(text);
  const offline = /daemon isn't answering|Cannot reach the chanto daemon/i.test(text);
  const stale = /credentials are out of date/i.test(text);
  errorAction = setup ? 'setup' : stale && desktop ? 'reconnect' : offline && desktop ? 'restart' : null;
  $('error-action').hidden = !errorAction;
  $('error-action').disabled = false;
  if (errorAction === 'setup') $('error-action').textContent = desktop ? 'Set up in Terminal' : 'Copy setup command';
  if (errorAction === 'restart') $('error-action').textContent = 'Restart daemon';
  if (errorAction === 'reconnect') $('error-action').textContent = 'Reconnect';
  $('error').hidden = false;
  $('status').classList.add('error');
  $('status').textContent = `Error · ${text}`;
  $('status').title = text;
}
function clearError() {
  if ($('error').hidden) return;
  $('error-text').textContent = '';
  $('error-action').hidden = true;
  errorAction = null;
  $('error').hidden = true;
  $('status').classList.remove('error');
  // Put the model/mode line back (or the run's task, which run mode shows).
  if (runId) { $('status').textContent = runState?.task || 'Run'; $('status').title = $('status').textContent; }
  else busy(running);
}
async function api(path, method = 'GET', body, signal) {
  let response;
  try {
    response = await fetch(path, { method, signal, headers: { Authorization: `Bearer ${token}`, ...(body ? { 'Content-Type': 'application/json' } : {}) }, body: body ? JSON.stringify(body) : undefined });
  } catch (e) {
    if (signal?.aborted) throw e;
    throw new Error(daemonLost());
  }
  if (!response.ok) {
    // A daemon restarted outside the app writes a new token; the page still
    // has the old one. The desktop shell can read it again and reopen.
    if (response.status === 401) {
      throw new Error(desktop
        ? 'The chanto daemon restarted, so this page\'s credentials are out of date. Reconnect below.'
        : 'This page\'s credentials are out of date. Reload the launch URL printed by `chanto web`.');
    }
    let message = `${response.status} ${response.statusText}`.trim();
    try { const e = await response.json(); if (e.error) message = e.error; } catch { /* not JSON */ }
    throw new Error(message);
  }
  return response;
}
const sessionPath = (id = current) => `/sessions/${encodeURIComponent(id)}`;
const runPath = (id = runId) => `/runs/${encodeURIComponent(id)}`;
const respondPath = () => runId ? `${runPath()}/respond` : `${sessionPath()}/respond`;
const ACTIVE = ['queued', 'running', 'needs-input'];
const ICON = {queued: '…', running: '●', 'needs-input': '⚠', done: '✓', failed: '✗', cancelled: '⊘', interrupted: '⏸'};
// Visual pieces: a fenced HTML or SVG block can be rendered in the preview
// pane, where it is served from memory into a sandboxed, offline frame.
const PREVIEWABLE = new Set(['html', 'htm', 'xhtml', 'svg']);
// Markdown is constructed only with DOM text nodes; raw HTML and URLs stay literal.
function copyText(text, button) {
  const done = ok => { button.classList.toggle('failed', !ok); button.textContent = ok ? 'Copied' : 'Copy failed'; setTimeout(() => { button.classList.remove('failed'); button.textContent = 'Copy'; }, 1200); };
  const write = navigator.clipboard?.writeText(text);
  if (write) write.then(() => done(true), () => done(false)); else done(false);
}
function markdown(target, text) {
  target.replaceChildren();
  let code = null;
  for (const line of text.split('\n')) {
    if (line.startsWith('```')) {
      if (code) code = null;
      else {
        const language = line.slice(3).trim().split(/\s+/)[0].toLowerCase();
        const block = node('div', null, 'code-block'), bar = node('div', null, 'code-bar');
        const pre = node('pre', '');
        bar.append(node('span', language || 'code', 'code-lang'));
        const copy = node('button', 'Copy', 'code-copy'); copy.type = 'button'; copy.title = 'Copy this code';
        copy.onclick = () => copyText(pre.textContent.replace(/\n$/, ''), copy);
        bar.append(copy);
        if (!remoteMode && PREVIEWABLE.has(language)) {
          const button = node('button', 'Preview', 'preview-artifact'); button.type = 'button';
          button.title = `Render this ${language.toUpperCase()} block in the preview pane`;
          button.onclick = () => action(() => showPreview(pre.textContent, language));
          bar.append(button);
        }
        block.append(bar, pre); target.append(block); code = pre;
      }
      continue;
    }
    if (code) { code.textContent += line + '\n'; continue; }
    const heading = /^(#{1,6}) (.*)/.exec(line);
    const item = /^(?:[-*] |\d+\. )(.*)/.exec(line);
    const quote = /^> (.*)/.exec(line);
    const block = node(heading ? `h${heading[1].length}` : item ? 'li' : quote ? 'blockquote' : 'p');
    const source = heading ? heading[2] : item ? item[1] : quote ? quote[1] : line;
    for (const part of source.split(/(`[^`]+`|\*\*[^*]+\*\*)/g)) {
      block.append(part.startsWith('`') ? node('code', part.slice(1,-1)) : part.startsWith('**') ? node('strong', part.slice(2,-2)) : document.createTextNode(part));
    }
    if (item) {
      const tag = /^\d/.test(line) ? 'ol' : 'ul';
      let list = target.lastElementChild;
      if (list?.localName !== tag) { list = node(tag); target.append(list); }
      list.append(block);
    } else target.append(block);
  }
}
function message(role, text) { const a = node('article', null, role); a.append(node('strong', role)); const content = node('div'); a.append(content); markdown(content, text); $('chat').append(a); return content; }
// ---------------------------------------------------------------- preview
// The HTML goes to the server (which serves it sandboxed and without network
// access) rather than into this page, so model-written markup never runs with
// the conversation's origin or the bearer token.
let preview = null; // the last piece: {html, title} or {image, title}
function previewTitle(source, language) {
  const title = /<title[^>]*>([^<]{1,120})<\/title>/i.exec(source);
  return title ? title[1].trim() : `${language === 'svg' ? 'SVG' : 'HTML'} preview`;
}
async function showPreview(source, language) {
  const html = language === 'svg'
    ? `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><style>:root{color-scheme:light dark}html,body{margin:0;height:100%;display:grid;place-items:center}svg{max-width:100%;max-height:100%}</style>${source}`
    : source;
  preview = {html, title: previewTitle(source, language)};
  native({type: 'preview', available: true});
  await renderPreview();
}
function showImage(src) {
  preview = {image: src, title: 'Tool output image'};
  native({type: 'preview', available: true});
  return renderPreview();
}
async function renderPreview() {
  if (!preview) return;
  $('preview-title').textContent = preview.title;
  // A tool's image is already a data URL, so it stays on this page; generated
  // HTML goes through the server's sandboxed frame instead.
  $('preview-image').hidden = !preview.image;
  $('preview-frame').hidden = Boolean(preview.image);
  $('preview-external').hidden = Boolean(preview.image);
  if (preview.image) {
    $('preview-frame').src = 'about:blank';
    $('preview-image').src = preview.image;
  } else {
    $('preview-image').removeAttribute('src');
    const out = await (await api('/artifacts', 'POST', {html: preview.html})).json();
    $('preview-frame').src = out.url;
  }
  $('preview-pane').hidden = false;
}
function hidePreview() { $('preview-frame').src = 'about:blank'; $('preview-image').removeAttribute('src'); $('preview-pane').hidden = true; }
function addImages(container, images) {
  for (const src of images || []) {
    const img = node('img', null, 'tool-image');
    img.src = src; img.alt = 'Tool output image'; img.title = 'Show this image larger';
    img.onclick = () => action(() => showImage(src));
    container.append(img);
  }
  return Boolean(images?.length);
}
function togglePreview() { return preview && $('preview-pane').hidden ? renderPreview() : hidePreview(); }
$('preview-close').onclick = hidePreview;
$('preview-external').onclick = () => window.open($('preview-frame').src, '_blank', 'noopener');
function tool(id, label, output = '') {
  let block = tools.get(id);
  if (!block) { const d = node('details'); d.append(node('summary', label)); block = node('pre'); d.append(block); $('chat').append(d); tools.set(id, block); }
  block.replaceChildren(...clean(output).split('\n').map(line => node('div', line, /^\s*(?:\d+ )?\+/.test(line) ? 'added' : /^\s*(?:\d+ )?-/.test(line) ? 'removed' : '')));
  return block;
}
function busy(value, label = '') {
  running = value;
  $('interrupt').disabled = !value;
  $('rewind').disabled = value || !current;
  for (const b of document.querySelectorAll('.rewind-here')) b.disabled = value;
  if (runId) return;
  // Apply needs something to apply; it stays disabled while the field holds
  // only the configured default (see `syncApply`).
  for (const control of [$('mode'), $('model'), $('list-models')]) control.disabled = value;
  $('apply-model').disabled = value || !$('model').value.trim();
  $('status').textContent = `${$('model').value || 'Default model'} · ${$('mode').value} · ${label || (value ? 'Working…' : 'Ready')}${usage}`;
  $('status').title = $('status').textContent;
  $('status').classList.toggle('busy', Boolean(value));
}
function todos(items) { $('todos').replaceChildren(...(items || []).map(t => node('li', `${t.status === 'completed' ? '✓' : t.status === 'in_progress' ? '→' : '○'} ${t.content}`))); }
async function list() {
  const data = await (await api(`/sessions${project ? `?cwd=${encodeURIComponent(project)}` : ''}`)).json();
  if (!data.sessions.length) { $('sessions').replaceChildren(node('li', project ? 'No sessions in this project yet' : 'No sessions yet', 'empty')); return; }
  const active = data.sessions.find(s => s.id === current && s.title);
  if (active) nameSession(active.title);
  $('sessions').replaceChildren(...data.sessions.map(s => {
    const label = s.title || `New session · ${s.model || s.id.slice(0,8)}`;
    const li = node('li'), open = node('button'), del = node('button', '×');
    open.title = label;
    // A running turn gets a pulsing dot rather than a text bullet; the title
    // truncates inside the flex row so the dot is never clipped.
    open.append(node('span', label, 'session-title'));
    if (s.busy) { const dot = node('span', null, 'busy-dot'); dot.title = 'Turn running'; open.append(dot); }
    open.setAttribute('aria-current', String(s.id === current)); del.setAttribute('aria-label', `Delete session ${s.id}`);
    open.onclick = () => action(async () => { if (s.saved) { await create({resume:s.id}); } else await select(s.id, s.busy && !remoteMode); });
    del.onclick = () => action(async () => { if (!confirm(`Delete session ${s.id}?`)) return; await api(`${sessionPath(s.id)}?purge=true${project ? `&cwd=${encodeURIComponent(project)}` : ''}`, 'DELETE'); if (current === s.id) reset(); await list(); });
    li.append(...(remoteMode ? [open] : [open, del])); return li;
  }));
}
function reset() { controller?.abort(); runId = null; runState = null; runMode(false); current = null; assistant = null; tools.clear(); usage = ''; prompts.length = 0; request = null; preview = null; hidePreview(); native({type: 'preview', available: false}); queued = []; renderQueue(); if ($('dialog').open) $('dialog').close(); $('chat').replaceChildren(); $('jump-latest').hidden = true; todos([]); nameSession(''); busy(false); }
// ------------------------------------------------------------ background runs
async function runs() {
  const data = await (await api('/runs')).json();
  native({type: 'runs', waiting: data.runs.filter(r => r.state === 'needs-input').length, active: data.runs.filter(r => ACTIVE.includes(r.state)).length});
  if (!data.runs.length) { $('runs').replaceChildren(node('li', 'No background runs', 'empty')); return; }
  const item = r => {
    const li = node('li'), b = node('button', `${ICON[r.state] || ''} ${r.state} · ${r.job ? `job ${r.job} · ` : ''}${r.kind === 'goal' ? 'goal · ' : ''}${r.task.split('\n')[0].slice(0, 160)}`, r.state === 'needs-input' ? 'needs-input' : r.state === 'failed' ? 'failed' : '');
    b.title = `Run ${r.id} on ${r.branch}`; b.setAttribute('aria-current', String(r.id === runId));
    b.onclick = () => action(() => openRun(r.id)); li.append(b); return li;
  };
  // A team's sub-task runs are listed together under the team.
  const out = [], teams = new Map();
  for (const r of data.runs) {
    if (!r.team) { out.push(item(r)); continue; }
    let t = teams.get(r.team);
    if (!t) { t = {li: node('li', null, 'team'), head: node('span'), ul: node('ul'), runs: []}; t.li.append(t.head, t.ul); teams.set(r.team, t); out.push(t.li); }
    t.runs.push(r); t.ul.append(item(r));
  }
  for (const [id, t] of teams) {
    const active = t.runs.filter(r => ACTIVE.includes(r.state)).length, waiting = t.runs.filter(r => r.state === 'needs-input').length;
    t.head.textContent = `Team ${id} · ${t.runs.length} run${t.runs.length === 1 ? '' : 's'}${active ? ` · ${active} active` : ''}${waiting ? ` · ${waiting} waiting for you` : ''}`;
    t.ul.setAttribute('aria-label', `Runs of team ${id}`);
  }
  $('runs').replaceChildren(...out);
}
function runMode(on) {
  $('run-bar').hidden = !on; $('composer').hidden = on; $('todos').hidden = on;
  document.querySelector('main > header').hidden = on;
}
function showRun(r) {
  runState = r;
  const parts = [`Run ${r.id}`, r.state + (r.pending ? ' — waiting for you' : ''), r.branch, `$${(r.cost_usd || 0).toFixed(2)}`];
  if (r.job) parts.push(`job ${r.job} (${r.trigger || 'schedule'})`);
  if (r.verify) parts.push(`verify ${r.verify.status}`);
  if (r.isolation) parts.push(r.isolation);
  if (r.error) parts.push(r.error);
  $('run-info').textContent = parts.join(' · ');
  $('status').textContent = r.task;
  const active = ACTIVE.includes(r.state), remote = r.source?.kind !== 'local';
  $('run-stop').disabled = !active;
  $('run-resume').disabled = active || r.state === 'done';
  $('run-apply').disabled = $('run-pr').disabled = active || remote;
  $('run-apply').title = $('run-pr').title = remote ? 'This run\'s code came from another machine: apply it there with chanto runs apply --on' : '';
  // The open request is the run's own `pending`; answered ones go away.
  if (r.pending && request?.request_id !== r.pending.request_id && !prompts.some(p => p.request_id === r.pending.request_id)) { prompts.push(r.pending); showPrompt(); }
  if (!r.pending) dropPrompt();
}
function dropPrompt(id) {
  for (let i = prompts.length - 1; i >= 0; i--) if (!id || prompts[i].request_id === id) prompts.splice(i, 1);
  if (request && (!id || request.request_id === id)) { request = null; if ($('dialog').open) { $('dialog').close(); focusBefore?.focus(); } }
}
async function openRun(id) {
  reset(); runId = id; current = `run:${id}`; runMode(true);
  history.replaceState(null, '', `${location.pathname}${location.search}#run=${encodeURIComponent(id)}`);
  const key = current, r = await (await api(runPath(id))).json();
  if (current !== key) return;
  replayedTo = r.last_seq || 0;
  message('task', r.task); showRun(r); nameSession(r.task);
  controller = new AbortController(); const signal = controller.signal;
  const response = await api(`${runPath(id)}/events`, 'GET', null, signal);
  consume(response, key, signal).catch(e => { if (!signal.aborted && current === key && !/stream ended/.test(e.message)) report(e); });
  await runs();
}
async function runAction(path, body, confirmText) {
  if (confirmText && !confirm(confirmText)) return;
  const out = await (await api(`${runPath()}/${path}`, 'POST', body || {})).json();
  if (out.output) message('chanto', out.output);
  if (out.id) showRun(out);
  await runs();
}
$('refresh-runs').onclick = () => action(runs);
$('run-stop').onclick = () => action(() => runAction('stop', null, 'Stop this run? Its worktree and branch are kept.'));
$('run-resume').onclick = () => action(() => runAction('resume'));
$('run-apply').onclick = () => action(() => runAction('apply', {pr: false}, 'Apply this run\'s commits onto the branch checked out in its repository?'));
$('run-pr').onclick = () => action(() => runAction('apply', {pr: true}, 'Push the run\'s branch and open a pull request?'));
$('run-close').onclick = () => { reset(); history.replaceState(null, '', location.pathname + location.search); action(runs); };

async function snapshot(id) {
  const response = await api(sessionPath(id)); const s = await response.json();
  if (current !== id) return;
  $('model').value = s.model; $('mode').value = s.mode; todos(s.todos);
  // The first prompt names the conversation; reminders aren't part of it.
  const first = s.messages.find(m => m.role === 'user' && (m.content || '').split('<system-reminder>')[0].trim());
  nameSession(first ? first.content.split('<system-reminder>')[0] : '');
  $('chat').replaceChildren(); tools.clear(); assistant = null;
  for (const m of s.messages) {
    // As in the terminal: what Chanto adds to a prompt (and reminder-only turns) isn't shown.
    const text = m.role === 'user' ? (m.content || '').split('<system-reminder>')[0].trim() : m.content;
    if (m.role === 'tool') {
      const pre = tool(`history-${tools.size}`, 'Tool result', m.content);
      if (addImages(pre.parentElement, m.images)) pre.parentElement.open = true;
    } else if (text) {
      const content = message(m.role, text);
      addImages(content, m.images);
      // Prompts that start a checkpoint can be rewound or forked from.
      if (m.checkpoint != null) {
        const b = node('button', 'Rewind…', 'rewind-here'); b.type = 'button'; b.setAttribute('aria-label', 'Rewind or fork before this prompt');
        b.onclick = () => action(() => openCheckpoint(m.checkpoint)); content.parentElement.append(b);
      }
    }
    for (const t of m.tool_calls || []) tool(t.id, t.name, JSON.stringify(t.arguments,null,2));
  }
  busy(false);
}
async function select(id, isBusy = false) {
  reset(); current = id;
  if (!isBusy) { try { await snapshot(id); } catch (e) { if (!e.message.includes('busy')) throw e; isBusy = true; } }
  busy(isBusy); controller = new AbortController();
  ready = stream(id, controller.signal); await ready; await list(); $('prompt').focus();
  // Prompts already waiting for the running turn can still be edited.
  if (isBusy && !remoteMode) await refreshQueue();
}
async function create(extra = {}) {
  const options = { mode: $('mode').value, ...(project ? {cwd: project} : {}), ...extra }; if ($('model').value.trim()) options.model = $('model').value.trim();
  const s = await (await api('/sessions', 'POST', options)).json(); await select(s.id); for (const w of s.warnings || []) message('warning', w);
}
// A new chat: in the desktop app that needs a project folder first, so the
// session never lands in the daemon's own folder.
function newSession() {
  if (desktop && !project) { native({type: 'pick-project'}); return; }
  return create();
}
async function stream(id, signal) {
  const response = await api(`${sessionPath(id)}/events`, 'GET', null, signal);
  // Resolve subscription readiness before sending a turn, then consume in the background.
  consume(response, id, signal).catch(e => { if (!signal.aborted && current === id) { report(new Error(`Event connection lost: ${e.message}. Select the session to reconnect.`)); $('send').disabled = true; } });
  $('send').disabled = false;
}
async function consume(response, id, signal) {
  const reader = response.body.getReader(), decoder = new TextDecoder(); let buffer = '';
  try {
    while (!signal.aborted) {
      const {value,done} = await reader.read(); if (done) throw new Error('stream ended');
      buffer += decoder.decode(value, {stream:true}).replace(/\r\n/g,'\n');
      let end;
      while ((end = buffer.indexOf('\n\n')) >= 0) {
        const frame = buffer.slice(0,end); buffer = buffer.slice(end+2);
        const data = frame.split('\n').filter(l => l.startsWith('data:')).map(l => l.slice(5).trimStart()).join('\n');
        if (data && current === id) event(JSON.parse(data), id);
      }
    }
  } finally { reader.releaseLock(); }
}
function event(e, id) {
  const chat = $('chat'), follow = chat.scrollHeight - chat.scrollTop - chat.clientHeight < 100;
  switch (e.type) {
    case 'assistant_start': assistant = null; break;
    // Remote control: prompts typed in the terminal (or sent from here), and a
    // conversation replaced there (/clear, /resume, /rewind).
    case 'user': assistant = null; message('user', e.text); break;
    case 'reset': if (remoteMode) action(() => select(id)); break;
    case 'text': if (!assistant) assistant = {el:message('assistant',''), text:''}; assistant.text += e.text; markdown(assistant.el, assistant.text); break;
    case 'assistant_end': assistant = null; break;
    case 'tool_start': assistant = null; tool(e.id,e.label); break;
    case 'tool_progress': tool(e.id,'Tool', (tools.get(e.id)?.textContent || '') + '\n' + e.text); break;
    case 'tool_end': {
      const pre = tool(e.id, 'Tool result', [e.display, e.output !== e.display ? e.output : ''].filter(Boolean).join('\n\n'));
      if (e.is_error) {
        const card = pre.parentElement;
        card.classList.add('failed');
        card.querySelector('summary').textContent += ' · failed';
      }
      // A screenshot is the result: show it without a second click.
      if (addImages(pre.parentElement, e.images)) pre.parentElement.open = true;
      break;
    }
    case 'todos': todos(e.todos); break;
    case 'mode': $('mode').value = e.mode; break;
    case 'busy': busy(true,e.label); break;
    case 'usage': usage = ` · context ${e.context_used}/${e.context_max} · tokens ${e.total_in} in / ${e.total_out} out`; busy(running); break;
    case 'permission_request': case 'question': if (runId) break; if (!prompts.some(p => p.request_id === e.request_id) && request?.request_id !== e.request_id) prompts.push(e); showPrompt(); break;
    case 'tool_call': action(() => api(`${sessionPath(id)}/respond`, 'POST', {id:e.id,result:{content:'Browser has no handler for this client tool',is_error:true}})); break;
    case 'turn_done':
      assistant = null;
      busy(false);
      if (e.output) {
        message('chanto', e.output);
        const models = [...e.output.matchAll(/^- `([^`]+)`/gm)].map(m => node('option', m[1]));
        if (models.length) $('models').replaceChildren(...models);
      }
      if (e.error) report(e.error);
      if (e.interrupted) message('chanto', 'Interrupted');
      prompts.length = 0;
      request = null;
      if ($('dialog').open) { $('dialog').close(); focusBefore?.focus(); }
      action(async () => {
        const s = await (await api(sessionPath(id))).json();
        if (current === id) { $('model').value = s.model; $('mode').value = s.mode; busy(false); }
        await refreshQueue();
        await list();
      });
      break;
    case 'warning': case 'error': case 'info': message(e.type,e.message); break;
    case 'background_agent': message('background',`${e.description}: ${e.status}`); break;
    case 'run_state':
      if (!runId) break;
      if (e.seq > replayedTo) { showRun(e.run); action(runs); }
      if (!ACTIVE.includes(e.state)) message('chanto', `Run ${e.state}${e.run?.verify ? ` · verify ${e.run.verify.status}` : ''}${e.run?.error ? ` · ${e.run.error}` : ''}`);
      break;
    case 'request_done': dropPrompt(e.request_id); break;
  }
  if (follow) chat.scrollTop = chat.scrollHeight;
  $('jump-latest').hidden = follow;
}
function showPrompt() {
  if (request || !prompts.length) return;
  request = prompts.shift(); focusBefore = document.activeElement;
  const question = request.type === 'question';
  $('dialog-title').textContent = question ? request.question : request.label || request.tool;
  $('detail').textContent = clean([request.detail,request.reason].filter(Boolean).join('\n'));
  $('answer-label').textContent = question ? 'Your answer' : 'Feedback (optional)'; $('answer').value = ''; $('dialog-error').textContent = '';
  $('options').replaceChildren(...(request.options || []).map(o => { const b=node('button',o); b.type='button'; b.onclick=()=>{$('answer').value=o; $('answer').focus();}; return b; }));
  const later = runId ? [['later','Decide later']] : [];
  $('decisions').replaceChildren(...(question ? [['answer','Submit answer'],['cancel','Cancel']] : [['allow','Allow once'],['allow_always',request.always_label || 'Always allow'],['deny','Deny']]).concat(later).map(([value,label])=>{const b=node('button',label); b.value=value; return b;}));
  $('dialog').showModal(); $('answer').focus();
}
async function reply(value) {
  if(!request) return;
  // A background run keeps waiting; answer from here or a terminal later.
  if(value === 'later') { request=null; $('dialog').close(); focusBefore?.focus(); return; }
  const body = {request_id:request.request_id};
  if(request.type === 'question') { if(value !== 'cancel') body.answer=$('answer').value; }
  else { body.decision=value; body.feedback=$('answer').value; }
  try { await api(respondPath(),'POST',body); request=null; $('dialog').close(); focusBefore?.focus(); showPrompt(); }
  catch(e) { fillMessage($('dialog-error'), explain(e)); }
}
function action(fn) { clearError(); return Promise.resolve().then(fn).catch(report); }
$('error-close').onclick = clearError;
// The banner's action: restart a lost daemon or reconnect to a restarted one
// (or open setup in Terminal); a browser gets the setup command instead.
$('error-action').onclick = async () => {
  if (errorAction === 'restart' || errorAction === 'reconnect') {
    // Both go through the shell: it finds the daemon (starting one if the
    // old one is gone) and reopens the page with the token it is using now.
    native({type: 'restart'});
    $('error-action').disabled = true;
    const reconnect = errorAction === 'reconnect';
    $('error-action').textContent = reconnect ? 'Reconnecting…' : 'Restarting…';
    fillMessage($('error-text'), reconnect ? 'Reconnecting to the chanto daemon…' : 'Starting the chanto daemon…');
    return;
  }
  if (desktop) {
    native({type: 'setup'});
    $('error-action').hidden = true;
    fillMessage($('error-text'), 'Finish setup in the Terminal window, then reload this page.');
    return;
  }
  try { await navigator.clipboard.writeText('chanto setup'); $('error-action').textContent = 'Copied'; }
  catch { $('error-action').textContent = 'Copy failed'; }
  setTimeout(() => { if (!$('error').hidden) $('error-action').textContent = 'Copy setup command'; }, 1400);
};
$('reply').onsubmit=e=>{e.preventDefault(); reply(e.submitter?.value || (request.type==='question'?'answer':'deny'));};
$('dialog').addEventListener('cancel',e=>{e.preventDefault(); reply(runId?'later':request.type==='question'?'cancel':'deny');});
$('new').onclick=()=>action(newSession); $('refresh').onclick=()=>action(list);
// ---------------------------------------------------------------- queue
// A prompt typed while the turn runs is delivered at the model's next step.
// Until the agent takes it, it waits above the composer where it can be edited
// or withdrawn, like pulling a queued message back into the terminal's prompt.
let queueTimer = null, refreshingQueue = false;
function renderQueue() {
  // The terminal's session (`/remote`) keeps its queue in the terminal.
  if (remoteMode) {
    $('queue').hidden = true;
    if (queueTimer) { clearInterval(queueTimer); queueTimer = null; }
    return;
  }
  $('queue').hidden = !queued.length;
  $('queue').replaceChildren(...queued.map(q => {
    const li = node('li'), text = node('span', oneLine(q.text, 120), 'queue-text');
    text.title = q.text;
    const edit = node('button', 'Edit'); edit.type = 'button'; edit.title = 'Move this back into the prompt';
    edit.onclick = () => action(() => editQueued(q.id));
    const remove = node('button', '×'); remove.type = 'button'; remove.title = 'Withdraw this message';
    remove.setAttribute('aria-label', `Withdraw queued message: ${oneLine(q.text, 60)}`);
    remove.onclick = () => action(() => removeQueued(q.id));
    li.append(text, edit, remove); return li;
  }));
  // Poll only while the queue can change: the turn is running, or messages are
  // waiting for the next one.
  if ((running || queued.length) && !queueTimer) {
    queueTimer = setInterval(() => { if (!document.hidden) refreshQueue().catch(() => {}); }, 1000);
  } else if (!running && !queued.length && queueTimer) {
    clearInterval(queueTimer); queueTimer = null;
  }
}
async function refreshQueue() {
  if (!current || remoteMode || refreshingQueue) return;
  refreshingQueue = true;
  try {
    const data = await (await api(`${sessionPath()}/queue`)).json();
    const next = data.queued || [];
    // A message the server took at a step boundary moves into the conversation.
    for (const q of queued) if (!next.some(n => n.id === q.id)) { message('user', q.text); assistant = null; }
    queued = next;
    renderQueue();
  } finally { refreshingQueue = false; }
}
async function removeQueued(id) {
  queued = queued.filter(q => q.id !== id); renderQueue();
  await api(`${sessionPath()}/queue/${encodeURIComponent(id)}`, 'DELETE');
}
async function editQueued(id) {
  const item = queued.find(q => q.id === id); if (!item) return;
  await removeQueued(id);
  $('prompt').value = item.text; $('prompt').focus();
}
async function send(text) {
  // The desktop app never starts a session in the daemon's own folder; ask
  // for a project first and keep what was typed for the next Enter.
  if (!current && desktop && !project) { native({type: 'pick-project'}); return false; }
  if(!current) await create();
  await ready;
  if ($('session-name').textContent === 'New session') nameSession(text);
  const out = await (await api(`${sessionPath()}/messages`,'POST',{text})).json();
  // While the turn runs the server queues it for the model's next step.
  if(out.queued) {
    if (!remoteMode) {
      if(out.message) { queued.push(out.message); renderQueue(); }
      await refreshQueue();
    }
    assistant=null; busy(true); return true;
  }
  if(!remoteMode) message('user',text);
  assistant=null; busy(true);
  return true;
}
$('composer').onsubmit=e=>{e.preventDefault(); const text=$('prompt').value.trim(); if(text) action(async()=>{if(await send(text)) $('prompt').value='';});};
// Scrolled up mid-turn: offer a way back to the live edge without losing the
// reader's place for every following event.
const chatEl = $('chat');
chatEl.addEventListener('scroll', () => { $('jump-latest').hidden = chatEl.scrollHeight - chatEl.scrollTop - chatEl.clientHeight < 100; });
$('jump-latest').onclick = () => { chatEl.scrollTop = chatEl.scrollHeight; $('jump-latest').hidden = true; $('prompt').focus(); };
// The welcome state's suggestions fill the prompt rather than sending it, so
// they can be edited before anything reaches the model.
for (const b of document.querySelectorAll('#suggestions button')) b.onclick = () => { $('prompt').value = b.dataset.prompt; $('prompt').focus(); };
$('prompt').onkeydown=e=>{
  if(e.key!=='Enter'||e.shiftKey) return;
  // Never submit while an IME is composing (CJK input presses Enter to commit).
  if(e.isComposing||e.keyCode===229) return;
  e.preventDefault();
  $('composer').requestSubmit();
};
$('interrupt').onclick=()=>action(()=>api(`${sessionPath()}/interrupt`,'POST'));
// ----------------------------------------------------------- rewind and fork
// Like esc esc in the terminal: pick a prompt, then restore the code, the
// conversation, or both to before it, or fork a new session there.
let rewindFocus = null;
const oneLine = (text, n = 80) => { const t = String(text ?? '').trim().split('\n')[0]; return t.length > n ? `${t.slice(0, n - 1)}…` : t; };
function rewindDialog(title, detail, items, actions) {
  $('rewind-title').textContent = title; $('rewind-detail').textContent = detail; $('rewind-error').textContent = '';
  $('checkpoints').replaceChildren(...items); $('checkpoints').hidden = !items.length;
  $('rewind-actions').replaceChildren(...actions.concat([['Cancel', async () => $('rewind-dialog').close()]]).map(([label, fn, disabled]) => {
    const b = node('button', label); b.type = 'button'; b.disabled = !!disabled;
    b.onclick = () => fn().catch(e => { fillMessage($('rewind-error'), explain(e)); }); return b;
  }));
  if (!$('rewind-dialog').open) { rewindFocus = document.activeElement; $('rewind-dialog').showModal(); }
  $('rewind-dialog').querySelector('button:not(:disabled)')?.focus();
}
$('rewind-dialog').addEventListener('close', () => rewindFocus?.focus());
const checkpointList = async () => (await api(`${sessionPath()}/checkpoints`)).json();
async function openRewind() {
  if (!current || runId || remoteMode) return;
  const data = await checkpointList();
  const items = data.checkpoints.slice().reverse().map(c => {
    const li = node('li'), b = node('button', `${c.index}. ${oneLine(c.prompt, 120)}`); b.type = 'button'; b.title = c.timestamp ? new Date(c.timestamp).toLocaleString() : '';
    b.onclick = () => chooseRewind(c); li.append(b); return li;
  });
  const actions = [['Fork current conversation', () => fork()]];
  if (data.undo) actions.push(['Undo last code restore', () => rewindTo({undo: true})]);
  rewindDialog('Rewind or fork', items.length ? 'Go back to before a prompt:' : 'Nothing to rewind yet.', items, actions);
}
async function openCheckpoint(index) {
  const c = (await checkpointList()).checkpoints.find(c => c.index === index);
  if (!c) throw new Error('That checkpoint is no longer available');
  chooseRewind(c);
}
function chooseRewind(c) {
  rewindDialog(`Before "${oneLine(c.prompt)}"`, `${c.timestamp ? `${new Date(c.timestamp).toLocaleString()} · ` : ''}${c.code ? 'Code can be restored.' : 'No file changes were recorded for this prompt or later ones.'}`, [], [
    ['Restore code and conversation', () => rewindTo({checkpoint: c.index, mode: 'both'})],
    ['Conversation only', () => rewindTo({checkpoint: c.index, mode: 'conversation'}), !c.conversation],
    ['Code only', () => rewindTo({checkpoint: c.index, mode: 'code'}), !c.code],
    ['Fork from here', () => fork(c.index), !c.conversation],
  ]);
}
async function rewindTo(body) {
  const id = current, path = `${sessionPath(id)}/rewind`;
  // Code restores show what they'd write and delete first, like the terminal.
  const preview = await (await api(path, 'POST', {...body, preview: true})).json();
  if (preview.summary && !confirm(`${preview.summary}\n\n${body.undo ? 'Undo restore?' : 'Restore?'}`)) return;
  const out = await (await api(path, 'POST', body)).json();
  $('rewind-dialog').close();
  if (current !== id) return;
  await snapshot(id);
  message('chanto', body.undo ? `Restored the state before the last code restore — ${out.files_restored} file(s) restored`
    : `Rewound to before "${oneLine(out.prompt, 60)}" — ${out.files_restored} file(s) restored${body.mode !== 'conversation' && out.undo ? '; the previous code state is saved (Rewind… › Undo last code restore).' : ''}`);
  // The rewound prompt comes back for editing.
  if (!body.undo && body.mode !== 'code' && !$('prompt').value.trim()) $('prompt').value = out.prompt;
  await list();
}
async function fork(point) {
  const s = await (await api(`${sessionPath()}/fork`, 'POST', point == null ? {} : {point})).json();
  $('rewind-dialog').close();
  await select(s.id); for (const w of s.warnings || []) message('warning', w);
  message('chanto', `Forked session: ${s.id}`);
}
$('rewind').onclick = () => action(openRewind);
// As in the terminal: esc interrupts a running turn; esc twice when idle opens rewind.
let lastEscape = 0;
document.addEventListener('keydown', e => {
  if (e.key !== 'Escape' || document.querySelector('dialog[open]')) return;
  if (running && !$('interrupt').disabled) { $('interrupt').click(); lastEscape = 0; return; }
  if (Date.now() - lastEscape < 600 && !$('rewind').disabled) { lastEscape = 0; action(openRewind); return; }
  lastEscape = Date.now();
});
$('mode').onchange=()=>{if(current) action(()=>send(`/mode ${$('mode').value}`));};
$('model-form').onsubmit=e=>{e.preventDefault(); if(current && $('model').value.trim()) action(()=>send(`/model ${$('model').value.trim()}`));};
const syncApply = () => { $('apply-model').disabled = running || !$('model').value.trim(); };
$('model').addEventListener('input', syncApply);
$('list-models').onclick=()=>action(()=>send('/models'));
// ------------------------------------------------------------------ projects
function showProject() {
  $('project').textContent = project ? project.split(/[\\/]/).filter(Boolean).pop() : desktop ? 'No folder chosen' : 'Server folder';
  $('project').title = project || (desktop ? 'Sessions need a project folder' : 'Sessions start in the folder the server runs in');
  $('open-project').textContent = project ? 'Change' : desktop ? 'Choose…' : 'Change';
  setTitle();
  native({type: 'project', path: project});
}
// One server works in one project at a time: switching closes the other
// project's live sessions (their transcripts stay saved) after asking.
async function openProject(cwd) {
  cwd = String(cwd || '').trim(); if (!cwd) return;
  const previous = project, use = dir => { project = dir; if (dir) sessionStorage.setItem('chanto-project', dir); else sessionStorage.removeItem('chanto-project'); showProject(); };
  use(cwd);
  try { await create({cwd}); }
  catch (e) {
    if (!/is serving/.test(e.message)) { use(previous); throw e; }
    const live = (await (await api('/sessions')).json()).sessions.filter(s => !s.saved);
    if (!confirm(`${e.message.split(';')[0]}.\nClose its ${live.length} open session(s) and switch to ${cwd}? Their transcripts stay saved.`)) { use(previous); return; }
    for (const s of live) await api(sessionPath(s.id), 'DELETE');
    await create({cwd});
  }
}
$('open-project').onclick = () => action(async () => {
  if (desktop) return native({type: 'pick-project'});
  const cwd = prompt('Project folder (absolute path) to start a session in', project);
  if (cwd) await openProject(cwd);
});
// --------------------------------------------------------------------- voice
// The mic records with MediaRecorder; the server transcribes with the
// configured [voice] backend. The text lands in the prompt for review.
let recorder = null, recordStart = 0, recordTimer = null;
async function voiceStatus() {
  const v = await (await api('/voice')).json();
  $('mic').disabled = !v.available;
  // The desktop app has the menu's Dictate; without a backend the button is
  // only a dead control, so leave it out.
  if (desktop && !v.available) $('mic').hidden = true;
  $('mic').title = v.available ? `Dictate a message (${v.backend})` : `Voice input unavailable: ${v.reason}`;
}
async function transcribe(blob) {
  const response = await fetch('/voice/transcribe', { method: 'POST', headers: { Authorization: `Bearer ${token}`, 'Content-Type': blob.type || 'audio/webm' }, body: blob });
  const out = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(out.error || response.statusText);
  return out.text || '';
}
function micLabel(text, pressed) { $('mic').textContent = text; $('mic').setAttribute('aria-pressed', String(pressed)); }
async function toggleMic() {
  if (recorder) { recorder.stop(); return; }
  const stream = await navigator.mediaDevices.getUserMedia({audio: true});
  const chunks = [];
  recorder = new MediaRecorder(stream);
  recorder.ondataavailable = e => { if (e.data.size) chunks.push(e.data); };
  recorder.onstop = () => action(async () => {
    clearInterval(recordTimer); stream.getTracks().forEach(t => t.stop());
    const type = recorder.mimeType; recorder = null;
    micLabel('Transcribing…', false); $('mic').disabled = true;
    try {
      const text = (await transcribe(new Blob(chunks, {type}))).trim();
      if (text) { const box = $('prompt'); box.value = box.value && !/\s$/.test(box.value) ? `${box.value} ${text}` : box.value + text; box.focus(); }
      else $('status').textContent = 'No speech recognized';
    } finally { micLabel('Mic', false); $('mic').disabled = false; }
  });
  recorder.start(); recordStart = Date.now();
  const tick = () => { const s = Math.floor((Date.now() - recordStart) / 1000); micLabel(`● Stop ${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`, true); };
  tick(); recordTimer = setInterval(tick, 500);
}
$('mic').onclick = () => action(toggleMic);
// Hooks for the desktop shell's menus.
window.chanto = {
  newSession: () => action(newSession),
  openProject: cwd => action(() => openProject(cwd)),
  showRuns: () => action(async () => { await runs(); $('runs').scrollIntoView(); ($('runs').querySelector('button') || $('refresh-runs')).focus(); }),
  dictate: () => action(toggleMic),
  togglePreview: () => action(togglePreview),
};
showProject();
if (desktop) document.documentElement.classList.add('desktop');
native({type: 'ready'});
// `/remote` serves only the terminal's session: no runs, new sessions or rewind.
async function remoteSession() {
  const health = await fetch('/health').then(r => r.json()).catch(() => ({}));
  if (!health.remote) return null;
  remoteMode = true;
  document.body.classList.add('remote');
  for (const id of ['new', 'project-line', 'runs-section', 'rewind']) $(id).hidden = true;
  for (const h of document.querySelectorAll('aside h2')) h.hidden = true;
  return health.session;
}
if(token) action(async()=>{const remote=await remoteSession();voiceStatus().catch(()=>{});await list();busy(false);if(remote){await select(remote);return;}await runs();if(deepRun) await openRun(deepRun); setInterval(()=>{ if(!document.hidden) runs().catch(()=>{}); }, 5000);}); else { report('Open the launch URL printed by chanto web to authenticate.'); $('send').disabled = true; $('new').disabled = true; for (const b of document.querySelectorAll('#suggestions button')) b.disabled = true; }
