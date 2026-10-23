"""Pure review logic and local fixtures; no network or real home access."""
import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest

import test_run
from test_run import FakeGh, REPO, fixture
import review
import run
from mock_model import MockModel

DIFF = '''diff --git a/old.py b/new.py
similarity index 70%
rename from old.py
rename to new.py
--- a/old.py
+++ b/new.py
@@ -2,3 +2,4 @@ function
 context
-old
+new
+extra
 tail
@@ -12 +13 @@
-last
+replacement
\\ No newline at end of file
diff --git a/dead b/dead
--- a/dead
+++ /dev/null
@@ -1 +0,0 @@
-deleted
'''


def finding(**kwargs):
    return dict(path="new.py", line=3, severity="high", title="Bad arithmetic", why="Subtracts instead of adding", scenario="add(2,3) gives -1", confidence=.99, verified=False, **kwargs)


class ReviewLogic(unittest.TestCase):
    def test_line_mapping_renames_deletions_and_multiple_hunks(self):
        self.assertEqual(review.diff_lines(DIFF), {"new.py": {2, 3, 4, 5, 13}})

    def test_quoted_paths_and_binary(self):
        diff = 'diff --git "a/a b" "b/a b"\n+++ "b/a b"\n@@ -0,0 +1,2 @@\n+one\n+two\ndiff --git a/bin b/bin\nBinary files differ\n'
        self.assertEqual(review.diff_lines(diff), {"a b": {1, 2}})

    def test_validation_confidence_ignore_paths_and_sort(self):
        low = {**finding(), "severity": "low", "confidence": .8}
        invalid = [{**finding(), "line": 9}, {**finding(), "scenario": ""}, {**finding(), "path": "../new.py"}, {**finding(), "confidence": "high"}]
        data = json.dumps({"findings": [low, finding(), finding(), *invalid], "checked": ["addition"]})
        parsed = review.parse_result(data, DIFF, {"strictness": "high"})
        self.assertEqual(len(parsed["findings"]), 1)
        self.assertFalse(parsed["findings"][0]["verified"])
        self.assertEqual(review.parse_result(data, DIFF, {"strictness": "all", "ignore_paths": ["*.py"]})["findings"], [])
        with self.assertRaises(ValueError):
            review.parse_result("No bugs!", DIFF, {"strictness": "high"})

    def test_json_extraction_tolerates_fences_and_prose(self):
        obj = {"findings": [], "checked": ["x"]}
        for text in (json.dumps(obj), "```json\n" + json.dumps(obj) + "\n```", "Here is the review:\n```\n" + json.dumps(obj) + "\n```\nDone.", "Result: " + json.dumps(obj)):
            self.assertEqual(review.extract_json(text), obj, text)
        with self.assertRaises(ValueError):
            review.extract_json("No bugs found.")

    def test_suggestion_format(self):
        self.assertEqual(review.suggestion("    return a + b"), "```suggestion\n    return a + b\n```")
        self.assertEqual(review.suggestion(""), "```suggestion\n\n```")
        with self.assertRaises(ValueError):
            review.suggestion("```\nspoof")
        f = finding(replacement="fixed")
        self.assertNotIn("```suggestion", review.finding_body(f))
        f["verified"] = True
        self.assertIn("```suggestion", review.finding_body(f))

    def test_update_and_dedupe_only_bot_comments(self):
        f = finding()
        class Gh(FakeGh):
            def pages(self, path):
                if "/issues/" in path:
                    return [{"id": 1, "body": review.MARKER, "user": {"login": "alice"}}, {"id": 2, "body": review.MARKER, "user": {"login": run.BOT_NAME}}]
                return [{"body": review.finding_body(f), "user": {"login": run.BOT_NAME}}]
        gh = Gh()
        review.publish(gh, 12, "sha", {"checked": ["arithmetic"], "findings": [{**f, "line": 13}]}, {"max_inline_comments": 10})
        self.assertEqual(len(gh.calls), 1)
        self.assertEqual(gh.calls[0][1:3], (f"repos/{REPO}/issues/comments/2", "PATCH"))
        self.assertIn("not verified", gh.calls[0][3]["body"])

    def test_app_bot_owns_markers_but_humans_cannot_spoof(self):
        f = finding()
        app = {"login": "acme-reviewer[bot]", "type": "Bot"}
        human = {"login": "mallory", "type": "User"}
        class Gh(FakeGh):
            def pages(self, path):
                if "/issues/" in path:
                    return [{"id": 5, "body": review.MARKER, "user": app}]
                return [{"body": review.finding_body(f), "user": human}]
        gh = Gh()
        review.publish(gh, 12, "sha", {"checked": [], "findings": [f]}, {"max_inline_comments": 10})
        self.assertEqual(gh.calls[0][1:3], (f"repos/{REPO}/issues/comments/5", "PATCH"))
        # The human-pasted finding marker does not suppress the real finding.
        self.assertEqual(len(gh.calls[1][3]["comments"]), 1)
        self.assertFalse(review.is_bot({"user": {"login": "fake[bot]", "type": "User"}}))

    def test_new_summary_when_none_exists_and_zero_cap(self):
        gh = FakeGh()
        review.publish(gh, 12, "sha", {"checked": ["x"], "findings": [finding()]}, {"max_inline_comments": 0})
        self.assertEqual([c[1:3] for c in gh.calls], [(f"repos/{REPO}/issues/12/comments", "POST")])
        self.assertTrue(gh.calls[0][3]["body"].startswith(review.MARKER))

    def test_one_review_capped_at_ten(self):
        gh = FakeGh()
        findings = [{**finding(), "title": str(i)} for i in range(20)]
        review.publish(gh, 12, "sha", {"checked": [], "findings": findings}, {"max_inline_comments": 10})
        self.assertEqual(len(gh.calls), 2)
        self.assertEqual(len(gh.calls[1][3]["comments"]), 10)
        self.assertEqual(gh.calls[1][3]["commit_id"], "sha")

    def test_pull_request_target_refused(self):
        self.assertEqual(run.classify("pull_request_target", fixture("pull_request"), "@chanto", "").kind, "skip")

    def test_check_gate_removes_credentials(self):
        captured = {}
        def runner(cmd, **kwargs):
            captured.update(kwargs["env"])
            return subprocess.CompletedProcess(cmd, 0, "is_error: false\n", "")
        import tempfile
        with tempfile.TemporaryDirectory() as tmp:
            self.assertTrue(review.run_checks({"CHANTO_HOME": tmp, "GH_TOKEN": "secret", "OPENAI_API_KEY": "secret", 'INPUT_PROVIDER_CONFIG': '[providers.test]\napi_key = "secret"\n'}, {"checks": ["true"]}, runner))
        self.assertNotIn("GH_TOKEN", captured)
        self.assertNotIn("OPENAI_API_KEY", captured)
        self.assertNotIn("INPUT_PROVIDER_CONFIG", captured)
        self.assertFalse(review.run_checks({}, {"checks": []}, runner))


