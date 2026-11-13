#!/usr/bin/env bash
# Build and stage the `h4rnd` sidecar where Tauri's externalBin expects it.
#
#   packaging/scripts/stage-sidecar.sh <target-triple|universal-apple-darwin>
#
# Linux/macOS runners use the ordinary target triple. A macOS universal build
# needs both Apple targets already built; this script lipos them into the
# `universal-apple-darwin` name Tauri looks for.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TARGET="${1:?usage: stage-sidecar.sh <target-triple|universal-apple-darwin>}"
export PATH="${HOME}/.cargo/bin:${PATH}"

case "${TARGET}" in
  universal-apple-darwin)
    X86="${ROOT}/target/x86_64-apple-darwin/release/h4rnd"
    ARM="${ROOT}/target/aarch64-apple-darwin/release/h4rnd"
    [[ -x "${X86}" && -x "${ARM}" ]] || {
      printf 'error: build both Apple targets before staging the universal sidecar\n' >&2
      exit 1
    }
    OUT="${ROOT}/apps/desktop/src-tauri/binaries/h4rnd-${TARGET}"
    lipo -create -output "${OUT}" "${X86}" "${ARM}"
    ;;
  *windows*)
    cargo build --release --locked -p h4rn-daemon --target "${TARGET}"
    OUT="${ROOT}/apps/desktop/src-tauri/binaries/h4rnd-${TARGET}.exe"
    cp "${ROOT}/target/${TARGET}/release/h4rnd.exe" "${OUT}"
    ;;
  *)
    cargo build --release --locked -p h4rn-daemon --target "${TARGET}"
    OUT="${ROOT}/apps/desktop/src-tauri/binaries/h4rnd-${TARGET}"
    cp "${ROOT}/target/${TARGET}/release/h4rnd" "${OUT}"
    ;;
esac

chmod +x "${OUT}" 2>/dev/null || true
printf 'sidecar: %s (%s bytes)\n' "${OUT}" "$(stat -c '%s' "${OUT}" 2>/dev/null || stat -f '%z' "${OUT}")"
