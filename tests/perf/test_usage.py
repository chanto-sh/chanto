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
        # Spin for 50 ms of the child's own CPU time: Windows counts CPU in
        # 15.6 ms ticks, and a fixed sum on a fast runner could finish, start-up
        # included, inside one tick and read as 0.
        usage, code = run(
            "import time\n"
            "end = time.process_time() + 0.05\n"
            "while time.process_time() < end:\n"
            "    pass"
        )
        self.assertEqual(code, 0)
        # The point is that the kernel reported something, not the value.
        self.assertGreater(usage.cpu_ms, 0)
        self.assertGreater(usage.rss_mb, 1)
        self.assertLess(usage.rss_mb, 4096)

    def test_wall_time_is_measured_too(self):
        usage, _ = run("pass")
        self.assertGreater(usage.wall_ms, 0)
        # The kernel's CPU time cannot exceed the wall clock the run took.
        self.assertLessEqual(usage.cpu_ms, usage.wall_ms + 100)

    def test_more_work_costs_more_cpu(self):
        idle, _ = run("pass")
        # Spin for 300 ms of the child's own CPU time rather than a fixed
        # amount of work: Windows counts CPU in 15.6 ms ticks, and on a fast
        # runner a fixed sum took as many ticks as starting Python did.
        busy, _ = run(
            "import time\n"
            "end = time.process_time() + 0.3\n"
            "while time.process_time() < end:\n"
            "    pass"
        )
        self.assertGreater(busy.cpu_ms, idle.cpu_ms + 150)

    def test_the_exit_code_is_reported(self):
        _, code = run("raise SystemExit(3)")
        self.assertEqual(code, 3)


@unittest.skipUnless(sys.platform.startswith("linux"), "taskset is Linux-only")
class PinnedTest(unittest.TestCase):
    def test_one_core_is_a_single_cpu(self):
        _, lead = gate.pinned("/bin/true", 1)
        self.assertEqual(lead, ["-c", "0", "/bin/true"])

    def test_more_cores_are_a_range(self):
        _, lead = gate.pinned("/bin/true", 2)
        self.assertEqual(lead, ["-c", "0,1", "/bin/true"])

    def test_a_pinned_child_still_reports_its_own_usage(self):
        exe, lead = gate.pinned(sys.executable, 1)
        home = Path(tempfile.mkdtemp(prefix="chanto-pinned-"))
        usage, code = gate.wait_with_usage(
            exe, lead + ["-c", "sum(range(200000))"], gate.child_env(home, 1)
        )
        # `taskset` runs the binary in place, so this is the child's own CPU
        # time and not a wrapper's.
        self.assertEqual(code, 0)
        self.assertGreater(usage.cpu_ms, 0)
        self.assertGreater(usage.wall_ms, 0)


class PlatformTest(unittest.TestCase):
    def test_the_config_is_read_for_this_platform(self):
        cfg = gate.load_config(gate.CONFIG)
        self.assertEqual(cfg["platform"], gate.PLATFORM)
        self.assertEqual(cfg["ceilings"], cfg["budgets"][gate.PLATFORM])

    def test_windows_has_its_own_ceilings(self):
        self.assertIn("windows", gate.load_config(gate.CONFIG)["budgets"])

    def test_the_device_ceilings_are_usable(self):
        device = gate.load_config(gate.CONFIG)["device"]
        self.assertGreaterEqual(device["cpus"], 1)
        self.assertGreater(device["reps"], 0)
        for key in ("startup_wall_ms", "turn_wall_ms", "rss_mb"):
            self.assertGreater(device[key], 0, key)
        # A phone-sized budget, not a desktop's.
        self.assertLessEqual(device["rss_mb"], 1024)


class DeviceReportTest(unittest.TestCase):
    def report(self, verdict):
        metric = {
            "head": 10.0, "base": None, "change": None, "budget": 100.0,
            "p_value": None, "significant": False, "over_budget": verdict == "fail",
            "too_big": False, "near_budget": False, "verdict": verdict,
        }
        return {
            "platform": "linux",
            "base": None,
            "head": "/tmp/chanto",
            "workload": {"turns": 40, "blob_kb": 40},
            "reps": {"startup": 1, "turns": 1},
            "metrics": {"startup_cpu_ms": dict(metric)},
            "device": {
                "cpus": 1,
                "reps": 5,
                "metrics": {"device_turn_wall_ms": metric},
            },
            "ok": verdict == "pass",
        }

    def test_a_failing_device_ceiling_is_reported(self):
        text = gate.markdown(self.report("fail"))
        self.assertIn("The small device could not keep up", text)
        self.assertIn("Per request (ms)", text)

    def test_a_passing_device_section_is_reported(self):
        text = gate.markdown(self.report("pass"))
        self.assertIn("### Small device", text)
        self.assertNotIn("could not keep up", text)

    def test_a_report_without_a_device_section_has_none(self):
        report = self.report("pass")
        del report["device"]
        self.assertNotIn("### Small device", gate.markdown(report))


if __name__ == "__main__":
    unittest.main()
