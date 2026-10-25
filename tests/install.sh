#!/bin/sh
# Offline installer integration tests: all downloads go to a localhost fixture.
set -eu
root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
server_pid=
trap '[ -z "$server_pid" ] || kill "$server_pid"; rm -rf "$tmp"' EXIT HUP INT TERM
export HOME="$tmp/home" CHANTO_HOME="$tmp/home/.chanto"
mkdir -p "$HOME" "$tmp/mock"
cat > "$tmp/mock/uname" <<'SH'
#!/bin/sh
case "$1" in -s) echo "$TEST_OS";; -m) echo "$TEST_ARCH";; esac
SH
chmod +x "$tmp/mock/uname"
export PATH="$tmp/mock:$PATH"
sh "$root/tests/install-fixture.sh" "$tmp"
ossl=$(cat "$tmp/openssl")
export CHANTO_RELEASE_KEY_FILE="$tmp/test-release.pub"
CHANTO_RELEASE_SSH_KEY=$(cat "$tmp/test-release.ssh")
export CHANTO_RELEASE_SSH_KEY
# The keys the installers pin are the release key in packaging/, and both
# scripts hold the same rollback floor.
pem=$(sed -n 2p "$root/packaging/release-signing-key.pub")
ssh=$(cut -d' ' -f2- "$root/packaging/release-allowed-signers")
for f in install.sh install.ps1; do
    grep -q -F "$pem" "$root/$f"
    grep -q -F "$ssh" "$root/$f"
done
floor=$(sed -n 's/^ *min_version=\(v[0-9.]*\)$/\1/p' "$root/install.sh")
[ -n "$floor" ]
grep -q -F "\$minVersion = '$floor'" "$root/install.ps1"
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
for tool in sh awk sed tar gzip mktemp rm mkdir cp chmod dirname python3 shasum; do
    ln -s "$(command -v "$tool")" "$tmp/wget-only/$tool"
done
ln -s "$(command -v "$ossl")" "$tmp/wget-only/openssl"
cp "$tmp/mock/uname" "$tmp/wget-only/uname"
cat > "$tmp/wget-only/wget" <<'PY'
#!/usr/bin/env python3
import sys, urllib.request
urllib.request.urlretrieve(sys.argv[2], sys.argv[4])
PY
chmod +x "$tmp/wget-only/wget"
TEST_OS=Linux TEST_ARCH=x86_64 PATH="$tmp/wget-only" sh "$root/install.sh" --prefix "$tmp/wget-dest" > "$tmp/log"
[ "$("$tmp/wget-dest/bin/chanto")" = x86_64-unknown-linux-musl ]
# Without OpenSSL 3, ssh-keygen checks SHA256SUMS.sshsig instead.
nossl=/nonexistent/openssl
if command -v ssh-keygen >/dev/null 2>&1; then
    CHANTO_OPENSSL=$nossl TEST_OS=Linux TEST_ARCH=x86_64 sh "$root/install.sh" --prefix "$tmp/ssh-dest" > "$tmp/log"
    [ "$("$tmp/ssh-dest/bin/chanto")" = x86_64-unknown-linux-musl ]
    sshkg=checked
fi
# With neither verifier, nothing is installed.
mkdir -p "$tmp/no-verifier"
for tool in sh awk sed tar gzip mktemp rm mkdir cp chmod dirname curl shasum uname; do
    ln -s "$(command -v "$tool")" "$tmp/no-verifier/$tool"
