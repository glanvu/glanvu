#!/usr/bin/env bash
# Update all packaging manifests to a new version.
# Usage: ./scripts/bump-packaging.sh <version>    (e.g. 0.5.1 or v0.5.1)
# Requires: gh (GitHub CLI, authenticated)
set -euo pipefail

VERSION="${1:-}"
[[ -z "$VERSION" ]] && { echo "Usage: $0 <version>  (e.g. 0.5.1)" >&2; exit 1; }
VERSION="${VERSION#v}"
TAG="v${VERSION}"
REPO="glanvu/glanvu"

PREV_VERSION=$(awk '/^[[:space:]]*version / {gsub(/"/, "", $2); print $2; exit}' dist/brew/Casks/glanvu.rb)
[[ -z "$PREV_VERSION" ]] && { echo "Could not read current version from dist/brew/Casks/glanvu.rb" >&2; exit 1; }
[[ "$PREV_VERSION" == "$VERSION" ]] && { echo "Already at $VERSION, nothing to do." >&2; exit 0; }

echo "Bumping $PREV_VERSION → $VERSION"

# ── Fetch digests from GitHub release ─────────────────────────────────────────
echo "Fetching release digests for $TAG from $REPO..."
ASSETS=$(gh release view "$TAG" --repo "$REPO" --json assets \
  --jq '.assets[] | "\(.name) \(.digest)"')

extract() { echo "$ASSETS" | awk "/$1/ {print \$2}" | sed 's/^sha256://'; }
SHA_MACOS_ARM=$(extract "Glanvu-${VERSION}-macos-arm64\\.zip")
SHA_MACOS_X86=$(extract "Glanvu-${VERSION}-macos-x86_64\\.zip")
SHA_LINUX=$(extract     "glanvu-${VERSION}-linux-x86_64\\.tar\\.gz")
SHA_WIN=$(extract       "Glanvu-${VERSION}-windows-x86_64\\.zip")

[[ -z "$SHA_MACOS_ARM" || -z "$SHA_MACOS_X86" || -z "$SHA_LINUX" || -z "$SHA_WIN" ]] && {
  echo "Error: could not find all four digests. Does the release have both macOS assets?" >&2
  printf "%s\n" "$ASSETS" >&2; exit 1; }

SHA_WIN_UP=$(echo "$SHA_WIN" | tr '[:lower:]' '[:upper:]')
printf "macOS arm64: %s\nmacOS x86_64: %s\nLinux:       %s\nWindows:     %s\n\n" \
  "$SHA_MACOS_ARM" "$SHA_MACOS_X86" "$SHA_LINUX" "$SHA_WIN"

# Helper: in-place perl substitution (portable across macOS/Linux)
p() { perl -i -pe "$1" "$2"; }

# ── Homebrew cask (macOS arm64 + x86_64) ─────────────────────────────────────
echo "Updating Homebrew cask..."
p "s/^  version \".*\"/  version \"${VERSION}\"/" dist/brew/Casks/glanvu.rb
# Update each arch's sha256 using awk for context-aware block matching.
awk -v arm="$SHA_MACOS_ARM" -v intel="$SHA_MACOS_X86" '
  /on_arm do/   { in_arm=1 }
  /on_intel do/ { in_intel=1 }
  /^  end/      { in_arm=0; in_intel=0 }
  in_arm   && /sha256/ { sub(/"[0-9a-f]{64}"/, "\"" arm   "\"") }
  in_intel && /sha256/ { sub(/"[0-9a-f]{64}"/, "\"" intel "\"") }
  { print }
' dist/brew/Casks/glanvu.rb > dist/brew/Casks/glanvu.rb.tmp \
  && mv dist/brew/Casks/glanvu.rb.tmp dist/brew/Casks/glanvu.rb

# ── Homebrew formula (Linux + macOS arm64 + macOS x86_64) ────────────────────
echo "Updating Homebrew formula..."
# URLs: bump version string in all three download URLs.
p "s|releases/download/v[0-9.]+/glanvu-[0-9.]+-linux|releases/download/v${VERSION}/glanvu-${VERSION}-linux|g" dist/brew/Formula/glanvu.rb
p "s|releases/download/v[0-9.]+/Glanvu-[0-9.]+-macos|releases/download/v${VERSION}/Glanvu-${VERSION}-macos|g" dist/brew/Formula/glanvu.rb
# version appears three times; replace all.
p "s/version \"[0-9.]+\"/version \"${VERSION}\"/g" dist/brew/Formula/glanvu.rb
# sha256: use awk for context-aware block matching (linux / on_arm / on_intel).
awk -v linux="$SHA_LINUX" -v arm="$SHA_MACOS_ARM" -v intel="$SHA_MACOS_X86" '
  /on_linux do/  { in_linux=1 }
  /on_arm do/    { in_arm=1 }
  /on_intel do/  { in_intel=1 }
  /^    end/     { in_arm=0; in_intel=0 }
  /^  end/       { in_linux=0 }
  in_linux && /sha256/ { sub(/"[0-9a-f]{64}"/, "\"" linux "\"") }
  in_arm   && /sha256/ { sub(/"[0-9a-f]{64}"/, "\"" arm   "\"") }
  in_intel && /sha256/ { sub(/"[0-9a-f]{64}"/, "\"" intel "\"") }
  { print }
