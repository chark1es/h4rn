# CLIProxyAPI

h4rn treats [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) ("CPA") as an optional
upstream, not as a harness. A profile stores a proxy root URL and two independent keys, and each
harness has its own toggle, so Claude Code can route through a proxy while Codex stays direct.

The integration is specified by [ADR-015](decisions.md#adr-015-cliproxyapi-is-feature-detected-never-version-detected)
and [ADR-016](decisions.md#adr-016-secrets-in-os-keychains-envelope-encrypted-in-postgres). The
research behind it, including route inventories and source citations, is in
[`docs/research/cliproxyapi.md`](research/cliproxyapi.md).

## Setup

1. Run a reachable CPA listener. The default is `http://127.0.0.1:8317`; a reverse proxy with TLS is
   fine as long as h4rn can reach `/healthz`.
2. Have two values ready:
   - **client API key** — one entry of `access.api-keys`, used by harnesses for `/v1` and `/v1beta`;
   - **management key** (optional) — `remote-management.secret-key` or `MANAGEMENT_PASSWORD`, needed
     for aliases, credential health and the optional v8/usage routes.
3. Add a profile with `AddProxyProfile`. The profile root may be entered as `https://proxy.example`,
   `.../v1` or `.../v1beta`; h4rn normalises it for probes and per-harness recipes.
4. Call `TestProxyProfile` to see capabilities and diagnostics. Nothing is inferred from a version
   string: each boolean records that a probe succeeded.
5. Enable routing per harness with `SetProxyHarnessOverride { profile_id, harness, enabled,
   default_model }`. `ListProxyModels` supplies the picker.

### Where the data lives

| Data | Location |
|---|---|
| Profile metadata (id, label, root URL, default flag, toggles) | `<data-dir>/proxy/proxy-profiles.json`, `0600` |
| Management and client keys | OS keychain, `service = dev.h4rn.proxy` |
| Keys on headless hosts | `<data-dir>/proxy/secrets/*.secret`, ChaCha20-Poly1305, `0600` sibling key |

Keys never enter the metadata file, a log line or a response. Requests that create or update a
profile carry plaintext keys **into** the daemon; every profile that leaves the daemon exposes only
`has_management_key` / `has_api_key`. The secret store falls back to the encrypted file when no
credential store is available, and a broken keychain does not brick a headless host.

### Protocol surface

| Request | Response |
|---|---|
| `ListProxyProfiles` | `ProxyProfiles { profiles }` |
| `AddProxyProfile { profile }` | `ProxyProfile { profile }` |
| `UpdateProxyProfile { profile_id, patch }` | `ProxyProfile { profile }` |
| `RemoveProxyProfile { profile_id }` | `Ack` |
| `TestProxyProfile { profile_id }` | `ProxyTested { report }` |
| `ListProxyModels { profile_id }` | `ProxyModels { catalog }` |
| `SetProxyHarnessOverride { profile_id, harness, enabled, default_model }` | `ProxyProfile { profile }` |
| `ListProxyLimits { profile_id }` | `ProxyLimits { limits }` |
| `PickProxyFailover { profile_id, current_model, policy }` | `ProxyFailover { choice }` |

Changes are also broadcast as ephemeral `ServerFrame::ProxyEvent` frames (`profiles_changed`,
`profile_tested`, `models_changed`, `limits_changed`). They are daemon-global, carry no sequence
number and are outside the durable thread log; a reconnecting client re-reads the profile list.

## Capability detection

`TestProxyProfile` runs the sequence below and caches the result for five minutes. A failed model or
limits read drops the cache, so a restarted proxy or a flipped management flag is noticed on the next
call. Writes invalidate the affected profile immediately.

```
HEAD /healthz                         -> reachable
GET  /v0/management/debug  (no key)   -> disabled | needs_key | open, plus X-CPA-* headers
GET  /v0/management/usage-queue       -> usage_queue
GET  /v0/management/usage             -> legacy_usage
GET  /v8/management/config            -> v8_config, v8_etag
GET  /v8/management/performance       -> performance
POST /v8/management/credentials/verify-> credentials_verify
GET  /v0/management/auth-files        -> cooldowns
GET  /v0/management/quota/providers   -> quota_providers
GET  /v1/models                       -> openai_models, fork_model_enrichment
GET  /v1/models  + Anthropic-Version  -> claude_models
GET  /v1beta/models                   -> gemini_models
```

A 404 only means "absent" once the keyless management probe established that management routes are
registered at all; otherwise "disabled" and "route missing" are indistinguishable. The `X-CPA-*`
headers are informational — h4rn records them but never uses them to decide a feature.

### Upstream vs. fork

| Capability | Probe | Upstream `v8.0.69` | Fork (target) |
|---|---|---|---|
| Health | `HEAD /healthz` | yes | yes |
| Management routes registered | keyless `/v0/management/debug` → 401/403 | when configured | when configured |
| Build headers | `X-CPA-VERSION`, `X-CPA-COMMIT`, `X-CPA-SUPPORT-PLUGIN` | yes | yes |
| Legacy usage | `/v0/management/usage` | 404 since 6.10 | 404 |
| Usage queue | `/v0/management/usage-queue` | yes (≥ 6.10.8) | yes |
| v8 config + ETag | `/v8/management/config` | yes (≥ 8.0.0) | yes |
| Fork performance API | `/v8/management/performance` | **no (404)** | **yes** |
| Credentials verify | `POST /v8/management/credentials/verify` | **yes** | **no (404)** |
| Credential cooldowns | `/v0/management/auth-files` | when supported | yes |
| Quota providers | `/v0/management/quota/providers` | plugin-dependent | plugin-dependent |
| OpenAI model shape | `/v1/models` without an identifying header | yes | yes |
| Claude model shape | `/v1/models` + `Anthropic-Version` | yes | yes |
| Gemini model shape | `/v1beta/models` | yes | yes |
| Forge enrichment | `max_output_length`, `reasoning_parameters` | no | **yes** (registry-dependent) |

The fork is based on an older tag than upstream, so "newer version means more features" is false in
practice. This is why the table is a list of probes rather than a version range.

## Per-harness overrides

Every recipe is applied through `SessionSpec.env_overrides`; keys ride the child environment, never
`argv`.

| Harness | Env / config | Base URL | Status |
|---|---|---|---|
| Claude Code | `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_MODEL`, `ANTHROPIC_SMALL_FAST_MODEL`, `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` | root, **no** `/v1` (Claude appends `/v1/messages`) | verified |
| Codex CLI | `H4RN_CODEX_MODEL_PROVIDER`, `H4RN_CODEX_BASE_URL`, `H4RN_CODEX_WIRE_API=responses`, `H4RN_CODEX_API_KEY_ENV=CLIPROXY_API_KEY`, plus the key under `CLIPROXY_API_KEY` | root + `/v1` (Codex appends `/responses`) | verified |
| OpenCode | `H4RN_OPENCODE_BASE_URL`, `H4RN_OPENCODE_API_KEY`, `H4RN_OPENCODE_PROVIDER`, `H4RN_OPENCODE_PACKAGE=@ai-sdk/openai-compatible`, `H4RN_OPENCODE_MODELS`; the adapter generates `OPENCODE_CONFIG_CONTENT` | root + `/v1` | verified |
| Antigravity | `GOOGLE_GEMINI_BASE_URL`, `GEMINI_API_KEY`, `GEMINI_MODEL` | root | **unverified** — the path `agy` appends is not confirmed |

Notes:

- The model the child asks for is the proxy catalogue id; a `provider/model` prefix is added only
  for OpenCode's generated config and stripped from the upstream id.
- `default_model` on the toggle is used when the thread's model is empty or literally `default`.
- The profile's default flag wins when its toggle is on; otherwise the first profile (in creation
  order) with that harness enabled is used, so different harnesses can route through different
  proxies.
- Live verification of the Claude Code recipe is an ignored test:

  ```bash
  H4RN_LIVE_CPA_BASE_URL=https://proxy.example \
  H4RN_LIVE_CPA_API_KEY=... \
    cargo test -p h4rn-proxy --test live -- --ignored --nocapture
  ```

  It detects capabilities, lists the catalogue, runs one real Claude Code turn with
  `Reply with exactly: pong`, and asserts the exact reply. Set `H4RN_LIVE_CPA_MANAGEMENT_KEY` to
  exercise management routes and `H4RN_LIVE_CPA_MODEL` to choose a model.

## Model catalogue

`/v1/models` answers three different shapes depending on request headers, so `ListProxyModels` asks
twice — once without an identifying header for OpenAI and once with `Anthropic-Version` for Claude —
and separately calls `/v1beta/models` for Gemini. Entries are merged by id and carry their source.

When a management key is available, aliases are merged from `oauth.model-alias`,
`openai-compatibility[].models` and `api-keys.<provider>[].models`. The management config is read
only for ids and display names; its secret values are never copied into the catalogue.

## Credential health and failover

CPA has no server-side per-key rate limiting. The durable signal is per-credential **cooldowns**,
optionally scoped to a model, read from `/v0/management/auth-files` and exposed as
`ListProxyLimits`. Without a management key the response is `available: false` with a note, not an
error.

`PickProxyFailover` is a pure selection over the current catalogue and cooldown snapshot:

| Strategy | Choice |
|---|---|
| `same_family_first` (default) | a model from the same vendor family that is not cooling down, else any eligible model |
| `account_first` | a model for which a non-disabled credential has no active cooldown |
| `any_model` | the first eligible model |

The `exclude` list is honoured, and a model with an active cooldown is never chosen. The selection
is advisory: CPA retries across credentials itself, and h4rn uses the choice to decide what to send
on the next turn.

## Diagnostic codes

`ProxyTestReport.diagnostics` is ordered error, warning, info. Codes include
`unreachable`, `unhealthy`, `management_disabled`, `management_key_missing`, `management_open`,
`management_key_rejected`, `client_key_rejected`, `client_route_absent`, `models_unreachable` and
`management_basic`. Messages and hints never contain a key.
