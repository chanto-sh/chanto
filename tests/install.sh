#!/bin/sh
# Offline installer integration tests: all downloads go to a localhost fixture.
set -eu
root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
server_pid=
trap '[ -z "$server_pid" ] || kill "$server_pid"; rm -rf "$tmp"' EXIT HUP INT TERM
export HOME="$tmp/home" CHANTO_HOME="$tmp/home/.chanto"
mkdir -p "$HOME" "$tmp/releases/download/v9.8.7" "$tmp/releases/latest/download" "$tmp/mock"
cat > "$tmp/mock/uname" <<'SH'
#!/bin/sh
case "$1" in -s) echo "$TEST_OS";; -m) echo "$TEST_ARCH";; esac
SH
chmod +x "$tmp/mock/uname"
export PATH="$tmp/mock:$PATH"
for target in aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
    name=chanto-v9.8.7-$target
    mkdir -p "$tmp/package/$name"
    printf '#!/bin/sh\necho %s\n' "$target" > "$tmp/package/$name/chanto"
    tar -czf "$tmp/releases/download/v9.8.7/$name.tar.gz" -C "$tmp/package" "$name"
done
(cd "$tmp/releases/download/v9.8.7" && shasum -a 256 ./*.tar.gz | sed 's|  ./|  |' > SHA256SUMS)
cp "$tmp/releases/download/v9.8.7/SHA256SUMS" "$tmp/releases/latest/download/SHA256SUMS"
python3 - "$tmp/releases" "$tmp/port" <<'PY' &
import functools, http.server, pathlib, sys
class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args): pass
server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), functools.partial(QuietHandler, directory=sys.argv[1]))
pathlib.Path(sys.argv[2]).write_text(str(server.server_port))
server.serve_forever()
PY
server_pid=$!
while [ ! -s "$tmp/port" ]; do sleep 0.05; done
CHANTO_RELEASE_BASE_URL="http://127.0.0.1:$(cat "$tmp/port")"
export CHANTO_RELEASE_BASE_URL
for row in 'Darwin arm64 aarch64-apple-darwin' 'Darwin x86_64 x86_64-apple-darwin' 'Linux aarch64 aarch64-unknown-linux-musl' 'Linux amd64 x86_64-unknown-linux-musl' 'Linux x86_64 x86_64-unknown-linux-musl' 'Linux arm64 aarch64-unknown-linux-musl'; do
    # shellcheck disable=SC2086 # Each row deliberately contains three fields.
    set -- $row
    export TEST_OS="$1" TEST_ARCH="$2"
    sh "$root/install.sh" --prefix "$tmp/dest" > "$tmp/log"
    [ "$("$tmp/dest/bin/chanto")" = "$3" ]
    sh "$root/install.sh" --version v9.8.7 --prefix "$tmp/dest" > "$tmp/log"
done
# A restricted PATH proves the wget branch works without curl installed.
mkdir -p "$tmp/wget-only"
for tool in sh awk sed tar mktemp rm mkdir cp chmod dirname python3 shasum; do
    ln -s "$(command -v "$tool")" "$tmp/wget-only/$tool"
done
cp "$tmp/mock/uname" "$tmp/wget-only/uname"
cat > "$tmp/wget-only/wget" <<'PY'
#!/usr/bin/env python3
import sys, urllib.request
urllib.request.urlretrieve(sys.argv[2], sys.argv[4])
PY
chmod +x "$tmp/wget-only/wget"
TEST_OS=Linux TEST_ARCH=x86_64 PATH="$tmp/wget-only" sh "$root/install.sh" --prefix "$tmp/wget-dest" > "$tmp/log"
[ "$("$tmp/wget-dest/bin/chanto")" = x86_64-unknown-linux-musl ]
export TEST_OS=Linux TEST_ARCH=riscv64
if sh "$root/install.sh" --prefix "$tmp/dest" > "$tmp/log" 2>&1; then exit 1; fi
rg 'unsupported architecture' "$tmp/log" >/dev/null
export TEST_ARCH=x86_64
before=$("$tmp/dest/bin/chanto")
printf 'corrupted archive' >> "$tmp/releases/download/v9.8.7/chanto-v9.8.7-x86_64-unknown-linux-musl.tar.gz"
if sh "$root/install.sh" --prefix "$tmp/dest" > "$tmp/log" 2>&1; then exit 1; fi
rg 'checksum mismatch' "$tmp/log" >/dev/null
[ "$("$tmp/dest/bin/chanto")" = "$before" ]
# Exercise the retained source path with a fake Cargo, without a release build.
mkdir -p "$tmp/source" "$tmp/source-tools"
cp "$root/install.sh" "$tmp/source/install.sh"
cat > "$tmp/source-tools/cargo" <<'SH'
#!/bin/sh
[ "$*" = 'build --release --locked' ] || exit 1
mkdir -p target/release
printf '#!/bin/sh\necho from-source\n' > target/release/chanto
SH
chmod +x "$tmp/source-tools/cargo"
PATH="$tmp/source-tools:$PATH" sh "$tmp/source/install.sh" --from-source --prefix "$tmp/source-dest" > "$tmp/log"
[ "$("$tmp/source-dest/bin/chanto")" = from-source ]
python3 "$root/packaging/homebrew/render.py" v9.8.7 "$tmp/releases/latest/download/SHA256SUMS" "$tmp/chanto.rb"
if rg '@VERSION@|@SHA_' "$tmp/chanto.rb"; then exit 1; fi
printf '%s\n' 'installer: six OS/arch mappings, curl/wget, pinned/latest releases, mismatch preservation, formula rendering passed'
