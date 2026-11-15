#!/usr/bin/env bash
# Fill the winget, Homebrew cask and AUR templates from release artifacts.
#
#   packaging/scripts/update-manifests.sh --version 1.0.0 --dist dist --out packaging-out
#
# The output is PR-ready but never published: CI uploads it as a workflow
# artifact and a release asset so a maintainer can review and submit it.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VERSION=""
DIST="${ROOT}/dist"
OUT="${ROOT}/packaging-out"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) VERSION="${2:?}"; shift 2 ;;
    --dist) DIST="${2:?}"; shift 2 ;;
    --out) OUT="${2:?}"; shift 2 ;;
    -h|--help)
      sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
  esac
done

[[ -n "${VERSION}" ]] || {
  printf 'error: --version is required\n' >&2
  exit 2
}
[[ -d "${DIST}" ]] || {
  printf 'error: artifact directory %s does not exist\n' "${DIST}" >&2
  exit 2
}

find_artifact() {
  local pattern="$1"
  local found
  found="$(find "${DIST}" -maxdepth 3 -type f -name "${pattern}" 2>/dev/null | sort | head -n 1)"
  if [[ -z "${found}" ]]; then
    printf 'error: no artifact matching %s under %s\n' "${pattern}" "${DIST}" >&2
    return 1
  fi
  printf '%s\n' "${found}"
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

MSI="$(find_artifact '*.msi')"
DMG="$(find_artifact '*.dmg')"
DEB="$(find_artifact '*_amd64.deb' || find_artifact '*.deb')"
MSI_NAME="$(basename "${MSI}")"
DMG_NAME="$(basename "${DMG}")"
DEB_NAME="$(basename "${DEB}")"

export H4RN_VERSION="${VERSION}"
export H4RN_BASE_URL="https://github.com/chark1es/h4rn/releases/download/v${VERSION}"
export H4RN_MSI_NAME="${MSI_NAME}"
export H4RN_MSI_SHA256="$(sha256_of "${MSI}")"
export H4RN_DMG_NAME="${DMG_NAME}"
export H4RN_DMG_SHA256="$(sha256_of "${DMG}")"
export H4RN_DEB_NAME="${DEB_NAME}"
export H4RN_DEB_SHA256="$(sha256_of "${DEB}")"
export H4RN_TEMPLATES="${ROOT}/packaging"
export H4RN_OUT="${OUT}"

python3 - <<'PY'
import os
import pathlib

version = os.environ["H4RN_VERSION"]
base = os.environ["H4RN_BASE_URL"]
templates = pathlib.Path(os.environ["H4RN_TEMPLATES"])
out = pathlib.Path(os.environ["H4RN_OUT"])

values = {
    "__VERSION__": version,
    "__MSI_URL__": f"{base}/{os.environ['H4RN_MSI_NAME']}",
    "__MSI_SHA256__": os.environ["H4RN_MSI_SHA256"],
    "__DMG_NAME__": os.environ["H4RN_DMG_NAME"],
    "__DMG_SHA256__": os.environ["H4RN_DMG_SHA256"],
    "__DEB_SHA256_X86_64__": os.environ["H4RN_DEB_SHA256"],
}


def render(source: pathlib.Path, destination: pathlib.Path) -> None:
    text = source.read_text(encoding="utf-8")
    for key, value in values.items():
        text = text.replace(key, value)
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(text, encoding="utf-8")


# winget: version, locale and installer manifests, addressed by version.
winget_out = out / "winget" / "manifests" / "h" / "H4rn" / "h4rn" / version
for name in ("H4rn.h4rn.yaml", "H4rn.h4rn.locale.en-US.yaml", "H4rn.h4rn.installer.yaml"):
    render(templates / "winget" / name, winget_out / name)

# Homebrew cask.
render(templates / "homebrew" / "h4rn.rb", out / "homebrew" / "h4rn.rb")

# AUR: PKGBUILD and the generated .SRCINFO AUR expects beside it.
aur_out = out / "aur"
render(templates / "aur" / "PKGBUILD", aur_out / "PKGBUILD")
render(templates / "aur" / ".SRCINFO", aur_out / ".SRCINFO")

note = f"""h4rn {version} package manifests
=================================

These files are generated from the release artifacts and are not published.

winget
  {winget_out}
  Submit the directory as a microsoft/winget-pkgs PR.

Homebrew
  {out / 'homebrew' / 'h4rn.rb'}
  Copy into the tap (or a homebrew-cask PR).

AUR
  {aur_out}
  Push PKGBUILD and .SRCINFO to the h4rn-bin AUR repository.
"""
(out / "README.txt").write_text(note, encoding="utf-8")
print(note)
PY

printf 'manifests: %s\n' "${OUT}"
