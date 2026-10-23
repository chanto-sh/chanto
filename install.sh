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
    [ "$version" = latest ] || valid_version "$version" || fail 'version must be vX.Y.Z'
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
    if [ "$version" = latest ]; then
        download "$base/latest/download/SHA256SUMS" "$tmp/SHA256SUMS" || fail "no published release found at $base (to build from a checkout instead: ./install.sh --from-source)"
        version=$(sed -n "s/.*chanto-\(v[0-9][^ /]*\)-$target.tar.gz$/\1/p" "$tmp/SHA256SUMS")
        [ -n "$version" ] || fail 'release has no compatible binary'
    else
        download "$base/download/$version/SHA256SUMS" "$tmp/SHA256SUMS" || fail "release $version not found at $base"
    fi
    valid_version "$version" || fail 'invalid release version'
    # From v0.1.2 on, SHA256SUMS is signed with the release key
    # (packaging/release-signing-key.pub), so the checksums can't be swapped
    # along with the archives. A missing or wrong signature stops the install;
    # it is checked whenever an OpenSSL that can verify Ed25519 (3.0+) exists.
    case "$version" in
        v0.1.0 | v0.1.1) ;;
        *)
            download "$base/download/$version/SHA256SUMS.sig" "$tmp/SHA256SUMS.sig" ||
                fail "release $version has no SHA256SUMS.sig: refusing an unsigned release"
            ossl=
            for o in openssl /opt/homebrew/opt/openssl@3/bin/openssl /usr/local/opt/openssl@3/bin/openssl; do
                command -v "$o" >/dev/null 2>&1 || continue
                case "$("$o" pkeyutl -help 2>&1)" in *-rawin*) ossl=$o; break ;; esac
            done
            if [ -n "$ossl" ]; then
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
                    fail 'signature check failed for SHA256SUMS: refusing to install'
            else
                echo 'note: release signature not checked (needs OpenSSL 3); checksums still are' >&2
            fi
            ;;
    esac
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
