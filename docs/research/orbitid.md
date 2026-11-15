# OrbitID as the OIDC provider for h4rn-connect

Research notes for wiring `h4rn-connect` (Rust, axum + sqlx) to OrbitID.

**Two trees are cited.** Keep them straight:

| Legend | Path | What it is |
|---|---|---|
| `[TS]` | `~/.t3/projects/orbitid` | Git `main`: TypeScript + Hono backend (`packages/server`), the released/reference implementation. `docker-compose.yml` and `Dockerfile` here build this. |
| `[Rust]` | `~/.t3/worktrees/orbitid/t3-f011ae99-f1d7062ac35686f3` | Uncommitted Rust rewrite (axum + sqlx + Postgres) under review. Same HTTP contract, a few intentional additions. |

Statements marked **verified** were checked by running the TS server locally on a throwaway
Postgres database (`NODE_ENV=development`, port 4111) and driving the full flow with `curl`;
the process and database were removed afterwards.

---

## 1. Issuer, discovery, JWKS, signing

### Issuer URL

The issuer is simply the instance's public base URL, with **no path prefix and no trailing slash**:

- `[TS]` `packages/server/src/env.ts:41` — `ORBITID_BASE_URL`, default `http://localhost:<port>`, `.replace(/\/$/, "")`.
- `[Rust]` `crates/orbitid-oidc/src/services/oidc/issuer.rs:27-58` — `ORBITID_BASE_URL` wins
  (`base_url_explicit`); only when it is unset **and** `ORBITID_TRUST_PROXY >= 1` are
  `X-Forwarded-Proto` / `X-Forwarded-Host` / `X-Forwarded-Port` honoured. Never a trailing slash.

Local default: `http://localhost:4000` (`ORBITID_PORT` default 4000). Docker compose defaults
`ORBITID_BASE_URL` to `http://localhost:${ORBITID_PORT:-4000}` (`[TS]` `docker-compose.yml:26`,
`[Rust]` `docker-compose.yml:38-40`).

`iss` in every token is this value verbatim (`[TS]` `packages/server/src/services/tokens.ts:233`,
`packages/server/src/services/oidc.ts:364,418`).

**h4rn must treat the issuer as configuration** (`OIDC_ISSUER`), never as a constant: there is no
hosted instance to hardcode (see §6).

### Discovery document

`GET /.well-known/openid-configuration`

- `[TS]` route `packages/server/src/modules/oidc.ts:146`, document built in
  `packages/server/src/services/oidc.ts:769-808`.
- `[Rust]` routes `crates/orbitid-oidc/src/routes/well_known.rs:19-36` (also
  `/.well-known/oauth-authorization-server` and one path-prefix segment), document in
  `crates/orbitid-oidc/src/services/oidc/discovery.rs:44-156`.

Actual TS document (verified live, `http://localhost:4111`):

```json
{
  "issuer": "http://localhost:4111",
  "authorization_endpoint": "http://localhost:4111/oauth/authorize",
  "token_endpoint": "http://localhost:4111/oauth/token",
  "userinfo_endpoint": "http://localhost:4111/oauth/userinfo",
  "jwks_uri": "http://localhost:4111/oauth/jwks",
  "revocation_endpoint": "http://localhost:4111/oauth/revoke",
  "introspection_endpoint": "http://localhost:4111/oauth/introspect",
  "response_types_supported": ["code"],
  "response_modes_supported": ["query"],
  "grant_types_supported": ["authorization_code", "refresh_token"],
  "subject_types_supported": ["public"],
  "id_token_signing_alg_values_supported": ["RS256"],
  "scopes_supported": ["openid", "profile", "email", "organizations", "offline_access"],
  "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post", "none"],
  "revocation_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post"],
  "introspection_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post"],
  "code_challenge_methods_supported": ["S256"],
  "claims_supported": ["sub", "iss", "aud", "exp", "iat", "nonce", "name", "given_name",
    "family_name", "preferred_username", "picture", "email", "email_verified",
    "org_id", "org_slug", "org_role", "org_permissions", "organizations"],
  "prompt_values_supported": ["consent", "none"]
}
```

Additions when enabled / in the Rust build:

| Key | When | Source |
|---|---|---|
| `device_authorization_endpoint` | `oidcProvider.deviceAuthorizationEnabled` | `[TS]` oidc.ts:804-806, `[Rust]` discovery.rs:149-154 |
| `urn:ietf:params:oauth:grant-type:device_code` in `grant_types_supported` | same | `[TS]` oidc.ts:783-785 |
| `access_token_format`, `access_token_signing_alg_values_supported`, `access_token_claims_supported` | `accessTokenFormat = "jwt"` | `[TS]` oidc.ts:799-803 |
| `end_session_endpoint` (`/oauth/end_session`) | always | `[Rust]` discovery.rs:71-74 only — **absent in TS** |
| `response_modes_supported: ["query","form_post"]` | always | `[Rust]` discovery.rs:76-79 |
| `prompt_values_supported: ["none","login","consent","select_account"]` | always | `[Rust]` discovery.rs:112-115 |
| `phone`, `groups` scopes, 27 `claims_supported` (incl. `auth_time`, `at_hash`, `azp`, `sid`) | always | `[Rust]` discovery.rs:14-42, `shared.rs:19-28` |

### JWKS and algorithms

- `GET /oauth/jwks` — `[TS]` `packages/server/src/modules/oidc.ts:147`, keys from
  `packages/server/src/services/tokens.ts:79-86`.
- Also `GET /.well-known/jwks.json` and `GET /v1/jwks` serve the **same** key set (module
  `packages/server/src/modules/jwks.ts`), same `signing_keys` table.
- One RS256 key is active at a time; generated on first boot, private JWK encrypted with
  `ORBITID_ENCRYPTION_KEY` (`[TS]` tokens.ts:14,25-41). The JWKS returns every stored key, so old
  keys stay valid for verification after rotation (`[TS]` tokens.ts:70-86).
- `id_token_signing_alg_values_supported` is `["RS256"]` only. `alg: "none"` and HS256 are not
  offered; session-token verification pins `algorithms: ["RS256"]` (`[TS]` tokens.ts:276-284).
- JWK fields: `kty: RSA`, `alg: RS256`, `use: sig`, `kid`, `n`, `e` (verified live).
- Claims: `iss`, `sub`, `aud` (= `client_id`, **a string**, not an array), `iat`, `exp`;
  JWT access tokens add `azp`, `scope`, `jti`, `sid`.

---

## 2. Registering an OIDC client

### Backend API (works headlessly; the only way to register non-`http(s)` redirect URIs)

```bash
curl -sS -X POST "$ORBITID_URL/v1/oauth_applications" \
  -H "Authorization: Bearer $ORBITID_SECRET_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "h4rn-connect",
    "redirect_uris": ["h4rn://callback", "http://127.0.0.1:53682/callback"],
    "scopes": "openid profile email organizations offline_access",
    "public": false,
    "consent_screen_enabled": false,
    "pkce_required": true
  }'
```

Routes: `[TS]` `packages/server/src/modules/oidc.ts:586-629`; validation and defaults
`packages/server/src/services/oidc.ts:90-113`; `[Rust]`
`crates/orbitid-oidc/src/routes/applications.rs:144-192`, defaults
`crates/orbitid-oidc/src/services/oidc/applications/serialize.rs:120-150`.

Response (verified live, ids from the probe run; name shortened):

```json
{
  "object": "oauth_application",
  "id": "oa_qF1o9Ev66s2iaIz91iguzeoUJvA",
  "name": "h4rn-connect",
  "client_id": "oa_wiJVGcNDwVH0SqoAliUkoyO4o3m",
  "public": false,
  "redirect_uris": ["h4rn://callback", "http://127.0.0.1:53682/callback"],
  "scopes": "openid profile email organizations offline_access",
  "consent_screen_enabled": false,
  "pkce_required": true,
  "created_at": 1794535900739,
  "updated_at": 1794535900739,
  "client_secret": "oas_x0zd_Jg__ebFg6PKPDVrbuYI0QUZz2Gs"
}
```

`client_secret` is returned **only** on create (and on
`POST /v1/oauth_applications/:id/rotate_secret`). Prefixes: client_id `oa_…`, secret `oas_…`.
Other routes: `GET /v1/oauth_applications`, `GET|PATCH|DELETE /v1/oauth_applications/:id`.

