#!/usr/bin/env bash
# Postgres- and OrbitID-backed integration tests for h4rn-connect.
#
#   ./scripts/test-connect.sh            # core Postgres tests
#   ./scripts/test-connect.sh --orbitid  # also run the real OrbitID e2e suite
#
# The script owns the lifecycle. It starts Postgres (and, with --orbitid, a
# real OrbitID from the TS tree in $H4RN_ORBITID_SOURCE) in Docker on free
# ports, waits for discovery, creates the test user/session and OIDC clients
# through OrbitID's own APIs, runs the `#[ignore]`d tests, and always tears the
# stack down with `docker compose down -v --remove-orphans` (including on
# failure). Nothing is mocked: the tests use the real schema, router, provider
# and network.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ORBITID=0
for arg in "$@"; do
  case "$arg" in
    --orbitid) ORBITID=1 ;;
    -h|--help)
      sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "unknown argument: $arg" >&2
      exit 2
      ;;
  esac
done

# Docker Desktop on WSL exposes the CLI as docker.exe but not the socket. The
# compose file still works because the CLI talks to the Windows engine.
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
  python3 - <<'PY'
import socket

sock = socket.socket()
sock.bind(("127.0.0.1", 0))
print(sock.getsockname()[1])
sock.close()
PY
}

PROJECT="h4rn-connect-it-${RANDOM}-$$"
export H4RN_PG_PORT="${H4RN_TEST_PG_PORT:-$(free_port)}"
export H4RN_PG_USER=h4rn
export H4RN_PG_PASSWORD=h4rn-test-password
export H4RN_PG_DATABASE=h4rn
# Compose validates the full file even when only postgres is started.
export H4RN_CONNECT_PUBLIC_BASE_URL="http://localhost:8787"
export H4RN_CONNECT_KEK="$(openssl rand -base64 32)"
export H4RN_CONNECT_SIGNING_KEY="$(openssl rand -base64 32)"

H4RN_ORBITID_IMAGE="${H4RN_ORBITID_IMAGE:-orbitid:local}"
ORBITID_ENC_KEY="$(openssl rand -base64 32)"
export H4RN_ORBITID_IMAGE
export H4RN_ORBITID_PORT="${H4RN_TEST_ORBITID_PORT:-$(free_port)}"
export H4RN_ORBITID_ALT_PORT="${H4RN_TEST_ORBITID_ALT_PORT:-$(free_port)}"
export ORBITID_BASE_URL="http://localhost:${H4RN_ORBITID_PORT}"
export ORBITID_ALT_BASE_URL="http://localhost:${H4RN_ORBITID_ALT_PORT}"
export ORBITID_ENCRYPTION_KEY="$ORBITID_ENC_KEY"

