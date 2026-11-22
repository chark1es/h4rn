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
  a shared file). Before replacing a binary, `h4rnd update` rewrites this action
  with -NoStart, so the task can always recover a missing install path. The
  launcher waits up to 30 seconds for the updater's installation lock, then
  repairs the install path itself and starts the daemon, so it works even when
  the binary at that path is an older image without `--recover-only`.

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
  [string]$LegacyTaskName = "h4rn",
  # Rewrite the action without touching a running daemon (used by `h4rnd update`).
  [switch]$NoStart
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $Binary)) {
  throw "h4rnd.exe not found at $Binary; pass -Binary with its real path"
}

$Binary = (Resolve-Path $Binary).Path
New-Item -ItemType Directory -Force -Path $DataDir | Out-Null

$directory = Split-Path -Parent $Binary
$fileName = [IO.Path]::GetFileName($Binary)
$binaryName = [IO.Path]::GetFileNameWithoutExtension($Binary)
$binaryExt = [IO.Path]::GetExtension($Binary)
$previous = Join-Path $directory ($binaryName + ".previous" + $binaryExt)
$old = Join-Path $directory ($binaryName + ".old" + $binaryExt)
$failed = Join-Path $directory ($binaryName + ".failed" + $binaryExt)
# The same lock file `h4rnd update` takes (see update::update_lock_path).
$lock = Join-Path $directory ($binaryName + ".update" + $binaryExt + ".lock")

# Single-quoted PowerShell literals: a quote is escaped by doubling it.
$quote = { param($value) "'" + $value.Replace("'", "''") + "'" }

# The task action is inline (there is no script file to keep private) and uses
# single-quoted paths, so the only quoting that matters is PowerShell's.
#
# Recovery belongs to the launcher, not to the daemon, so it also works when the
# installed h4rnd.exe is an older image that does not understand
# `--recover-only`. It waits (bounded) for the updater's lock, then repairs the
# install path under that lock, in the same order as startup recovery: a
# moved-aside `.old` (interrupted update) or `.failed` (interrupted rollback) is
# renamed back, and a `.previous` copy is published through a staging file and a
# rename so a crash cannot leave a truncated h4rnd.exe.
$format = '$b = {0}; $p = {1}; $o = {2}; $x = {3}; $d = {4}; $l = {5}; $f = {6}; ' +
  '$deadline = (Get-Date).AddSeconds(30); $lock = $null; ' +
  'while ($null -eq $lock) {{ ' +
  'try {{ $lock = [IO.File]::Open($l, "OpenOrCreate", "ReadWrite", "None") }} ' +
  'catch [System.IO.IOException] {{ ' +
  'if ((Get-Date) -ge $deadline) {{ throw "another h4rnd update is running (lock: $l)" }} ' +
  'Start-Sleep -Milliseconds 250 ' +
  '}} ' +
  '}} ' +
  'try {{ ' +
  'if (-not (Test-Path -LiteralPath $b)) {{ ' +
  'if (Test-Path -LiteralPath $o) {{ ' +
  'Move-Item -LiteralPath $o -Destination $b -Force ' +
  '}} elseif (Test-Path -LiteralPath $x) {{ ' +
  'Move-Item -LiteralPath $x -Destination $b -Force ' +
  '}} elseif ((Test-Path -LiteralPath $p) -and ((Get-Item -LiteralPath $p).Length -gt 0)) {{ ' +
  '$t = Join-Path (Split-Path -Parent $b) ($f + ".update-" + $PID + "-0-" + ({{0:x8}} -f (Get-Random -Maximum 2147483647))); ' +
  'Copy-Item -LiteralPath $p -Destination $t -Force; ' +
  'Move-Item -LiteralPath $t -Destination $b -Force ' +
  '}} ' +
  '}} ' +
  '}} finally {{ if ($null -ne $lock) {{ $lock.Dispose() }} }} ' +
  'if (-not (Test-Path -LiteralPath $b)) {{ throw "h4rnd.exe is missing and no usable backup exists" }}; ' +
  '& $b --data-dir $d serve; exit $LASTEXITCODE'
$values = (& $quote $Binary), (& $quote $previous), (& $quote $old), (& $quote $failed), (& $quote $DataDir), (& $quote $lock), (& $quote $fileName)
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

if ($NoStart) {
  Write-Host "Updated the recovery launcher of scheduled task $TaskName (not started)"
} else {
  Start-ScheduledTask -TaskName $TaskName
  Write-Host "Installed and started scheduled task $TaskName"
}
Write-Host "Data directory: $DataDir"
Write-Host "Check health:   curl.exe http://127.0.0.1:47800/healthz"
