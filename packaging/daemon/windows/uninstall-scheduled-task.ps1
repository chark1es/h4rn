<#
.SYNOPSIS
  Remove the h4rn scheduled task. The data directory is left untouched.
#>
[CmdletBinding()]
param(
  [string]$TaskName = "h4rn"
)

$ErrorActionPreference = "Stop"

if (-not (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue)) {
  Write-Host "Scheduled task $TaskName is not installed"
  exit 0
}

Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
Write-Host "Removed scheduled task $TaskName"
