$ErrorActionPreference = 'Stop'

$packageName = 'glanvu'
$toolsDir    = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
$url64       = 'https://github.com/glanvu/glanvu/releases/download/v0.9.0/Glanvu-0.9.0-windows-x86_64.zip'
$checksum64  = '7A42DA2CF4362BA11D91BC246BFBDB3670ECE010CB7D3D4ABF3AECE9E75492ED'

# Downloads the official release zip, verifies its SHA256, and extracts it into the package
# tools dir. Chocolatey auto-shims the extracted glanvu.exe onto the PATH as `glanvu`.
Install-ChocolateyZipPackage -PackageName $packageName `
  -Url64bit       $url64 `
  -Checksum64     $checksum64 `
  -ChecksumType64 'sha256' `
  -UnzipLocation  $toolsDir
