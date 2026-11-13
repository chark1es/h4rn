#!/usr/bin/env bash
# Install the h4rn daemon as a per-user launch agent on macOS.
#
#   packaging/daemon/launchd/install.sh [/usr/local/bin/h4rnd]
#
# The plist template contains __HOME__ because launchd does not expand `~`.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
BINARY="${1:-/usr/local/bin/h4rnd}"
PLIST_DIR="${HOME}/Library/LaunchAgents"
LOG_DIR="${HOME}/Library/Logs/h4rn"
LABEL="dev.h4rn.h4rnd"

[[ -x "${BINARY}" ]] || {
  printf 'error: %s is not executable; pass the h4rnd path as the first argument\n' "${BINARY}" >&2
  exit 1
}

mkdir -p "${PLIST_DIR}" "${LOG_DIR}"
sed -e "s|__HOME__|${HOME}|g" \
  -e "s|/usr/local/bin/h4rnd|${BINARY}|g" \
  "${ROOT}/packaging/daemon/launchd/dev.h4rn.h4rnd.plist" \
  >"${PLIST_DIR}/${LABEL}.plist"

launchctl bootout "gui/$(id -u)/${LABEL}" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "${PLIST_DIR}/${LABEL}.plist"
launchctl kickstart -k "gui/$(id -u)/${LABEL}"

printf 'installed %s\n' "${PLIST_DIR}/${LABEL}.plist"
printf 'logs: %s\n' "${LOG_DIR}"
