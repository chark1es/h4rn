# h4rn

A cross-harness, cross-machine control surface for coding agents — the prompt book above the work.

h4rn is one instrument for every agent you run: Claude Code, Codex, OpenCode, Antigravity, any
local ACP command ([docs/acp.md](docs/acp.md)) and whatever comes next, on this machine or another
one. The desktop, mobile and browser clients are
pure views over a single Rust daemon and one typed WebSocket protocol.

> **Status: v1.0.1.** The daemon, the typed protocol, the Noise transport,
> the relay, MCP sharing, skills, metering, forges and packaging are implemented and tested; the
> desktop, mobile and browser clients are pure views over the daemon. Two paths are deliberately
> not implemented yet — see [Known limits](#known-limits). The detailed history is in
> [CHANGELOG.md](CHANGELOG.md).

## What is here

| Path | What it is |
|---|---|
| `crates/h4rn-protocol` | The wire contract: IDs, client/server frames, events, projections, PTY framing. TypeScript is generated from it. |
| `crates/h4rn-core` | ADR-004 lifecycle state machine, thread projections, event store (SQLite + in-memory). |
| `crates/h4rn-harness` | The async `Harness`/`HarnessSession` trait, normalized harness events, and a `FakeHarness` for tests. |
| `crates/h4rn-daemon` | The process that owns threads, sessions, files, terminals, MCP, skills, proxy routing and the local store. |
| `crates/h4rn-connect` | The self-hostable relay (axum + Postgres): OIDC/DPoP auth, pairing scopes, enrollment and skill/usage sync. |
| `crates/h4rn-transport` | Noise end-to-end encrypted multipath transport: session layer, path selection and relay connector (ADR-009/010/031/032). |
| `crates/h4rn-transport-wasm` | `wasm-bindgen` facade over the same transport for the browser. |
| `crates/h4rn-vt` | Terminal (VT) handling for PTY output. |
| `crates/h4rn-mcp`, `h4rn-skills` | The in-tree MCP hub with cross-machine sharing, and skill bundles with registry sync. |
| `crates/h4rn-proxy`, `h4rn-meter` | CLIProxyAPI routing and failover, and the durable usage ledger. |
| `crates/h4rn-forge` | Forge integration (branches, PRs). |
| `apps/ui` | React 19 + Vite + Tailwind v4 shell; shadcn primitives, design tokens from `DESIGN.md`. |
| `apps/desktop`, `apps/mobile` | Tauri v2 shells. Excluded from the Cargo workspace; they consume `apps/ui/dist`. |
| `apps/web` | Static publish step that copies the UI build into `apps/web/dist`. |
| `deploy/` | `h4rn-connect` container image and compose file. |

Architecture decisions live in [docs/decisions.md](docs/decisions.md); product intent is in
[PRODUCT.md](PRODUCT.md) and the design system in [DESIGN.md](DESIGN.md). Packaging and the
tag-driven release process are in [docs/release.md](docs/release.md); relay deployment is in
[docs/self-hosting.md](docs/self-hosting.md). Thread titles written by a model are described in
[docs/text-generation.md](docs/text-generation.md).

## Known limits

These paths exist in the protocol but not in the daemon yet. The UI never
offers them: the new-thread dialog says it does not create a worktree, and the
Delegate dialog lists other online hosts as a disabled option with the reason.

- **A new thread cannot request a worktree.** `CreateThread` with `worktree`
  set fails with `NotImplemented`; a new thread runs directly in the workspace
  directory you choose. Subagent worktree isolation is implemented and is a
  different path ([docs/subagents.md](docs/subagents.md)).
- **A subagent runs only on its parent's host.** `spawn_subagent` with another
  host fails with `Unsupported` before creating a thread; the Delegate dialog
  lists other online hosts but disables them with that reason.

## Prerequisites

- Rust `1.99.1` (pinned by `rust-toolchain.toml`) with `clippy` and `rustfmt`.
- Bun `1.4.4` (`packageManager` in `package.json`).
- Linux desktop builds additionally need WebKitGTK 4.1 and Tauri's system packages.
- Android/iOS builds need the Tauri mobile toolchain and SDKs.

## Quickstart

```sh
bun install

# Rust
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

# UI
bun run --cwd apps/ui dev        # http://127.0.0.1:1420
bun run --cwd apps/ui build

# Shells
bun run desktop                  # tauri dev against apps/ui
bun run web                      # copy the UI build into apps/web/dist
```

## Background daemon

Build and install `h4rnd` outside the build directory, then let it manage its
own per-user service and updates:

```sh
cargo build --release -p h4rn-daemon
install -m 0755 target/release/h4rnd ~/.local/bin/h4rnd   # Linux/macOS

h4rnd service install      # log-in service; idempotent
h4rnd service status       # installed / active / enabled
h4rnd service restart
h4rnd service logs
h4rnd service uninstall    # keeps the data directory

h4rnd update --check       # current vs latest release
h4rnd update               # signed update; restarts the service
```

The service runs as your user, not root, because the agents it supervises need
your credentials. On Linux, `service status` says when lingering is off and
prints the `loginctl enable-linger` command to keep the daemon running after
logout. On Windows the task is named per account (`h4rn-<user>`), `status`
reports `enabled: unknown`, and `logs` prints guidance because the task has no
console. Development builds have no updater key, so `h4rnd update` refuses to
self-update and prints the manual steps; release builds require a minisign
signature over `SHA256SUMS` before replacing the binary, hold a per-installation
lock while they do it, and refuse while a turn is running (or while a daemon
cannot be checked) unless you pass `--force`. The busy check follows the
registered service's own endpoint, starting with a `--bind` in its command line
and then its `H4RN_BIND`, not the `--bind` of the update command.
A rollback backup is staged and renamed into place, so a failed publication
leaves the previous one usable, and `--rollback` refuses a backup that is
missing, empty or damaged. If a turn starts
after the new binary is installed but before the restart, the update keeps the
binary and asks you to run `h4rnd service restart` when the turn finishes.

## Protocol bindings

`h4rn-protocol` is the single source of truth. TypeScript under `apps/ui/src/gen/` is generated,
committed, and never edited by hand:

```sh
cargo test -p h4rn-protocol      # regenerate
./scripts/check-bindings.sh      # fail if the working tree differs
```

CI runs the check on every push and pull request.

## Verification

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

cd apps/ui
bun run typecheck
bun run lint
bun run test
bun run build
```

## Relay

```sh
cd deploy
docker compose up --build
curl http://127.0.0.1:8787/healthz
```

## License

MIT.