# `docker.exe` on WSL is a Windows process and does not inherit the WSL
# environment, so exported variables never reach compose interpolation. Feed
# the same values through an env file instead.
ENV_FILE="$(mktemp -t h4rn-connect-tests.XXXXXX)"
JAR_FILE="$(mktemp -t h4rn-connect-cookies.XXXXXX)"
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
H4RN_ORBITID_ALT_PORT=$H4RN_ORBITID_ALT_PORT
ORBITID_BASE_URL=$ORBITID_BASE_URL
ORBITID_ALT_BASE_URL=$ORBITID_ALT_BASE_URL
ORBITID_ENCRYPTION_KEY=$ORBITID_ENCRYPTION_KEY
EOF
COMPOSE=("$DOCKER" compose --env-file "$ENV_FILE" -p "$PROJECT"
  --profile orbitid --profile orbitid-alt
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
  if [[ "$name" == orbitid* ]]; then
    "${COMPOSE[@]}" logs --tail=100 "$name" >&2 || true
  fi
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
  # The Docker engine cannot bind-mount WSL's ext4 home reliably, so stream the
  # build context over stdin instead of passing a path.
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

echo "--- running Postgres integration tests"
cargo test -p h4rn-connect --test postgres --test skills -- --ignored --test-threads=1

if [[ "$ORBITID" == "1" ]]; then
  build_orbitid

  echo "--- starting OrbitID on ${ORBITID_BASE_URL}"
  "${COMPOSE[@]}" up -d --wait orbitid-postgres orbitid
  wait_for_url "${ORBITID_BASE_URL}/.well-known/openid-configuration" orbitid

  # Compile the e2e binary before minting the short-lived session JWTs.
  echo "--- building the OrbitID e2e test binary"
  cargo test -p h4rn-connect --test orbitid --no-run

  SECRET_KEY="$(ORBITID_ENCRYPTION_KEY="$ORBITID_ENC_KEY" node -e '
    const { createHmac } = require("crypto");
    process.stdout.write("sk_test_" + createHmac("sha256", process.env.ORBITID_ENCRYPTION_KEY)
      .update("orbitid/secret-key").digest("base64url"));
  ')"

  echo "--- creating test user and headless session"
  USER_JSON="$(curl -sS -X POST "${ORBITID_BASE_URL}/v1/users" \
    -H "Authorization: Bearer ${SECRET_KEY}" -H 'Content-Type: application/json' \
    -d '{"email_address":["ada@h4rn.test"],"password":"Sup3rSecret!","first_name":"Ada"}')"
  echo "$USER_JSON" | python3 -c 'import sys,json; json.load(sys.stdin)' >/dev/null || {
    echo "--- could not create the test user: $USER_JSON" >&2
    exit 1
  }

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

  app_payload() {
    python3 - "$1" "$2" "$redirect_uri" <<'PY'
import json, sys
name, public, redirect = sys.argv[1], sys.argv[2] == "true", sys.argv[3]
print(json.dumps({
    "name": name,
    "redirect_uris": [redirect],
    "scopes": "openid profile email offline_access",
    "public": public,
    "consent_screen_enabled": False,
    "pkce_required": True,
}))
PY
  }

  register_app() {
    local name=$1 public=$2 body
    body="$(curl -sS -X POST "${ORBITID_BASE_URL}/v1/oauth_applications" \
      -H "Authorization: Bearer ${SECRET_KEY}" -H 'Content-Type: application/json' \
      --data-binary "$(app_payload "$name" "$public")")"
    if ! echo "$body" | python3 -c 'import sys,json; json.load(sys.stdin)["client_id"]' >/dev/null 2>&1; then
      echo "--- could not register ${name}: ${body}" >&2
      return 1
    fi
    printf '%s' "$body"
  }

  echo "--- registering public and confidential h4rn OIDC clients"
  PUBLIC_APP="$(register_app h4rn-public true)"
  CONF_APP="$(register_app h4rn-confidential false)"
  PUBLIC_CLIENT_ID="$(echo "$PUBLIC_APP" | python3 -c 'import sys,json; print(json.load(sys.stdin)["client_id"])')"
  CONF_CLIENT_ID="$(echo "$CONF_APP" | python3 -c 'import sys,json; print(json.load(sys.stdin)["client_id"])')"
  CONF_CLIENT_SECRET="$(echo "$CONF_APP" | python3 -c 'import sys,json; print(json.load(sys.stdin)["client_secret"])')"

  echo "--- enabling device grant and a long enough session JWT"
  curl -sS -X PATCH "${ORBITID_BASE_URL}/v1/instance" \
    -H "Authorization: Bearer ${SECRET_KEY}" -H 'Content-Type: application/json' \
    -d "{\"application\":{\"homeUrl\":\"${ORBITID_BASE_URL}\"},
         \"oidcProvider\":{\"deviceAuthorizationEnabled\":true,
                          \"deviceCodeIntervalSeconds\":1,
                          \"deviceCodeLifetimeSeconds\":300},
         \"session\":{\"tokenLifetimeSeconds\":3600}}" >/dev/null

  # A second instance with the same database and signing keys but a different
  # issuer, so the test can prove that an `iss` that is not configured is
  # rejected. It shares the session cookie, so it mints its own JWT.
  echo "--- starting the alternate-issuer OrbitID on ${ORBITID_ALT_BASE_URL}"
  "${COMPOSE[@]}" up -d --wait orbitid-alt
  wait_for_url "${ORBITID_ALT_BASE_URL}/.well-known/openid-configuration" orbitid-alt
  ALT_SESSION_JWT="$(curl -sS -b "$JAR_FILE" -X POST \
    "${ORBITID_ALT_BASE_URL}/v1/client/sessions/${SESSION_ID}/tokens" \
    -H 'Content-Type: application/json' -d '{}' \
    | python3 -c 'import sys,json; print(json.load(sys.stdin)["jwt"])')"

  export H4RN_TEST_ORBITID_URL="$ORBITID_BASE_URL"
  export H4RN_TEST_ORBITID_ALT_URL="$ORBITID_ALT_BASE_URL"
  export H4RN_TEST_ORBITID_SESSION_JWT="$SESSION_JWT"
  export H4RN_TEST_ORBITID_ALT_SESSION_JWT="$ALT_SESSION_JWT"
  export H4RN_TEST_OIDC_CLIENT_ID="$PUBLIC_CLIENT_ID"
  export H4RN_TEST_OIDC_CONFIDENTIAL_CLIENT_ID="$CONF_CLIENT_ID"
  export H4RN_TEST_OIDC_CONFIDENTIAL_CLIENT_SECRET="$CONF_CLIENT_SECRET"
  export H4RN_TEST_OIDC_REDIRECT_URI="$redirect_uri"
  unset H4RN_TEST_OIDC_CLIENT_SECRET || true

  echo "--- running OrbitID e2e against ${ORBITID_BASE_URL}"
  cargo test -p h4rn-connect --test orbitid -- --ignored --test-threads=1
fi
