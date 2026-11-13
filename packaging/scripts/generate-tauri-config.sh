#!/usr/bin/env bash
# Write the release-only Tauri config overlay.
#
# The committed `tauri.conf.json` carries a placeholder updater key and the
# hosted/GitHub endpoint so ordinary development builds work with no secrets.
# Release builds merge this generated overlay instead:
#
#   H4RN_UPDATER_ENDPOINT   update manifest URL (hosted or self-hosted)
#   H4RN_UPDATER_PUBKEY     contents of the Tauri minisign public key
#   TAURI_SIGNING_PRIVATE_KEY
#                           when set, produce `.sig` updater artifacts
#   H4RN_WINDOWS_SIGN_COMMAND
#                           optional signtool command template (`%1` is the file)
#
# Output: apps/desktop/src-tauri/tauri.release.conf.json (gitignored).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="${ROOT}/apps/desktop/src-tauri/tauri.release.conf.json"

ENDPOINT="${H4RN_UPDATER_ENDPOINT:-https://github.com/chark1es/h4rn/releases/latest/download/latest.json}"
PUBKEY="${H4RN_UPDATER_PUBKEY:-}"
SIGNING_KEY="${TAURI_SIGNING_PRIVATE_KEY:-}"
SIGN_COMMAND="${H4RN_WINDOWS_SIGN_COMMAND:-}"

PYTHON="${PYTHON:-python3}"
if ! command -v "${PYTHON}" >/dev/null 2>&1; then
  PYTHON=python
fi

if [[ -n "${SIGNING_KEY}" && -z "${PUBKEY}" ]]; then
  printf 'error: TAURI_SIGNING_PRIVATE_KEY is set but H4RN_UPDATER_PUBKEY is empty\n' >&2
  printf '       both are required to publish updater artifacts\n' >&2
  exit 1
fi

"${PYTHON}" - "$OUT" "$ENDPOINT" "$PUBKEY" "$SIGNING_KEY" "$SIGN_COMMAND" <<'PY'
import json
import sys

out, endpoint, pubkey, signing_key, sign_command = sys.argv[1:]

config = {}

if pubkey.strip():
    config.setdefault("plugins", {})["updater"] = {
        "endpoints": [endpoint],
        "pubkey": pubkey.strip(),
        "windows": {"installMode": "passive"},
    }

bundle = {}
if signing_key.strip():
    bundle["createUpdaterArtifacts"] = True
if sign_command.strip():
    bundle.setdefault("windows", {})["signCommand"] = sign_command.strip()
if bundle:
    config["bundle"] = bundle

with open(out, "w", encoding="utf-8") as handle:
    json.dump(config, handle, indent=2, sort_keys=True)
    handle.write("\n")

print(f"wrote {out}")
if not pubkey.strip():
    print("note: H4RN_UPDATER_PUBKEY is empty; the release keeps the placeholder key and will not emit updater artifacts")
if not signing_key.strip():
    print("note: TAURI_SIGNING_PRIVATE_KEY is empty; updater artifacts are disabled")
PY
