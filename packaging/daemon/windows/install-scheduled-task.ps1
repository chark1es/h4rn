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
  its action is a short inline recovery step (passed with -Command, never
  written to a shared file) that puts h4rnd.old.exe, or the h4rnd.previous.exe
  backup, back before starting the daemon.

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
  [string]$TaskName = "h4rn"
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

# The task action. It is inline (there is no script file to keep private) and
# uses single-quoted paths, so the only quoting that matters is PowerShell's.
$format = '$b = {0}; $p = {1}; $o = {2}; $d = {3}; ' +
  'if (-not (Test-Path -LiteralPath $b)) {{ ' +
  'if (Test-Path -LiteralPath $o) {{ Move-Item -LiteralPath $o -Destination $b -Force }} ' +
  'elseif (Test-Path -LiteralPath $p) {{ Copy-Item -LiteralPath $p -Destination $b -Force }} ' +
  '}}; & $b --data-dir $d serve; exit $LASTEXITCODE'
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

Start-ScheduledTask -TaskName $TaskName

Write-Host "Installed and started scheduled task $TaskName"
Write-Host "Data directory: $DataDir"
Write-Host "Check health:   curl.exe http://127.0.0.1:47800/healthz"
