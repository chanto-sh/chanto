"""Tests for tests/perf/exact.py's parsing and verdicts."""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import exact  # noqa: E402

CFG = {
    "tolerance": {"startup_instructions": 0.01, "turn_instructions": 0.01, "prompt_bytes": 0.0},
    "ceilings": {"prompt_bytes": 1000},
}


def costs(startup=100, turns=100, prompt=500):
    return {"startup_instructions": startup, "turn_instructions": turns, "prompt_bytes": prompt}


def failed(head, base=None):
    return [r[0] for r in exact.verdicts(head, base, CFG) if r[5]]


class Exact(unittest.TestCase):
    def test_reads_callgrinds_count(self):
        self.assertEqual(exact.collected("==1== Events : Ir\n==1== Collected : 123456\n"), 123456)
        with self.assertRaises(exact.gate.GateError):
            exact.collected("valgrind: failed")

    def test_prompt_counts_system_messages_and_tools(self):
        body = {"messages": [{"role": "system", "content": "abc"}, {"role": "user", "content": "x" * 99}]}
        without = exact.prompt_bytes(body)
        self.assertLess(without, 99)
        self.assertGreater(exact.prompt_bytes({**body, "tools": [{"name": "Read"}]}), without)

    def test_a_change_over_its_tolerance_fails(self):
        self.assertEqual(failed(costs(), costs()), [])
        self.assertEqual(failed(costs(startup=101), costs()), [])
        self.assertEqual(failed(costs(startup=102), costs()), ["startup_instructions"])
        self.assertEqual(failed(costs(prompt=501), costs()), ["prompt_bytes"])
        self.assertEqual(failed(costs(startup=50), costs()), [])

    def test_a_ceiling_holds_without_a_base(self):
        self.assertEqual(failed(costs(prompt=1001)), ["prompt_bytes"])
        self.assertEqual(failed(costs(startup=10**12)), [])


if __name__ == "__main__":
    unittest.main()
