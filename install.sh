#!/bin/sh
# Download a verified release; --from-source retains the local build path.
# --desktop also installs the desktop app from the same release.
set -eu
fail() { echo "install: $*" >&2; exit 1; }
prefix=${PREFIX:-$HOME/.local}
version=latest
source=false
desktop=false
while [ "$#" -gt 0 ]; do
    case "$1" in
        --prefix|--version) [ "$#" -ge 2 ] || fail "$1 needs a value"; key=$1; shift; case "$key" in --prefix) prefix=$1;; *) version=$1;; esac;;
        --from-source) source=true;;
        --desktop) desktop=true;;
        --help) echo 'Usage: install.sh [--version vX.Y.Z] [--prefix DIR] [--desktop] [--from-source]'; exit 0;;
        *) fail "unknown option: $1";;
    esac
    shift
done
[ "$source" = false ] || [ "$desktop" = false ] || fail '--desktop installs a release; from a checkout, see "Desktop app" in docs/reference.md'
if [ "$source" = true ]; then
    cd "$(dirname "$0")"
    cargo build --release --locked
    binary=target/release/chanto
else
    valid_version() { awk -v v="$1" 'BEGIN {exit !(v ~ /^v[0-9]+\.[0-9]+\.[0-9]+$/)}'; }
    # Whether version $1 is older than $2 (both vX.Y.Z).
    older() {
        awk -v a="${1#v}" -v b="${2#v}" 'BEGIN { split(a, x, "."); split(b, y, ".")
            for (i = 1; i <= 3; i++) if (x[i] + 0 != y[i] + 0) exit !(x[i] + 0 < y[i] + 0); exit 1 }'
    }
    # A signature proves a release is genuine, not that it's the newest, so a
    # hostile host could serve an older signed release as "latest". "latest"
    # must be at least min_version; raise it with each security release (see
    # the release workflow). An older --version is still allowed, with a note.
    min_version=v0.1.5
    first_signed=v0.1.2 # the first release with SHA256SUMS.sig
    first_sshsig=v0.1.5 # the first release with SHA256SUMS.sshsig
    if [ "$version" != latest ]; then
        valid_version "$version" || fail 'version must be vX.Y.Z'
        ! older "$version" "$first_signed" ||
            fail "$version predates signed releases ($first_signed), so it can't be verified; to build it from a checkout instead: git checkout $version && ./install.sh --from-source"
        ! older "$version" "$min_version" ||
            echo "note: $version is older than $min_version and may lack security fixes" >&2
    fi
    os=$(uname -s); arch=$(uname -m)
    case "$arch" in arm64|aarch64) arch=aarch64;; x86_64|amd64) arch=x86_64;; *) fail "unsupported architecture: $arch";; esac
    case "$os" in Darwin) target=$arch-apple-darwin;; Linux) target=$arch-unknown-linux-musl;; *) fail "unsupported OS: $os (use install.ps1 on Windows)";; esac
    if [ "$desktop" = true ]; then
        case "$os-$arch" in
            Darwin-*) desktop_archive_target=universal-apple-darwin.zip;;
            Linux-x86_64) desktop_archive_target=x86_64-unknown-linux-gnu.tar.gz;;
            *) fail "the desktop app isn't built for $os $arch";;
        esac
    fi
    download() {
        if command -v curl >/dev/null 2>&1; then curl -fsSL "$1" -o "$2"
        elif command -v wget >/dev/null 2>&1; then wget -q "$1" -O "$2"
        else fail 'curl or wget is required'; fi
    }
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT HUP INT TERM
    base=${CHANTO_RELEASE_BASE_URL:-https://chanto.sh/releases}
    # Every release from v0.1.2 on signs SHA256SUMS with the release key
    # (packaging/release-signing-key.pub). Nothing in SHA256SUMS, not even the
    # version "latest" points at, is used before that signature checks out, so
    # a tampered download host can't pick the version or swap the checksums
    # along with the archives.
    if [ "$version" = latest ]; then sums_url=$base/latest/download
    else sums_url=$base/download/$version; fi
    download "$sums_url/SHA256SUMS" "$tmp/SHA256SUMS" || if [ "$version" = latest ]; then
        fail "no published release found at $base (to build from a checkout instead: ./install.sh --from-source)"
    else fail "release $version not found at $base"; fi
    # OpenSSL 3 verifies SHA256SUMS.sig. Without it (stock macOS has LibreSSL,
    # which can't), ssh-keygen verifies the same key's SSH signature,
    # SHA256SUMS.sshsig, published from v0.1.5 on. With neither, stop:
    # installing unverified would trust whatever the host served.
    # CHANTO_OPENSSL is word-split: a list of candidates, no spaces in paths.
    ossl=
    for o in ${CHANTO_OPENSSL:-openssl /opt/homebrew/opt/openssl@3/bin/openssl /usr/local/opt/openssl@3/bin/openssl}; do
        command -v "$o" >/dev/null 2>&1 || continue
        case "$("$o" pkeyutl -help 2>&1)" in *-rawin*) ossl=$o; break ;; esac
    done
    sshkg=
    if [ -z "$ossl" ] && command -v ssh-keygen >/dev/null 2>&1; then
        # ssh-keygen -Y (sign and verify) arrived in OpenSSH 8.1: a version
        # that has it asks for -Y's argument, an older one rejects the option.
        case "$(ssh-keygen -Y 2>&1)" in *"requires an argument"*) sshkg=ssh-keygen ;; esac
    fi
    # Two downloads of "latest" can straddle a release being published.
    retry=
    [ "$version" != latest ] || retry=' (if a release was published just now, retry in a minute)'
    if [ -n "$ossl" ]; then
        download "$sums_url/SHA256SUMS.sig" "$tmp/SHA256SUMS.sig" || fail "release has no SHA256SUMS.sig, which every release from $first_signed on has: refusing to install$retry"
        if [ -n "${CHANTO_RELEASE_KEY_FILE:-}" ]; then
            cp "$CHANTO_RELEASE_KEY_FILE" "$tmp/release.pub"
        else
            printf '%s\n' '-----BEGIN PUBLIC KEY-----' \
                'MCowBQYDK2VwAyEAHk8ypq/plTuzCUZ4P9+bmwesZLkwM18CBO7AcUQLGd0=' \
                '-----END PUBLIC KEY-----' > "$tmp/release.pub"
        fi
        "$ossl" base64 -d -A -in "$tmp/SHA256SUMS.sig" -out "$tmp/SHA256SUMS.sig.bin" 2>/dev/null ||
            fail 'SHA256SUMS.sig is not valid base64'
        "$ossl" pkeyutl -verify -pubin -inkey "$tmp/release.pub" -rawin \
            -in "$tmp/SHA256SUMS" -sigfile "$tmp/SHA256SUMS.sig.bin" >/dev/null 2>&1 ||
            fail "signature check failed for SHA256SUMS: refusing to install$retry"
    elif [ -n "$sshkg" ]; then
        if ! download "$sums_url/SHA256SUMS.sshsig" "$tmp/SHA256SUMS.sshsig"; then
            # For "latest", the unverified SHA256SUMS only picks the message.
            claimed=$version
            [ "$version" != latest ] ||
                claimed=$(sed -n "s/.*chanto-\(v[0-9][^ /]*\)-$target.tar.gz$/\1/p" "$tmp/SHA256SUMS")
            if valid_version "$claimed" && ! older "$claimed" "$first_sshsig"; then
                fail "release has no SHA256SUMS.sshsig, which every release from $first_sshsig on has: refusing to install$retry"
            fi
            # Releases before v0.1.5 are signed, but only in the form OpenSSL checks.
            fail "this release predates SSH signatures ($first_sshsig), so only OpenSSL 3 can check its signature: install it (macOS: brew install openssl@3) and retry"
        fi
        if [ -n "${CHANTO_RELEASE_SSH_KEY:-}" ]; then
            printf 'chanto-release %s\n' "$CHANTO_RELEASE_SSH_KEY" > "$tmp/allowed_signers"
        else
            printf '%s\n' 'chanto-release ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIB5PMqav6ZU7swlGeD/fm5sHrGS5MDNfAgTuwHFECxnd' > "$tmp/allowed_signers"
        fi
        ssh-keygen -Y verify -f "$tmp/allowed_signers" -I chanto-release -n chanto-release \
            -s "$tmp/SHA256SUMS.sshsig" < "$tmp/SHA256SUMS" >/dev/null 2>&1 ||
            fail "signature check failed for SHA256SUMS: refusing to install$retry"
    else
        fail 'cannot check the release signature: needs OpenSSL 3 or ssh-keygen (OpenSSH 8.1+); install one (macOS: brew install openssl@3; Debian/Ubuntu: apt install openssl) and retry, or build from a checkout with ./install.sh --from-source'
    fi
    if [ "$version" = latest ]; then
        version=$(sed -n "s/.*chanto-\(v[0-9][^ /]*\)-$target.tar.gz$/\1/p" "$tmp/SHA256SUMS")
        [ -n "$version" ] || fail 'release has no compatible binary'
        valid_version "$version" || fail 'invalid release version'
        ! older "$version" "$min_version" ||
            fail "the download host offers $version as the latest release, older than $min_version: refusing a possible rollback (to install $version anyway: --version $version)"
    fi
    # Download a file of this release into $tmp and check it against SHA256SUMS.
    fetch() {
        expected=$(awk -v name="$1" '$2 == name {print $1}' "$tmp/SHA256SUMS")
        [ "${#expected}" = 64 ] || fail "missing or invalid checksum for $1"
        download "$base/download/$version/$1" "$tmp/$1"
        if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum "$tmp/$1")
        elif command -v shasum >/dev/null 2>&1; then actual=$(shasum -a 256 "$tmp/$1")
        else fail 'sha256sum or shasum is required'; fi
        actual=${actual%% *}
        [ "$actual" = "$expected" ] || fail "checksum mismatch for $1"
    }
    archive=chanto-$version-$target.tar.gz
    fetch "$archive"
    tar -xzf "$tmp/$archive" -C "$tmp"
    binary=$tmp/chanto-$version-$target/chanto
    if [ "$desktop" = true ]; then
        desktop_archive=chanto-desktop-$version-$desktop_archive_target
        grep -q " $desktop_archive\$" "$tmp/SHA256SUMS" || fail "release $version has no desktop app for this platform"
        fetch "$desktop_archive"
    fi
