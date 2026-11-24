"""Tests for action/quality.py: python3 -m unittest discover -s action/tests"""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
import quality  # noqa: E402

BASE = "a" * 40
REPORT = """✓ cargo test --quiet (3.1s)
✗ no-todo (No TODOs)
  src/a.rs:12: // TODO later
  src/b, c.rs:3: x
✗ secrets (looks like a key, token or password)
  config.py:4: KEY = "[REDACTED:GITHUB_TOKEN]"
3 violations of 2 rules in .chanto/quality.toml. Fix the code; the rules are the project's.
"""


class Plan(unittest.TestCase):
    def test_pull_request_checks_its_changes(self):
        args, why = quality.plan("pull_request", {"pull_request": {"base": {"sha": BASE}}})
        self.assertEqual(args, ["--base", BASE, "--head", "HEAD"])
        self.assertIn("pull request", why)

    def test_push_checks_the_pushed_commits_or_everything(self):
        self.assertEqual(quality.plan("push", {"before": BASE})[0], ["--base", BASE, "--head", "HEAD"])
        self.assertEqual(quality.plan("push", {"before": quality.ZERO})[0], ["--all"])

    def test_merge_group_and_manual_runs(self):
        mg = quality.plan("merge_group", {"merge_group": {"base_sha": BASE}})[0]
        self.assertEqual(mg, ["--base", BASE, "--head", "HEAD"])
        self.assertEqual(quality.plan("workflow_dispatch", {})[0], ["--all"])
        self.assertEqual(quality.plan("schedule", {})[0], ["--all"])

    def test_untrusted_events_are_refused(self):
        for event in ("pull_request_target", "issue_comment", "workflow_run"):
            args, why = quality.plan(event, {})
            self.assertIsNone(args)
            self.assertIn(event, why)


class Report(unittest.TestCase):
    def test_findings_become_annotations(self):
        a = quality.annotations(REPORT)
        self.assertEqual(len(a), 3)
        self.assertEqual(a[0], "::error file=src/a.rs,line=12,title=no-todo%3A No TODOs::// TODO later")
        self.assertTrue(a[1].startswith("::error file=src/b%2C c.rs,line=3,"), a[1])
        self.assertIn("title=secrets%3A looks like a key%2C token or password", a[2])
        self.assertNotIn("✓", "".join(a))

    def test_annotations_are_relative_to_the_repository(self):
        a = quality.annotations(REPORT, "services/api/")
        self.assertTrue(a[0].startswith("::error file=services/api/src/a.rs,line=12,"), a[0])

    def test_no_findings_no_annotations(self):
        self.assertEqual(quality.annotations("Quality check: 1 rule and the secrets scan, nothing found."), [])

    def test_summary(self):
        self.assertIn("✅", quality.summary("every file", "ok", True))
        s = quality.summary("every file", REPORT, False)
        self.assertIn("❌", s)
        self.assertIn("src/a.rs:12", s)