done
if CHANTO_OPENSSL=$nossl TEST_OS=Linux TEST_ARCH=x86_64 PATH="$tmp/no-verifier" sh "$root/install.sh" --prefix "$tmp/nv-dest" > "$tmp/log" 2>&1; then exit 1; fi
grep -q 'cannot check the release signature' "$tmp/log"
[ ! -e "$tmp/nv-dest/bin/chanto" ]
# A changed SHA256SUMS or a missing signature stops the install, with either
# verifier, for a pinned version and for latest. A forged "latest" can't pick
# the version or the binary: a release listing an older, unsigned version with
# the attacker's checksum is refused before its archive is fetched.
forge() {
    printf '%s  chanto-v0.1.1-x86_64-unknown-linux-musl.tar.gz\n' \
        0000000000000000000000000000000000000000000000000000000000000000 > "$1/SHA256SUMS"
}
kept=$("$tmp/dest/bin/chanto")
for d in download/v9.8.7 latest/download; do
    sums="$tmp/releases/$d/SHA256SUMS"
    cp "$sums" "$tmp/sums.orig"
    forge "$tmp/releases/$d"
    for o in "" $nossl; do
        [ -z "$o" ] || [ -n "${sshkg:-}" ] || continue
        if [ "$d" = latest/download ]; then set --; else set -- --version v9.8.7; fi
        if CHANTO_OPENSSL=$o TEST_OS=Linux TEST_ARCH=x86_64 sh "$root/install.sh" "$@" --prefix "$tmp/dest" > "$tmp/log" 2>&1; then exit 1; fi
        grep -q 'signature check failed' "$tmp/log"
        for sig in SHA256SUMS.sig SHA256SUMS.sshsig; do
            mv "$tmp/releases/$d/$sig" "$tmp/sig.orig"
            if CHANTO_OPENSSL=$o TEST_OS=Linux TEST_ARCH=x86_64 sh "$root/install.sh" "$@" --prefix "$tmp/dest" > "$tmp/log" 2>&1; then
                status=0
            else
                status=1
            fi
            mv "$tmp/sig.orig" "$tmp/releases/$d/$sig"
            # Only the signature the chosen verifier reads matters.
            case "$o:$sig" in :SHA256SUMS.sig | ?*:SHA256SUMS.sshsig)
                [ "$status" = 1 ]
                grep -q -e 'has no SHA256SUMS.sig,' -e 'has no SHA256SUMS.sshsig,' -e 'predates SSH signatures' "$tmp/log" ;;
            esac
        done
    done
    cp "$tmp/sums.orig" "$sums"
done
[ "$("$tmp/dest/bin/chanto")" = "$kept" ]
# The rollback floor: a genuinely signed "latest" older than the floor is
# refused; asked for by name, it installs with a note. Unsigned versions
# (before v0.1.2) are refused up front, with no advice to install OpenSSL.
cp "$tmp/releases/download/v0.1.3/SHA256SUMS"* "$tmp/releases/latest/download/"
for o in "" $nossl; do
    [ -z "$o" ] || [ -n "${sshkg:-}" ] || continue
    if CHANTO_OPENSSL=$o TEST_OS=Linux TEST_ARCH=x86_64 sh "$root/install.sh" --prefix "$tmp/old" > "$tmp/log" 2>&1; then exit 1; fi
    grep -q 'refusing a possible rollback' "$tmp/log"
    [ ! -e "$tmp/old" ]
    CHANTO_OPENSSL=$o TEST_OS=Linux TEST_ARCH=x86_64 sh "$root/install.sh" --version v0.1.3 --prefix "$tmp/old" > "$tmp/log" 2>&1
    grep -q 'v0.1.3 is older than' "$tmp/log"
    rm -rf "$tmp/old"
    if CHANTO_OPENSSL=$o TEST_OS=Linux TEST_ARCH=x86_64 sh "$root/install.sh" --version v0.1.1 --prefix "$tmp/old" > "$tmp/log" 2>&1; then exit 1; fi
    grep -q 'predates signed releases' "$tmp/log"
    if grep -q 'OpenSSL' "$tmp/log"; then exit 1; fi
done
# Without OpenSSL, a signed release from before SSH signatures asks for
# OpenSSL; one that should have an SSH signature but doesn't is refused.
if [ -n "${sshkg:-}" ]; then
    mv "$tmp/releases/download/v0.1.3/SHA256SUMS.sshsig" "$tmp/v0.1.3.sshsig"
    rm "$tmp/releases/latest/download/SHA256SUMS.sshsig"
    if CHANTO_OPENSSL=$nossl TEST_OS=Linux TEST_ARCH=x86_64 sh "$root/install.sh" --version v0.1.3 --prefix "$tmp/old" > "$tmp/log" 2>&1; then exit 1; fi
    grep -q 'predates SSH signatures' "$tmp/log"
    if CHANTO_OPENSSL=$nossl TEST_OS=Linux TEST_ARCH=x86_64 sh "$root/install.sh" --prefix "$tmp/old" > "$tmp/log" 2>&1; then exit 1; fi
    grep -q 'predates SSH signatures' "$tmp/log"
    cp "$tmp/releases/download/v9.8.7/SHA256SUMS"* "$tmp/releases/latest/download/"
    rm "$tmp/releases/latest/download/SHA256SUMS.sshsig"
    if CHANTO_OPENSSL=$nossl TEST_OS=Linux TEST_ARCH=x86_64 sh "$root/install.sh" --prefix "$tmp/old" > "$tmp/log" 2>&1; then exit 1; fi
    grep -q 'which every release from v0.1.5 on has' "$tmp/log"
    mv "$tmp/v0.1.3.sshsig" "$tmp/releases/download/v0.1.3/SHA256SUMS.sshsig"
