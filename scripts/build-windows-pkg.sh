#!/usr/bin/env bash
# Package the Windows release binary into a .zip.
# Usage: ./scripts/build-windows-pkg.sh
# Expects: target/release/glanvu.exe already built.
# Output: dist/windows/
#
# Runs natively on Windows (Git Bash / MSYS2) or on Linux/macOS for testing.
# Zipping uses PowerShell when available (Windows), otherwise falls back to zip.

set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')
VERSION=${VERSION:-0.0.0}
BINARY="target/release/glanvu.exe"

if [[ ! -f "$BINARY" ]]; then
    echo "Binary not found: $BINARY — run 'cargo build --release' first"
    exit 1
fi

DIST="dist/windows"
STAGING="$DIST/Glanvu-${VERSION}"
ZIP_NAME="Glanvu-${VERSION}-windows-x86_64.zip"

rm -rf "$DIST"
mkdir -p "$STAGING"

cp "$BINARY"  "$STAGING/glanvu.exe"
cp README.md  "$STAGING/README.md"
cp LICENSE    "$STAGING/LICENSE"

# PDFium (D13 in the decision log): Glanvu's first native runtime dependency, fetched separately
# from a pinned bblanchon/pdfium-binaries release — CI sets GLANVU_PDFIUM_DIST_DIR to the
# extracted archive directory before calling this script. Dropped next to glanvu.exe: Windows'
# own DLL search order already checks the exe's directory first, and it's also where
# `Pdfium::bind_to_library`'s own search (glanvu-core/src/pdf.rs) looks. Optional locally: without
# it, Glanvu still builds and runs — every other format works, PDFs show a clean error.
# NOTE: assumes the archive's DLL sits at bin/pdfium.dll (Windows SDK-style archives typically
# split runtime DLLs from lib/include under bin/, unlike the lib/-only macOS/Linux archives) —
# verify against the actual downloaded archive before relying on it in CI.
if [[ -n "${GLANVU_PDFIUM_DIST_DIR:-}" && -f "$GLANVU_PDFIUM_DIST_DIR/bin/pdfium.dll" ]]; then
    cp "$GLANVU_PDFIUM_DIST_DIR/bin/pdfium.dll" "$STAGING/pdfium.dll"
    echo "    PDFium: bundled from $GLANVU_PDFIUM_DIST_DIR"
else
    echo "    PDFium: not bundled (set GLANVU_PDFIUM_DIST_DIR to include PDF support) — see README"
fi

if command -v powershell.exe &>/dev/null; then
    powershell.exe -NoProfile -Command \
        "Compress-Archive -Path '${STAGING}' -DestinationPath '${DIST}/${ZIP_NAME}' -Force"
else
    (cd "$DIST" && zip -r "$ZIP_NAME" "Glanvu-${VERSION}")
fi

echo ""
echo "✅  Built: $DIST/$ZIP_NAME"
echo "    Binary size: $(du -sh "$BINARY" | cut -f1)"
echo ""
echo "To run: unzip the archive and double-click glanvu.exe"
echo "        (first run: Windows Defender SmartScreen — click 'More info' → 'Run anyway')"
