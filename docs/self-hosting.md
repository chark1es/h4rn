# Self-hosting h4rn-connect

`h4rn-connect` is the relay: OIDC login, device/user/machine records, pairing, presence,
the opaque `/relay` WebSocket, audit and usage. It never sees prompt or terminal content
(ADR-010). There is no hosted default and no compiled-in issuer (ADR-018, ADR-025); a
deployment must supply a public URL, Postgres, a key-encryption key (KEK) and at least one
OIDC provider.

The image is built from `deploy/Dockerfile` and published on release tags to
`ghcr.io/chark1es/h4rn-connect` for `linux/amd64` and `linux/arm64`; the reference stack is
`deploy/docker-compose.yml`, pinned to that image. Everything below is deployment configuration,
not code.

A Helm chart for Kubernetes lives at [`packaging/helm/h4rn-connect`](../packaging/helm/h4rn-connect/values.yaml)
(Deployment, Service, Ingress, PodDisruptionBudget, optional in-chart Postgres or external
database, chart-managed or existing Secret). The release process is in
[release.md](release.md).

## 1. Quick start (local)

```bash
cp deploy/.env.example deploy/.env
# Fill H4RN_PG_PASSWORD, H4RN_CONNECT_KEK and H4RN_CONNECT_SIGNING_KEY:
openssl rand -base64 32   # KEK
openssl rand -base64 32   # signing seed

docker compose -f deploy/docker-compose.yml up -d --wait
curl -fsS http://localhost:8787/healthz    # "ok"
curl -fsS http://localhost:8787/readyz     # {"status":"ready"}
```

The compose file starts `postgres:17-alpine` and the relay. Postgres is the system of record
(ADR-012); the relay applies `crates/h4rn-connect/migrations/` at boot, so there is no
separate migrate step and no SQLite file to back up.

`H4RN_CONNECT_ALLOW_INSECURE=true` is required when the public URL is `http://` or when an
OIDC issuer is not HTTPS. Do not use it in production.

### Build from the working tree

The base compose file is pinned to the published image, so a deployment never silently builds
local code. To build the same stack from a checkout, add the build override:

```bash
docker compose -f deploy/docker-compose.yml \
               -f deploy/docker-compose.build.yml up -d --build
```

Set `H4RN_CONNECT_IMAGE` to tag the local build differently from the published image.

### Kubernetes

```bash
helm install h4rn packaging/helm/h4rn-connect \
  --namespace h4rn --create-namespace \
  --set config.publicBaseUrl=https://connect.example.com \
  --set config.allowInsecure=false \
  --set ingress.enabled=true --set ingress.hosts[0].host=connect.example.com
```

With the defaults the chart runs Postgres in-cluster. For a managed database set
`postgres.enabled=false` and either `externalDatabase.url` or its host/user/password fields.
Set `secrets.existingSecret` to a Secret containing `database-url`, `kek`, `signing-key` (and
`postgres-password` when Postgres is in-cluster); otherwise the chart generates and reuses them
across upgrades. Pass `config.oidcProviders` as the same JSON array described below.

## 2. Configuration

Environment variables win over the optional TOML file. The file is read from
`H4RN_CONNECT_CONFIG`, otherwise `./h4rn-connect.toml` when present. The same keys work in
both, using the snake_case names shown below.

