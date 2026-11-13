#!/usr/bin/env bash
# Live Gitea/Forgejo tests for h4rn-forge.
#
#   ./scripts/test-forges.sh            # Gitea in Docker
#   ./scripts/test-forges.sh --forgejo  # Forgejo instead of Gitea
#   ./scripts/test-forges.sh --github   # also run the read-only GitHub test
#
# The script owns the lifecycle. It starts a real Gitea (or Forgejo) container
# on a free port, creates an admin user and token through the forge's own CLI,
# seeds a repository, a feature branch, a pull request, a commit status and a
# comment through the HTTP API, runs the `#[ignore]`d live tests, and always
# removes the container (also on failure). Nothing is mocked.
#
# The GitHub test is read-only and only runs with --github and a token in
# H4RN_GITHUB_TOKEN; it never writes to the repository.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# cargo is not necessarily on PATH in a non-login shell.
export PATH="${HOME}/.cargo/bin:${PATH}"

RUN_GITHUB=0
KIND=gitea
for arg in "$@"; do
  case "$arg" in
    --forgejo) KIND=forgejo ;;
    --github) RUN_GITHUB=1 ;;
    -h|--help)
      sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
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

if [[ "$KIND" == "forgejo" ]]; then
  IMAGE="${H4RN_FORGEJO_IMAGE:-codeberg.org/forgejo/forgejo:9}"
  CONTAINER="h4rn-forgejo-it-${RANDOM}-$$"
else
  IMAGE="${H4RN_GITEA_IMAGE:-gitea/gitea:1.22}"
  CONTAINER="h4rn-gitea-it-${RANDOM}-$$"
fi
PORT="${H4RN_GITEA_PORT:-}"
if [[ -n "$PORT" ]]; then
  PUBLISH=(-p "${PORT}:3000")
  ROOT_URL="http://127.0.0.1:${PORT}/"
else
  # Let Docker pick a host port. A free port chosen inside WSL can fall into
  # the Windows Hyper-V excluded range and fail to bind.
  PUBLISH=(-p 3000)
  ROOT_URL="http://127.0.0.1:3000/"
fi
ADMIN=h4rn-admin
ADMIN_PASSWORD="h4rn-Admin-Passw0rd!"
REPO="${ADMIN}/forge-smoke"
FEATURE=feature

cleanup() {
  local status=$?
  if [[ "${CLEANED_UP:-0}" == "1" ]]; then
    exit "$status"
  fi
  CLEANED_UP=1
  set +e
  echo "--- removing container ${CONTAINER}"
  "$DOCKER" rm -f "$CONTAINER" >/dev/null 2>&1 || true
  if [[ $status -ne 0 ]]; then
    echo "--- forge tests failed with exit code ${status}"
  fi
  exit "$status"
}
trap cleanup EXIT INT TERM

wait_for_url() {
  local url=$1 tries=${2:-120}
  for ((i = 1; i <= tries; i++)); do
    if curl -fsS "$url" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "--- timed out waiting for ${url}" >&2
  "$DOCKER" logs --tail=100 "$CONTAINER" >&2 || true
  return 1
}

api() {
  local method=$1 path=$2 body=${3:-}
  if [[ -n "$body" ]]; then
    curl -fsS -X "$method" "${BASE}${path}" \
      -H "Authorization: token ${TOKEN}" \
      -H 'Content-Type: application/json' \
      -d "$body"
  else
    curl -fsS -X "$method" "${BASE}${path}" \
      -H "Authorization: token ${TOKEN}"
  fi
}

json_field() {
  python3 -c 'import sys,json; print(json.load(sys.stdin)[sys.argv[1]])' "$1"
}

echo "--- starting ${IMAGE}"
"$DOCKER" run -d --name "$CONTAINER" \
  "${PUBLISH[@]}" \
  -e "GITEA__database__DB_TYPE=sqlite3" \
  -e "GITEA__database__PATH=/data/gitea/gitea.db" \
  -e "GITEA__server__ROOT_URL=${ROOT_URL}" \
  -e "GITEA__server__HTTP_PORT=3000" \
  -e "GITEA__server__DISABLE_SSH=true" \
  -e "GITEA__security__INSTALL_LOCK=true" \
  -e "GITEA__service__DISABLE_REGISTRATION=true" \
  -e "GITEA__repository__DEFAULT_BRANCH=main" \
  -e "GITEA__log__LEVEL=Warn" \
  "$IMAGE" >/dev/null