fi
cp "$tmp/releases/download/v9.8.7/SHA256SUMS"* "$tmp/releases/latest/download/"
export TEST_OS=Linux TEST_ARCH=riscv64
if sh "$root/install.sh" --prefix "$tmp/dest" > "$tmp/log" 2>&1; then exit 1; fi
grep -q 'unsupported architecture' "$tmp/log"
export TEST_ARCH=x86_64
before=$("$tmp/dest/bin/chanto")
printf 'corrupted archive' >> "$tmp/releases/download/v9.8.7/chanto-v9.8.7-x86_64-unknown-linux-musl.tar.gz"
if sh "$root/install.sh" --prefix "$tmp/dest" > "$tmp/log" 2>&1; then exit 1; fi
grep -q 'checksum mismatch' "$tmp/log"
[ "$("$tmp/dest/bin/chanto")" = "$before" ]
# install.ps1, where PowerShell runs on x64 (it installs only the x64 build):
# both verifiers, a forged "latest", and no verifier at all.
if command -v pwsh >/dev/null 2>&1 &&
    [ "$(pwsh -NoProfile -c '[Runtime.InteropServices.RuntimeInformation]::OSArchitecture' 2>/dev/null)" = X64 ]; then
    # A script rather than a function: sh keeps `VAR=x function` assignments.
    printf '#!/bin/sh\nexec "%s" -NoProfile -NonInteractive -File "%s" "$@"\n' "$(command -v pwsh)" "$root/install.ps1" > "$tmp/ps1"
    chmod +x "$tmp/ps1"
    "$tmp/ps1" -Prefix "$tmp/ps" > "$tmp/log"
    [ "$(cat "$tmp/ps/bin/chanto.exe")" = "windows v9.8.7" ]
    if [ -n "${sshkg:-}" ]; then
        CHANTO_OPENSSL=$nossl "$tmp/ps1" -Version v9.8.7 -Prefix "$tmp/ps-ssh" > "$tmp/log"
        [ "$(cat "$tmp/ps-ssh/bin/chanto.exe")" = "windows v9.8.7" ]
    fi
    if CHANTO_OPENSSL=$nossl PATH="$tmp/no-verifier" "$tmp/ps1" -Prefix "$tmp/ps-nv" > "$tmp/log" 2>&1; then exit 1; fi
    grep -q 'Cannot check the release signature' "$tmp/log"
    cp "$tmp/releases/latest/download/SHA256SUMS" "$tmp/sums.orig"
    forge "$tmp/releases/latest/download"
    sed 's/x86_64-unknown-linux-musl.tar.gz/x86_64-pc-windows-msvc.zip/' "$tmp/releases/latest/download/SHA256SUMS" > "$tmp/forged"
    cp "$tmp/forged" "$tmp/releases/latest/download/SHA256SUMS"
    for o in "" $nossl; do
        [ -z "$o" ] || [ -n "${sshkg:-}" ] || continue
        if CHANTO_OPENSSL=$o "$tmp/ps1" -Prefix "$tmp/ps-forged" > "$tmp/log" 2>&1; then exit 1; fi
        grep -q 'Signature check failed' "$tmp/log"
    done
    cp "$tmp/sums.orig" "$tmp/releases/latest/download/SHA256SUMS"
    cp "$tmp/releases/download/v0.1.3/SHA256SUMS"* "$tmp/releases/latest/download/"
    if "$tmp/ps1" -Prefix "$tmp/ps-old" > "$tmp/log" 2>&1; then exit 1; fi
    grep -q 'refusing a possible rollback' "$tmp/log"
    "$tmp/ps1" -Version v0.1.3 -Prefix "$tmp/ps-old" > "$tmp/log" 2>&1
    [ "$(cat "$tmp/ps-old/bin/chanto.exe")" = "windows v0.1.3" ]
    if "$tmp/ps1" -Version v0.1.1 -Prefix "$tmp/ps-older" > "$tmp/log" 2>&1; then exit 1; fi
    grep -q 'predates signed releases' "$tmp/log"
    cp "$tmp/releases/download/v9.8.7/SHA256SUMS"* "$tmp/releases/latest/download/"
    [ ! -e "$tmp/ps-nv" ] && [ ! -e "$tmp/ps-forged" ] && [ ! -e "$tmp/ps-older" ]
    psh=checked
fi
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
if grep -E '@VERSION@|@SHA_' "$tmp/chanto.rb"; then exit 1; fi
printf '%s\n' "installer: six OS/arch mappings, curl/wget, pinned/latest releases, signatures (OpenSSL, ssh-keygen ${sshkg:-not available}, none: refused), forged latest, rollback floor, pinned keys, install.ps1 ${psh:-not run (needs pwsh on x64)}, mismatch preservation, formula rendering passed"