| Variable | Default | Purpose |
|---|---|---|
| `H4RN_CONNECT_PUBLIC_BASE_URL` | required | URL clients use, no trailing slash. Must be HTTPS in hosted mode. |
| `H4RN_CONNECT_BIND` | `127.0.0.1:8787` | Listen address. Containers set `0.0.0.0:8787`. |
| `H4RN_CONNECT_DATABASE_URL` (or `DATABASE_URL`) | required | Postgres connection string. |
| `H4RN_CONNECT_KEK` | required | 32 bytes, base64 or hex. Envelope-encrypts provider refresh tokens and the refresh-successor window (ADR-016). |
| `H4RN_CONNECT_SIGNING_KEY` | generated per boot in self-hosted mode | 32-byte Ed25519 seed for h4rn access tokens. Required in hosted mode so sessions survive a restart. |
| `H4RN_CONNECT_MODE` | `self_hosted` | `self_hosted` or `hosted`. Hosted requires HTTPS and a stable signing key. |
| `H4RN_CONNECT_CORS_ORIGINS` | empty | Comma-separated browser origins. Empty disables CORS. Native clients do not need it. |
| `H4RN_CONNECT_WEB_DIR` | empty | Directory with the built web UI; served at `/`. Empty disables static serving. |
| `H4RN_CONNECT_OIDC_PROVIDERS` | `[]` | JSON array of provider objects (below). |
| `H4RN_CONNECT_TRUST_FORWARDED_FOR` | `false` | Use `X-Forwarded-For` for rate limiting. Only behind a proxy that overwrites it. |
| `H4RN_CONNECT_ALLOW_INSECURE` | `false` | Allow HTTP public URLs/issuers. Local development only. |
| `H4RN_CONNECT_SESSION_TTL_SECS` | `600` | h4rn access-token lifetime. |
| `H4RN_CONNECT_REFRESH_TTL_SECS` | `2592000` | Refresh-token lifetime (30 days). |
| `H4RN_CONNECT_PAIRING_TTL_SECS` | `300` | Pairing lifetime. The product contract is five minutes; values above one hour are rejected. |
| `H4RN_CONNECT_REFRESH_GRACE_SECS` | `30` | Window in which a retried rotation receives the same successor (ADR-011). |
| `H4RN_CONNECT_RATE_PER_IP_PER_MINUTE` | `240` | General API requests per IP per minute. `0` disables. |
| `H4RN_CONNECT_RATE_AUTH_PER_MINUTE` | `30` | Login/device-grant requests per IP per minute. |
| `H4RN_CONNECT_RATE_PAIRING_PER_HOUR` | `60` | Pairing creates and claims per IP per hour. |
| `H4RN_CONNECT_RATE_RELAY_CONNECTS_PER_MINUTE` | `120` | Relay upgrades per IP per minute. |
| `H4RN_CONNECT_PAIRING_ATTEMPTS` | `10` | Failed claims before a pairing is burned. |

`RUST_LOG` controls tracing. The relay logs metadata (ids, status,
frame sizes), never payloads or secrets.

### OIDC providers

`H4RN_CONNECT_OIDC_PROVIDERS` is a JSON array:

```json
[
  {
    "id": "orbitid",
    "display_name": "OrbitID",
    "issuer": "https://auth.example.com",
    "client_id": "oa_...",
    "client_secret": "...",
    "scopes": ["openid", "profile", "email", "offline_access"],
    "redirect_uris": [
      "http://127.0.0.1:53682/callback",
      "h4rn://callback"
    ],
    "device_grant": true
  }
]
```

- `id` is the path segment in `/v1/auth/<id>/...`; lowercase letters, digits and `-`.
- `issuer` must be root-mounted (no path prefix) with no trailing slash. The relay normalises
  the trailing slash but rejects a prefix because OrbitID's TS discovery only answers at the
  root.
- `scopes`: `openid` and `offline_access` are added automatically. OrbitID treats scopes as an
  allowlist, so register the same set on the application.
- `redirect_uris` is an exact-match allowlist. Register the fixed desktop loopback port and
  the mobile `h4rn://callback` deep link on the provider as well.
- `device_grant` only enables the local policy; the provider must also advertise
  `device_authorization_endpoint` (OrbitID has it off by default).

The relay also accepts its own callback `${PUBLIC_BASE_URL}/v1/auth/<id>/callback` for
server-side code exchange.

### OrbitID

Register the application through the Backend API (the dashboard refuses non-HTTP redirect
URIs):

```bash
SK=sk_...                                   # orbitid keys / derived secret
curl -sS -X POST "$ORBITID_URL/v1/oauth_applications" \
  -H "Authorization: Bearer $SK" -H "Content-Type: application/json" \
  -d '{"name":"h4rn-connect",
       "redirect_uris":["http://127.0.0.1:53682/callback","h4rn://callback"],
       "scopes":"openid profile email offline_access",
       "public":false,"consent_screen_enabled":false,"pkce_required":true}'
```

Enable the device grant for daemons:

```bash
curl -sS -X PATCH "$ORBITID_URL/v1/instance" \
  -H "Authorization: Bearer $SK" -H "Content-Type: application/json" \
  -d '{"oidcProvider":{"deviceAuthorizationEnabled":true}}'
```

Use the returned `client_id`/`client_secret` in the provider JSON. If consent is enabled,
`/oauth/authorize` redirects to `application.homeUrl` (default `http://localhost:3000`), so
set that to the provider's own URL or keep consent disabled for automated deployments.

The optional compose profile runs a local OrbitID for development (build its image first):

```bash
docker build -t orbitid:local /path/to/orbitid
docker compose -f deploy/docker-compose.yml --profile orbitid up -d --wait
```

## 3. TLS, proxy and static web

