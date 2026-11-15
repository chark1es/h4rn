#!/usr/bin/env bash
# Docker-backed end-to-end tests for daemon-side remote access (Phase 3).
#
#   ./scripts/test-remote.sh
#
# Starts Postgres and a real OrbitID in Docker on free ports, bootstraps the
# test user/session and a confidential OIDC client, then runs the ignored
# `h4rn-daemon` remote test. The test signs a daemon in with the device grant,
# pairs a native device, creates a thread over the relay with `FakeHarness`,
# switches from a direct path to the relay after the direct socket dies, and
# revokes the device. Everything is torn down with
# `docker compose down -v --remove-orphans`, including on failure.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

for arg in "$@"; do
  case "$arg" in
    -h|--help)
      sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "unknown argument: $arg" >&2
      exit 2
      ;;
  esac
done

# Docker Desktop on WSL exposes the CLI as docker.exe but not the socket.
if [[ -z "${DOCKER:-}" ]]; then
  if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
    DOCKER=docker
  elif [[ -x "/mnt/c/Program Files/Docker/Docker/resources/bin/docker.exe" ]]; then
    DOCKER="/mnt/c/Program Files/Docker/Docker/resources/bin/docker.exe"
  else
    echo "docker is not reachable; start Docker Desktop or set DOCKER" >&2
    exit 1
  fi
fi

free_port() {
  # Docker Desktop on WSL publishes on Windows, where the ephemeral range
  # (often 49k-65k) is sometimes reserved. Stay in a range Windows accepts.
  python3 - <<'PY'
import random
import socket

for _ in range(200):
    port = random.randint(20000, 29999)
    sock = socket.socket()
    try:
        sock.bind(("127.0.0.1", port))
    except OSError:
        sock.close()
        continue
    sock.close()
    print(port)
    break
else:
    raise SystemExit("no free port in 20000-29999")
PY
}

PROJECT="h4rn-remote-it-${RANDOM}-$$"
export H4RN_PG_PORT="${H4RN_TEST_PG_PORT:-$(free_port)}"
export H4RN_PG_USER=h4rn
export H4RN_PG_PASSWORD=h4rn-test-password
export H4RN_PG_DATABASE=h4rn
export H4RN_CONNECT_PUBLIC_BASE_URL="http://localhost:8787"
export H4RN_CONNECT_KEK="$(openssl rand -base64 32)"
export H4RN_CONNECT_SIGNING_KEY="$(openssl rand -base64 32)"

H4RN_ORBITID_IMAGE="${H4RN_ORBITID_IMAGE:-orbitid:local}"
ORBITID_ENC_KEY="$(openssl rand -base64 32)"
export H4RN_ORBITID_IMAGE
export H4RN_ORBITID_PORT="${H4RN_TEST_ORBITID_PORT:-$(free_port)}"
export ORBITID_BASE_URL="http://localhost:${H4RN_ORBITID_PORT}"
export ORBITID_ENCRYPTION_KEY="$ORBITID_ENC_KEY"

# docker.exe does not inherit the WSL environment, so compose gets an env file.
ENV_FILE="$(mktemp -t h4rn-remote-tests.XXXXXX)"
JAR_FILE="$(mktemp -t h4rn-remote-cookies.XXXXXX)"
WASM_DIR=""
cat >"$ENV_FILE" <<EOF
H4RN_PG_PORT=$H4RN_PG_PORT
H4RN_PG_USER=$H4RN_PG_USER
H4RN_PG_PASSWORD=$H4RN_PG_PASSWORD
H4RN_PG_DATABASE=$H4RN_PG_DATABASE
H4RN_CONNECT_PUBLIC_BASE_URL=$H4RN_CONNECT_PUBLIC_BASE_URL
H4RN_CONNECT_KEK=$H4RN_CONNECT_KEK
H4RN_CONNECT_SIGNING_KEY=$H4RN_CONNECT_SIGNING_KEY
H4RN_ORBITID_IMAGE=$H4RN_ORBITID_IMAGE
H4RN_ORBITID_PORT=$H4RN_ORBITID_PORT
ORBITID_BASE_URL=$ORBITID_BASE_URL
ORBITID_ENCRYPTION_KEY=$ORBITID_ENCRYPTION_KEY
EOF
COMPOSE=("$DOCKER" compose --env-file "$ENV_FILE" -p "$PROJECT"
  --profile orbitid
  -f deploy/docker-compose.yml)

