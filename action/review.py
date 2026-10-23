"""Structured PR reviews. Standard library only; GitHub I/O lives in run.Gh."""
from __future__ import annotations

import fnmatch
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import tomllib

MARKER = "<!-- chanto:review-summary -->"
SEVERITIES = {"critical": 0, "high": 1, "medium": 2, "low": 3}
SCHEMA = '''Return ONLY JSON: {"checked": ["short description"], "findings": [
{"path": "relative/file", "line": 12, "severity": "high", "title": "Specific bug",
"why": "Why this is wrong", "scenario": "Concrete failing input or execution",
"confidence": 0.95, "replacement": "optional replacement of this single line"}]}.
Report few high-confidence correctness or security bugs, not style preferences.
Lines must be RIGHT-side lines in the diff. Omit replacement unless a mechanical
single-line fix is sufficient. Never claim a fix is verified; the runner checks it.'''


def config(root=Path("."), ref=None, runner=subprocess.run):
    """Read policy from the trusted base, never from a PR's edited policy."""
    def read(path):
        if ref:
            r = runner(["git", "-c", "core.hooksPath=/dev/null", "show", f"{ref}:{path}"], capture_output=True, text=True)
            return r.stdout if r.returncode == 0 else ""
        p = root / path
        return p.read_text() if p.is_file() else ""
    raw = read(".chanto/review.toml")
    cfg = tomllib.loads(raw) if raw else {}
    if not raw:
        raw = read(".chanto/config.toml")
        cfg = tomllib.loads(raw).get("review", {}) if raw else {}
    cfg.setdefault("strictness", "high")
    if cfg["strictness"] not in ("high", "normal", "all"):
        raise ValueError("strictness must be high, normal, or all")
    cfg["max_inline_comments"] = max(0, min(10, int(cfg.get("max_inline_comments", 10))))
    for key in ("ignore_paths", "checks", "allowed_domains"):
        if key in cfg and (not isinstance(cfg[key], list) or not all(isinstance(x, str) for x in cfg[key])):
            raise ValueError(f"{key} must be a list of strings")
    if "checks" not in cfg:
        checks = []
        if read("Cargo.toml"):
            checks.append("cargo test -j 2 --quiet")
        if read("go.mod"):
            checks.append("go vet ./... && go test ./...")
        package = read("package.json")
        if package:
            scripts = json.loads(package).get("scripts", {})
            if scripts.get("test") and "no test specified" not in scripts["test"]:
                checks.append("npm test --silent")
            if scripts.get("typecheck"):
                checks.append("npm run typecheck --silent")
        if any(read(p) for p in ("pyproject.toml", "pytest.ini", "tox.ini")):
            checks.append("python3 -m pytest -q -x")
        cfg["checks"] = checks
    cfg["instructions"] = str(cfg.get("instructions", "")) + "\n" + "\n".join(read(p) for p in ("REVIEW.md", "AGENTS.md"))
    return cfg


def diff_lines(diff):
    """Map new-file paths to RIGHT-side reviewable lines (context + additions)."""
    result, path, line = {}, None, None
    for text in diff.splitlines():
        if text.startswith("diff --git "):
            path, line = None, None
        elif text.startswith("+++ "):
            name = text[4:]
            if name.startswith('"'):
                try:
                    name = json.loads(name)
                except ValueError:
                    continue
            path = name[2:] if name.startswith("b/") else None
            if path:
                result.setdefault(path, set())
            line = None
        elif text.startswith("@@ "):
            m = re.match(r"@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@", text)
            line = int(m[1]) if m else None
        elif path and line is not None and text.startswith(("+", " ")):
            result[path].add(line)
            line += 1
    return result


def extract_json(text):
    """The review object, tolerating a code fence or prose around it."""
    text = text.strip()
    try:
        return json.loads(text)
    except ValueError:
        pass
    fenced = re.search(r"```(?:json)?\s*\n(.*?)\n```", text, re.S)
    if fenced:
        try:
            return json.loads(fenced[1])
        except ValueError:
            pass
    start, end = text.find("{"), text.rfind("}")
    if start < 0 or end < start:
        raise ValueError("no JSON object in the review")
    return json.loads(text[start:end + 1])


