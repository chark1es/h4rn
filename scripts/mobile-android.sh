#!/usr/bin/env bash
# Build, install and verify the h4rn Android app from WSL2.
#
# The build runs on the Linux side: the Rust toolchain, Android SDK and NDK are
# all here, and building in place avoids mirroring the worktree to a Windows
# path (which is what apps/desktop needs only because WebKitGTK is unavailable).
# Gradle/APK artefacts go through the Windows adb.exe to the Windows emulator.
#
# Usage:
#   scripts/mobile-android.sh <command>
#
# Commands:
#   init         Generate src-tauri/gen/android and apply native/overlay files
#   build        Build debug APKs, one per ABI (see H4RN_ANDROID_TARGETS)
#   install      Install the newest APK on the running emulator/device
#   run          build + install + daemon + adb reverse + deep-link launch + screenshot
#   screenshot   Capture the emulator screen to apps/mobile/screenshots/
#   logs         Dump filtered logcat to apps/mobile/screenshots/logcat.txt
#   daemon       Start the demo daemon (fake harness) if it is not running
#
# Environment:
#   H4RN_ANDROID_TARGETS   space-separated rust targets
#                          (default: "x86_64 aarch64": emulator + phone ABIs,
#                          one APK per ABI)
#   H4RN_AVD               emulator AVD (default: Medium_Phone_API_36.0)
#   H4RN_PORT              daemon port (default: 47800)
#   H4RN_DATA_DIR          daemon data dir
#                          (default: ~/.local/share/h4rn-mobile-daemon)
#   H4RN_DAEMON_BIN        h4rnd binary (default: target/debug/h4rnd)
#   H4RN_ADB / H4RN_EMULATOR  Windows tool paths
#   H4RN_WIN_USER          Windows profile name for the default tool paths
#                          (default: $USER)
#   H4RN_ANDROID_REINIT=1  regenerate gen/android before building
#   H4RN_NO_EMULATOR=1     never start the emulator
#
# The emulator reaches the Windows host at 10.0.2.2. WSL2 forwards its loopback
# listeners to Windows localhost, so `adb reverse tcp:47800 tcp:47800` lets the
# app use ws://127.0.0.1:47800/ws through the Windows adb server, which is both
# faster than NAT and the same URL a desktop/dev browser uses.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MOBILE_DIR="${REPO_ROOT}/apps/mobile"
GEN_DIR="${MOBILE_DIR}/src-tauri/gen/android"
OVERLAY_DIR="${MOBILE_DIR}/native/android"
SCREENSHOT_DIR="${MOBILE_DIR}/screenshots"
ANDROID_SDK="${ANDROID_HOME:-${HOME}/Android/Sdk}"

# The Windows side of WSL is where Android Studio installed the device tools.
# The Windows profile name normally matches $USER; override it with
# H4RN_WIN_USER (or pass full H4RN_ADB / H4RN_EMULATOR paths) when it does not.
WIN_USER="${H4RN_WIN_USER:-${USER}}"
ADB="${H4RN_ADB:-/mnt/c/Users/${WIN_USER}/AppData/Local/Android/Sdk/platform-tools/adb.exe}"
EMULATOR="${H4RN_EMULATOR:-/mnt/c/Users/${WIN_USER}/AppData/Local/Android/Sdk/emulator/emulator.exe}"
AVD="${H4RN_AVD:-Medium_Phone_API_36.0}"
PACKAGE="dev.h4rn.app"
PORT="${H4RN_PORT:-47800}"
DATA_DIR="${H4RN_DATA_DIR:-${HOME}/.local/share/h4rn-mobile-daemon}"
DAEMON_BIN="${H4RN_DAEMON_BIN:-${REPO_ROOT}/target/debug/h4rnd}"
TARGETS="${H4RN_ANDROID_TARGETS:-x86_64 aarch64}"

