#!/usr/bin/env python3
"""Compare the published OpenSSF Scorecard result with the committed baseline.

Run by scorecard.yml after each analysis of main. The published result can lag
the analysis by a minute or two, so this waits until the API reports the commit
that was just analysed. Each check scoring below .github/scorecard-baseline.json
is listed, and the script exits 1 so the run fails and opens an issue. A check
scoring above its baseline is only reported: raise the baseline in a pull
request so the new score is kept.

Usage: scorecard-regressions.py <repo> <commit> [baseline]
"""
import json
import sys
import time
import urllib.request
from pathlib import Path

API = "https://api.securityscorecards.dev/projects/github.com/{repo}"


def published(repo, commit, attempts=30, wait=30):
    """The published result for `commit`, polling while the API catches up."""
    for i in range(attempts):
        try:
            with urllib.request.urlopen(API.format(repo=repo), timeout=30) as r:
                data = json.load(r)
            if data.get("repo", {}).get("commit") == commit:
                return data
            seen = data.get("repo", {}).get("commit", "?")[:12]
            print(f"published result is for {seen}, waiting for {commit[:12]} ({i + 1}/{attempts})")
        except OSError as e:
            print(f"fetching the published result failed: {e} ({i + 1}/{attempts})")
        time.sleep(wait)
    sys.exit(f"the published result never reached {commit[:12]}")


def main():
    repo, commit = sys.argv[1], sys.argv[2]
    baseline = json.loads(Path(sys.argv[3] if len(sys.argv) > 3 else ".github/scorecard-baseline.json").read_text())
    data = published(repo, commit)
    scores = {c["name"]: c for c in data["checks"]}
    drops, rises = [], []
    for name, floor in sorted(baseline.items()):
        check = scores.get(name)
        if check is None:
            drops.append(f"- **{name}**: missing from the result (baseline {floor})")
        elif check["score"] < floor:
            drops.append(f"- **{name}**: {check['score']} (baseline {floor}): {check.get('reason', '')}")
        elif check["score"] > floor:
            rises.append(f"- {name}: {check['score']} (baseline {floor})")
    print(f"overall score {data['score']} for {commit[:12]}")
    if rises:
        print("Higher than the baseline (raise it in .github/scorecard-baseline.json):")
        print("\n".join(rises))
    if drops:
        Path("scorecard-drops.md").write_text(
            f"Scorecard for {commit[:12]} scored below `.github/scorecard-baseline.json`:\n\n"
            + "\n".join(drops)
            + "\n\nDetails are in the Security tab's code-scanning alerts. If the drop is"
            " intended, lower the baseline in a pull request that says why.\n"
        )
        print("Below the baseline:")
        print("\n".join(drops))
        sys.exit(1)


if __name__ == "__main__":
    main()
