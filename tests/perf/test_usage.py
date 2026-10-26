"""Tests for how the performance gate measures a child process.

These run on whichever platform is measuring, so the Windows path (which has
no `wait4`) is exercised by the Windows runner in CI.

    python3 -m unittest discover -s tests/perf -p 'test_*.py'
"""
import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("perf_gate", HERE / "gate.py")
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


def run(code):
    home = Path(tempfile.mkdtemp(prefix="chanto-usage-"))
    return gate.wait_with_usage(sys.executable, ["-c", code], gate.child_env(home, 1))


class UsageTest(unittest.TestCase):
    def test_a_finished_child_reports_cpu_time_and_peak_memory(self):
        usage, code = run("sum(range(200000))")
        self.assertEqual(code, 0)
        # A Python start-up is milliseconds of CPU and megabytes of memory;
        # the point is that the kernel reported something, not the value.
        self.assertGreater(usage.cpu_ms, 0)
        self.assertGreater(usage.rss_mb, 1)
        self.assertLess(usage.rss_mb, 4096)

    def test_more_work_costs_more_cpu(self):
        idle, _ = run("pass")
        busy, _ = run("sum(range(4000000))")
        self.assertGreater(busy.cpu_ms, idle.cpu_ms)

    def test_the_exit_code_is_reported(self):
        _, code = run("raise SystemExit(3)")
        self.assertEqual(code, 3)


class PlatformTest(unittest.TestCase):
    def test_the_config_is_read_for_this_platform(self):
        cfg = gate.load_config(gate.CONFIG)
        self.assertEqual(cfg["platform"], gate.PLATFORM)
        self.assertEqual(cfg["ceilings"], cfg["budgets"][gate.PLATFORM])

    def test_windows_has_its_own_ceilings(self):
        self.assertIn("windows", gate.load_config(gate.CONFIG)["budgets"])


if __name__ == "__main__":
    unittest.main()