Terminate TLS at a reverse proxy (Caddy, nginx, Traefik, a cloud load balancer) and forward
both `/` and `/relay` to the same port. WebSocket upgrades need `Connection`/`Upgrade`
forwarding; no other special routing is required.

Set `public_base_url` to the externally visible HTTPS URL. If the proxy does not overwrite
`X-Forwarded-For`, leave `H4RN_CONNECT_TRUST_FORWARDED_FOR=false` so clients cannot spoof
their source address and evade rate limits.

To serve the UI from the relay, build it and mount the output read-only:

```bash
bun install && bun run --filter @h4rn/web build   # or your publish step
H4RN_CONNECT_WEB_DIR=/srv/h4rn/web
```

The relay answers unknown non-API paths with `index.html` for client-side routing; API paths
still return the JSON error envelope.

## 4. Operations

- **Health:** `GET /healthz` is liveness (`ok`), `GET /readyz` also checks Postgres.
- **Migrations:** applied automatically at startup from the embedded directory. Back up
  Postgres before upgrading a deployment.
- **Secrets:** the KEK decrypts provider refresh tokens and the rotating-refresh successor.
  Rotating it without re-encrypting those rows logs users out. Keep it out of images and
  environment examples that are committed.
- **Restart behaviour:** with a signing key set, access tokens stay valid across restarts.
  Without one in self-hosted mode, sessions end on restart (deliberate warning at boot).
- **Revocation:** `DELETE /v1/devices/{id}` revokes the device's sessions and tells the relay
  peer to disconnect; it does not require the daemon to be online.
- **Audit:** `/v1/audit` is metadata-only and cursor-paged (`before_id`, `limit`). Pairing,
  device, host and logout events are recorded; payloads and secrets never are. A signed-in
  daemon also mirrors remote-action records (`thread.created`, `terminal.opened`,
  `file.written`) to `POST /v1/audit`, and keeps a local first copy even when offline.
- **Usage:** `POST /v1/usage` appends to the durable ledger (ADR-024).

### Daemon login, pairing and addresses

Point a daemon at this deployment and complete the OIDC device grant:

```bash
h4rnd login --connect https://connect.example.com --provider orbitid --name workstation
h4rnd serve
h4rnd pair              # QR for a device, via connect
h4rnd pair --direct     # QR with a LAN/tailnet endpoint and one-time PSK
```

`login` stores connect credentials in the OS keychain, or in an
encrypted `connect-credentials.json` on headless hosts, and registers the
machine's hosts (including WSL distributions). `logout` revokes the session.
While signed in, `serve` keeps the authenticated `/relay` WebSocket open,
publishes presence and mirrors audit records.

`PUT /v1/machines/{id}/addresses` stores the candidates a client should probe:
LAN, tailnet when Tailscale is up, and `relay:<public base URL>` last. The
daemon republishes them on startup and on new `h4rnd login`. Addresses are
metadata; nothing about the relay payload is stored.

## 5. Tests

Hermetic unit and router tests:

```bash
cargo test --workspace
```

Postgres integration tests against the real schema and router (starts `postgres:17` in
Docker and always tears it down with `down -v`):

```bash
./scripts/test-connect.sh
```

OrbitID end-to-end test (needs a running instance and an application; skipped unless
configured):

```bash
H4RN_TEST_ORBITID_URL=http://localhost:4000 \
H4RN_TEST_OIDC_CLIENT_ID=oa_... \
./scripts/test-connect.sh --orbitid
```

The script uses `docker.exe` when Docker Desktop exposes only the Windows CLI. It feeds
compose through a temporary env file because a Windows `docker.exe` does not inherit WSL
environment variables.

Daemon-side remote access (Postgres, the real relay and OrbitID in Docker, a device grant,
pairing, relay thread creation, direct-to-relay failover and revocation):

```bash
./scripts/test-remote.sh
```

The script starts both stacks on free ports, runs the ignored
`crates/h4rn-daemon/tests/remote.rs` end-to-end test, and always removes its containers and
volumes with `compose down -v`.

Build and publish the image directly (the release workflow does this for both architectures):

```bash
docker buildx build --platform linux/amd64,linux/arm64 \
  -f deploy/Dockerfile -t ghcr.io/chark1es/h4rn-connect:<version> --push .
```

## 6. What the relay does not do

- It does not implement Noise or read relay payloads; the daemon and client own that
  (ADR-010).
- It does not host a default issuer or relay; both are configuration (ADR-018, ADR-025).
- It does not accept `form_post` for OrbitID's TS build, and treats RP-initiated logout as
  optional.
- It does not retry `invalid_grant`: the client must authenticate again (ADR-011).
