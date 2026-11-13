#!/usr/bin/env bash
# Build and verify the h4rn Windows desktop app (Tauri v2) from WSL2.
#
# Tauri cannot be built on Linux in this environment (no WebKitGTK), and the
# Windows toolchain cannot build from a \\wsl$ UNC path, so this script mirrors
# the repository into a Windows-local directory and drives cargo/bun there.
#
# Usage:
#   scripts/desktop-build-windows.sh [all|sync|lint|build|verify]
#
# Environment:
#   H4RN_WIN_BUILD_DIR        Windows-local build directory
#                             (default: /mnt/c/Users/chark1es/h4rn-build)
#   H4RN_WIN_TOOLCHAIN        rustup toolchain override for Windows; empty uses
#                             rust-toolchain.toml (1.99.1)
#   H4RN_CARGO / H4RN_BUN     explicit Windows executables
#   H4RN_SKIP_CLIPPY=1        skip fmt/clippy (faster iteration)
#
# Artifacts land in <build dir>/apps/desktop/src-tauri/target/release/bundle.
# Verification launches the built exe, waits for the sidecar daemon on
# 127.0.0.1:47800, screenshots the window, closes the app, and confirms the
# sidecar exited.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WIN_BUILD_DIR="${H4RN_WIN_BUILD_DIR:-/mnt/c/Users/chark1es/h4rn-build}"
WIN_CARGO="${H4RN_CARGO:-C:\\Users\\chark1es\\.cargo\\bin\\cargo.exe}"
WIN_BUN="${H4RN_BUN:-C:\\Users\\chark1es\\.bun\\bin\\bun.exe}"
TARGET_TRIPLE="x86_64-pc-windows-msvc"
SIDECAR="${WIN_BUILD_DIR}/apps/desktop/src-tauri/binaries/h4rnd-${TARGET_TRIPLE}.exe"
SCREENSHOT_NAME="desktop-windows.png"

if [[ ! -d "${WIN_BUILD_DIR}" ]]; then
  mkdir -p "${WIN_BUILD_DIR}"
fi
WIN_PATH="$(wslpath -w "${WIN_BUILD_DIR}")"
WIN_PATH="${WIN_PATH%\\}"
APP_EXE="${WIN_PATH}\\apps\\desktop\\src-tauri\\target\\release\\h4rn.exe"
SCREENSHOT_WIN="${WIN_PATH}\\apps\\desktop\\screenshots\\${SCREENSHOT_NAME}"
SHELL_LOG_WIN="${LOCALAPPDATA_WIN:-C:\\Users\\chark1es\\AppData\\Local}\\dev.h4rn.desktop\\logs\\shell.log"

