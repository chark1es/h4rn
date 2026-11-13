# h4rn — research synthesis and build verdicts

This is the architecture-level synthesis of five deep-dive documents. It exists so a
reader can decide *what to build* in ten minutes and then follow a link for the *why*.

| Detail document | Scope | Lines |
|---|---|---|
| [research/t3code.md](research/t3code.md) | T3 Code (`pingdotgg/t3code` @ `8a1811e`): harness adapters, lifecycle, MCP injection, terminal, attachments, relay, Tailscale | 737 |
| [research/cliproxyapi.md](research/cliproxyapi.md) | CLIProxyAPI upstream `v8.0.69` + `chark1es` fork `v8.0.47-46`: surfaces, feature detection, per-harness recipes, metering truth | 1,117 |
| [research/orbitid.md](research/orbitid.md) | OrbitID as OIDC provider, TS build vs uncommitted Rust rewrite | 579 |
| [research/ui-libs.md](research/ui-libs.md) | `thinking-orbs@0.3.2`, `blobatar@2.7.0`, agent-state and identity mapping | 485 |
| [research/antigravity.md](research/antigravity.md) | `agy` 1.5.7: ACP, stream-json, one-shot JSON, MCP, auth, T3's integration | 503 |

**Verdict legend.** `COPY` = take the design and the constants essentially verbatim.
`ADAPT` = take the design, replace the implementation or the constraints.
`REJECT` = do not build this, and record why so it is not re-litigated.

**Method note.** The detail documents mark claims `verified` (read in source and
cross-checked, or executed) versus `read`/`UNVERIFIED`; verdicts here inherit that
confidence, and every verdict is a statement about a reference, not about h4rn's code.

Decisions that follow from these verdicts are numbered in [decisions.md](decisions.md).

---

## 0. Cross-cutting summary

| Reference | Overall | The one thing worth taking | The one thing to refuse |
|---|---|---|---|
| T3 Code | **ADAPT heavily** | Event-sourced orchestration with a durable effect outbox | Its two parallel harness layers and its Electron/Effect dependency graph |
| CLIProxyAPI | **ADAPT as an upstream, COPY its detection stance** | 401-header feature detection and per-harness base-URL recipes | Its in-memory usage model and the `/v0` + `/v8` dual config surface |
| Thinking Orbs | **COPY** | 2D-canvas orbs with real offscreen pause and reduced-motion frames | The `gravity` layer |
| blobatar | **COPY** | Key-addressed deterministic SVG avatars, 5.9 KB, no ids or defs | `palette` overrides that void the contrast guarantee |
| OrbitID | **COPY the grant, code around the quirks** | Authorization code + S256 PKCE + `offline_access`, discovered from config | Assuming a hosted instance, `form_post`, or a stable refresh token |
| Antigravity | **COPY ACP-first** | Client-owned `session/request_permission` makes approval a real state | `--continue`, `protocolVersion` literals, anything built on the remote-control daemon |

---

## 1. T3 Code

Source: [research/t3code.md](research/t3code.md). Read at commit `8a1811e`, server `0.1.9`.

The structural fact that governs every verdict: T3 Code runs **Electron 44 + Node +
TypeScript + Effect** with **one process per environment**, harness access through vendor
**TypeScript SDKs**, a **libghostty-vt WASM + Canvas 2D** terminal, and **no application-level
encryption** on its relay. h4rn is Rust + Tauri + a per-host daemon. So its *semantics* are
the asset and its *implementations* are mostly liabilities.

