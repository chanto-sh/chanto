#!/usr/bin/env bash
# macOS: a project and CHANTO_HOME on a nearly full 30 MB disk image; the
# model overwrites a file with 2 MB. Passes when the file is left intact
# (the write fails) and no session temp files are left behind.
#   CARGO_TARGET_DIR=... tests/chaos/disk_full.sh
set -u
here="$(cd "$(dirname "$0")" && pwd)"
T="$(mktemp -d "${CHAOS_TMP:-${TMPDIR:-/tmp}}/diskfull.XXXXXX")"
F="$T/vol"; mkdir -p "$F"
hdiutil create -size 30m -fs HFS+ -volname chaosfull -quiet "$T/full.dmg" || exit 2
hdiutil attach "$T/full.dmg" -mountpoint "$F" -nobrowse -quiet || exit 2
trap 'hdiutil detach "$F" -quiet -force; rm -rf "$T"' EXIT
mkdir -p "$F/w/proj" "$F/w/hh"
printf 'mode = "acceptEdits"\n[feedback]\nshare_errors = false\n' > "$F/w/hh/config.toml"
(cd "$F/w/proj" && git init -q && python3 -c "print('important line\n' * 2000, end='')" > important.txt && git add . && git -c user.email=t@t -c user.name=t commit -qm i)
before="$(md5 -q "$F/w/proj/important.txt")"
python3 - "$here/scenarios/disk_full.json" "$T/df.json" <<'EOF'
import json, sys
t = json.load(open(sys.argv[1]))
t[1]["tool_calls"][0]["args"]["content"] = "x" * (2 * 1024 * 1024)
t = [x for x in t if not any(c["name"] == "Bash" for c in x.get("tool_calls", []))]
json.dump(t, open(sys.argv[2], "w"))
EOF
avail=$(df -k "$F" | tail -1 | awk '{print $4}')
dd if=/dev/zero of="$F/filler" bs=1k count=$((avail - 300)) 2>/dev/null
HH="$F/w/hh" PROJ="$F/w/proj" TIMEOUT=60 "$here/run.sh" "$T/df.json" -p go >/dev/null 2>&1
fail=0
[ "$(md5 -q "$F/w/proj/important.txt")" = "$before" ] && echo "PASS file intact" || { echo "FAIL important.txt changed on a full disk"; fail=1; }
ls "$F/w/hh/sessions/"*/ 2>/dev/null | grep -q '\.tmp$' && { echo "FAIL session temp file left behind"; fail=1; } || echo "PASS no temp files"
exit $fail
