<#
.SYNOPSIS
  Install h4rnd as a per-user Windows scheduled task (runs at logon).

.DESCRIPTION
  h4rnd is a console process, not a Windows service: it does not implement the
  Service Control Manager protocol, so `sc create`/`New-Service` would fail with
  error 1053. A scheduled task is the supported way to keep a user-context
  daemon running on Windows without a service wrapper such as NSSM or WinSW.

  The task runs in the current user's interactive session, which is what the
  managed coding agents need for credentials and filesystem access. It starts
  at logon and restarts on failure.

  Task names are machine-wide, so `h4rnd service install` passes a per-account
  name (`h4rn-<user>`).

  A Windows update cannot replace a running executable: it moves h4rnd.exe to
  h4rnd.old.exe and then moves the new binary in. If that is interrupted,
  h4rnd.exe is briefly absent, so the task does not run the binary directly:
  its action is a short inline launcher (passed with -Command, never written to
  a shared file). The launcher runs the daemon's own `--recover-only`, which
  takes the same installation lock as an update and publishes a recovered
  binary with a rename, then starts the recovered h4rnd.exe.

  Usage:
    .\install-scheduled-task.ps1 -Binary "C:\Program Files\h4rn\h4rnd.exe" `
      -DataDir "$env:LOCALAPPDATA\h4rn"

  Run without elevation. To keep the daemon running after logout, use a real
  service wrapper instead (the binary must then be launched in a service-aware
  mode), or run the task under a dedicated account with "Run whether user is
  logged on or not".
#>
[CmdletBinding()]
param(
  [string]$Binary = (Join-Path $PSScriptRoot "..\h4rnd.exe"),
  [string]$DataDir = (Join-Path $env:LOCALAPPDATA "h4rn"),
  [string]$TaskName = "h4rn",
  [string]$LegacyTaskName = "h4rn"
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $Binary)) {
  throw "h4rnd.exe not found at $Binary; pass -Binary with its real path"
}

$Binary = (Resolve-Path $Binary).Path
New-Item -ItemType Directory -Force -Path $DataDir | Out-Null

$directory = Split-Path -Parent $Binary
$binaryName = [IO.Path]::GetFileNameWithoutExtension($Binary)
$binaryExt = [IO.Path]::GetExtension($Binary)
$previous = Join-Path $directory ($binaryName + ".previous" + $binaryExt)
$old = Join-Path $directory ($binaryName + ".old" + $binaryExt)

# Single-quoted PowerShell literals: a quote is escaped by doubling it.
$quote = { param($value) "'" + $value.Replace("'", "''") + "'" }

# The task action is inline (there is no script file to keep private) and uses
# single-quoted paths, so the only quoting that matters is PowerShell's.
#
# Recovery is not a PowerShell copy any more: it runs the daemon's own
# `--recover-only`, which takes the same installation lock as `h4rnd update` and
# publishes a restored binary with a rename. If h4rnd.exe was moved aside, the
# moved-aside image performs that recovery and the task then starts the install
# path it recovered.
$format = '$b = {0}; $p = {1}; $o = {2}; $d = {3}; ' +
  '$launcher = if (Test-Path -LiteralPath $b) {{ $b }} ' +
  'elseif (Test-Path -LiteralPath $o) {{ $o }} ' +
  'elseif (Test-Path -LiteralPath $p) {{ $p }} ' +
  'else {{ throw "h4rnd.exe is missing and no backup exists" }}; ' +
  '& $launcher --recover-only; if ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}; ' +
  '& $b --data-dir $d serve; exit $LASTEXITCODE'
$values = (& $quote $Binary), (& $quote $previous), (& $quote $old), (& $quote $DataDir)
$command = $format -f $values

$powershell = Join-Path $env:SystemRoot "System32\WindowsPowerShell\v1.0\powershell.exe"
$action = New-ScheduledTaskAction `
  -Execute $powershell `
  -Argument ("-NoProfile -NonInteractive -ExecutionPolicy Bypass -Command " + $command) `
  -WorkingDirectory $directory
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$settings = New-ScheduledTaskSettingsSet `
  -AllowStartIfOnBatteries `
  -DontStopIfGoingOnBatteries `
  -ExecutionTimeLimit ([TimeSpan]::Zero) `
  -RestartCount 3 `
  -RestartInterval (New-TimeSpan -Minutes 1)
$principal = New-ScheduledTaskPrincipal `
  -UserId "$env:USERDOMAIN\$env:USERNAME" `
  -LogonType Interactive `
  -RunLevel Limited

Register-ScheduledTask `
  -TaskName $TaskName `
  -Action $action `
  -Trigger $trigger `
  -Settings $settings `
  -Principal $principal `
  -Description "Local h4rn daemon: supervises coding-agent harnesses and serves the h4rn protocol on loopback." `
  -Force | Out-Null

# Older releases used one machine-wide `h4rn` task. Migrate it now that the
# per-account task is registered, so only one daemon starts at logon.
if ($LegacyTaskName -and $LegacyTaskName -ne $TaskName) {
  if (Get-ScheduledTask -TaskName $LegacyTaskName -ErrorAction SilentlyContinue) {
    Stop-ScheduledTask -TaskName $LegacyTaskName -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName $LegacyTaskName -Confirm:$false
    Write-Host "Removed legacy scheduled task $LegacyTaskName"
  }
}

Start-ScheduledTask -TaskName $TaskName

Write-Host "Installed and started scheduled task $TaskName"
Write-Host "Data directory: $DataDir"
Write-Host "Check health:   curl.exe http://127.0.0.1:47800/healthz"
