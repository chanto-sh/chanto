"""Tests for action/run.py: python3 -m unittest discover -s action/tests"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
import run  # noqa: E402

FIX = HERE / "fixtures"
REPO = "acme/widgets"


def fixture(name: str) -> dict:
    return json.loads((FIX / f"{name}.json").read_text())


class FakeGh:
    """Records calls instead of talking to GitHub."""

    def __init__(self, perm="write", same_repo=True):
        self.repo = REPO
        self.perm = perm
        self.same_repo = same_repo
        self.calls = []
        self.comments = {}

    def pages(self, path):
        return []

    def api(self, path, method="GET", body=None):
        self.calls.append(("api", path, method, body))
        return {"id": 900}

    def permission(self, user):
        self.calls.append(("permission", user))
        return {"permission": self.perm, "role_name": self.perm}

    def react(self, job, content="eyes"):
        self.calls.append(("react", job.comment_id, content))

    def comment(self, job, body):
        cid = 1000 + len(self.comments)
        self.comments[cid] = body
        self.calls.append(("comment", job.number, body))
        return cid

    def edit_comment(self, job, cid, body):
        self.comments[cid] = body
        self.calls.append(("edit", cid, body))

    def pr(self, number):
        full = REPO if self.same_repo else "someone/fork"
        return {"number": number, "head": {"ref": "feature-x", "sha": "headsha", "repo": {"full_name": full}}}

    def diff(self, number):
        return "diff --git a/broken.py b/broken.py\n--- a/broken.py\n+++ b/broken.py\n@@ -1,3 +1,3 @@\n def add(a, b):\n     # add both numbers\n-    return a + b\n+    return a - b\n"

    def review(self, number, body):
        self.calls.append(("review", number, body))

    def create_pr(self, head, base, title, body):
        self.calls.append(("create_pr", head, base, title))
        return f"https://github.com/{REPO}/pull/99"


class Classify(unittest.TestCase):
    def test_issue_comment_on_issue(self):
        j = run.classify("issue_comment", fixture("issue_comment"), "@chanto", "")
        self.assertEqual((j.kind, j.number, j.is_pr, j.comment_id, j.author), ("comment", 7, False, 555, "alice"))
        self.assertIn("off-by-one", j.request)

    def test_issue_comment_on_pr(self):
        ev = fixture("issue_comment")
        ev["issue"]["pull_request"] = {"url": "x"}
        self.assertTrue(run.classify("issue_comment", ev, "@chanto", "").is_pr)

    def test_trigger_required_and_word_boundary(self):
        ev = fixture("issue_comment")
        ev["comment"]["body"] = "ping @chantobot please"
        self.assertEqual(run.classify("issue_comment", ev, "@chanto", "").kind, "skip")
        ev["comment"]["body"] = "@CHANTO, fix it"
        self.assertEqual(run.classify("issue_comment", ev, "@chanto", "").kind, "comment")
        ev["action"] = "edited"
        self.assertEqual(run.classify("issue_comment", ev, "@chanto", "").kind, "skip")

    def test_review_comment(self):
        j = run.classify("pull_request_review_comment", fixture("review_comment"), "@chanto", "")
        self.assertEqual((j.kind, j.comment_kind, j.is_pr, j.number), ("comment", "review", True, 12))
        self.assertEqual(j.extra["path"], "src/lib.rs")

    def test_pull_request_review(self):
        j = run.classify("pull_request", fixture("pull_request"), "@chanto", "")
        self.assertEqual((j.kind, j.number), ("review", 12))
        self.assertEqual(j.request, run.DEFAULT_REVIEW_PROMPT)
        ev = fixture("pull_request")
        ev["pull_request"]["draft"] = True
        self.assertEqual(run.classify("pull_request", ev, "@chanto", "").kind, "skip")
        ev["action"] = "closed"
        self.assertEqual(run.classify("pull_request", ev, "@chanto", "").kind, "skip")

    def test_dispatch(self):
        self.assertEqual(run.classify("workflow_dispatch", {}, "@chanto", "update deps").kind, "prompt")
        self.assertEqual(run.classify("workflow_dispatch", {}, "@chanto", "").kind, "skip")


class Helpers(unittest.TestCase):
    def test_permissions(self):
        self.assertTrue(run.can_write({"permission": "admin"}))
        self.assertTrue(run.can_write({"permission": "write", "role_name": "maintain"}))
        self.assertTrue(run.can_write({"permission": "read", "role_name": "maintain"}))
        self.assertFalse(run.can_write({"permission": "read", "role_name": "triage"}))
        self.assertFalse(run.can_write({"permission": "none"}))

    def test_parse_command(self):
        self.assertEqual(run.parse_command("@chanto review", "@chanto"), "review")
        self.assertEqual(run.parse_command("hey @Chanto: FIX the off-by-one", "@chanto"), "fix")
        self.assertEqual(run.parse_command("@chanto, explain this hunk", "@chanto"), "explain")
        self.assertIsNone(run.parse_command("@chanto reviewer wanted", "@chanto"))
        self.assertIsNone(run.parse_command("@chanto deploy", "@chanto"))

    def test_author_association_prefilter(self):
        for ok in ("OWNER", "MEMBER", "COLLABORATOR", "", "member"):
            self.assertTrue(run.may_write(ok), ok)
        for bad in ("CONTRIBUTOR", "FIRST_TIME_CONTRIBUTOR", "FIRST_TIMER", "MANNEQUIN", "NONE"):
            self.assertFalse(run.may_write(bad), bad)
        event = fixture("issue_comment")
        event["comment"]["author_association"] = "CONTRIBUTOR"
        self.assertEqual(run.classify("issue_comment", event, "@chanto", "").association, "CONTRIBUTOR")

    def test_prompt_wraps_untrusted_text(self):
        j = run.classify("issue_comment", fixture("issue_comment"), "@chanto", "")
        p = run.build_prompt(j, REPO, "@chanto")
        self.assertIn("<request>", p)
        self.assertIn("new branch", p)
        self.assertIn(REPO, p)
        j.is_pr = True
        p = run.build_prompt(j, REPO, "@chanto", diff="+x", head_ref="feat", can_push=False)
        self.assertIn("from a fork", p)
        self.assertIn("<diff>", p)

    def test_plan_changes(self):
        j = run.Job("comment", number=1, is_pr=True)
        same = {"head": {"repo": {"full_name": REPO}}}
        fork = {"head": {"repo": {"full_name": "x/y"}}}
        self.assertEqual(run.plan_changes(j, ["a"], same, REPO), "push")
        self.assertEqual(run.plan_changes(j, ["a"], fork, REPO), "fork")
        self.assertEqual(run.plan_changes(j, [], same, REPO), "none")
        self.assertEqual(run.plan_changes(run.Job("comment", number=1), ["a"], None, REPO), "pr")
        self.assertEqual(run.plan_changes(run.Job("review", number=1, is_pr=True), ["a"], same, REPO), "none")

    def test_redact(self):
        env = {"ANTHROPIC_API_KEY": "sk-ant-secret-123"}
        self.assertEqual(run.redact("key is sk-ant-secret-123", env), "key is ***")

    def test_chanto_command(self):
        cmd = run.chanto_command({"INPUT_MODEL": "anthropic/x", "INPUT_MAX_TURNS": "20", "INPUT_TRUST_PROJECT": "false"})
        self.assertEqual(cmd, ["chanto", "-p", "--output-format", "json", "--model", "anthropic/x", "--max-turns", "20"])


class Hardening(unittest.TestCase):
    def test_issue_author_must_have_access(self):
        event = {"action": "edited", "sender": {"login": "maint"}, "issue": {"number": 1, "user": {"login": "outsider"}, "body": "@chanto do x", "title": "t"}}
        job = run.classify("issues", event, "@chanto", "")
        self.assertEqual(job.author, "outsider")

    def test_redacts_encoded_tokens(self):
        import base64
        tok = "ghs_abcdefghijklmnop"
        header = base64.b64encode(f"x-access-token:{tok}".encode()).decode()
        out = run.redact(f"a {header} b {tok}", {"GITHUB_TOKEN": tok})
        self.assertNotIn(tok, out)
        self.assertNotIn(header, out)

    def test_agent_env_hides_github_token(self):
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            child = run.agent_env({"GITHUB_TOKEN": "x", "GH_TOKEN": "y", "ANTHROPIC_API_KEY": "k", "CHANTO_HOME": d})
            self.assertNotIn("GITHUB_TOKEN", child)
            self.assertNotIn("GH_TOKEN", child)
            self.assertEqual(child["ANTHROPIC_API_KEY"], "k")
            self.assertIn("Read(.git/**)", Path(d, "config.toml").read_text())


class EndToEnd(unittest.TestCase):
    """main() in a real git repo with a stub Chanto and a fake GitHub."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.old_home = {k: os.environ.get(k) for k in ("HOME", "CHANTO_HOME", "XDG_CONFIG_HOME", "GIT_CONFIG_NOSYSTEM")}
        os.environ.update(HOME=str(self.tmp), CHANTO_HOME=str(self.tmp / "home"), XDG_CONFIG_HOME=str(self.tmp / "xdg"), GIT_CONFIG_NOSYSTEM="1")
        self.repo = self.tmp / "repo"
        self.origin = self.tmp / "origin.git"
        subprocess.run(["git", "init", "-q", "--bare", str(self.origin)], check=True)
        subprocess.run(["git", "init", "-q", "-b", "main", str(self.repo)], check=True)
        self.repo.mkdir(exist_ok=True)
        (self.repo / "README.md").write_text("hi\n")
        (self.repo / ".chanto").mkdir()
        (self.repo / ".chanto/review.toml").write_text('checks = ["true"]\n')
        (self.repo / "broken.py").write_text("def add(a, b):\n    # add both numbers\n    return a - b\n")
        g = lambda *a: subprocess.run(["git", "-C", str(self.repo), *a], check=True, capture_output=True)
        g("add", "-A")
        g("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "readme")
        g("remote", "add", "origin", str(self.origin))
        g("push", "-q", "origin", "main")
        # Stub chanto: records its stdin, edits a file, prints a JSON result.
        self.stub = self.tmp / "chanto-stub"
        self.stub.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, pathlib, subprocess, sys\n"
            "if '--run-tool' in sys.argv:\n"
            "    r = subprocess.run(json.loads(sys.argv[-1])['command'], shell=True, capture_output=True, text=True)\n"
            "    print('is_error: ' + ('false' if r.returncode == 0 else 'true'))\n"
            "    sys.exit(0)\n"
            "prompt = sys.stdin.read()\n"
            f"pathlib.Path({str(self.tmp / 'prompt.txt')!r}).write_text(prompt)\n"
            f"pathlib.Path({str(self.tmp / 'args.txt')!r}).write_text(' '.join(sys.argv[1:]))\n"
            "if os.environ.get('STUB_EDIT'): pathlib.Path('fix.txt').write_text('fixed')\n"
            "result = 'Fixed the bug. Token sk-ant-secret-123'\n"
            "if 'Return ONLY JSON' in prompt:\n"
            f"    pathlib.Path({str(self.tmp / 'review-prompt.txt')!r}).write_text(prompt)\n"
            "    result = json.dumps({'checked': ['Arithmetic on the added line'], 'findings': [{'path': 'broken.py', 'line': 3, 'severity': 'high', 'title': 'Addition subtracts', 'why': 'The function subtracts its second operand.', 'scenario': 'add(2, 3) returns -1 instead of 5', 'confidence': .99, 'replacement': '    return a + b'}]})\n"
            "print(json.dumps({'type': 'result', 'is_error': False, 'result': result}))\n"
        )
        self.stub.chmod(0o755)
        self.cwd = os.getcwd()
        os.chdir(self.repo)

    def tearDown(self):
        os.chdir(self.cwd)
        shutil.rmtree(self.tmp, ignore_errors=True)
        for key, value in self.old_home.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value

    def env(self, event_name, fixture_name, **extra):
        path = self.tmp / "event.json"
        path.write_text(json.dumps(fixture(fixture_name)))
        e = {
            "GITHUB_REPOSITORY": REPO,
            "GITHUB_EVENT_NAME": event_name,
            "GITHUB_EVENT_PATH": str(path),
            "GITHUB_RUN_ID": "42",
            "CHANTO_BIN": str(self.stub),
            "ANTHROPIC_API_KEY": "sk-ant-secret-123",
            "INPUT_MODEL": "anthropic/claude-sonnet-5-5",
            "PATH": os.environ["PATH"],
        }
        e.update(extra)
        return e

    def test_issue_comment_opens_pr(self):
        gh = FakeGh()
        os.environ["STUB_EDIT"] = "1"
        try:
            code = run.main(self.env("issue_comment", "issue_comment"), gh)
        finally:
            del os.environ["STUB_EDIT"]
        self.assertEqual(code, 0)
        kinds = [c[0] for c in gh.calls]
        self.assertEqual(kinds, ["permission", "react", "comment", "create_pr", "edit"])
        final = gh.comments[1000]
        self.assertIn("Fixed the bug", final)
        self.assertNotIn("sk-ant-secret-123", final)
        self.assertIn("Opened https://github.com/acme/widgets/pull/99", final)
        # The untrusted comment went in via stdin, not argv.
        self.assertIn("off-by-one", (self.tmp / "prompt.txt").read_text())
        self.assertNotIn("off-by-one", (self.tmp / "args.txt").read_text())
        branches = subprocess.run(["git", "--git-dir", str(self.origin), "branch"], capture_output=True, text=True).stdout
        self.assertIn("chanto/issue-7-", branches)
        create = [c for c in gh.calls if c[0] == "create_pr"][0]
        self.assertEqual(create[2], "main")

    def test_question_without_changes(self):
        gh = FakeGh()
        self.assertEqual(run.main(self.env("issue_comment", "issue_comment"), gh), 0)
        self.assertNotIn("create_pr", [c[0] for c in gh.calls])
        self.assertNotIn("Opened", gh.comments[1000])

    def test_untrusted_commenter_ignored(self):
        gh = FakeGh(perm="read")
        self.assertEqual(run.main(self.env("issue_comment", "issue_comment"), gh), 0)
        self.assertEqual([c[0] for c in gh.calls], ["permission"])
        self.assertFalse((self.tmp / "prompt.txt").exists())

    def test_outside_association_refused_without_api_call(self):
        path = self.tmp / "outsider.json"
        event = fixture("issue_comment")
        event["comment"]["author_association"] = "NONE"
        path.write_text(json.dumps(event))
        gh = FakeGh(perm="admin")
        self.assertEqual(run.main(self.env("issue_comment", "issue_comment", GITHUB_EVENT_PATH=str(path)), gh), 0)
        self.assertEqual(gh.calls, [])
        self.assertFalse((self.tmp / "prompt.txt").exists())

    def test_collaborator_association_still_needs_permission(self):
        path = self.tmp / "collab.json"
        event = fixture("issue_comment")
        event["comment"]["author_association"] = "COLLABORATOR"
        path.write_text(json.dumps(event))
        gh = FakeGh(perm="read")
        self.assertEqual(run.main(self.env("issue_comment", "issue_comment", GITHUB_EVENT_PATH=str(path)), gh), 0)
        self.assertEqual([c[0] for c in gh.calls], ["permission"])
        self.assertFalse((self.tmp / "prompt.txt").exists())

    def test_pull_request_review_is_read_only(self):
        gh = FakeGh()
        # Fake `gh pr diff` is on FakeGh; main never shells out to gh for reviews.
        self.assertEqual(run.main(self.env("pull_request", "pull_request"), gh), 0)
        reviews = [c for c in gh.calls if c[0] == "api" and c[1].endswith("/reviews")]
        self.assertEqual(len(reviews), 1)
        self.assertEqual(reviews[0][3]["comments"][0]["line"], 3)
        args = (self.tmp / "args.txt").read_text()
        self.assertIn("--disallowed-tools Browser,Bash,Task,Skill,ExitPlanMode,WebFetch,WebSearch,Edit,Write,MultiEdit,NotebookEdit", args)
        self.assertIn("<diff>", (self.tmp / "review-prompt.txt").read_text())

    def test_pr_comment_pushes_to_branch(self):
        gh = FakeGh()
        # Simulate `gh pr checkout` by creating the branch locally.
        real_run = subprocess.run

        def runner(cmd, **kw):
            if cmd[:3] == ["gh", "pr", "checkout"]:
                return real_run(["git", "checkout", "-q", "-b", "feature-x"], **kw)
            return real_run(cmd, **kw)

        ev = fixture("issue_comment")
        ev["issue"]["pull_request"] = {"url": "x"}
        path = self.tmp / "event-pr.json"
        path.write_text(json.dumps(ev))
        env = self.env("issue_comment", "issue_comment", GITHUB_EVENT_PATH=str(path))
        os.environ["STUB_EDIT"] = "1"
        try:
            code = run.main(env, gh, runner=runner)
        finally:
            del os.environ["STUB_EDIT"]
        self.assertEqual(code, 0)
        self.assertIn("Pushed 1 changed file(s) to `feature-x`", gh.comments[1000])
        log = subprocess.run(["git", "--git-dir", str(self.origin), "log", "--format=%an %s", "feature-x"], capture_output=True, text=True).stdout
        self.assertIn("github-actions[bot] Apply changes requested in #7", log)

    def test_fork_pr_not_pushed(self):
        gh = FakeGh(same_repo=False)
        real_run = subprocess.run

        def runner(cmd, **kw):
            if cmd[:3] == ["gh", "pr", "checkout"]:
                return real_run(["git", "checkout", "-q", "-b", "fork-branch"], **kw)
            return real_run(cmd, **kw)

        ev = fixture("issue_comment")
        ev["issue"]["pull_request"] = {"url": "x"}
        path = self.tmp / "event-pr.json"
        path.write_text(json.dumps(ev))
        os.environ["STUB_EDIT"] = "1"
        try:
            run.main(self.env("issue_comment", "issue_comment", GITHUB_EVENT_PATH=str(path)), gh, runner=runner)
        finally:
            del os.environ["STUB_EDIT"]
        self.assertEqual(len(gh.comments), 1)
        self.assertIn("forks", gh.comments[1000])
        self.assertFalse((self.tmp / "prompt.txt").exists())

    def test_unknown_command_replies_with_usage(self):
        path = self.tmp / "unknown.json"
        event = fixture("issue_comment")
        event["comment"]["body"] = "@chanto please deploy to prod"
        path.write_text(json.dumps(event))
        gh = FakeGh()
        self.assertEqual(run.main(self.env("issue_comment", "issue_comment", GITHUB_EVENT_PATH=str(path)), gh), 0)
        self.assertIn("@chanto fix <what>", gh.comments[1000])
        self.assertFalse((self.tmp / "prompt.txt").exists())


if __name__ == "__main__":
    unittest.main()
