#!/usr/bin/env python3
"""Chaos runs against the fake model, judged by what must always hold.

Whatever a run does, these must stay true, so a failure is nearly always a bug
and not timing:

  no panic        no Rust panic in the output or logs
  no hang         it exits within the time limit
  no orphans      nothing it started is still running a moment later (Unix)
  sessions parse  every saved session is valid JSON, with no temp files left
  contained       a file outside the project is untouched
  no leak         a planted secret never reaches the model

Tiers: `pr` is a fixed set that takes minutes; `nightly` adds the slower cases,
more crash points and the seeded TUI fuzzer. A failing run prints its seed.

    python3 tests/chaos/invariants.py --bin target/debug/chanto [--tier pr|nightly]
        [--only NAME] [--seed N]
"""
import argparse
import json
import os
from pathlib import Path
import random
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time

HERE = Path(__file__).resolve().parent
SCENARIOS = HERE / "scenarios"
WINDOWS = sys.platform == "win32"
CANARY = "sk-chaos-canary-7f3a91c2d4e5b6a8"
OUTSIDE_TEXT = "outside the project\n"
LIMIT_S = 90
# Each case's first request is the chaos; `stall` needs the idle timeout short.
BASE_CONFIG = "stream_idle_timeout = 5\n[feedback]\nshare_errors = false\n"

STREAM_PR = (
    "truncated", "truncated_args", "eof_mid_args", "eof_mid_text", "invalid_args", "args_array",
    "args_null", "unknown_tool", "empty_name", "html_body", "empty_body", "only_done", "error_event",
    "reset", "stall", "invalid_utf8", "unicode", "ansi_title", "empty", "many_calls", "dup_ids",
)
STREAM_NIGHTLY = ("length_args", "length_text", "args_wrong_types", "huge_index", "drip", "huge_text",
                  "huge_line", "reasoning_only")
BASH_PR = ("exit_code", "cd_away", "stdin_read", "ansi", "child_daemon", "longline")
BASH_NIGHTLY = ("flood", "binary", "stderr_flood", "env_huge", "forever", "tty", "bg_many")
WEIRD = ("dir", "loop", "glob_loop", "grep_loop", "ls_loop", "fifo", "devzero")


class Failure(Exception):
    pass