class ReviewFixture(unittest.TestCase):
    setUp = test_run.EndToEnd.setUp
    tearDown = test_run.EndToEnd.tearDown
    env = test_run.EndToEnd.env
    def test_dry_run_known_bug_verified_in_scratch(self):
        (self.repo / "check.py").write_text("from broken import add\nassert add(2, 3) == 5\n")
        (self.repo / ".chanto/review.toml").write_text('checks = ["python3 -B check.py"]\n')
        subprocess.run(["git", "add", "-A"], check=True)
        subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "bug fixture"], check=True)
        before = (self.repo / "broken.py").read_text()
        gh, output = FakeGh(), io.StringIO()
        with contextlib.redirect_stdout(output):
            code = run.main(self.env("pull_request", "pull_request", INPUT_DRY_RUN="true"), gh)
        data = json.loads(output.getvalue())
        self.assertEqual(code, 0)
        self.assertTrue(data["findings"][0]["verified"])
        self.assertEqual(data["findings"][0]["line"], 3)
        self.assertEqual((self.repo / "broken.py").read_text(), before)
        self.assertFalse(any(c[0] in ("api", "comment", "edit", "react") for c in gh.calls))

    def test_failed_check_never_offers_suggestion(self):
        (self.repo / ".chanto/review.toml").write_text('checks = ["false"]\n')
        gh, output = FakeGh(), io.StringIO()
        with contextlib.redirect_stdout(output):
            self.assertEqual(run.main(self.env("pull_request", "pull_request", INPUT_DRY_RUN="true"), gh), 0)
        self.assertFalse(json.loads(output.getvalue())["findings"][0]["verified"])

    def test_failed_fix_not_published(self):
        (self.repo / ".chanto/review.toml").write_text('checks = ["false"]\n')
        gh = FakeGh()
        os.environ["STUB_EDIT"] = "1"
        try:
            self.assertEqual(run.main(self.env("issue_comment", "issue_comment"), gh), 1)
        finally:
            os.environ.pop("STUB_EDIT", None)
        self.assertNotIn("create_pr", [c[0] for c in gh.calls])
        self.assertIn("not published", gh.comments[1000])

    def test_explain_has_no_write_tools(self):
        env = self.env("issue_comment", "issue_comment")
        path = Path(env["GITHUB_EVENT_PATH"])
        event = json.loads(path.read_text())
        event["comment"]["body"] = "@chanto explain addition"
        path.write_text(json.dumps(event))
        self.assertEqual(run.main(env, FakeGh()), 0)
        self.assertIn("--mode plan --sandbox read-only", (self.tmp / "args.txt").read_text())

    def test_config_from_base_cannot_be_overridden_by_pr(self):
        base = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
        (self.repo / ".chanto/review.toml").write_text('checks = ["false"]\nmodel = "attacker/model"\n')
        self.assertEqual(review.config(ref=base)["checks"], ["true"])
        self.assertNotIn("model", review.config(ref=base))


