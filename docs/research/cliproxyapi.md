# CLIProxyAPI — integration research for h4rn

Sources read (read-only clones):

| Repo | Path | HEAD | Describe |
|---|---|---|---|
| upstream `router-for-me/CLIProxyAPI` | `/tmp/h4rn-research/cpa-upstream` | `1aaf56bc6a0fc6fce19049ba7ae8554c882304a6` (2026-11-13) | `v8.0.69-1-g1aaf56bc` |
| fork `chark1es/CLIProxyAPI` (as cloned) | `/tmp/h4rn-research/cpa-fork` | `9b8e59955e08b278ae752d0b3fc8994e004355d0` (2026-11-12) | `v8.0.47-46-g9b8e5995`, 260 commits behind upstream |

Legend — **verified** = read in source at the cited line and cross-checked against a test or second
call site; **read** = present in source/docs but not exercised; **UNVERIFIED** = inferred, not
confirmed. Numbers in this document are counts from source, not impressions.

---

## 0. Verdict for h4rn

CLIProxyAPI ("CPA") is **not** a harness. It is a **multi-credential reverse proxy**: it terminates
OpenAI-, Anthropic- and Gemini-shaped APIs on one port, maps each incoming request onto one of N
stored credentials (OAuth subscriptions or API keys), retries across them, and exposes a large
management API for reading/patching its own config.

For h4rn there are three separable things worth taking:

1. **As an upstream provider.** CPA is a valid `base_url` for every harness h4rn drives
   (Claude Code, Codex, OpenCode, Gemini CLI, Antigravity). §7 has verified recipes. This is the
   cheapest integration: h4rn treats it as one more OpenAI/Anthropic-compatible endpoint.
2. **As a pattern library.** The interesting ideas are credential cooldowns with model-level
   granularity (`CooldownView`), a three-format `/v1/models` dispatcher keyed on request headers,
   and a "management API that is 404 unless configured" stance. §2, §3, §5.
3. **As a management surface to mirror.** 144 `/v0/management` routes plus a v8 config-layout
   projection. If h4rn ships a UI, this route set is the reference for what users ask for.

What it is **not**: there is **no server-side per-key rate limiting**, **no persistent usage
history**, and **no queue/quota accounting that survives a restart**. §5.7 documents those
negatives, because a naive reading of the route names (`/usage`, `/api-key-usage`, `/quota/*`)
suggests more than exists.

---

## 1. Process and surfaces

One HTTP server. Public API, management API, OAuth callbacks and a static control panel all share
the same listener (default `:8317` — `internal/config/parse.go`, verified by
`internal/config/config_v8_test.go:27`).

| Surface | Path root | Auth plane |
|---|---|---|
| Client API | `/v1/*`, `/v1beta/*`, `/openai/v1/*`, `/backend-api/codex/*`, `/v1/realtime*`, `/v1/tunnel` | **client access key** (`access.api-keys`) |
| Management API | `/v0/management/*`, `/v8/management/*` | **management key** (separate secret) |
| Control panel | `GET /management.html` | none (it calls the management API from the browser) |
| Health | `GET`/`HEAD /healthz` → `{"status":"ok"}` | none |
| Root | `GET /` → `{"message":"CLIProxyAPI Server","endpoints":[...]}` | none |
| OAuth redirects | `/anthropic/callback`, `/codex/callback`, `/antigravity/callback`, `/callback`, `/devin/callback` (+ provider-specific) | none (state-validated) |

These are **two independent auth planes** and conflating them is the most common integration
mistake: the management key is a single shared secret, not an access key, and access keys cannot
reach `/v0/management`.

### 1.1 Where state lives

| State | Location | Survives restart |
|---|---|---|
| Config | `config.yaml`, rewritten in place by management writes | yes |
| Credentials | `auth-dir` (`~/.cli-proxy-api` by default), one JSON per credential | yes |
| Per-credential counters (`success`/`failed`/`recent_requests`) | in-memory `coreauth.Auth` | **no** |
| Cooldown state | in-memory, optionally persisted (`routing.cooldown.save-cooldown-status`) | partial |
| Usage queue | in-process ring buffer (`internal/redisqueue/queue.go:30`) | **no** |
| Request/error logs | files on disk when enabled | yes |

The package is called `redisqueue` but there is **no Redis**: `internal/redisqueue/queue.go` is a
mutex-guarded slice plus a subscriber map. This matters for feature detection (§6) and for anyone
expecting a durable bus.

---

## 2. Management API — `/v0/management`

### 2.1 Availability: routes can be *absent*, not just unauthorized

Three separate gates, in this order (`internal/api/server_management.go:213-227`,
`internal/api/server.go:255-266`):

```go
hasManagementSecret := cfg.RemoteManagement.SecretKey != "" || envManagementSecret || s.localPassword != ""
s.managementRoutesEnabled.Store(hasManagementSecret)
if hasManagementSecret { s.registerManagementRoutes() }
```

and in the middleware:

```go
if cfg.Home.Enabled                { 404 }   // home mode hides management entirely
if !s.managementRoutesEnabled.Load() { 404 }
```

So:

| Condition | Result |
|---|---|
| no `remote-management.secret-key`, no `MANAGEMENT_PASSWORD`, no local password | routes never registered → **404** (not 401) |
| `home.enabled: true` | **404** |
| routes enabled, but `secret-key` empty **and** env empty (local-password-only boot) | **403 `remote management key not set`** |

That third row is a real bug-shaped edge case, verified at
`internal/api/handlers/management/handler.go:427`: `registerManagementRoutes` is called when only
`s.localPassword` is set (`server.go:258`), but `AuthenticateManagementKey` rejects before it ever
consults `localPassword`, because `secretHash == "" && envSecret == ""`. A TUI-mode deployment with
only a local password therefore serves 404→403 and can never authenticate. **Do not model
"routes exist ⇒ key is usable".**

Reload can toggle availability at runtime (`internal/api/server_reload.go:130-175`), so a 404 is
not necessarily permanent.

### 2.2 Authentication

`internal/api/handlers/management/handler.go:324-455`, verified.

Accepted credentials, in order:

1. `Authorization: Bearer <key>` (case-insensitive `bearer`), else
2. `Authorization: <key>` raw, else
3. `X-Management-Key: <key>`

`provided` is the first non-empty of those. Then:

| Check | Outcome |
|---|---|
| client IP not `127.0.0.1`/`::1` and `allow-remote` false | 403 `remote management disabled` |
| 5 failed attempts from an IP | 30-min ban → 403 `IP banned due to too many failed attempts. Try again in <dur>` |
| both `secret-key` hash and env secret empty | 403 `remote management key not set` |
| no credential supplied | 401 `missing management key` |
| local client and matches `localPassword` | allow |
| matches `MANAGEMENT_PASSWORD` (any client) | allow |
| bcrypt match against `remote-management.secret-key` | allow |
| otherwise | 401 `invalid management key` |