log() {
  printf '\n== %s ==\n' "$*"
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

find_java() {
  if [[ -n "${JAVA_HOME:-}" && -x "${JAVA_HOME}/bin/java" ]]; then
    return
  fi
  local candidate
  candidate="$(ls -d "${HOME}"/.local/jdk/jdk-21* 2>/dev/null | sort -V | tail -n 1 || true)"
  if [[ -z "${candidate}" ]]; then
    log "Downloading Temurin JDK 21 into ~/.local/jdk"
    mkdir -p "${HOME}/.local/jdk"
    curl -fsSL --retry 3 -o /tmp/h4rn-jdk21.tar.gz \
      "https://api.adoptium.net/v3/binary/latest/21/ga/linux/x64/jdk/hotspot/normal/eclipse"
    tar -xzf /tmp/h4rn-jdk21.tar.gz -C "${HOME}/.local/jdk"
    rm -f /tmp/h4rn-jdk21.tar.gz
    candidate="$(ls -d "${HOME}"/.local/jdk/jdk-21* 2>/dev/null | sort -V | tail -n 1 || true)"
  fi
  [[ -n "${candidate}" ]] || die "could not install a JDK 21"
  JAVA_HOME="${candidate}"
  export JAVA_HOME
}

setup_env() {
  find_java
  export ANDROID_HOME="${ANDROID_SDK}"
  export ANDROID_SDK_ROOT="${ANDROID_SDK}"
  if [[ -z "${NDK_HOME:-}" ]]; then
    NDK_HOME="$(ls -d "${ANDROID_SDK}"/ndk/* 2>/dev/null | sort -V | tail -n 1 || true)"
    export NDK_HOME
  fi
  export PATH="${HOME}/.cargo/bin:${JAVA_HOME}/bin:${ANDROID_SDK}/platform-tools:${PATH}"
  export CI="${CI:-1}"
  [[ -d "${ANDROID_SDK}" ]] || die "Android SDK not found at ${ANDROID_SDK}"
  [[ -n "${NDK_HOME:-}" && -d "${NDK_HOME}" ]] || die "Android NDK not found under ${ANDROID_SDK}/ndk"
  command -v bun >/dev/null || die "bun is not on PATH"
  command -v cargo >/dev/null || die "cargo is not on PATH"
}

rust_target_triple() {
  case "$1" in
    x86_64) printf 'x86_64-linux-android' ;;
    aarch64) printf 'aarch64-linux-android' ;;
    armv7) printf 'armv7-linux-androideabi' ;;
    i686) printf 'i686-linux-android' ;;
    *) die "unknown Android target $1 (expected x86_64, aarch64, armv7 or i686)" ;;
  esac
}

ensure_rust_targets() {
  local target triple installed
  installed="$(rustup target list --installed)"
  for target in ${TARGETS}; do
    triple="$(rust_target_triple "${target}")"
    if ! grep -qx "${triple}" <<<"${installed}"; then
      log "Installing Rust target ${triple}"
      rustup target add "${triple}"
    fi
  done
}

apply_native_overlay() {
  [[ -d "${OVERLAY_DIR}/app" ]] || die "native overlay is missing at ${OVERLAY_DIR}/app"
  # The overlay owns the manifest, MainActivity, Keystore plugin and network
  # security policy. The overlay manifest carries the h4rn:// intent filter
  # inside the deep-link plugin's AUTO-GENERATED comment block: the plugin
  # rewrites that block when its build script runs, and the filter stays
  # present when an up-to-date cargo build skips the plugin entirely (cargo
  # build scripts are not rerun just because the generated manifest changed).
  cp -a "${OVERLAY_DIR}/app/." "${GEN_DIR}/app/"
}

ensure_android_project() {
  if [[ ! -d "${GEN_DIR}" || "${H4RN_ANDROID_REINIT:-0}" == "1" ]]; then
    log "Generating the Tauri Android project"
    rm -rf "${GEN_DIR}"
    (cd "${MOBILE_DIR}" && bun run tauri android init --ci)
  fi
  apply_native_overlay
}

build_apk() {
  setup_env
  ensure_rust_targets
  ensure_android_project

  local -a targets
  read -r -a targets <<<"${TARGETS}"
  local -a args=(--debug --apk --ci)
  args+=(--target "${targets[@]}")
  if [[ "${#targets[@]}" -gt 1 ]]; then
    args+=(--split-per-abi)
  fi

  log "Building the debug APK (${TARGETS})"
  (cd "${MOBILE_DIR}" && bun run tauri android build "${args[@]}")

  local apk
  apk="$(newest_apk || true)"
  [[ -n "${apk}" ]] || die "the build produced no APK"
  log "APK ${apk}"
}

