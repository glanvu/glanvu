# Glanvu Scoop Bucket

This directory holds the [Scoop](https://scoop.sh) manifest for Glanvu (Windows).

## Setup (one-time, when publishing)

1. Create a public GitHub repo named `scoop-glanvu` under the `glanvu` organization.
2. Copy `bucket/glanvu.json` into a `bucket/` directory in that repo.

## User installation

```powershell
scoop bucket add glanvu https://github.com/glanvu/scoop-glanvu
scoop install glanvu
```

## Release workflow

On each release, update `version`, `hash`, and `extract_dir` in `bucket/glanvu.json`:

```powershell
# Hash of the Windows zip (from the GitHub release):
gh release view v<version> --json assets --jq '.assets[] | select(.name | endswith("windows-x86_64.zip")) | .digest'
```

`checkver` + `autoupdate` let `scoop update` and maintainers' `excavator` bots pick up
new releases automatically, so manual edits are only needed if the URL pattern changes.

## Notes

- The Windows zip nests the binary under `Glanvu-<version>\glanvu.exe`, so the manifest
  uses `extract_dir` to point Scoop at that subfolder. Keep it in sync with the version.
- The build is unsigned: Windows SmartScreen may warn on first run. Scoop installs are
  generally unaffected since the binary is run from the user's Scoop path.