`MANAGEMENT_PASSWORD` sets `allowRemoteOverride = true` (`handler.go:76-86`), i.e. it **defeats**
`allow-remote: false`. `remote-management.secret-key` is bcrypt-hashed on load, so the plaintext
never appears in `GET /config` (which returns the loaded struct, hash included).

Every failure body is the same shape: `{"error":"<exact string above>"}`.

**Response headers are emitted before authentication** (`handler.go:326-329`), so they are visible
on a 401 and are the cheapest feature probe in the product:

| Header | Source | Value |
|---|---|---|
| `X-CPA-VERSION` | `buildinfo.Version` | `v8.0.69` style |
| `X-CPA-COMMIT` | `buildinfo.Commit` | git SHA |
| `X-CPA-BUILD-DATE` | `buildinfo.BuildDate` | build stamp |
| `X-CPA-SUPPORT-PLUGIN` | `pluginhost.SupportPluginHeaderValue()` | `"1"` with CGO, `"0"` without |

They are **not** set on the two `oauth-callback` routes, which bypass the group middleware
(`server_management.go:25-26`).

### 2.3 Route inventory — 144 method/path pairs

Extracted mechanically from `internal/api/server_management.go`; all 144 are inside the
key-requiring group except the two `oauth-callback` routes. Upstream and fork lists are **identical**
at both HEADs. Group counts reconcile as 4 + 34 + 9 + 5 + 6 + 32 + 12 + 10 + 10 + 8 + 12 + 2 = 144.

**Config, version, panel (4)**

| Method | Path | Notes |
|---|---|---|
| GET | `/config` | loaded config object (secrets hashed/masked as stored) |
| GET | `/config.yaml` | raw YAML, `application/yaml; charset=utf-8`, `Cache-Control: no-store`, `X-Content-Type-Options: nosniff` |
| PUT | `/config.yaml` | validate-then-write; 400 `invalid_yaml`, 422 `invalid_config`; 404 `{"error":"not_found","message":"config file not found"}` |
| GET | `/latest-version` | `{"latest-version":"vX.Y.Z"}`; errors `request_create_failed`, `request_failed`, `unexpected_status`, `decode_failed`, `invalid_response` |

**Scalars, flags and routing (34)**

| Method(s) | Path | Body / response |
|---|---|---|
| GET, PUT, PATCH | `/debug` | `{"debug":true\|false}` |
| GET, PUT, PATCH | `/logging-to-file` | own key name |
| GET, PUT, PATCH | `/logs-max-total-size-mb` | `{"logs-max-total-size-mb":N}` |
| GET, PUT, PATCH | `/error-logs-max-files` | `{"error-logs-max-files":N}` |
| GET, PUT, PATCH | `/usage-statistics-enabled` | gates the usage queue |
| GET, PUT, PATCH | `/force-model-prefix` | own key name |
| GET, PUT, PATCH, DELETE | `/proxy-url` | DELETE clears |
| GET, PUT, PATCH | `/ws-auth` | websocket auth toggle |
| GET, PUT, PATCH | `/routing/strategy` | `{"strategy": ...}`, one of `round-robin`, `weighted-round-robin`, `fill-first` |
| GET, PUT, PATCH | `/quota-exceeded/switch-project` | `{"switch-project":bool}` |
| GET, PUT, PATCH | `/quota-exceeded/switch-preview-model` | `{"switch-preview-model":bool}` |

Every write returns `{"status":"ok"}`. PUT/PATCH bodies are `{"value": <v>}` except where a named
key is shown.

**Retry / cooldown knobs (9)**

| Method(s) | Path | Key |
|---|---|---|
| GET, PUT, PATCH | `/request-retry` | `request-retry` (int) |
| GET, PUT, PATCH | `/max-retry-credentials` | `max-retry-credentials` |
| GET, PUT, PATCH | `/max-retry-interval` | `max-retry-interval` (seconds) |

**Operations / quota (5)**

| Method | Path | Shape |
|---|---|---|
| POST | `/api-call` | `{auth_index?, method, url, proxy_url?, header?, data?}` → `{status_code, header, body}`; 400 `missing method`/`missing url`/`invalid url`/`invalid proxy_url` |
| GET | `/quota/providers` | `{providers:[{plugin_id, provider, display_name, supported_providers, supports_reset}]}` |
| POST | `/quota/fetch` | `{auth_index\|authIndex\|AuthIndex, plugin_id?, provider?}` |
| POST | `/quota/reset` | same body; 404 `quota provider not found for plugin` |
| POST | `/reset-quota` | `{auth_index}` → `{status:"ok", auth_index, models}` |

**Access keys and usage (6)**

| Method(s) | Path | Shape |
|---|---|---|
| GET, PUT, PATCH, DELETE | `/api-keys` | `{"api-keys":["sk-...", ...]}` |
| GET | `/api-key-usage` | see §5.3 |
| GET | `/usage-queue` | see §5.3 |

**Provider credential families (32)** — each family is GET/PUT/PATCH/DELETE:

`/gemini-api-key`, `/interactions-api-key`, `/claude-api-key`, `/codex-api-key`, `/xai-api-key`,
`/meta-api-key`, `/vertex-api-key`, `/openai-compatibility`.

GET returns `{<config-key>: [ ... ]}`. PUT accepts either the bare array or `{"items":[...]}`.
PATCH body `{index|match, value:{...}}`. DELETE accepts `?index=`, `?api-key=` (+ `&base-url=`
when disambiguating). `/openai-compatibility` DELETE also accepts `?name=`.

**OAuth filters and alias tables (12)** — GET/PUT/PATCH/DELETE each:

`/oauth-excluded-models`, `/oauth-model-alias`, `/oauth-request-scoped-errors`.

All three are `map[channel][]entry` documents in both directions, e.g.
`{"oauth-model-alias":{"claude":[{...}]}}`. `/oauth-model-alias` PATCH takes
`{provider|channel, aliases:[...]}` and returns 400 `invalid channel` if neither is supplied.

**Auth files and model registry (10)**

| Method | Path | Notes |
|---|---|---|
| GET | `/auth-files` | see §5.4; `page`/`page_size` (default 50), filters `name`, `auth_index` |
| GET | `/auth-files/models?name=` | `{models:[{id, display_name?, type?, owned_by?}]}`; 400 `name is required` |
| GET | `/auth-files/download?name=` | attachment; 400 `invalid name`/`name must end with .json`, 404 `file not found` |
| POST | `/auth-files` | multipart or raw JSON upload |
| DELETE | `/auth-files` | `?name=`, or `?all=true\|1\|*` → `{status:"ok", deleted:N}` |
| PATCH | `/auth-files/status` | `{name, auth_index?, disabled}` |
| PATCH | `/auth-files/fields` | arbitrary field patch on the credential file |
| POST | `/auth-files/refresh` | `{name\|all\|auth_index}` or `?all=true` → `{ok:true, results}` |
| POST | `/vertex/import` | multipart `file` |
| GET | `/model-definitions/:channel` | `{channel, models}`; 400 `{"error":"unknown channel","channel":...}` |

