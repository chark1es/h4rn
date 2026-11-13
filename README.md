# h4rn

A cross-harness, cross-machine control surface for coding agents — the prompt book above the work.

h4rn is one instrument for every agent you run: Claude Code, Codex, OpenCode, Antigravity and
whatever comes next, on this machine or another one. The desktop, mobile and browser clients are
pure views over a single Rust daemon and one typed WebSocket protocol.

> **Status: Phase 1a scaffold.** The workspace compiles, the lifecycle core is tested, and the
> generated protocol bindings are checked in. There are no harness adapters or product screens
> yet. See [CHANGELOG.md](CHANGELOG.md).

## What is here

| Path | What it is |
|---|---|
| `crates/h4rn-protocol` | The wire contract: IDs, client/server frames, events, projections, PTY framing. TypeScript is generated from it. |
| `crates/h4rn-core` | ADR-004 lifecycle state machine, thread projections, event store (SQLite + in-memory). |
| `crates/h4rn-harness` | The async `Harness`/`HarnessSession` trait, normalized harness events, and a `FakeHarness` for tests. |
| `crates/h4rn-daemon` | The process that owns threads, sessions and the local store. |
| `crates/h4rn-connect` | The self-hostable relay (axum). Serves `/healthz`; sync lands in a later phase. |
| `crates/h4rn-transport` | WebSocket transport and resume logic. |
| `crates/h4rn-vt` | Terminal (VT) handling for PTY output. |
| `crates/h4rn-mcp`, `h4rn-skills` | MCP servers and skill discovery. |
| `crates/h4rn-proxy`, `h4rn-meter` | Provider proxying and usage accounting. |
| `crates/h4rn-forge` | Forge integration (branches, PRs). |
| `apps/ui` | React 19 + Vite + Tailwind v4 shell; shadcn primitives, design tokens from `DESIGN.md`. |
| `apps/desktop`, `apps/mobile` | Tauri v2 shells. Excluded from the Cargo workspace; they consume `apps/ui/dist`. |
| `apps/web` | Static publish step that copies the UI build into `apps/web/dist`. |
| `deploy/` | `h4rn-connect` container image and compose file. |

Architecture decisions live in [docs/decisions.md](docs/decisions.md); product intent is in
[PRODUCT.md](PRODUCT.md) and the design system in [DESIGN.md](DESIGN.md).

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
