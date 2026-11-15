# Changelog

All notable changes to h4rn are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses semantic versioning.

## [Unreleased]

### Council round 1: authorization, transport and pairing

- **Binary PTY input is authorised (A1).** It needs `Hello`, the `terminal` scope and a viewer
  this connection attached; blob chunks need `Hello` and `approve`.
- **`/secure-mcp` authenticates the peer (A2).** The caller must be a registered peer daemon key;
  `machine` and device id come from the registry, not the frame; `allowed_devices: None` means the
  target machine's daemon only.
- **Authorization fails closed (A3).** A persisted device registry (`devices.json`) records scopes
  and pairing provenance per key. No entry means denied; the identity is the handshake key, never
  the relay's device id. Pairings made before scopes were recorded must be paired again.
- **Reconnect reconciles revocations (A4).** A relay snapshot removes connect-paired devices it no
  longer lists and ends their sessions; direct pairings are kept.
- **`h4rnd pair --direct --scope` is honoured (A5).** Without `--scope` a direct pairing grants every
  scope, now stated in the help. The pairing watcher also runs for daemons not signed in to connect.
- **Short authentication string for typed codes (A6).** The phone shows a 12-digit code derived in
  Rust and confirms it against the host's before pinning the daemon key. New request
  `PairingConfirmations`; wasm export `pairing_sas` (committed wasm rebuilt).
- **One QR pairs one device (A7).** The one-time PSK is consumed atomically at authorisation.
- **`SetThreadTab` and `Snooze` need `approve` (A8).**
- **Scopes are per machine in h4rn-connect (A9).** Admin on one machine no longer mints or revokes
  pairings for another.

### Cross-machine MCP sharing, target side

- **Shares now reach the target machine.** Each daemon keeps a peer directory (from connect, or
  added directly), dials a peer's `/secure-mcp` with its own daemon key, receives that peer's
  grants as pushed snapshots (share, unshare, policy change, server removal, peer revoked), builds
  the tunnel itself and registers the server with its hub. Unshare, revocation and a dropped
  session unregister it. B's MCP list shows the server as running on the owner.
- Limits: LAN/tailnet endpoints only (no relay path for MCP tunnels yet).

### Council round 1: correctness and hygiene

- **IDE filesystem.** Creating or writing through a dangling symlink is refused instead of
  following it outside the workspace. Saving a file that was deleted mid-edit now returns a
  conflict with `current_etag: null` (the IDE conflict dialog offers "Recreate with mine" or
  "Leave deleted") instead of silently recreating it.
- **Journal.** `Journal::append` writes the thread row, the domain log and the wire log in one
  SQLite transaction.
- **Auto-resume.** A due schedule on an active thread waits silently instead of emitting an
  Error every second, and a message appended to an active turn cancels the stale schedule so it
  cannot fire a duplicate prompt.
- **h4rn-connect.** The skill-sync pull cursor is derived from the rows the pull returned, so a
  concurrent push cannot be skipped. The OIDC token exchange repeats `redirect_uri`.
- **Browser wasm freshness.** `scripts/wasm.sh check` rebuilds `h4rn-transport-wasm` with the
  pinned `wasm-bindgen` CLI and a path-remapped build and fails on any difference from
  `apps/ui/src/gen/transport-wasm`; CI runs it (a new `wasm` job). The committed wasm was
  rebuilt so it no longer embeds machine paths.
- **Lint.** `bun run lint` runs `biome check --error-on-warnings`, so any warning fails instead
  of scrolling past; the stale registry comment in `h4rn-harness` is fixed.

### UI fidelity round 4

- **Apply and restart (Providers).** `ApplyProxyOverrides` interrupts the live turn of every
  routed harness, drops its session, and the next message starts one with the new routing.
  Each per-harness override now reports its `config_path` and a redacted `preview` (secrets are
  `null`, never sent).
- **Fail over…** options carry an `account_label` (the credential name without directory or
  extension) and plain-text reasons; the card and the menu name the same account.
- **Test runs** show `48 passed` / `46 passed · 2 failed` in the tool row's result column
  (`ToolStats::Tests`, parsed in Rust from the shell output).
- The phone auto-resume card has its own padded layout; opening the right panel at a narrow
  width closes the sidebar to make room; every native `<select>` is a `ChipSelect` and a lint
  rule keeps it that way; the palette gained Deny, Changes, Subagents, IDE, Right panel,
  Terminal, Sidebar and Pair a device; `bun run lint` reports 0 warnings.