cleanup() {
  local status=$?
  if [[ "${CLEANED_UP:-0}" == "1" ]]; then
    exit "$status"
  fi
  CLEANED_UP=1
  set +e
  echo "--- tearing down compose project ${PROJECT}"
  "${COMPOSE[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
  rm -f "${ENV_FILE:-}" "${JAR_FILE:-}"
  [[ -n "${WASM_DIR:-}" ]] && rm -rf "$WASM_DIR"
  if [[ $status -ne 0 ]]; then
    echo "--- tests failed with exit code ${status}"
  fi
  exit "$status"
}
trap cleanup EXIT INT TERM

wait_for_url() {
  local url=$1 name=$2 tries=${3:-120}
  for ((i = 1; i <= tries; i++)); do
    if curl -fsS "$url" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "--- timed out waiting for ${name} at ${url}" >&2
  [[ "$name" == orbitid* ]] && "${COMPOSE[@]}" logs --tail=100 "$name" >&2 || true
  return 1
}

build_orbitid() {
  if "$DOCKER" image inspect "$H4RN_ORBITID_IMAGE" >/dev/null 2>&1; then
    return 0
  fi
  local source="${H4RN_ORBITID_SOURCE:-/home/chark1es/.t3/projects/orbitid}"
  if [[ ! -f "$source/Dockerfile" ]]; then
    cat >&2 <<EOF
--- ${H4RN_ORBITID_IMAGE} is not built and no OrbitID source tree was found.
    Set H4RN_ORBITID_SOURCE to a checkout containing the TS server Dockerfile.
EOF
    return 1
  fi
  echo "--- building ${H4RN_ORBITID_IMAGE} from ${source} (read-only)"
  (
    cd "$source"
    tar -cf - \
      --exclude='./node_modules' --exclude='*/node_modules' \
      --exclude='./.git' --exclude='./data' \
      --exclude='*/dist' --exclude='*/.next' \
      --exclude='./.env' --exclude='./.env.local' .
  ) | "$DOCKER" build -t "$H4RN_ORBITID_IMAGE" -
}

echo "--- starting postgres:17 on 127.0.0.1:${H4RN_PG_PORT}"
"${COMPOSE[@]}" up -d --wait postgres
export H4RN_TEST_DATABASE_URL="postgres://${H4RN_PG_USER}:${H4RN_PG_PASSWORD}@127.0.0.1:${H4RN_PG_PORT}/${H4RN_PG_DATABASE}"

build_orbitid
echo "--- starting OrbitID on ${ORBITID_BASE_URL}"
"${COMPOSE[@]}" up -d --wait orbitid-postgres orbitid
wait_for_url "${ORBITID_BASE_URL}/.well-known/openid-configuration" orbitid

echo "--- compiling the remote e2e binary"
cargo test -p h4rn-daemon --test remote --no-run

echo "--- building the wasm client for the node relay check"
command -v node >/dev/null 2>&1 || {
  echo "node is required for the wasm-over-relay check" >&2
  exit 1
}
command -v wasm-bindgen >/dev/null 2>&1 || {
  echo "wasm-bindgen (matching Cargo.lock) is required; see docs/transport.md" >&2
  exit 1
}
WASM_DIR="$(mktemp -d -t h4rn-wasm.XXXXXX)"
cargo build -p h4rn-transport-wasm --target wasm32-unknown-unknown --release
wasm-bindgen --target nodejs --out-dir "$WASM_DIR" \
  "${CARGO_TARGET_DIR:-$ROOT/target}/wasm32-unknown-unknown/release/h4rn_transport_wasm.wasm"
export H4RN_TEST_WASM_DIR="$WASM_DIR"
export H4RN_TEST_NODE_SCRIPT="$ROOT/scripts/wasm-relay-client.mjs"

