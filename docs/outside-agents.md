# Agents and MCP

h4rn is an MCP server in two directions. Agents h4rn runs get tools for the thread they are working
in. Agents h4rn did **not** start — Claude Code or Codex in your own terminal, ChatGPT, a bot — can
connect to the daemon over OAuth, once you approve a level, and work with your projects and
threads.

The token model behind both is [ADR-044](decisions.md).

## Tools for agents h4rn runs

Every harness session is already given the hub endpoint and a bearer token minted for that one
session (ADR-006). The same token is bound to the session's thread, so the tools below act on
the calling thread and accept no thread id:

| Tool | What it does |
|---|---|
| `link_pull_request` | Links a PR from the thread's workspace repository, after one forge read. |
| `unlink_pull_request` | Removes the link and stops watching it. |
| `list_thread_pull_requests` | The thread's links with their latest forge state. |
| `watch_pull_request` | Starts the two-minute PR watch; a change starts a turn in the thread. |
| `unwatch_pull_request` | Stops the watch; the link stays. |
| `thread_status` | The thread's state and how many messages wait behind the running turn. |
| `list_forge_pull_requests` | Open, mine or review-requested PRs in the workspace repository. |

`link_pull_request` names a repository only when the workspace has more than one forge remote;
otherwise it uses the one remote. A failed forge read is reported as exactly that, never cached as
a state.

Tools are listed only to sessions that have a thread and whose harness switch is on. Settings →
**Sharing & access** shows one switch per harness. Turning a switch off hides the tools from new
sessions and refuses direct calls from live ones. The switch is stored in
`mcp-agent-tools.json` in the data directory; a missing entry means enabled.

## Outside agents

An outside agent connects to:

```text
http://127.0.0.1:<port>/mcp
```

The URL is on Settings → Sharing & access with a **Copy MCP URL** button. It is the daemon's
loopback listener: the OAuth redirect must finish on `https` or a localhost address, and the MCP
endpoint carries a bearer token, so it is deliberately not exposed on the LAN or tailnet.

### Sign in

The daemon is its own OAuth 2.1 authorization server:

- `GET /.well-known/oauth-authorization-server` — endpoints and supported methods;
- `GET /.well-known/oauth-protected-resource` — the `/mcp` resource;
- `POST /oauth/register` — dynamic client registration (public clients, no secret);
- `GET /oauth/authorize` — the approval page;
- `POST /oauth/approve` — the page's form;
- `POST /oauth/token` — authorization code with PKCE (S256) and refresh tokens;
- `POST /oauth/revoke` — revocation.

Redirect URIs are admitted only when they are `https`, or plain `http` to `localhost`,
`127.0.0.1` or `[::1]`, with no embedded credential and no fragment. Everything else is refused at
registration, before a browser is sent anywhere.

For Claude Code:

```sh
claude mcp add --transport http h4rn http://127.0.0.1:<port>/mcp
claude mcp login h4rn
```

For Codex:

```sh
codex mcp add h4rn --url http://127.0.0.1:<port>/mcp
codex mcp login h4rn
```

The first request without a token answers `401` with a `WWW-Authenticate` header pointing at the
protected-resource metadata, so a client that supports OAuth discovers the rest.

### The approval page

The page is served by the daemon because the browser belongs to the outside client, not to the
h4rn app. It names the client, says where its code will be returned, and asks for one of three
levels:

- **Read only** — list projects, threads, providers and models, and read a thread.
- **Supervised** — read, and start or message threads. Every thread it starts is forced to
  `approval_required`, and every thread it messages is switched to that mode first.
- **Full access** — read, start and message threads at the mode the call asks for.

The chosen level is written into the grant. The daemon checks it again on every tool call, so a
client cannot call a write tool by guessing its name. The page is not framed, is not cached, and
carries a strict content-security policy.

Only approve a sign-in you just started. Anyone who can open the page on the machine can approve
it, exactly as anyone who can read the data directory can use the local access token.

### Tokens

- Access tokens last **one hour**; refresh tokens last **thirty days** and rotate on every use.
- Both are random 256-bit values with a recognisable prefix (`h4rn_at_`, `h4rn_rt_`).
- Only a BLAKE3 digest is written to `outside_agents.json`; a copy of the file is not a copy of the
  credentials. No token is ever logged or returned in a tool result.
- Revoking an agent in Settings → Sharing & access marks every token in its family revoked and
  deletes its codes and pending approvals. The next request gets `401`.
- OAuth endpoints and the MCP endpoint are rate-limited per client and token.

### What an outside agent can call

| Tool | Level |
|---|---|
| `list_projects` | read |
| `list_threads` | read |
| `get_thread` | read |
| `list_providers` | read (harnesses, models recent threads used, proxy models) |
| `start_thread` | supervised, full |
| `send_thread_message` | supervised, full |
| `stop_thread` | supervised, full |

`start_thread` needs a project id or an absolute workspace, plus a harness and a model from
`list_providers`. It never creates a worktree. A thread it starts cannot run with more permission
than its grant allows.

An outside agent never sees the person's own upstream MCP servers; it sees only this tool set.

## Removal

Settings → Sharing & access lists connected agents with their level, when they last used a token
and when the sign-in expires. **Revoke** cuts one off immediately.

## Relay

The Connect relay carries the daemon's Noise transport (ADR-018/031), not an HTTP MCP endpoint, so
this phase offers the copied URL only on the local machine. A hosted agent needs a future HTTP
route on the relay; the UI does not pretend one exists.

## Tests

- `crates/h4rn-daemon/tests/mcp_server.rs` boots a real daemon: a FakeHarness session receives the
  thread-bound token; the hub lists and calls the tools; OAuth registration, PKCE, the approval
  page, level enforcement, rate limits, revocation, hashed storage and the protocol operations are
  exercised; and the PR-link tools run against a scripted forge.
- Unit tests in `crates/h4rn-daemon/src/mcp_server/` cover redirect allow-listing, PKCE vectors,
  expiry, one-time codes, token revocation and the store's hashed form.
- `apps/ui/src/features/access/` covers the Sharing & access list, copy and revoke.