### Gaps left by UI fidelity round 3

- **Pairing scopes and a real short code (ADR-042).** The Devices checkboxes now choose what the
  paired device may do (`view`, `approve`, `terminal`, `files`, `admin`); connect records and
  signs them, and the daemon refuses anything outside them. The 8-character code is Crockford
  base32, single use, expires with the QR and is rate limited; the phone pairing screen accepts
  it (`POST /v1/pairings/claim-code`). `h4rnd pair --scope` sets scopes from the terminal.
- **Fail over…** on a limit-hit thread resumes now on another model or account: the daemon names
  its default and the eligible alternatives (`ListFailoverOptions`, `FailOver`).
- **PR detail:** a Commits tab, `required` marks on checks, View log and Re-run failed (GitHub
  check suites, GitLab pipelines; disabled with the reason on Gitea/Forgejo).
- **MCP import conflicts** use one "Resolve…" popover (keep mine, use theirs, rename, skip).
- **Tool rows** show `+N −M` on edits and match counts on searches, derived in Rust for all four
  adapters (`ToolCall.stats`).
- **Sidebar footer** names the host on a fresh install and counts real hosts otherwise.

### Credentials and remote sign-in

- **Browser storage never holds a credential.** The daemon token, pairing URI, Noise
  identity/pin/session and h4rn-connect access/refresh tokens now go through one platform secret
  store: Android Keystore / iOS Keychain behind `secret_*`, the OS keychain behind `keychain_*`
  on desktop, and AES-GCM envelopes in IndexedDB under a non-extractable `CryptoKey` on the web
  (ADR-041). Legacy `localStorage`/`sessionStorage` values are migrated on first boot and
  deleted; `?token=` and `?pair=` bootstrap secrets are consumed and stripped from the address
  bar, and `sessionStorage` keeps only short-lived, non-secret OIDC state.
