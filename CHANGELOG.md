# Changelog

All notable changes to h4rn are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses semantic versioning.

## [Unreleased]

### Phase 5 — CLIProxyAPI

- **Proxy crate** (`h4rn-proxy`): profiles, capability probes, three-shape model catalogue,
  per-harness env/config recipes, credential health and failover selection, with secrets behind a
  narrow `SecretStore` trait.
- **Profiles**: multiple profiles with one default, per-profile/per-harness toggles and an optional
  per-toggle default model. The default profile wins when its toggle is on; otherwise profiles are
  tried in creation order. Metadata is a `0600` JSON file that never contains a key.
- **Feature detection**: probes only, never version strings. `HEAD /healthz`, a keyless management
  probe for `X-CPA-*` headers, authenticated route probes (usage queue, legacy usage, v8 config with
  ETag, fork performance, credentials verify, auth files, quota providers) and header-selected model
  shapes. Capabilities and catalogues are cached for five minutes and invalidated by writes or a
  failed read.
- **Models and aliases**: `/v1/models` is read once in the OpenAI shape and once with
  `Anthropic-Version` for the Claude shape, plus `/v1beta/models`; entries merge by id with
  provenance. Management-config aliases (`oauth.model-alias`, `openai-compatibility`,
  `api-keys.<provider>`) are merged when a management key is available, without copying secrets.
- **Per-harness routing**: Claude Code (root URL, token, model discovery), Codex
  (`model_providers` with `env_key` and `wire_api`), OpenCode (generated `OPENCODE_CONFIG_CONTENT`)
  and Antigravity (marked unverified). Overrides are applied through
  `SessionSpec.env_overrides`, so keys never enter `argv`.
- **Health and failover**: credential cooldowns from `auth-files` exposed as `ListProxyLimits`;
  `PickProxyFailover` implements `same_family_first`, `account_first` and `any_model`, honouring
  exclusions and active cooldowns.
- **Protocol and daemon**: new `h4rn-protocol::proxy` wire types, nine client requests, response
  bodies, ephemeral `ProxyEvent` server frames, a keychain-or-encrypted-file `DaemonSecretStore`,
  daemon wiring and session-start override application.
- **Tests**: wiremock coverage for upstream/fork probes, 404/401/403 management and client outcomes,
  header-selected shapes, ETag, aliases and secret redaction; golden environment/config tests for
  every harness; an ignored live test that runs a real Claude Code turn through a proxy and asserts
  the reply.
- **Docs**: [`docs/cliproxyapi.md`](docs/cliproxyapi.md) covers setup, the upstream-vs-fork
  capability matrix and the per-harness override table.

### Phase 6 — forges

- **Provider-agnostic forge layer** (`h4rn-forge`): one `Forge` trait with
  GitHub (REST, plus GraphQL review threads and a REST fallback), GitLab REST
  v4, and a shared Gitea/Forgejo API v1 client. Normalised PRs, diffs, review
  threads (path/line/side/resolved), general comments, checks/pipelines and
  review state live in `h4rn-protocol::forge`.
- **Repository detection** from ssh, scp-like and https git remotes, with
  credential redaction, nested GitLab namespaces, and explicit
  `forge-hosts.json` mappings for GitHub Enterprise, self-hosted GitLab,
  Gitea/Forgejo and ssh aliases; unknown hosts are never guessed.
- **Accounts and auth**: least-privilege PATs validated before storage, GitHub
  OAuth device flow (`H4RN_GITHUB_CLIENT_ID`), and explicit ask-then-import from
  `gh auth token` / `glab auth status --show-token`. Tokens live in the OS
  keychain or the encrypted-file fallback; they never reach the wire, logs or
  audit.
- **Daemon wiring** for `ForgeListAccounts`, `AddAccount`, `RemoveAccount`,
  `DetectRepo`, `ListPrs`, `GetPr`, `GetPrDiff`, `ListPrComments`, `ListChecks`,
  `Reply`, `Comment`, `OpenPr` and `HandPrToAgent`; PR writes are recorded in
  `forge-audit.jsonl`.
- **Checks polling** with backoff (`H4RN_FORGE_POLL_SECS`) and a transient
  `forges_checks_updated` event, per-account request budgets
  (`H4RN_FORGE_RPM`), `retry-after`/`x-ratelimit-reset` handling, GitHub ETag
  revalidation and bounded pagination.
