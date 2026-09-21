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
TMP_DMG="$OUT_DIR/holt-tmp-$ARCH.dmg"
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
rm -rf "$STAGING" "$DMG" "$TMP_DMG"
mkdir -p "$STAGING"
cp -R "$APP" "$STAGING/"
ln -s /Applications "$STAGING/Applications"

# Build a read-write image first so the Finder window can be styled (background,
# icon layout, volume icon), then convert to the compressed read-only DMG.
hdiutil create -volname "$APP_NAME" -srcfolder "$STAGING" -ov -format UDRW "$TMP_DMG" >/dev/null
rm -rf "$STAGING"

MOUNT_DIR="$(hdiutil attach -readwrite -noverify -noautoopen "$TMP_DMG" | sed -n 's/.*\(\/Volumes\/.*\)$/\1/p')"
if [[ -z "$MOUNT_DIR" ]]; then
  echo "failed to mount $TMP_DMG" >&2
  exit 1
fi
trap 'hdiutil detach "$MOUNT_DIR" -quiet 2>/dev/null || true' ERR
DISK_NAME="$(basename "$MOUNT_DIR")"

mkdir -p "$MOUNT_DIR/.background"
tiffutil -cathidpicheck "$ROOT_DIR/assets/dmg/background.png" "$ROOT_DIR/assets/dmg/background@2x.png" -out "$MOUNT_DIR/.background/background.tiff"
cp "$APP/Contents/Resources/AppIcon.icns" "$MOUNT_DIR/.VolumeIcon.icns"
SetFile -c icnC "$MOUNT_DIR/.VolumeIcon.icns"
SetFile -a C "$MOUNT_DIR"

# Icon positions must match the artwork in assets/dmg/background.png.
osascript - "$DISK_NAME" <<'APPLESCRIPT'
on run argv
  set diskName to item 1 of argv
  tell application "Finder"
    tell disk diskName
      open
      set current view of container window to icon view
      set toolbar visible of container window to false
      set statusbar visible of container window to false
      set the bounds of container window to {400, 100, 1020, 480}
      set viewOptions to the icon view options of container window
      set arrangement of viewOptions to not arranged
      set icon size of viewOptions to 104
      set background picture of viewOptions to file ".background:background.tiff"
      set position of item "Holt.app" of container window to {150, 145}
      set position of item "Applications" of container window to {470, 145}
      close
      open
      update without registering applications
      delay 2
      close
    end tell
  end tell
end run
APPLESCRIPT

sync
hdiutil detach "$MOUNT_DIR" -quiet
trap - ERR
hdiutil convert "$TMP_DMG" -format UDZO -imagekey zlib-level=9 -ov -o "$DMG" >/dev/null
rm -f "$TMP_DMG"

xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" --wait
xcrun stapler staple "$DMG"
xcrun stapler validate "$DMG"
echo "Created $DMG"
