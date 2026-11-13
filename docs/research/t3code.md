# T3 Code — implementation research for h4rn

Source read: `pingdotgg/t3code`, commit `8a1811e269e81ba0ebc59db34d4dc4e24986df2c`
(2026-11-12, server version `0.1.9`). Cloned read-only to `/tmp/h4rn-research/t3code`.
Everything below is from source unless it is explicitly marked as a judgement call.
Where a thing is *not* in the code, it says so.

## 0. What T3 Code actually is (context that changes every verdict)

| Layer | T3 Code | h4rn |
|---|---|---|
| Desktop shell | **Electron 44** (`apps/desktop/package.json`, `electron: 44.10.1`) | Tauri v2 |
| Server | Node + TypeScript, Effect (`apps/server`), single process per "environment" | Rust daemon |
| Wire | Effect RPC over WebSocket + HTTP (`apps/server/src/ws.ts`, `apps/server/src/orchestration-v2/http.ts`) | TBD |
| Storage | SQLite (`apps/server/src/persistence/`), event log + projections | TBD |
| Terminal client | **libghostty-vt compiled to WASM + Canvas 2D**, *not xterm.js* (`apps/web/src/terminal/ghostty/README.md`) | TBD |
| Terminal server | `node-pty` (`apps/server/src/terminal/NodePtyAdapter.ts`) | portable-pty |
| Harness access | Per-provider TypeScript adapters, most via vendor **SDKs**, not hand-rolled protocols | Rust clients / CLI |

Two important structural facts:

1. There are **two generations of harness code** in-tree: a legacy "provider/driver"
   layer (`apps/server/src/provider/`) and the current "orchestration-v2 adapter" layer
   (`apps/server/src/orchestration-v2/Adapters/`, plus `packages/provider-*`). V1 rows
   survive only as importer input (`apps/server/src/orchestration-v2/legacy/`). Do not
   copy both layers — that duplication is the single biggest maintenance cost visible in
   the repo (the Claude V2 adapter alone is 13,733 lines).
2. It is a **documented architecture**, not just code: `docs/orchestration-v2/*.md` is
   unusually good and is the fastest way to understand the graph model
   (`AppThread → Run → ExecutionNode`, provider threads attached to nodes).

---

## 1. Spawning and driving each harness

### 1.1 Process/protocol matrix

| Harness | Process model | Protocol | Resume | Approvals | Interrupt |
|---|---|---|---|---|---|
| Claude Code | `claude` CLI child, **spawned per turn** by `@anthropic-ai/claude-agent-sdk` inside the server process | stream-json + SDK **control channel** on stdin/stdout | SDK `resume: <sessionId>` (else `sessionId: <new>`) | SDK `canUseTool` + `permissionMode` | SDK `query.interrupt()`; SIGKILL group after 10 s |
| Codex | `codex app-server`, **one shared process per provider instance**, many threads | JSON-RPC over stdio, newline-delimited (JSONL) | `thread/resume` (+`thread/fork`) | provider-initiated JSON-RPC approval requests; policy `untrusted`/`on-request`/`never` + reviewer `user`/`auto_review` | `turn/interrupt` (request ack, then `turn/completed`) |
| OpenCode 1.x/2.x | `opencode serve --hostname=127.0.0.1 --port=<n>`, or a user-supplied external server URL | HTTP + SSE via `@opencode-ai/sdk/v2` | `session.get` / prompt with existing `sessionID` | OpenCode permission rules pushed per session | `session.abort` |
| ACP family (Grok, Kiro, Fx, Antigravity, ACP-registry) | agent binary over stdio | ACP JSON-RPC, `protocolVersion: 2` | `session/load` or `session/resume` when advertised, else portable context handoff | `session/request_permission` (agent-initiated) | `session/cancel` |
| Cursor | `@cursor/sdk` (`Agent.create`/`Agent.resume`), spawns `cursor-agent` | SDK (vendor) | agent id | SDK approval / `autoReview` + sandbox | SDK cancel |
| Pi | `pi --mode rpc --extension <cache>/pi-t3-mcp-extension.ts` | JSON-RPC-ish RPC mode | session id | runtime-mode env | cancel |
| Muse | `@muse-code/sdk` host, `session/start` / `session/resume` JSON-RPC | vendor JSON-RPC | `session/resume` | `permissionProfile` | cancel |
| Antigravity | ACP adapter with `mcpOverHttp: true` (`AntigravityAdapterV2.ts:214`) | ACP | ACP | ACP | ACP |

### 1.2 Claude Code — details that matter

- Import: `@anthropic-ai/claude-agent-sdk` (`ClaudeAdapterV2.ts:49`). T3 never speaks
  stream-json itself; the SDK does. The CLI is resolved from `settings.binaryPath`
  (`pathToClaudeCodeExecutable`) or `claude` on `PATH`.
- **One CLI process per turn**, not per session — explicit comment at
  `ClaudeAdapterV2.ts:12161`: *"The SDK spawns the CLI per turn, so only the open waits for
  a provider update."* Continuity comes from `resume`.
- Process-group hygiene (`ClaudeAdapterV2.ts:375-412`) is worth copying in Rust:
  ```
  { trap '' TERM; read -r _ <&3; kill -TERM 0; sleep 5; kill -KILL 0; } </dev/null >/dev/null 2>&1 &
  exec "$@" 3<&-
  ```
  spawned as `/bin/sh -c <script> t3-claude <cmd> <args>` with `detached: true`,
  `stdio: ["pipe","pipe","pipe","pipe"]`, and `SIGKILL` on the group at exit. The
  inherited fd 3 pipe is held only by the server, so **if the server crashes the CLI's
  whole process group is killed** — this is the anti-orphan trick h4rn needs.
  On non-Linux/darwin paths they fall back to plain `NodeChildProcess.spawn`.
- Descendants are additionally tracked via `ps -A -o pid=,ppid=,pgid=,lstart=` polling
  (`CLAUDE_DESCENDANT_POLL = "1 second"`) to catch `setsid` daemons.
