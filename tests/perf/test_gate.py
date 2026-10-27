"""Tests for the performance gate's statistics and verdicts.

Run with the rest of the Python checks:

    python3 -m unittest discover -s tests/perf -p 'test_*.py'
"""
import importlib.util
import json
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("perf_gate", HERE / "gate.py")
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)

SAME = [10.0, 10.2, 9.8, 10.1, 9.9, 10.3, 9.7, 10.0, 10.1, 9.9]
SLOWER = [12.5, 12.7, 12.4, 12.6, 12.3, 12.8, 12.5, 12.2, 12.6, 12.4]


class SignificanceTest(unittest.TestCase):
    def test_a_separated_shift_is_significant(self):
        self.assertLess(gate.mann_whitney_greater(SAME, SLOWER), 0.01)

    def test_the_same_samples_are_not(self):
        self.assertGreater(gate.mann_whitney_greater(SAME, SAME), 0.5)

    def test_the_direction_matters(self):
        self.assertGreater(gate.mann_whitney_greater(SLOWER, SAME), 0.5)

    def test_a_faster_set_is_not_reported_as_slower(self):
        self.assertGreater(gate.mann_whitney_greater(SLOWER, SAME), 0.99)

    def test_ties_do_not_divide_by_zero(self):
        self.assertGreater(gate.mann_whitney_greater([5.0] * 8, [5.0] * 8), 0.5)

    def test_samples_too_small_to_test_are_not_significant(self):
        self.assertEqual(gate.mann_whitney_greater([10.0], [20.0]), 1.0)


class VerdictTest(unittest.TestCase):
    def judge(self, base, head, budget=100.0, tolerance=0.05, large_change=0.25):
        return gate.judge("startup_cpu_ms", base, head, budget, tolerance, 0.01, large_change)

    def test_a_change_inside_tolerance_passes(self):
        m = self.judge(SAME, [v * 1.02 for v in SAME])
        self.assertEqual(m["verdict"], "pass")

    def test_a_significant_small_regression_fails(self):
        m = self.judge(SAME, [v * 1.10 for v in SAME])
        self.assertEqual(m["verdict"], "fail")
        self.assertTrue(m["significant"])
        self.assertFalse(m["over_budget"])

    def test_over_the_ceiling_fails_even_without_a_base(self):
        m = self.judge(None, [500.0, 510.0])
        self.assertEqual(m["verdict"], "fail")
        self.assertTrue(m["over_budget"])
        self.assertIsNone(m["base"])

    def test_a_large_change_fails_without_significance(self):
        # Overlapping sample sets: the test cannot separate them, but a 30%
        # median jump is still a regression.
        base = [10.0, 10.1, 13.0, 12.9, 10.2, 12.8, 10.0, 13.0, 10.1, 12.9]
        head = [13.0, 13.1, 16.9, 16.8, 13.2, 16.7, 13.0, 16.9, 13.1, 16.8]
        m = self.judge(base, head)
        self.assertEqual(m["verdict"], "fail")
        self.assertTrue(m["too_big"])

    def test_a_faster_change_passes_and_is_reported(self):
        m = self.judge(SAME, [v * 0.9 for v in SAME])
        self.assertEqual(m["verdict"], "pass")
        self.assertLess(m["change"], -0.05)

    def test_noise_below_tolerance_gets_no_p_value(self):
        m = self.judge(SAME, [v * 1.02 for v in SAME])
        self.assertIsNone(m["p_value"])

    def test_peak_memory_uses_the_worst_run(self):
        m = gate.judge("startup_rss_mb", None, [20.0, 60.0, 21.0], 50.0, 0.1, 0.01, 0.25)
        self.assertEqual(m["head"], 60.0)
        self.assertEqual(m["verdict"], "fail")

    def test_cpu_uses_the_median_run(self):
        m = gate.judge("startup_cpu_ms", None, [10.0, 11.0, 40.0], 50.0, 0.1, 0.01, 0.25)
        self.assertEqual(m["head"], 11.0)
        self.assertEqual(m["verdict"], "pass")


class ClockStepTest(unittest.TestCase):
    def test_the_step_is_the_smallest_gap_between_values(self):
        self.assertEqual(gate.clock_step([15.625, 31.25, 46.875]), 15.625)
        self.assertAlmostEqual(gate.clock_step([10.0, 10.1, 13.0]), 0.1)

    def test_one_value_has_no_step(self):
        self.assertEqual(gate.clock_step([10.0]), 0.0)