class Run:
    """A throwaway home and project, and a fake model that dumps what it's sent."""

    def __init__(self, name, config="", project_name="proj"):
        self.name = name
        self.root = Path(tempfile.mkdtemp(prefix="chaos-")).resolve()
        self.home = self.root / "home"
        self.chanto_home = self.root / "hh"
        self.project = self.root / project_name
        # Not under the temp directory: the sandbox lets commands write there.
        self.outside = Path(tempfile.mkdtemp(prefix=".chanto-chaos-outside-", dir=Path.home())).resolve()
        self.dumps = self.root / "req"
        for d in (self.home, self.chanto_home, self.project):
            d.mkdir(parents=True)
        (self.outside / "keep.txt").write_text(OUTSIDE_TEXT)
        (self.chanto_home / "config.toml").write_text(BASE_CONFIG + config)
        (self.project / "a.txt").write_text("hello\n")
        (self.project / ".env").write_text(f"API_KEY={CANARY}\n")
        git = subprocess.run(["git", "init", "-q"], cwd=self.project, capture_output=True)
        if git.returncode:
            raise Failure(f"git init failed: {git.stderr[:200]}")
        self.mock = None
        self.env = {}

    def start_model(self, scenario, case=None):
        # UTF-8 on Windows too: scenario files hold emoji.
        env = dict(os.environ, FAKE_DUMP=str(self.dumps), PYTHONUTF8="1")
        if case:
            env["CASE"] = case
        self.mock = subprocess.Popen(
            [sys.executable, str(HERE / "fakemodel.py"), str(scenario), "--log", str(self.root / "model.log")],
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, env=env,
        )
        deadline = time.time() + 15
        port = None
        while time.time() < deadline and not port:
            line = self.mock.stdout.readline()
            if line.startswith("PORT"):
                port = line.split()[1]
        if not port:
            raise Failure("the fake model didn't start")
        self.env = {k: os.environ[k] for k in (
            "PATH", "LANG", "LC_ALL", "SYSTEMROOT", "SYSTEMDRIVE", "WINDIR", "TEMP", "TMP", "PATHEXT",
            "APPDATA", "LOCALAPPDATA", "ProgramFiles", "ProgramW6432", "ProgramFiles(x86)", "COMSPEC",
        ) if k in os.environ}
        self.env.update(
            HOME=str(self.home), USERPROFILE=str(self.home), CHANTO_HOME=str(self.chanto_home),
            CHANTO_COMPAT_HOME=str(self.home / "compat"), CHANTO_PROVIDER="openai",
            CHANTO_BASE_URL=f"http://127.0.0.1:{port}/v1", CHANTO_MODEL="fake", CHANTO_API_KEY="fake",
            CHANTO_NO_UPDATE_CHECK="1", CHANTO_NO_CLAUDE_CONFIG="1", CHANTO_NO_CODEX_CONFIG="1",
            CHANTO_NO_PRESENCE="1", CI="1", XDG_CONFIG_HOME=str(self.home / ".config"),
        )

    def stop_model(self):
        if self.mock:
            self.mock.kill()
            self.mock.wait()
            self.mock.stdout.close()
            self.mock = None

    def spawn(self, binary, args):
        options = {} if WINDOWS else {"start_new_session": True}
        return subprocess.Popen(
            [binary, *args], cwd=self.project, env=self.env, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, **options,
        )

    def requests(self):
        """Every request body the model got, as text."""
        if not self.dumps.is_dir():
            return []
        return [p.read_text(errors="replace") for p in sorted(self.dumps.iterdir())]

    def close(self):
        self.stop_model()
        for path, dirs, _ in os.walk(self.root):
            for d in dirs:  # A read-only case's folders must be removable.
                try:
                    os.chmod(os.path.join(path, d), stat.S_IRWXU)
                except OSError:
                    pass
        shutil.rmtree(self.root, ignore_errors=True)
        shutil.rmtree(self.outside, ignore_errors=True)


def run_to_end(run, binary, args, limit=LIMIT_S):
    """(output, exit code); a run that overstays is the `no hang` failure."""
    p = run.spawn(binary, args)
    try:
        out, _ = p.communicate(timeout=limit)
    except subprocess.TimeoutExpired:
        kill_group(p)
        p.communicate()
        raise Failure(f"no hang: still running after {limit} s") from None
    check_orphans(p)
    return out.decode(errors="replace"), p.returncode