- **Agent handoff** (`pr_context_for_agent`): unresolved threads, failing
  checks and diff stats rendered as compact markdown for a new or existing
  thread.
- **Docs and tests**: `docs/forges.md` (setup, scopes, self-hosted URLs,
  capability matrix), wiremock coverage for all three API shapes, a real Docker
  Gitea/Forgejo run via `scripts/test-forges.sh`, and an ignored read-only live
  GitHub test.

### Phase 3 — connect

- **Relay service** (`h4rn-connect`): axum + Postgres, self-hostable image and compose stack
  (`deploy/Dockerfile`, `deploy/docker-compose.yml`, `deploy/.env.example`), optional local
  OrbitID profile, `/healthz`, `/readyz`, JSON errors as `{code, message, hint}`, optional
  static web serving, CORS and per-IP rate limits.
- **OIDC login**: authorization code + S256 PKCE via `openidconnect` with a stored nonce,
  device authorization grant for daemons, `userinfo` fallback for thin ID tokens, refresh and
  revocation, and the OrbitID quirks documented in `docs/research.md` §9.
- **Sessions and devices**: h4rn-signed EdDSA access tokens plus rotating refresh tokens
  bound to the device's Ed25519 key through DPoP-style proofs with a per-request nonce; refresh
  rotation is single-flight per family with a 30-second idempotent successor window and
  family revocation on reuse outside it; device revocation ends its sessions.
- **Pairing**: `h4rn://pair?...` QR creation and same-user single-use claims, five-minute
  expiry, rate-limited attempts, daemon notification on claim and revoke, and machine host
  lists including WSL entries.
- **Relay WebSocket**: authenticated `/relay` for daemons and clients, opaque
  `RelayFrame { conn_id, kind, payload }` routing by connection id, pairing enforcement,
  bounded per-peer queues, payload-blind metadata logs and presence broadcasts.
- **Data and audit**: Postgres migrations for users, identities, auth flows, sessions,
  devices, machines/hosts, pairings, presence, envelope-encrypted secrets, audit log, device
  authorizations and the usage ledger; `/v1/me`, devices, machines, pairings, audit, usage and
  auth routes.
- **Shared wire types** (`h4rn-protocol::connect`) and regenerated TypeScript bindings.
- **Docs and tests**: `docs/self-hosting.md`, ADR-028–030, unit tests for pairing hashes,
  proof/envelope/crypto, refresh classification and relay routing/backpressure, plus real
  Postgres integration tests run by `scripts/test-connect.sh` and an optional live OrbitID
  test behind `--orbitid`.

### Phase 3 — remote access glue

- **Daemon login** (`h4rnd login --connect`, `h4rnd logout`): OIDC device
  authorization grant with a printed verification URL and user code, device
  registration with the daemon's transport keys, host and address publishing,
  and credentials in the OS keychain with an encrypted-file fallback
  (ChaCha20-Poly1305, `0600` sibling key) for headless Linux and WSL.
- **Authenticated relay connector**: a persistent, DPoP-proofed `/relay`
  WebSocket with exponential backoff, JSON `RelayFrame`/binary transport
  adaptation, 30 s presence pings, `DevicePaired` allowlisting and
  `DeviceRevoked` session drops. Fixed `http(s)` to `ws(s)` relay URL
  conversion.
- **Pairing CLI** (`h4rnd pair`, `--direct`): connect pairing rendered as a
  terminal QR plus `h4rn://pair` URL, and a connect-free `IKpsk2` flow whose
  QR carries LAN/tailnet candidates, the daemon key and a single-use PSK
  picked up by the running daemon.
- **Tailscale**: best-effort detection through `tailscale status --json` /
  `tailscale ip -4`, `tailscale = auto|off`, optional secure-listener bind on
  the tailnet address, and address advertisement ordered LAN, tailnet,
  relay.
- **Remote-action audit**: `thread.created`, `terminal.opened` and
  `file.written` written locally to `remote-audit.jsonl` and mirrored
  best-effort to `POST /v1/audit`.
- **Connect additions**: machine address candidates (`PUT
  /v1/machines/{id}/addresses`, migration `0002_machine_addresses.sql`) and
  the audit-ingest route.
- **Transport fixes for multi-path remote sessions**: one application-frame
  and one command channel per logical session, resumed paths that attach
  without a second loop, authoritative session close on a peer `Close`
  control, persisted allowlists, and resume-ticket handling.
