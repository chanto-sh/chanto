# SPDX-License-Identifier: Apache-2.0
"""Python client for `chanto serve` (standard library only).

    from chanto_sdk import Chanto

    h = Chanto.connect()                 # reads ~/.chanto/server.json
    # or, with no server running: `with Chanto.start() as h:`
    s = h.session(mode="acceptEdits")
    for ev in s.send("fix the failing test"):
        if ev["type"] == "text":
            print(ev["text"], end="", flush=True)
    print(s.last_result)

Background runs (a task in its own git worktree, see sdk/README.md):

    run = h.start_run("add retries to the HTTP client", cwd="/path/to/repo")
    for ev in h.run_events(run["id"]):
        if ev["type"] == "permission_request":
            h.respond_run(run["id"], decision="allow")
    print(h.run(run["id"])["state"])

Permission prompts and questions are answered by callbacks: `on_permission(ev)`
returns "allow" | "allow_always" | "deny" (default: deny, so nothing runs
unattended unless you opt in), and `on_question(ev)` returns an answer string.
"""

from __future__ import annotations

import json
import os
import threading
import queue
import subprocess
import time
import uuid
import urllib.error
import urllib.request
from typing import Callable, Iterator, Optional

try:
    # The API's types, generated from sdk/schema.json (the server's own wire types).
    from chanto_types import PermissionRequestEvent, QuestionEvent, StreamEvent
except ImportError:  # this file copied on its own
    PermissionRequestEvent = QuestionEvent = StreamEvent = dict  # type: ignore


# The oldest `chanto` these bindings work with.
MIN_SERVER_VERSION = "0.1.12"


def _version(text: str) -> tuple:
    return tuple(int(p) for p in text.split("-")[0].split(".")[:3])


class ChantoError(Exception):
    pass


class Tool:
    """A client tool; the handler receives the input object."""
    def __init__(self, name, description, schema, handler):
        self.name, self.description, self.schema, self.handler = name, description, schema, handler


def tool(name, description, schema):
    def decorate(fn):
        return Tool(name, description, schema, fn)
    return decorate


