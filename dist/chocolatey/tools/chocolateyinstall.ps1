$ErrorActionPreference = 'Stop'

$packageName = 'glanvu'
$toolsDir    = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
$url64       = 'https://github.com/glanvu/glanvu/releases/download/v0.6.0/Glanvu-0.6.0-windows-x86_64.zip'
$checksum64  = '26BEF39481BC130DB50FF05EF8B4953A2C4621BD841E75D3C8C32A069B92E4FA'

# Downloads the official release zip, verifies its SHA256, and extracts it into the package
# tools dir. Chocolatey auto-shims the extracted glanvu.exe onto the PATH as `glanvu`.
Install-ChocolateyZipPackage -PackageName $packageName `
  -Url64bit       $url64 `
  -Checksum64     $checksum64 `
  -ChecksumType64 'sha256' `
  -UnzipLocation  $toolsDir
