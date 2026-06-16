#!/usr/bin/env bash
# Package the Linux release binary into a .tar.gz and a .deb.
# Usage: ./scripts/build-linux-pkg.sh
# Expects: target/release/glanvu already built.
# Output: dist/linux/

set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')
VERSION=${VERSION:-0.0.0}
ARCH="x86_64"
BINARY="target/release/glanvu"

if [[ ! -f "$BINARY" ]]; then
    echo "Binary not found: $BINARY — run 'cargo build --release' first"
    exit 1
fi

DIST="dist/linux"
TAR_NAME="glanvu-${VERSION}-linux-${ARCH}.tar.gz"
DEB_NAME="glanvu_${VERSION}_amd64.deb"

rm -rf "$DIST"
mkdir -p "$DIST"

# --- tarball ---
STAGING_TAR=$(mktemp -d)
trap 'rm -rf "$STAGING_TAR"' EXIT
DIR="glanvu-${VERSION}"
mkdir -p "$STAGING_TAR/$DIR"
cp "$BINARY"  "$STAGING_TAR/$DIR/glanvu"
cp README.md  "$STAGING_TAR/$DIR/README.md"
cp LICENSE    "$STAGING_TAR/$DIR/LICENSE"
chmod +x "$STAGING_TAR/$DIR/glanvu"
tar -czf "$DIST/$TAR_NAME" -C "$STAGING_TAR" "$DIR"
echo "    Tarball: $DIST/$TAR_NAME"

# --- .deb ---
STAGING_DEB=$(mktemp -d)
trap 'rm -rf "$STAGING_TAR" "$STAGING_DEB"' EXIT

mkdir -p "$STAGING_DEB/DEBIAN"
mkdir -p "$STAGING_DEB/usr/bin"
mkdir -p "$STAGING_DEB/usr/share/doc/glanvu"
mkdir -p "$STAGING_DEB/usr/share/applications"

cp "$BINARY"  "$STAGING_DEB/usr/bin/glanvu"
chmod +x      "$STAGING_DEB/usr/bin/glanvu"
cp README.md  "$STAGING_DEB/usr/share/doc/glanvu/README.md"
cp LICENSE    "$STAGING_DEB/usr/share/doc/glanvu/copyright"

INSTALLED_KB=$(du -sk "$STAGING_DEB/usr" | cut -f1)

cat > "$STAGING_DEB/DEBIAN/control" <<EOF
Package: glanvu
Version: ${VERSION}
Architecture: amd64
Maintainer: Glanvu <hello@glanvu.com>
Installed-Size: ${INSTALLED_KB}
Depends: libc6 (>= 2.17), libx11-6, libxkbcommon0
Recommends: mesa-vulkan-drivers | libvulkan1
Section: graphics
Priority: optional
Homepage: https://glanvu.com
Description: Fast, keyboard-driven image viewer and converter
 GPU-accelerated viewer for JPEG, PNG, WebP, GIF, BMP, and TIFF.
 Keyboard-first navigation, thumbnail grid, directory explorer,
 slideshow, and headless batch conversion.
EOF

cat > "$STAGING_DEB/usr/share/applications/glanvu.desktop" <<EOF
[Desktop Entry]
Name=Glanvu
Comment=Fast, keyboard-driven image viewer
Exec=glanvu %F
Icon=glanvu
Type=Application
Categories=Graphics;Viewer;
MimeType=image/jpeg;image/png;image/gif;image/bmp;image/tiff;image/webp;
StartupNotify=true
EOF

dpkg-deb --build "$STAGING_DEB" "$DIST/$DEB_NAME"
echo "    Package: $DIST/$DEB_NAME"

echo ""
echo "✅  Built: $DIST/"
echo "    Binary size: $(du -sh "$BINARY" | cut -f1)"
echo ""
echo "To install (Debian/Ubuntu): sudo dpkg -i $DIST/$DEB_NAME"
echo "To run:                     glanvu /path/to/image.jpg"
