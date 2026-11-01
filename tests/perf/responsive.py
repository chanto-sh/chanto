#!/usr/bin/env python3
"""What timing per request can't see: an idle app, typing, and shell commands.

  idle     (Unix) the TUI at its prompt, fresh and after a 20,000-line reply:
           it must draw nothing, and stay under a CPU ceiling.
  typing   (Unix) bytes drawn per key, which a full redraw would multiply, and
           the time to the first output after a key and after PageUp.
  paste    (Unix) bytes drawn for a 1 MB paste, of many lines and of one long
           line: it collapses to a tag, so it must not grow with the paste.
  shell    40 turns that each run one command: processes started per command
           (Windows, from its job accounting) and CPU per turn, against ceilings.

Windows has no pseudo-terminal here, so it gets only `shell`. Ceilings are under
`responsive` in tests/perf/budgets.json.

    python3 tests/perf/responsive.py --bin target/release/chanto
"""
import argparse
import json
import os
from pathlib import Path
import re
import select
import statistics
import subprocess
import sys
import tempfile
import threading
import time

sys.path.insert(0, str(Path(__file__).parent))
import gate  # noqa: E402

BUDGETS = Path(__file__).with_name("budgets.json")
WINDOWS = sys.platform == "win32"
IDLE_S = 30
TURNS = 40


def cpu_ms(pid):
    """CPU a live process has used so far."""
    if sys.platform == "linux":
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return (int(fields[11]) + int(fields[12])) * 10.0
    out = subprocess.run(
        ["ps", "-o", "cputime=", "-p", str(pid)], capture_output=True, text=True
    ).stdout.strip()
    total = 0.0
    for part in out.replace("-", ":").split(":"):
        total = total * 60 + float(part)
    return total * 1000


class Tui:
    """The app in a pseudo-terminal, with its output counted as it arrives."""

    def __init__(self, binary, scenario, work):
        import fcntl
        import pty
        import struct
        import termios

        home, cwd = work / "home", work / "project"
        home.mkdir()
        cwd.mkdir()
        self.mock, port = gate.mock_start(scenario, work / "tui.log")
        env = dict(gate.child_env(home, port), TERM="xterm-256color", CHANTO_TIMING="1")
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(cwd)
            os.execve(binary, [binary], env)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        self.out = bytearray()
        self.drawn = 0
        self.last = time.perf_counter()
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        while True:
            try:
                if select.select([self.fd], [], [], 0.002)[0]:
                    data = os.read(self.fd, 65536)
                    if not data:
                        return
                    self.out += data
                    self.drawn += len(data)
                    self.last = time.perf_counter()
            except OSError:
                return

    def seen(self, pattern):
        return re.search(pattern, re.sub(rb"\x1b\[[0-9;?]*[a-zA-Z]", b"", bytes(self.out)))

    def wait(self, pattern, seconds):
        deadline = time.time() + seconds
        while time.time() < deadline:
            if self.seen(pattern):
                return True
            time.sleep(0.01)
        return False

    def start(self):
        """Past the first run's question, to a settled prompt."""
        if self.wait(rb"Send anonymous error reports|setup done", 20) and self.seen(rb"Send anonymous"):
            time.sleep(0.3)
            os.write(self.fd, b"n\r")
        if not self.wait(rb"setup done", 20):
            raise gate.GateError("the TUI never reached its first screen")
        time.sleep(3)

    def send(self, text):
        os.write(self.fd, text)
        time.sleep(0.3)
        os.write(self.fd, b"\r")

    def settle(self, seconds=1.5, limit=120):
        start = time.time()
        while time.perf_counter() - self.last < seconds and time.time() - start < limit:
            time.sleep(0.05)
        time.sleep(seconds)

    def idle(self):
        """(percent of a core, bytes drawn) over the idle period."""
        cpu, drawn = cpu_ms(self.pid), self.drawn
        time.sleep(IDLE_S)
        return (cpu_ms(self.pid) - cpu) / IDLE_S / 10, self.drawn - drawn

    def key(self, data):
        """(ms to the first output after a key, bytes it drew)."""
        drawn = self.drawn
        start = time.perf_counter()
        os.write(self.fd, data)
        while self.drawn == drawn and time.perf_counter() - start < 2:
            time.sleep(0.0002)
        first = (self.last - start) * 1000 if self.drawn != drawn else None
        time.sleep(0.08)
        return first, self.drawn - drawn

    def typing(self, keys=30):
        """(median ms to output, median bytes per key)."""
        rows = [self.key(b"a") for _ in range(keys)]
        for _ in range(keys):
            self.key(b"\x7f")
        if any(first is None for first, _ in rows):
            raise gate.GateError("a key drew nothing")
        return statistics.median(r[0] for r in rows), statistics.median(r[1] for r in rows)

    def paste(self, text):
        """(bytes drawn until it settles, ms to the first draw) for a bracketed paste."""
        drawn = self.drawn
        start = time.perf_counter()
        data = b"\x1b[200~" + text.encode() + b"\x1b[201~"
        sent = 0
        while sent < len(data):
            sent += os.write(self.fd, data[sent:sent + 65536])
        while self.drawn == drawn and time.perf_counter() - start < 30:
            time.sleep(0.001)
        first = (self.last - start) * 1000
        self.settle(0.4, 30)
        return self.drawn - drawn, first

    def close(self):
        import signal

        os.kill(self.pid, signal.SIGKILL)
        os.waitpid(self.pid, 0)
        self.mock.kill()
        self.mock.wait()