- **Tests and docs**: `crates/h4rn-daemon/tests/remote.rs` with
  `scripts/test-remote.sh` (Postgres, relay and OrbitID in Docker: device
  grant, pairing, relay-only thread creation, direct→relay failover under
  2 s, revocation and audit), plus updates to `docs/transport.md` and
  `docs/self-hosting.md`.

### Phase 3 — encrypted multipath transport

- **Noise E2E per path** (`h4rn-transport/src/noise.rs`):
  `Noise_IK_25519_ChaChaPoly_BLAKE2s`, with `IKpsk2` for one-time pairing,
  X25519 device identity plus Ed25519 signing key, daemon key pinning from the
  pairing payload, an explicit allowlist/revocation policy, rekey budgets and a
  session-bound prologue.
- **Path-agnostic reliable session** (`src/session.rs`, `src/wire.rs`): 20-byte
  clear packet header, 40-byte encrypted record header, per-direction 64-bit
  sequences, cumulative ack plus 64-bit bitmap, bounded retransmit buffer,
  fragmentation/reassembly, backpressure, 256-byte padding buckets, and an
  opaque resume token carried inside the encrypted `Open` record.
- **Client multipath engine** (`src/engine.rs`): parallel Noise probes for
  direct, Tailscale and relay candidates, health pings, smoothed RTT, immediate
  failover, hysteretic switch-back, and a transient `TransportStatus` event.
- **Daemon `/secure` listener** (`h4rn-daemon`): `SecureService` over
  WebSockets, a `SecureSocket` adapter that feeds the existing protocol loop,
  detached-session resume, fail-closed `404` when disabled, and a `403` for
  plaintext `/ws` on non-loopback binds. Enabled with `H4RN_SECURE=1` and
  optional `H4RN_PAIRING_PSK`.
- **wasm client** (`h4rn-transport-wasm`, `apps/ui/src/gen/transport-wasm`):
  the same core compiled to `wasm32-unknown-unknown` with a `wasm-bindgen`
  API, plus `apps/ui/src/lib/client/secure.ts` implementing the UI `Transport`
  interface. TypeScript owns only sockets, timers and persisted opaque tokens.
- **Protocol bindings**: `TransportStatus`, path/session enums and the
  `transport_status` event, with regenerated TypeScript bindings.
- **Tests**: handshake, revoked key and PSK pairing; loss/reorder/duplication;
  failover without loss or duplication in under 2 s; hysteretic switch-back;
  resume across reconnect; a real daemon Noise `CreateThread` with
  `FakeHarness`; and a wasm handshake smoke test.
- **Docs**: ADR-028/029/030 and [`docs/transport.md`](docs/transport.md)
  covering protocol, threat model and failover.

### Phase 3 — mobile

- **Android/iOS shell** (`apps/mobile`): Tauri v2 app `dev.h4rn.app` that loads the same React
  bundle as desktop, with min SDK 26, target SDK 37, iOS 16, scaffold icons and the `h4rn://`
  URL scheme. Native customisations live in `apps/mobile/native/android/` and are copied over
  the ignored generated project.
- **Pairing** (`apps/ui/src/lib/platform/mobile.ts`): a mobile-only adapter added with one import
  in `main.tsx`. It accepts `h4rn://connect?daemon=…&token=…` on cold and warm starts, scans
  pairing QR codes through `tauri-plugin-barcode-scanner`, stores the pairing in the Android
  Keystore and localStorage, and keeps the token out of the URL. Desktop and browser builds are
  unchanged.
- **Keystore secret store** (`SecretStorePlugin.kt`): AES-256/GCM key in the Android Keystore
  behind `secret_get` / `secret_set` / `secret_delete`; invalidated keys are dropped so the app
  can pair again. iOS returns an explicit unimplemented error rather than falling back to
  plaintext.
- **Native integration**: edge-to-edge window with `env(safe-area-inset-*)`, hardware back mapped
  onto WebView history, and a network security config that allows cleartext only for loopback and
  emulator host aliases in release (LAN hosts only in debug).
- **Build pipeline** (`scripts/mobile-android.sh`): `init`, `build`, `install`, `run`,
  `screenshot`, `logs` and `daemon` commands. Builds one debug APK per ABI (x86_64 for the
  emulator, aarch64 for phones); `run` starts a fake-harness daemon, wires
  `adb reverse tcp:47800 tcp:47800`, launches through a pairing deep link, screenshots and dumps
  logcat.
