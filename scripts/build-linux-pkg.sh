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

# PDFium (D13 in the decision log): Glanvu's first native runtime dependency, fetched separately
# from a pinned bblanchon/pdfium-binaries release — CI sets GLANVU_PDFIUM_DIST_DIR to the extracted
# archive directory before calling this script. Bundled *next to the binary* in both packages
# (not the more idiomatic /usr/lib/glanvu/ for the .deb) so `Pdfium::bind_to_library`'s "next to
# current_exe()" search order (glanvu-core/src/pdf.rs) works identically across all three
# platforms, with no Linux-specific path case. Optional locally: without it, Glanvu still builds
# and runs — every other format works, PDFs show a clean "library not found" error.
# NOTE: assumes the archive's library sits at lib/libpdfium.so — verify against the actual
# downloaded archive before relying on it in CI.
PDFIUM_SO=""
if [[ -n "${GLANVU_PDFIUM_DIST_DIR:-}" && -f "$GLANVU_PDFIUM_DIST_DIR/lib/libpdfium.so" ]]; then
    PDFIUM_SO="$GLANVU_PDFIUM_DIST_DIR/lib/libpdfium.so"
fi

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
if [[ -n "$PDFIUM_SO" ]]; then
    cp "$PDFIUM_SO" "$STAGING_TAR/$DIR/libpdfium.so"
    echo "    PDFium: bundled from $GLANVU_PDFIUM_DIST_DIR"
else
    echo "    PDFium: not bundled (set GLANVU_PDFIUM_DIST_DIR to include PDF support) — see README"
fi
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
# Bundled, not distro-installed, so it has no apt-visible package name — no new `Depends:` entry.
if [[ -n "$PDFIUM_SO" ]]; then
    cp "$PDFIUM_SO" "$STAGING_DEB/usr/bin/libpdfium.so"
fi

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
MimeType=image/jpeg;image/png;image/gif;image/bmp;image/tiff;image/webp;image/svg+xml;application/pdf;
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
