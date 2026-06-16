# Glanvu Homebrew Tap

[Homebrew](https://brew.sh) tap for [Glanvu](https://glanvu.com) — fast, keyboard-driven image viewer and batch converter.

## Install

**macOS** — installs `Glanvu.app`:
```bash
brew tap glanvu/glanvu
brew install --cask glanvu
```

**Linux** — installs `glanvu` binary to PATH:
```bash
brew tap glanvu/glanvu
brew install glanvu
```

**macOS CLI-only** — binary on PATH without the `.app`:
```bash
brew tap glanvu/glanvu
brew install glanvu
```

## What's in this tap

| File | Description |
|---|---|
| `Casks/glanvu.rb` | macOS app bundle — installs `Glanvu.app` |
| `Formula/glanvu.rb` | Formula for Linux (and macOS CLI) |

## Per-release update

Update `version` and `sha256` in both files, or run from the main repo:

```bash
./scripts/bump-packaging.sh <new-version>
# then push the updated Casks/ and Formula/ to this repo
```