- Environment (`apps/server/src/provider/Drivers/ClaudeHome.ts:52-96`): sets
  `CLAUDE_CODE_ENTRYPOINT=t3-code`, sets `CLAUDE_CONFIG_DIR` (never `HOME`, to keep the
  macOS Keychain login working), and **deletes** inherited host-session variables
  (`CLAUDECODE`, `CLAUDE_CODE_SESSION_ID`, `CLAUDE_CODE_MESSAGING_SOCKET`, …) so a nested
  Claude does not believe it is part of the user's terminal session. It also deletes
  `ELECTRON_RUN_AS_NODE` so commands Claude runs do not start apps as Node.
- System prompt is appended through the SDK (`systemPrompt: { type: "preset", preset:
  "claude_code", append: [...] }`), not via `--append-system-prompt`, because the SDK
  overwrites CLI append text (`ClaudeAdapterV2.ts:2101-2121`).
- Resume identity: `input.resume ? { resume: nativeThreadId } : { sessionId:
  nativeThreadId }` (`ClaudeAdapterV2.ts:1996-2000`). Only successfully opened sessions are
  marked resumable.

### 1.3 Codex — details that matter

- Command: `codex app-server -c thread_unload_delay_secs=0 <user launch args>`
  (`apps/server/src/provider/codexLaunchArgs.ts:16-20`). `-c thread_unload_delay_secs=0`
  exists so Codex's own unload delay does not stack on T3's explicit unsubscribe.
