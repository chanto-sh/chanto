"""The invariants catch what they claim to, without a Chanto binary."""
import runpy
import unittest
from pathlib import Path

inv = runpy.run_path(str(Path(__file__).with_name("invariants.py")), run_name="invariants")
Run, Failure, check = inv["Run"], inv["Failure"], inv["check_common"]


class Invariants(unittest.TestCase):
    def setUp(self):
        self.run_dir = Run("test")
        self.addCleanup(self.run_dir.close)

    def fails(self, message, output=""):
        with self.assertRaises(Failure) as e:
            check(self.run_dir, output)
        self.assertIn(message, str(e.exception))

    def test_a_clean_run_passes(self):
        check(self.run_dir, "all fine")

    def test_a_panic_in_the_output_or_logs(self):
        self.fails("no panic", "thread 'main' panicked at src/lib.rs:1:1")

    def test_a_panic_in_the_logs(self):
        logs = self.run_dir.chanto_home / "logs"
        logs.mkdir()
        (logs / "a.log").write_text("panicked at x")
        self.fails("no panic")

    def test_a_broken_session_or_temp_file(self):
        sessions = self.run_dir.chanto_home / "sessions" / "p"
        sessions.mkdir(parents=True)
        (sessions / "a.json").write_text('{"id":')
        self.fails("sessions parse")
        (sessions / "a.json").write_text("{}")
        check(self.run_dir, "")
        (sessions / "a.json.1.tmp").write_text("{}")
        self.fails("temp file")

    def test_a_change_outside_the_project(self):
        (self.run_dir.outside / "new.txt").write_text("x")
        self.fails("contained")

    def test_a_secret_in_a_request(self):
        self.run_dir.dumps.mkdir()
        (self.run_dir.dumps / "0001.json").write_text(f'{{"c":"{inv["CANARY"]}"}}')
        self.fails("no leak")


if __name__ == "__main__":
    unittest.main()
