import runpy
import unittest
from pathlib import Path

gate = runpy.run_path(str(Path(__file__).with_name("test_gate.py")), run_name="test_gate")
require = gate["require_tests"]


class RequireTests(unittest.TestCase):
    def test_code_without_a_test_fails(self):
        why = require("fix(tui): a", "", ["src/tui/draw.rs"], "")
        self.assertIn("Tests: none", why)
        self.assertIsNotNone(require("feat!: a", "", ["core/src/model.rs"], ""))

    def test_a_test_file_or_inline_test_passes(self):
        for path in ("tests/a.rs", "src/a/tests.rs", "src/a_tests.rs", "web/test_x.py", "a/b.test.ts"):
            self.assertIsNone(require("fix: a", "", ["src/a.rs", path], ""), path)
        diff = "+++ b/src/a.rs\n+    #[tokio::test]\n+    async fn x() {}\n"
        self.assertIsNone(require("fix: a", "", ["src/a.rs"], diff))
        self.assertIsNotNone(require("fix: a", "", ["src/a.rs"], "-    #[test]\n"))

    def test_other_types_and_paths_are_left_alone(self):
        self.assertIsNone(require("docs: a", "", ["src/a.rs"], ""))
        self.assertIsNone(require("perf: a", "", ["src/a.rs"], ""))
        self.assertIsNone(require("fix: a", "", ["docs/a.md", ".github/workflows/ci.yml"], ""))
        self.assertIsNone(require("fix: a", "", ["scripts/a.py"], ""))

    def test_opting_out_needs_a_reason_outside_the_template_comments(self):
        for body, ok in (
            ("Tests: none (only a log message)", True),
            ("tests: none - log message", True),
            ("Tests: none", False),
            ("<!-- Tests: none (the template's example) -->", False),
        ):
            result = require("fix: a", body, ["src/a.rs"], "")
            self.assertEqual(result is None, ok, body)


class Coverage(unittest.TestCase):
    def test_changed_lines_reads_a_zero_context_diff(self):
        diff = "+++ b/src/a.rs\n@@ -1,0 +2,2 @@\n+x\n+y\n@@ -9 +11 @@\n-a\n+b\n+++ /dev/null\n@@ -1 +0,0 @@\n"
        self.assertEqual(gate["changed_lines"](diff), {"src/a.rs": {2, 3, 11}})

    def test_the_report_counts_only_executable_product_lines(self):
        lcov = "SF:/repo/src/a.rs\nDA:2,5\nDA:3,0\nend_of_record\nSF:/repo/src/a_tests.rs\nDA:1,0\nend_of_record\n"
        hits = gate["covered_lines"](lcov, Path("/repo"))
        changed = {"src/a.rs": {1, 2, 3}, "src/a_tests.rs": {1}, "src/gone.rs": {4}}
        self.assertEqual(gate["patch_report"](changed, hits), (1, 2, {"src/a.rs": [3]}))

    def test_a_line_in_two_test_binaries_counts_if_either_ran_it(self):
        lcov = "SF:/r/a.rs\nDA:1,0\nend_of_record\nSF:/r/a.rs\nDA:1,2\nend_of_record\n"
        self.assertEqual(gate["covered_lines"](lcov, Path("/r")), {"a.rs": {1: 2}})


if __name__ == "__main__":
    unittest.main()
