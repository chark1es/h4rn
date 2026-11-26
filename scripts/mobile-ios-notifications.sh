#!/usr/bin/env bash
# Add h4rn's Notification Service Extension to the generated iOS project.
#
# `tauri ios init` writes apps/mobile/src-tauri/gen/apple from Tauri's own
# XcodeGen template, and the tree is git-ignored and recreated by every init.
# This script runs after init: it writes an XcodeGen spec for the extension
# target next to the generated project.yml, includes it from there, and runs
# XcodeGen again. XcodeGen merges an included spec into the main one and
# appends arrays, so the app target gains the extension as a dependency (and
# embeds it) without this script editing Tauri's template.
#
# The extension words each alert in the phone's language (ADR-055). Its code
# and words live in apps/mobile/native/ios-notifications.
#
# Usage:
#   bun run --cwd apps/mobile tauri ios init --ci
#   scripts/mobile-ios-notifications.sh
#
# Run it again after every `tauri ios init`; running it twice is harmless.
#
# Environment:
#   APPLE_DEVELOPMENT_TEAM  team id for the extension's automatic signing; the
#                           same variable Tauri reads for the app target.
#                           Defaults to the team in the generated project, if
#                           any. Simulator builds need none.
#
# Signing material (the App ID, provisioning profiles, the APNs key) is never
# read or written here; see docs/mobile-notifications.md.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GEN_DIR="${REPO_ROOT}/apps/mobile/src-tauri/gen/apple"
SPEC="${GEN_DIR}/project.yml"
FRAGMENT="h4rn-notifications.yml"
TARGET="H4rnNotifications"
# From gen/apple to apps/mobile/native/ios-notifications; XcodeGen resolves
# an included spec's paths against the directory it lives in.
SOURCES="../../../native/ios-notifications"

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

# The value of the first `key: value` line in the spec, without quotes.
spec_value() {
  local line
  line="$(grep -m 1 -E "^[[:space:]]+$1:[[:space:]]*[^[:space:]]" "${SPEC}" || true)"
  line="${line#*:}"
  line="${line#"${line%%[![:space:]]*}"}"
  line="${line%"${line##*[![:space:]]}"}"
  line="${line#\"}"
  line="${line%\"}"
  printf '%s' "${line}"
}

[[ -f "${SPEC}" ]] ||
  die "${SPEC} does not exist; run 'bun run --cwd apps/mobile tauri ios init' first"
[[ -d "${REPO_ROOT}/apps/mobile/native/ios-notifications/NotificationService" ]] ||
  die "apps/mobile/native/ios-notifications is missing"

# Tauri names the app target `<app name>_iOS`, two spaces in under `targets:`.
app_targets="$(grep -E '^  [^[:space:]#][^:]*_iOS:[[:space:]]*$' "${SPEC}" || true)"
[[ -n "${app_targets}" ]] || die "no <name>_iOS app target in ${SPEC}"
[[ "$(printf '%s\n' "${app_targets}" | wc -l | tr -d ' ')" == "1" ]] ||
  die "more than one <name>_iOS target in ${SPEC}"
app_target="$(printf '%s' "${app_targets}" | sed -E 's/^  ([^:]+):.*$/\1/')"

# The app's own bundle id, version and build number, as Tauri wrote them. The
# extension's must match: its id is a child of the app's, and App Store
# Connect warns when an extension's version differs from its app's.
# `tauri ios build --build-number` later updates every target's build number
# together (agvtool new-version -all).
bundle_id="$(spec_value PRODUCT_BUNDLE_IDENTIFIER)"
short_version="$(spec_value CFBundleShortVersionString)"
version="$(spec_value CFBundleVersion)"
[[ -n "${bundle_id}" ]] || die "no PRODUCT_BUNDLE_IDENTIFIER in ${SPEC}"
[[ -n "${short_version}" && -n "${version}" ]] ||
  die "no CFBundleShortVersionString / CFBundleVersion in ${SPEC}"
team="${APPLE_DEVELOPMENT_TEAM:-$(spec_value DEVELOPMENT_TEAM)}"
team_setting=""
if [[ -n "${team}" ]]; then
  team_setting="        DEVELOPMENT_TEAM: \"${team}\""
fi

cat >"${GEN_DIR}/${FRAGMENT}" <<YAML
# Written by scripts/mobile-ios-notifications.sh; do not edit. Included from
# project.yml, so XcodeGen merges it into Tauri's spec (ADR-055).
targets:
  ${app_target}:
    dependencies:
      - target: ${TARGET}
  ${TARGET}:
    type: app-extension
    platform: iOS
    sources:
      - path: ${SOURCES}/Sources/H4rnAlertWords
      - path: ${SOURCES}/NotificationService
    info:
      path: ${TARGET}/Info.plist
      properties:
        CFBundleDisplayName: h4rn
        CFBundleShortVersionString: "${short_version}"
        CFBundleVersion: "${version}"
        NSExtension:
          NSExtensionPointIdentifier: com.apple.usernotifications.service
          NSExtensionPrincipalClass: \$(PRODUCT_MODULE_NAME).NotificationService
    settings:
      base:
        PRODUCT_BUNDLE_IDENTIFIER: ${bundle_id}.notifications
        PRODUCT_NAME: ${TARGET}
        SWIFT_VERSION: "5.0"
        TARGETED_DEVICE_FAMILY: "1,2"
        ENABLE_BITCODE: false
        SKIP_INSTALL: true
        CODE_SIGN_STYLE: Automatic
${team_setting}
YAML

if grep -qE '^include:' "${SPEC}"; then
  grep -qF "${FRAGMENT}" "${SPEC}" ||
    die "${SPEC} already has an include: list; add ${FRAGMENT} to it by hand"
else
  {
    printf 'include:\n  - %s\n' "${FRAGMENT}"
    cat "${SPEC}"
  } >"${SPEC}.h4rn"
  mv "${SPEC}.h4rn" "${SPEC}"
fi

command -v xcodegen >/dev/null ||
  die "xcodegen is not on PATH ('tauri ios init' installs it with Homebrew)"
(cd "${GEN_DIR}" && xcodegen generate --spec project.yml)

# Fail here, not at install time, if the merge did not embed the extension.
pbxproj="$(find "${GEN_DIR}" -maxdepth 2 -path '*.xcodeproj/project.pbxproj' | head -n 1)"
[[ -n "${pbxproj}" ]] || die "XcodeGen wrote no project.pbxproj"
grep -q "${TARGET}.appex in " "${pbxproj}" ||
  die "${app_target} does not embed ${TARGET}.appex; check ${GEN_DIR}/${FRAGMENT}"
grep -q 'com.apple.usernotifications.service' "${GEN_DIR}/${TARGET}/Info.plist" ||
  die "${TARGET}/Info.plist does not declare a notification service"

printf 'Added %s (%s.notifications) to %s\n' "${TARGET}" "${bundle_id}" "${app_target}"
