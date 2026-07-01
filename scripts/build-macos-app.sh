#!/usr/bin/env bash
# Build a macOS .app bundle for Glanvu.
# Usage: ./scripts/build-macos-app.sh [--release | --debug] [--target <triple>]
# Output: dist/macos/Glanvu.app
#
# Cross-compilation example (from Apple Silicon):
#   rustup target add x86_64-apple-darwin
#   ./scripts/build-macos-app.sh --release --target x86_64-apple-darwin
#
# Note: the bundle is ad-hoc signed below (identity "-", free, no Apple Developer
# account needed). Ad-hoc signing alone doesn't satisfy notarization, so a
# quarantined (downloaded) copy still triggers Gatekeeper on first launch — but
# as the standard "unidentified developer" dialog with an Open Anyway escape,
# not the unsigned "app is damaged, move to Trash" dead end. Full notarization
# requires a paid Apple Developer account ($99/yr) — deferred for cost.

set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE=--release
TARGET=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --release|--debug) PROFILE="$1"; shift ;;
        --target) TARGET="$2"; shift 2 ;;
        *) echo "Unknown argument: $1" >&2; exit 1 ;;
    esac
done

# Ensure the cross-compilation target is installed for the *active* toolchain.
# rust-toolchain.toml pins the channel, so the target must be added to that
# toolchain specifically — adding it to `stable` (e.g. via a CI action) is not enough.
if [[ -n "$TARGET" ]]; then
    rustup target add "$TARGET"
fi

if [[ "$PROFILE" == "--release" ]]; then
    if [[ -n "$TARGET" ]]; then
        cargo build --release --target "$TARGET"
        BINARY="target/$TARGET/release/glanvu"
    else
        cargo build --release
        BINARY="target/release/glanvu"
    fi
else
    if [[ -n "$TARGET" ]]; then
        cargo build --target "$TARGET"
        BINARY="target/$TARGET/debug/glanvu"
    else
        cargo build
        BINARY="target/debug/glanvu"
    fi
fi

# Version for the bundle (single source of truth: workspace Cargo.toml).
VERSION=$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')
VERSION=${VERSION:-0.0.0}

APP=dist/macos/Glanvu.app
CONTENTS=$APP/Contents
MACOS=$CONTENTS/MacOS
RESOURCES=$CONTENTS/Resources

rm -rf "$APP"
mkdir -p "$MACOS" "$RESOURCES"

# Binary
cp "$BINARY" "$MACOS/Glanvu"
chmod +x "$MACOS/Glanvu"

# Info.plist — registers Glanvu with the OS as a viewer for image files.
# Heredoc is unquoted so ${VERSION} expands; the plist body has no other shell metacharacters.
cat > "$CONTENTS/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>
  <string>Glanvu</string>

  <key>CFBundleDisplayName</key>
  <string>Glanvu</string>

  <key>CFBundleIdentifier</key>
  <string>com.glanvu.app</string>

  <key>CFBundleVersion</key>
  <string>${VERSION}</string>

  <key>CFBundleShortVersionString</key>
  <string>${VERSION}</string>

  <key>NSHumanReadableCopyright</key>
  <string>© 2026 Juan García Longarón. Apache-2.0 licensed.</string>

  <key>CFBundlePackageType</key>
  <string>APPL</string>

  <key>CFBundleSignature</key>
  <string>????</string>

  <key>CFBundleExecutable</key>
  <string>Glanvu</string>

  <key>CFBundleIconFile</key>
  <string>AppIcon</string>

  <key>NSHighResolutionCapable</key>
  <true/>

  <key>NSPrincipalClass</key>
  <string>NSApplication</string>

  <key>LSApplicationCategoryType</key>
  <string>public.app-category.graphics-design</string>

  <!-- Allow the app to appear in "Open With" for image files even without codesigning. -->
  <key>LSMinimumSystemVersion</key>
  <string>12.0</string>

  <!-- File type associations: Finder will offer "Open With > Glanvu" for these. -->
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key>
      <string>Image</string>
      <key>CFBundleTypeRole</key>
      <string>Viewer</string>
      <!-- LSHandlerRank = Default makes Glanvu a candidate for "default app" -->
      <key>LSHandlerRank</key>
      <string>Alternate</string>
      <key>CFBundleTypeExtensions</key>
      <array>
        <string>jpg</string>
        <string>jpeg</string>
        <string>png</string>
        <string>gif</string>
        <string>bmp</string>
        <string>tif</string>
        <string>tiff</string>
        <string>webp</string>
        <string>svg</string>
      </array>
      <key>CFBundleTypeMIMETypes</key>
      <array>
        <string>image/jpeg</string>
        <string>image/png</string>
        <string>image/gif</string>
        <string>image/bmp</string>
        <string>image/tiff</string>
        <string>image/webp</string>
        <string>image/svg+xml</string>
      </array>
    </dict>
  </array>

  <!-- URL scheme: glanvu://path/to/file (future use) -->
  <key>CFBundleURLTypes</key>
  <array>
    <dict>
      <key>CFBundleURLName</key>
      <string>Glanvu Image</string>
      <key>CFBundleURLSchemes</key>
      <array>
        <string>glanvu</string>
      </array>
    </dict>
  </array>