class CoarseClockTest(unittest.TestCase):
    """Windows charges CPU in 15.625 ms ticks, so samples come in steps."""

    # The real Windows samples of a run where both sets overlapped.
    BASE = [78.125, 93.75, 125.0, 171.875, 93.75, 78.125, 125.0, 93.75,
            109.375, 125.0, 156.25, 125.0, 78.125, 156.25, 93.75, 78.125,
            109.375, 93.75, 93.75, 93.75]
    HEAD = [125.0, 109.375, 93.75, 109.375, 78.125, 109.375, 109.375, 125.0,
            109.375, 93.75, 109.375, 125.0, 125.0, 78.125, 93.75, 93.75,
            109.375, 109.375, 125.0, 109.375]

    def test_a_one_tick_median_shift_is_not_a_regression(self):
        m = gate.judge("startup_cpu_ms", self.BASE, self.HEAD, 160.0, 0.04, 0.01, 0.10)
        self.assertFalse(m["significant"])
        self.assertFalse(m["too_big"])
        self.assertEqual(m["verdict"], "pass")

    def test_a_shift_of_several_ticks_still_fails(self):
        head = [v + 46.875 for v in self.HEAD]
        m = gate.judge("startup_cpu_ms", self.BASE, head, 160.0, 0.04, 0.01, 0.10)
        self.assertTrue(m["too_big"])
        self.assertEqual(m["verdict"], "fail")


class AlternationTest(unittest.TestCase):
    def test_the_order_swaps_on_every_rep(self):
        pairs = [("head", "h"), ("base", "b")]
        self.assertEqual(gate.alternating(pairs, 0)[0][0], "head")
        self.assertEqual(gate.alternating(pairs, 1)[0][0], "base")

    def test_a_single_binary_is_unchanged(self):
        pairs = [("head", "h")]
        for i in range(4):
            self.assertEqual(gate.alternating(pairs, i), pairs)


class ConfigTest(unittest.TestCase):
    def setUp(self):
        self.cfg = json.loads((HERE / "budgets.json").read_text())

    def test_every_metric_has_a_tolerance_and_a_ceiling_per_platform(self):
        for platform, ceilings in self.cfg["budgets"].items():
            self.assertEqual(sorted(ceilings), sorted(gate.LABELS), platform)
        self.assertEqual(sorted(self.cfg["tolerance"]), sorted(gate.LABELS))

    def test_the_thresholds_are_sane(self):
        self.assertLess(0, self.cfg["alpha"])
        self.assertLess(self.cfg["alpha"], 0.5)
        self.assertLess(0, self.cfg["large_change"])
        for value in self.cfg["tolerance"].values():
            self.assertLess(0, value)
            self.assertLess(value, self.cfg["large_change"])

    def test_the_published_workload_is_what_the_gate_runs(self):
        # docs/reference.md describes the public benchmark as 40 turns of
        # 40 KB files; the gate must measure the same thing.
        self.assertEqual(self.cfg["workload"], {"turns": 40, "blob_kb": 40})

    def test_the_ceilings_are_above_zero(self):
        for ceilings in self.cfg["budgets"].values():
            for metric, value in ceilings.items():
                self.assertGreater(value, 0, metric)


class MarkdownTest(unittest.TestCase):
    def report(self, verdict):
        metric = {
            "base": 10.0, "head": 10.5, "change": 0.05, "budget": 50.0,
            "p_value": 0.2, "significant": False, "over_budget": False,
            "too_big": False, "near_budget": False, "verdict": verdict,
            "base_samples": SAME, "head_samples": SAME,
        }
        return {
            "platform": "linux", "base": "/tmp/base", "head": "/tmp/head",
            "workload": {"turns": 40, "blob_kb": 40},
            "reps": {"startup": 15, "turns": 10},
            "metrics": {metric_name: dict(metric) for metric_name in gate.LABELS},
            "ok": verdict == "pass",
        }

    def test_a_passing_run_says_so(self):
        text = gate.markdown(self.report("pass"))
        self.assertIn("within their ceiling", text)
        self.assertIn("| Startup CPU (ms) | 10.0 | 10.5 | +5.0% |", text)

    def test_a_failing_run_names_the_metric(self):
        report = self.report("fail")
        report["metrics"]["turn_cpu_ms"]["over_budget"] = True
        text = gate.markdown(report)
        self.assertIn("A metric regressed", text)
        self.assertIn("fail (over ceiling)", text)
        self.assertIn("CPU per request (ms)", text)

    def test_a_missing_base_is_named(self):
        report = self.report("pass")
        report["base"] = None
        self.assertIn("ceilings only", gate.markdown(report))


if __name__ == "__main__":
    unittest.main()