`model-definitions` channels (`internal/registry/model_definitions.go:554-604`): `claude`, `gemini`,
`gemini-interactions`, `vertex`, `aistudio`, `codex`, `kimi`, `antigravity`, `xai`, `devin`, `meta`.

**OAuth login flows (10)**

| Method | Path |
|---|---|
| GET | `/anthropic-auth-url`, `/codex-auth-url`, `/antigravity-auth-url`, `/kimi-auth-url`, `/kimi-ai-auth-url`, `/xai-auth-url`, `/devin-auth-url`, `/meta-auth-url` |
| GET | `/get-auth-status?state=` |
| DELETE | `/oauth-session?state=` |

`get-auth-status` returns `{status:"ok"}` with no state, `{status:"error","error":"invalid state"}`,
`{"status":"error","error":"unknown or expired state"}`, or `{status:"wait"}` mid-commit.
`DELETE /oauth-session` → `{status:"ok", cancelled:bool}`, 400 `missing state`/`invalid state`.

**Logs (8)**

| Method | Path | Notes |
|---|---|---|
| GET | `/logs` | requires `logging-to-file`; `{lines, line-count, latest-timestamp, next-cursor[, cursor-reset]}` |
| DELETE | `/logs` | clear |
| GET | `/request-error-logs` | `{files:[{name,size,modified}]}`; empty when `request-log` is true |
| GET | `/request-error-logs/:name` | download |
| GET | `/request-log-by-id/:id` | **file attachment**, not JSON |

**Plugins (12)**

| Method | Path |
|---|---|
| GET | `/plugins` |
| GET | `/plugin-store` |
| POST | `/plugin-store/:id/install` |
| DELETE | `/plugins/:id` |
| PATCH | `/plugins/:id/enabled` |
| GET, PUT, PATCH | `/plugins/:id/config` |
| GET, POST, DELETE | `/plugins/:id/quota` |
| POST | `/plugins/:id/quota/reset` |

`GET /plugins` → `{plugins_enabled, plugins_dir, plugins:[{id, path, configured, registered,
enabled, effective_enabled, supports_oauth, oauth_provider, oauth_providers?, oauth_provider_entries?,
supports_quota, quota_provider?, logo, config_fields, menus, metadata}]}`.

**OAuth callbacks (2)** — the only two routes outside the group middleware:

| Method | Path |
|---|---|
| GET | `/v0/management/oauth-callback` |
| POST | `/v0/management/oauth-callback` |

### 2.4 `/v8/management`

Registered from `internal/api/server_management_v8.go`. Same key, same middleware. Differences that
matter:

| Feature | v0 | v8 |
|---|---|---|
| Config read | `GET /config` returns the **loaded struct** | `GET /config` returns the **v8-layout projection** |
| Config writes | per-field routes | `PUT`/`PATCH /config` and `DELETE /config/*path` |
| Scalars | `{"value": x}` | direct JSON values |
| Optimistic concurrency | none | strong `ETag` + `If-Match` → 412 `precondition_failed` |
| Legacy field names on write | accepted | **rejected** |

Per `docs/management-api-v8.md` (read): `PUT /config` replaces, `PATCH` merges, `DELETE
/v8/management/config/*path` removes a subtree, and the path namespace is the v8 layout
(`server.*`, `access.api-keys`, `routing.*`, `oauth.*`, `upstream.*`, `observability.*`,
`multimedia.*`, `credentials.*`). The alias table is `internal/config/config_v8.go:80-140`
(e.g. `{"api-keys", "access.api-keys"}`, `{"request-retry", "routing.retry.request-retry"}`).

Note the v8 layout maps `{"remote-management", "management"}`, so on v8 the management block lives
under `management.*` rather than `remote-management.*`.

Also present upstream v8 only: `POST /v8/management/credentials/verify`. The fork does **not** have
it. Both have `/config/*path`, SIWC (`/v8/management/siwc/*`) and plugin routes.

### 2.5 Fork-only: `GET /v8/management/performance`

Added by fork commit `92fc192b` ("feat(management): record per-call generation throughput"), present
in fork HEAD, **absent upstream**. There is no `/v0/management/performance`.

Query: `from`, `to` (RFC3339 **UTC**), `provider`, `model`, `limit` (default 100, max 500),
`offset`. Window max 7 days, default last 1 hour.

Errors: `from/to must be an RFC3339 timestamp`, `from/to must be UTC`, `to must be after from`,
`query window must not exceed 7 days`, `limit must be a positive integer`,
`offset must be a non-negative integer`.

Response (`internal/performance/types.go`, `performance.go`):

```
{ from, to, retention_start, recording_since, truncated,
  options: { providers:[], models:[] },
  totals:  { calls, successful, failed, measured_calls, output_tokens, duration_ms, tps, timing_basis },
  groups:  [ ... ],
  series:  [ 60 ... ],
  total_requests,
  requests: [ { id, provider, model, requested_at, stream, failed, output_tokens,
                latency_ms, ttft_ms, duration_ms, tps, timing_basis } ] }
```

`timing_basis` ∈ `generation`, `end_to_end`, `unavailable`, `mixed`. Retention: 7 days / 50,000
calls. This is the only real throughput/TPS surface in either repo; everything else in §5 is
counts-only.

> Do not document `GET /v8/management/observability/requests/in-flight`: it exists in commit
> `b285660f` on a side branch but is in **neither** HEAD.

---

## 3. Model listing

### 3.1 `GET /v1/models` is three endpoints wearing one path

Dispatch (`internal/api/server_routes.go:628-665`), verified:

```
1. UA is Grok shell                  -> flat Grok list
2. query has "client_version"        -> Codex compact list (home mode: home handler)
3. home.enabled                      -> home model list
4. Anthropic-Version header present
   OR User-Agent starts "claude-cli"  -> Claude format
5. otherwise                         -> OpenAI format
```

Two consequences for h4rn: model-list **shape depends on headers, not on a path**, and a client
that sets no identifying header gets OpenAI shape even if it will only speak Anthropic later.

### 3.2 Claude shape

`internal/client/claude/models/models.go`:

```json
{ "data": [ { "type": "model", "id": "claude-sonnet-5-5",
              "display_name": "Claude Sonnet 5.5",
              "created_at": "2025-11-04T00:00:00Z",
              "max_input_tokens": 200000, "max_tokens": 64000 } ],
  "has_more": false, "first_id": "...", "last_id": "..." }
```

