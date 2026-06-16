# Glanvu winget Manifests

[winget](https://learn.microsoft.com/windows/package-manager/) manifests for Glanvu
(Windows). Unlike Homebrew/Scoop, these are **submitted by pull request** to the central
[microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs) repo — there is no
self-hosted bucket.

## Submitting (per release)

1. Validate locally (requires winget on Windows 10/11):

   ```powershell
   winget validate --manifest dist\winget\manifests\g\Glanvu\Glanvu\0.5.0
   winget install --manifest dist\winget\manifests\g\Glanvu\Glanvu\0.5.0   # test install
   ```

2. Fork `microsoft/winget-pkgs`, copy the version folder to the same path in the fork:
   `manifests/g/Glanvu/Glanvu/0.5.0/`, and open a PR.

   Easiest path: use [`wingetcreate`](https://github.com/microsoft/winget-create):

   ```powershell
   wingetcreate update Glanvu.Glanvu --version 0.5.0 `
     --urls https://github.com/glanvu/glanvu/releases/download/v0.5.0/Glanvu-0.5.0-windows-x86_64.zip `
     --submit
   ```

3. Microsoft's bots validate the PR; once merged, users can:

   ```powershell
   winget install Glanvu.Glanvu
   ```

## Notes

- The Windows zip nests the binary under `Glanvu-<version>\glanvu.exe`, so the installer
  manifest uses `InstallerType: zip` + `NestedInstallerType: portable` with the relative
  path. Update the `RelativeFilePath` version if the zip layout changes.
- `InstallerSha256` must be UPPERCASE (winget requirement). Regenerate per release.
- The build is unsigned; SmartScreen may warn on first run. winget itself installs fine.