- **OIDC sign-in completes on every shell.** The web returns through `/auth/callback` and
  reloads the route the person left, desktop waits on the loopback callback, and mobile opens the
  provider in the system browser and returns through `h4rn://callback`. The device-code grant
  remains as "Sign in on another device"; the remote e2e now drives both the browser redirect
  and the device-code fallback (from a fresh profile, since the relay refuses a revoked device's key) against Docker OrbitID.
- **Resume now acknowledges immediately.** `ResumeNow` finishes the schedule and returns before
  the harness produces its first token, and the resumed turn reports progress through
  `ScheduleChanged` and thread events, so a realistic slow harness no longer times out the
  request. A normal `SendMessage` likewise returns once the turn is recorded, and the thread
  record is no longer locked across the harness write, so a slow adapter cannot stall thread
  listings, snapshots or settings requests with it.

### UI: layout v2

- **One left sidebar, one right panel.** The nav rail and inbox column are merged into a single
  resizable sidebar grouped by project (folder + host), with Working / Done / Settled, Snoozed,
  search, and a hosts footer that opens presence, pairing and management. Settings, including
  MCP servers and Skills, temporarily replaces the thread list with its own nav and a back button.
  The sidebar collapses with ⌘B and keeps a needs-you dot on the reveal button.
- **Thread | IDE is one segmented toggle** (⌘E). The IDE follows the selected thread's project,
  keeps its tabs per project, and says "Restored N tabs" when it returns. Its file tree moved into
  the right panel's Files tab; the header carries a thread chip back to the conversation.
- **The right panel** is scoped to the selected thread: Changes (vs agent / vs HEAD, by turn),
  Files, and Subagents. It remembers open state, width and tab, opens with ⌘⌥B, and folds into one
  Changes | Subagents sheet on a phone, where Files and the IDE are desktop-only.
- **Pull requests** no longer claims a list column: the main area shows the list and the detail.
- **Shortcuts** add ⌘B, ⌘E, ⌘⌥B, D, ⌘J, ⌘P and T, and Esc now leaves Settings and MCP & skills.
- `apps/ui/scripts/screenshots.mjs` captures the new `panel-changes`, `ide-switch`,
  `sidebar-collapsed`, `p-changes` and `p-subagents` screens alongside the existing set, and the
  Playwright e2e drives the new view toggle and right panel against the real daemon.

### Phase 7 — packaging

- **Desktop bundles** ship on Windows (MSI and NSIS), macOS (universal DMG) and Linux
  (AppImage and `.deb`) with the `h4rnd` sidecar staged per target. Tauri's updater plugin is
  configured through a generated release overlay: `H4RN_UPDATER_ENDPOINT` selects the hosted or
  self-hosted manifest, `H4RN_UPDATER_PUBKEY` pins the minisign key, and signed `.sig`/`latest.json`
  artifacts are emitted only when the signing key is present.
- **Standalone `h4rnd` archives** for Linux x86_64 glibc/musl, Linux aarch64, macOS universal and
  Windows, each with a checksum, `README.txt` and contrib service files (systemd user unit,
  launchd plist, Windows scheduled-task scripts).
- **Android release signing** reads the upload keystore from
  `ANDROID_KEYSTORE_PATH`/password/alias/key variables (CI decodes `ANDROID_KEYSTORE_BASE64`) and
  falls back to debug signing locally; the Play internal track uploads through a fastlane lane.
- **iOS Keychain secret storage** implements `secret_get`/`secret_set`/`secret_delete` in Swift
  with `kSecClassGenericPassword` and `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`, matching
  the Android Keystore command surface; the CI macOS job compiles the Swift package for the
  simulator and uploads a signed TestFlight build when Apple credentials exist.
- **Relay image and chart**: `ghcr.io/<owner>/h4rn-connect` is built for `linux/amd64` and
  `linux/arm64` with Buildx on tags, compose is pinned to the published tag (with a local build
  override), and a Helm chart provides Deployment, Service, Ingress, PodDisruptionBudget and
  optional in-chart Postgres or an external database URL.
- **Tag-driven release workflow** (`.github/workflows/release.yml`) builds all targets into a
  draft release, writes `SHA256SUMS`, and generates review-only winget, Homebrew cask and AUR
  manifests. `docs/release.md` lists every secret and the release/update flow.

### UI: Quiet native redesign

- **Direction A ("Quiet native") replaces the Prompt Book look** across `apps/ui`: graphite
  neutrals with one 258° blue accent reserved for "you", Geist and Geist Mono self-hosted via
  `@fontsource-variable`, hairline panes, 4 px spacing, and the 4/6/8/12/14 radii scale.
- **Shell, inbox and thread** are rebuilt around the prototype: 212 px sidebar with hosts and
  connection line, 328 px inbox with Working / Done / Settled segments and state glyphs, a
  detail pane with provenance, an approval card above the composer, and a usage footer.
- **Subagents inspector, terminal, attachment tray/lightbox, command palette, new-thread and
  shortcuts dialogs, toasts** are restyled to the same grammar; the terminal follows the theme.
- **IDE** ports the prototype's file tree, agent-changes group, tabs, `Edit | vs HEAD | vs agent`
  control, change banner with Revert file / Accept, full-hunk diff bands and 26 px status bar.
- **Phone** gains the large-title inbox, 66 px rows, pushed thread header, compact approval card
  and review sheet, full-screen terminal with key bar, pairing consent flow, and a 14-day Usage
  chart.
- **Offline, loading, empty and partial-failure states** now use the prototype's copy and
  layout-shaped skeletons, including "Offline · threads as of HH:MM", "Showing 3 of 4 hosts",
  the paused-stream line and disabled approvals with a reason.
- **New mock-mode surfaces** for Pull requests, MCP & skills, Usage, Settings (hosts, devices,
  providers) and pairing are routed now and clearly marked as Phase 4–6 shells until their wire
  protocol lands.
- `apps/ui/scripts/screenshots.mjs` now captures the 22 prototype screens in light and dark at
  1440×900 or 390×844 into `apps/ui/screenshots/`, and `?scenario=` adds `loading`, `offline`
  and `error` states for review.

### Phase 4 — skills

- **Skill bundles** (`h4rn-skills`): directories with `SKILL.md` are
  canonicalised into a BLAKE3-addressed manifest (relative paths, sizes,
  digests, executable bits) and stored once under `skills/bundles/<version>`,
  with SQLite metadata in `skills/registry.db`. Frontmatter parsing, git
  sources pinned to a resolved commit, size/file caps and path-traversal
  validation are included.
- **Per-harness installers**: native `SKILL.md` copies for Claude Code, Codex,
  OpenCode and Antigravity, with user and project roots detected, per-machine
  enable/disable, upgrade-in-place and a `.h4rn-managed` JSON marker. A
  user-owned directory is never overwritten or removed; Codex and Antigravity
  share `.agents/skills` through the marker's harness list.
- **Import**: preview and commit scans of harness directories with
  content-hash dedupe, duplicate detection and conflict rows for a different
  version of the same name.
- **Cross-machine sync**: `h4rn-connect` endpoints `POST /v1/skills/sync/push`
  and `/pull`, Postgres migrations `0003_skills.sql`, envelope encryption at
  rest, version vectors with last-writer-wins and conflict surfacing, pull
  cursors and tombstones. The daemon refuses to upload a bundle the secret
  scanner flags and reports `Blocked` with a file/line hint.
- **Protocol**: `SkillsList/Add/Update/Remove/SetEnabled/ImportPreview/
  ImportCommit/Conflicts/ResolveConflict/Diff/SyncNow`, response bodies
  `Skills`, `SkillAdded`, `SkillDetail`, `SkillsImportPreview`,
  `SkillConflicts`, `SkillDiff`, `SkillSyncStatus`, plus transient
  `SkillsChanged` and `SyncStatus` events. TypeScript bindings regenerated.
- **Daemon glue**: `AppState.skills`, all request handlers, transient event
  forwarding, and a `ConnectClient` implementation of `SkillSyncApi`.
- **Docs and tests**: `docs/skills.md`, ADR-034/035, hermetic tests for
  frontmatter, hashing, installers, import, conflicts and secrets, a real
  Claude Code skill-pickup test behind `--ignored`, and two-registry sync tests
  run against real Postgres by `scripts/test-connect.sh`.

### Phase 4 — MCP hub

- **One hub per daemon** (`h4rn-mcp`): a loopback streamable-HTTP endpoint at `/mcp` speaking
  revisions `2025-06-18`, `2025-03-26` and `2024-11-05`, with per-thread bearer tokens stored as
  BLAKE3 digests, scopes for server and tool narrowing, `Mcp-Session-Id` handling and `DELETE`
  revocation.
- **Namespacing and providers**: `server__tool` names with deterministic `_2`/`_3` collisions, a
  reserved `h4rn` namespace and the `ToolProvider` extension point behind the built-in
  `h4rn__echo`.
- **Upstreams**: stdio, streamable HTTP and legacy SSE with lazy start, health ticks, restart
  backoff, capability caching, list-changed invalidation, timeouts, UTF-8-safe output caps and
  cancellation.
- **Durable configuration**: `mcp.db` registry for transports, enable matrices, imports and
  shares; environment variables and headers stored only as references, with values in the OS
  keychain or an encrypted-file fallback.
- **Native import**: Claude Code (`~/.claude.json`, `.mcp.json`), Codex (`config.toml`),
  OpenCode (`opencode.json`) and Antigravity (`mcp_config.json`), with a conflict preview and
  `KeepMine`/`UseTheirs`/`Rename`/`Skip` resolution.
- **Cross-machine sharing**: owner-enforced policies (allowed tools, confirmation, allowed
  devices), a second secure `/secure-mcp` tunnel carrying JSON-RPC, local audit of every remote
  call, and no credential forwarding.
- **Harness injection**: Claude, Codex, OpenCode and Antigravity receive the endpoint and bearer
  token per session; the token never appears in argv.
- **Protocol and events**: `ClientRequest::Mcp` with the full MCP operation set, legacy request
  mapping, `McpServers`/`McpServer`/`McpImportPreview`/`McpTools`/`McpCall` responses and
  transient `McpServerStatus`/`McpToolCalled` events on `Topic::Mcp`.
- **Docs and tests**: `docs/mcp.md`, ADR-036/037, real stdio-fixture and two-daemon tunnel tests,
  secret non-leak tests, and ignored real Claude/Codex injection tests (`h4rn__echo`).
### Phase 4 — cross-harness subagents

- **Child threads** (`h4rn-daemon`): `spawn_subagent` creates a normal thread
  with `parent_thread_id` and a pre-allocated `SubagentId`, so delegation reuses
  the journal, resume, usage and approval machinery. `Delegated` items and
  `SubagentSpawned`/`SubagentUpdated`/`SubagentFinished` events project the
  parent's view; child rows stay out of the top-level list and inbox.
- **Supervisor**: one daemon-wide watcher enforces turn, wall-clock and token
  budgets, maps terminal turns to `completed`/`failed`/`cancelled`/
  `budget_exceeded`, folds the last assistant text, usage and changed files into
  a result, and cleans worktrees on settle. Nesting is capped at depth 2 by
  default and harness availability is checked against the installed catalog.
- **Isolation and context**: shared workspace or a git worktree under
  `<data_dir>/subagents/<id>/worktree` on `h4rn/sa-<short id>`; context handoff
  is none, a caller summary, or selected workspace files (128 KiB cap, path
  confinement).
- **Approvals**: a child inherits the parent runtime mode; its approvals are
  mirrored to the parent as `sa:<id>` with `[Subagent]` attribution and either
  side answers both. Settling a child clears stale mirrored hands.
- **MCP tools**: `h4rn__spawn_subagent`, `get_subagent_status`,
  `get_subagent_result` (bounded wait), `cancel_subagent` and `list_subagents`,
  scoped by the parent session token. The registry now supports multiple
  providers per namespace so these coexist with `h4rn__echo`.
- **Protocol**: `ClientRequest::Subagent { SubagentRequest }`,
  `SubagentView`/budget/context/result types, `ThreadSummary.parent_thread_id`,
  `ThreadFilter.include_children`, `ResponseBody::Subagents/Subagent`,
  `ApprovalRequested.subagent_id`, and TypeScript bindings.
- **Docs and tests**: `docs/subagents.md`, ADR-038/039, unit tests for budgets
  and depth, an MCP-HTTP integration test with `FakeHarness` parent and child,
  and an ignored real Claude Code → Codex delegation test.
- **Metering, resume and durability**: child turns are metered with
  `parent_thread_id`, and `GetUsage`/reports accept `include_children` to roll a
  delegation tree into the parent's totals. Token budgets enforce the ledger's
  metered totals and wall-clock excludes time paused for a resume. A limit-hit
  child becomes `scheduled`, arms auto-resume and is mirrored to the parent
  instead of failing. Supervisor records persist in `subagents.db`, so the
  delegation card, status and result survive a daemon restart.

### Phase 5 — metering and auto-resume

- **Ledger** (`h4rn-meter`): durable, append-only SQLite rows per turn with
  input/output/cached read/cached creation tokens, cost and provenance, TTFT,
  TPS excluding tool time, generation and wall time, timestamps, workspace,
  machine, account, harness, model, provider and raw payload. Prices are a
  deterministic estimate when the harness reports no cost; unknown models stay
  visibly unpriced.
- **Metering**: per-turn observation of harness usage, first-token TTFT, tool
  call intervals, `TurnCompleted` accounting and a live `UsageTick` throttled to
  about one per second. Reports aggregate per thread, workspace, machine or
  globally, with optional hour/day buckets and thread/workspace/machine/model/
  harness grouping.
- **Proxy usage**: optional ingestion of CLIProxyAPI `/v8/management/performance`
  rows with stable ids, deduplicated against self-metered turns by
  `(thread_id, run_id)`, correlation id or same provider/model within two
  minutes. Documented in [`docs/metering.md`](docs/metering.md).
- **Limits and resets**: `LimitHit` records harness, provider/account, model,
  reset time, consumption and message; the UI can read limits, countdowns and
  reset history.
- **Auto-resume**: schedules at the provider reset plus deterministic jitter,
  capped exponential backoff with an explicit reason when the reset is unknown,
  persisted schedules across restarts, `CancelResume` and `ResumeNow`, thread
  stays Working with `AgentState::Scheduled`, queued input held, resumed turn
  uses a configurable prompt, and optional proxy failover recorded visibly on
  the schedule.
- **Protocol and daemon**: `GetUsage`, `GetLimits`, `ListResets`, `CancelResume`,
  `ResumeNow`, `SetAutoResumePolicy`; ephemeral `UsageTick`, `LimitStatus` and
  `ScheduleChanged` frames; daemon handlers, meter assembly and background
  proxy-ingest/usage-sync loops.
- **Sync**: idempotent batched ledger sync to h4rn-connect (`row_id` unique per
  account), migration `0004_usage_sync.sql`, `POST /v1/usage` and
  `GET /v1/usage/aggregate` with real Postgres aggregation.
- **Tests**: ledger math, price provenance, dedupe, scheduler restart/cancel/
  resume-now/failover, lifecycle while scheduled, a FakeHarness
  limit-hit→scheduled→resumed end-to-end test, Postgres sync idempotency and
  aggregate buckets.
- **Subagent rollup**: ledger rows carry `parent_thread_id`; `GetUsage`'s
  `include_children` folds a parent's child turns into its totals, and subagent
  token budgets enforce the same metered counts.
- **Docs**: [`docs/metering.md`](docs/metering.md) defines every metric, its
  source and the dedupe rule.

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