| Item | Verdict | Reason |
|---|---|---|
| Per-harness process/protocol matrix (§1.1) | COPY | The matrix itself is the knowledge: one row per harness is exactly h4rn's `HarnessSpec` shape. Re-implement the protocols in Rust; keep the table as data. |
| Process-group watchdog + inherited fd-3 liveness pipe (§1.2) | COPY | ~20 lines of `/bin/sh` that kills the agent's whole process group when the daemon dies. Eliminates the entire orphaned-agent bug class. |
| Claude env hygiene: `CLAUDE_CONFIG_DIR` not `HOME`, delete `CLAUDECODE`/`CLAUDE_CODE_SESSION_ID`/`ELECTRON_RUN_AS_NODE` | COPY | Preserves the user's macOS Keychain login while stopping a nested Claude from believing it is inside the user's terminal. |
| Claude driven through `@anthropic-ai/claude-agent-sdk` | REJECT | No Rust SDK exists. Drive `claude -p --input-format stream-json --output-format stream-json --verbose` directly, long-lived, with `--resume`. |
| Codex `app-server` JSONL, one shared process per provider instance, per-thread `config` | ADAPT | Correct model. Hand-write the JSONL framing in Rust (128 MiB line cap, 32-message buffer, explicit `respond`/`respondError`). |
| OpenCode `opencode serve` + Basic auth + `/health` version gate | ADAPT | Same shape; Rust HTTP client, `MINIMUM_OPENCODE_VERSION` gate, and the external-server mode that deliberately skips MCP/attachment injection. |
| ACP family incl. ACP `terminal/*` client bridge | ADAPT | ACP is h4rn's primary path for Antigravity and the fallback for anything ACP-registered. Implement the client half (permissions, fs, terminal) once. |
| Two generations of harness code (V1 provider drivers + V2 adapters) | REJECT | Pure legacy tax, and the 13k-line Claude / 8.5k-line Codex single-file adapters come with it. One trait, one adapter per harness, split into transport/normaliser/policy/projection. |
| Lifecycle: Working / Active / Snoozed / Settled (§2) | ADAPT | Copy the predicate set exactly, then insert h4rn's **Done** tab between Working and Settled (ADR-004). |
| Settlement algorithm incl. PR-link outcome, user-authored-only clock, 2-min queued-turn hold (§2.3) | COPY | Already paid for the hard cases: clock skew, background work, subagent children, user override winning races. |
| `settledOverride: null \| "settled" \| "active"` (enum, not bool) | COPY | An enum is the only way "un-settled by hand" and "settle by default" can coexist without a second flag. |
| Snooze + hand-raise wake rules + DST-safe presets (§2.2) | COPY | Run completion deliberately does *not* wake a snoozed thread; approvals/questions/failures do. DST arithmetic is non-trivial and already solved. |
| 2 s working-exit grace, 60 s stale-exit bypass, placement-change suppression | COPY | Without the grace an orchestrator turn flaps out of Working for one frame; without the 60 s bypass a reconnecting client lies. |
| One streamable-HTTP MCP endpoint (`/mcp`) + per-session bearer (§3) | COPY | One hub per daemon is the whole design; harnesses differ only in *delivery*. |
| Bearer stored as SHA-256, liveness-bounded, revoked on session release | COPY | Token never recoverable from storage; 24 h window measured on max(wall, monotonic) so clock changes cannot extend it. |
| Token never in argv; Claude gets MCP over stdin control request | ADAPT | The *rule* is mandatory. Without the Agent SDK, deliver via a 0600 tempfile `--mcp-config` + `--strict-mcp-config` (ADR-006). |
| Codex double registration (`t3-code` + `t3-code-direct` for `thread_wait`) | ADAPT | Codex-specific workaround for Code Mode nesting. Port it only after `spawn_subagent`-style blocking tools exist. |
| ACP stdio MCP bridge that re-invokes the server as an MCP child | ADAPT | Needed when the agent only speaks stdio MCP; implement as a thin `h4rn acp-mcp-bridge` subcommand. |
| Pi generated-extension injection (§3.5) | REJECT | Pi is not an h4rn harness. Do not carry a bespoke extension-file mechanism for a provider we do not drive. |
| Event store + **durable effect outbox** + idempotent command receipts (§7.1) | COPY | The single most valuable idea in the repo. A restart must resume the turn, not lose it; duplicate `commandId` returns the stored sequence. |
| Snapshot + `afterSequence` resume with bounded replay (128 events), snapshot on gap/prune | COPY | Copy the *policy* (10 user turns / 75 items / 1 MiB snapshot) not just the mechanism. |
| `AppThread → Run → root ExecutionNode`; only root completes a run | COPY | Fixes "a subagent finished so the thread says Done" by construction. |
| Checkpoints as git objects under a private ref (§7.1) | COPY | Temp index + `write-tree` + `commit-tree -p HEAD` + `update-ref`. Cheap, GC-able, never touches the user's index. |
| Rotating NDJSON provider diagnostics with key redaction | COPY | Makes transport-replacement replay tests possible and is the only sane way to debug vendor protocols. |
| cgroup/systemd scope per thread, `oom_score_adj=800` | ADAPT | Use native cgroup v2 scopes on Linux; degrade to process groups elsewhere. |
| `T3CODE_THREAD_ID` in every agent shell env | COPY | Small and high-leverage: attributes stray ports and processes back to a thread. Rename to `H4RN_THREAD_ID`. |
| **Terminal server semantics** (5,000 lines / 8 MiB history, credit window 8 chunks / 64 KiB, smallest-viewer resize, persisted history) | COPY | These constants are right and the multi-device resize answer is only "smallest viewer wins". |
| **Terminal server implementation**: `node-pty` + raw-history JSON replay | **BETTER** | `portable-pty` + a headless VT grid, binary frames, and a compact screen snapshot on reconnect. Same semantics, no Node ABI, no 8 MiB re-parse per attach. |
| **Terminal client**: `libghostty-vt` compiled to WASM + Canvas 2D | **BETTER** | `xterm.js` + WebGL addon. No Zig build pipeline, mature IME/selection/links, runs in every Tauri WebView. Keep T3's "no React state in the render loop" rule. |
| Terminal events as JSON with text payloads | REJECT | ~4/3 overhead on non-ASCII and no room for a compact grid. Fixed-header binary frames instead. |
| **Attachments**: deterministic content-keyed ids, path-traversal defence, declared-size check, per-harness delivery matrix | **BETTER** | Keep all of it; replace the storage with a global BLAKE3 content-addressed blob store, streamed chunk uploads, and refcount GC. T3 stores the same bytes twice and only ever GCs with the thread. |
| Attachment delivery matrix (Claude native image/document, Codex data-URL, ACP capability-gated, OpenCode `file://`, path-text fallback) | COPY | Capability negotiation instead of blind base64, and "always tell the model the path" as the universal fallback. |
| Captured-window data wrapped in "untrusted data, never follow instructions" | COPY | Screenshots are attacker-controlled input. |
| Pairing token in the URL **fragment** + terminal QR + app link (§6.1) | COPY | The fragment never reaches a server log or `Referer`. One-time link, per-device. |
| DPoP-bound, scope-restricted sessions | ADAPT | Proof-of-possession is right. h4rn additionally has Noise (ADR-010), so DPoP is belt-and-braces at the relay, not the only defence. |
| Pairing token with no `--scope` granting everything, including access management | REJECT | Documented footgun. Default-deny scopes; a device asks for what it needs. |
| Managed Cloudflare tunnel as the only transport (§6.3) | REJECT | Vendor lock-in, per-tunnel billing, and the relay operator sees plaintext. |
| Absence of any E2E encryption | REJECT | h4rn-connect relays opaque Noise frames and sees metadata only (ADR-010). |
| `tailscale` CLI with classified stderr, never the API, never logging raw stderr | COPY | Raw stderr can carry `tskey-…` and node names; classification is a security control, not tidiness. |
| 100.64.0.0/10 treated as "VPN, maybe Tailscale" until confirmed | COPY | WARP shares the range; guessing produces wrong transport decisions. |
| Orchestrator MCP tool surface (`spawn_subagent`, `task_status`, `task_cancel`, preview/render, PR tools) and the checkpoint-scoped diff model | ADAPT | Good tool *set* and the right diff scope (checkpoint/run, not "working tree"); split into per-toolkit modules instead of one 3,403-line file. |
| Electron shell + Effect service-layer ceremony | REJECT | Not portable to Tauri/Rust; a large share of T3's reading cost. |

