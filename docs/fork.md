# Forking a thread at a turn

A fork starts a new thread from a finished point in another thread's
conversation, so you can try a different direction without losing the original.
It is an ordinary thread: it runs its own session, has its own queue, approvals
and schedules, and its own title, model and permissions.

## Making a fork

**Fork from here** is in the menu of every finished turn. The thread menu and
the command palette offer **Fork from latest turn** while the newest turn is
settled; it is unavailable while that turn is still running or its queued
message has not started. A turn that is still running, or a message still
waiting to run, is refused with a message to let it finish first.

The fork keeps:

- the same project, workspace, harness, agent instance, model, runtime mode,
  reasoning effort and fast-tier setting;
- a title of `<original title> (fork)`;
- everything the source showed through the chosen turn, inclusive, with each
  record's own time: messages and their attachments, tool calls, delegated
  subagent rows, the plan, error and usage-limit rows, and an assistant reply
  that was still streaming when h4rn stopped (rebuilt from its streamed text).

The original thread is read, never changed. Deleting, archiving or settling it
afterwards does not affect the fork.

A fork of a thread that runs in a linked worktree starts in that same worktree
path, as a second thread in the same checkout. Per-fork worktrees are not part
of this feature yet.

## Continuing the conversation

The first new message starts the fork's session. There are two ways to carry the
conversation over:

- **Native fork.** When the harness can copy its own conversation, h4rn asks it
  to. Codex app-server's `thread/fork` names the provider turn to copy through
  (`lastTurnId`), so an earlier turn stays exact even after the source carries
  on. Claude Code is started with `--resume <session-id> --fork-session`, which
  copies the whole saved conversation, so it is used only while the chosen turn
  is still the source session's tip; h4rn re-checks that when the fork starts.
  If the source has moved on by then, or the fork cannot be bounded, the native
  intent is cleared and the fork hands off instead. The provider keeps its own
  context, including tool work h4rn never journaled, and the source session
  keeps running untouched.
- **Portable handoff.** For a harness with no native fork, a source that moved
  on, or a native fork that dies before it reports a session, h4rn starts a
  fresh session and sends a bounded handoff of the copied transcript ahead of
  your exact request. The handoff selects the first request plus the newest
  messages under the same byte budget as an import (`H4RN_HANDOFF_BYTES`, 16,000
  by default, clamped to 1,024–64,000) and never shortens your own message.

The native attempt is spent once. If it succeeds, the fork's session id becomes
the fork's own; if it fails before starting, the fork records that and uses the
handoff for that turn. A restart never tries to fork the source again.

A native fork copies the provider's conversation. A handoff is text only: it
carries the visible messages, not attachments or tool-call state, though the
fork's own transcript still shows them.

## Lineage

The fork's header shows **Forked from <title>** and links back to the source
thread. Only the source thread and turn ids are stored, so renaming the source
shows the new name in every fork. If the source is no longer in your thread
list, the line drops the link and says the source is gone.

## Safety

The source and the fork share no live state:

- no harness session: a native fork asks the provider to copy the conversation,
  and a handoff starts a new one;
- no queued message or pending approval (a message queued behind the running
  turn is its own turn and is never copied into the one it waits behind);
- no parked prompt and no rate-limit auto-resume schedule.

A fork that has not yet sent a message has no live session, so it costs nothing
until you write.
