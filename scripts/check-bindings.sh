#!/usr/bin/env bash
#
# Fails when the checked-in TypeScript bindings differ from what the protocol
# crate generates. CI runs this after `cargo test`; developers should run it
# before committing a protocol change.
#
# The check compares the generated directory before and after regeneration
# rather than comparing it to git, so it works in a dirty working tree too
# (the generated files may legitimately be new and uncommitted).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

gen="apps/ui/src/gen"

snapshot() {
  find "$gen" -type f -print0 | sort -z | xargs -0 sha256sum
}

before="$(snapshot)"

cargo test -p h4rn-protocol --test bindings -- --nocapture

after="$(snapshot)"

if [ "$before" != "$after" ]; then
  echo
  echo "error: generated TypeScript bindings are out of date:" >&2
  diff <(printf '%s\n' "$before") <(printf '%s\n' "$after") >&2 || true
  echo >&2
  echo "Run 'cargo test -p h4rn-protocol' and commit the result." >&2
  exit 1
fi

echo
echo "TypeScript bindings are up to date."
