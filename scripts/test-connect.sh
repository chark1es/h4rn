#!/usr/bin/env bash
# Postgres-backed integration tests for h4rn-connect.
#
#   ./scripts/test-connect.sh            # core Postgres tests
#   ./scripts/test-connect.sh --orbitid  # also run the OrbitID e2e test
#
# The script owns the lifecycle: it starts postgres:17 from the deployment
# compose file, runs the `#[ignore]`d tests against it, and always tears the
# stack down with `docker compose down -v`. Nothing is mocked; the tests use
# the real schema and the real router.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ORBITID=0
for arg in "$@"; do
  case "$arg" in
    --orbitid) ORBITID=1 ;;
    -h|--help)
      sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'
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

COMPOSE=("$DOCKER" compose -f deploy/docker-compose.yml)
export H4RN_PG_PORT="${H4RN_TEST_PG_PORT:-55432}"
export H4RN_PG_USER=h4rn
export H4RN_PG_PASSWORD=h4rn-test-password
export H4RN_PG_DATABASE=h4rn
# Compose validates the full file even when only postgres is started.
export H4RN_CONNECT_PUBLIC_BASE_URL="http://localhost:8787"
export H4RN_CONNECT_KEK="$(openssl rand -base64 32)"
export H4RN_CONNECT_SIGNING_KEY="$(openssl rand -base64 32)"

# `docker.exe` on WSL is a Windows process and does not inherit the WSL
# environment, so exported variables never reach compose interpolation. Feed
# the same values through an env file instead.
ENV_FILE="$(mktemp -t h4rn-connect-tests.XXXXXX)"
cat >"$ENV_FILE" <<EOF
H4RN_PG_PORT=$H4RN_PG_PORT
H4RN_PG_USER=$H4RN_PG_USER
H4RN_PG_PASSWORD=$H4RN_PG_PASSWORD
H4RN_PG_DATABASE=$H4RN_PG_DATABASE
H4RN_CONNECT_PUBLIC_BASE_URL=$H4RN_CONNECT_PUBLIC_BASE_URL
H4RN_CONNECT_KEK=$H4RN_CONNECT_KEK
H4RN_CONNECT_SIGNING_KEY=$H4RN_CONNECT_SIGNING_KEY
ORBITID_ENCRYPTION_KEY=test-orbitid-encryption-key
EOF
COMPOSE=("$DOCKER" compose --env-file "$ENV_FILE" -f deploy/docker-compose.yml)

cleanup() {
  echo "--- tearing down compose stack"
  "${COMPOSE[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
  rm -f "${ENV_FILE:-}"
}
trap cleanup EXIT

echo "--- starting postgres:17 on 127.0.0.1:${H4RN_PG_PORT}"
"${COMPOSE[@]}" up -d --wait postgres

export H4RN_TEST_DATABASE_URL="postgres://${H4RN_PG_USER}:${H4RN_PG_PASSWORD}@127.0.0.1:${H4RN_PG_PORT}/${H4RN_PG_DATABASE}"

echo "--- running Postgres integration tests"
cargo test -p h4rn-connect --test postgres -- --ignored --test-threads=1

if [[ "$ORBITID" == "1" ]]; then
  if [[ -z "${H4RN_TEST_ORBITID_URL:-}" || -z "${H4RN_TEST_OIDC_CLIENT_ID:-}" ]]; then
    cat <<'EOF'
--- OrbitID e2e requested but not configured.
    Set H4RN_TEST_ORBITID_URL (issuer, e.g. http://localhost:4000),
    H4RN_TEST_OIDC_CLIENT_ID and optionally H4RN_TEST_OIDC_CLIENT_SECRET,
    then re-run. See docs/self-hosting.md for the local OrbitID profile.
EOF
    exit 0
  fi
  echo "--- running OrbitID e2e against ${H4RN_TEST_ORBITID_URL}"
  cargo test -p h4rn-connect --test orbitid -- --ignored --test-threads=1
fi