</dict>
</plist>
PLIST

# Credits.html — AppKit's standard "About" panel loads this automatically from Resources
# and renders it below the version, with clickable links.
cat > "$RESOURCES/Credits.html" <<'CREDITS'
<!DOCTYPE html>
<html>
<head><meta charset="utf-8"></head>
<body style="font-family:-apple-system,Helvetica,sans-serif;font-size:11px;text-align:center;color:#444;margin:0;">
<p style="margin:4px 0;">A fast, keyboard-driven, cross-platform<br>image viewer &amp; converter.</p>
<p style="margin:8px 0;">
<a href="https://glanvu.com">glanvu.com</a>
</p>
<p style="margin:8px 0;">
Support development:<br>
<a href="https://ko-fi.com/juanyque">Ko-fi</a>
&nbsp;&middot;&nbsp;
<a href="https://github.com/sponsors/juanyque">GitHub Sponsors</a>
</p>
</body>
</html>
CREDITS

# Icon — generate AppIcon.icns from assets/AppIcon.png.
ICON_SRC="assets/AppIcon.png"
if [[ -f "$ICON_SRC" ]]; then
    ICONSET_DIR=$(mktemp -d)
    ICONSET="$ICONSET_DIR/AppIcon.iconset"
    mkdir -p "$ICONSET"
    sips -z 16   16   "$ICON_SRC" --out "$ICONSET/icon_16x16.png"      > /dev/null
    sips -z 32   32   "$ICON_SRC" --out "$ICONSET/icon_16x16@2x.png"   > /dev/null
    sips -z 32   32   "$ICON_SRC" --out "$ICONSET/icon_32x32.png"      > /dev/null
    sips -z 64   64   "$ICON_SRC" --out "$ICONSET/icon_32x32@2x.png"   > /dev/null
    sips -z 128  128  "$ICON_SRC" --out "$ICONSET/icon_128x128.png"    > /dev/null
    sips -z 256  256  "$ICON_SRC" --out "$ICONSET/icon_128x128@2x.png" > /dev/null
    sips -z 256  256  "$ICON_SRC" --out "$ICONSET/icon_256x256.png"    > /dev/null
    sips -z 512  512  "$ICON_SRC" --out "$ICONSET/icon_256x256@2x.png" > /dev/null
    sips -z 512  512  "$ICON_SRC" --out "$ICONSET/icon_512x512.png"    > /dev/null
    sips -z 1024 1024 "$ICON_SRC" --out "$ICONSET/icon_512x512@2x.png" > /dev/null
    iconutil --convert icns "$ICONSET" --output "$RESOURCES/AppIcon.icns"
    rm -rf "$ICONSET_DIR"
    echo "    Icon:   AppIcon.icns (from $ICON_SRC)"
else
    echo "# placeholder — add assets/AppIcon.png for a real icon" > "$RESOURCES/AppIcon.icns"
    echo "    Icon:   placeholder (assets/AppIcon.png not found)"
fi

# Ad-hoc code-sign the bundle: no certificate needed, but it gives Gatekeeper a
# valid signature to check. Without this, a copy downloaded via a browser or
# Homebrew (which sets the com.apple.quarantine xattr) is reported as "damaged"
# with no bypass on Sonoma+; ad-hoc signed, it falls back to the milder
# "unidentified developer" prompt (System Settings → Privacy & Security → Open
# Anyway). See WIP/glanvu/doc/glanvu.distribution-runbook.md.
codesign --force --deep --sign - "$APP"

echo ""
echo "✅  Built: $APP"
echo "    Binary: $(du -sh "$MACOS/Glanvu" | cut -f1)"
echo ""
echo "To install: cp -r dist/macos/Glanvu.app /Applications/"
echo "To run:     open dist/macos/Glanvu.app"
echo "            (first run: right-click → Open to bypass Gatekeeper)"
echo ""
echo "To open a file: open -a Glanvu /path/to/image.jpg"
echo ""
echo "Next steps:"
echo "  1. Notarize for a fully Gatekeeper-clean install (requires Apple Developer account)"
echo "     xcrun notarytool submit ... && xcrun stapler staple dist/macos/Glanvu.app"
