#!/bin/sh
# Runs the OpenSSF Scorecard checks that only need the files in the
# repository, so a change that would lower the project's score is caught
# before it's pushed. The `scorecard guard` CI job runs this same script.
#
#   brew install osv-scanner zizmor scorecard
#   scripts/scorecard-check.sh                  # all three
#   scripts/scorecard-check.sh osv zizmor       # just these
#
# - osv: known vulnerabilities in every lockfile (Cargo, npm). This is what
#   Scorecard's Vulnerabilities check runs, and like it, it honours only the
#   osv-scanner.toml next to each lockfile; .cargo/audit.toml and deny.toml
#   don't count.
# - zizmor: the workflows, at medium severity or above: unpinned actions,
#   broad token permissions, dangerous triggers, template injection. With
#   GH_TOKEN or GITHUB_TOKEN set it also runs the audits that ask GitHub
#   (impostor commits, actions with known vulnerabilities).
# - scorecard: Scorecard itself in local mode, for Pinned-Dependencies,
#   Token-Permissions, Dangerous-Workflow and Binary-Artifacts. A score below
#   the floor fails.
#
# A tool that isn't installed is skipped with a hint, unless
# SCORECARD_CHECK_REQUIRE=1 (as in CI). Exits non-zero on any finding.
set -eu

cd "$(git rev-parse --show-toplevel)"
[ $# -gt 0 ] || set -- osv zizmor scorecard
require=${SCORECARD_CHECK_REQUIRE:-0}
status=0

# have <tool> <install hint>
have() {
    command -v "$1" >/dev/null 2>&1 && return 0
    if [ "$require" = 1 ]; then
        echo "error: $1 isn't installed ($2)" >&2
        status=1
    else
        echo "skip: $1 isn't installed ($2)" >&2
    fi
    return 1
}

run_osv() {
    have osv-scanner "brew install osv-scanner" || return 0
    if ! python3 scripts/check-advisory-exceptions.py; then
        status=1
        return 0
    fi
    echo "osv-scanner: scanning the lockfiles" >&2
    # Recursive, as Scorecard scans; files that git ignores (target/,
    # node_modules/) are skipped.
    if ! osv-scanner scan source --recursive . >&2; then
        echo "error: osv-scanner found a known vulnerability; update the dependency, or if it can't affect Chanto, add it with a reason to the osv-scanner.toml next to that lockfile" >&2
        status=1
    fi
}

run_zizmor() {
    have zizmor "brew install zizmor" || return 0
    mode=--offline
    if [ -n "${GH_TOKEN:-}${GITHUB_TOKEN:-}" ]; then mode=; fi
    echo "zizmor: auditing .github/workflows/ ${mode:-(online)}" >&2
    # shellcheck disable=SC2086 # $mode is empty or one flag
    if ! zizmor $mode --min-severity=medium --no-progress .github/workflows/ >&2; then
        echo "error: zizmor found a workflow problem; fix it, or if it's intended, add \`# zizmor: ignore[<audit>]\` with the reason on that line" >&2
        status=1
    fi
}

run_scorecard() {
    have scorecard "brew install scorecard" || return 0
    have python3 "python.org" || return 0
    # Scorecard reads every file under the directory, so it gets a copy of
    # what git would commit (tracked and new files, as they are on disk),
    # never build output.
    tree=$(mktemp -d)
    trap 'rm -rf "$tree"' EXIT
    git ls-files -co --exclude-standard | while IFS= read -r f; do
        [ -e "$f" ] && printf '%s\n' "$f"
    done | tar -cf - -T - | tar -xf - -C "$tree"
    echo "scorecard: local checks" >&2
    if ! scorecard --local "$tree" --format json --show-details \
        --checks Pinned-Dependencies,Token-Permissions,Dangerous-Workflow,Binary-Artifacts \
        >"$tree/.scorecard.json" 2>"$tree/.scorecard.log"; then
        cat "$tree/.scorecard.log" >&2
        echo "error: scorecard failed to run" >&2
        status=1
        return 0
    fi
    # The floor is 10 for every check.
    python3 - "$tree/.scorecard.json" <<'PY' || status=1
import json, sys

floor = {}
failed = False
for check in json.load(open(sys.argv[1]))["checks"]:
    name, score = check["name"], check["score"]
    want = floor.get(name, 10)
    ok = score >= want
    failed |= not ok
    print(f"{'ok  ' if ok else 'FAIL'} {name:20} {score:>2} / 10 (floor {want}): {check['reason']}", file=sys.stderr)
    if not ok:
        for line in check.get("details") or []:
            if line.startswith("Warn"):
                print(f"       {line}", file=sys.stderr)
sys.exit(1 if failed else 0)
PY
}

for check in "$@"; do
    case "$check" in
        osv) run_osv ;;
        zizmor) run_zizmor ;;
        scorecard) run_scorecard ;;
        *)
            echo "error: unknown check $check (osv, zizmor, scorecard)" >&2
            exit 2
            ;;
    esac
done
exit "$status"