- **CI**: `mobile-android` builds both APKs on Linux with Temurin 21, SDK 36 and NDK
  27.1.12297006; `mobile-ios` builds an unsigned simulator app on `macos-latest`. TestFlight
  signing requirements and the current Android/iOS limitations are documented in
  `docs/mobile.md`.
- **Verified on the Android emulator**: connect screen, deep-link pairing, online inbox,
  fake-harness thread with streamed output, Keystore cold-start reuse, QR camera permission,
  safe-area layout and hardware back. Screenshots are in `apps/mobile/screenshots/`.

### Phase 2 — more harnesses, terminal, attachments and IDE mode

- **Codex adapter** (`h4rn-harness/src/codex.rs`): long-lived `codex app-server` sessions over
  newline-delimited JSON-RPC with `initialize`, `thread/start`/`thread/resume`, `turn/start`,
  server-initiated approvals, MCP injection through the thread config, provider/base-URL
  overrides, interrupt and process-group shutdown.
- **OpenCode adapter** (`h4rn-harness/src/opencode.rs`): HTTP + SSE sessions against the local
  `opencode` server, model/provider config via `OPENCODE_CONFIG_CONTENT`, permission rules per
  runtime mode, approvals, MCP registration and prompt file parts.
- **Antigravity adapter + ACP client** (`h4rn-harness/src/antigravity.rs`, `acp/`): Agent Client
  Protocol over stdio for Google Antigravity, with capability negotiation, streaming
  updates, permission requests, image/resource prompt content, usage mapping and session
  resume.
- **PTY terminal** (`h4rn-vt`, `h4rn-daemon/src/terminal/`, `apps/ui/src/features/terminal`):
  daemon-owned PTYs with credit-based binary output framing, resize/ack flow control,
  ANSI screen snapshots on attach, multiple viewers per terminal, and an xterm.js pane that
  survives reconnects.
- **Attachments** (`h4rn-protocol/src/blob.rs`, `h4rn-daemon/src/blobs/`,
  `h4rn-harness/src/attachments.rs`, `apps/ui/src/features/attachments`): BLAKE3
  content-addressed blob store with chunked streaming upload, resume, range downloads,
  thumbnails, dedupe and refcount GC; composer tray, picker, lightbox and transcript
  rendering; a shared per-harness delivery matrix (Claude native image blocks, Codex
  `localImage` paths, OpenCode `file://` image parts, ACP image blocks when negotiated,
  `@path` mentions otherwise).
- **IDE mode** (`h4rn-protocol/src/fs.rs`, `h4rn-daemon/src/fs/`, `apps/ui/src/features/ide`):
  workspace-scoped file tree, editor tabs, search, file watching, create/rename/trash/restore
  and git diffs backed by a daemon-side fs/git service with etag write-conflict detection.
- The daemon now routes both binary stream types — PTY input and blob upload chunks — over
  one WebSocket, and the UI client dispatches both while keeping transient fs events out of
  the durable event cursor.

### Phase 1c — protocol gaps closed

- Added `ProtoErrorCode::NotImplemented` and replaced the daemon's
  `Unsupported`-plus-message-prefix workaround.
- `CreateThread` now returns `thread_created` (`thread_id`, `turn_id`, summary) and
  `SendMessage` returns `message_accepted` (`thread_id`, `turn_id`, `seq`), so the UI reconciles
  optimistic rows without guessing.
- Every thread-scoped event carries `thread_id` (and `turn_id` where applicable);
  `ItemCompleted` nests its `item`. The UI no longer infers ownership from the last event.
- `RespondApproval` carries the selected `ApprovalOption.id`, and the daemon broadcasts
  `approval_resolved` with the option and answering device; every UI resolves from the stream.
- Documented `Event.seq` as a durable, daemon-global cursor. `Thread.events` is now
  `SeqEvent[]`, `Subscribe` accepts `since_seq`, and the client resumes a gap-free global replay
  across reconnects (ADR-003 note).
- `CreateThread` accepts `runtime_mode` (`approval_required`, `auto_accept_edits`,
  `full_access`) and optional `worktree`; the Claude adapter maps runtime mode to
  `--permission-mode`. Worktree creation returns `not_implemented` for now.
- Added `AgentState::Queued`, `Scheduled` and `Unreachable`, with matching UI labels and glyphs.
- The daemon resolves harnesses through `h4rn_harness::registry::builtin_adapters()`, injects
  `FakeHarness` only under `H4RN_FAKE_HARNESS=1`, persists `session_id` for `--resume`, reports
  real `last_turn` usage, and surfaces `stderr_tail` on crashes.