Sorted by `display_name`, then `id`. `created_at` is RFC3339 derived from a Unix `created`. Models
whose ID does not start with `claude-` get a cloaking prefix `claude-fable-5-dd-` plus the reversed
ID, unless `claude-code.disable-cloaking-model-list` is set — a deliberate anti-fingerprinting hack
worth knowing about before diffing lists.

### 3.3 OpenAI shape

`sdk/api/handlers/openai/openai_handlers.go:65-110`:

```json
{ "object": "list",
  "data": [ { "id": "gpt-5.1", "object": "model", "created": 1699000000, "owned_by": "openai" } ] }
```

`created`/`owned_by` are optional upstream.

### 3.4 Gemini shape

`sdk/api/handlers/gemini/gemini_handlers.go:46-84`:

```json
{ "models": [ { "name": "models/gemini-3-pro",
                "displayName": "...", "description": "...",
                "supportedGenerationMethods": ["generateContent"] } ] }
```

Names carry the `models/` prefix; display name and description fall back to the raw name;
`supportedGenerationMethods` defaults to `["generateContent"]`.

### 3.5 `GET /v1/models/{model}`

`sdk/api/handlers/handlers_interceptors.go:736-771`. Filters the merged list by `id` **or** `slug`
and returns the entry itself (not wrapped). Miss:

```json
{"error":{"message":"Model not found","type":"invalid_request_error","code":"model_not_found"}}
```

### 3.6 `/v1beta/models` and `/v1beta/models/*action`

`GET /v1beta/models` → Gemini shape. `GET`/`POST /v1beta/models/*action` handles both
`models/{m}` and `models/{m}:generateContent` / `:streamGenerateContent` / `:countTokens` /
`:embedContent` style actions by inspecting the captured wildcard. **`/v1beta1` is not registered**
— a client hardcoded to `v1beta1` will 404 (§7.5).

### 3.7 Custom and alias models — where they are configured

Three independent mechanisms. All are `name` → `alias` renames except the third, which also
supplies metadata.

**(a) `oauth.model-alias.<channel>[]`** — `internal/config/config_types.go:588-600`

```yaml
oauth:
  model-alias:
    claude:
      - name: claude-sonnet-5-5      # upstream / upstream-reported ID
        alias: sonnet                # what clients see
        display-name: Sonnet
        force-mapping: false
        fork: false
        openapi: false
```

Channels: `vertex`, `aistudio`, `antigravity`, `claude`, `codex`, `kimi`, `xai`, `meta`, plus
plugin providers. `fork: true` keeps the original ID *and* adds the alias. `force-mapping` rewrites
the model on the way upstream rather than only in listings.

**(b) `openai-compatibility[].models[]`** — same style plus capability fields:

```yaml
openai-compatibility:
  - name: my-gateway
    base-url: https://gw.example/v1
    api-keys: [...]
    models:
      - name: upstream-id
        alias: client-id
        display-name: ...
        max-context-length: 200000
        force-mapping: false
        input-modalities: [text, image]
        output-modalities: [text]
        thinking: {...}
        image: false
        is-compat: true
        use-max-completion-tokens: true
```

**(c) `api-keys.<provider>[].models[]`** — the v8-layout per-upstream model list, same fields.

Credential structs also carry per-model overrides: `ClaudeModel`, `CodexModel`, `GeminiModel` each
have `name`, `alias`, `display-name`, `max-context-length`, `force-mapping`, `is-compat`,
`thinking`; Codex adds `support-configuration-update`.

`GET /model-definitions/:channel` is the read-only static catalogue per channel — useful for a UI
picker, but it is *not* what `/v1/models` serves.

### 3.8 Fork enrichment of `/v1/models`

Fork commit `6ae18171` adds, per entry: `name`, `max_output_length`,
`reasoning_parameters.efforts`, and passes registry fields through —
`context_length`, `max_context_length`, `max_completion_tokens`, `input_modalities`,
`output_modalities`, `thinking`, `supported_parameters`, `type`, `display_name`, `version`. Required
OpenAI fields are preserved. The fork also extends registry `convertModelToMap`
(`internal/registry/model_registry.go:1906-1913`) with modalities/thinking, which upstream lacks.

Consequence: a client can detect the fork by asking for `/v1/models` with OpenAI shape and looking
for `max_output_length` / `reasoning_parameters` on an entry. **UNVERIFIED** whether every entry
carries them (depends on registry metadata).

---

## 4. Compatible endpoints

### 4.1 Route table

`internal/api/server_routes.go:54-150`, verified.

**OpenAI family** (`/v1`, access-key auth)

| Method | Path |
|---|---|
| GET | `/models`, `/models/*model` |
| POST | `/chat/completions`, `/completions` |
| POST | `/images/generations`, `/images/edits` |
| POST | `/videos`, `/videos/generations`, `/videos/edits`, `/videos/extensions` |
| GET | `/videos/:request_id` |
| POST | `/audio/speech`, `/tts` |
| POST | `/alpha/search` |
| POST | `/live`; GET `/live/:call_id` |

**OpenAI Responses / Codex**

| Method | Path |
|---|---|
| GET | `/v1/responses` (websocket upgrade) |
| POST | `/v1/responses`, `/v1/responses/compact` |
| GET/POST | `/backend-api/codex/responses`, `/backend-api/codex/responses/compact` |
| POST | `/backend-api/codex/alpha/search` |

`/backend-api/codex/*` exists so Codex's `chatgpt_base_url` can be pointed at CPA directly.

**Anthropic family**

| Method | Path |
|---|---|
| POST | `/v1/messages` |
| POST | `/v1/messages/count_tokens` |

**Gemini family** (`/v1beta`)

| Method | Path |
|---|---|
| GET | `/models` |
| POST | `/interactions` |
| GET, POST | `/models/*action` |

**Multimedia (second namespace)**

| Method | Path |
|---|---|
| POST | `/openai/v1/videos` |
| GET | `/openai/v1/videos/:video_id`, `/openai/v1/videos/:video_id/content` |

**Realtime**

| Method | Path | Auth |
|---|---|---|
| GET | `/v1/realtime` | realtime middleware |
| POST | `/v1/realtime`, `/v1/realtime/calls` | realtime |
| GET | `/v1/realtime/calls/:call_id` | realtime |
| POST | `/v1/realtime/client_secrets`, `/v1/realtime/sessions`, `/v1/realtime/transcription_sessions` | standard |
| GET, POST | `/v1/realtime/translations` | realtime |
| POST | `/v1/realtime/translations/client_secrets` | standard |
| POST | `/v1/realtime/calls/:call_id/{hangup,accept,reject,refer}` | standard |

**Tunnel** — `GET /v1/tunnel` plus POST/PUT/PATCH/DELETE/HEAD/TRACE/CONNECT on the same path;
503 `{"error":"tunnel hub unavailable"}` without a hub.

### 4.2 Client authentication

