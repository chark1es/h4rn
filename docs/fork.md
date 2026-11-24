# Forking a thread at a turn

A fork starts a new thread from a finished point in another thread's
conversation, so you can try a different direction without losing the original.
It is an ordinary thread: it runs its own session, has its own queue, approvals
and schedules, and its own title, model and permissions.

## Making a fork

**Fork from here** is in the menu of every finished turn. The thread menu and
the command palette offer **Fork from latest turn** for the newest finished
turn. A turn that is still running, or has a message queued behind it, is
refused with a message to let it finish first.

The fork keeps:

- the same project, workspace, harness, agent instance, model, runtime mode,
  reasoning effort and fast-tier setting;
- a title of `<original title> (fork)`;
- the transcript through the chosen turn, inclusive, with each message's own
  timestamp.

The original thread is read, never changed. Deleting, archiving or settling it
afterwards does not affect the fork.

A fork of a thread that runs in a linked worktree starts in that same worktree
path, as a second thread in the same checkout. Per-fork worktrees are not part
of this feature yet.

## Continuing the conversation

The first new message starts the fork's session. There are two ways to carry the
conversation over:

- **Native fork.** When the chosen turn is the latest turn of the source's live
  session and the harness can copy its own conversation, h4rn asks it to. Claude
  Code is started with `--resume <session-id> --fork-session`; Codex app-server
  uses `thread/fork`. The provider keeps its own context, including tool work
  h4rn never journaled, and the source session keeps running untouched.
- **Portable handoff.** For an earlier turn, a harness with no native fork, or a
  native fork that dies before it reports a session, h4rn starts a fresh session
  and sends a bounded handoff of the copied transcript ahead of your exact
  request. The handoff selects the first request plus the newest messages under
  the same byte budget as an import (`H4RN_HANDOFF_BYTES`, 16,000 by default,
  clamped to 1,024–64,000) and never shortens your own message.

The native attempt is spent once. If it succeeds, the fork's session id becomes
the fork's own; if it fails before starting, the fork records that and uses the
handoff for that turn. A restart never tries to fork the source again.

A native fork copies the provider's conversation; a handoff copies only the
visible messages. Attachments and tool-call state are not carried into a
handoff.

## Lineage

The fork's header shows **Forked from <title>** and links back to the source
thread. Only the source thread and turn ids are stored, so renaming the source
shows the new name in every fork. If the source is no longer in your thread
list, the line drops the link and says the source is gone.

## Safety

The source and the fork share no live state:

- no harness session: a native fork asks the provider to copy the conversation,
  and a handoff starts a new one;
- no queued message or pending approval;
- no parked prompt and no rate-limit auto-resume schedule.

A fork that has not yet sent a message has no live session, so it costs nothing
until you write.