Dashboard equivalent: `/oauth-apps` (`apps/dashboard/src/pages/oauth-apps.tsx`). **Quirk:** the
dashboard rejects any redirect URI that is not `http:`/`https:`
(`apps/dashboard/src/pages/_config/settings.ts:634-640`, checked at `oauth-apps.tsx:96-101`), so
`h4rn://callback` must be registered through the Backend API (verified: the API accepts it; the
zod schema is `z.string().url()`, `[TS]` oidc.ts:92).

### Client types and PKCE

| | Confidential (`public: false`) | Public (`public: true`) |
|---|---|---|
| Secret | `oas_…`, `client_secret_basic` or `client_secret_post` | none; sending any secret → `401 invalid_client` |
| PKCE `[TS]` | default **required** (`pkce_required` defaults `true`, oidc.ts:107) | forced |
| PKCE `[Rust]` | default **off** unless `pkce_required: true` (serialize.rs:133-138) | forced |
| introspect | allowed | `401 invalid_client` (verified) |

Only `S256` is accepted in both builds (`[TS]` oidc.ts:176-179, 441-449; discovery
`code_challenge_methods_supported: ["S256"]`). A challenge with no `code_challenge_method` is
treated as `S256` (`[TS]` oidc.ts:177).

### Redirect URI rules

- `[TS]`: **exact string match** against the registered list
  (`packages/server/src/services/oidc.ts:196-200`). A loopback desktop client with an ephemeral port
  must register the exact port, or use a fixed port.
- `[Rust]`: exact match **plus RFC 8252 §7.3 loopback-port tolerance** — same scheme `http`, same
  host (`localhost`/`127.0.0.1`/`[::1]`), same path and query, port may differ
  (`crates/orbitid-oidc/src/services/oidc/issuer.rs:68-83`, used in
  `applications/auth.rs:143-163`). If h4rn targets both builds, register the concrete port and use
  it, or ship a fixed loopback port.
- Custom schemes (`h4rn://callback`) are accepted by the API in both builds (`url::Url::parse` /
  zod `.url()`), but **not** by the dashboard UI.
- An unregistered `redirect_uri` at `/oauth/authorize` is rejected with **HTTP 400 JSON**, not a
  redirect (`[TS]` oidc.ts:155-160, verified: `400` for `https://evil.test/cb`).

### Device authorization grant (CLI / daemon pairing)

Off by default in both builds (`[TS]` `packages/shared/src/config.ts:113-119`, `[Rust]`
`crates/orbitid-core/src/settings/types/security.rs:57-75`). Enable with:

```bash
curl -sS -X PATCH "$ORBITID_URL/v1/instance" \
  -H "Authorization: Bearer $ORBITID_SECRET_KEY" -H "Content-Type: application/json" \
  -d '{"oidcProvider":{"deviceAuthorizationEnabled":true,
                        "deviceCodeIntervalSeconds":1,
                        "deviceCodeLifetimeSeconds":120}}'
```

That is a deep-merged settings patch (`[TS]` `packages/server/src/modules/admin/instance.ts:97-105`).

```bash
# confidential clients MUST authenticate here too (verified: otherwise 401 invalid_client)
curl -sS -u "$CLIENT_ID:$CLIENT_SECRET" -X POST "$ORBITID_URL/oauth/device_authorization" \
  -d 'scope=openid email offline_access'
```

```json
{
  "device_code": "odc_WJEbJ2hQhAuRhlRxTV1cAykEazSllbxZCgkDiGYhpqk",
  "user_code": "634Z-XMKF",
  "verification_uri": "http://localhost:4111/oauth/device",
  "verification_uri_complete": "http://localhost:4111/oauth/device?user_code=634Z-XMKF",
  "expires_in": 120,
  "interval": 1
}
```

Approval (cookie **or** bearer session JWT, see §5) via
`POST /v1/client/oauth/device/approve {"user_code":"…","action":"approve"}`
(`[TS]` oidc.ts:513-534), then poll
`POST /oauth/token grant_type=urn:ietf:params:oauth:grant-type:device_code&device_code=…`
(verified: tokens returned after approval). Errors: `authorization_pending`, `slow_down`,
`access_denied`, `expired_token`; `slow_down` raises the interval by 5s for that code's lifetime
(`[TS]` `services/oidc.ts:695-735`). Device codes are stored hashed; user codes are 8 chars from a
look-alike-free alphabet, burned after 10 attempts per code / 20 lookups per IP per 10 min
(`[TS]` `modules/oidc.ts:37-40,378-393`).

