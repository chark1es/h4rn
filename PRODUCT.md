# Product

<!-- impeccable:product-schema 1 -->

## Platform

adaptive

## Stack

Greenfield. Shell: Tauri v2 with a React + TypeScript webview; server: Rust daemon; terminal: xterm.js over a PTY with binary framing; UI: shadcn/ui + Tailwind v4 + Motion. Mobile is a dedicated app per platform (iOS/Android), not a shrunk webview; native per OS versus a shared React Native surface remains open (see Capabilities and Constraints).

## Users

**Primary:** one developer, or a developer on a small team, who runs coding agents (Claude Code, Codex CLI, OpenCode, Antigravity) on one or more machines — a workstation, a laptop, a WSL distro, a home server or VPS. They are the operator, not the spectator: they start work, answer approvals, redirect a wrong turn, and judge the result.

Two real situations, both first-class:

1. **At the desk, in flow.** Three panes, keyboard-first, terminal and diffs open, several threads running at once across machines. They want density and speed, not a dashboard.
2. **Away from the desk, on a phone.** Standing in a queue, on a train, woken by a notification. They want to know *is anything stuck, did it finish, does it need me*, then approve, reply, resume, or snooze — in thumb reach, in under a minute.

**Secondary:** a teammate given scoped access to a host or a thread, and a returning user who has been away for days and needs to reconstruct what happened without reading every transcript.

## Product Purpose

h4rn is the control surface above coding-agent harnesses. It runs agents on machines the user owns, streams their work, and lets the user supervise from any device — web, desktop, phone — over h4rn-connect (relay) or Tailscale.

Success means: **nothing is silently stuck, and the user always knows what ran where.** A turn that needs approval surfaces in Working; a finished turn becomes Done until acknowledged; a thread left alone settles quietly. The user can leave and come back without reconstructing state from logs.

## Positioning

A **cross-harness, cross-machine control plane** — not another agent.

- One normalized thread/run/event model over each harness's native protocol, so Claude Code, Codex, OpenCode and Antigravity are comparable without pretending they are identical.
- Identity and provenance are explicit everywhere: which machine, which harness instance, which subagent, which MCP server actually ran — not just "the agent".
- Pairing and remote access are product features, not a networking tutorial: QR pairing, revocable devices, presence, Tailscale or relay.
- Usage-limit recovery is a first-class state: countdown, cancel, resume now, optional failover to another harness or credential.
- Secrets never leave the host; the control plane carries references and capability, not credentials.

A neighboring agent product could copy a feature, but not the operating model: the same thread lifecycle, the same "where it runs" grammar, and the same attention inbox across every harness and every device.

## Operating Context

- Agents run as child processes (or shared app-servers) on a host daemon; one host can expose several harnesses and, on Windows, several WSL distros as separate hosts.
- Transport is either a direct/tailnet connection or the h4rn-connect relay. Both can drop. Offline is normal, not exceptional.
- Sign-in to the relay is OIDC via a self-hosted OrbitID issuer; hosts are paired to devices, not to accounts alone.
- The user is often mid-task with several threads running, and approvals arrive as interruptions. Snooze and settlement exist because attention is finite.
- CLIProxyAPI can sit in front of harnesses as a multi-credential upstream; setup is per host, with optional per-harness overrides and custom model aliases.
- MCP servers and skills are shared across harnesses and machines, but each harness exposes them differently; where a server actually runs and whether a harness can receive it are facts the UI must show.
- Git forges (GitHub/GitLab) provide PRs/MRs, checks, comments; the user hands review comments back to the agent.
- Usage limits are wall-clock events with reset countdowns; a run can be paused for hours and resumed automatically.

## Capabilities and Constraints

**Confirmed functionality**

- Threads with live stream, tool calls, diffs, file changes, attachments, and an xterm.js terminal.
- Thread tabs **Working / Done / Settled** with a precise lifecycle: Working = a turn is running, queued, awaiting approval, or scheduled for auto-resume; Done = last turn finished and not yet acknowledged; Settled = acknowledged or auto-settled after idle (default 24 h). Manual override to any tab and snooze are always available.
- Thinking orbs show agent state; blobatars give each machine, harness, harness instance and subagent a deterministic identity.
- Cross-harness subagent tree, including children spawned on another harness or machine.
- Shared MCP servers and skills across harnesses and machines, with per-harness/per-machine toggles, conflict detection, and host-local secrets.
- CLIProxyAPI setup with a per-harness override toggle and custom model definitions/aliases.
- Usage/limits/stats dashboards: tokens, cost, TPS, TTFT, runtime, active vs idle time, reset countdowns.
- Auto-resume after a usage limit with countdown, cancel, resume now, and optional failover.
- Basic IDE mode: file tree, tabs, CodeMirror 6, search, diff vs HEAD and vs agent changes.
- Git forge PR/MR view: comments, checks, reply, open PR, hand a comment to the agent.
- Device pairing via QR, device revoke, presence; host list including WSL distros as separate hosts.

