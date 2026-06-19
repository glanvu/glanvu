#!/usr/bin/env bash
# Hotfix: build the Intel (x86_64) macOS .app for the current version,
# upload it to the existing GitHub release, and update the Homebrew cask.
#
# Usage: ./scripts/hotfix-macos-intel.sh
# Requires: cargo, gh (authenticated), rustup, zip, shasum

set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')
TAG="v${VERSION}"
ZIP="Glanvu-${VERSION}-macos-x86_64.zip"
REPO="glanvu/glanvu"

echo "==> Glanvu ${VERSION} — Intel macOS hotfix"
echo ""

# 1. Ensure the cross-compilation target is installed.
echo "--> Adding x86_64-apple-darwin target..."
rustup target add x86_64-apple-darwin

echo ""

# 2. Build Intel .app bundle.
echo "--> Building Intel .app (cargo build --release --target x86_64-apple-darwin)..."
./scripts/build-macos-app.sh --release --target x86_64-apple-darwin

echo ""

# 3. Package.
echo "--> Packaging ${ZIP}..."
(cd dist/macos && zip -r "../../${ZIP}" Glanvu.app)
echo "    Size: $(du -sh "${ZIP}" | cut -f1)"

# 4. Compute sha256.
SHA=$(shasum -a 256 "${ZIP}" | cut -d' ' -f1)
echo "    sha256: ${SHA}"
echo ""

# 5. Upload to the GitHub release.
echo "--> Uploading ${ZIP} to release ${TAG}..."
gh release upload "${TAG}" "${ZIP}" --repo "${REPO}" --clobber
echo "    Done."
echo ""

# 6. Update Homebrew cask and formula (Intel sha256 block in each).
update_intel_sha() {
  local file="$1"
  awk -v intel_sha="$SHA" '
    /on_intel do/ { in_intel=1 }
    /^  end|^    end/ { in_intel=0 }
    in_intel && /sha256/ { sub(/\"[0-9a-f]{64}\"/, "\"" intel_sha "\"") }
    { print }
  ' "$file" > "$file.tmp" && mv "$file.tmp" "$file"
}

CASK="dist/brew/Casks/glanvu.rb"
FORMULA="dist/brew/Formula/glanvu.rb"
echo "--> Updating ${CASK} with Intel sha256..."
update_intel_sha "${CASK}"
echo "    Updated."
echo "--> Updating ${FORMULA} with Intel sha256..."
update_intel_sha "${FORMULA}"
echo "    Updated."
echo ""

echo "======================================================"
echo "Hotfix complete. Steps remaining (you do these):"
echo ""
echo "  1. Review the updated cask:"
echo "       cat ${CASK}"
echo ""
echo "  2. Push the updated cask to homebrew-glanvu:"
echo "       # In your local checkout of glanvu/homebrew-glanvu:"
echo "       cp $(pwd)/${CASK} Casks/glanvu.rb"
echo "       git add Casks/glanvu.rb"
echo "       git commit -m 'glanvu ${VERSION}: add Intel (x86_64) macOS support'"
echo "       git push"
echo ""
echo "  3. One-time trust (run this on each Mac that uses the tap):"
echo "       brew trust glanvu/glanvu"
echo ""
echo "  4. Test on an Intel Mac:"
echo "       brew untap glanvu/glanvu 2>/dev/null; brew tap glanvu/glanvu"
echo "       brew install --cask glanvu"
echo "======================================================"