- One app-server serves many threads (`CodexAdapterV2.ts:1469`: *"One app-server serves
  many threads, so the id goes in per-thread shell config, not its env"*), and it is
  wrapped in `AgentScope` with `shared: true`.
- Escalation: `forceKillAfter: "2 seconds"` on `ChildProcess.make`
  (`CODEX_APP_SERVER_FORCE_KILL_AFTER`).
- Turn-scoped state is passed as **config on the request**, not on the process:
  `thread/start` / `thread/resume` / `thread/fork` carry `{ cwd, model, config }`, including
  `shell_environment_policy.set.T3CODE_THREAD_ID = <threadId>` so every shell Codex
  spawns is attributable to a T3 thread (used by port discovery).
- Runtime modes map to `approvalPolicy`/`approvalsReviewer`
  (`CodexAdapterV2.ts:723-760`): `approval-required → untrusted/user`,
  `auto-accept-edits → on-request/user`, `auto → on-request/auto_review`,
  `full-access → never/user`.
- Protocol framing is hand-written in `packages/effect-codex-app-server/src/protocol.ts`:
  JSONL split on `\n`, 128 MiB max line, bounded 32 buffered raw messages, explicit
  `respond`/`respondError` so agent-initiated requests (approvals) can be answered.

### 1.4 OpenCode — details that matter

- Spawn: `opencode serve --hostname=<h> --port=<n>` (`packages/provider-opencode/src/server/OpenCodeRuntime.ts:594`),
  with `OPENCODE_CONFIG_CONTENT` (default `{}`) and optional
  `OPENCODE_SERVER_PASSWORD`; client auth is `Basic base64("opencode:<password>")`
  (`OpenCodeRuntime.ts:559-565`).
- Minimum version gate: `MINIMUM_OPENCODE_VERSION = "1.14.19"`, health check against
  `/health` expecting `{ healthy: true, version }`.
- An **external** server (`settings.serverUrl`) is supported; in that mode T3 deliberately
  skips T3-MCP injection and attachment file URIs, because the external server may not
  share paths or reach the loopback MCP endpoint (`v2/adapter.ts:4126-4129`,
  `4156-4160`).
- Server processes are tracked in `OpenCodeServerLedger` so a restarted T3 can reap orphans.

### 1.5 ACP family

- `initialize` with `protocolVersion: 2`; capabilities negotiated
  (`initializeResult.agentCapabilities.mcpCapabilities`,
  `promptCapabilities`, `sessionCapabilities`).
- Session creation is `session/new`, then `session/load`/`session/resume`/`session/fork`
  depending on what the agent advertised (`AcpSessionRuntime.ts:2230-2270`). Missing
  capabilities degrade by policy: steer → interrupt+restart, fork → portable context
  transfer (see `docs/orchestration-v2/provider-switching-and-context.md`).
- Approvals are agent-initiated `session/request_permission`; T3 answers from the runtime
  policy in `packages/provider-acp/src/server/clientPolicy.ts`.
- The ACP **terminal** capability is bridged too (`clientTerminals.ts`), i.e. an agent can
  ask T3 to run a command in a terminal it manages.

**Verdict — 1. Harness spawning: ADAPT.** Copy the per-harness adapter matrix, process-group
watchdog, env scrubbing and per-thread config injection; but h4rn must re-implement each
protocol in Rust (no TS SDKs), and Claude in particular must be driven via the documented
CLI `--print --input-format stream-json --output-format stream-json` surface instead of the
Agent SDK.

---

## 2. Thread lifecycle: Working / Active / Snoozed / Settled

T3 has no "Done" tab. The sidebar is `Active` (with a collapsed `Working` shelf), `Snoozed`,
and `Settled` (`docs/user/thread-sidebar.md`). The exact rules live in client state, not the
server, except settlement which is server-side.

### 2.1 Working (the "Working" section)

`packages/client-runtime/src/state/threadInbox.ts:17-32`:

```ts
if (thread.hasPendingApprovals || thread.hasPendingUserInput) return false;
if (thread.pendingSubagentRequest != null) return false;
if (!threadRuntimeIsActive(thread.runtime) && thread.runtime?.status !== "idle") return false;
const run = thread.latestRun;
const runSettled =
  run !== null && run.status !== "preparing" && run.status !== "queued" &&
  run.status !== "starting" && run.status !== "running" &&
  run.status !== "waiting" && thread.runtime?.activeRunId !== run.runId;
return !(thread.interactionMode === "plan" && thread.hasActionableProposedPlan && runSettled);
```

Reading that literally: a thread is Working when the runtime is active **or** the latest run
is still in `preparing|queued|starting|running|waiting`, or the runtime still points at that
run — *unless* the thread is raising a hand (approval, question, subagent request), in which
case it is **not** Working; it returns to the inbox so it can be seen. A plan-mode thread with
an actionable proposed plan and a finished run is also not Working.

Then a hysteresis layer (`createInboxReturnTracker`, same file):

- `WORKING_EXIT_GRACE_MS = 2_000` — a thread that stopped working keeps reading as Working
  for 2 s, because an orchestrator run can end moments before its continuation starts.
- `STALE_EXIT_MS = 60_000` — no grace if the run ended more than 60 s ago (client was
  disconnected); the wide margin absorbs clock skew between client and server.
- Grace is suppressed by `actionable`, by any placement change (`archivedAt`, `pinnedAt`,
  `settledOverride`, `settledAt`, `snoozed*`, `activeOrderKey`), and while snoozed.
- Only shelf placement is held; execution state and attention requests stay authoritative.
- Working order is "last thread you sent work to first"
  (`sortWorkingThreadsBySend`, by `latestUserAuthoredMessageAt`).
- `asLaunchingThread` synthesises `runtime.status = "preparing"` for a thread the local client
  just created and whose run has not landed yet, so it is not listed twice.

### 2.2 Snooze

`packages/client-runtime/src/state/threadSettled.ts:157-172` — snoozed iff
`snoozedUntil > now` **and** the thread did not raise a hand after it was snoozed
(`threadRaisedHandWhileSnoozed`, lines 95-123). Raising a hand means: pending approval, a
non-`user_input` runtime request, a **new** question created after `snoozedAt` (snoozing an
existing question means "ask me later"), a pending subagent request, a pending guest message,
or a failure newer than `snoozedAt`.

Deliberately *not* a wake reason: run completion. A snoozed long-running thread keeps waking
itself for background work and stays hidden until its timer.

Server-side `isSnoozed` (`ThreadSettlementService.ts:168-198`) mirrors the client, so
auto-settlement and the sidebar agree. `canSnooze` refuses while approvals/questions/queued
turns/guest messages exist.

Presets are durations ("3 hours") or wall-clock times on a named day, resolved in the client's
local timezone with DST-safe calendar-day arithmetic (`addSnoozeDays`), deduped by wake minute,
and "today" options disappear once their time passes. Custom snooze is date+time or
amount+unit, rejecting DST-skipped local times.

### 2.3 Settling (server-side, survives apps being closed)

`ThreadSettlementServiceV2.sweep` runs on a schedule and on merge events.
`isAutoSettlementCandidate` (lines 148-165) refuses if **any** of:

```
archivedAt != null | settledOverride != null | pinnedAt != null |
autoSettleDisabledAt != null | pendingRuntimeRequest != null |
pendingGuestMessageCount > 0 | activityRunStatus != null |
backgroundWorkHoldsCompletion(pendingBackgroundTasks) |
any pending pull-request watch | hasQueuedTurnStart(...) | isSnoozed(...)
```

`resolveAutoSettlementAt` (200-259) then:

1. If any linked PR snapshot is unknown → don't settle.
2. If a linked PR is `open` → ignore PR outcome, only inactivity can settle.
   Otherwise take the most recent terminal link. If the PR merged (and
   `autoSettleOnMerge`) or closed after the thread's last user-authored message → settle
   immediately.
3. `autoSettleKeepPullRequests` and any PR link → never settle by inactivity.
4. Inactivity: `latest(latestUserMessageAt, latestRunRequestedAt, latestRunStartedAt,
   latestRunCompletedAt)`, floored at `createdAt`, adjusted by `lastSnoozeWakeAt` and an
   elapsed snooze timer. Settle if older than `autoSettleAfterDays` (default 7 days).

Extra rules from source:

- Only **user-authored** messages resume a thread; a turn the thread started itself for
  background work or a PR watch does not reset the clock.
- Subagent (child) threads settle with the parent and skip their own PR lookup.
- `hasQueuedTurnStart` holds settlement for ≤ 2 min after a user message that has not yet
  produced a run, bounded on both sides to survive clock skew.
- Settling closes terminals that sit at an idle prompt and keeps their output; running
  commands stay open unless the project action says `stopOnSettle` / `runOnSettle`.
- `thread.auto-settle` re-checks `snapshotAt` and refuses to override an explicit
  `settledOverride`, so a user action that races the sweep wins.

Manual overrides are an enum, not a boolean:
`settledOverride: null | "settled" | "active"` (`packages/contracts/src/orchestrationV2.ts:499`).
`"active"` means "user un-settled me, do not auto-settle until new activity resumes the rules".
Commands: `thread.settle`, `thread.unsettle`, `thread.auto-settle`, `thread.snooze`,
`thread.unsnooze`, `thread.auto-settle.set` (`orchestrationV2.ts:3085-3125`).

**Verdict — 2. Lifecycle: COPY.** The rule set is precise, testable, and already paid for
the hard edge cases (2 s grace, clock-skew bounds, hand-raise wakes, manual override enum);
implement it identically, with the client holding placement and the server holding
settlement.

---

## 3. Injecting T3's own MCP servers into each harness  ← the critical one

The shared design (documented in `docs/orchestration-v2/orchestrator-mcp-server.md`, code in
`apps/server/src/mcp/`):

- One **streamable-HTTP MCP endpoint**, path `/mcp`, on the same loopback server
  (`packages/../apps/server/src/mcp/McpHttpServer.ts:1312`, protocols
  `2025-06-18` and `2026-07-28`), server key `t3-code`.
- Per-provider-session **bearer credential** minted by `McpSessionRegistry.issue`
  (`apps/server/src/mcp/McpSessionRegistry.ts`), scoped to environment + thread + provider
  instance + provider session, with capabilities `preview` and `orchestration`. Token is
  stored only as SHA-256 (`hashToken`), expires after a liveness window
  (`DEFAULT_LIVENESS_WINDOW_MS = 24 h`, using max(wall, monotonic) elapsed so clock changes
  cannot extend it), and is revoked when the provider session is released.
- `McpProviderSessions` (`packages/provider-core/src/server/McpProviderSessions.ts`) maps
  `threadId → { endpoint, authorizationHeader, capabilities, browserToolsAvailable }` so an
  adapter can read the config at turn time.
- Tool exposure: a client that sends `t3-tool-exposure: deferred` gets only a core set plus
  `tool_search` (`McpHttpServer.ts:1340-1360`), because some agents cannot defer tools
  themselves.

Per-harness mechanisms — **these are different on purpose, and each difference is a
constraint, not a preference**:

### 3.1 Codex — per-thread `config` on `thread/start|resume|fork`

`apps/server/src/orchestration-v2/Adapters/CodexAdapterV2.ts:1445-1490`:

```ts
config: {
  ...CODEX_THREAD_CONFIG,
  [`shell_environment_policy.set.${T3CODE_THREAD_ID}`]: threadId,
  "mcp_servers.t3-code":          { url, tool_timeout_sec, http_headers: { Authorization } },
  "mcp_servers.t3-code-direct":  { ...same, enabled_tools: ["t3_thread_wait"],
                                   omit_tools_from: ["code_mode", "deferred"] },
}
```

- Only `mcp_servers.t3-code*` is overridden, so MCP servers the user put in their Codex
  launch arguments survive.
- Registered **twice** because Codex's "Code Mode" runs a blocking `t3_thread_wait` as a
  nested call inside a script, and the model keeps working around it. Making it a
  direct-only tool is a Codex-specific workaround.
- No CLI flag, no env var, no config file — everything rides the JSON-RPC request, so one
  shared app-server can serve threads with different credentials.

### 3.2 Claude — SDK query option → `mcp_set_servers` control request on stdin

`claudeMcpQueryOverrides` (`ClaudeAdapterV2.ts:2239-2266`):

```ts
mcpServers: { "t3-code": { type: "http", url, headers: { Authorization },
                           timeout: 65 * 60 * 1000 } },
allowedTools: [...existing, "mcp__t3-code__*"],   // read-only sandbox gets an explicit list
```

- The 65-minute timeout exists because T3 waits can block for an hour vs Claude's 60 s default.
- Read-only sandboxes pre-approve only an annotated read-only allow-list
  (`CLAUDE_READ_ONLY_T3_MCP_ALLOWED_TOOLS`, lines 2215-2234), not the wildcard.
- **Delivery mechanism matters:** the test
  `"sends MCP servers over stdin, not argv or the environment"` asserts the control request
  `{ subtype: "mcp_set_servers", servers: { "t3-code": … } }` arrives on stdin and that the
  spawn args contain **no** `--mcp-config` and no credential
  (`ClaudeAdapterV2.test.ts:2995-3018`). That is a deliberate choice: argv is world-readable
  via `ps`, and env leaks into every child process the agent spawns.

### 3.3 OpenCode — HTTP management API `mcp.add`

- 1.x (`packages/provider-opencode/src/server/adapter.ts:977-987`):
  `client.mcp.add({ name: "t3-code", config: { type: "remote", url, headers: { Authorization },
  oauth: false } })`.
- 2.x (`packages/provider-opencode/src/server/v2/adapter.ts:4153-4200`): the same call plus
  `location: { directory }`, and it re-adds on directory or credential change, removes on
  finalizer, and sleeps `MCP_TOOL_RELOAD_SETTLE` on versions `< 2.2.6` because those load a
  new server's tools 100 ms after connect.
- Skipped entirely for an external server (`connection.external`).
- Also injects an **orchestration system prompt** via
  `client.session.instructions.entry.put({ sessionID, key, ... })`, conditional on whether
  MCP is actually available (`t3OrchestrationSystemPrompt(mcpAvailable, hasDevice)`).

### 3.4 ACP — `session/new` `mcpServers`, with three possible shapes

`acpMcpContext` (`packages/provider-acp/src/server/adapter.ts:887-935`) builds all three and
`sessionMcpServers` (`AcpSessionRuntime.ts:2146-2165`) picks by what the agent advertised:

1. **stdio bridge (default).** T3 re-invokes *itself* as an MCP server:
   ```ts
   { name: "t3-code", command: self.command,
     args: [...selfInvocationArgs(self, ["acp-mcp-bridge"])],
     env: [ELECTRON_RUN_AS_NODE=1,
           T3_ACP_MCP_ENDPOINT=<http://127.0.0.1:port/mcp>,
           T3_ACP_MCP_AUTHORIZATION=<bearer>] }
   ```
   The bridge (`apps/server/src/mcp/AcpMcpStdioBridge.ts`) relays JSON-RPC **verbatim** to the
   HTTP endpoint, forcing `mcp-protocol-version: 2025-06-18` on follow-ups. ACP mandates
   stdio MCP; HTTP/SSE support varies.
   There is a second fast path, `acp-mcp-call`, spawned **per tool call** by agents that
   cannot hold a session (`apps/server/src/binMain.ts:4`).
2. **`acp` registry reference** if `agentCapabilities.mcpCapabilities.acp === true`
   (`{ type: "acp", name, serverId }`).
3. **HTTP** only if the flavor sets `mcpOverHttp: true` (Antigravity, ACP-registry) *and*
   `mcpCapabilities.http === true`; the comment records that `codex-acp 1.2.0` and
   `pi-acp` silently drop injected HTTP servers, which is why it is opt-in per flavor.

Also sets `T3CODE_THREAD_ID` plus `T3_ACP_MCP_*` into the agent's process env so tool
subprocesses can find their thread.

### 3.5 Pi — generated extension file + env

`packages/provider-pi/src/server/mcpInjection.ts`:

- Writes `pi-t3-mcp-extension.ts` into a server cache dir (atomic staging then rename),
  and spawns `pi --mode rpc --extension <cache>/pi-t3-mcp-extension.ts`.
- Passes `T3_MCP_URL=http://127.0.0.1:<port>/mcp` and `T3_MCP_BEARER_TOKEN=<bearer>` in env,
  and **deletes** `T3_MCP_URL` from the inherited env first (`mcpInjection.ts:297`).
- The extension implements the HTTP MCP JSON-RPC client itself, exposes tools as
  `mcp__t3-code__*`, and keeps hidden `mcp__t3_code__` aliases so a user-configured server
  cannot take the namespace.
- Rationale in `orchestrator-mcp-server.md`: Pi's native MCP has a 60 s default request
  timeout that would kill long T3 waits, and `mcp.json` entries override native
  registrations anyway.

### 3.6 Cursor / Muse

- Cursor: SDK `mcpServers` on `Agent.create`/`Agent.resume` **and each send**
  (`packages/provider-cursor/src/server/adapter.ts:211-226`, `2458-2493`); on
  `@cursor/sdk >= 1.0.39` send-level servers replace only the inline set, so ambient
  `settingSources` servers survive.
- Muse: JSON-RPC session config, `{ transport: "streamableHttp", mode: "optional", url,
  headers }`, and it hard-fails with "Update Muse Code…" if the host did not grant
  `sessionMcp` (`packages/provider-muse/src/server/adapter.ts:1933-1950`).

**Verdict — 3. Per-harness MCP injection: ADAPT.** Keep the exact matrix and the
"credential never in argv" rule, but h4rn must implement it itself: Claude via
`--mcp-config` (0600 temp file or inline JSON) + `--strict-mcp-config` since there is no
Rust Agent SDK, and every other harness via its native request field/API as tabulated.

---

## 4. Terminal view

### 4.1 What the code does

**Server (PTY).**
- `node-pty` loaded lazily through `createRequire` so the native addon stays external in
  single-executable builds (`apps/server/src/terminal/NodePtyAdapter.ts:8-40`). There is a
  patched `node-pty@1.2.0-beta.15` in `patches/`.
- Shell selection, `fish --no-query-term` to avoid startup waits (`docs/user/terminal.md`).
- Scrollback: **server keeps raw output text**, `DEFAULT_HISTORY_LINE_LIMIT = 5_000` and
  `DEFAULT_HISTORY_BYTE_LIMIT = 8 MiB` (`apps/server/src/terminal/Manager.ts:96-97`), in
  `BoundedTerminalHistory` — chunked, O(1) amortised trim, surrogate-pair-safe, and
  persisted to a log file with a tail read that skips UTF-8 continuation bytes
  (`Manager.ts:2058-2126`).
- **Multiple viewers share one PTY sized to the smallest viewer** (`Manager.ts:183-186`,
  `resizeSession`), so a narrow phone still sees the whole TUI.
- No server-side screen model, no alternate-screen handling beyond tracking
  `pendingHistoryControlSequence` (there is a 1049 round-trip comment at `Manager.ts:1106`).

**Transport.**
- Effect RPC streams `terminalAttach` / `terminalObserve` over WebSocket.
- Events are **JSON objects with text payloads** (`packages/contracts/src/terminal.ts:200-262`):
  `{ type: "output", data: string }`, plus `started|exited|closed|error|cleared|restarted|activity`.
  No binary frames.
- Attach sends a `snapshot` first: `{ snapshot: { history, cwd, worktreePath, label, … } }`,
  then live output. This is snapshot-on-(re)connect via **raw history replay**.
- Backpressure is a credit window: `MAX_PENDING_CHUNKS = 8`, `MAX_PENDING_BYTES = 64 KiB`
  per stream, one `Ack` per chunk while under the window, and a resync subscription swap
  when the queue overflows (`apps/server/src/terminal/OutputProtocol.ts:14-22`, 190-241).
- Grid changes ride on the `activity` event (`cols`/`rows`) because older clients reject
  unknown event types.

**Client.**
- **Not xterm.js.** `apps/web/src/terminal/ghostty/` is a browser adapter for the official
  `libghostty-vt` C ABI compiled to WASM (`ghostty-vt.wasm`), with a 112-byte
  `ghostty-write-pty.wasm` trampoline for terminal-generated PTY replies, and a **Canvas 2D**
  renderer batching backgrounds and style runs (`renderer.ts`). README: *"It is
  intentionally not an xterm compatibility layer."*
- `surface.ts` owns input, IME, selection, scrolling, sizing, links, cursor blink; no React
  state in the render loop.
- The repo pins Ghostty by revision in `native/libghostty-vt/VERSION` and rebuilds with Zig
  0.15.2 (`apps/web/scripts/build-libghostty-wasm.sh`).

### 4.2 Assessment

What T3 gets right:
- Raw-history replay is *exact*: the client is the only parser, so no server/client VT
  divergence is possible.
- Byte-bounded history with a real backpressure window instead of dropping output.
- Smallest-viewer resize is the correct answer for multi-device attach.
- Persisted history means a server restart still shows scrollback.

What it costs:
- Reconnect cost is O(history) through JSON strings — up to 8 MiB of escaping and a full
  client-side re-parse per attach.
- No binary framing means ~4/3 overhead on non-ASCII output and no room for a compact
  screen representation.
- Shipping a Zig→WASM VT is a heavy build dependency; h4rn would have to vendor it.
- `node-pty` is a native addon that must be rebuilt per ABI and unpacked out of
  single-file bundles; this is the exact class of problem a Rust daemon removes.

### 4.3 Recommendation for h4rn

**Use `portable-pty` + xterm.js with the WebGL addon, binary frames, and a server-side
snapshot — i.e. copy T3's semantics, replace its two heavy dependencies.**

Concretely:

1. **PTY:** `portable-pty` in the Rust daemon. It is pure-ish Rust (ConPTY on Windows,
   `forkpty` elsewhere), no Node ABI, no spawn-helper chmod dance.
2. **Server state:** keep the byte ring buffer *and* add a headless VT (`alacritty_terminal`'s
   `Term`, or `vt100`, or FFI to `libghostty-vt` if parity with T3's emulation is required)
   to maintain a screen + scrollback grid. The grid lets reconnect send a **compact screen
   snapshot + the last N lines**, not 8 MiB of history.
3. **Wire:** WebSocket **binary** frames, fixed header e.g.
   `[u8 kind][u16 terminal][u32 seq][u32 len]`, coalesced on a ~16 ms tick. Keep T3's credit
   window (8 chunks / 64 KiB) — it is the right size and prevents one chatty build from
   starving the RPC stream.
4. **Reconnect:** snapshot carries `(screen_cells, scrollback_tail, seq)`; client resets its
   xterm instance and applies the snapshot rather than replaying history. Fall back to raw
   replay only for a cold attach where the server has no retained grid.
5. **Resize:** copy smallest-viewer-wins, and include `cols/rows` in the snapshot so a
   reconnecting viewer cannot race a resize.
6. **Client:** xterm.js + `@xterm/addon-webgl` (fallback to canvas), `addon-fit`,
   `addon-unicode11`, `addon-web-links`. It is mature, handles IME/selection/links, and a
   Tauri webview runs it without a WASM build pipeline. Keep T3's "no React state in the
   render loop" rule.

**Verdict — 4. Terminal: ADAPT.** Copy layout semantics, scrollback bounds, backpressure and
smallest-viewer resize; replace node-pty + Ghostty-WASM with portable-pty + headless VT +
xterm.js/WebGL and binary frames with snapshot-on-reconnect.

---

## 5. Attachments

### 5.1 Storage

`apps/server/src/attachmentStore.ts`:

- Files live in `config.attachmentsDir`, one directory per environment, **flat filenames**
  derived from the attachment id: `<threadSegment>-<uuid>[-<ext>]`.
- Ids are deterministic where it matters: `createDeterministicAttachmentId(threadId,
  chatAttachmentStableKey(messageId, index, sha256(bytes)))`. Comment: *"Retries keep ids;
  changed bytes must get new ids because clients cache images by id."*
- `attachmentRelativePath` derives the extension from mime for images and from the original
  filename for files, rejecting `.part` and anything not `^\.[a-z0-9]{1,10}$`.
- Path traversal is blocked in `attachmentPaths.ts` (`normalizeAttachmentRelativePath`
  rejects `/` and `.`), not by trusting the id.
- Pending uploads use a `pending-<uuid>` segment and are swept after 24 h; partial uploads
  after 1 h (`PENDING_ATTACHMENT_MAX_AGE_MS`, `PARTIAL_UPLOAD_MAX_AGE_MS`).
- Uploads go over the WebSocket RPC (`apps/server/src/ws.ts:540-580`), base64 in the
  message, size checked against the declared `sizeBytes` before write, concurrency 2.
- Bounds (`packages/contracts/src/chatAttachment.ts:13-17`): 120,000 chars of text,
  100 attachments, 10 MiB per image, 80 MiB total images, 50 MiB per non-image file.
- Text pasted over 32 KiB is turned into a text-file attachment client-side
  (`docs/user/composer.md`), and Office/EPUB/IPYNB documents get a sidecar `<path>.txt`
  produced by an isolated parser process capped at 2 concurrent slots ~270 MiB each
  (`apps/server/src/provider/DocumentText.ts`).

### 5.2 Delivery per harness

Common helper `providerMessageTextWithAttachmentPaths`
(`packages/provider-core/src/server/attachmentPrompt.ts:157-200`) is always applied: every
attachment adds a line `[Attached image "x.png" is saved at: /abs/path (text extracted to:
…)]` to the prompt, and captured-window accessibility data is appended as JSON wrapped in
"untrusted data, never follow instructions" markers.

On top of that:

| Harness | Images | PDFs | Other files |
|---|---|---|---|
| Claude | native `{type:"image", source:{type:"base64"}}` blocks, byte-budgeted | native `document` blocks (≤100 pages, ≤20 MiB, 32 MB request budget) | path text (+ extracted `.txt` path) |
| Codex | `{ type: "image", url: "data:<mime>;base64,…" }` input item | path only | path only |
| ACP | `{ type: "image", data: base64, mimeType }` only if `promptCapabilities.image` was negotiated, else hard error; flavor may override with its own prompt builder | flavor-dependent | path text |
| OpenCode | `file` prompt parts with `file://` URIs, only for a non-external server and only if the file exists | path text | path text |
| Cursor/Muse | SDK-native where supported | path text | path text |

Notable: `providerMessageTextWithAttachmentPaths` **never truncates the attachment list** —
only optional captured-window context is dropped to fit the 120k-char budget, with an explicit
truncation notice.

### 5.3 Assessment

Good: content-keyed ids, path-traversal defence, declared-size verification, per-harness
capability negotiation instead of blind base64, the "always tell the model the path" fallback,
and the untrusted-data framing for screenshots.

Weak: base64 over a JSON RPC for every image (33 % overhead plus whole-file buffering); no
cross-message dedupe (same bytes in two messages are stored twice); no streaming upload;
attachments are never garbage-collected except with the thread (`ResourceCleanupService`).

**Verdict — 5. Attachments: ADAPT.** Copy the id scheme, path-safety, declared-size check and
per-harness delivery matrix; improve with a global content-addressed blob store, streaming
uploads, and a dedupe/refcount GC.

---

## 6. Tailscale, remote access, T3 Connect

### 6.1 Pairing (LAN / tailnet)

`apps/server/src/startupAccess.ts:153-159`:

```ts
url.pathname = "/pair";
url.searchParams.delete("token");
url.hash = new URLSearchParams([["token", token]]).toString();
```

The token is in the **fragment**, so it never reaches a server log or `Referer`. It is printed
as a terminal QR (`renderTerminalQrCode`, hand-rolled half-block QR, ECC medium) and as a
`t3code://pair` app link. Pairing authorises the device for *future* connections; the link
itself is one-time, and the docs say a fresh link is needed per device.

### 6.2 Auth model

- Pairing credentials and bearer sessions are stored hashed, with **DPoP** proof-of-possession
  (`apps/server/src/auth/dpop.ts`, `apps/web/src/cloud/dpop.ts`, DB migration
  `032_AuthPairingProofKeyThumbprint`). The browser keeps its DPoP private key in IndexedDB
  (`t3code:cloud-auth`).
- Scopes are explicit and numerous (`token:operate`, `orchestration:operate|read`,
  `filesystem:read|write`, `source-control:write`, `relay:read`, …). Without `--scope` a
  pairing token gets everything, including access management — documented as a footgun.
- MCP clients get a restricted `mcp-client` subject confined to `/mcp`
  (`EnvironmentAuth.ts:76-82`).

### 6.3 T3 Connect (the relay)

- **Account-based**, not a peer-to-peer tunnel: `t3 up --connect` / `t3 connect link`, or
  Settings → Sharing & access on a desktop host.
- Relay API is a typed HttpApi (`packages/contracts/src/relay.ts`): `/v1/client/environment-links`,
  `/v1/environments/:id/tunnel`, `/v1/environments/:id/hooks/wake`, device registration,
  APNs/Live-Activity push, agent-awareness publish, `/health`, OAuth discovery endpoints.
- Linking is a **challenge → environment-signed JWT proof** exchange
  (`RelayEnvironmentLinkChallengeRequest/Response`, `RelayEnvironmentLinkProofPayload`
  carrying `descriptor`, `environmentPublicKey`, `endpoint`, `origin`, `scopes`).
  Access tokens are DPoP-bound (`RelayExchangeDpopAccessTokenEndpoint`), short-lived, and
  renewed without dropping a healthy connection.
- Transport is a **managed Cloudflare tunnel**: the environment spawns
  `cloudflared tunnel --no-autoupdate --loglevel info --output default --metrics
  127.0.0.1:0 [--protocol http2] run` with `TUNNEL_TOKEN` in env
  (`apps/server/src/cloud/ManagedEndpointRuntime.ts:756-780`), plus recovery/registration
  endpoints so a restarted host reuses the same hostname. Idle tunnels are released on
  shutdown (Cloudflare bills per provisioned tunnel) and re-provisioned on wake.
- `RelayManagedEndpointProviderKind = "manual" | "cloudflare_tunnel" | "t3_relay"`, so the
  relay design is at least nominally pluggable.
- Guest links ride on it: a 24 h, one-browser link that shows a thread read-only-ish and
  queues guest messages for the owner to Send/Dismiss (`docs/user/guest-links.md`).
- **No application-level encryption exists in the code.** There is no E2E/sealed-box/noise
  layer anywhere in `packages/contracts/src`, `apps/server/src`, `packages/client-runtime/src`
  or the docs. Confidentiality is TLS to Cloudflare/relay plus DPoP-bound scoped tokens;
  the relay operator (and Cloudflare) can see plaintext at the tunnel hop.

### 6.4 Tailscale

`packages/tailscale/src/tailscale.ts` shells out to the `tailscale` CLI (never the API):

- probes with `tailscale status` (3.5 s timeout, 500 ms SIGTERM→SIGKILL grace) and
  `tailscale debug prefs`,
- enables HTTPS with `tailscale serve --bg --https=<port> http://<host>:<localPort>`,
- parses routes from `tailscale serve status`, supports TCP forwards without a mount,
- classifies stderr into `no-existing-handler | not-logged-in | not-running |
  permission-denied | unsupported-target | unknown` **specifically so raw stderr (which can
  contain `tskey-…` and node names) is never logged**,
- 100.64.0.0/10 is treated as "VPN, maybe Tailscale" and only reported as Tailscale when the
  machine confirms it — because WARP shares that range.

**Verdict — 6. Remote access: ADAPT.** Copy fragment-token pairing + QR, DPoP-bound scoped
sessions, and the tailscale-CLI-with-classified-stderr approach; do **not** copy the
Cloudflare-tunnel dependency — run h4rn-connect's own relay (QUIC/WSS) and add the E2E
encryption T3 lacks.

---

## 7. Other things worth copying, and things to reject

### 7.1 Copy

- **Event-sourced orchestration with a durable effect outbox.**
  `EventStoreV2.append({ commandId, events })` → stored events with monotonic sequence
  (`apps/server/src/orchestration-v2/EventStore.ts`); `EffectOutbox` persists provider side
  effects (`provider-turn.start|interrupt`, `provider-session.detach`, `apply-model-selection`)
  so a restart resumes instead of losing the turn. Commands are idempotent by `commandId`
  with receipts (`CommandReceiptStore.ts`), and duplicates return the same stored sequence.
  This is the single most valuable idea in the repo.
- **Snapshot + cursor resume.** `subscribeOrchestrationV2Thread(afterSequence)` replays up to
  `THREAD_RESUME_MAX_REPLAY_EVENTS = 128` events, and falls back to a bounded snapshot on
  overflow/pruning/gap (`apps/server/src/ws.ts:892-1110`). The snapshot window is
  `maxUserTurns: 10`, `maxItems: 75`, `maxTurnItems: 500`, `maxEncodedBytes: 1 MiB`
  (`threadHistoryPaging.ts:14-19`). Copy the *policy*, not just the mechanism.
- **Run vs provider turn vs execution node.** `AppThread → Run → root ExecutionNode →
  {tool, approval, subagent}`; only the root node completes the run; child
  `turn/completed` never closes the parent. It fixes the "subagent finished so the thread
  says Done" bug by construction.
- **Checkpoints as git objects, not stashes.**
  `refs/t3/orchestration-v2/checkpoints/<scope>/<ordinal>` created with a temp index +
  `git write-tree` + `git commit-tree -p HEAD` + `update-ref`
  (`apps/server/src/vcs/GitVcsDriver.ts:1310-1365`, `CheckpointService.ts:26`). Cheap,
  content-addressed, GC-able, and it does not disturb the user's index.
- **Raw provider diagnostics with rotation and redaction.** Native frames are logged to
  rotating NDJSON with `Authorization`/`apiKey`/`token`/`password`/`secret` keys redacted and
  structured `{ provider, protocol, kind, providerSessionId, event }`
  (`EventNdjsonLogger.ts`, `redactCodexProtocolValue`). Makes replay tests possible by
  replacing only the transport.
- **AgentScope.** systemd user-scope wrapping per thread on Linux (so `SIGKILL` of a scope
  works even when the kernel is throttling the process), `oom_score_adj=800` for agents,
  and `shared` scopes for processes serving many threads
  (`packages/shared/src/AgentScope.ts`). h4rn should do the cgroup equivalent natively.
- **`T3CODE_THREAD_ID` in every agent shell environment** so the server can map a listening
  port back to a thread (`PortScanner.AGENT_THREAD_ENV`). Small, high leverage.
- **Append-only "returned to you" ordering** instead of mutable sort keys: `skipped sort
  keys`, `lastVisitedAt`, `unsettledAt`, and a client-side return tracker.

### 7.2 Adapt

- **Orchestrator MCP tool surface** (`delegate_task`, `task_status`, `task_cancel`,
  `t3_thread_launch`, `t3_thread_read|wait`, `html_preview`, `html_render`, worktree and PR
  tools). The tool *set* is good; the implementation is 3,403 lines in one file
  (`OrchestratorMcpService.ts`) and should be split per toolkit in Rust.
- **Portable context handoff between providers** (`ContextHandoffService.ts`,
  `docs/orchestration-v2/provider-switching-and-context.md`) — worth having, but start with
  "fork creates lineage; first run resolves natively or by summary".
- **Diff view.** The server exposes checkpoint diffs and per-run diffs; the client renders
  with `@pierre/diffs` plus its own file tree, comment annotations, scroll restore and test-
  file hiding (`apps/web/src/components/DiffPanel.tsx`, `components/diffs/`). Copy the data
  model (diff scoped to checkpoint/run, not to "working tree"), pick a Rust/JS renderer
  later.

### 7.3 Reject

- **Two parallel harness layers (V1 provider drivers + V2 adapters).** Pure legacy tax.
- **13k-line single-file adapters** (Claude) and **8.5k-line** Codex adapters. Split by
  concern: transport, normalizer, policy, projection.
- **Electron shell and the Effect-TS dependency graph** — not portable to Tauri/Rust, and the
  Effect service-layer ceremony is a large share of the reading cost.
- **Ghostty-WASM terminal pipeline** for h4rn v1 (see §4.3).
- **Cloudflared as the only managed transport**, and the absence of E2E encryption.
- **`--scope`-less tokens granting full access** by default.
- **`structuredClone`/JSON event payloads for terminal output** — binary or nothing.

---

## 8. Concrete implications for h4rn (short)

1. Define the **event/command/outbox** core first; every harness adapter should be a pure
   translator into `provider.*` events. T3's V2 refactor is exactly this and it is the reason
   its Claude/Codex bugs are diagnosable.
2. Define the **per-harness injection table** (§1/§3) as data, not code: a `HarnessSpec` with
   `{ transport, resume, approval, interrupt, mcp: { mechanism, placement } }`. T3 hard-codes
   this knowledge in seven adapters; h4rn can make it a table with per-harness escape hatches.
3. Build **one MCP server** and treat *how it is delivered* as a per-harness detail:
   request field (Codex, ACP), HTTP API (OpenCode), SDK option (Cursor), generated extension
   (Pi), control message or 0600 config file (Claude).
4. Steal the **process-group watchdog + fd-3 liveness pipe** for every agent process. It is
   20 lines of shell and it eliminates the entire orphaned-agent class of bug.
5. Thread lifecycle: client owns placement, server owns settlement, and both share one
   predicate set. Put those predicates in a shared crate with property tests over clock skew.

---

## Appendix — file index for follow-up reading

| Topic | Path |
|---|---|
| V2 architecture docs | `docs/orchestration-v2/README.md`, `feature-lifecycles.md`, `orchestrator-mcp-server.md` |
| Claude adapter | `apps/server/src/orchestration-v2/Adapters/ClaudeAdapterV2.ts` |
| Codex adapter | `apps/server/src/orchestration-v2/Adapters/CodexAdapterV2.ts` |
| Codex JSONL protocol | `packages/effect-codex-app-server/src/protocol.ts` |
| OpenCode runtime / v2 adapter | `packages/provider-opencode/src/server/OpenCodeRuntime.ts`, `.../v2/adapter.ts` |
| ACP runtime / adapter | `packages/provider-acp/src/server/AcpSessionRuntime.ts`, `adapter.ts` |
| ACP stdio MCP bridge | `apps/server/src/mcp/AcpMcpStdioBridge.ts` |
| Pi MCP extension | `packages/provider-pi/src/server/mcpInjection.ts`, `mcpExtensionSource.ts` |
| MCP HTTP server / registry | `apps/server/src/mcp/McpHttpServer.ts`, `McpSessionRegistry.ts` |
| MCP session config | `packages/provider-core/src/server/mcpSession.ts`, `McpProviderSessions.ts` |
| Attachments | `apps/server/src/attachmentStore.ts`, `packages/provider-core/src/server/attachmentPrompt.ts` |
| Terminal server | `apps/server/src/terminal/Manager.ts`, `NodePtyAdapter.ts`, `OutputProtocol.ts` |
| Terminal client | `apps/web/src/terminal/ghostty/README.md`, `surface.ts`, `renderer.ts` |
| Lifecycle (client) | `packages/client-runtime/src/state/threadInbox.ts`, `threadSettled.ts` |
| Settlement (server) | `apps/server/src/orchestration-v2/ThreadSettlementService.ts` |
| Event store / outbox | `apps/server/src/orchestration-v2/EventStore.ts`, `EffectOutbox.ts`, `CommandReceiptStore.ts` |
| Checkpoints | `apps/server/src/orchestration-v2/CheckpointService.ts`, `apps/server/src/vcs/GitVcsDriver.ts` |
| Pairing / auth | `apps/server/src/startupAccess.ts`, `apps/server/src/auth/EnvironmentAuth.ts`, `auth/dpop.ts` |
| Relay contracts / tunnel | `packages/contracts/src/relay.ts`, `apps/server/src/cloud/ManagedEndpointRuntime.ts` |
| Tailscale | `packages/tailscale/src/tailscale.ts` |
| Agent process scope | `packages/shared/src/AgentScope.ts` |