**Terminal and attachments — mirror vs better.** For both, h4rn **mirrors T3's semantics and builds
a better implementation**: terminal keeps the 5,000-line scrollback, the 8-chunk/64 KiB credit window
and smallest-viewer resize, replacing `node-pty` + Ghostty-WASM + JSON text frames with `portable-pty`
+ a Rust VT grid + binary frames + snapshot-on-reconnect; attachments keep the id scheme, path safety,
declared-size check and delivery matrix, replacing per-message base64 files with a BLAKE3
content-addressed store, streamed chunks and refcount GC.

---

## 2. CLIProxyAPI (upstream + fork)

Sources: [research/cliproxyapi.md](research/cliproxyapi.md). Upstream `1aaf56bc` (`v8.0.69`),
fork `9b8e5995` (`v8.0.47-46`, **260 commits behind**). Upstream and fork route inventories are
identical at 144 `/v0/management` method/path pairs.

CPA is **not a harness**. It is a multi-credential reverse proxy that terminates OpenAI-,
Anthropic- and Gemini-shaped APIs on one port and maps each request onto one of N stored
credentials. For h4rn that means exactly three things: an optional upstream, a pattern
library, and a management-surface reference.

| Item | Verdict | Reason |
|---|---|---|
| CPA as a `base_url` for Claude Code, Codex, OpenCode, Antigravity | ADOPT (COPY recipes) | Cheapest possible integration: no plugin, no fork needed. Recipes and the `/v1` asymmetry are in §4.3/§7 of the detail doc. |
| Path math: Anthropic base URL **omits** `/v1`, OpenAI **includes** it | COPY as fixtures | This is the single most common misconfiguration. Encode it as a test per harness, not as prose. |
| Feature detection from `X-CPA-VERSION`/`X-CPA-COMMIT`/`X-CPA-SUPPORT-PLUGIN` on a keyless 401 | COPY | Headers are emitted *before* auth, so version, build and plugin capability are free. |
| Route probes (`/usage`, `/usage-queue`, `/v8/management/config`, `/v8/management/performance`) | COPY | Never infer from version strings: the fork is *behind* upstream, so "newer ⇒ more features" is false. |
| 404-based probing only after proving management routes are live (401 without a key) | COPY | Otherwise "disabled" and "route absent" are indistinguishable. |
| Three-format `/v1/models` dispatch on request headers | COPY pattern | One endpoint serving Claude Code, Codex and plain SDKs is what makes CPA usable; h4rn's own model picker should model the same dispatch. |
| Credential cooldowns with `scope`/`model_key`/`backoff_level`/`retry_at` | COPY | Model-level granularity is the useful unit and is the closest thing to a rate-limit view in the product. |
| Management API 404 when unconfigured (not 401) | COPY stance | Removes an entire class of unauthenticated endpoints; an absent surface cannot be exploited. |
| Two independent auth planes (client access keys vs management key) | COPY | Conflating them is the most common integration mistake; model them as separate principals. |
| `/v0` + `/v8` config layouts side by side | REJECT | Doubles the surface and creates alias tables. One versioned layout with one migration path. |
| `/usage` (removed in 6.10.0) | REJECT | Route does not exist on modern builds and was never persisted; any dashboard built on it resets. |
| `/usage-queue`: destructive `PopOldest` + publish-before-queue fan-out | REJECT | Reading has side effects, and a subscribed plugin makes it permanently empty. Split into a durable append log and a cursor read. |
| `/api-key-usage` | REJECT | Leaks plaintext credentials in the response key (`<base_url>\|<api_key>`), counts only `api_key` accounts, resets on restart, and shows at most 200 minutes. |
| Per-key `access.api-keys: []string` | ADAPT | Keep the flat list for wire compatibility; add h4rn-side `{id, label, rpm, budget}` because users demonstrably want it. |
| Fork `GET /v8/management/performance` (TPS/TTFT, 7-day / 50,000-call retention) | ADAPT | The only real throughput surface in either repo. Use when present; treat as best-effort because it is fork-only. |
| Fork `/v1/models` enrichment (`max_output_length`, `reasoning_parameters`) | ADAPT | Usable as a fork detector and for a richer model picker; **UNVERIFIED** that every entry carries it. |
| 144 `/v0/management` routes as a UI checklist | ADAPT | Good inventory of what users ask a proxy to do; do not mirror the route shape. |
| Plugin host and quota plugins (`supports_quota`, `/quota/*`) | REJECT for v1 | Large surface, plugin-defined payloads, and h4rn is not a proxy. Revisit only if h4rn ships credential routing. |
| Realtime/tunnel endpoints (`/v1/realtime*`, `/v1/tunnel`) | REJECT | No h4rn harness uses them; the tunnel hub is 503 without a hub anyway. |
| Local-password-only management boot (routes registered, auth 403 forever) | REJECT / do-not-model | A verified code path where `localPassword` alone yields 404→403. Proof that "routes exist ⇒ key usable" is a false assumption. |
| Antigravity `GOOGLE_GEMINI_BASE_URL` recipe | ADOPT with a verification gate | Documented knob, but the path `agy` appends is **UNVERIFIED** (likely `/v1beta`). Probe with a proxy log before shipping a default. |