`internal/access/config_access/provider.go:55-90`, verified. Tried in order, first match wins:

| Source | Notes |
|---|---|
| `Authorization` | `Bearer ` prefix stripped; raw value also accepted |
| `X-Goog-Api-Key` | Gemini-style |
| `X-Api-Key` | Anthropic-style |
| `?key=` | query |
| `?auth_token=` | query |

Key set = `access.api-keys` (v8 layout) / top-level `api-keys` (legacy). It is a flat
`[]string` (`internal/config/sdk_config.go:99-100`): no per-key name, quota, model allow-list or
rate limit. If no keys are configured the provider is **unregistered**, which means the client API
is unauthenticated — a default-deny expectation would be wrong here.

Failure body for the normal path: `{"error":"<message>"}`. The realtime path nests it:
`{"error":{"message":...,"type":...,"param":...,"code":...}}`
(`internal/api/server_middleware.go:539-556`), and a bad realtime client secret returns
`invalid_realtime_client_secret`.

### 4.3 Path math per client — the table that prevents 404s

CPA registers exact prefixes. Each client appends its own suffix, so the base URL must be chosen to
complement it:

| Client | Env / config key | What the client appends | Base URL to use |
|---|---|---|---|
| Claude Code | `ANTHROPIC_BASE_URL` | `/v1/messages` | `http://127.0.0.1:8317` (no `/v1`) |
| Codex CLI | `model_providers.<id>.base_url` | `/responses` | `http://127.0.0.1:8317/v1` |
| OpenAI SDK | `base_url` | `/chat/completions` | `http://127.0.0.1:8317/v1` |
| Anthropic SDK | `base_url` | `/v1/messages` | `http://127.0.0.1:8317` |
| Gemini SDK / genai | `http_options.base_url` | `/v1beta/models/...` | `http://127.0.0.1:8317` |
| Gemini CLI | `GOOGLE_GEMINI_BASE_URL` | `/v1beta/...` (expected — **UNVERIFIED** by execution) | `http://127.0.0.1:8317` |
| Antigravity CLI | `GOOGLE_GEMINI_BASE_URL` | unknown — **UNVERIFIED** | `http://127.0.0.1:8317` |
| OpenCode (OpenAI-compat) | `providers.<id>.settings.baseURL` | `/chat/completions` or `/responses` per `package` | `http://127.0.0.1:8317/v1` |
| OpenCode (Anthropic-compat) | same | `/v1/messages` | `http://127.0.0.1:8317` |
| Grok shell | `base_url` | Grok-specific | `http://127.0.0.1:8317/v1` |

The asymmetry (Anthropic omits `/v1`, OpenAI includes it) is the single most common misconfiguration,
because both are "the base URL" in their respective docs.

---

## 5. Usage, stats, quota, "rate limits"

### 5.1 Timeline

| Version | Change | Evidence |
|---|---|---|
| ≤ 6.9.x | in-memory `GET /usage` statistics + export/import | `internal/api/handlers/management/usage.go` at tag `v6.9.47` |
| **6.10.0** | usage tracking/logging **removed**: `GET /usage`, `GET /usage/export`, `POST /usage/import`, `internal/usage/logger_plugin.go`, TUI usage tab | commit `18bb9c31` "chore: remove usage tracking and logging functionality" |
| 6.10.x | `GET /usage-queue` added/renamed | commit `da6c599e`, present from `v6.10.8` |
| 8.0.0 | `/v8/management` + v8 config layout | commit `31f4cfab` |
| fork only | `/v8/management/performance` | commit `92fc192b` |

If h4rn needs historical usage from a CPA deployment, the version decides the whole strategy:
`/usage` on ≥ 6.10 returns **404** (route does not exist), and the data simply is not recorded.

### 5.2 Pre-6.10 `/usage` (for reading old deployments)

`GET /usage` → `{usage: StatisticsSnapshot, failed_requests}`.
`GET /usage/export` → `{version:1, exported_at, usage}`.
`POST /usage/import` accepts `{version, usage}` with version 0 or 1 →
`{added, skipped, total_requests, failed_requests}`.

`StatisticsSnapshot`: `total_requests`, `success_count`, `failure_count`, `total_tokens`, `apis`,
`requests_by_day`, `requests_by_hour`, `tokens_by_day`, `tokens_by_hour`.

```
APISnapshot    { total_requests, total_tokens, models: {<model>: ModelSnapshot} }
ModelSnapshot  { total_requests, total_tokens, details: [RequestDetail] }
RequestDetail  { timestamp, latency_ms, source, auth_index, tokens, failed }
TokenStats     { input_tokens, output_tokens, reasoning_tokens, cached_tokens, total_tokens }
```

Even then it was **process-local**: restart wiped it. `RequestDetail` bucketed by day/hour, not a
queryable time series.

### 5.3 Current usage surfaces

**(a) `GET /api-key-usage`** — `internal/api/handlers/management/api_key_usage.go`

Returns the map directly, no wrapper:

```json
{ "claude": { "https://api.anthropic.com|sk-ant-...":
              { "success": 12, "failed": 1,
                "recent_requests": [ {"time":"12:00-12:10","success":3,"failed":0}, ... ] } },
  "openai-compatibility": { "|sk-...": { ... } } }
```

Key is `"<base_url>|<api_key>"`; provider is lowercased and replaced by `compat_name` when present.
Only credentials whose `AccountInfo()` kind is `api_key` appear — OAuth credentials are excluded
(use `/auth-files` for those). The `api_key` component is the **plaintext credential**, so this
endpoint leaks keys to anyone holding the management key.

`recent_requests` is 20 buckets × 600 s = **200 minutes**
(`sdk/cliproxy/auth/types.go:323-324`), label `HH:MM-HH:MM` formatted in the **server's local
time** (`types.go:442-446`), fields `time`, `success`, `failed`. Buckets are aligned to absolute
10-minute epochs and the array always has 20 entries, so empty buckets read as zeros.
Counters reset on restart.

**(b) `GET /usage-queue?count=N`** — `internal/api/handlers/management/usage.go`

Returns a **bare JSON array** of raw usage records. `count` defaults to 1; non-positive or
non-integer → 400 `{"error":"count must be a positive integer"}`.

Two behaviours that break naive consumers:

* **Destructive.** It calls `redisqueue.PopOldest(count)` — reading consumes.
* **Fan-out, not fan-in.** `Enqueue` publishes to subscribers first and only queues when there are
  none (`internal/redisqueue/queue.go:60-64,141-160`). If any plugin has called `SubscribeUsage`,
  `/usage-queue` stays permanently empty. An empty array therefore does **not** imply "no traffic".

Gated by two flags: the queue must be enabled (management secret present or `home.enabled`,
`server.go:259`) and `usage-statistics-enabled` must be true
(`internal/redisqueue/usage_toggle.go`). Retention is
`observability.usage.redis-usage-queue-retention-seconds`, default 60, clamped to 1..3600
(`internal/config/parse.go:30,97-101`).

