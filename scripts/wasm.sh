#!/usr/bin/env bash
#
# Builds, or checks the freshness of, the committed browser transport wasm in
# apps/ui/src/gen/transport-wasm.
#
#   scripts/wasm.sh build     # rebuild and overwrite the committed files
#   scripts/wasm.sh check     # rebuild into a temp dir and fail on any diff
#   scripts/wasm.sh version   # print the pinned wasm-bindgen-cli version
#
# The artifact is only reproducible with the same toolchain, so the script pins
# the `wasm-bindgen` CLI (it must match the `wasm-bindgen` crate in Cargo.lock)
# and remaps the machine-specific cargo/checkout paths that rustc would
# otherwise embed in the panic strings. It does not run wasm-opt.
#
# CI runs `check`; to install the CLI locally:
#   cargo install wasm-bindgen-cli --version "$(scripts/wasm.sh version)" --locked
set -euo pipefail

# Keep in step with the `wasm-bindgen` crate in Cargo.lock (checked below) and
# with docs/transport.md.
WASM_BINDGEN_CLI_VERSION="0.2.130"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

out_dir="apps/ui/src/gen/transport-wasm"
mode="${1:-check}"

if [ "$mode" = "version" ]; then
  echo "$WASM_BINDGEN_CLI_VERSION"
  exit 0
fi
if [ "$mode" != "build" ] && [ "$mode" != "check" ]; then
  echo "usage: $0 build|check|version" >&2
  exit 2
fi

locked="$(awk '/^name = "wasm-bindgen"$/ {getline; gsub(/version = |"/, ""); print; exit}' Cargo.lock)"
if [ "$locked" != "$WASM_BINDGEN_CLI_VERSION" ]; then
  echo "error: Cargo.lock pins wasm-bindgen $locked but scripts/wasm.sh pins the CLI at $WASM_BINDGEN_CLI_VERSION." >&2
  echo "Update WASM_BINDGEN_CLI_VERSION (and docs/transport.md), then rebuild with '$0 build'." >&2
  exit 1
fi
if ! command -v wasm-bindgen >/dev/null 2>&1; then
  echo "error: wasm-bindgen CLI not found. Install it with:" >&2
  echo "  cargo install wasm-bindgen-cli --version $WASM_BINDGEN_CLI_VERSION --locked" >&2
  exit 1
fi
cli="$(wasm-bindgen --version | awk '{print $2}')"
if [ "$cli" != "$WASM_BINDGEN_CLI_VERSION" ]; then
  echo "error: wasm-bindgen CLI is $cli, expected $WASM_BINDGEN_CLI_VERSION:" >&2
  echo "  cargo install wasm-bindgen-cli --version $WASM_BINDGEN_CLI_VERSION --locked --force" >&2
  exit 1
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/h4rn-wasm.XXXXXX")"
trap 'rm -rf "$work"' EXIT

cargo_home="${CARGO_HOME:-$HOME/.cargo}"
# RUSTFLAGS replaces `.cargo/config.toml`'s target rustflags, so the getrandom
# backend cfg is repeated here.
export RUSTFLAGS="--cfg getrandom_backend=\"wasm_js\" --remap-path-prefix=${cargo_home}=/cargo --remap-path-prefix=${root}=/h4rn"
export CARGO_TARGET_DIR="$work/target"

cargo build --release --locked --target wasm32-unknown-unknown -p h4rn-transport-wasm
wasm-bindgen "$work/target/wasm32-unknown-unknown/release/h4rn_transport_wasm.wasm" \
  --out-dir "$work/out" --out-name h4rn_transport_wasm --target web

built=(h4rn_transport_wasm_bg.wasm h4rn_transport_wasm_bg.wasm.d.ts h4rn_transport_wasm.d.ts h4rn_transport_wasm.js)

if [ "$mode" = "build" ]; then
  for file in "${built[@]}"; do
    cp "$work/out/$file" "$out_dir/$file"
  done
  echo "Rebuilt $out_dir"
  exit 0
fi

stale=0
for file in "${built[@]}"; do
  if ! cmp -s "$work/out/$file" "$out_dir/$file"; then
    echo "stale: $out_dir/$file" >&2
    stale=1
  fi
done
if [ "$stale" -ne 0 ]; then
  echo >&2
  echo "error: the committed wasm does not match crates/h4rn-transport-wasm." >&2
  echo "Run 'scripts/wasm.sh build' and commit the result." >&2
  exit 1
fi
echo "The committed transport wasm is up to date."