log() {
  printf '\n== %s ==\n' "$*"
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

toolchain_prefix() {
  if [[ -n "${H4RN_WIN_TOOLCHAIN:-}" ]]; then
    printf 'set RUSTUP_TOOLCHAIN=%s && ' "${H4RN_WIN_TOOLCHAIN}"
  fi
}

win() {
  local command="$1"
  (cd "${WIN_BUILD_DIR}" && cmd.exe /c "cd /d ${WIN_PATH} && $(toolchain_prefix)${command}")
}

powershell() {
  (cd "${WIN_BUILD_DIR}" && powershell.exe -NoProfile -NonInteractive -Command "$1")
}

sync_tree() {
  log "Mirroring ${REPO_ROOT} to ${WIN_BUILD_DIR}"
  # rsync is not installed in this WSL image, so stream a filtered tar. Files
  # deleted in the repo are not removed from the build tree (build outputs are
  # preserved anyway); Cargo and Vite only read what they need.
  tar -C "${REPO_ROOT}" \
    --exclude='target' \
    --exclude='node_modules' \
    --exclude='.git' \
    --exclude='dist' \
    --exclude='binaries/h4rnd-*' \
    --exclude='*/binaries/h4rnd-*' \
    --exclude='desktop-windows.png' \
    -cf - . | tar -C "${WIN_BUILD_DIR}" -xf -
}

format_and_lint() {
  if [[ "${H4RN_SKIP_CLIPPY:-0}" == "1" ]]; then
    log "Skipping fmt/clippy (H4RN_SKIP_CLIPPY=1)"
    return
  fi
  log "cargo fmt --check (desktop crate)"
  win "${WIN_CARGO} fmt --check --manifest-path apps/desktop/src-tauri/Cargo.toml"
  log "cargo clippy -D warnings (desktop crate)"
  win "${WIN_CARGO} clippy --manifest-path apps/desktop/src-tauri/Cargo.toml --all-targets -- -D warnings"
}

build_sidecar() {
  log "Building h4rnd.exe (${TARGET_TRIPLE}, release)"
  win "${WIN_CARGO} build --release --manifest-path crates/h4rn-daemon/Cargo.toml"
  [[ -f "${WIN_BUILD_DIR}/target/release/h4rnd.exe" ]] || die "h4rnd.exe was not produced"
  cp "${WIN_BUILD_DIR}/target/release/h4rnd.exe" "${SIDECAR}"
  log "Sidecar staged at apps/desktop/src-tauri/binaries/$(basename "${SIDECAR}")"
}

build_app() {
  log "Installing JS workspace dependencies (bun)"
  win "${WIN_BUN} install --frozen-lockfile" || {
    printf 'warning: frozen install failed; retrying a plain install\n' >&2
    win "${WIN_BUN} install"
  }

  log "Building the UI and the Tauri bundle (msi + nsis)"
  win "${WIN_BUN} run --cwd apps/desktop build"

  log "Artifacts"
  find "${WIN_BUILD_DIR}/apps/desktop/src-tauri/target/release/bundle" \
    -maxdepth 2 -type f \( -name '*.msi' -o -name '*.exe' \) -print 2>/dev/null || true
  ls -l "${WIN_BUILD_DIR}/apps/desktop/src-tauri/target/release/h4rn.exe"
}

owned_daemon_pids() {
  powershell "Get-CimInstance Win32_Process -Filter \"Name = 'h4rnd.exe'\" | Where-Object { \$_.ExecutablePath -like '${WIN_PATH}\\*' } | ForEach-Object { \$_.ProcessId }"
}

stop_owned_daemons() {
  powershell "Get-CimInstance Win32_Process -Filter \"Name = 'h4rnd.exe'\" | Where-Object { \$_.ExecutablePath -like '${WIN_PATH}\\*' } | ForEach-Object { Stop-Process -Id \$_.ProcessId -Force -ErrorAction SilentlyContinue }" >/dev/null 2>&1 || true
}

health_status() {
  powershell "& curl.exe -s -m 2 -o NUL -w '%{http_code}' http://127.0.0.1:47800/healthz" || true
}

verify() {
  log "Verifying the built app"

  # A daemon from an earlier run would make the sidecar look healthy when it is
  # not the one this launch spawned. Stop only daemons living inside the build
  # tree; never touch the user's own h4rnd.
  local stale
  stale="$(owned_daemon_pids | tr -d '\r' || true)"
  if [[ -n "${stale}" ]]; then
    printf 'Stopping stale build-tree daemon(s): %s\n' "${stale}"
    stop_owned_daemons
    sleep 1
  fi

  local status
  status="$(health_status | tr -d '\r')"
  if [[ "${status}" == "200" ]]; then
    die "something is already answering on 127.0.0.1:47800; stop it before verifying the sidecar"
  fi

  log "Launching ${APP_EXE}"
  # `cmd /c start` blocks under WSL interop because the GUI process inherits
  # the console handles; Start-Process returns immediately.
  powershell "Start-Process -FilePath '${APP_EXE}'"

  local ready=""
  for _ in $(seq 1 60); do
    status="$(health_status | tr -d '\r' || true)"
    if [[ "${status}" == "200" ]]; then
      ready="yes"
      break
    fi
    sleep 1
  done
  [[ -n "${ready}" ]] || die "the sidecar daemon did not answer on 127.0.0.1:47800 within 60s"

  log "Sidecar health"
  powershell "& curl.exe -s http://127.0.0.1:47800/healthz"
  printf '\n'
  powershell "Get-CimInstance Win32_Process -Filter \"Name = 'h4rnd.exe'\" | Select-Object ProcessId, ExecutablePath | Format-List"

  mkdir -p "${WIN_BUILD_DIR}/apps/desktop/screenshots"
  local script="${WIN_BUILD_DIR}/apps/desktop/screenshots/capture.ps1"
  cat > "${script}" <<'POWERSHELL'
param([Parameter(Mandatory = $true)][string]$Out)
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class H4rnWindow {
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
  public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
}
"@

$process = Get-Process h4rn | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if (-not $process) { throw "the h4rn window was not found" }

[void][H4rnWindow]::SetForegroundWindow($process.MainWindowHandle)
Start-Sleep -Milliseconds 900

$rect = New-Object H4rnWindow+RECT
[void][H4rnWindow]::GetWindowRect($process.MainWindowHandle, [ref]$rect)
$width = $rect.Right - $rect.Left
$height = $rect.Bottom - $rect.Top
if ($width -le 0 -or $height -le 0) { throw "the h4rn window has no area" }

$bitmap = New-Object System.Drawing.Bitmap $width, $height
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.CopyFromScreen($rect.Left, $rect.Top, 0, 0, $bitmap.Size)
$bitmap.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$graphics.Dispose()
$bitmap.Dispose()
Write-Output $Out
POWERSHELL

  log "Capturing the window"
  powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$(wslpath -w "${script}")" -Out "${SCREENSHOT_WIN}"
  [[ -f "${WIN_BUILD_DIR}/apps/desktop/screenshots/${SCREENSHOT_NAME}" ]] || die "screenshot was not written"
  mkdir -p "${REPO_ROOT}/apps/desktop/screenshots"
  cp "${WIN_BUILD_DIR}/apps/desktop/screenshots/${SCREENSHOT_NAME}" \
    "${REPO_ROOT}/apps/desktop/screenshots/${SCREENSHOT_NAME}"

  log "Closing the app and checking that the sidecar exits"
  powershell "Get-Process h4rn | ForEach-Object { [void]\$_.CloseMainWindow() }"

  local closed=""
  for _ in $(seq 1 15); do
    if [[ -z "$(powershell "Get-Process h4rn -ErrorAction SilentlyContinue" || true)" ]]; then
      closed="yes"
      break
    fi
    sleep 1
  done
  [[ -n "${closed}" ]] || die "h4rn did not close after CloseMainWindow"

  local lingering=""
  for _ in $(seq 1 10); do
    lingering="$(powershell "Get-Process h4rnd -ErrorAction SilentlyContinue" || true)"
    [[ -z "${lingering}" ]] && break
    sleep 1
  done
  if [[ -n "${lingering}" ]]; then
    printf 'warning: h4rnd is still running after the shell exited:\n%s\n' "${lingering}" >&2
    powershell "Get-Process h4rnd -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue"
    die "the sidecar did not exit with the shell (leftover process killed)"
  fi
}

copy_diagnostics() {
  local log_path
  log_path="$(wslpath -u "${SHELL_LOG_WIN}" 2>/dev/null || true)"
  if [[ -n "${log_path}" && -f "${log_path}" ]]; then
    cp "${log_path}" "${WIN_BUILD_DIR}/shell-verification.log"
    printf 'Shell log copied to %s\n' "${WIN_BUILD_DIR}/shell-verification.log"
  fi
}

case "${1:-all}" in
  sync)
    sync_tree
    ;;
  build)
    sync_tree
    format_and_lint
    build_sidecar
    build_app
    ;;
  lint)
    sync_tree
    format_and_lint
    ;;
  verify)
    verify
    copy_diagnostics
    ;;
  all)
    sync_tree
    format_and_lint
    build_sidecar
    build_app
    verify
    copy_diagnostics
    log "Done"
    printf 'Screenshot: %s/apps/desktop/screenshots/%s\n' "${REPO_ROOT}" "${SCREENSHOT_NAME}"
    ;;
  *)
    die "unknown step ${1}; expected all, sync, lint, build or verify"
    ;;
esac
