#!/bin/sh
# Runs CodeQL on this machine with the query suite GitHub's code scanning uses
# for this repository (security-extended), so a change to credentials,
# permissions or `unsafe` code can be checked before it's pushed. Pull requests
# are scanned anyway, and one that adds a new alert can't merge.
#
#   brew install --cask codeql      # or a release from github.com/github/codeql-cli-binaries
#   scripts/codeql.sh               # Rust: about 10 minutes
#   scripts/codeql.sh rust javascript actions python
#
# Prints each finding and exits non-zero if there are any; the SARIF files are
# in target/codeql/ for editors that read them.
set -eu

command -v codeql >/dev/null 2>&1 || {
    echo "error: codeql isn't installed (brew install --cask codeql)" >&2
    exit 2
}
cd "$(git rev-parse --show-toplevel)"
out=target/codeql
mkdir -p "$out"
[ $# -gt 0 ] || set -- rust

# Only Chanto's own sources: build output holds generated code, and
# .claude/worktrees holds whole extra checkouts of the repository.
config="$out/config.yml"
printf 'paths-ignore:\n  - target\n  - fuzz/target\n  - .claude\n  - "**/node_modules"\n' >"$config"

found=0
for lang in "$@"; do
    db="$out/db-${lang}"
    sarif="$out/${lang}.sarif"
    log="$out/${lang}-database.log"
    echo "codeql: building the ${lang} database (log: ${log})" >&2
    codeql database create "$db" --language="$lang" --build-mode=none \
        --source-root=. --codescanning-config="$config" --overwrite >"$log" 2>&1 || {
        tail -20 "$log" >&2
        exit 2
    }
    echo "codeql: analyzing ${lang}" >&2
    codeql database analyze "$db" \
        "codeql/$lang-queries:codeql-suites/$lang-security-extended.qls" \
        --download --format=sarif-latest --output="$sarif" --quiet
    n=$(python3 - "$sarif" <<'PY'
import json, sys
results = [r for run in json.load(open(sys.argv[1]))["runs"] for r in run["results"]]
for r in results:
    loc = r["locations"][0]["physicalLocation"]
    where = f'{loc["artifactLocation"]["uri"]}:{loc.get("region", {}).get("startLine", "?")}'
    print(f'{r.get("level", "warning"):8} {r["ruleId"]:40} {where}', file=sys.stderr)
    print(f'         {r["message"]["text"].splitlines()[0][:150]}', file=sys.stderr)
print(len(results))
PY
)
    echo "codeql: ${n} ${lang} finding(s); SARIF in ${sarif}" >&2
    found=$((found + n))
done
[ "$found" -eq 0 ]
