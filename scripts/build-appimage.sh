#!/usr/bin/env bash
set -euo pipefail

# Builds Holt and packages it as an AppImage for the host arch (x64 or
# arm64).
#
# Usage: scripts/build-appimage.sh
# The host must be x86_64 or aarch64 Linux; the AppImage lands in
# target/release/. Needs ImageMagick (magick or convert) and curl besides
# cargo.
#
# linuxdeploy is pulled from its "continuous" release but pinned by
# sha256: if upstream replaces an asset, the checksum step fails the
# build closed until the pin below is re-made deliberately — the same
# posture as the SHA-pinned actions in release.yml.
#
# fontconfig and freetype are Holt's only link-time C libraries on Linux
# (wayland/x11/vulkan load at runtime via dlopen), and both are excluded
# from bundling on purpose: an AppImage carrying its own libfontconfig
# poisons the host's font cache, and libfontconfig1 ships with every
# desktop distro anyway.

LINUXDEPLOY_URL_BASE="https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous"
LINUXDEPLOY_SHA256_X86_64="8aea8da0f7f7039d2a2cecb14657d752a222a5e1d3825caeef186c82f751cdd1"
LINUXDEPLOY_SHA256_AARCH64="5c1fddf96066891e829831cac0d84424690f3b22846c7f8f1bb9990a5c6c73f4"

case "$(uname -m)" in
  x86_64) ARCH="x64"; LINUXDEPLOY_ARCH="x86_64" ;;
  aarch64) ARCH="arm64"; LINUXDEPLOY_ARCH="aarch64" ;;
  *) echo "unsupported host arch: $(uname -m)" >&2; exit 1 ;;
esac
LINUXDEPLOY_URL="$LINUXDEPLOY_URL_BASE/linuxdeploy-$LINUXDEPLOY_ARCH.AppImage"
LINUXDEPLOY_SHA256="$LINUXDEPLOY_SHA256_X86_64"
if [[ "$ARCH" == "arm64" ]]; then
  LINUXDEPLOY_SHA256="$LINUXDEPLOY_SHA256_AARCH64"
fi

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT_DIR/Cargo.toml" | head -1)"
APP_NAME="Holt"
OUT_DIR="$ROOT_DIR/target/release"
APPDIR="$OUT_DIR/holt-appdir"

cd "$ROOT_DIR"
cargo build --release -p holt

rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/icons/hicolor/512x512/apps"
cp "$OUT_DIR/holt" "$APPDIR/usr/bin/holt"

# app-icon.png is 1024x1024; desktops only look in the standard hicolor
# sizes, so shrink to 512 (IM7 spells it magick, IM6 convert).
if command -v magick >/dev/null 2>&1; then
  magick crates/ui/assets/app-icon.png -resize 512x512 "$APPDIR/usr/share/icons/hicolor/512x512/apps/holt.png"
else
  convert crates/ui/assets/app-icon.png -resize 512x512 "$APPDIR/usr/share/icons/hicolor/512x512/apps/holt.png"
fi

cat > "$APPDIR/holt.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Holt
Exec=holt
Icon=holt
Categories=Development;
Terminal=false
DESKTOP

LINUXDEPLOY="$OUT_DIR/linuxdeploy-$LINUXDEPLOY_ARCH.AppImage"
curl -sSL -o "$LINUXDEPLOY" "$LINUXDEPLOY_URL"
echo "$LINUXDEPLOY_SHA256  $LINUXDEPLOY" | sha256sum -c -
chmod +x "$LINUXDEPLOY"

# APPIMAGE_EXTRACT_AND_RUN: CI runners mount no FUSE, so run linuxdeploy
# from its extracted tree. OUTPUT names the artifact; linuxdeploy deploys
# the ldd-visible libraries into the AppDir and writes the AppRun wrapper.
APPIMAGE_EXTRACT_AND_RUN=1 OUTPUT="$OUT_DIR/$APP_NAME-$VERSION-$ARCH.AppImage" \
  "$LINUXDEPLOY" \
    --appdir "$APPDIR" \
    --desktop-file "$APPDIR/holt.desktop" \
    --icon-file "$APPDIR/usr/share/icons/hicolor/512x512/apps/holt.png" \
    --exclude-library libfontconfig.so.1 \
    --exclude-library libfreetype.so.6 \
    --output appimage

echo "Created $OUT_DIR/$APP_NAME-$VERSION-$ARCH.AppImage"
