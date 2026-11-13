#!/usr/bin/env bash
#
# Fails when the checked-in TypeScript bindings differ from what the protocol
# crate generates. CI runs this after `cargo test`; developers should run it
# before committing a protocol change.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

cargo test -p h4rn-protocol --test bindings -- --nocapture

gen="apps/ui/src/gen"
status="$(git status --porcelain --untracked-files=all -- "$gen")"

if [ -n "$status" ]; then
  echo
  echo "error: generated TypeScript bindings are out of date:" >&2
  echo "$status" >&2
  echo >&2
  echo "Run 'cargo test -p h4rn-protocol' and commit the result." >&2
  exit 1
fi

echo
echo "TypeScript bindings are up to date."