**Record schema** — `internal/redisqueue/plugin.go:213-301`, verified field by field:

| Group | Fields |
|---|---|
| identity | `provider`, `executor_type`, `model`, `alias`, `endpoint`, `auth_type`, `auth_index`, `access_provider?`, `is_native_key` |
| correlation | `request_id`, `execution_id?`, `trace_id?`, `session_id?`, `parent_session_id?`, `node_kind?`, `is_fork?`, `is_compaction?`, `home_selection?` |
| client | `api_key` (**plaintext client key**), `access_token_sha256?`, `client_ip`, `resolved_client_ip`, `x_forwarded_for`, `user_agent` |
| timing | `timestamp`, `latency_ms`, `ttft_ms` |
| request shape | `reasoning_effort`, `service_tier`, `effective_service_tier?`, `response_service_tier?`, `upstream_service_tier?`, `response_model?`, `speed?`, `inference_geo?` |
| accounting | `accounting_version`, `token_breakdown`, `tokens`, `cache_details?`, `cost_details?`, `web_search_requests?`, `image_count?`, `requested_image_details?`, `image_details?`, `media_counts?`, `tool_usage_details?`, `iterations?` |
| outcome | `failed`, `generate`, `stream`, `fail{status_code, body}`, `response_headers?` |

`tokens` block: `input_tokens`, `output_tokens`, `reasoning_tokens`, `cached_tokens`,
`cache_read_tokens`, `cache_read_tokens_present`, `cache_creation_tokens`, optional
`cache_creation_{5m,30m,1h,12h}_tokens`, optional `text/image/audio/video/file_input_tokens`,
`cached_{text,image,audio}_tokens`, `text/image/audio_output_tokens`,
`accepted/rejected_prediction_tokens`, `total_tokens`.

`fail.body` is the upstream error body verbatim — useful, but it can contain prompt fragments.

### 5.4 Per-credential counters in `/auth-files`

`internal/api/handlers/management/auth_files.go:654-760`, response struct at `:221`.

List response: `{observed_at, files:[...]}`; with `page`/`page_size` it adds `total`, `page`,
`page_size`, `has_more`.

Per-entry fields:

| Group | Fields |
|---|---|
| identity | `id`, `auth_index`, `name`, `type`, `provider`, `label`, `source`, `path`, `size` |
| state | `status`, `status_message`, `disabled`, `unavailable`, `runtime_only` |
| counters | `success`, `failed`, `recent_requests` |
| quota | `quota`, optional `model_quotas`, `supports_quota`, `quota_provider`, `quota_plugin_id` (v8), `quota_probe` (v0) |
| identity detail | `email`, `project_id`, `account_type`, `account`, `created_at`, `modtime`, `updated_at`, `last_refresh`, `next_retry_after`, `priority`, `cooldowns`, `id_token` |

`quota` is `{observed_at, signals}` — signal keys are provider/plugin-defined, so treat it as opaque
(**UNVERIFIED** shape per provider).

`cooldowns[]` is `CooldownView` (`sdk/cliproxy/auth/cooldown_view.go`):

```json
{ "scope": "model", "model_key": "claude-sonnet-5-5", "reason": "...",
  "retry_at": "...", "remaining_seconds": 42, "backoff_level": 2, "http_status": 429 }
```

This is the closest thing to a rate-limit view in the product.

### 5.5 Cooldowns are the "rate limit"

There is no request-rate limiter. What exists:

* `routing.retry.request-retry` — extra credential retry rounds (0 disables).
* `routing.retry.max-retry-credentials`, `routing.retry.max-retry-interval` (seconds cap on waits).
* `routing.cooldown.disable-cooling`, `save-cooldown-status`,
  `transient-error-cooldown-seconds`.
* Per-credential `request-retry` override on most credential types
  (`config_types.go:780-782` etc.), where `nil`/negative means "global", `0` means "no extra rounds".

So CPA reacts to upstream 429s by cooling a credential down and failing over; it never throttles a
client. If h4rn needs client-side fairness, it must implement it itself.

### 5.6 Quota endpoints

| Endpoint | Semantics |
|---|---|
| `GET /quota/providers` | which plugins can probe quota, and whether they can reset |
| `POST /quota/fetch` | on-demand probe → quota payload |
| `POST /quota/reset` | plugin-side reset (e.g. clear an exhausted window) |
| `POST /reset-quota` | core reset of the credential's local quota state → `{status:"ok", auth_index, models}` |

`/reset-quota` errors: 400 `auth_index is required`, 400 `invalid request body`, 404 `auth not found`,
500 `failed to reset quota: ...`.
`/quota/fetch` errors: 400 `auth_index is required`, 404 `auth not found`,
501 `no quota provider available for credential`, 502 `failed to fetch quota: ...` /
`quota probe failed: ...`.
`/quota/reset` → 404 `quota provider not found for plugin` when the plugin target is missing.

Note `POST /reset-quota` (local state) and `POST /quota/reset` (provider-side) are different
operations with confusingly similar names.

### 5.7 Negative findings (important)

| Assumption | Reality |
|---|---|
| Usage history is queryable | No — removed in 6.10.0; only a 60 s (max 1 h) in-memory ring plus in-memory counters |
| `/usage-queue` is a reliable feed | No — destructive `PopOldest`, and empty whenever a plugin is subscribed |
| Per-key quotas exist | No — `access.api-keys` is `[]string` |
| Per-key rate limits exist | No |
| Counters survive restart | No, except cooldowns when `save-cooldown-status` is on |
| `/api-key-usage` covers OAuth credentials | No — `api_key` accounts only |
| Reading usage is side-effect free | No — `/usage-queue` consumes |

---

## 6. Feature detection without hardcoding

Ranked by cost. Every probe below is a read.

### 6.1 Probe matrix

