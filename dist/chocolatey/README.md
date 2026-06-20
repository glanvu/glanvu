# Glanvu Chocolatey Package

[Chocolatey](https://chocolatey.org) package for Glanvu (Windows). The package downloads the
official release zip, verifies its SHA256, and shims `glanvu.exe` onto the PATH.

## Building & publishing (per release)

Requires Chocolatey on Windows and a [community.chocolatey.org](https://community.chocolatey.org)
account + API key.

```powershell
cd dist\chocolatey
choco pack                                   # produces glanvu.<version>.nupkg
choco install glanvu -s . -y                 # local install test
choco apikey --key <YOUR_KEY> --source https://push.chocolatey.org/
choco push glanvu.<version>.nupkg --source https://push.chocolatey.org/
```

The first push goes through Chocolatey's **moderation** queue (automated + human review);
subsequent versions are usually faster.

## User installation

```powershell
choco install glanvu
```

## Release workflow

On each release, update in `tools/chocolateyinstall.ps1` and `glanvu.nuspec`:

- `$url64` → new version URL
- `$checksum64` → UPPERCASE SHA256 of the Windows zip (and `VERIFICATION.txt`)
- `<version>` in the nuspec

```powershell
gh release view v<version> --json assets `
  --jq '.assets[] | select(.name | endswith("windows-x86_64.zip")) | .digest'
```

## Notes

- `VERIFICATION.txt` + bundled `LICENSE.txt` are required by Chocolatey moderation for
  packages that redistribute binaries.
- The build is unsigned; SmartScreen may warn on first run.
