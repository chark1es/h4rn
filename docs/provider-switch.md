# Switching a thread to another harness

A thread can change provider between turns. The history, workspace and every
setting the new harness offers stay with it; the next turn starts a fresh
session there and continues from a bounded summary of the conversation.

## Where it is offered

- **The thread header.** The harness name beside the title is a control. It
  opens a small panel with the other harnesses installed on that host and the
  configured ACP agents. Choose one, pick a model for it, and press **Switch**.
- **The command palette.** `⌘K`, then **Switch harness…** for the thread in view.

Only installed harnesses are offered. An adapter the daemon cannot start would
only fail after the switch, so it is not listed.

## What happens

1. The old harness session is shut down cleanly. No next session can start
   until it has ended, so the thread never has two live sessions.
2. The old harness-native resume id is cleared on the stored thread, so a
   daemon restart cannot resume the old conversation.
3. `HarnessSwitched { from, to, at }` is journaled and the transcript shows a
   quiet divider: **Switched from Claude Code to Codex**.
4. The first message on the new harness starts a fresh session and sends a
   bounded handoff of the thread's journaled messages before your request. The
   builder and budget are the same ones the import fallback uses
   ([docs/import.md](import.md)): 16,000 UTF-8 bytes by default,
   `H4RN_HANDOFF_BYTES` clamped to 1,024–64,000. Your request is appended
   verbatim and is never shortened. Later messages resume the new session
   normally.
5. Each turn is metered against the harness and model that actually ran it.

## Settings that carry over

The permission mode, reasoning effort and fast tier follow the thread when the
new harness offers them. A setting it does not offer is dropped to that
harness's safe default, and the switch reports which ones and the values they
held:

- a **Plan** mode moving to a harness without one becomes **Approval required**;
- a reasoning effort the new harness does not expose returns to its default;
- a fast tier moving to a harness without one is turned off.

The model is chosen for the new harness rather than carried over, because model
names rarely mean the same thing across providers.

## When a switch is refused

A switch happens between turns, so these states refuse it with a plain message
instead of splitting work across providers:

- a turn is running (or a prompt is still being handed to the harness);
- a message is queued behind the running turn;
- a prompt is parked waiting for a delivery;
- a rate-limit pause is armed with an auto-resume;
- an approval is waiting for an answer.

A refused switch changes nothing: the thread keeps its harness, its session and
its queued or parked work.

## Known limits

- An ACP agent configured with a fixed working directory cannot take over a
  thread whose workspace is somewhere else; the switch says so instead of
  silently changing the thread's workspace.
- Switching is not offered while an approval is pending. Answering it first
  keeps the answer in the session that asked the question.