**Fork status risk.** The fork is 260 commits behind upstream and adds exactly two things h4rn
wants (`/performance`, model enrichment). Do not build a hard dependency on either; wrap both
behind capability probes so an upstream-only CPA still works.

---

## 3. Thinking Orbs (`thinking-orbs@0.3.2`)

Source: [research/ui-libs.md](research/ui-libs.md) §2, §4.

| Item | Verdict | Reason |
|---|---|---|
| Adopt for transient agent state | COPY | 14.5 KB gzipped, zero runtime deps, plain 2D canvas. That is the safest rendering path across WebKitGTK, WKWebView and WebView2 — the exact opposite of WebGL. |
| `gravity` layer | REJECT | The only expensive path in the package (`ctx.filter` blur, `getImageData`, per-frame sprite warp) and it needs three platform-specific cursor rasters. A novelty in a control surface. |
| Offscreen/tab-hidden pause, DPR capped at 2 | COPY | Long transcripts stay cheap without a bespoke scheduler. |
| Reduced motion → static representative frame per state | COPY | h4rn would otherwise have to build this; here it is a `matchMedia` behaviour. |
| Nine-state API has no `idle`/`error`/`done` | ADAPT | Compose them from `paused`, `speed`, `color`, `opts.shape`. Ship an h4rn-owned `<AgentGlyph state>` that owns the mapping table. |
| State mapping table (§4) | ADAPT (design decision, not a fact) | Only `working`, `searching`, `solving` are unambiguous. `listening` vs `searching` and `weaving` vs `composing` are taste. |
| `size={20}` inline, `64` for a chat avatar, avoid `32` | COPY | 32 is interpolated in log space and has never had a tuning pass. |
| One orb per visible turn, not per message | COPY | Each visible orb is a canvas plus a rAF callback, even with per-instance `IntersectionObserver`. |
| Do **not** use `connecting` for transport connecting | COPY rule | A 2 s animation is the wrong affordance for a short handshake; use a disabled control. |

---

## 4. blobatar (`blobatar@2.7.0` + `@blobatar/react@2.7.0`)

Source: [research/ui-libs.md](research/ui-libs.md) §3, §5.

| Item | Verdict | Reason |
|---|---|---|
| Adopt for machine / harness / subagent / user identity | COPY | 5.9 KB, no deps, pure string→string SVG, no `<defs>`/gradients/filters/ids — N avatars cannot collide. |
| Static `<img>` data-URI default in lists | COPY | One element per entry, no live animations. Correct for a 500-row sidebar. |
| `animate="hover"` in grids, `"always"` only for a single hero avatar | COPY | Exactly one live CSS animation at a time. |
| Pin the exact version (`2.7.0`), treat a major bump as a migration | COPY rule | Major *is* the generation selector: ranges and shape thresholds are frozen per major, and a golden fixture enforces it. |
| Seed scheme with type prefixes (`machine:`, `harness:`, `subagent:`, `thread:`, `user:`) | COPY | Traits are key-addressed with strong avalanche, so two entity kinds sharing an id would silently alias. Prefix makes the namespace explicit. |
| Keep the parent id in a subagent seed | COPY | Stable across restarts, unique per parent, no global registry. |
| Lowercase ids in the daemon | COPY | Normalisation is NFC + trim + lowercase, so `SA_2` and `sa_2` are the same face. Decide once, server-side. |
| `palette` overrides | REJECT | Bypasses the 4.5:1 eye/body contrast guarantee by definition. |
| `hue`/`tone` pinning per role | ADAPT (only if role colour is designed) | Locks a whole family to one colour and removes "nobody chose their colour". If used, lock hue only and let the id drive shape. |
| `title` = human label, always | COPY | The seed is unreadable; `<title>` is what a screen reader announces. |
| `expression={thinking}` on harness avatars | ADAPT (optional, secondary) | The orb answers "what is happening", the face "who". Never run both at full amplitude in one row. |

---

## 5. OrbitID (OIDC for h4rn-connect)

Source: [research/orbitid.md](research/orbitid.md). `[TS]` = `main` TS+Hono build,
`[Rust]` = uncommitted axum+sqlx rewrite. **There is no hosted instance** — no SaaS, no
default issuer, no published image. Everything is configuration.

