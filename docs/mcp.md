# MCP hub

h4rn runs exactly one MCP hub per daemon and injects it into every harness session as the single
entry `h4rn` (ADR-006). This document describes what Phase 4 ships: the loopback streamable-HTTP
endpoint, per-session tokens, upstream transports, native-config import, cross-machine sharing and
the security boundaries around secrets. The design is recorded in
[ADR-036](decisions.md#adr-036-mcp-is-implemented-in-tree-with-server__tool-names-and-deterministic-collisions)
and [ADR-037](decisions.md#adr-037-mcp-configuration-and-secrets-live-apart-sharing-is-owner-executed).

## The endpoint

- `POST /mcp` implements MCP revision `2025-06-18` (with `2025-03-26` and `2024-11-05` accepted for
  older harnesses). It speaks JSON-RPC in, JSON-RPC out; batches are accepted and notifications get
  `202 Accepted`.
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
- `confirm_tools` (and `confirm_all`) require an approval on the owning machine before each call;
- `allowed_devices: None` admits the target machine's paired device only;
- every remote call is audit-logged locally with the authenticated device key, the machine, the
  server, the tool and the outcome.

The unknown machine dials the owner's `/secure-mcp` route over the same Noise transport as
`/secure` (a second secure session; multiplexing is not required). After the internal handshake
the tunnel carries JSON-RPC frames. The owner re-authorizes every call, strips namespaces on
`tools/list` and re-adds them on `tools/call`, and never forwards credentials: the upstream server
runs on the owner.

Phase 4 registers remote bindings at runtime (`McpHub::register_remote`); the daemon's tunnel
client is `TransportMcpTunnel`. A proxied server cannot be re-shared.

## Protocol operations

`ClientRequest::Mcp { request }` carries every operation:

`ListServers`, `Upsert`, `Remove`, `SetEnabled`, `Import`, `ApplyImport`, `ResolveConflict`,
`Share`, `Unshare`, `ListTools`, `TestCall`.

The legacy `McpList`, `McpUpsert` and `McpRemove` requests are mapped onto `ListServers`, `Upsert`
and `Remove` for older clients. Responses use `McpServers`, `McpServer`, `McpImportPreview`,
`McpTools`, `McpCall`, or `Ack`; errors use the standard protocol error shape.

## Extension point

h4rn's own tool sets register a `ToolProvider` with `McpHub::register_provider`. A provider owns a
namespace, lists tools for a scope, and executes calls. The built-in `EchoProvider` exposes
`h4rn__echo` (and `h4rn__server_info`); subagent, preview/render and forge tools plug in the same
way without changing the hub, the HTTP layer or any adapter.

## Tests

- `crates/h4rn-mcp/tests/mcp_stdio.rs` starts a real stdio MCP fixture process behind the HTTP hub
  and covers auth, negotiation, namespacing, tool calls, resources, prompts and token scopes.
- `crates/h4rn-daemon/tests/mcp.rs` covers the protocol operations, secret non-leakage in the
  SQLite registry and views, and two daemons proxying a shared server over the encrypted tunnel.
- `crates/h4rn-daemon/tests/e2e_mcp.rs` (ignored by default) injects the real endpoint into Claude
  Code and Codex and asserts the model calls `h4rn__echo`:

  ```text
  cargo test -p h4rn-daemon --test e2e_mcp -- --ignored --nocapture
  ```
