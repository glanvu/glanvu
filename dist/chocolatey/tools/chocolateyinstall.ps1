$ErrorActionPreference = 'Stop'

$packageName = 'glanvu'
$toolsDir    = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
$url64       = 'https://github.com/glanvu/glanvu/releases/download/v0.6.1/Glanvu-0.6.1-windows-x86_64.zip'
$checksum64  = '4A433CE7185034AB9717BA644607CE46486A50E9758CE2C61FFD039FE2C79DB7'

# Downloads the official release zip, verifies its SHA256, and extracts it into the package
# tools dir. Chocolatey auto-shims the extracted glanvu.exe onto the PATH as `glanvu`.
Install-ChocolateyZipPackage -PackageName $packageName `
  -Url64bit       $url64 `
  -Checksum64     $checksum64 `
  -ChecksumType64 'sha256' `
  -UnzipLocation  $toolsDir
