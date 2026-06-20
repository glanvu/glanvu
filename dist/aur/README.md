# Glanvu AUR Package

`PKGBUILD` for the [AUR](https://aur.archlinux.org) package `glanvu-bin` — installs the
prebuilt x86_64 release binary (no compilation).

## Setup (one-time, when publishing)

Requires an [AUR account](https://aur.archlinux.org/register) with an SSH key registered.

```bash
git clone ssh://aur@aur.archlinux.org/glanvu-bin.git
cd glanvu-bin
cp /path/to/glanvu/dist/aur/PKGBUILD .
cp /path/to/glanvu/dist/aur/.SRCINFO .
git add PKGBUILD .SRCINFO
git commit -m "Initial import: glanvu-bin <version>"
git push
```

## User installation

```bash
yay -S glanvu-bin        # or: paru -S glanvu-bin
```

## Release workflow

On each release, bump `pkgver`, reset `pkgrel=1`, update `sha256sums`, then regenerate
`.SRCINFO`:

```bash
# Update sha256sums with the Linux tarball digest from the GitHub release:
gh release view v<version> --json assets \
  --jq '.assets[] | select(.name | endswith("linux-x86_64.tar.gz")) | .digest'

makepkg --printsrcinfo > .SRCINFO   # run on an Arch system
git commit -am "Update to <version>" && git push
```

## Notes

- `.SRCINFO` here is hand-maintained to match `PKGBUILD`; the canonical way to generate it
  is `makepkg --printsrcinfo` on an Arch system. Verify before pushing if you edit PKGBUILD.
- A from-source `glanvu` package (building with cargo) could be added later for users who
  prefer compiling; `glanvu-bin` covers the common case.
