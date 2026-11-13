#!/usr/bin/env bash
# Create the standalone `h4rnd` release archive for one target.
#
#   packaging/scripts/build-daemon-archives.sh <target-triple|universal-apple-darwin> [output-dir]
#
# The archive contains the binary and the service files from packaging/daemon/
# so a user can install it without cloning the repository. Checksums are
# written beside each archive and consumed by update-manifests.sh.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TARGET="${1:?usage: build-daemon-archives.sh <target-triple|universal-apple-darwin> [output-dir]}"
OUT_DIR="${2:-${ROOT}/dist/daemon}"
VERSION="${H4RN_VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' "${ROOT}/Cargo.toml" | head -n 1)}"
export PATH="${HOME}/.cargo/bin:${PATH}"

mkdir -p "${OUT_DIR}"
STAGE="$(mktemp -d)"
trap 'rm -rf "${STAGE}"' EXIT
NAME="h4rnd-v${VERSION}-${TARGET}"
BUNDLE="${STAGE}/${NAME}"
mkdir -p "${BUNDLE}/contrib"

case "${TARGET}" in
  universal-apple-darwin)
    X86="${ROOT}/target/x86_64-apple-darwin/release/h4rnd"
    ARM="${ROOT}/target/aarch64-apple-darwin/release/h4rnd"
    [[ -x "${X86}" && -x "${ARM}" ]] || {
      printf 'error: both Apple targets must be built before creating the universal archive\n' >&2
      exit 1
    }
    lipo -create -output "${BUNDLE}/h4rnd" "${X86}" "${ARM}"
    cp "${ROOT}/packaging/daemon/launchd/dev.h4rn.h4rnd.plist" "${BUNDLE}/contrib/"
    ;;
  *windows*)
    cargo build --release --locked -p h4rn-daemon --target "${TARGET}"
    cp "${ROOT}/target/${TARGET}/release/h4rnd.exe" "${BUNDLE}/"
    cp "${ROOT}/packaging/daemon/windows/install-scheduled-task.ps1" "${BUNDLE}/contrib/"
    cp "${ROOT}/packaging/daemon/windows/uninstall-scheduled-task.ps1" "${BUNDLE}/contrib/"
    ;;
  *)
    cargo build --release --locked -p h4rn-daemon --target "${TARGET}"
    cp "${ROOT}/target/${TARGET}/release/h4rnd" "${BUNDLE}/h4rnd"
    cp "${ROOT}/packaging/daemon/systemd/h4rnd.service" "${BUNDLE}/contrib/"
    ;;
esac

cat >"${BUNDLE}/README.txt" <<EOF
h4rnd ${VERSION} for ${TARGET}

Run the daemon in the foreground:

    ./h4rnd serve

Install as a background service (see contrib/ and docs/self-hosting.md):

    Linux   systemd --user:  ~/.config/systemd/user/h4rnd.service
    macOS   launchd:         ~/Library/LaunchAgents/dev.h4rn.h4rnd.plist
    Windows scheduled task:  contrib/install-scheduled-task.ps1

The data directory defaults to the platform application-data directory; use
--data-dir or H4RN_DATA_DIR to override it.
EOF

if [[ "${TARGET}" == *windows* ]]; then
  if command -v zip >/dev/null 2>&1; then
    (cd "${STAGE}" && zip -qr "${OUT_DIR}/${NAME}.zip" "${NAME}")
  else
    # Git Bash on a Windows runner has no `zip`; PowerShell needs Windows
    # paths, so convert them instead of handing it the MSYS/WSL path.
    STAGE_WIN="$(cygpath -w "${STAGE}" 2>/dev/null || wslpath -w "${STAGE}")"
    OUT_WIN="$(cygpath -w "${OUT_DIR}" 2>/dev/null || wslpath -w "${OUT_DIR}")"
    powershell.exe -NoProfile -NonInteractive -Command \
      "Compress-Archive -Path '${STAGE_WIN}\\${NAME}' -DestinationPath '${OUT_WIN}\\${NAME}.zip' -Force" \
      >/dev/null
  fi
  ARCHIVE="${OUT_DIR}/${NAME}.zip"
else
  tar -C "${STAGE}" -czf "${OUT_DIR}/${NAME}.tar.gz" "${NAME}"
  ARCHIVE="${OUT_DIR}/${NAME}.tar.gz"
fi

if command -v sha256sum >/dev/null 2>&1; then
  (cd "${OUT_DIR}" && sha256sum "$(basename "${ARCHIVE}")" >"$(basename "${ARCHIVE}").sha256")
else
  (cd "${OUT_DIR}" && shasum -a 256 "$(basename "${ARCHIVE}")" >"$(basename "${ARCHIVE}").sha256")
fi

printf 'archive: %s (%s bytes)\n' "${ARCHIVE}" "$(stat -c '%s' "${ARCHIVE}" 2>/dev/null || stat -f '%z' "${ARCHIVE}")"