---

## 3. Scopes, claims, tokens, introspection, logout

### Scopes

`[TS]` base set (always allowed on top of the app's configured `scopes`):
`openid profile email organizations offline_access`
(`packages/server/src/services/oidc.ts:25-32,147-157`).

`[Rust]` base set: `openid profile email phone groups organizations offline_access`
(`crates/orbitid-oidc/src/services/oidc/shared.rs:19-28`), but the **application's `scopes` field is
the enforced allowlist** plus `openid` (`applications/auth.rs:10-43`). A Rust instance whose app was
created with the default `"openid profile email"`
(`crates/orbitid-core/src/resources/api.rs:158`) answers `invalid_scope` for `offline_access`.
Always send the scopes you need at registration.

Any unknown scope → `invalid_scope` (`[TS]` oidc.ts:152-157).

### Claims

`id_token` when the scope is granted (TS, verified by decoding):

```
sub, nonce, name, given_name, family_name, preferred_username, updated_at,
email, email_verified, iss, iat, exp, aud
```

- `[TS]` `buildIdTokenClaims` `packages/server/src/services/oidc.ts:309-329`.
- No `azp`, no `auth_time`, no `at_hash`, no `sid` in the TS build.
- `[Rust]` adds `azp` (= client_id), `at_hash`, `sid`, `auth_time` (from the authorizing session),
  `locale` (from public metadata) and `groups` for the `groups` scope
  (`crates/orbitid-oidc/src/services/oidc/claims.rs:305-350`).
- `sub` is the OrbitID user id `user_…`; subjects are **public** (`subject_types_supported:
  ["public"]`), never pairwise.
- `email_verified` mirrors the primary address's verification state; `email` can be `null`.

Organization claims with the `organizations` scope: `org_id`, `org_slug`, `org_role`,
`org_permissions`, and `organizations: [{id, slug, name, role}]` — active org comes from the
authorizing browser session; the device grant has no session, so only the membership list is
returned (`[TS]` oidc.ts:264-300, 327; `[Rust]` claims.rs:77-165). In the Rust build org claims
are empty unless `organizations.enabled` is on (`claims.rs:82-84`).

`userinfo` (GET or POST, `Authorization: Bearer`; TS also accepts `?access_token=` on GET) returns
`sub` plus the same profile/email/org claims filtered by the granted scopes
(`[TS]` `packages/server/src/services/oidc.ts:526-545`). Quirk: with the `profile` scope and no
avatar, TS returns `"picture": null` (verified) while the `id_token` omits the key.

### Token formats

| Setting | Access token | Verified |
|---|---|---|
| `accessTokenFormat: "opaque"` (default) | `oat_…`, must be checked at `/oauth/userinfo` or `/oauth/introspect` | yes |
| `accessTokenFormat: "jwt"` | RS256 JWT signed with the instance key; claims `sub`, `azp`, `scope`, `jti`, `sid`, plus org claims; `aud` = client_id | yes |

JWT path: `[TS]` oidc.ts:346-423; `[Rust]` `token_set.rs:83-150`. Even JWT access tokens are still
recorded in `oauth_grants`, so revoking them invalidates `/oauth/userinfo` and `/oauth/introspect`
(the signature alone is not enough) — verified: `revoke` → `200`, then `userinfo` → `401`.

`id_token` is always a JWT (RS256). Refresh tokens are opaque `ort_…`, authorization codes `oac_…`,
device codes `odc_…`.

### Lifetimes

| Item | `[TS]` | `[Rust]` |
|---|---|---|
| Authorization code | 10 min (`oidc.ts:39`) | 10 min (`shared.rs:58`) |
| Access token | 1 h (`:37`) | per-app, default 1 h, bounds 60s–24h (`shared.rs:56`, `applications/update.rs:94-100`) |
| Refresh token | 30 days (`:38`), not sliding | per-app, default 30 days, bounds 60s–365 days |
| `id_token` | 1 h (`:40`) | per-app, default 1 h, bounds 60s–24h |
| Device code | `deviceCodeLifetimeSeconds` = 600 (`config.ts:117`) | same |

`token_type` is `"Bearer"` with a capital B; `expires_in` is an integer number of seconds.
`/oauth/token`, `/oauth/revoke`, `/oauth/introspect` and `/oauth/device_authorization` accept both
`application/x-www-form-urlencoded` and JSON bodies (`docs/API.md:154-158`); `oauth2` sends form
encoding by default, which is what was verified. Public clients send `client_id` in the body;
confidential clients may use Basic auth or `client_id` + `client_secret` in the body.

### Refresh behaviour

Issued only when `offline_access` was granted (`[TS]` oidc.ts:394-412). Rotation on every use, tied
to a `family_id`; presenting a rotated-out token revokes the whole family including its access
tokens (`[TS]` oidc.ts:471-501). A refresh response contains a new `access_token`, a new
`refresh_token` and (because `openid` stays in scope) a new `id_token` — verified.

The Rust build adds a **30-second idempotent retry window** for a just-rotated token (returns the
same successor instead of treating the retry as theft) and serialises rotation with a family
advisory lock (`crates/orbitid-oidc/src/services/oidc/refresh.rs:1-110`, `shared.rs:43-44`,
`settings/types/security.rs:63,73`). The TS build has no grace window: **two concurrent refreshes in
one process will kill the family**. h4rn must single-flight its refresh.

### Introspection, revocation

- `POST /oauth/introspect` (RFC 7662): confidential clients only; returns `{"active": false}`
  for unknown/foreign/expired/revoked tokens; active responses include `scope`, `client_id`,
  `sub`, `iss`, `aud`, `exp`, `iat`, `token_type` (`[TS]` oidc.ts:574-590; `[Rust]`
  `routes/token.rs:278-301`).
- `POST /oauth/revoke` (RFC 7009): authenticates the client; revoking a refresh token revokes the
  family; a token belonging to another client is silently ignored, `200` either way (`[TS]`
  oidc.ts:566-571).
- Client auth errors set `WWW-Authenticate: Basic realm="orbitid"` (`[TS]` `modules/oidc.ts:65-66`).

### Logout / end_session

- `[TS]`: **no `end_session_endpoint`**, and `GET /oauth/end_session` → `404` (verified). The only
  logout is the Frontend API session end (`POST /v1/client/sessions/:id/end`).
- `[Rust]`: RP-initiated logout `GET|POST /oauth/end_session` with `id_token_hint`, `client_id`,
  `post_logout_redirect_uri` (must be registered in `post_logout_redirect_uris`, loopback-port
  tolerant), `state` (`crates/orbitid-oidc/src/routes/logout.rs:22-91`,
  `applications/auth.rs:165-180`). h4rn should feature-detect the endpoint from discovery rather
  than assume it.

---

## 4. Deviations that can break a generic Rust `openidconnect` client

`openidconnect` (v3, `oauth2` 5) validates discovery, the ID token signature, `iss`, `aud` and
`exp`. OrbitID satisfies those, but these are the traps:

1. **No trailing slash on `issuer`.** `[TS]` env.ts:41, `[Rust]` issuer.rs:27-58. If h4rn builds
   `IssuerUrl` from config, normalise with `trim_end_matches('/')`; a mismatch makes
   `ProviderMetadata::discover` fail with "issuer mismatch" rather than a clear error.
2. **Discovery is only at the exact root path in the TS build.** `[TS]` registers exactly
   `/.well-known/openid-configuration` (`modules/oidc.ts:146`); a prefixed issuer path (e.g.
   `https://host/auth`) will not resolve there. The Rust build serves prefixed well-known paths and
   RFC 8414 metadata (`routes/well_known.rs:19-36`). Prefer a root-mounted issuer.
3. **PKCE is mandatory by default in the TS build**, even for confidential clients
   (`pkce_required` defaults `true`, oidc.ts:107). Always send `code_challenge` +
   `code_challenge_method=S256`; `plain` is refused. In the Rust build confidential clients
   default to no PKCE, but sending PKCE works in both.
4. **Scopes are an allowlist.** `[Rust]` rejects `offline_access`/`organizations` unless they were
   configured on the app (auth.rs:10-43). `[TS]` always allows its five base scopes. Register with
   `"openid profile email offline_access"` (add `organizations` if needed).
5. **The ID token is thin in the TS build**: no `azp`, `at_hash`, `auth_time`, `sid`
   (oidc.ts:309-329). Libraries that verify `at_hash`, or that implement `max_age` /
   `prompt=login`, will fail or silently not re-authenticate. `prompt` is only interpreted for
   `none` and `consent` in TS (`modules/oidc.ts:187-190`); `prompt=login` is ignored. The Rust
   build implements `login`/`select_account` and `max_age` (`routes/discovery.rs:163-284`).
6. **`response_mode=form_post` is advertised only by the Rust build.** TS advertises `["query"]`
   and does not read the parameter: it will still 302 with the code in the query string
   (`modules/oidc.ts:150-203`). Do not request `form_post` against a TS instance.
7. **No `end_session_endpoint` in the TS build** (§3). RP-initiated logout must be optional.
8. **Refresh-token reuse kills the family in the TS build.** A retried refresh after a timeout
   looks like theft and revokes everything (oidc.ts:477-496). Single-flight refresh; only the Rust
   build has the 30 s grace.
9. **Consent and sign-in screens live on `application.homeUrl`, not on the issuer.** Default is
   `http://localhost:3000` (`[TS]` `packages/shared/src/config.ts:42`, `[Rust]`
   `settings/types/general.rs:23`). With consent enabled and no prior consent, `/oauth/authorize`
   redirects to `http://localhost:3000/oauth-consent?...` (verified); with no session it redirects
   to `http://localhost:3000/sign-in?redirect_url=...` (verified). For automated tests either set
   `consent_screen_enabled: false` (do this) or `PATCH /v1/instance` to set
   `application.homeUrl` to the OrbitID server URL (it serves the dashboard at `/`).
10. **Introspection requires a confidential client**; public clients get `401 invalid_client`
    (`[TS]` modules/oidc.ts:350-351, verified).
11. **Errors are dual-shaped.** `/oauth/*` returns RFC 6749 `{error, error_description}`; every
    `/v1/*` route returns `{errors:[{code,message,…}], trace_id}` (API.md:112-125). `oauth2`'s
    error parser is fine with the former; do not reuse the Backend API error type for OAuth calls.
12. **CORS differs.** TS applies the credentials-enabled Frontend API policy to every route, so a
    browser on an unlisted origin cannot call `/oauth/token` (`[TS]` `packages/server/src/app.ts:47-57`).
    Rust answers wildcard CORS on the OIDC endpoints (`crates/orbitid-oidc/src/lib.rs:57-116`).
    Server-side h4rn is unaffected; a browser-based pair flow is not.
13. **`aud` is a single string, not an array**, and equals `client_id` (verified). `azp` equals
    `client_id` when present. `openidconnect` handles both shapes, but hand-written claim structs
    should use an untagged enum or `String`.
14. **Rate limits.** Sign-in, device lookup and device authorization are rate limited and can
    return `429` with `Retry-After` (`[TS]` `modules/oidc.ts:358`, README of the API). A test suite
    that signs in hundreds of times from one IP will hit this; reuse one session.
15. **Sign-in sets `__client` twice** in one response (verified: two `Set-Cookie` headers with
    different values, last one valid). Keep the last cookie or, better, use the session JWT.
16. **`token_type` is `"Bearer"`**, `scope` is a space-separated string, and `expires_in` is
    seconds — all standard, just noting because the API elsewhere uses snake_case objects.

Nothing in the flow needs a non-standard grant: authorization code + S256 PKCE + `offline_access`
works with `openidconnect::core::CoreClient` unchanged, provided items 1, 3, 4 and 9 are handled.

---

## 5. Running OrbitID locally for h4rn integration tests

### Option A — the released TypeScript build (what `main` builds)

```bash
cd /path/to/orbitid                 # the tree with packages/server
cp .env.example .env
{
  echo "ORBITID_ENCRYPTION_KEY=$(openssl rand -base64 32)"
  echo "POSTGRES_PASSWORD=$(openssl rand -hex 16)"
  echo "ORBITID_BASE_URL=http://localhost:4000"
  echo "ORBITID_PORT=4000"
} > .env

docker compose up -d --wait         # postgres:17-alpine + the app on :4000
curl -fsS http://localhost:4000/health
```

Files: `[TS]` `docker-compose.yml:1-40`, `Dockerfile:1-27`, `.env.example`, `docs/SELF_HOSTING.md:11-26`.

Notes:

- The TS compose file forwards only the variables listed under `environment:`
  (`docker-compose.yml:24-33`). `ORBITID_SECRET_KEY` is **not** among them, so setting it in `.env`
  has no effect there — derive it or print it.
- `NODE_ENV=production` is baked into the image (`Dockerfile:11`), so the derived key is `sk_live_…`.

### Secret key, non-interactively

Two ways, both deterministic:

1. Derive it. `sk_<test|live>_` + base64url(HMAC-SHA256(key = `ORBITID_ENCRYPTION_KEY`,
   message = `orbitid/secret-key`)) — `[TS]` `packages/server/src/bootstrap.ts:17-19`, `[Rust]`
   `crates/orbitid-core/src/state.rs:188-200`.

   ```bash
   node -e 'const{createHmac}=require("crypto");
     console.log("sk_live_"+createHmac("sha256",process.env.ORBITID_ENCRYPTION_KEY)
       .update("orbitid/secret-key").digest("base64url"))'
   ```

   **Verified**: the value computed this way equals the key the server prints at boot
   (`sk_test_…` in development mode, `sk_live_…` with `NODE_ENV=production`).

2. Print it from the container: `docker compose exec orbitid node_modules/.bin/tsx
   packages/server/src/cli.ts keys` (TS, `docs/SELF_HOSTING.md:20-26`) or `orbitid keys` (Rust,
   `crates/orbitid-server/src/cli.rs:49,130`).

### Option B — the Rust rewrite

Same `docker compose up -d --wait`, but `POSTGRES_PASSWORD` is mandatory (`:?` in the compose file),
`ORBITID_SECRET_KEY` **is** forwarded, the image is built from source, there is no data volume and
the container runs read-only (`[Rust]` `docker-compose.yml:1-80`, `Dockerfile:76-90`). One-shot invocation:

```bash
POSTGRES_PASSWORD=orbitid-test ORBITID_ENCRYPTION_KEY="$(openssl rand -base64 32)" \
  docker compose up -d --wait
```

### Create the client and settings programmatically

```bash
SK=$(node -e '…')                       # or the value from `keys`
APP=$(curl -sS -X POST http://localhost:4000/v1/oauth_applications \
  -H "Authorization: Bearer $SK" -H "Content-Type: application/json" \
  -d '{"name":"h4rn-connect",
       "redirect_uris":["http://127.0.0.1:53682/callback"],
       "scopes":"openid profile email offline_access",
       "public":false,"consent_screen_enabled":false,"pkce_required":true}')
CLIENT_ID=$(echo "$APP" | jq -r .client_id)
CLIENT_SECRET=$(echo "$APP" | jq -r .client_secret)

# Only if the test needs the browser sign-in/consent screens or the device grant:
curl -sS -X PATCH http://localhost:4000/v1/instance \
  -H "Authorization: Bearer $SK" -H "Content-Type: application/json" \
  -d "{\"application\":{\"homeUrl\":\"http://localhost:4000\"},
       \"oidcProvider\":{\"deviceAuthorizationEnabled\":true}}"
```

Teardown (also removes the Postgres volume so the next run starts clean):

```bash
docker compose down -v --remove-orphans
```

For parallel test runs use a compose project name and a free port:
`docker compose -p h4rn-it-$RANDOM up -d --wait` with `ORBITID_PORT=14000`.

### Fully headless authorization-code flow (verified against the TS build)

No browser and no consent screen needed. The trick is that `/oauth/authorize` resolves the caller
from a **session JWT bearer** as well as from the `__client` cookie
(`[TS]` `packages/server/src/modules/machine/auth.ts:54-75`; `[Rust]`
`crates/orbitid-identity/src/machine_auth.rs:72-120`).

```bash
# 1. create a user with a verified email and a password (Backend API)
curl -sS -X POST "$ORBITID_URL/v1/users" -H "Authorization: Bearer $SK" \
  -H 'Content-Type: application/json' \
  -d '{"email_address":["ada@h4rn.test"],"password":"Sup3rSecret!","first_name":"Ada"}'

# 2. sign in (Frontend API); the session id is client.last_active_session_id
SESS=$(curl -sS -c jar.txt -X POST "$ORBITID_URL/v1/client/sign_ins" \
  -H 'Content-Type: application/json' \
  -d '{"identifier":"ada@h4rn.test","strategy":"password","password":"Sup3rSecret!"}' \
  | jq -r '.client.last_active_session_id')
#    (a repeat sign-in on the same client returns 400 identifier_already_signed_in and
#     meta.session_id with the existing session)

# 3. mint a session JWT (needs the __client cookie)
curl -sS -b jar.txt -X POST "$ORBITID_URL/v1/client/sessions/$SESS/tokens" \
  -H 'Content-Type: application/json' -d '{}'      # -> {"jwt":"eyJ…"}

# 4. authorize: 302 to the registered redirect_uri with ?code=…&state=…
curl -sS -D - -o /dev/null -H "Authorization: Bearer $JWT" \
  "$ORBITID_URL/oauth/authorize?response_type=code&client_id=$CLIENT_ID\
&redirect_uri=http%3A%2F%2F127.0.0.1%3A53682%2Fcallback\
&scope=openid%20profile%20email%20offline_access&state=xyz&nonce=n-1\
&code_challenge=$S256_CHALLENGE&code_challenge_method=S256"

# 5. exchange the code
curl -sS -u "$CLIENT_ID:$CLIENT_SECRET" -X POST "$ORBITID_URL/oauth/token" \
  -d grant_type=authorization_code -d "code=$CODE" \
  -d 'redirect_uri=http://127.0.0.1:53682/callback' -d "code_verifier=$VERIFIER"

# 6. userinfo / refresh / introspect / revoke as in §3
```

Verified outputs: the authorize 302 was
`h4rn://callback?code=oac_…&state=xyz` for a custom scheme, and
`http://127.0.0.1:53682/callback?code=…&state=…` for loopback; the exchange returned
`access_token`, `token_type`, `expires_in: 3600`, `scope`, `refresh_token`, `id_token`.
With `consent_screen_enabled: true` the same call redirects to
`<homeUrl>/oauth-consent?...`; `POST /oauth/consent` with the same bearer JWT completes it
(`{"client_id","approve":true,"scope","redirect_uri","state","response_type","code_challenge",
"code_challenge_method"}` → `{"redirect_url":"…?code=…&state=…"}`).

---

## 6. Is there a hosted instance?

**No.** OrbitID is self-hosted only; there is no SaaS and no default issuer anywhere:

- `README.md:5` — "There is no hosted service and nothing is billed."
- `docs/SELF_HOSTING.md:3` — "there is no hosted service behind it."
- `docs/ENVIRONMENT.md:48` — "There is no hosted default, so the SDK refuses to start without it."

A repo-wide search for `orbitid.dev|orbitid.com|orbitid.io|orbitid.app|orbitid.cloud|ghcr.io|
docker pull` found no instance URL and no published image reference; the only URLs in the docs are
placeholders (`https://auth.example.com`) plus the local defaults. The two review briefs in
`~/.cache/orbitid-r{3,4}-brief.md` mention no hosted URL either.

**Implication for h4rn:** `h4rn-connect` must take the issuer from configuration/env (e.g.
`OIDC_ISSUER_URL`), discover the rest from `/.well-known/openid-configuration`, and allow the
client id/secret, scopes and redirect URI to be configured too. There is nothing to hardcode, and
tests must bring their own instance (compose, as in §5).

### Recommended h4rn configuration

```
OIDC_ISSUER_URL=https://auth.example.com     # = ORBITID_BASE_URL, no trailing slash
OIDC_CLIENT_ID=oa_…
OIDC_CLIENT_SECRET=oas_…                     # empty for a public/CLI client
OIDC_SCOPES=openid profile email offline_access
OIDC_REDIRECT_URI=https://relay.example.com/oidc/callback  # or h4rn://callback
```

- Confidential client + authorization code + S256 PKCE + `offline_access`, consent off for tests.
- Single-flight refresh; treat `invalid_grant` on refresh as "re-authenticate the user".
- Feature-detect `end_session_endpoint`; only the Rust build advertises it.
- Verify ID tokens with the advertised `jwks_uri` (`/oauth/jwks`), RS256, `aud` = client_id.
- Do not assume `azp`/`at_hash`/`auth_time`/`sid` exist (TS build omits them).
- If the relay verifies access tokens offline, ask the operator to set
  `oidcProvider.accessTokenFormat = "jwt"`, otherwise call `/oauth/userinfo` per request (or cache
  introspection results briefly).
