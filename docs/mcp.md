# MCP hub

h4rn runs exactly one MCP hub per daemon and injects it into every harness session as the single
entry `h4rn` (ADR-006). This document describes what Phase 4 ships: the loopback streamable-HTTP
endpoint, per-session tokens, upstream transports, native-config import, cross-machine sharing and
the security boundaries around secrets. The design is recorded in
[ADR-036](decisions.md#adr-036-mcp-is-implemented-in-tree-with-server__tool-names-and-deterministic-collisions)
and [ADR-037](decisions.md#adr-037-mcp-configuration-and-secrets-live-apart-sharing-is-owner-executed).

## The endpoint

- `POST /mcp` implements MCP revision `2025-06-18` (with `2025-03-26` and `2024-11-05` accepted for
  older harnesses). It speaks JSON-RPC in, JSON-RPC out; one message per request, as the 2025-11-25
  transport specifies, and notifications get `202 Accepted`. A batch is refused.
- A request whose `Origin` is another site, or whose `Host` is not a loopback name, is refused: the
  endpoint carries a bearer token and must not answer a page that reached it by rebinding.
- `GET /mcp` returns `405`: the hub does not open a server-initiated stream, so a daemon restart
  only invalidates tokens rather than streams.
- `DELETE /mcp` revokes the caller's token.
- `initialize` binds the token to an `Mcp-Session-Id`; later requests may present it and a mismatch
  is `404`.
- The hub advertises tools, resources and prompts, with `listChanged` notifications for each.

The endpoint URL (`http://127.0.0.1:<port>/mcp`) and the bearer token are passed to the harness by
the adapter:

| Harness | Delivery |
| --- | --- |
| Claude Code | `--mcp-config <0600 tempfile> --strict-mcp-config`, server name `h4rn` |
| Codex | per-thread `mcp_servers.h4rn` config with `http_headers.Authorization` |
| OpenCode | `PUT /api/experimental/mcp/h4rn` management API |
| Antigravity | ACP `mcpServers` over HTTP when advertised, `mcp-remote` stdio otherwise |

The token never appears in argv. It is minted per thread (`h4rn_mcp_` prefix), stored only as a
BLAKE3 digest, expires after 24 hours, and is revoked when the thread's session ends.

## Scopes

Every token carries an `McpScope`:

- `harness` — which adapter owns the session, and therefore which per-harness enable switches
  apply;
- `machine` — the host whose per-machine enable switches apply;
- `thread_id` — attribution for events and audit;
- optional `allowed_servers` and `allowed_tools` — a narrower view than the enable matrix;
- `caller` — harness, local client or remote machine.

`tools/list`, `tools/call`, `resources/*` and `prompts/*` all apply the same scope. A tool that is
filtered out of `tools/list` cannot be called.

## Namespacing

Model-visible names are `server__tool`. Server names are lowercased and may not contain `__`;
`h4rn` is reserved for the hub's own tools. If two servers expose the same base name, the second is
allocated `_2`, then `_3`, deterministically by sorted server namespace. h4rn's own tools share the
same namespace, so a user server called `h4rn` is rejected rather than shadowing `h4rn__echo`.

Resources are namespaced by wrapping the original URI:
`h4rn-resource://<server>/<hex(original)>`. Prompt names are `server__prompt`. Both are unwrapped
before the upstream call.

## Upstreams

The registry persists stdio, streamable-HTTP and legacy HTTP+SSE servers in `mcp.db`:

- **stdio** — command, args, cwd and environment. The child is started lazily on first use, its
  stderr is drained, and it is killed when the hub stops.
- **streamable HTTP** — `POST` with `Accept: application/json, text/event-stream`; both plain JSON
  and short SSE responses are decoded; `Mcp-Session-Id` and `MCP-Protocol-Version` are handled.
- **legacy SSE** — one `GET` stream plus the announced message endpoint.

Shared behaviour:

- lazy start and capability caching;
- health ticks and restart with exponential backoff (500 ms doubling to 30 s);
- `notifications/tools/list_changed`, `resources/list_changed` and `prompts/list_changed`
  invalidate the cache and are forwarded to sessions;
- request and tool-call timeouts, plus output caps that cut on UTF-8 boundaries;
- cancellation via `notifications/cancelled`;
- upstream errors are redacted before they reach a view or a log line.

The manager keeps one status per server (`stopped`, `starting`, `running`, `error`) with a cached
tool count and the last redacted error. Status and tool-call events are forwarded to protocol
clients subscribed to `Topic::Mcp` as the transient `McpServerStatus` and `McpToolCalled` events.

## Native configuration import

`Import` reads each harness's own files and returns a preview instead of writing:

| Harness | Files |
| --- | --- |
| Claude Code | `~/.claude.json`, `<workspace>/.mcp.json` |
| Codex | `~/.codex/config.toml`, `[mcp_servers]` |
| OpenCode | `~/.config/opencode/opencode.json`, `<workspace>/opencode.json` |
| Antigravity | `~/.gemini/config/mcp_config.json` |

The preview separates entries that can be imported from conflicts (same name, different
configuration). `ApplyImport` writes only the non-conflicting candidates. A conflict needs an
explicit `ResolveConflict` with one of `KeepMine`, `UseTheirs`, `Rename { name }` or `Skip`.
Imported secret values are moved into the secret store; the registry keeps references.

## Secrets

The registry never holds a value. Every environment variable and HTTP header is stored as a
reference such as `mcp/srv_<id>/env/GITHUB_TOKEN` in `mcp.db`; the value lives in the OS keychain
through the `SecretStore` trait. When no keychain is available (headless Linux, WSL), the fallback
is an encrypted file (`mcp-secrets`, ChaCha20-Poly1305 with a `0600` sibling key). Neither the
protocol views nor `Debug` output nor status messages include values. A secret is deleted when it
is removed from a server or the server itself is removed.

## Cross-machine sharing

A server can be shared with another machine with `Share { name, to_machine, policy }`. The policy
is enforced by the **owner**:

- `allowed_tools: None` allows every tool; `Some(list)` is authoritative;
- `confirm_tools` (and `confirm_all`) require an approval on the owning machine before each call.
  The owner's daemon raises a pending card showing the caller, `server__tool` and a bounded
  arguments preview. It is answered with `RespondApproval { approval_id, decision }` by a client
  holding the `approve` scope; `PendingApprovals` re-reads the set after a reconnect. An approval
  expires after two minutes, is cancelled when the tunnel drops, and fails closed on either. A
  grant change — unshare, policy narrowing, server removal or peer revocation — withdraws the
  matching cards. After the answer, and again immediately before dispatch, the owner re-reads the
  grant and re-checks the authenticated peer, so a call whose grant changed while its card was up
  is refused (`GrantChanged`) instead of running;
- `allowed_devices` holds **connect device ids of peer daemons**, not ids of phones or other user
  devices. `None` admits the target machine's own daemon only. `Some(list)` compares the device id
  the owner recorded on that peer daemon's `host` registry entry when the peer was discovered
  (the `device_id` column of the daemon device in Settings → Devices), never a value claimed in
  the handshake frame;
- every protected operation on a persistent tunnel — `tools/list`, `tools/call`,
  `resources/list`/`read` and `prompts/list`/`get` — passes one guard before
  dispatch. It re-checks the authenticated peer, the current share and `allowed_devices` on the
  request itself, so unsharing, narrowing the policy or restricting devices applies to a tunnel
  that is already open, not only to the next `tools/call`;
- every remote call is audit-logged locally with the authenticated device key, the machine, the
  server, the tool and the outcome.

The caller is authenticated by its Noise key alone. The owner's device registry must list that key
as a peer daemon (`host` role) for the target machine; a user device key (even one with every
scope) is refused, and the `machine` / `device_id` in the handshake frame are checked against the
registry and refused on any disagreement.

The target machine (B) is a sibling daemon of the same account. It reaches the owner (A) over the
same Noise transport as `/secure`, on A's `/secure-mcp` route, authenticating as **its own daemon
key**. After the internal handshake the tunnel carries JSON-RPC frames. The owner re-authorizes
every protected request (tools, resources and prompts), strips namespaces on `tools/list` and
re-adds them on `tools/call`, and never forwards
credentials: the upstream server runs on the owner. A proxied server cannot be re-shared.

The owner does not stop reading the tunnel while a call waits for approval: a pending card never
blocks the next `tools/call` on the same tunnel, and at most eight calls run at once. When every
path to the caller is gone, that tunnel's pending cards are withdrawn through
`mcp_share_approval_resolved` with decision `cancel`, and the calls still in flight are dropped.

### How a share reaches the target

1. **Peers.** Each daemon keeps a `PeerDirectory`: for every other daemon it may talk to, the host
   id a share names, the pinned Noise key, its connect machine id and its relay address, plus any
   `ip:port` endpoints (LAN, tailnet). A signed-in daemon fills it from h4rn-connect (the account's
   daemon devices and machines), re-reading it when the relay reports a snapshot, presence change,
   pairing or revocation. Embedders can add peers with `PeerDirectory::replace_source`. Peer keys
   are admitted to, and revoked from, the secure transport allowlist by the directory. Admission
   is decided from the registry: a peer is skipped only when a `host` entry for its key exists and
   the transport already allows it. A failed `register_host` write is retried on the next peer
   report with bounded backoff, and a `client` key is never re-roled to make it fit.
2. **Grant discovery (pushed).** B opens a `grants` session on A's `/secure-mcp`
   (`{"h4rn_mcp_grants": "h4rn-mcp-grants-1"}`). A derives B's machine from the verified key,
   sends the full set of grants for it, and sends a new snapshot whenever a share is added, changed
   or removed, a server is removed, or B stops being a peer. Nothing polls. If the session drops, B
   unregisters everything from that peer and reconnects with backoff (500 ms to 30 s); the fresh
   snapshot is the reconciliation.
3. **Registration.** For each granted server B builds a `TransportMcpTunnel` from every candidate
   path and calls `McpHub::register_remote`. The multipath engine probes direct LAN and tailnet
   sockets first and ranks the relay last, so a relay-only pair still connects. B's harnesses then
   see `server__tool` like any other server, and B's `ListServers` shows it with `runs_on` set to
   the owner. If the name is already taken on B (a local server or another peer's), it is
   registered as `<name>-<owner host>`.
4. **Teardown.** `Unshare`, removing the server on A, revoking B's device or machine (the peer
   disappears from A's directory, which also revokes its key and closes live sessions), a dropped
   connection, or the owner leaving B's directory all call `McpHub::unregister_remote`.

### Relay path

When two machines share no LAN or tailnet route, the tunnel uses the same h4rn-connect relay as
the client protocol. A daemon cannot open a second relay WebSocket (connect keys a peer by device
id, so the second socket would evict the control connection), so peer sessions are multiplexed as
new `conn_id`s over the daemon's existing authenticated socket. Each `RelayOpen` carries an
explicit `service`: `secure` is the original client route, and `secure_mcp` reaches only the
peer-only `/secure-mcp` listener.

Connect admits a daemon-initiated session only when the target machine belongs to the same
account and is not the caller's own machine, and only when its daemon is online. The owner still
authorizes the actual caller by the Noise key registered as a `host`; the relay check narrows who
may ask, it does not grant access. User devices are refused `secure_mcp` outright.

Phones are clients and never run servers or hold shares. Revocation of a sibling daemon is
noticed on the next relay event, not instantly.

## Protocol operations

`ClientRequest::Mcp { request }` carries every operation:

`ListServers`, `Upsert`, `Remove`, `SetEnabled`, `Import`, `ApplyImport`, `ResolveConflict`,
`Share`, `Unshare`, `PendingApprovals`, `RespondApproval`, `ListTools`, `TestCall`,
`ListOutsideAgents`, `RevokeOutsideAgent`, `AgentTools`, `SetAgentToolsEnabled`.

The legacy `McpList`, `McpUpsert` and `McpRemove` requests are mapped onto `ListServers`, `Upsert`
and `Remove` for older clients. Responses use `McpServers`, `McpServer`, `McpImportPreview`,
`McpTools`, `McpCall`, `OutsideAgents`, `AgentTools`, or `Ack`; errors use the standard protocol
error shape.

## Extension point

h4rn's own tool sets register a `ToolProvider` with `McpHub::register_provider`. A provider owns a
namespace, lists tools for a scope, and executes calls. The built-in `EchoProvider` exposes
`h4rn__echo` (and `h4rn__server_info`); subagent, preview/render and forge tools plug in the same
way without changing the hub, the HTTP layer or any adapter.

The thread-scoped PR tools and `thread_status` are the `AgentToolsProvider` in
`crates/h4rn-daemon/src/mcp_server/agent_tools.rs`. Outside agents use the same `/mcp` route with
an OAuth token and a smaller tool set; see [outside-agents.md](outside-agents.md).

## Tests

- `crates/h4rn-mcp/tests/mcp_stdio.rs` starts a real stdio MCP fixture process behind the HTTP hub
  and covers auth, negotiation, namespacing, tool calls, resources, prompts and token scopes.
- `crates/h4rn-daemon/tests/mcp.rs` covers the protocol operations, secret non-leakage in the
  SQLite registry and views, and two daemons proxying a shared server over the encrypted tunnel.
- `crates/h4rn-daemon/tests/mcp_peering.rs` boots two daemons with no hand-built tunnel, shares a
  real stdio server, and asserts it appears on B, is callable, follows a policy change and
  disappears on unshare and on peer removal.
- `crates/h4rn-daemon/tests/e2e_mcp.rs` (ignored by default) injects the real endpoint into Claude
  Code and Codex and asserts the model calls `h4rn__echo`:

  ```text
  cargo test -p h4rn-daemon --test e2e_mcp -- --ignored --nocapture
  ```