def parse_result(text, diff, cfg):
    data = extract_json(text)
    if not isinstance(data, dict) or not isinstance(data.get("findings"), list):
        raise ValueError("review must contain a findings array")
    mapping = diff_lines(diff)
    threshold = {"high": .9, "normal": .75, "all": .6}[cfg["strictness"]]
    findings, seen = [], set()
    for f in data["findings"]:
        if not isinstance(f, dict):
            continue
        path, line = f.get("path"), f.get("line")
        if not isinstance(path, str) or type(line) is not int or line not in mapping.get(path, set()):
            continue
        if Path(path).is_absolute() or ".." in Path(path).parts or any(fnmatch.fnmatchcase(path, p) for p in cfg.get("ignore_paths", [])):
            continue
        if f.get("severity") not in SEVERITIES or not all(isinstance(f.get(k), str) and f[k].strip() for k in ("title", "why", "scenario")):
            continue
        confidence = f.get("confidence", 0)
        if type(confidence) not in (float, int) or not threshold <= confidence <= 1:
            continue
        source = f
        f = {k: f[k] for k in ("path", "line", "severity", "title", "why", "scenario", "confidence")}
        if isinstance(source.get("replacement"), str) and "\n" not in source["replacement"] and "\r" not in source["replacement"]:
            f["replacement"] = source["replacement"]
        f["verified"] = False
        key = fingerprint(f)
        if key not in seen:
            findings.append(f)
            seen.add(key)
    findings.sort(key=lambda f: (SEVERITIES[f["severity"]], f["path"], f["line"]))
    checked = data.get("checked", [])
    return {"findings": findings, "checked": [x[:300] for x in checked if isinstance(x, str)][:20] if isinstance(checked, list) else []}


def fingerprint(f):
    # Exclude line so unchanged issues aren't repeated when preceding code shifts.
    return hashlib.sha256((f['path'] + '\0' + f['title'].strip().lower() + '\0' + f['scenario'].strip().lower()).encode()).hexdigest()[:24]


def suggestion(text):
    # GitHub requires exactly three backticks; reject content that could close it.
    if "```" in text or "\r" in text:
        raise ValueError("replacement contains a suggestion fence")
    return "```suggestion\n" + text + "\n```"


def finding_body(f):
    body = f"**{f['severity'].upper()}: {f['title']}**\n\n{f['why']}\n\nFailing scenario: {f['scenario']}\n\n"
    body += "verified fix" if f.get("verified") else "not verified"
    if f.get("verified"):
        body += "\n\n" + suggestion(f["replacement"])
    return body + f"\n<!-- chanto:finding:{fingerprint(f)} -->"


def summary(data, sha):
    lines = [MARKER, f"## chanto review\n\nCommit `{sha}`"]
    for f in data["findings"]:
        lines.append(f"- **{f['severity'].upper()}** `{f['path']}:{f['line']}` — {f['title']}: {f['why']} Failing scenario: {f['scenario']} ({'verified fix' if f.get('verified') else 'not verified'})")
    if not data["findings"]:
        lines.append("No high-confidence findings in the reviewed diff.")
    lines.append("\nWhat I checked:\n" + "\n".join(f"- {x}" for x in data["checked"]))
    return "\n\n".join(lines)[:60000]


def is_bot(comment):
    """Comments the Action may own: github-actions or a GitHub App token's bot.

    Humans can quote the markers, so only bot accounts count; otherwise anyone
    could hijack the summary or suppress findings by pasting a marker."""
    user = comment.get("user") or {}
    login = user.get("login", "")
    return login == "github-actions[bot]" or (user.get("type") == "Bot" and login.endswith("[bot]"))


