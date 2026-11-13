<#
.SYNOPSIS
  Sign one Windows artifact with a PFX from CI secrets.

.DESCRIPTION
  Tauri invokes this through `bundle.windows.signCommand` with the artifact path
  as its only argument. When WINDOWS_CERTIFICATE_BASE64 is absent the script
  leaves the artifact unsigned and exits successfully, so PR and local builds
  keep working without the owner's certificate.

  Required environment for a signed release:
    WINDOWS_CERTIFICATE_BASE64  base64 of the .pfx
    WINDOWS_CERTIFICATE_PASSWORD

  Optional:
    WINDOWS_TIMESTAMP_URL       default http://timestamp.digicert.com
#>
[CmdletBinding()]
param(
  [Parameter(Mandatory = $true, Position = 0)]
  [string]$Path
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $Path)) {
  throw "artifact not found: $Path"
}

if ([string]::IsNullOrWhiteSpace($env:WINDOWS_CERTIFICATE_BASE64)) {
  Write-Host "WINDOWS_CERTIFICATE_BASE64 is not set; leaving $Path unsigned"
  exit 0
}
if ([string]::IsNullOrWhiteSpace($env:WINDOWS_CERTIFICATE_PASSWORD)) {
  throw "WINDOWS_CERTIFICATE_PASSWORD must be set when WINDOWS_CERTIFICATE_BASE64 is set"
}

$signtool = Get-Command signtool.exe -ErrorAction SilentlyContinue
if (-not $signtool) {
  $kits = "${env:ProgramFiles(x86)}\Windows Kits\10\bin"
  $signtool = Get-ChildItem -Path $kits -Filter signtool.exe -Recurse -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match '\\x64\\signtool\.exe$' } |
    Sort-Object FullName -Descending |
    Select-Object -First 1
}
if (-not $signtool) {
  throw "signtool.exe was not found; install the Windows SDK signing tools"
}

$pfx = Join-Path ([System.IO.Path]::GetTempPath()) ("h4rn-sign-{0}.pfx" -f [guid]::NewGuid())
try {
  [System.IO.File]::WriteAllBytes($pfx, [Convert]::FromBase64String($env:WINDOWS_CERTIFICATE_BASE64))
  $timestamp = if ($env:WINDOWS_TIMESTAMP_URL) { $env:WINDOWS_TIMESTAMP_URL } else { "http://timestamp.digicert.com" }

  & $signtool.FullName sign `
    /f $pfx `
    /p $env:WINDOWS_CERTIFICATE_PASSWORD `
    /fd SHA256 `
    /tr $timestamp `
    /td SHA256 `
    /a `
    $Path
  if ($LASTEXITCODE -ne 0) {
    throw "signtool failed with exit code $LASTEXITCODE for $Path"
  }
  Write-Host "signed $Path"
}
finally {
  Remove-Item -Force $pfx -ErrorAction SilentlyContinue
}
