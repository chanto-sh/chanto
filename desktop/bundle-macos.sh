#!/bin/sh
# Build chanto.app: the desktop app with the Chanto CLI inside it.
#
#   desktop/bundle-macos.sh              # release builds → target/release/bundle/chanto.app
#   desktop/bundle-macos.sh --debug      # debug builds (quick local check)
#   desktop/bundle-macos.sh --universal  # arm64 + x86_64 in one bundle (what releases ship)
#
# CHANTO_VERSION sets the bundle version (default: the CLI's, from Cargo.toml).
# The bundle is ad-hoc signed so macOS can remember its microphone permission;
# releases re-sign it with a Developer ID and notarize it when the signing
# secrets are configured (.github/workflows/release.yml).
set -eu
cd "$(dirname "$0")/.."

profile=release
flag=--release
universal=false
case "${1:-}" in
  --debug) profile=debug; flag= ;;
  --universal) universal=true ;;
  "") ;;
  *) echo "usage: $0 [--debug | --universal]" >&2; exit 2 ;;
esac

if [ "$universal" = true ]; then
  for t in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release --locked --target $t -p chanto
    cargo build --release --locked --target $t -p chanto-desktop
  done
else
  cargo build $flag -p chanto
  cargo build $flag -p chanto-desktop
fi

# The app and the CLI inside it are one product: both versions come from the
# CLI's manifest, which the release job checks against the desktop crate's.
version=${CHANTO_VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}
app="target/$profile/bundle/chanto.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
if [ "$universal" = true ]; then
  for b in chanto-desktop chanto; do
    lipo -create -output "$app/Contents/MacOS/$b" "target/aarch64-apple-darwin/release/$b" "target/x86_64-apple-darwin/release/$b"
  done
else
  cp "target/$profile/chanto-desktop" "target/$profile/chanto" "$app/Contents/MacOS/"
fi

# The icon: `assets/logo.png` is the SVG pre-rendered at 1024 px, so the bundle
# gets the same mark on machines where no SVG renderer is installed (and where
# Quick Look cannot run, as on a headless runner). Without it, render the SVG.
icon=""
tmp=$(mktemp -d)
png="$tmp/logo.png"
if [ -f assets/logo.png ]; then
  cp assets/logo.png "$png"
elif command -v rsvg-convert >/dev/null 2>&1; then
  rsvg-convert -w 1024 -h 1024 assets/logo.svg -o "$png" || rm -f "$png"
elif command -v magick >/dev/null 2>&1; then
  magick -background none assets/logo.svg -resize 1024x1024 "$png" || rm -f "$png"
fi
if [ ! -f "$png" ] && qlmanage -t -s 1024 -o "$tmp" assets/logo.svg >/dev/null 2>&1; then
  mv "$tmp/logo.svg.png" "$png" 2>/dev/null || true
fi
if [ -f "$png" ]; then
  mkdir "$tmp/chanto.iconset"
  for s in 16 32 128 256 512; do
    sips -z $s $s "$png" --out "$tmp/chanto.iconset/icon_${s}x${s}.png" >/dev/null
    d=$((s * 2))
    sips -z $d $d "$png" --out "$tmp/chanto.iconset/icon_${s}x${s}@2x.png" >/dev/null
  done
  iconutil -c icns "$tmp/chanto.iconset" -o "$app/Contents/Resources/chanto.icns" && icon="chanto"
fi
rm -rf "$tmp"
if [ -z "$icon" ]; then
  echo "warning: could not build the icon from assets/logo.png; the bundle has none" >&2
fi

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>chanto</string>
  <key>CFBundleDisplayName</key><string>chanto</string>
  <key>CFBundleIdentifier</key><string>sh.chanto.desktop</string>
  <key>CFBundleExecutable</key><string>chanto-desktop</string>
  <key>CFBundleIconFile</key><string>$icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key><string>Chanto records your voice to dictate prompts when you press the mic button.</string>
  <key>CFBundleDocumentTypes</key>
  <array><dict>
    <key>CFBundleTypeName</key><string>Folder</string>
    <key>CFBundleTypeRole</key><string>Viewer</string>
    <key>LSItemContentTypes</key><array><string>public.folder</string></array>
  </dict></array>
</dict>
</plist>
EOF

codesign --force --deep --sign - "$app" >/dev/null
echo "Built $app"