BINARY = Path(os.environ.get("CHANTO_TEST_BIN") or Path(__file__).resolve().parents[2] / "target/debug/chanto")


@unittest.skipUnless(BINARY.is_file(), "build Chanto first (cargo build) to run the real-binary dry run")
class RealBinaryDryRun(unittest.TestCase):
    """The real Chanto CLI against a local mock model: catches flag drift
    between run.py and the binary. No GitHub API or real model is used."""
    setUp = test_run.EndToEnd.setUp
    env = test_run.EndToEnd.env

    def tearDown(self):
        self.model.close()
        test_run.EndToEnd.tearDown(self)

    def test_dry_run_prints_review_json(self):
        reply = {"checked": ["Arithmetic in add()"], "findings": [
            {"path": "broken.py", "line": 3, "severity": "high", "title": "Addition subtracts",
             "why": "The function subtracts its second operand.", "scenario": "add(2, 3) returns -1 instead of 5",
             "confidence": .99, "replacement": "    return a + b"},
            {"path": "broken.py", "line": 99, "severity": "low", "title": "Off-diff", "why": "x", "scenario": "y", "confidence": .99},
        ]}
        self.model = MockModel(json.dumps(reply))
        (self.repo / "check.py").write_text("from broken import add\nassert add(2, 3) == 5\n")
        (self.repo / ".chanto/review.toml").write_text('checks = ["python3 -B check.py"]\n')
        subprocess.run(["git", "add", "-A"], check=True)
        subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "bug fixture"], check=True)
        before = (self.repo / "broken.py").read_text()
        env = self.env("pull_request", "pull_request", INPUT_DRY_RUN="true", CHANTO_BIN=str(BINARY), INPUT_MODEL="mock",
                       CHANTO_PROVIDER="openai", CHANTO_BASE_URL=self.model.url, CHANTO_API_KEY="mock-key", CI="1")
        env.pop("ANTHROPIC_API_KEY")
        gh, output = FakeGh(), io.StringIO()
        with contextlib.redirect_stdout(output):
            code = run.main(env, gh)
        self.assertEqual(code, 0, output.getvalue())
        data = json.loads(output.getvalue())
        self.assertEqual([(f["path"], f["line"]) for f in data["findings"]], [("broken.py", 3)])
        self.assertEqual(data["checked"], ["Arithmetic in add()"])
        self.assertIsInstance(data["findings"][0]["verified"], bool)
        if sys.platform == "darwin":
            # Seatbelt is always available there, so the sandboxed check gate runs for real.
            self.assertTrue(data["findings"][0]["verified"])
        self.assertEqual((self.repo / "broken.py").read_text(), before)
        self.assertFalse(any(c[0] in ("api", "comment", "edit", "react") for c in gh.calls))
        prompt = self.model.requests[0]["messages"][-1]["content"]
        self.assertIn("<diff>", json.dumps(prompt))