| Probe | Meaning | Discriminator |
|---|---|---|
| `HEAD /healthz` | server up, any version | 200 `{"status":"ok"}` |
| `GET /v0/management/debug` no key | management routes registered | 401 + `X-CPA-VERSION` ⇒ yes; 404 ⇒ disabled |
| `X-CPA-VERSION` header | exact version string | parse semver |
| `X-CPA-COMMIT`, `X-CPA-BUILD-DATE` | fork/vendor build identity | non-empty; fork builds differ from upstream tags |
| `X-CPA-SUPPORT-PLUGIN` | `1` = CGO plugin host, `0` = not | header value |
| authenticated `GET /v0/management/usage` | pre-6.10 usage API | 200 ⇒ ≤ 6.9.x; 404 ⇒ ≥ 6.10.0 |
| authenticated `GET /v0/management/usage-queue` | queue API | 200 ⇒ ≥ 6.10.8 |
| authenticated `GET /v8/management/config` | v8 layout | 200 + `ETag` ⇒ ≥ 8.0.0 |
| authenticated `GET /v8/management/performance` | fork throughput | 200 ⇒ fork ≥ 8.0.47 |
| `POST /v8/management/credentials/verify` | upstream-only v8 route | 404 ⇒ fork |
| `GET /v1/models` (no special headers) | OpenAI shape; fork adds `max_output_length`, `reasoning_parameters` | per-entry field presence |
| `GET /v1/models?client_version=x` with `Anthropic-Version` set | `client_version` short-circuits header dispatch and always returns the OpenAI list | stays OpenAI shape ⇒ branch present |
| `GET /v1/models` with `Anthropic-Version` | Claude shape | `data[].type == "model"` |
| `GET /v1beta/models` | Gemini shape | `models[].name` with `models/` prefix |
| `GET /v0/management/auth-files` | quota/cooldown capability | presence of `supports_quota`, `cooldowns`, `quota_provider` |
| `GET /v0/management/quota/providers` | any quota plugin | non-empty `providers[]` |
| `GET /v0/management/plugins` | plugin host | `plugins_enabled`, non-empty `plugins[]` |
| `GET /v0/management/model-definitions/claude` | static catalogue channels | 200 vs 400 `unknown channel` |

404-based rows are only meaningful **after** establishing that management routes are live (401
without a key). Otherwise `disabled` and `absent route` are indistinguishable — both 404.

### 6.2 Recommended sequence

```
probe():
  r = HEAD /healthz                      # cheap liveness
  if not r.ok: return DOWN

  r = GET  /v0/management/debug          # no Authorization header
  if r.status == 404:  mgmt = DISABLED                 # routes absent
  elif r.status in (401, 403):
       version = r.headers['X-CPA-VERSION']            # free
       commit  = r.headers['X-CPA-COMMIT']
       plugin  = r.headers['X-CPA-SUPPORT-PLUGIN'] == '1'
       mgmt    = NEEDS_KEY
  else: mgmt = OPEN                                    # misconfig; treat as untrusted

  if mgmt == NEEDS_KEY and we have a key:
       r = GET /v0/management/usage-queue
       if r.ok: usage_queue = True                     # >= 6.10.8
       else if r.status == 404: usage_queue = False

       r = GET /v8/management/config
       if r.ok and r.headers.get('ETag'): v8_layout = True

       r = GET /v8/management/performance
       if r.ok: fork = True                            # throughput API
```

Never infer features from the version string alone: the fork is `v8.0.47`-based and *behind*
upstream `v8.0.69`, so "newer version ⇒ more features" is false in practice, and vendors may
rebuild without changing `X-CPA-VERSION`.

### 6.3 Version parsing caveats

* `X-CPA-VERSION` is `buildinfo.Version`, typically `vMAJOR.MINOR.PATCH`; a `-dev`/`-dirty` suffix
  may appear on non-release builds. Treat unparsable as "unknown", not as oldest.
* `X-CPA-SUPPORT-PLUGIN` reflects **build tags**, not config, so it is stable per binary.
* `X-CPA-VERSION` is only trustworthy once you have seen it on a real 401 — it is not emitted by
  `/healthz`, `/`, or the OAuth callbacks.

---

## 7. Per-harness recipes

All snippets are from official docs; CPA's own agent-client pages are at
`https://help.router-for.me/agent-client/`.

### 7.1 Claude Code

Auth: `ANTHROPIC_AUTH_TOKEN` → `Authorization: Bearer`, or `ANTHROPIC_API_KEY` → `X-Api-Key`.
CPA accepts both. Base URL must **omit** `/v1`.

`~/.claude/settings.json`:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:8317",
    "ANTHROPIC_AUTH_TOKEN": "sk-your-cpa-key",
    "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY": "1",
    "CLAUDE_CODE_GATEWAY_MODEL_DISCOVERY_TIMEOUT_MS": "3000"
  }
}
```

Env-only equivalent:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8317
export ANTHROPIC_AUTH_TOKEN=sk-your-cpa-key
export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
claude
```

Notes, all from <https://code.claude.com/docs/en/env-vars>:

* `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` populates `/model` from the gateway's
  `/v1/models` — this is why CPA's Claude-shaped list matters.
* Discovery timeout defaults to 3000 ms; raise it for a slow proxy.
* `ANTHROPIC_CUSTOM_HEADERS` exists for extra headers; unnecessary against CPA.
* Claude Code sends a `claude-cli` User-Agent, which is exactly the signal CPA uses to pick the
  Claude model-list format (`server_routes.go:634-638`) — the two are intentionally paired.

### 7.2 Codex CLI

`~/.codex/config.toml`:

```toml
model = "gpt-5.1"
model_provider = "cliproxyapi"

[model_providers.cliproxyapi]
name = "CLIProxyAPI"
base_url = "http://127.0.0.1:8317/v1"
wire_api = "responses"
requires_openai_auth = true
http_headers = { "X-Api-Key" = "sk-your-cpa-key" }
```

Or with an env-held key:

```toml
[model_providers.cliproxyapi]
base_url = "http://127.0.0.1:8317/v1"
env_key = "CPA_API_KEY"
wire_api = "responses"
```

From <https://developers.openai.com/codex/config-reference>:

* `wire_api` only accepts `responses`; it is the default.
* `base_url` ends at `/v1` because Codex appends `/responses`.
* `requires_openai_auth` keeps Codex's auth bookkeeping happy with a non-OpenAI host.
* `http_headers` for static headers, `env_http_headers` for env-derived ones.
* `supports_websockets`, `supports_standalone_web_search` exist for realtime/alpha search — CPA
  exposes both (`GET /v1/responses` websocket, `POST /backend-api/codex/alpha/search`).
* A project-level `.codex/config.toml` **cannot** override `model_providers` or `openai_base_url`
  — this belongs in the user config.

Alternative: point `chatgpt_base_url` at CPA's Codex aliases so no provider block is needed —
`/backend-api/codex/responses` exists precisely for that (**UNVERIFIED** as a supported Codex
setting name; it is a CPA-side affordance).

### 7.3 OpenCode