| Item | Verdict | Reason |
|---|---|---|
| Issuer from `OIDC_ISSUER`, discovery from `/.well-known/openid-configuration` | COPY | Nothing to hardcode. Discovery gives every endpoint, including optional ones. |
| Authorization code + S256 PKCE + `offline_access` via `openidconnect` | COPY | Works unchanged with `CoreClient` once quirks 1, 4, 5 and 12 below are handled. |
| Confidential client with `client_secret_basic` | COPY | Needed for introspection, which public clients cannot use. |
| Device authorization grant for the daemon | ADAPT | Off by default; must be enabled per instance, and confidential clients must authenticate at `/oauth/device_authorization` too. |
| `end_session_endpoint` | ADAPT (feature-detect) | Advertised only by the Rust build; `[TS]` returns 404. Logout must be optional and local-first. |
| `response_mode=form_post` | REJECT | Advertised only by Rust; `[TS]` ignores it and still 302s with the code in the query. |
| A hosted OrbitID for h4rn users | REJECT (does not exist) | Onboarding must document "bring an IdP", or h4rn ships a generic OIDC fallback (ADR-025). |

### Breaks against OrbitID

Every quirk below has a coding consequence. This is the checklist ADR-011 is written against.

1. **No hosted instance and no default issuer.** A hardcoded `IssuerUrl` is a build error in
   disguise: every deployment must supply `OIDC_ISSUER_URL`, client id/secret, scopes and
   redirect URI. Tests must bring their own compose instance.
2. **`issuer` has no trailing slash and no path prefix.** A single trailing `/` makes
   `ProviderMetadata::discover` fail with an opaque "issuer mismatch". Normalise with
   `trim_end_matches('/')` and reject prefixed issuers at config load.
3. **Discovery exists only at the exact root path in the TS build.** A prefixed issuer
   (`https://host/auth`) does not resolve there. Mount the issuer at the root.
4. **PKCE is mandatory by default in the TS build, even for confidential clients**, and only
   `S256` is accepted (`plain` refused). Always send `code_challenge` + `code_challenge_method=S256`.
5. **The Rust build defaults PKCE *off* for confidential clients**, and a challenge with no
   `code_challenge_method` is treated as `S256`. Send PKCE unconditionally so both builds work.
6. **Scopes are an allowlist on the Rust build.** `offline_access` and `organizations` fail with
   `invalid_scope` unless they were in the application's `scopes` at registration. Register with
   `openid profile email offline_access` (+ `organizations` if used) and treat a registration
   that omits them as a deployment error, not a runtime retry.
7. **The `[TS]` id_token is thin**: no `azp`, `at_hash`, `auth_time` or `sid`. Any library that
   verifies `at_hash`, or that implements `max_age`/`prompt=login` re-authentication, will fail
   or silently not re-authenticate. Verify `iss`, `aud`, `exp`, `nonce` only.
8. **`prompt=login` is ignored by the TS build** (only `none` and `consent` are interpreted).
   Do not build a "force re-auth" flow on it; use a local session clear plus a fresh authorize.
9. **`response_mode=form_post` is advertised only by the Rust build.** Never request it.
10. **There is no `end_session_endpoint` in the TS build** and `GET /oauth/end_session` is 404.
    RP-initiated logout must be behind a discovery check, with local session teardown always
    succeeding regardless.
11. **Refresh-token reuse kills the whole family in the TS build**, with no grace window. Two
    concurrent refreshes read as theft and revoke every access and refresh token. Refresh must be
    **single-flight across every process** (desktop, daemon, relay) for a given user — one
    refresher, one persisted successor.
12. **Refresh tokens rotate on every use**, tied to a `family_id`. Persist the new refresh token
    before returning; losing it is equivalent to a forced re-login. The Rust build's 30 s
    idempotent retry window is a bonus, not something to rely on.
13. **`invalid_grant` on refresh means "re-authenticate the user"**, not "retry". Surface it as a
    first-class auth state and do not loop.
14. **Introspection requires a confidential client.** Public clients get
    `401 invalid_client`. The daemon's device-grant client must therefore be confidential if it
    introspects.
15. **The device grant is off by default** and must be enabled per instance
    (`oidcProvider.deviceAuthorizationEnabled`). Discovery will simply omit
    `device_authorization_endpoint`; the UI must not offer pairing-by-code when it is absent.
16. **Confidential clients must authenticate at `/oauth/device_authorization` as well**, or the
    call returns `401 invalid_client`. This is easy to miss because the token poll *does* expect
    it.
17. **The device grant has no browser session**, so `org_id`/`org_slug`/`org_role` are absent and
    only the membership list is returned. Do not key tenancy off the active org for daemon tokens.
18. **An unregistered `redirect_uri` returns HTTP 400 JSON at `/oauth/authorize`, not a redirect.**
    The desktop flow must handle a JSON 400, not just a bad callback.
19. **Redirect matching is exact in the TS build** (the Rust build adds RFC 8252 loopback-port
    tolerance). Ship a **fixed** loopback port and register it, or the desktop login breaks
    against a TS instance.
20. **The dashboard refuses non-`http(s)` redirect URIs**, so `h4rn://callback` can only be
    registered through the Backend API (`POST /v1/oauth_applications`). Document the curl, do not
    document the UI.
21. **Two error shapes.** `/oauth/*` returns RFC 6749 `{error, error_description}`; every `/v1/*`
    route returns `{errors:[{code,message,…}], trace_id}`. Never reuse the Backend API error type
    for OAuth calls.
22. **CORS differs.** The TS build applies its credentials-enabled Frontend API policy to every
    route, so a browser on an unlisted origin cannot call `/oauth/token`; the Rust build answers
    wildcard CORS on OIDC. Any browser-based pairing flow must be same-origin or server-mediated.
23. **`aud` is a single string equal to `client_id`**, never an array; `azp` equals `client_id`
    when present. Hand-written claim structs need an untagged enum or `String`.