def tui_scenario(work):
    long_reply = "\n".join(f"line {i} of a long reply with some `code` and **bold** words" for i in range(20000))
    scenario = work / "tui.json"
    scenario.write_text(json.dumps([{"text": long_reply, "chunk": 4000}]))
    return scenario


def paste_metrics(binary, work):
    tui = Tui(binary, tui_scenario(work), work)
    try:
        tui.start()
        size = 1024 * 1024
        line = "the quick brown fox jumps over the lazy dog, again and again\n"
        out = {}
        out["paste_lines_bytes"], out["paste_lines_ms"] = tui.paste((line * (size // len(line) + 1))[:size])
        out["paste_one_line_bytes"], out["paste_one_line_ms"] = tui.paste("x" * size)
        return out
    finally:
        tui.close()


def tui_metrics(binary, work):
    tui = Tui(binary, tui_scenario(work), work)
    try:
        tui.start()
        out = {}
        out["idle_percent"], out["idle_bytes"] = tui.idle()
        out["typing_ms"], out["typing_bytes"] = tui.typing()
        tui.send(b"go")
        if not tui.wait(rb"1999\d", 90):
            raise gate.GateError("the 20,000-line reply never reached the screen")
        tui.settle()
        out["idle_after_long_percent"], out["idle_after_long_bytes"] = tui.idle()
        out["typing_after_long_ms"], out["typing_after_long_bytes"] = tui.typing()
        pages = [tui.key(b"\x1b[5~")[0] for _ in range(15)]
        out["page_up_ms"] = statistics.median(p for p in pages if p is not None)
        return out
    finally:
        tui.close()


def shell_scenario(work):
    """A different command each turn: repeating one stops at the loop check."""
    scenario = work / "shell.py"
    scenario.write_text(
        "def respond(body, n):\n"
        f"    if n < {TURNS}:\n"
        "        return {'tool_calls': [{'name': 'Bash', 'args': {'command': f'echo hi{n}', 'description': 't'}}],\n"
        "                'finish_reason': 'tool_calls'}\n"
        "    return {'text': 'done.'}\n"
    )
    return scenario


def job_usage(binary, args, env):
    """(CPU ms, processes) of everything a run started, from a job object.

    A job object adds up every process ever in it, exited ones included, and
    the child is created suspended so nothing starts before it's assigned.
    """
    import ctypes
    from ctypes import wintypes

    class Accounting(ctypes.Structure):
        _fields_ = [
            ("user", ctypes.c_int64), ("kernel", ctypes.c_int64),
            ("period_user", ctypes.c_int64), ("period_kernel", ctypes.c_int64),
            ("page_faults", wintypes.DWORD), ("processes", wintypes.DWORD),
            ("active", wintypes.DWORD), ("terminated", wintypes.DWORD),
        ]

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    ntdll = ctypes.WinDLL("ntdll")
    kernel32.CreateJobObjectW.restype = wintypes.HANDLE
    kernel32.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
    kernel32.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
    kernel32.QueryInformationJobObject.argtypes = [
        wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD, ctypes.POINTER(wintypes.DWORD),
    ]
    ntdll.NtResumeProcess.argtypes = [wintypes.HANDLE]
    job = kernel32.CreateJobObjectW(None, None)
    p = subprocess.Popen(
        [binary, *args], env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL, creationflags=0x00000004,
    )
    if not kernel32.AssignProcessToJobObject(job, int(p._handle)):
        p.kill()
        raise gate.GateError("could not put the run in a job object")
    ntdll.NtResumeProcess(int(p._handle))
    code = p.wait(300)
    info = Accounting()
    kernel32.QueryInformationJobObject(job, 1, ctypes.byref(info), ctypes.sizeof(info), None)
    if code != 0:
        raise gate.GateError(f"{binary} exited {code} running shell commands")
    return (info.user + info.kernel) / 1e4, info.processes


def unix_usage(binary, args, env):
    p = subprocess.Popen(
        [binary, *args], env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )
    _, status, usage = os.wait4(p.pid, 0)
    p.returncode = os.waitstatus_to_exitcode(status)
    if p.returncode != 0:
        raise gate.GateError(f"{binary} exited {p.returncode} running shell commands")
    return (usage.ru_utime + usage.ru_stime) * 1000, None


def shell_metrics(binary, work, reps=5):
    home, project = work / "home", work / "project"
    home.mkdir()
    project.mkdir()
    scenario = shell_scenario(work)
    rows = []
    for _ in range(reps):
        log = work / "shell.log"
        log.write_text("")
        mock, port = gate.mock_start(scenario, log)
        try:
            usage = job_usage if WINDOWS else unix_usage
            rows.append(usage(
                binary, ["-p", "--mode", "bypassPermissions", "go", "--cwd", str(project)], gate.child_env(home, port)
            ))
        finally:
            mock.kill()
            mock.wait()
        made = gate.chat_requests(log)
        if made != TURNS + 1:
            raise gate.GateError(f"the shell run made {made} of {TURNS + 1} requests")
    out = {"shell_cpu_ms": statistics.median(r[0] for r in rows) / TURNS}
    if WINDOWS:
        out["shell_processes"] = statistics.median(r[1] for r in rows) / TURNS
    return out


def failures(metrics, ceilings):
    """The metrics over their ceilings. A metric with no ceiling only reports."""
    return [m for m, v in metrics.items() if m in ceilings and v > ceilings[m]]


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--bin", required=True)
    args = parser.parse_args()
    binary = str(Path(args.bin).resolve())
    cfg = json.loads(BUDGETS.read_text())["responsive"]
    ceilings = cfg[gate.PLATFORM]
    metrics = {}
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp).resolve()
        (tmp / "shell").mkdir()
        metrics.update(shell_metrics(binary, tmp / "shell"))
        if not WINDOWS:
            (tmp / "tui").mkdir()
            metrics.update(tui_metrics(binary, tmp / "tui"))
            (tmp / "paste").mkdir()
            metrics.update(paste_metrics(binary, tmp / "paste"))
    failed = failures(metrics, ceilings)
    lines = ["### Responsiveness", "| metric | value | ceiling | |", "|---|--:|--:|---|"]
    for name, value in metrics.items():
        lines.append(f"| {name} | {value:.2f} | {ceilings.get(name, '—')} | {'fail' if name in failed else 'pass'} |")
    report = "\n".join(lines)
    print(report)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as f:
            f.write(report + "\n")
    return 1 if failed else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except gate.GateError as e:
        print(f"::error::{e}")
        sys.exit(2)