class Main(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.ws = self.tmp / "ws"
        (self.ws / ".chanto").mkdir(parents=True)
        (self.ws / ".chanto/quality.toml").write_text("version = 1\n")
        self.event = self.tmp / "event.json"
        self.event.write_text(json.dumps({"pull_request": {"base": {"sha": BASE}}}))
        self.calls = []

    def env(self, **kw):
        e = {
            "GITHUB_WORKSPACE": str(self.ws),
            "GITHUB_EVENT_NAME": "pull_request",
            "GITHUB_EVENT_PATH": str(self.event),
            "GITHUB_STEP_SUMMARY": str(self.tmp / "summary.md"),
            "CHANTO_HOME": str(self.tmp / "home"),
        }
        e.update(kw)
        return e

    def runner(self, help_text="  quality check [--all]  ...", check_code=1, have_base=True):
        def run(cmd, **kw):
            self.calls.append(cmd)
            if cmd[:2] == ["chanto", "--help"]:
                return subprocess.CompletedProcess(cmd, 0, help_text, "")
            if cmd[:2] == ["git", "cat-file"]:
                return subprocess.CompletedProcess(cmd, 0 if have_base else 1, "", "")
            if cmd[:2] == ["git", "fetch"]:
                return subprocess.CompletedProcess(cmd, 1, "", "no such commit")
            return subprocess.CompletedProcess(cmd, check_code, REPORT if check_code else "nothing found\n", "")

        return run

    def test_runs_the_check_and_reports(self):
        code = quality.main(self.env(), self.runner())
        self.assertEqual(code, 1)
        cmd = self.calls[-1]
        self.assertEqual(cmd, ["chanto", "quality", "check", "--base", BASE, "--head", "HEAD", "--checks"])
        trusted = json.loads((self.tmp / "home/trusted.json").read_text())
        self.assertEqual(trusted, [str(self.ws.resolve())])
        self.assertIn("src/a.rs:12", (self.tmp / "summary.md").read_text())

    def test_passing_check_and_no_checks_flag(self):
        code = quality.main(self.env(INPUT_QUALITY_CHECKS="false"), self.runner(check_code=0))
        self.assertEqual(code, 0)
        self.assertNotIn("--checks", self.calls[-1])
        self.assertIn("✅", (self.tmp / "summary.md").read_text())

    def test_scope_all_and_outputs(self):
        out = self.tmp / "out.txt"
        code = quality.main(self.env(INPUT_QUALITY_SCOPE="all", GITHUB_OUTPUT=str(out)), self.runner())
        self.assertEqual(code, 1)
        self.assertEqual(self.calls[-1], ["chanto", "quality", "check", "--all", "--checks"])
        self.assertIn("findings=3\npassed=false\n", out.read_text())
        self.assertFalse(any(c[:2] == ["git", "cat-file"] for c in self.calls), "no base needed")

    def test_working_directory(self):
        sub = self.ws / "services" / "api"
        (sub / ".chanto").mkdir(parents=True)
        (sub / ".chanto/quality.toml").write_text("version = 1\n")
        seen = {}
        base = self.runner()

        def run(cmd, **kw):
            if cmd[:2] == ["chanto", "quality"]:
                seen["cwd"] = kw.get("cwd")
            return base(cmd, **kw)

        import io, contextlib
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = quality.main(self.env(INPUT_WORKING_DIRECTORY="services/api"), run)
        self.assertEqual(code, 1)
        self.assertEqual(Path(seen["cwd"]), sub.resolve())
        self.assertIn("::error file=services/api/src/a.rs,line=12,", buf.getvalue())
        trusted = json.loads((self.tmp / "home/trusted.json").read_text())
        self.assertEqual(trusted, [str(sub.resolve())])

    def test_bad_scope_and_directory_are_refused(self):
        self.assertEqual(quality.main(self.env(INPUT_QUALITY_SCOPE="some"), self.runner()), 1)
        self.assertEqual(quality.main(self.env(INPUT_WORKING_DIRECTORY="../elsewhere"), self.runner()), 1)
        self.assertEqual(quality.main(self.env(INPUT_WORKING_DIRECTORY="missing"), self.runner()), 1)
        self.assertFalse(any(c[:2] == ["chanto", "quality"] for c in self.calls))

    def test_refusals(self):
        self.assertEqual(quality.main(self.env(GITHUB_EVENT_NAME="pull_request_target"), self.runner()), 1)
        self.assertEqual(quality.main(self.env(), self.runner(help_text="Commands:\n  setup")), 1)
        self.assertEqual(quality.main(self.env(), self.runner(have_base=False)), 1)
        (self.ws / ".chanto/quality.toml").unlink()
        self.assertEqual(quality.main(self.env(), self.runner()), 1)
        # None of them ran the check.
        self.assertFalse(any(c[:2] == ["chanto", "quality"] for c in self.calls))


if __name__ == "__main__":
    unittest.main()