class Chanto:
    def __init__(self, url: str, token: str):
        self.url = url.rstrip("/")
        self.token = token

    @classmethod
    def connect(cls, url: Optional[str] = None, token: Optional[str] = None) -> "Chanto":
        """Connect to a running server; defaults come from ~/.chanto/server.json."""
        if url is None or token is None:
            home = os.environ.get("CHANTO_HOME") or os.path.expanduser("~/.chanto")
            with open(os.path.join(home, "server.json")) as f:
                info = json.load(f)
            url = url or info["url"]
            token = token or info["token"]
        client = cls(url, token)
        client.check_version()
        return client

    @classmethod
    def start(cls, binary: str = "chanto", *, cwd=None, model: Optional[str] = None, timeout: float = 30) -> "Chanto":
        """Start a private `chanto serve` for this program, on a free port.

        Stop it with `close()`, or use `with Chanto.start() as h:`. It doesn't
        touch ~/.chanto/server.json, so a server you're running keeps working.
        """
        token = uuid.uuid4().hex
        env = dict(os.environ, CHANTO_SERVER_TOKEN=token, CHANTO_PRIVATE_SERVER="1")
        args = [binary, "serve", "--port", "0"] + (["--model", model] if model else [])
        try:
            proc = subprocess.Popen(
                args, env=env, cwd=cwd, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE, text=True,
            )
        except OSError as e:
            raise ChantoError(f"couldn't start {binary}: {e}") from None
        found: queue.Queue = queue.Queue()

        def read_stderr():
            lines = []
            for line in proc.stderr:
                lines.append(line)
                if "listening on " in line:
                    found.put(line.split("listening on ", 1)[1].strip())
            found.put(None)
            client_tail.extend(lines[-5:])

        client_tail: list = []
        threading.Thread(target=read_stderr, daemon=True).start()
        try:
            url = found.get(timeout=timeout)
        except queue.Empty:
            url = None
        if url is None:
            proc.kill()
            proc.wait()
            raise ChantoError(f"`{' '.join(args)}` didn't start: {''.join(client_tail)[-500:]}")
        client = cls(url, token)
        client._process = proc
        try:
            client.check_version()
        except ChantoError:
            client.close()
            raise
        return client

    def close(self) -> None:
        """Stop the server `start` began; does nothing after `connect`."""
        proc = getattr(self, "_process", None)
        if proc is None:
            return
        self._process = None
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()

    def __enter__(self) -> "Chanto":
        return self

    def __exit__(self, *_exc) -> None:
        self.close()

    def check_version(self) -> str:
        """The server's version, after checking it's new enough for this SDK."""
        version = self.health().get("version", "")
        try:
            old = _version(version) < _version(MIN_SERVER_VERSION)
        except ValueError:
            old = False  # A version this can't read isn't reason to refuse.
        if old:
            raise ChantoError(
                f"chanto {version} is older than this SDK needs ({MIN_SERVER_VERSION}); run `chanto upgrade`"
            )
        return version

    def health(self) -> dict:
        try:
            with urllib.request.urlopen(self.url + "/health", timeout=10) as r:
                return json.loads(r.read() or b"{}")
        except OSError as e:
            raise ChantoError(f"no Chanto server at {self.url}: {e}") from None

    def _request(self, method: str, path: str, body: Optional[dict] = None) -> dict:
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(self.url + path, data=data, method=method)
        req.add_header("Authorization", f"Bearer {self.token}")
        if data is not None:
            req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req) as r:
                return json.loads(r.read() or b"{}")
        except urllib.error.HTTPError as e:
            raise ChantoError(f"{method} {path}: {e.code} {e.read().decode(errors='replace')}") from None

    def _raw(self, method: str, path: str, data: Optional[bytes] = None, content_type: str = "application/octet-stream") -> bytes:
        req = urllib.request.Request(self.url + path, data=data, method=method)
        req.add_header("Authorization", f"Bearer {self.token}")
        if data is not None:
            req.add_header("Content-Type", content_type)
        try:
            with urllib.request.urlopen(req) as r:
                return r.read()
        except urllib.error.HTTPError as e:
            raise ChantoError(f"{method} {path}: {e.code} {e.read().decode(errors='replace')}") from None

    # ------------------------------------------------------------ background runs

    def runs(self) -> list:
        """Background runs on this server, newest first."""
        return self._request("GET", "/runs")["runs"]

    def run(self, id: str) -> dict:
        """One run, with `diff_stat`, `commits`, `last_messages` and `last_seq`."""
        return self._request("GET", f"/runs/{id}")

    def start_run(self, task: str, cwd=None, *, source: Optional[dict] = None, model=None, provider=None,
                  base_url=None, mode=None, verify=None, sandbox=None, max_cost_usd=None, max_minutes=None) -> dict:
        """Start a background run from a checkout on the server's machine (`cwd`),
        or from a `source` ({"kind": "git", "url", "ref"} or {"kind": "bundle", ...})."""
        body = {k: v for k, v in dict(task=task, cwd=os.fspath(cwd) if cwd is not None else None, source=source,
            model=model, provider=provider, base_url=base_url, mode=mode, verify=verify, sandbox=sandbox,
            max_cost_usd=max_cost_usd, max_minutes=max_minutes).items() if v is not None}
        return self._request("POST", "/runs", body)

    def upload_bundle(self, path) -> str:
        """Upload a git bundle (`git bundle create`); returns the id for a bundle source."""
        with open(path, "rb") as f:
            return json.loads(self._raw("POST", "/runs/bundles", f.read()))["bundle_id"]

    def run_events(self, id: str, since: int = 0) -> Iterator[dict]:
        """Logged events after `since`, then live ones; ends when the run finishes."""
        req = urllib.request.Request(f"{self.url}/runs/{id}/events?since={since}")
        req.add_header("Authorization", f"Bearer {self.token}")
        data = []
        with urllib.request.urlopen(req) as r:
            for raw in r:
                line = raw.decode().rstrip("\r\n")
                if line.startswith("data:"):
                    data.append(line[5:].lstrip())
                elif line == "" and data:
                    yield json.loads("\n".join(data))
                    data = []

    def respond_run(self, id: str, decision: Optional[str] = None, answer: Optional[str] = None,
                    request_id: Optional[str] = None, feedback: Optional[str] = None) -> dict:
        """Answer the run's open permission request (decision) or question (answer)."""
        body = {k: v for k, v in dict(decision=decision, answer=answer, request_id=request_id, feedback=feedback).items() if v is not None}
        return self._request("POST", f"/runs/{id}/respond", body)

    def stop_run(self, id: str) -> dict:
        return self._request("POST", f"/runs/{id}/stop", {})

    def resume_run(self, id: str) -> dict:
        return self._request("POST", f"/runs/{id}/resume", {})

    def apply_run(self, id: str, pr: bool = False) -> str:
        """Apply a local run onto its checkout's current branch (or open a PR)."""
        return self._request("POST", f"/runs/{id}/apply", {"pr": pr})["output"]

    def run_bundle(self, id: str) -> bytes:
        """The run's commits as a git bundle (fetch it with `git fetch <file> refs/heads/<branch>`)."""
        return self._raw("GET", f"/runs/{id}/bundle")

    def delete_run(self, id: str) -> None:
        self._request("DELETE", f"/runs/{id}")

    def sessions(self) -> list:
        return self._request("GET", "/sessions")["sessions"]

    def session(self, model=None, mode=None, resume=None, *, system_prompt_append=None,
                allowed_tools=None, disallowed_tools=None, max_turns=None, verify=None,
                cwd=None, tools=None, **callbacks) -> "Session":
        body = {k: v for k, v in dict(model=model, mode=mode, resume=resume,
            system_prompt_append=system_prompt_append, allowed_tools=allowed_tools,
            disallowed_tools=disallowed_tools, max_turns=max_turns, verify=verify,
            cwd=os.fspath(cwd) if cwd is not None else None).items() if v is not None}
        tools = list(tools or [])
        body["tools"] = [dict(name=t.name, description=t.description, input_schema=t.schema) for t in tools]
        info = self._request("POST", "/sessions", body)
        return Session(self, info["id"], tools=tools, **callbacks)