24. **Access tokens may be opaque.** Default `access_token_format` is `opaque` (`oat_…`), so no
    offline verification is possible; a relay that wants to verify locally must ask the operator
    to set `accessTokenFormat: "jwt"`. Even then the grant row still governs revocation, so
    signature validity alone does not mean the token is live.
25. **Rate limits return 429 with `Retry-After`** on sign-in, device lookup and device
    authorization. A test suite that signs in hundreds of times from one IP will trip it; reuse a
    session. Device user codes burn after 10 attempts per code / 20 lookups per IP per 10 min, and
    `slow_down` raises the poll interval by 5 s for that code's lifetime.
26. **Sign-in sets `__client` twice in one response** (two `Set-Cookie` headers, last valid).
    Prefer the session JWT over cookie handling.
27. **Consent and sign-in screens live on `application.homeUrl`** (default
    `http://localhost:3000`), not on the issuer. With consent enabled and no prior consent,
    `/oauth/authorize` redirects off-origin. Set `consent_screen_enabled: false` for automation,
    or point `homeUrl` at the OrbitID server.
28. **`id_token_claims` differ between builds** (`[Rust]` adds `azp`, `at_hash`, `sid`,
    `auth_time`, `locale`, `groups`). Do not make any claim mandatory except `sub`/`iss`/`aud`/`exp`.
29. **`userinfo` returns `"picture": null`** with the `profile` scope when there is no avatar,
    while the id_token omits the key. Handle both.
30. **`token_type` is `"Bearer"` with a capital B**, `scope` is a space-separated string, and
    `expires_in` is integer seconds — standard, but the surrounding `/v1/*` API is snake_case
    objects, so mixing parsers is a real hazard.
31. **JWKS is served at three paths** (`/oauth/jwks`, `/.well-known/jwks.json`, `/v1/jwks`) from
    one key table, and every stored key is returned so rotation keeps old tokens verifiable. Use
    the advertised `jwks_uri` and cache by `kid`, not by path.
32. **RS256 only.** `alg: none` and HS256 are not offered, and verification pins
    `algorithms: ["RS256"]`. Do not make the algorithm configurable.

**Live-suite findings (2026-11-12).** These were found by actually running
`scripts/test-connect.sh --orbitid` against the TS build in Docker, not inferred from source.

33. **A session JWT lives 60 seconds by default** (`session.tokenLifetimeSeconds`). Automating
    `/oauth/authorize` with a bearer session works, but if the JWT was minted before a compile,
    queue wait or retry, the next authorize call silently returns to
    `<homeUrl>/sign-in?redirect_url=…` (or `login_required` under `prompt=none`) instead of the
    application redirect. Workaround: `PATCH /v1/instance {"session":{"tokenLifetimeSeconds":3600}}`
    and mint the JWT immediately before the run. The test script pre-builds the test binary first
    and does both.
34. **Session JWTs are issuer-bound, not key-bound.** `verifySessionToken` pins
    `issuer: ctx.env.baseUrl` (`[TS]` `services/tokens.ts:276-293`). Two OrbitID instances can
    share a Postgres database and signing keys, but instance B rejects a JWT minted by instance A
    with `401 authentication_invalid`. Workaround: reuse the signed-in `__client` cookie against
    each instance's `/v1/client/sessions/:id/tokens` to mint a per-issuer JWT; do not copy a JWT
    between base URLs.
35. **A shared key table makes an unconfigured issuer look cryptographically valid.** Because both
    instances above sign with the same `signing_keys` row, an ID token minted by the alternate base
    URL verifies against the configured instance's `jwks_uri`; only the `iss` claim distinguishes
    them. Treat issuer validation as a separate, mandatory check and report it distinctly
    (`issuer_mismatch`) rather than relying on signature failure. The e2e suite builds a second
    instance on the same DB specifically to prove this rejection.

---

## 6. Antigravity (`agy` 1.5.7)

Source: [research/antigravity.md](research/antigravity.md). Everything in §3–§5 of that document
was executed locally, not inferred.

