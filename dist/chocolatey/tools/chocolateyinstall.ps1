$ErrorActionPreference = 'Stop'

$packageName = 'glanvu'
$toolsDir    = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
$url64       = 'https://github.com/glanvu/glanvu/releases/download/v0.7.0/Glanvu-0.7.0-windows-x86_64.zip'
$checksum64  = 'FF84582EC9A17004D03BDD7E7E237DE298E3CF25F006D42337A5AF44A97A00AC'

# Downloads the official release zip, verifies its SHA256, and extracts it into the package
# tools dir. Chocolatey auto-shims the extracted glanvu.exe onto the PATH as `glanvu`.
Install-ChocolateyZipPackage -PackageName $packageName `
  -Url64bit       $url64 `
  -Checksum64     $checksum64 `
  -ChecksumType64 'sha256' `
  -UnzipLocation  $toolsDir