class Session:
    def __init__(
        self,
        client: Chanto,
        id: str,
        on_permission: Optional[Callable[[PermissionRequestEvent], str]] = None,
        on_question: Optional[Callable[[QuestionEvent], str]] = None,
        tools=None,
    ):
        self.tools = {t.name: t for t in tools or []}
        self.client = client
        self.id = id
        self.on_permission = on_permission or (lambda ev: "deny")
        self.on_question = on_question or (lambda ev: (ev.get("options") or [""])[0])
        self.last_result: Optional[str] = None
        self._events: "queue.Queue[dict]" = queue.Queue()
        self._reader = threading.Thread(target=self._read_events, daemon=True)
        self._ready = threading.Event()
        self._reader.start()
        self._ready.wait(10)

    def fork(self, name=None, point=None) -> "Session":
        body = {k: v for k, v in dict(name=name, point=point).items() if v is not None}
        info = self.client._request("POST", f"/sessions/{self.id}/fork", body)
        return Session(self.client, info["id"], on_permission=self.on_permission,
                       on_question=self.on_question, tools=self.tools.values())

    def _read_events(self) -> None:
        req = urllib.request.Request(f"{self.client.url}/sessions/{self.id}/events")
        req.add_header("Authorization", f"Bearer {self.client.token}")
        try:
            with urllib.request.urlopen(req) as r:
                self._ready.set()
                data = []
                for raw in r:
                    line = raw.decode().rstrip("\r\n")
                    if line.startswith("data:"):
                        data.append(line[5:].lstrip())
                    elif line == "" and data:
                        self._events.put(json.loads("\n".join(data)))
                        data = []
            self._events.put({"type": "turn_done", "error": "event stream closed"})
        except Exception as e:
            self._events.put({"type": "turn_done", "error": str(e)})
        finally:
            self._ready.set()

    def send(self, text: str, timeout: Optional[float] = None) -> Iterator[StreamEvent]:
        """Send a prompt (or /command) and yield events until the turn ends."""
        self.client._request("POST", f"/sessions/{self.id}/messages", {"text": text})
        while True:
            ev = self._events.get(timeout=timeout)
            t = ev.get("type")
            if t == "permission_request":
                decision = self.on_permission(ev)
                self.client._request("POST", f"/sessions/{self.id}/respond", {"request_id": ev["request_id"], "decision": decision})
            elif t == "question":
                self.client._request("POST", f"/sessions/{self.id}/respond", {"request_id": ev["request_id"], "answer": self.on_question(ev)})
            elif t == "tool_call":
                try:
                    content = self.tools[ev["name"]].handler(ev["input"])
                    result = {"content": content if isinstance(content, str) else json.dumps(content), "is_error": False}
                except Exception as e:
                    result = {"content": str(e), "is_error": True}
                self.client._request("POST", f"/sessions/{self.id}/respond", {"id": ev["id"], "result": result})
            yield ev
            if t == "turn_done":
                if ev.get("error"):
                    raise ChantoError(ev["error"])
                self.last_result = ev.get("result") or ev.get("output")
                return

    def run(self, text: str, **kw) -> Optional[str]:
        """Send a prompt and return the final answer."""
        for _ in self.send(text, **kw):
            pass
        return self.last_result

    def interrupt(self) -> None:
        self.client._request("POST", f"/sessions/{self.id}/interrupt", {})

    def checkpoints(self) -> dict:
        """The prompts this session can be rewound to (and whether each has code to
        restore), plus whether the last code restore can be undone."""
        return self.client._request("GET", f"/sessions/{self.id}/checkpoints")

    def rewind(self, checkpoint: int, mode: str = "both", preview: bool = False) -> dict:
        """Go back to before a prompt: restore its "code", the "conversation", or
        "both". With preview=True, nothing changes; returns what would be restored."""
        return self.client._request("POST", f"/sessions/{self.id}/rewind",
                                    {"checkpoint": checkpoint, "mode": mode, "preview": preview})

    def undo_rewind(self) -> dict:
        """Undo the last code restore."""
        return self.client._request("POST", f"/sessions/{self.id}/rewind", {"undo": True})

    def transcript(self) -> dict:
        return self.client._request("GET", f"/sessions/{self.id}")

    def close(self) -> None:
        self.client._request("DELETE", f"/sessions/{self.id}")


def query(prompt: str, **options) -> Optional[str]:
    """Run a prompt, starting a temporary local server if none is available."""
    process = None
    session = None
    try:
        try:
            client = Chanto.connect()
            client._request("GET", "/health")
        except (OSError, ValueError, ChantoError):
            # Port zero asks the OS for a free port; discovery supplies its URL.
            token = uuid.uuid4().hex
            process = subprocess.Popen(["chanto", "serve", "--port", "0"],
                env={**os.environ, "CHANTO_SERVER_TOKEN": token},
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            deadline = time.monotonic() + 30
            while True:
                if process.poll() is not None:
                    raise ChantoError("chanto serve exited during startup")
                try:
                    client = Chanto.connect()
                    if client.token == token:
                        client._request("GET", "/health")
                        break
                except (OSError, ValueError, ChantoError):
                    pass
                if time.monotonic() >= deadline:
                    raise ChantoError("chanto serve startup timed out")
                time.sleep(0.05)
        session = client.session(**options)
        return session.run(prompt)
    finally:
        try:
            if session is not None:
                session.close()
        finally:
            if process is not None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
