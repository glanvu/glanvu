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