**v2 config** (`opencode.jsonc`) — key is `providers`, runtime field is `package`
(<https://opencode.ai/docs/providers/>):

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "model": "cpa/gpt-5.1",
  "providers": {
    "cpa": {
      "name": "CLIProxyAPI",
      "env": ["CPA_API_KEY"],
      "package": "@opencode/ai/providers/openai-compatible",
      "settings": { "baseURL": "http://127.0.0.1:8317/v1" },
      "models": {
        "gpt-5.1": {},
        "gpt-5.1-codex": { "modelID": "gpt-5.1-codex" }
      }
    }
  }
}
```

Responses-API variant (needed for Codex-family models):

```jsonc
{
  "providers": {
    "cpa": {
      "env": ["CPA_API_KEY"],
      "package": "@opencode/ai/providers/openai-compatible/responses",
      "settings": { "baseURL": "http://127.0.0.1:8317/v1" },
      "models": { "gpt-5.1": {} }
    }
  }
}
```

Anthropic-compatible variant — base URL **omits** `/v1`:

```jsonc
{
  "providers": {
    "cpa-anthropic": {
      "env": ["CPA_API_KEY"],
      "package": "@opencode/ai/providers/anthropic-compatible",
      "settings": { "baseURL": "http://127.0.0.1:8317" },
      "models": { "claude-sonnet-5-5": {} }
    }
  }
}
```

Documented facts that matter here: the `providers` key becomes the model prefix
(`cpa/...`); custom providers must list every model explicitly — no catalog fallback; `modelID`
changes the ID sent upstream while the map key is the local ID; `headers` and `body` are
provider- or model-scoped; `settings.headerTimeout`/`chunkTimeout` (ms, or `false`) cover slow
first tokens — worth setting for OAuth-backed upstreams that occasionally stall.

**Legacy (1.x) form** — still common in the wild and what CPA's own page shows:

```jsonc
{
  "provider": {
    "openai": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "CLIProxyAPI",
      "options": { "baseURL": "http://127.0.0.1:8317/v1" },
      "models": { "gpt-5.1": { "name": "GPT-5.1" } }
    }
  }
}
```

Detecting which form the running version wants is **UNVERIFIED** from source; assume v2 unless the
user is pinned to 1.x.

### 7.4 Antigravity

**CLI (`agy`)** — `~/.gemini/antigravity-cli/settings.json` must set
`"modelProvider": "gemini"`, otherwise API-key auth is not selected:

```json
{ "modelProvider": "gemini" }
```

```bash
export GEMINI_API_KEY=sk-your-cpa-key
export GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:8317
agy
```

From <https://antigravity.google/docs/install>: `GEMINI_API_KEY` is the only accepted key variable
(a `.env` file and `GOOGLE_API_KEY` are ignored), and `GOOGLE_GEMINI_BASE_URL` is the documented
custom-endpoint override. **The path `agy` appends to that base URL is UNVERIFIED** — most likely
`/v1beta` (Gemini-compatible), but confirm with a proxy log before relying on it. If it appends
`/v1beta`, the base URL above is correct; if it appends something else, CPA will 404.

**IDE** — documents `auth.type: "gemini-api-key"` but **no base-URL override**. Routing the IDE
through CPA is **UNVERIFIED** and probably not possible without a hosts/proxy shim.

**SDK** — `LocalOpenAIAgentConfig(model=..., base_url=...)`
(<https://antigravity.google/docs/sdk/local-models>) exposes `base_url` but **no API-key field**;
whether it can authenticate to a key-protected CPA endpoint is **UNVERIFIED**. Against an
unprotected CPA listener it would work as an OpenAI-compatible client.

### 7.5 Gemini CLI

```bash
export GEMINI_API_KEY=sk-your-cpa-key
export GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:8317
export GEMINI_MODEL=gemini-3-pro
gemini
```

From `docs/reference/configuration.md` in `google-gemini/gemini-cli`:

* `GOOGLE_GEMINI_BASE_URL` overrides the Code Assist/Gemini API base.
* HTTPS is required unless the host is `localhost`, `127.0.0.1` or `[::1]` — so plain HTTP to
  `127.0.0.1` is allowed and anything else needs TLS.
* `GOOGLE_VERTEX_BASE_URL` is the vertex-ai equivalent.
* Settings may also live in `~/.gemini/settings.json`.

The appended path is `/v1beta` in the Gemini SDK convention, and CPA registers exactly `/v1beta`
(§4.1) — but **not** `/v1beta1`. Path agreement here is **UNVERIFIED by execution**; a 404 on
`/v1beta1/...` is the symptom if this assumption is wrong.

### 7.6 Generic SDKs

```python
# OpenAI (chat completions)
from openai import OpenAI
c = OpenAI(base_url="http://127.0.0.1:8317/v1", api_key="sk-your-cpa-key")

# OpenAI (responses)
c.responses.create(model="gpt-5.1", input="hi")

# Anthropic — note: no /v1 in base_url
from anthropic import Anthropic
a = Anthropic(base_url="http://127.0.0.1:8317", api_key="sk-your-cpa-key")

# Gemini
from google import genai
g = genai.Client(api_key="sk-your-cpa-key",
                 http_options={"base_url": "http://127.0.0.1:8317"})
```

CPA accepts `Authorization`, `X-Api-Key`, `X-Goog-Api-Key`, `?key=` and `?auth_token=`
(§4.2), so every SDK's native header works without shims.

### 7.7 What h4rn should model

| CPA concept | Recommendation for h4rn |
|---|---|
| `access.api-keys` as `[]string` | keep the flat model for compatibility, but add per-key `{id, label, rpm, budget}` in h4rn's own schema — CPA users demonstrably want it |
| credential cooldowns with `scope`/`model_key`/`backoff_level` | copy verbatim; model-level cooldown is the useful granularity |
| three-format `/v1/models` dispatch on headers | copy; it is what makes one endpoint serve Claude Code, Codex and SDKs |
| `/usage-queue` destructive pop + subscriber fan-out | **do not** copy: split into a durable append-only log and a separate cursor-based read |
| usage stats in memory | **do not** copy: persist, or accept that every dashboard resets |
| management 404 when unconfigured | copy the stance; it removes an entire class of unauth endpoints |
| `X-CPA-*` headers on 401 | copy; it makes feature detection free and version-agnostic |
| `/v0` + `/v8` config layouts side by side | avoid; it doubles the surface. Version the config with one migration path |

---

## 8. Open items / UNVERIFIED register

| Item | Status |
|---|---|
| Path appended by Antigravity CLI to `GOOGLE_GEMINI_BASE_URL` | **UNVERIFIED** (likely `/v1beta`) |
| Antigravity IDE → CPA routing | **UNVERIFIED**, probably unsupported |
| Antigravity SDK authentication against a key-protected CPA | **UNVERIFIED** |
| Exact `/v1beta` suffix for Gemini CLI | read from docs + CPA route shape; **UNVERIFIED by execution** |
| OpenCode version that switches `provider` → `providers` | **UNVERIFIED** from source |
| `chatgpt_base_url` as a supported Codex setting for `/backend-api/codex` | **UNVERIFIED** |
| Fork `/v1/models` enrichment on *every* entry | **UNVERIFIED** (registry-dependent) |
| Per-provider `quota.signals` keys | **UNVERIFIED**, plugin-defined |
| Whether `X-CPA-VERSION` ever carries a `-dirty` suffix in released builds | **UNVERIFIED** |
| Local-password-only management auth (403 despite routes) | **verified as a code path**, but not exercised against a running binary |