def publish(gh, number, sha, data, cfg):
    comments = gh.pages(f"repos/{gh.repo}/issues/{number}/comments")
    owned = [c for c in comments if is_bot(c) and MARKER in c.get("body", "")]
    body = summary(data, sha)
    if owned:
        gh.api(f"repos/{gh.repo}/issues/comments/{owned[0]['id']}", "PATCH", {"body": body})
    else:
        gh.api(f"repos/{gh.repo}/issues/{number}/comments", "POST", {"body": body})
    previous = gh.pages(f"repos/{gh.repo}/pulls/{number}/comments")
    bodies = "\n".join(c.get("body", "") for c in previous if is_bot(c))
    inline = [{"path": f["path"], "line": f["line"], "side": "RIGHT", "body": finding_body(f)} for f in data["findings"] if f"<!-- chanto:finding:{fingerprint(f)} -->" not in bodies][:cfg["max_inline_comments"]]
    if inline:
        gh.api(f"repos/{gh.repo}/pulls/{number}/reviews", "POST", {"commit_id": sha, "event": "COMMENT", "body": "Concrete findings from chanto.", "comments": inline})


def run_checks(env, cfg, runner=subprocess.run):
    """Independent check execution through chanto's sandbox, with no API keys."""
    checks = cfg.get("checks", [])
    if not checks:
        return False  # An empty gate cannot certify a fix.
    from run import agent_env, SECRET_ENV
    child = agent_env(env)
    # Check processes do not need workflow inputs or arbitrary job secrets.
    safe = {"PATH", "HOME", "CHANTO_HOME", "CHANTO_BIN", "LANG", "LC_ALL", "TZ", "TMPDIR", "TMP", "TEMP", "SYSTEMROOT", "WINDIR", "USERPROFILE", "GIT_CONFIG_NOSYSTEM"}
    child = {k: v for k, v in child.items() if k in safe}
    child["PYTHONDONTWRITEBYTECODE"] = "1"
    for key in SECRET_ENV:
        child.pop(key, None)
    child.pop("INPUT_PROVIDER_CONFIG", None)
    for command in checks:
        r = runner([env.get("CHANTO_BIN", "chanto"), "--sandbox", "workspace-write", "--run-tool", "Bash", json.dumps({"command": command, "timeout": 600000})], capture_output=True, text=True, env=child, timeout=660)
        if r.returncode or not r.stdout.startswith("is_error: false\n"):
            return False
    return True


def verify_fixes(data, env, cfg, runner=subprocess.run):
    """Each candidate is checked in isolation; nothing modifies the PR checkout."""
    from run import git, run_chanto
    if not cfg.get("checks"):
        return
    root = Path.cwd()
    for f in data["findings"][:cfg["max_inline_comments"]]:
        name = Path(f["path"]).name
        if "replacement" not in f or name.startswith("test_") or name.endswith(("_test.py", ".test.js", ".test.ts")) or "tests" in Path(f["path"]).parts:
            continue
        try:
            suggestion(f["replacement"])
        except ValueError:
            continue
        with tempfile.TemporaryDirectory(prefix="chanto-fix-") as tmp:
            scratch = Path(tmp).resolve() / "worktree"
            git("worktree", "add", "--detach", str(scratch), "HEAD", runner=runner)
            try:
                os.chdir(scratch)
                target = scratch / f["path"]
                if target.is_symlink() or not target.resolve().is_relative_to(scratch):
                    continue
                original = target.read_text()
                lines = original.splitlines(keepends=True)
                if f["line"] > len(lines):
                    continue
                lines[f["line"] - 1] = f["replacement"] + ("\n" if lines[f["line"] - 1].endswith("\n") else "")
                expected = "".join(lines)
                target.write_text(expected)
                # Verified mode also protects existing tests; edits are disabled so
                # the model cannot widen the candidate or weaken the check suite.
                _, ok = run_chanto("Check the existing mechanical fix. Do not edit files. Report the checks. If any fail, report failure.", env, "plan", read_only=True, runner=runner, verify=True)
                changes = git("diff", "--name-only", runner=runner).splitlines()
                f["verified"] = ok and changes == [f["path"]] and target.read_text() == expected and run_checks(env, cfg, runner)
            except (OSError, RuntimeError, subprocess.TimeoutExpired):
                f["verified"] = False
            finally:
                os.chdir(root)
                git("worktree", "remove", "--force", str(scratch), runner=runner)
