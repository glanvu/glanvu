# Glanvu Homebrew Tap

This directory contains the Homebrew cask formula for Glanvu.

## Setup (one-time, when publishing)

1. Create a public GitHub repo named `homebrew-glanvu` under the `glanvu` organization.
2. Copy `Casks/glanvu.rb` into that repo.
3. On each release, update `version` and `sha256` in `glanvu.rb`.

## User installation

```bash
brew tap glanvu/glanvu
brew install --cask glanvu
```

## Release workflow

1. Build the release binary: `make app`
2. Create a zip: `cd dist/macos && zip -r Glanvu-<version>-macos-arm64.zip Glanvu.app`
3. Upload to GitHub Releases.
4. Update `glanvu.rb` with the new version + `shasum -a 256 Glanvu-<version>-macos-arm64.zip`.
5. Push to homebrew-glanvu.