def kill_group(p):
    if WINDOWS:
        p.kill()
    else:
        try:
            os.killpg(p.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def group_alive(pgid):
    """Live members of a process group, zombies aside."""
    ps = subprocess.run(["ps", "-A", "-o", "pgid=,stat=,command="], capture_output=True, text=True).stdout
    alive = []
    for line in ps.splitlines():
        parts = line.split(None, 2)
        if len(parts) == 3 and parts[0] == str(pgid) and not parts[1].startswith("Z"):
            alive.append(parts[2][:80])
    return alive


def check_orphans(p, grace=4.0):
    if WINDOWS:
        return
    deadline = time.time() + grace
    while True:
        alive = group_alive(p.pid)
        if not alive:
            return
        if time.time() > deadline:
            kill_group(p)
            raise Failure(f"no orphans: left running: {alive[:3]}")
        time.sleep(0.1)


def check_common(run, output):
    logs = run.chanto_home / "logs"
    text = output
    if logs.is_dir():
        text += "".join(p.read_text(errors="replace") for p in logs.iterdir() if p.is_file())
    if "panicked at" in text:
        at = text.index("panicked at")
        raise Failure(f"no panic: {text[max(0, at - 80):at + 200]!r}")
    sessions = run.chanto_home / "sessions"
    if sessions.is_dir():
        for path in sessions.rglob("*"):
            if path.name.endswith(".tmp"):
                raise Failure(f"sessions parse: temp file left: {path.name}")
            if path.suffix == ".json":
                try:
                    json.loads(path.read_text())
                except ValueError as e:
                    raise Failure(f"sessions parse: {path.name}: {e}") from None
    if sorted(p.name for p in run.outside.iterdir()) != ["keep.txt"] or (
        (run.outside / "keep.txt").read_text() != OUTSIDE_TEXT
    ):
        raise Failure("contained: a file outside the project changed")
    for body in run.requests():
        if CANARY in body:
            raise Failure("no leak: the planted secret reached the model")


KEEP_SESSION = ["-p", "go", "--mode", "bypassPermissions"]


def chaos_case(name, scenario, case=None, args=KEEP_SESSION, config="", setup=None, also=None, limit=LIMIT_S):
    def body(binary, _seed):
        run = Run(name, config)
        try:
            if setup:
                setup(run)
            run.start_model(scenario, case)
            output, code = run_to_end(run, binary, args, limit)
            run.stop_model()
            check_common(run, output)
            if code < 0:
                raise Failure(f"no panic: killed by signal {-code}")
            if also:
                also(run, output, code)
        finally:
            run.close()

    return name, body


def weird_setup(run):
    subprocess.run(["bash", str(HERE / "weird_files_setup.sh")], cwd=run.project, check=True)


def scene(run, name, text):
    path = run.root / name
    path.write_text(text)
    return path


def tool_turns(*calls, final="done."):
    return [{"tool_calls": [c]} for c in calls] + [{"text": final}]


def json_scene(name, turns):
    """A scenario file that lasts the session; written once per process."""
    path = Path(tempfile.gettempdir()) / f"chanto-chaos-{os.getpid()}-{name}.json"
    path.write_text(json.dumps(turns))
    return path


def secret_cases():
    read_env = json_scene("read-env", tool_turns(
        {"name": "Read", "args": {"file_path": ".env"}},
        {"name": "Bash", "args": {"command": "cat .env", "description": "t"}},
        {"name": "Grep", "args": {"pattern": "API_KEY"}},
    ))

    def contained(name, args):
        def body(binary, _seed):
            run = Run(name)
            try:
                out = run.outside
                run.start_model(scene(run, "outside.json", json.dumps(tool_turns(
                    {"name": "Write", "args": {"file_path": str(out / "written.txt"), "content": "no\n"}},
                    {"name": "Edit", "args": {"file_path": str(out / "keep.txt"), "old_string": "outside", "new_string": "edited"}},
                    {"name": "Bash", "args": {"command": f"echo no > '{out / 'shell.txt'}'", "description": "t"}},
                    {"name": "Bash", "args": {"command": f"python3 -c \"open('{(out / 'py.txt').as_posix()}', 'w')\"", "description": "t"}},
                ))))
                output, _ = run_to_end(run, binary, args)
                run.stop_model()
                check_common(run, output)
            finally:
                run.close()

        return name, body

    def control_escape(binary, _seed):
        """With the sandbox off and approvals bypassed the write lands, so
        `contained` can fail; a pass here would mean it can't see anything."""
        run = Run("control:escape", "[sandbox]\nenabled = false\n")
        try:
            out = run.outside
            run.start_model(scene(run, "esc.json", json.dumps(tool_turns(
                {"name": "Bash", "args": {"command": f"echo no > '{out / 'shell.txt'}'", "description": "t"}},
            ))))
            output, _ = run_to_end(run, binary, KEEP_SESSION)
            run.stop_model()
            try:
                check_common(run, output)
            except Failure as e:
                if "contained" in str(e):
                    return
                raise
            raise Failure("control: an escaped write wasn't noticed")
        finally:
            run.close()

    def read_env_arrives(run, _output, _code):
        bodies = "".join(run.requests())
        if "API_KEY" not in bodies:
            raise Failure("control: .env never reached the model, so `no leak` tested nothing")

    return [
        ("control:escape-is-noticed", control_escape) if not WINDOWS else None,
        chaos_case("secrets:read-env", read_env, also=read_env_arrives),
        contained("contained:default-mode", ["-p", "go"]),
        contained("contained:accept-edits", ["-p", "go", "--mode", "acceptEdits"]),
        contained("contained:auto", ["-p", "go", "--mode", "auto"]),
    ]


def ran_token(token):
    def check(run, _output, _code):
        if not any(token in body for body in run.requests()[1:]):
            tail = run.requests()[-1][-300:] if run.requests() else "no requests"
            raise Failure(f"the command didn't run: {tail!r}")

    return check


def environment_cases():
    """The setups a clean runner never sees: each must run a command, or fail clearly."""
    token = "chaos-ran-4417"
    echo = json_scene("echo", tool_turns({"name": "Bash", "args": {"command": f"echo {token}", "description": "t"}}))
    runs_it = ran_token(token)

    def broken_git(run):
        stub = run.root / "stub"
        stub.mkdir()
        if WINDOWS:
            (stub / "git.cmd").write_text("@exit /b 127\r\n")
        else:
            (stub / "git").write_text("#!/bin/sh\nexit 127\n")
            (stub / "git").chmod(0o755)
        run.stub = stub

    cases = [
        chaos_case("env:default", echo, also=runs_it),
        chaos_case("env:sandbox-off", echo, config="[sandbox]\nenabled = false\n", also=runs_it),
    ]

    def odd_path(binary, _seed):
        run = Run("env:odd-path", project_name="prójéct with spaces 日本")
        try:
            run.start_model(echo)
            output, _ = run_to_end(run, binary, KEEP_SESSION)
            run.stop_model()
            check_common(run, output)
            runs_it(run, output, 0)
        finally:
            run.close()

    def no_git(binary, _seed):
        run = Run("env:broken-git")
        try:
            broken_git(run)
            run.start_model(echo)
            run.env["PATH"] = str(run.stub) + os.pathsep + run.env["PATH"]
            output, _ = run_to_end(run, binary, KEEP_SESSION)
            run.stop_model()
            check_common(run, output)
            runs_it(run, output, 0)
        finally:
            run.close()

    cases += [("env:odd-path", odd_path), ("env:broken-git", no_git)]
    if WINDOWS:
        for shell in ("powershell.exe", "cmd.exe"):
            def forced(binary, _seed, shell=shell):
                run = Run(f"env:{shell}")
                try:
                    run.start_model(echo)
                    run.env["CHANTO_SHELL"] = shell
                    output, _ = run_to_end(run, binary, KEEP_SESSION)
                    run.stop_model()
                    check_common(run, output)
                    runs_it(run, output, 0)
                finally:
                    run.close()

            cases.append((f"env:{shell}", forced))
    else:
        def read_only(which):
            def body(binary, _seed):
                run = Run(f"env:read-only-{which}")
                try:
                    run.start_model(echo)
                    target = run.chanto_home if which == "home" else run.project
                    target.chmod(0o555)
                    output, _ = run_to_end(run, binary, KEEP_SESSION)
                    run.stop_model()
                    target.chmod(0o755)
                    check_common(run, output)
                finally:
                    run.close()

            return body

        cases += [("env:read-only-home", read_only("home")), ("env:read-only-project", read_only("project"))]
    return cases


def crash_case(index, seed):
    """Killed outright mid-turn: nothing half-written, and the session resumes."""
    def body(binary, base_seed):
        rng = random.Random((seed if base_seed is None else base_seed) + index)
        run = Run(f"crash:{index}")
        try:
            content = "complete line\n" * 20000
            turns = []
            for i in range(30):
                turns.append({"tool_calls": [{"name": "Write", "args": {"file_path": f"step{i}.txt", "content": content}}]})
            turns.append({"text": "done."})
            scenario = scene(run, "crash.json", json.dumps(turns))
            run.start_model(scenario)
            p = run.spawn(binary, KEEP_SESSION)
            time.sleep(rng.uniform(0.02, 0.5))
            kill_group(p)
            output = p.communicate()[0].decode(errors="replace")
            run.stop_model()
            for path in run.project.glob("step*.txt"):
                if path.read_text() != content:
                    raise Failure(f"no half-written files: {path.name} is {path.stat().st_size} bytes")
            logs_ok = run.chanto_home / "sessions"
            if logs_ok.is_dir():
                for path in logs_ok.rglob("*.json"):
                    try:
                        json.loads(path.read_text())
                    except ValueError as e:
                        raise Failure(f"sessions parse after kill: {e}") from None
            hello = scene(run, "again.json", json.dumps([{"text": "still here"}]))
            run.dumps = run.root / "req2"
            run.start_model(hello)
            again, code = run_to_end(run, binary, ["--continue", "-p", "next", "--mode", "bypassPermissions"])
            run.stop_model()
            if code != 0:
                raise Failure(f"resume after kill exited {code}: {again[-300:]!r}")
            if not run.requests():
                raise Failure("resume after kill never reached the model")
            check_common(run, output + again)
        finally:
            run.close()

    return f"crash:kill-{index}", body


def tui_fuzz_case(index):
    """Random keys, pastes and resizes; then it must exit and restore the terminal."""
    def body(binary, base_seed):
        import fcntl
        import pty
        import select
        import struct
        import termios

        seed = (base_seed if base_seed is not None else 1000) + index
        rng = random.Random(seed)
        run = Run(f"tui:{seed}")
        try:
            turns = {"turns": [], "default": {"text": "more text with `code`\n" * 30, "chunk": 8}}
            for i in range(50):
                turns["turns"].append({"tool_calls": [{"name": "Bash", "args": {"command": f"echo step{i}", "description": "t"}}]})
                turns["turns"].append({"text": f"reply {i}\n" * 20, "chunk": 12})
            run.start_model(scene(run, "tui.json", json.dumps(turns)))
            env = dict(run.env, TERM="xterm-256color")
            env.pop("CI", None)
            pid, fd = pty.fork()
            if pid == 0:
                os.chdir(run.project)
                os.execve(binary, [binary], env)

            def size(rows, cols):
                fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

            size(40, 120)
            out = bytearray()

            def drain(seconds):
                end = time.time() + seconds
                while time.time() < end:
                    if select.select([fd], [], [], 0.01)[0]:
                        try:
                            data = os.read(fd, 65536)
                        except OSError:
                            return False
                        if not data:
                            return False
                        out.extend(data)
                return True

            drain(1.5)
            os.write(fd, b"n\r")
            drain(1.0)
            keys = [b"\x1b[A", b"\x1b[B", b"\x1b[C", b"\x1b[D", b"\x1b[5~", b"\x1b[6~", b"\t", b"\x7f", b"\x1b",
                    b"\x0f", b"\x12", b"\x15", b"\x0b", b"\x01", b"\x05", b"y", b"n", b"1", b"2"]
            words = [b"hello ", b"read a.txt ", "日本語 ".encode(), "🙂 ".encode(), b"@a", b"/", b"/help", b"/status",
                     b"/model", b"/context", b"/cost", b"/mode", b"/resume", b"/sandbox"]
            actions = ["key"] * 6 + ["word"] * 3 + ["enter"] * 2 + ["paste", "resize", "interrupt", "wait"]
            log = []
            for _ in range(rng.randint(150, 250)):
                action = rng.choice(actions)
                log.append(action)
                if action == "key":
                    os.write(fd, rng.choice(keys))
                elif action == "word":
                    os.write(fd, rng.choice(words))
                elif action == "enter":
                    os.write(fd, b"\r")
                elif action == "paste":
                    size_bytes = rng.choice([50, 5000, 200000])
                    os.write(fd, b"\x1b[200~" + (b"x" * (size_bytes // 2) + b"\n" + b"y" * (size_bytes // 2))[:60000] + b"\x1b[201~")
                elif action == "resize":
                    size(rng.randint(5, 60), rng.randint(10, 200))
                elif action == "interrupt":
                    os.write(fd, b"\x03")
                    drain(0.05)
                else:
                    drain(rng.uniform(0.05, 0.5))
                if not drain(rng.uniform(0.0, 0.03)):
                    break
                if b"panicked at" in out:
                    break
            exited = False
            size(40, 120)
            for _ in range(6):
                try:
                    os.write(fd, b"\x1b\x1b\x15/exit\r")
                except OSError:
                    exited = True
                    break
                drain(1.0)
                done, _ = os.waitpid(pid, os.WNOHANG)
                if done:
                    exited = True
                    break
                try:
                    os.write(fd, b"\x03\x03")
                except OSError:
                    pass
                drain(1.0)
                done, _ = os.waitpid(pid, os.WNOHANG)
                if done:
                    exited = True
                    break
            if not exited:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
                raise Failure(f"no hang: wouldn't exit (seed {seed}, last actions {log[-8:]})")
            drain(0.3)
            os.close(fd)
            run.stop_model()
            text = bytes(out)
            if b"\x1b[?1049h" in text and b"\x1b[?1049l" not in text:
                raise Failure(f"terminal restored: never left the alternate screen (seed {seed})")
            try:
                check_common(run, text.decode(errors="replace"))
            except Failure as e:
                raise Failure(f"{e} (seed {seed}, last actions {log[-8:]})") from None
        finally:
            run.close()

    return f"tui:fuzz-{index}", body


def all_cases(tier):
    stream = SCENARIOS / "stream.py"
    bash = SCENARIOS / "bash_chaos.py"
    weird = SCENARIOS / "weird_files.py"
    cases = [chaos_case(f"stream:{c}", stream, c) for c in STREAM_PR]
    cases += [c for c in secret_cases() if c] + environment_cases()
    cases += [crash_case(i, 7) for i in range(3)]
    if not WINDOWS:
        cases += [chaos_case(f"bash:{c}", bash, c) for c in BASH_PR]
        cases += [chaos_case(f"weird:{c}", weird, c, setup=weird_setup) for c in WEIRD]
        cases.append(tui_fuzz_case(0))
    if tier == "nightly":
        cases += [chaos_case(f"stream:{c}", stream, c) for c in STREAM_NIGHTLY]
        cases += [crash_case(i, 7) for i in range(3, 15)]
        if not WINDOWS:
            cases += [chaos_case(f"bash:{c}", bash, c) for c in BASH_NIGHTLY]
            cases += [tui_fuzz_case(i) for i in range(1, 12)]
    return cases


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--bin", required=True)
    parser.add_argument("--tier", choices=("pr", "nightly"), default="pr")
    parser.add_argument("--only", help="run the cases whose name contains this")
    parser.add_argument("--seed", type=int, help="the base seed of the crash and TUI cases")
    args = parser.parse_args()
    binary = str(Path(args.bin).resolve())
    failed = []
    cases = [c for c in all_cases(args.tier) if not args.only or args.only in c[0]]
    for name, body in cases:
        start = time.time()
        try:
            body(binary, args.seed)
            print(f"PASS {name} ({time.time() - start:.1f}s)", flush=True)
        except Failure as e:
            print(f"FAIL {name} ({time.time() - start:.1f}s): {e}", flush=True)
            failed.append(name)
    print(f"{len(cases) - len(failed)} of {len(cases)} passed", flush=True)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as f:
            f.write(f"### Chaos ({args.tier}): {len(cases) - len(failed)} of {len(cases)} passed\n")
            f.writelines(f"- FAIL `{n}`\n" for n in failed)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