newest_apk() {
  local preferred="${1:-}"
  local found
  if [[ -n "${preferred}" ]]; then
    found="$(find "${GEN_DIR}/app/build/outputs/apk" -type f -name "*${preferred}*.apk" \
      -printf '%T@ %p\n' 2>/dev/null | sort -nr | awk 'NR == 1 { print $2 }')"
    if [[ -n "${found}" ]]; then
      printf '%s\n' "${found}"
      return
    fi
  fi
  find "${GEN_DIR}/app/build/outputs/apk" -type f -name '*.apk' \
    -printf '%T@ %p\n' 2>/dev/null | sort -nr | awk 'NR == 1 { print $2 }'
}

device_connected() {
  "${ADB}" start-server >/dev/null 2>&1 || true
  # Windows adb.exe emits CRLF; strip the CR so the state column matches.
  "${ADB}" devices | tr -d '\r' | awk '$2 == "device" { found = 1 } END { exit !found }'
}

wait_for_boot() {
  local deadline=$((SECONDS + 240))
  while ((SECONDS < deadline)); do
    if [[ "$("${ADB}" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" == "1" ]]; then
      return
    fi
    sleep 2
  done
  die "the emulator did not finish booting"
}

ensure_device() {
  if device_connected; then
    log "Using the connected Android device"
    return
  fi
  [[ "${H4RN_NO_EMULATOR:-0}" == "1" ]] && die "no Android device is connected"

  log "Starting the ${AVD} emulator"
  nohup "${EMULATOR}" -avd "${AVD}" -no-snapshot-save \
    >/tmp/h4rn-emulator.log 2>&1 &
  wait_for_boot
}