- Added a real Claude Code daemon e2e (`crates/h4rn-daemon/tests/e2e_claude.rs`, ignored by
  default) and a Playwright UI e2e driving `h4rnd serve` with the fake harness.

### Phase 1d — desktop shell

- `apps/desktop`: Tauri v2 shell that starts the bundled `h4rnd` daemon as a sidecar when
  `127.0.0.1:47800/healthz` is not answering, reads its access token with `h4rnd token`, and
  loads the UI with `?daemon=<ws_url>&token=<token>`. The token never reaches a log.
- Windows WSL hosts: `daemon_endpoints()` lists `wsl.exe -l -q` distributions (UTF-16 output
  decoded) and, when one is activated, starts
  `wsl.exe -d <distro> -- h4rnd serve --bind 127.0.0.1:47801`, reachable through WSL2
  localhost forwarding.
- Narrow IPC surface (ADR-001): `daemon_endpoints`, `open_external` (http/https/mailto only),
  and keychain `get`/`set`/`delete` through the OS credential store (ADR-016). Capabilities
  grant only core APIs and scoped link opening.
- Window remembers size and position (`tauri-plugin-window-state`), opens at 1440x900 with a
  960x600 minimum, keeps the native title bar, follows the system light/dark theme, and stays
  hidden until a daemon endpoint is chosen.
- Bundles: MSI + NSIS on Windows, DMG/APP on macOS, AppImage/DEB on Linux; `h4rnd` ships as an
  `externalBin` sidecar. On exit the shell stops only the daemon it spawned.
- `scripts/desktop-build-windows.sh` mirrors the repo to a Windows-local path, builds
  `h4rnd.exe` and the Tauri sidecar, runs `cargo fmt --check` and `cargo clippy -D warnings`,
  produces the installers, and verifies the packaged app end-to-end (sidecar health check,
  window screenshot, clean teardown).

### Added

- Cargo workspace (edition 2024, resolver 3) with pinned dependency versions and
  `rust-toolchain.toml`, `rustfmt.toml` and `.editorconfig`.
- `h4rn-protocol`: string-newtype IDs (UUID v7), `ClientFrame`/`ServerFrame`, the full
  `ClientRequest` surface (including placeholder terminal, filesystem, MCP, skills, proxy, usage
  and forge requests), `Event`, the ten `AgentState` values, `ThreadTab`, `ThreadSummary`,
  `HarnessKind`, usage/approval/tool-call types, and binary PTY framing.
- Generated TypeScript bindings under `apps/ui/src/gen/` with a barrel and
  `scripts/check-bindings.sh` drift check.
- `h4rn-core`: the ADR-004 lifecycle (`apply(state, event)`, `tab_for(state, now, settings)`)
  with Working/Done/Settled placement, manual override and pins, snooze wake rules, and 24-hour
  auto-settle. Event store trait with SQLite and in-memory backends, embedded migrations.
- `h4rn-harness`: async `Harness`/`HarnessSession` trait, normalized `HarnessEvent` stream,
  `FakeHarness`/`FakeSession`, and a reusable test spec.
- Compiling skeletons with tests for `h4rn-daemon`, `h4rn-connect`, `h4rn-transport`, `h4rn-vt`,
  `h4rn-mcp`, `h4rn-skills`, `h4rn-proxy`, `h4rn-meter` and `h4rn-forge`. The relay serves
  `/healthz`.
- Tauri v2 desktop and mobile shells (workspace-excluded) with deep-link, opener, dialog and
  mobile barcode-scanner plugins, capabilities and placeholder icons.
- UI scaffold: React 19, Vite 8, TypeScript strict, Tailwind v4, design tokens and fonts from
  `DESIGN.md`, shadcn primitives (button, badge, input, textarea, separator, skeleton, tabs,
  dialog, command, sheet, tooltip, scroll-area, dropdown-menu), `motion`, `thinking-orbs` and
  `blobatar`, plus a placeholder shell.
- `apps/web` static publish step.
- GitHub Actions CI: Rust format/lint/test, bindings drift, UI typecheck/lint/test/build, Tauri
  desktop compile check, and a Docker image build.
- `deploy/Dockerfile` and `deploy/docker-compose.yml` for the relay.
