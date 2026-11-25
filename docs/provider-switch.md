# Switching a thread to another harness

A thread can change provider between turns. The history, workspace and every
setting the new harness offers stay with it; the next turn starts a fresh
session there and continues from a bounded handoff of the conversation.

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
4. The first turn on the new harness starts a fresh session and sends a bounded
   handoff of the thread's journaled messages before your request, whether that
   turn began as a Send, a message promoted from the queue, or an auto-resume.
   The builder and budget are the same ones the import fallback uses
   ([docs/import.md](import.md)): 16,000 UTF-8 bytes by default,
   `H4RN_HANDOFF_BYTES` clamped to 1,024–64,000. Your request is appended
   verbatim and is never shortened. The flag is cleared only once the harness
   accepts that prompt, so a failed start or write still sends the history on
   the retry. That clear also names the switch it belongs to and changes only
   the flag, so a write that finishes after the thread has switched again can
   never restore the old harness, model or resume id. Later messages resume the
   new session normally. A message still waiting behind the running turn is not
   history: the handoff leaves every queued prompt out, by its recorded id, and
   those ids are kept durably until a provider accepts the prompt, so a restart
   does not turn a queued message into history. A rate-limited write parks the
   request you sent, not the prompt with its handoff block; the block is built
   again for the next delivery instead of being wrapped twice. A turn is only
   shown as done once that acceptance is durable (and the switch flag is
   cleared), so the thread cannot look idle while the journal still calls your
   request undelivered.
5. Each turn is metered against the harness and model that actually ran it.
6. A recorded usage limit belonged to the old provider, so the switch clears
   it and its countdown leaves the sidebar and the phone inbox. An armed
   auto-resume still refuses the switch, because it owns a prompt.

## Settings that carry over

The permission mode, reasoning effort and fast tier follow the thread when the
new harness offers them. A setting it does not offer is dropped to that
harness's safe default, and the switch reports which ones and the values they
held:

- a **Plan** mode moving to a harness without one becomes **Approval required**;
- a reasoning effort the new harness does not expose returns to its default;
- a fast tier moving to a harness without one is turned off.

A configured ACP agent names its own modes and efforts at session start. The
switch resolves that catalogue before it commits (the same check Settings runs),
so a value the agent does not advertise is dropped to its safe default and
reported instead of being carried into a session that cannot start. When the
agent names its modes but none is a safe replacement — for example it offers
only `accept-edits` and the thread is in Plan — the switch is refused before it
commits and names the modes the agent does offer, rather than reporting success
and failing on the first Send. An agent whose whole catalogue is outside h4rn's
vocabulary (`review-only`, say) is refused by name too: it offers no mode a
thread could run in, which is not the same as an agent that never named one.

The model is chosen for the new harness rather than carried over, because model
names rarely mean the same thing across providers.

A thread already on a configured ACP agent can switch to a different configured
agent; the agent id is part of what the switch changes.

## When a switch is refused

A switch happens between turns, so these states refuse it with a plain message
instead of splitting work across providers:

- a turn is running (or a prompt is still being handed to the harness);
- a message is queued behind the running turn;
- a prompt is parked waiting for a delivery;
- a rate-limit pause is armed with an auto-resume;
- an approval is waiting for an answer.

A refused switch changes nothing: the thread keeps its harness, its session, its
queued or parked work and its recorded usage limit with its countdown. The
refusal carries the advice that fits it, not a generic "wait for the turn".

The switch itself is one durable decision. The thread row and the divider are
written in one transaction, and only then is the old session ended; a journal
that refuses the write leaves the thread on its old harness with its session
intact. An approval whose session is gone is dropped rather than blocking every
later switch, and is resolved on every client so no cue or phone sheet stays up
for an answer no session can take. The fallback from a dead native resume (an
imported conversation whose harness exited before it found the session) writes
only its own fields and only on the switch it read, so a Stop and a switch that
land while that write waits are not undone by it. Each cue has a daemon-minted client id, and
the harness's own request id stays private to its session: two agents can both
ask approval `7` without either answer reaching the other's question, and an
answer for a retired cue is refused instead of resolving a newer one.

## Known limits

- An ACP agent configured with a fixed working directory cannot take over a
  thread whose workspace is somewhere else; the switch says so instead of
  silently changing the thread's workspace.
- Switching is not offered while an approval is pending. Answering it first
  keeps the answer in the session that asked the question.