install_apk() {
  ensure_device
  local apk="${H4RN_APK:-$(newest_apk x86_64)}"
  if [[ -z "${apk}" ]]; then
    apk="$(newest_apk)"
  fi
  [[ -n "${apk}" ]] || die "no APK found; run the build command first"
  log "Installing $(basename "${apk}")"
  # A Windows adb.exe cannot open a Linux path, and `adb install` on one hangs
  # instead of failing. Stage the APK under the Windows temp dir when the adb
  # binary is a /mnt/c path, then install from the Windows path.
  local install_path="${apk}"
  if [[ "${ADB}" == /mnt/* ]]; then
    local win_temp="/mnt/c/Users/${WIN_USER}/AppData/Local/Temp/h4rn-mobile"
    mkdir -p "${win_temp}"
    cp -f "${apk}" "${win_temp}/app.apk"
    install_path="C:\\Users\\${WIN_USER}\\AppData\\Local\\Temp\\h4rn-mobile\\app.apk"
  fi
  "${ADB}" install -r -t "${install_path}"
}

daemon_healthy() {
  curl -fsS -m 2 -o /dev/null "http://127.0.0.1:${PORT}/healthz" 2>/dev/null
}

ensure_daemon() {
  mkdir -p "${DATA_DIR}"
  if daemon_healthy; then
    log "Daemon already listening on 127.0.0.1:${PORT}"
  else
    if [[ ! -x "${DAEMON_BIN}" ]]; then
      log "Building h4rnd"
      (cd "${REPO_ROOT}" && cargo build -p h4rn-daemon)
    fi
    log "Starting the demo daemon on 127.0.0.1:${PORT} (fake harness)"
    (cd "${REPO_ROOT}" && H4RN_FAKE_HARNESS=1 nohup "${DAEMON_BIN}" \
      --data-dir "${DATA_DIR}" --bind "127.0.0.1:${PORT}" serve \
      >"${DATA_DIR}/daemon.log" 2>&1 &)
    local deadline=$((SECONDS + 30))
    while ((SECONDS < deadline)); do
      if daemon_healthy; then
        break
      fi
      sleep 1
    done
    daemon_healthy || {
      tail -n 40 "${DATA_DIR}/daemon.log" >&2 || true
      die "the daemon did not answer on 127.0.0.1:${PORT}"
    }
  fi

  TOKEN="$("${DAEMON_BIN}" --data-dir "${DATA_DIR}" token)"
  [[ -n "${TOKEN}" ]] || die "could not read the daemon access token"
}

urlencode() {
  python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1], safe=""))' "$1"
}

wire_reverse() {
  log "Wiring adb reverse tcp:${PORT} tcp:${PORT}"
  "${ADB}" reverse "tcp:${PORT}" "tcp:${PORT}" >/dev/null
  "${ADB}" reverse --list

  # WSL2's localhost forwarding normally makes the WSL listener visible to the
  # Windows adb server. Surface the check instead of failing later inside the
  # WebView, where the only signal is an offline banner.
  if command -v powershell.exe >/dev/null; then
    local status
    status="$(powershell.exe -NoProfile -Command \
      "& curl.exe -s -m 3 -o NUL -w '%{http_code}' http://127.0.0.1:${PORT}/healthz" \
      2>/dev/null | tr -d '\r' || true)"
    if [[ "${status}" != "200" ]]; then
      printf 'warning: Windows cannot reach 127.0.0.1:%s (%s). Check WSL localhostForwarding.\n' \
        "${PORT}" "${status:-no response}" >&2
    fi
  fi
}

launch_with_pairing() {
  local daemon_url="ws://127.0.0.1:${PORT}/ws"
  local deep_link="h4rn://connect?daemon=$(urlencode "${daemon_url}")&token=${TOKEN}"
  log "Launching through ${daemon_url}"
  "${ADB}" shell "am force-stop ${PACKAGE}" >/dev/null 2>&1 || true
  if ! "${ADB}" shell "am start -W -a android.intent.action.VIEW -c android.intent.category.BROWSABLE -d '${deep_link}' ${PACKAGE}" 2>&1 | tee /tmp/h4rn-am-start.log | grep -q "Status: ok"; then
    printf 'warning: the h4rn:// deep link did not launch the app; falling back to the launcher\n' >&2
    "${ADB}" shell monkey -p "${PACKAGE}" -c android.intent.category.LAUNCHER 1 >/dev/null
  fi
}

screenshot() {
  ensure_device
  mkdir -p "${SCREENSHOT_DIR}"
  local name="${1:-mobile-$(date +%Y%m%d-%H%M%S).png}"
  [[ "${name}" == *.png ]] || name="${name}.png"
  local path="${SCREENSHOT_DIR}/${name}"
  # exec-out keeps the PNG bytes intact through the Windows adb.exe interop.
  if ! "${ADB}" exec-out screencap -p >"${path}" || [[ ! -s "${path}" ]]; then
    "${ADB}" shell screencap -p /sdcard/h4rn-screen.png
    "${ADB}" pull /sdcard/h4rn-screen.png "${path}" >/dev/null
  fi
  printf 'Screenshot: %s\n' "${path}"
}

dump_logs() {
  ensure_device
  mkdir -p "${SCREENSHOT_DIR}"
  local path="${SCREENSHOT_DIR}/logcat.txt"
  "${ADB}" logcat -d -v threadtime 2>/dev/null |
    grep -Ei 'h4rn|tauri|chromium|AndroidRuntime|RustStdoutStderr|SecretStore' |
    tail -n 600 >"${path}" || true
  printf 'Logcat: %s\n' "${path}"
  tail -n 60 "${path}" || true
}

command="${1:-run}"
case "${command}" in
  init)
    setup_env
    ensure_android_project
    ;;
  build)
    build_apk
    ;;
  install)
    setup_env
    install_apk
    ;;
  run)
    build_apk
    ensure_device
    ensure_daemon
    wire_reverse
    install_apk
    launch_with_pairing
    log "Waiting for the WebView to connect"
    sleep 8
    screenshot "mobile-emulator.png"
    dump_logs
    ;;
  screenshot)
    setup_env
    screenshot "${2:-mobile-$(date +%Y%m%d-%H%M%S).png}"
    ;;
  logs)
    dump_logs
    ;;
  daemon)
    setup_env
    ensure_daemon
    printf 'Token: %s\n' "${TOKEN}"
    ;;
  *)
    die "unknown command ${command}; expected init, build, install, run, screenshot, logs or daemon"
    ;;
esac