SECRET_KEY="$(ORBITID_ENCRYPTION_KEY="$ORBITID_ENC_KEY" node -e '
  const { createHmac } = require("crypto");
  process.stdout.write("sk_test_" + createHmac("sha256", process.env.ORBITID_ENCRYPTION_KEY)
    .update("orbitid/secret-key").digest("base64url"));
')"

echo "--- creating the test user and headless session"
curl -sS -X POST "${ORBITID_BASE_URL}/v1/users" \
  -H "Authorization: Bearer ${SECRET_KEY}" -H 'Content-Type: application/json' \
  -d '{"email_address":["ada@h4rn.test"],"password":"Sup3rSecret!","first_name":"Ada"}' \
  | python3 -c 'import sys,json; json.load(sys.stdin)' >/dev/null

SIGNIN_JSON="$(curl -sS -c "$JAR_FILE" -X POST "${ORBITID_BASE_URL}/v1/client/sign_ins" \
  -H 'Content-Type: application/json' \
  -d '{"identifier":"ada@h4rn.test","strategy":"password","password":"Sup3rSecret!"}')"
SESSION_ID="$(echo "$SIGNIN_JSON" | python3 -c '
import sys, json
d = json.load(sys.stdin)
print((d.get("client") or {}).get("last_active_session_id")
      or (d.get("response") or {}).get("created_session_id") or "")
')"
[[ -n "$SESSION_ID" ]] || {
  echo "--- could not sign the test user in: $SIGNIN_JSON" >&2
  exit 1
}
SESSION_JWT="$(curl -sS -b "$JAR_FILE" -X POST \
  "${ORBITID_BASE_URL}/v1/client/sessions/${SESSION_ID}/tokens" \
  -H 'Content-Type: application/json' -d '{}' \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["jwt"])')"

redirect_uri="${H4RN_TEST_OIDC_REDIRECT_URI:-h4rn://callback}"
CONF_APP="$(curl -sS -X POST "${ORBITID_BASE_URL}/v1/oauth_applications" \
  -H "Authorization: Bearer ${SECRET_KEY}" -H 'Content-Type: application/json' \
  --data-binary "$(python3 - "$redirect_uri" <<'PY'
import json, sys
print(json.dumps({
    "name": "h4rn-remote-confidential",
    "redirect_uris": [sys.argv[1]],
    "scopes": "openid profile email offline_access",
    "public": False,
    "consent_screen_enabled": False,
    "pkce_required": True,
}))
PY
)")"
CONF_CLIENT_ID="$(echo "$CONF_APP" | python3 -c 'import sys,json; print(json.load(sys.stdin)["client_id"])')"
CONF_CLIENT_SECRET="$(echo "$CONF_APP" | python3 -c 'import sys,json; print(json.load(sys.stdin)["client_secret"])')"

echo "--- enabling the device grant"
curl -sS -X PATCH "${ORBITID_BASE_URL}/v1/instance" \
  -H "Authorization: Bearer ${SECRET_KEY}" -H 'Content-Type: application/json' \
  -d "{\"application\":{\"homeUrl\":\"${ORBITID_BASE_URL}\"},
       \"oidcProvider\":{\"deviceAuthorizationEnabled\":true,
                        \"deviceCodeIntervalSeconds\":1,
                        \"deviceCodeLifetimeSeconds\":300},
       \"session\":{\"tokenLifetimeSeconds\":3600}}" >/dev/null

export H4RN_TEST_ORBITID_URL="$ORBITID_BASE_URL"
export H4RN_TEST_ORBITID_SESSION_JWT="$SESSION_JWT"
export H4RN_TEST_OIDC_CONFIDENTIAL_CLIENT_ID="$CONF_CLIENT_ID"
export H4RN_TEST_OIDC_CONFIDENTIAL_CLIENT_SECRET="$CONF_CLIENT_SECRET"
export H4RN_TEST_OIDC_REDIRECT_URI="$redirect_uri"

echo "--- running the remote access e2e against ${ORBITID_BASE_URL}"
cargo test -p h4rn-daemon --test remote -- --ignored --test-threads=1
