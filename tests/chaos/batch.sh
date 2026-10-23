#!/usr/bin/env bash
# Run each CASE of a scenario file through run.sh in -p mode and summarise.
#   tests/chaos/batch.sh SCENARIO.py CASE... [-- Chanto args]
here="$(cd "$(dirname "$0")" && pwd)"
scen="$1"; shift
cases=(); while [ $# -gt 0 ] && [ "$1" != "--" ]; do cases+=("$1"); shift; done
[ "${1:-}" = "--" ] && shift
for c in "${cases[@]}"; do
  start=$(python3 -c 'import time;print(time.time())')
  out="$(CASE="$c" KEEP=1 "$here/run.sh" "$scen" "$@" 2>&1)"; rc=$?
  dur=$(python3 -c "import time;print(round(time.time()-$start,1))")
  W="$(printf '%s\n' "$out" | sed -n 's/^W=\([^ ]*\) .*/\1/p' | head -1)"
  reqs=$(wc -l < "$W/model.log" 2>/dev/null | tr -d ' ')
  printf '=== %-20s rc=%s %ss reqs=%s bytes=%s\n' "$c" "$rc" "$dur" "$reqs" "$(printf '%s' "$out" | wc -c | tr -d ' ')"
  printf '%s\n' "$out" | grep -v '^W=' | cut -c1-240 | tail -${TAILN:-6} | sed 's/^/    /'
  [ -n "${KEEPW:-}" ] || rm -rf "$W"
done