fi
[ -f "$binary" ] || fail 'archive has no Chanto binary'
mkdir -p "$prefix/bin"
rm -f "$prefix/bin/chanto"
cp "$binary" "$prefix/bin/chanto"
chmod +x "$prefix/bin/chanto"
printf '%s\n' installer > "$prefix/bin/chanto.install-method"
echo "installed: $prefix/bin/chanto"
if [ "$desktop" = true ]; then
    case "$os" in
        Darwin)
            # chanto.app carries its own copy of the CLI. Downloaded with curl,
            # it has no quarantine flag, so Gatekeeper doesn't block it.
            apps=${CHANTO_APP_DIR:-$HOME/Applications}
            mkdir -p "$apps" "$tmp/app"
            ditto -x -k "$tmp/$desktop_archive" "$tmp/app"
            [ -d "$tmp/app/chanto.app" ] || fail 'archive has no chanto.app'
            rm -rf "$apps/chanto.app"
            ditto "$tmp/app/chanto.app" "$apps/chanto.app"
            echo "installed: $apps/chanto.app"
            ;;
        Linux)
            tar -xzf "$tmp/$desktop_archive" -C "$tmp"
            dir=$tmp/chanto-desktop-$version-x86_64-unknown-linux-gnu
            [ -f "$dir/chanto-desktop" ] || fail 'archive has no chanto-desktop binary'
            rm -f "$prefix/bin/chanto-desktop"
            cp "$dir/chanto-desktop" "$prefix/bin/chanto-desktop"
            chmod +x "$prefix/bin/chanto-desktop"
            data=${XDG_DATA_HOME:-$HOME/.local/share}
            mkdir -p "$data/applications" "$data/icons/hicolor/256x256/apps"
            sed "s|^Exec=chanto-desktop|Exec=$prefix/bin/chanto-desktop|" "$dir/chanto-desktop.desktop" > "$data/applications/chanto-desktop.desktop"
            cp "$dir/chanto-desktop.png" "$data/icons/hicolor/256x256/apps/chanto-desktop.png"
            echo "installed: $prefix/bin/chanto-desktop (and a launcher in $data/applications)"
            if command -v ldconfig >/dev/null 2>&1 && ! ldconfig -p 2>/dev/null | grep -q 'libwebkit2gtk-4\.1\.so'; then
                echo "The desktop app needs GTK 3 and WebKitGTK 4.1, e.g.: sudo apt install libwebkit2gtk-4.1-0 libgtk-3-0 libxdo3"
            fi
            ;;
    esac
fi
case ":$PATH:" in *":$prefix/bin:"*) ;; *) echo "Add to PATH: export PATH=\"$prefix/bin:\$PATH\"";; esac
