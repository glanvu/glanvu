$ErrorActionPreference = 'Stop'

$packageName = 'glanvu'
$toolsDir    = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
$url64       = 'https://github.com/glanvu/glanvu/releases/download/v0.5.0/Glanvu-0.5.0-windows-x86_64.zip'
$checksum64  = 'E095838461DD27610A6CDB702EB3161B3883860DF38281FC1AF46C0BA7CA57DF'

# Downloads the official release zip, verifies its SHA256, and extracts it into the package
# tools dir. Chocolatey auto-shims the extracted glanvu.exe onto the PATH as `glanvu`.
Install-ChocolateyZipPackage -PackageName $packageName `
  -Url64bit       $url64 `
  -Checksum64     $checksum64 `
  -ChecksumType64 'sha256' `
  -UnzipLocation  $toolsDir
