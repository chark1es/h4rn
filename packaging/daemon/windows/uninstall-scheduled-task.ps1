<#
.SYNOPSIS
  Remove the h4rn scheduled task. The data directory is left untouched.

.DESCRIPTION
  Removes this account's task and the machine-wide `h4rn` task that releases
  before per-account names may have registered. Both are removed even when only
  one of them exists.
#>
[CmdletBinding()]
param(
  [string]$TaskName = "h4rn",
  [string]$LegacyTaskName = "h4rn"
)

$ErrorActionPreference = "Stop"

$targets = @($TaskName, $LegacyTaskName) | Where-Object { $_ } | Select-Object -Unique
$removed = @()
foreach ($name in $targets) {
  if (Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue) {
    Stop-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName $name -Confirm:$false
    $removed += $name
  }
}

if ($removed.Count -eq 0) {
  Write-Host "Scheduled task $TaskName is not installed"
  exit 0
}

Write-Host ("Removed scheduled task(s): " + ($removed -join ", "))
