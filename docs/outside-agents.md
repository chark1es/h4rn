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
endpoint carries a bearer token, so it is deliberately not exposed on the LAN or tailnet. A LAN or
tailnet listener answers 404 for `/mcp` and every OAuth route, and the daemon refuses a request
whose `Host` is not a loopback name.

### Sign in

The daemon is its own OAuth 2.1 authorization server:

- `GET /.well-known/oauth-authorization-server` — endpoints and supported methods;
- `GET /.well-known/oauth-protected-resource` — the `/mcp` resource;
- `POST /oauth/register` — dynamic client registration (public clients, no secret);
- `GET /oauth/authorize` — the page the client's browser opens;
- `GET /oauth/authorize/pending` — the page's own navigation once the person answered;
- `POST /oauth/token` — authorization code with PKCE (S256) and refresh tokens;
- `POST /oauth/revoke` — revocation.

There is no unauthenticated approve endpoint. The client's browser cannot grant anything: the
person answers the request inside h4rn.

Redirect URIs are admitted only when they are `https`, or plain `http` to `localhost`,
`127.0.0.1` or `[::1]`, with no embedded credential and no fragment. Everything else is refused at
registration, before a browser is sent anywhere. A token request may omit `redirect_uri` (PKCE
binds the exchange); when it sends one, it must match the authorization request exactly. A client
that names an RFC 8707 `resource` gets a token bound to it, and `/mcp` refuses a token issued for
anything else. The token endpoint checks the same binding: naming a different `resource`, or naming
this daemon's endpoint for a code approved without one, is refused with `invalid_target` before the
code is spent.

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

### Approving a sign-in

The authorize page names the client, labels that name **unverified (the app chose it)**, shows the
exact callback origin the browser will return to, and says: approve this in h4rn. It never offers a
level and has no form.

Open h4rn → Settings → Sharing & access. The request is listed under **Waiting for you** with the
same callback. Pick one of:

- **Read only** — list projects, threads, providers and models, and read a thread.
- **Supervised** — read, and start or message threads. Every thread it starts runs at
  `approval_required`, and every message it sends carries that ceiling with it.
- **Full access** — read, start and message threads at the mode the call asks for.

Nothing is preselected: Supervised and Full access need an explicit choice. The browser then
follows the answer to the callback on its own, so no form ever posts to another site.

Answering without a level is refused. The level is written into the grant and checked again on
every tool call, so a client cannot call a write tool by guessing its name.

Only approve a sign-in you just started. A hostile page cannot approve one for you either: the
daemon refuses an `Origin` that is not its own exact `scheme://host:port` (another loopback port is
still another site), a `Sec-Fetch-Site` other than `same-origin` or `none` (so `same-site` is
refused too) and any `Host` that is not loopback. The sign-in list itself is bounded: at most three
requests per client and sixteen in total wait for you at once, and a request that this check refuses
never spends a rate-limit hit.

### Tokens

- Access tokens last **one hour**; refresh tokens last **thirty days** and rotate on every use.
- Both are random 256-bit values with a recognisable prefix (`h4rn_at_`, `h4rn_rt_`).
- Only a BLAKE3 digest is written to `outside_agents.json`; a copy of the file is not a copy of the
  credentials. No token is ever logged or returned in a tool result. An approval's redirect carries
  the one-time code, so it is kept in memory only: the store file never contains a live code, and a
  restart asks the browser to start the sign-in again instead of replaying one.
- Revoking an agent in Settings → Sharing & access marks every token in its family revoked, deletes
  its codes and pending approvals, and frees its registration slot (so the next agent can register
  once the cap is reached). The answer comes only after the file is on disk, so a restart cannot
  resurrect the grant. The next request gets `401`; a revocation that could not be saved reports an
  error rather than success.
- A replayed authorization code revokes the tokens it issued, and a replayed rotated refresh token
  revokes its whole family: either one means two parties hold the same credential.
- If `outside_agents.json` cannot be read (corrupt, or written by an older build), the daemon
  quarantines it as `outside_agents.corrupt-<time>.json`, starts with no grants, and logs a
  warning. Agents reconnect by signing in again.
- Registration is capped and unused registrations expire after a day. The unauthenticated OAuth
  endpoints — registration, authorize, the token endpoint and revocation — have an aggregate rate
  limit checked before the body is read, and the per-client buckets are keyed only on identifiers
  the daemon has registered — a flood of random values grows nothing.

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
than its grant allows: a supervised grant always starts at `approval_required`, and a message it
sends to an existing thread is clamped to that mode for the turn without changing the person's own
thread setting. The ceiling holds for the whole turn: raising the thread's mode while that turn runs
changes the live session only as far as the ceiling allows, and the person's next turn gets the new
mode. A turn stopped by a provider limit resumes under the same ceiling, even across a daemon
restart.

An outside agent never sees the person's own upstream MCP servers; it sees only this tool set.

Both MCP endpoints accept one JSON-RPC message per request. A batch is refused, and a request whose
`Origin` is another site is refused, so one scope check and one rate-limit hit always belong to one
call.

`get_thread` and `list_threads` redact credentials before they build either output form, including
the thread title (which is the first line of the person's first prompt): the daemon's own access
token, `h4rn_`-prefixed tokens, and the forge token shapes h4rn already recognises (`ghp_`,
`glpat-`, `ATATT` and the rest).

## Removal

Settings → Sharing & access lists connected agents with their level, when they last used a token
and when the sign-in expires. **Revoke** cuts one off immediately.

## Relay

The Connect relay carries the daemon's Noise transport (ADR-018/031), not an HTTP MCP endpoint, so
this phase offers the copied URL only on the local machine. A hosted agent needs a future HTTP
route on the relay; the UI does not pretend one exists: when h4rn is open as a remote client, the
Copy button is not shown and the page says the URL is the loopback address of the other computer
and cannot be reached by an agent on the internet yet.

## Tests

- `crates/h4rn-daemon/tests/mcp_server.rs` boots a real daemon: a FakeHarness session receives the
  thread-bound token; the hub lists and calls the tools; OAuth registration, PKCE (including a
  rejected `plain` challenge), the sign-in answered over the protocol, the page's redirect shape,
  level enforcement, aggregate and per-client rate limits, revocation, hashed storage and the
  protocol operations are exercised; approval without an owner session, a cross-site `Origin`, a
  page on another loopback port, `same-site`, a rebinding `Host`, a non-loopback listener, a foreign
  `resource`, a resource named at the token endpoint, a JSON-RPC batch, a supervised ceiling raised
  after queueing, refresh rotation, redacted titles and tool output, and a revocation that cannot be
  saved are proved; and the PR-link tools run against a scripted forge.
- Unit tests in `crates/h4rn-daemon/src/mcp_server/` cover redirect allow-listing, PKCE vectors,
  expiry, one-time codes, replay revocation, resource binding at the token endpoint, the
  revocation-versus-use race, a revocation freeing its registration slot, an approval never writing
  its code to disk, sign-in caps, registration caps, the bounded rate limiter, quarantine of an
  unreadable store, and the page having no form.
- `crates/h4rn-daemon/src/threads.rs` covers the ceiling through a capped turn, across a mode change,
  on a reused session, and through a usage limit and restart.
- `apps/ui/src/features/access/` covers the Sharing & access list, the waiting sign-in (unverified
  name, callback origin, no preselected level), copy, revoke, and the remote-copy explanation.