| Item | Verdict | Reason |
|---|---|---|
| ACP over stdio (`agy acp`) as the primary surface | COPY | JSON-RPC 2.0 NDJSON, session create/load/list/close, streaming updates, and **client-owned permissions**. |
| `session/request_permission` answered by h4rn | COPY | Makes "waiting for approval" a real h4rn state that a human can answer, instead of a TTY prompt nobody sees. This is the cleanest of all four harnesses. |
| `stream-json` print mode as fallback | COPY | Complete turn-shaped fallback needing no ACP client. Envelope is `{"event":"user","message":{…}}`; a missing `event` is a hard error. |
| One-shot `-p … --output-format json` for utility calls (titles, commit messages) | COPY | `conversation_id`/`status`/`response`/`usage`; failures arrive as `AGY_ERROR: {…}` on stderr with exit code 3. No prose scraping. |
| `-p ""` **last**, with all other flags first | COPY as a hardcoded spawn rule | `-p` takes an optional value and swallows the next token (`-p took "--input-format" as its prompt`); bare `-p` at the end fails. This is a verified trap. |
| `--conversation <uuid>` for resume; never `--continue` | COPY | `--continue` is per-profile and racy with more than one agent. Session not found must be a clean "start new", not a crash. |
| Read `protocolVersion` from the reply; never hardcode 1 or 2 | COPY | 1.5.7 answers `1` to a `2` request; T3's fixture from an older build says `2`. Same capability set either way. |
| `session/new` `mcpServers` + `mcpOverHttp: true` | ADAPT | Antigravity is one of the few ACP flavors that reliably accepts injected HTTP MCP; still gate on `mcpCapabilities.http`. |
| `agy mcp add` for user-visible servers; `session/new` for ephemeral ones | COPY | CLI validates; `mcp_config.json` is plain JSON read at startup, so write it atomically only when needed. |
| `--process-per-session` | COPY | Crash containment at ~one worker per session; cheap relative to a 268 MiB binary. |
| `--app-data-dir` / `--gemini-dir` per Google account | COPY | The only way to isolate two accounts without a container. |
| `--sandbox`, omit `--dangerously-skip-permissions`, enforce client-side | COPY | Mode `Auto` is the **default** in 1.5.7 headless runs, i.e. auto-approving by default. Set `--mode plan\|accept-edits` explicitly and keep permissions client-side. |
| Empty `.gemini/skills` and `.agents/skills` created in the workspace at session start | COPY the allowlist | Real bug source: a clean-tree check or workspace-diff feature trips over two empty dirs. Allow-list exactly these paths. |
| No rewind, no manual compaction over ACP | REJECT those features for AG sessions | `agy` cannot rewind a conversation, and `/compact` exists only in the CLI. The UI must not offer either for Antigravity threads. |
| `--cloud_code_endpoint <url>` as the documented gateway knob | COPY | The only first-party, documented endpoint override. `GOOGLE_GEMINI_BASE_URL`/`CLOUD_CODE_URL`/`AGY_LLM_GATEWAY_MODELS` exist but their semantics are **UNVERIFIED**. |
| Account-scoped model list from `agy models` / `session/new.availableModels` | COPY | The list includes Claude and GPT models and varies by plan. Never hardcode it. |
| Managed `agy` download (~268 MiB + ≥1.25 GiB staging) | ADAPT | Default to a manual `agy` on `PATH`; offer managed install as an explicit opt-in. |
| `agy remote-control` daemon / IDE sidecar | REJECT | Not a documented contract, more surface than an ACP child, and no h4rn need. |
| Gemini CLI as a fallback plan | REJECT | Not installed here, duplicates Google auth/quota, and `agy` is a superset for these surfaces. |
| Reusing the ACP runtime for structured JSON helpers | ADAPT with a gate | T3 only allows it when global hooks and MCP config are empty — an invisible helper must not inherit a user's global tooling. |

---

## 7. Open risks

Ordered by expected cost, with the mitigation each verdict already implies.

1. **Reverse-engineered harness protocols drift.** Claude's `stream-json`, Codex's `app-server`
   JSON-RPC and OpenCode's HTTP API are not stable public contracts; T3 pins vendor SDKs
   precisely because of this. Mitigation: one adapter per harness behind the `Harness` trait,
   feature-detect at startup, and keep the NDJSON redacted-diagnostics log so a break is
   diagnosable from a user's machine.
2. **Single-refresher is a distributed problem.** OrbitID's TS build revokes a token family on
   any refresh-token reuse, and h4rn has up to three processes holding the same user's tokens
   (desktop, daemon, relay). Mitigation: exactly one refresher per user, a persisted successor,
   single-flight across processes (not just within one), and `invalid_grant` treated as
   re-authenticate (ADR-011, ADR-016).
3. **CLIProxyAPI fork drift.** The fork is 260 commits behind and is the only source of
   `/performance` and model enrichment. Mitigation: capability probes only, self-metered usage as
   the primary source, fork surfaces as optional enrichment (ADR-015, ADR-024).
4. **Antigravity is account-gated and quota-shaped.** `SUBSCRIPTION_REQUIRED` and usage-limit
   responses are normal, not errors, and the model list is plan-dependent. Mitigation: model them
   as first-class thread states and never hardcode models or limits.
5. **Antigravity's `GOOGLE_GEMINI_BASE_URL` path is unverified** (likely `/v1beta`). A wrong
   assumption is a silent 404 through CPA. Mitigation: probe with a proxy log before shipping a
   default; keep `--cloud_code_endpoint` as the documented path.
6. **Noise E2E plus multipath is unproven in this shape.** Path switching must not drop the
   session, and pairing must bind static keys atomically or a replayed QR yields a different
   peer. Multi-device key management (one daemon, N paired devices, one revoked device) is
   under-specified by every reference — T3 has no E2E at all. Mitigation: sequence numbers plus
   acks at the session layer, explicit device-revocation events, and property tests over
   reordering/duplication.
7. **Relay metadata leakage.** Even with opaque frames the operator sees connection ids, frame
   sizes, timing and thread counts. Mitigation: fixed-size padding buckets for control frames,
   coalescing on a tick, and no thread titles in relay-visible fields (ADR-010).
8. **One WebSocket carrying control, PTY and blobs** invites head-of-line blocking. Mitigation:
   binary framing with per-stream sequence numbers, the copied 8-chunk/64 KiB credit window, and
   chunked blob uploads interleaved rather than one giant frame (ADR-001, ADR-007, ADR-008).
9. **`alacritty_terminal` is pre-1.0 and its API churns.** Mitigation: wrap it behind an
   h4rn-owned `h4rn-vt` trait, pin the exact version, and keep `vt100` as a documented fallback
   (ADR-007).
10. **Attachment refcount GC is easy to get wrong.** Concurrent uploads, retries and failed
    threads can leak blobs or collect a live one. Mitigation: two-phase upload with a pending
    segment, GC only over unreferenced blobs older than a grace window, and a leak audit metric.