if [[ -z "$PORT" ]]; then
  MAPPING="$("$DOCKER" port "$CONTAINER" 3000/tcp | head -1 | tr -d '\r')"
  PORT="${MAPPING##*:}"
  [[ -n "$PORT" ]] || {
    echo "--- could not discover the published port" >&2
    exit 1
  }
fi
BASE="http://127.0.0.1:${PORT}"
echo "--- ${IMAGE} listening on ${BASE}"

wait_for_url "${BASE}/api/v1/version"

BINARY="$("$DOCKER" exec --user git "$CONTAINER" sh -c 'command -v gitea || command -v forgejo' | tr -d '\r')"
echo "--- creating admin ${ADMIN} with ${BINARY}"

# The container may accept HTTP before the CLI database is ready.
for ((i = 1; i <= 60; i++)); do
  if "$DOCKER" exec --user git "$CONTAINER" "$BINARY" admin user create \
    --username "$ADMIN" --password "$ADMIN_PASSWORD" \
    --email "admin@h4rn.test" --admin --must-change-password=false >/dev/null 2>&1; then
    break
  fi
  if "$DOCKER" exec --user git "$CONTAINER" "$BINARY" admin user list 2>/dev/null | grep -q "$ADMIN"; then
    break
  fi
  sleep 1
  if [[ $i -eq 60 ]]; then
    echo "--- could not create the admin user" >&2
    "$DOCKER" logs --tail=100 "$CONTAINER" >&2 || true
    exit 1
  fi
done

TOKEN="$("$DOCKER" exec --user git "$CONTAINER" "$BINARY" admin user generate-access-token \
  --username "$ADMIN" --token-name "h4rn-smoke-${RANDOM}" --scopes all --raw | tr -d '\r')"
[[ -n "$TOKEN" ]] || {
  echo "--- could not mint an access token" >&2
  exit 1
}

echo "--- seeding ${REPO}"
api POST /api/v1/user/repos \
  "{\"name\":\"forge-smoke\",\"description\":\"h4rn forge smoke test\",\"private\":true,\"auto_init\":true,\"default_branch\":\"main\"}" >/dev/null

CONTENT="$(printf 'first line\n' | base64 | tr -d '\n')"
api POST "/api/v1/repos/${REPO}/contents/feature.txt" \
  "{\"content\":\"${CONTENT}\",\"message\":\"add the feature file\",\"new_branch\":\"${FEATURE}\"}" >/dev/null

PR_JSON="$(api POST "/api/v1/repos/${REPO}/pulls" \
  "{\"title\":\"Add the feature file\",\"body\":\"Smoke test for the h4rn forge client.\",\"head\":\"${FEATURE}\",\"base\":\"main\"}")"
PR_NUMBER="$(printf '%s' "$PR_JSON" | json_field number)"
HEAD_SHA="$(printf '%s' "$PR_JSON" | python3 -c 'import sys,json; print(json.load(sys.stdin)["head"]["sha"])')"

api POST "/api/v1/repos/${REPO}/statuses/${HEAD_SHA}" \
  "{\"state\":\"success\",\"context\":\"h4rn/smoke\",\"description\":\"docker ${KIND} smoke test\",\"target_url\":\"${BASE}/\"}" >/dev/null

api POST "/api/v1/repos/${REPO}/issues/${PR_NUMBER}/comments" \
  '{"body":"Seed comment for the h4rn forge smoke test."}' >/dev/null

echo "--- running live ${KIND} tests against ${BASE}"
H4RN_GITEA_URL="$BASE" \
H4RN_GITEA_TOKEN="$TOKEN" \
H4RN_GITEA_REPO="$REPO" \
H4RN_GITEA_PR="$PR_NUMBER" \
H4RN_GITEA_KIND="$KIND" \
  cargo test -p h4rn-forge --test wiremock_clients -- --ignored live_gitea_read_and_write --test-threads=1

if [[ "$RUN_GITHUB" == "1" ]]; then
  if [[ -z "${H4RN_GITHUB_TOKEN:-}" ]]; then
    echo "--- skipping the read-only GitHub test: H4RN_GITHUB_TOKEN is unset" >&2
  else
    echo "--- running the read-only GitHub test"
    cargo test -p h4rn-forge --test wiremock_clients -- --ignored live_github_read_only --test-threads=1
  fi
fi
