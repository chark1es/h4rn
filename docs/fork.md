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

A turn is offered only on the journal's evidence that it ended: it was copied
from a finished conversation, or the daemon saw it start and saw it end (a
restart records the turn it lost as interrupted). A turn whose run a restart
interrupted is read the same way even from an older journal that stored the host
loss without the turn's completion row: the loss itself is the evidence, so the
turn beside it is interrupted and the turns after it stay forkable. A prompt that
never started is not a turn: it is left out of a later turn's copy and cannot
itself be forked. A turn that started but never settled — with a later turn
behind it — is refused rather than copied half-written.

The fork keeps:

- the same project, workspace, harness, agent instance, model, runtime mode,
  reasoning effort and fast-tier setting;
- a title of `<original title> (fork)`;
- everything the source showed through the chosen turn, inclusive, with each
  record's own time and in the same order, by the journal's own sequence:
  messages and their attachments, tool calls, delegated subagent rows in their
  final state, the plan, error and usage-limit rows, and an assistant reply
  that was still streaming when h4rn stopped (rebuilt from its streamed text).
  A prompt is shown before the turn it started, as in the live thread, so every
  copied prompt sits above its own reply — including two prompts queued behind
  one running turn, which stay above that turn's reply where the source showed
  them;
- each turn's start at that turn's own first record's time, including turns
  that came from an import;
- the turn's own completion: its token usage, its file-change list for the
  "N files changed" card, and its real status. A turn the daemon lost to a
  restart copies as **interrupted**; copied history was already finished and
  stays finished, so an imported turn — which has no completion row of its own —
  copies as **completed**;
- its own copy of every attachment blob. The fork retains the blob and gets its
  own per-thread file, so deleting the source never removes a file the fork
  still shows. Adoption is all or nothing: if any attachment is gone, the fork
  is refused with a message, and every retain it already made is rolled back,
  together with the per-thread files it materialised. A journal write that fails
  after adoption rolls back the same way.

A queued message is not a turn: it waits in the thread's queue, and a fork of
the turn it waits behind does not copy it. A delivery claim is no turn either,
so a message a claim is starting is not copied. A message the queue promotes
after the copy began waits for the copy gate like any other send, so it cannot
reach the fork's reading of the source. A turn interrupted by a restart is
settled as interrupted.

Journals written before queued prompts had their own turn id are read the same
way: a prompt filed under the running turn goes to the next turn that starts and
has no prompt of its own. A prompt that never became a turn — the daemon stopped
before it could run — is never attached to a later turn that already has its
own. Its row cannot name a real turn, so no fork is offered from it, and a fork
at that point or any later one is refused with a clear message instead of
copying a transcript the source did not have. That refusal has its own advice —
**Fork from an earlier turn or start a new thread** — rather than the generic
"wait for the turn" note.

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
  A whole-session copy is only offered while the source really is at the
  chosen tip, so a message still waiting in the source's queue or a claim it is
  starting makes the fork hand off instead. While a provider reads the source's
  saved session, the source's next message is accepted at once — it already has
  its turn and its durable row — but it cannot reach the harness until the copy
  is over. The copy is over when
  the fork reports its session or the startup fails. The deadline covers the
  whole copy, not just the child starting: a provider returns from its start as
  soon as the process is up and only then reads the source, so the fork holds
  the source's gate until that provider reports its new session. A provider
  still reading the source when the deadline passes has its process shut down,
  any session it produces later is discarded without being used, and the fork
  continues on the handoff. Only then is the source released, so a message sent
  after the deadline can never reach something the fork shows. A queued message
  that becomes active while a copy is pending waits for that gate in the
  background, never while holding the thread's session-switch lock: a Send that
  arrives once the queue has moved on is still answered at once. Only a thread
  that started a session of its own offers its session id: a fork that has not
  sent anything yet still points at the conversation it was made from, so a
  fork of that fork uses the handoff instead. If the source has moved on by
  then, or the fork cannot be bounded, the native intent is cleared and the
  fork hands off instead. The provider keeps its own context, including tool
  work h4rn never journaled, and the source session keeps running untouched.
- **Portable handoff.** For a harness with no native fork, a source that moved
  on, or a native fork that dies before it reports a session, h4rn starts a
  fresh session and sends a bounded handoff of the copied transcript ahead of
  your exact request. The handoff selects the first request plus the newest
  messages under the same byte budget as an import (`H4RN_HANDOFF_BYTES`, 16,000
  by default, clamped to 1,024–64,000) and never shortens your own message. It
  reads only the turns before the one it is starting, so a message queued while
  the startup was pending is not sent twice when it is promoted.

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
- no queued message, delivery claim or pending approval (a queued or claimed
  message is not a turn, so it is never copied into the turn it waits behind);
- no parked prompt and no rate-limit auto-resume schedule.

A fork that has not yet sent a message has no live session, so it costs nothing
until you write.

## Publishing a fork

Creating a fork publishes only the new thread's summary on the live feed. Its
transcript is durable history and arrives through the same paged history as any
other thread, so forking hundreds of turns never fills the event feed or makes a
slow client miss a later completion on another thread.
