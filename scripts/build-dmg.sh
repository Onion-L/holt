#!/usr/bin/env bash
set -euo pipefail

# Builds Holt.app and a signed, notarized DMG.
#
# Usage: scripts/build-dmg.sh [--target <apple-darwin triple>]
# Without --target, builds for the host arch.
#
# Environment overrides:
#   CODESIGN_IDENTITY  codesign identity (default: the project's Developer ID cert)
#   NOTARY_PROFILE     notarytool keychain profile (default: holt-notary)

TARGET=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --target) TARGET="${2:?--target needs a value}"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 1 ;;
  esac
done

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT_DIR/Cargo.toml" | head -1)"
APP_NAME="Holt"
IDENTIFIER="com.onion.holt"
CODESIGN_IDENTITY="${CODESIGN_IDENTITY:-Developer ID Application: Xiang Li (AQMMZQ3MDX)}"
NOTARY_PROFILE="${NOTARY_PROFILE:-holt-notary}"

cd "$ROOT_DIR"
if [[ -n "$TARGET" ]]; then
  case "$TARGET" in
    aarch64-apple-darwin) ARCH="arm64" ;;
    x86_64-apple-darwin) ARCH="x64" ;;
    *) echo "unsupported target: $TARGET" >&2; exit 1 ;;
  esac
  rustup target add "$TARGET"
  cargo build --release -p holt --target "$TARGET"
  OUT_DIR="$ROOT_DIR/target/$TARGET/release"
else
  cargo build --release -p holt
  OUT_DIR="$ROOT_DIR/target/release"
  case "$(uname -m)" in
    arm64) ARCH="arm64" ;;
    x86_64) ARCH="x64" ;;
    *) echo "unsupported host arch: $(uname -m)" >&2; exit 1 ;;
  esac
fi

APP="$OUT_DIR/$APP_NAME.app"
DMG="$OUT_DIR/$APP_NAME-$VERSION-$ARCH.dmg"
ICONSET="$OUT_DIR/holt.iconset"
STAGING="$OUT_DIR/holt-dmg-staging"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$OUT_DIR/holt" "$APP/Contents/MacOS/holt"

rm -rf "$ICONSET"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" crates/ui/assets/app-icon.png --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" crates/ui/assets/app-icon.png --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleDisplayName</key><string>Holt</string>
<key>CFBundleExecutable</key><string>holt</string>
<key>CFBundleIdentifier</key><string>$IDENTIFIER</string>
<key>CFBundleName</key><string>Holt</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>$VERSION</string>
<key>CFBundleVersion</key><string>$VERSION</string>
<key>CFBundleIconFile</key><string>AppIcon</string>
</dict></plist>
PLIST

codesign --force --deep --options runtime --timestamp --sign "$CODESIGN_IDENTITY" "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"
rm -rf "$STAGING" "$DMG"
mkdir -p "$STAGING"
cp -R "$APP" "$STAGING/"
ln -s /Applications "$STAGING/Applications"
hdiutil create -volname "$APP_NAME" -srcfolder "$STAGING" -ov -format UDZO "$DMG" >/dev/null
xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" --wait
xcrun stapler staple "$DMG"
xcrun stapler validate "$DMG"
rm -rf "$STAGING"
echo "Created $DMG"