**Constraints**

- shadcn/ui + Tailwind v4 + Motion; light and dark themes.
- Animations 150–250 ms, interruptible, disabled under reduced motion.
- Keyboard-first with a command palette; WCAG AA.
- Empty, loading, error and offline states are designed, not defaulted. Skeletons or orbs instead of spinners.
- Phone layouts are genuinely mobile — bottom navigation, sheets, thumb reach — not a scaled desktop.
- Harness capabilities differ (attachments, MCP transport, approvals, resume). The UI may normalize vocabulary but must not imply parity that does not exist.
- Secrets and credentials stay on the host; the client never receives them.

**Undecided / open**

- Final brand wordmark and any logo asset.
- Whether mobile ships as fully native per OS or a shared React Native surface (the design language must adapt either way).
- Exact default auto-settle window per plan is 24 h; the mechanism is confirmed, the final default is configurable.

## Brand Commitments

- Name is **h4rn**, lowercase, always.
- Voice: precise, quiet, technical without cosplay. Short sentences, concrete nouns, no hype, no exclamation marks, no anthropomorphizing the agent beyond its actual state.
- No mascots, no illustration system, no decorative gradients. Identity comes from deterministic blobatars and live agent state, not from brand characters.
- The product should read as an instrument: restrained surfaces, one working accent, and information density where the user is competent.

**Anti-references** (what h4rn must not look or sound like):
- The rejected "Prompt Book" look: a stage-manager metaphor with an amber cue gutter, numbered turns, a stage-black rail, a black status bar in both themes, pill/chip provenance rows, and an orange logo block. Direction A (Quiet native) replaced it because it made the user learn theatre to read an inbox.
- A generic SaaS analytics dashboard: hero KPI cards, gradient chart fills, friendly empty-state illustrations, purple accent.
- A terminal costume: phosphor green on black, scanlines, monospace everywhere, "hacker" chrome. h4rn runs terminals; it is not one.
- An IDE cosplay: VS Code's blue, a permanent activity bar with twenty icons, a minimap on every surface.
- The "AI product" aesthetic: neon glow, glassmorphism, orbit/particle backgrounds, a chatbot bubble as the primary metaphor.
- Warm-cream editorial or broadsheet pastiche: serif headlines, italic pull quotes, ruled-paper tables.
- Gamification: streaks, confetti, scores, mascots, celebration animations.
- Enterprise admin gloom: grey-on-grey tables, no point of view, every state reduced to a red badge.

## Evidence on Hand

- `docs/research/t3code.md` — thread lifecycle rules, terminal semantics, attachments, pairing/auth, settlement. Source-verified.
- `docs/research/ui-libs.md` — `thinking-orbs` 0.3.2 state mapping and `blobatar` 2.7.0 seed/identity scheme, with measured bundle and API facts.
- `docs/research/cliproxyapi.md` — management API, model listing/aliases, usage surfaces, per-harness recipes.
- `docs/research/orbitid.md` — OIDC discovery, device grant, scopes/claims, token lifetimes.
- No customer quotes, benchmarks, press, screenshots, or pricing exist. None may be fabricated.

## Product Principles

1. **Where it runs is part of the answer.** A model name without a machine, harness and credential context is an incomplete fact. Show provenance at the point of decision.
2. **Attention is the scarce resource.** The inbox is the product: Working needs you only when it raises a hand; Done is a debt to acknowledge; Settled is permission to forget. Never let a stuck thread hide.
3. **Reversible by default.** Interrupt, cancel, resume, snooze, un-settle, revoke, roll back. Every automated action has a visible manual override and a stated consequence.
4. **Host-local truth.** Credentials, secrets and raw logs stay on the host. The control surface shows state and capability, never the secret itself.
5. **One grammar, honest differences.** The same lifecycle, identity and status vocabulary across harnesses and devices — while capability gaps (attachment support, MCP transport, failover) are named rather than smoothed over.

## Accessibility & Inclusion

- WCAG 2.2 AA in both themes: text and essential non-text contrast checked per token pair; state is never carried by color alone.
- Full keyboard operation with visible focus, a documented shortcut map, and a command palette that covers every primary action.
- `prefers-reduced-motion`: all orb/avatar animation and transitions collapse to static representative frames; no information is lost.
- Touch targets ≥ 44×44 px on phone; no hover-only affordances; screen-reader labels on orbs, blobatars, diffs and status changes.
- Long-running and disruptive events are announced politely (`aria-live`), never as a stream of interruptions.
