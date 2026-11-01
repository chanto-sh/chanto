"""Tests for tests/perf/responsive.py's verdicts and CPU parsing."""
import os
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import responsive  # noqa: E402


class Responsive(unittest.TestCase):
    def test_only_a_metric_over_its_ceiling_fails(self):
        metrics = {"idle_bytes": 1, "typing_ms": 2.0, "page_up_ms": 40.0}
        self.assertEqual(responsive.failures(metrics, {"idle_bytes": 0, "typing_ms": 2.0}), ["idle_bytes"])

    @unittest.skipIf(sys.platform == "win32", "reads a live process through /proc or ps")
    def test_reads_a_live_process_cpu(self):
        self.assertGreaterEqual(responsive.cpu_ms(os.getpid()), 0)

    def test_the_tui_scenario_has_the_long_reply(self):
        import json
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            scenario = responsive.tui_scenario(Path(tmp))
            self.assertIn("line 19999", json.loads(scenario.read_text())[0]["text"])


if __name__ == "__main__":
    unittest.main()