' dist/brew/Formula/glanvu.rb > dist/brew/Formula/glanvu.rb.tmp \
  && mv dist/brew/Formula/glanvu.rb.tmp dist/brew/Formula/glanvu.rb

# ── Scoop ─────────────────────────────────────────────────────────────────────
echo "Updating Scoop..."
p "s/\"version\": \"[^\"]*\"/\"version\": \"${VERSION}\"/"                        dist/scoop/bucket/glanvu.json
p "s/\"hash\": \"[^\"]*\"/\"hash\": \"${SHA_WIN}\"/"                              dist/scoop/bucket/glanvu.json
p "s/\"extract_dir\": \"Glanvu-[^\"]*\"/\"extract_dir\": \"Glanvu-${VERSION}\"/" dist/scoop/bucket/glanvu.json
p "s|Glanvu-[0-9.]+-windows-x86_64|Glanvu-${VERSION}-windows-x86_64|g"           dist/scoop/bucket/glanvu.json

# ── AUR ───────────────────────────────────────────────────────────────────────
echo "Updating AUR..."
p "s/^pkgver=.*/pkgver=${VERSION}/"                                                         dist/aur/PKGBUILD
p "s/sha256sums=\('[^']*'\)/sha256sums=('${SHA_LINUX}')/"                                   dist/aur/PKGBUILD

p "s/pkgver = .*/pkgver = ${VERSION}/"                                                      dist/aur/.SRCINFO
p "s/sha256sums = .*/sha256sums = ${SHA_LINUX}/"                                            dist/aur/.SRCINFO
p "s|source = glanvu-bin-[^ ]+|source = glanvu-bin-${VERSION}.tar.gz::https://github.com/glanvu/glanvu/releases/download/v${VERSION}/glanvu-${VERSION}-linux-x86_64.tar.gz|" dist/aur/.SRCINFO

# ── winget — create new version folder ────────────────────────────────────────
echo "Updating winget..."
WINGET_OLD="dist/winget/manifests/g/Glanvu/Glanvu/${PREV_VERSION}"
WINGET_NEW="dist/winget/manifests/g/Glanvu/Glanvu/${VERSION}"
[[ ! -d "$WINGET_NEW" ]] && cp -r "$WINGET_OLD" "$WINGET_NEW"
for f in "$WINGET_NEW"/*.yaml; do
  p "s/PackageVersion: .*/PackageVersion: ${VERSION}/"                              "$f"
  p "s|/v[0-9.]+/Glanvu-[0-9.]+-windows|/v${VERSION}/Glanvu-${VERSION}-windows|g" "$f"
  p "s/InstallerSha256: [0-9A-F]{64}/InstallerSha256: ${SHA_WIN_UP}/"              "$f"
  p "s|Glanvu-[0-9.]+\\\\glanvu|Glanvu-${VERSION}\\\\glanvu|"                      "$f"
done

# ── Chocolatey ────────────────────────────────────────────────────────────────
echo "Updating Chocolatey..."
p "s|/v[0-9.]+/Glanvu-[0-9.]+-windows|/v${VERSION}/Glanvu-${VERSION}-windows|g"  dist/chocolatey/tools/chocolateyinstall.ps1
p "s/'[0-9A-F]{64}'/'${SHA_WIN_UP}'/"                                             dist/chocolatey/tools/chocolateyinstall.ps1
p "s|<version>[^<]*</version>|<version>${VERSION}</version>|"                     dist/chocolatey/glanvu.nuspec
p "s|releases/tag/v[0-9.]+|releases/tag/v${VERSION}|g"                            dist/chocolatey/glanvu.nuspec
p "s|releases/tag/v[0-9.]+|releases/tag/v${VERSION}|g"                            dist/chocolatey/tools/VERIFICATION.txt
p "s/Glanvu-[0-9.]+-windows-x86_64/Glanvu-${VERSION}-windows-x86_64/g"           dist/chocolatey/tools/VERIFICATION.txt
p "s/[0-9A-F]{64}/${SHA_WIN_UP}/"                                                 dist/chocolatey/tools/VERIFICATION.txt

echo ""
echo "Done — all manifests at ${VERSION}."
echo "Review: git diff dist/"
echo ""
printf "Publish checklist:\n"
printf "  brew:        push dist/brew/Casks/ + Formula/  →  github.com/glanvu/homebrew-glanvu\n"
printf "  scoop:       push dist/scoop/bucket/glanvu.json  →  github.com/glanvu/scoop-glanvu  bucket/\n"
printf "  aur:         cd dist/aur && git push aur@aur.archlinux.org:glanvu-bin.git\n"
printf "  winget:      PR to microsoft/winget-pkgs with dist/winget/manifests/g/Glanvu/Glanvu/%s/\n" "$VERSION"
printf "  chocolatey:  cd dist/chocolatey && choco pack && choco push\n"