11. **Settlement idle default (24 h) is shorter than T3's 7 days.** Users will be surprised by a
    thread vanishing from Done. Mitigation: per-project setting, never settle a thread with a
    pending PR watch or a queued turn, and make the auto-settle timestamp visible before it fires
    (ADR-004).
12. **WebKitGTK `@property` floor for animated blobatars.** Old Linux WebKitGTK renders the face
    but snaps poses. Mitigation: static avatars on Linux, or set a WebKitGTK ≈2.42 floor in the
    Linux bundle (ADR-014).
13. **Mobile Tauri v2 background limits.** iOS suspends WebSockets and the OS will not keep a
    daemon connection alive. Mitigation: relay-side push (APNs/FCM) as a metadata-only wake, with
    the app reconnecting and resuming by sequence number (ADR-027).
14. **`ts-rs` drift on serde-tagged enums.** Generated TS can silently disagree with the Rust
    wire format for internally/adjacently tagged unions. Mitigation: pin the serde representation
    in the protocol crate, generate in CI, fail on any diff, and keep a wire-compat test that
    deserialises golden fixtures (ADR-002).
15. **WSL and remote hosts break path assumptions.** `/mnt/c` paths do not round-trip, 9p has no
    inotify, and ports are reachable from Windows but not the other way. Mitigation: host-scoped
    path translation in the daemon, polling fallback for file watch, and port discovery attributed
    by `H4RN_THREAD_ID` (ADR-019).
16. **Self-hosted-only OrbitID is an onboarding cliff.** Every h4rn user needs an IdP before the
    relay works at all. Mitigation: generic OIDC issuers plus a documented single-tenant mode, and
    optionally local accounts for a solo deployment (ADR-025).

---

## 8. Where the decisions live

| Verdict cluster | ADRs |
|---|---|
| One protocol, Rust core / TS view | ADR-001, ADR-002, ADR-017 |
| Event sourcing, threads, lifecycle | ADR-003, ADR-004 |
| Harness adapters and MCP hub | ADR-005, ADR-006, ADR-020, ADR-021 |
| Terminal and attachments | ADR-007, ADR-008 |
| Transport, E2E, auth, secrets | ADR-009, ADR-010, ADR-011, ADR-016, ADR-018, ADR-025 |
| Storage and metering | ADR-012, ADR-024 |
| UI stack, editor, design system | ADR-013, ADR-014 |
| Proxy integration | ADR-015 |
| Hosts, checkpoints, diagnostics, push | ADR-019, ADR-022, ADR-023, ADR-027 |
| Antigravity policy | ADR-026 |

---

## 9. Phase 3 implementation findings — OrbitID breaks (2026-11-12)

These came from building the Rust OIDC client in `crates/h4rn-connect/src/oidc.rs` against
the quirks catalogued in [research/orbitid.md](research/orbitid.md). They are implementation
consequences, not new provider behaviour, and each one changed code.

1. **`openidconnect` 3.5 requires a nonce verifier for every code exchange.** The device
   grant has no browser session and OrbitID's device ID token has no `nonce` (the TS build
   only binds a nonce on the authorization-code path). `exchange_code` must use the stored
   nonce, while the device-grant ID token needs a verifier that accepts absence; signature,
   issuer, audience and expiry are still checked. Requiring a nonce there rejects every
   daemon login.
2. **The crate's default token-endpoint auth is `client_secret_basic`.** OrbitID advertises
   both basic and post. The relay pins `AuthType::RequestBody` because the device
   authorization, poll, revoke and userinfo calls are hand-written form posts; mixing basic
   for one call and post for the others produces `invalid_client` only on some endpoints.
3. **The device endpoint is absent, not merely disabled.** With
   `deviceAuthorizationEnabled=false`, the TS discovery document omits
   `device_authorization_endpoint` entirely. Configuration alone cannot enable the grant:
   the relay feature-detects the endpoint and returns a hint instead of failing at poll time.
4. **A confidential client must authenticate at `/oauth/device_authorization`, not just at
   `/oauth/token`.** RFC 8628 allows a public client there; OrbitID returns `invalid_client`
   unless `client_secret` is included. The same secret is sent on every poll.
5. **The discovery JSON contains fields the crate does not model.** Device authorization,
   revocation and end-session endpoints have to be parsed from a second, permissive raw
   struct. `openidconnect`'s validated metadata remains the source for authorization, token,
   JWKS and ID-token verification; the relay never trusts the raw copy for values it also
   validates.
6. **Device polling errors are normal control flow, not failures.** `authorization_pending`
   and `slow_down` arrive as HTTP 400 with a JSON `error`; `slow_down` may omit
   `Retry-After`. Treating either as an upstream error makes a normal approval look like an
   outage. `access_denied` and `expired_token` are terminal; unknown codes are retried with a
   bounded interval.
7. **`invalid_grant` has two meanings and must not be retried.** At the token endpoint it
   means the code or refresh token is dead (ADR-011: re-authenticate). At the device poll it
   is not used; terminal device states use `access_denied`/`expired_token`. Mapping both to a
   generic upstream error hides the difference.
8. **Issuer normalisation must happen before discovery, not after.** `ProviderMetadata`
   compares the configured issuer byte-for-byte against the document. A configured trailing
   slash or a prefixed path fails with an opaque issuer mismatch; the relay normalises and
   rejects a path prefix at config validation, with a message that names the provider.
