# Editing an earlier prompt

**Edit from here** sits beneath a turn's first sent message, its cue. Choosing
it rewinds the thread to just before that message: the message and everything
after it leave the active conversation and the harness's own history. The
prompt and its attachments come back to the composer, ready to edit and resend.

A message steered into a running turn is journaled inside that same turn. It
has no edit action of its own, and the daemon refuses one that names it: cutting
there would also remove the cue before it, a message the person did not choose.
Edit the cue to rewind the whole turn.

This is a conversation edit, not a time machine. It does not undo external
actions the agent took elsewhere — a push, a deploy, a message to another
system. Only the thread, the harness history and (optionally) the worktree go
back.

Restoring files is a **Labs** feature and off by default. The conversation
rewind is ready; the file restore is still being hardened, so it is a
deliberate choice in Settings → Labs → **Rewind files with Edit from here**.
While it is off, the dialog offers only the conversation rewind, the thread row
reports file rewind as unavailable, and the daemon refuses a request that asks
for files.

## What happens to the history

- The message you chose is removed, together with every later turn.
- Prompts and cuts are read in journal order. A prompt sent after an earlier
  rewind is part of the conversation: editing it removes it and everything
  after it, and never resurrects the prompt the earlier rewind removed. A
  queued message joins the transcript only when it runs, under its own turn.
- The kept conversation stays readable and is what the next turn continues
  from. A handoff keeps a turn's prompt and its reply together even when a
  message queued behind that turn reached the journal first.
- The daemon journals one **rewound** event. A client that reloads later folds
  the same cut, so every device sees the same thread. The prompt itself is
  returned only to the device that asked, in the answer, so a replay never
  reopens a composer someone had already closed.
- If a daemon restarts before the next prompt, the recorded provider boundary
  survives: the next session still starts from the edited history instead of
  silently resuming the old one. The boundary is written as soon as the
  provider accepts the prompt, before the turn can settle, so a busy turn and a
  restart cannot lose it.
- A fork made after a rewind reads the same cut. The removed prompt is absent
  from the fork's transcript and from its handoff history, and a fork at a
  removed turn is refused with a clear message. Removed text — which may be a
  pasted secret — never comes back through a new thread.
- If a rewind's cut cannot be recorded, nothing changes: the files are put
  back and the thread row still describes the conversation that is in the
  journal.

### Harness history

The thread row says what the harness *can* do. The dialog asks the daemon what
the selected message will really use, because an older prompt recorded before
its provider boundary was kept — and a conversation that never started a
provider session — can only use a handoff.

| Harness | Method | What it does |
|---|---|---|
| Codex | Native | On the next session start, resumes the thread and calls `thread/revert` before the removed prompt's own recorded provider turn. A later prompt's turn is never borrowed, because reverting before it would keep the edited message. |
| Claude Code | Native | The next session resumes with `--resume-session-at <message id>`. Every prompt is sent through the tagged frame, and h4rn records the uuid of the **last kept chain entry**: the assistant reply before the removed prompt, or the kept prompt itself when its turn wrote no reply (stopped, or failed first). That flag is inclusive, so the boundary is the last kept message, never the removed prompt. |
| OpenCode, Antigravity, configured ACP agents | Handoff | These harnesses cannot truncate a live conversation through the protocol h4rn speaks. h4rn starts a fresh session and gives it a bounded handoff of the kept history, using the same builder as imported threads. |

When no provider session exists yet (a thread whose first turn never started a
session), a rewind is always a handoff: there is no provider history to
truncate. A queued message becomes part of the conversation when it runs, under
its own turn, so its provider boundary is its own, not the turn that happened to
be running when it arrived.

A message whose delivery is uncertain may already be in the provider
conversation. When a promoted prompt's write fails after the adapter was called,
its turn settles without knowing the outcome, so the daemon shows the text as
**May have been delivered** right away — the same row a restart would recover —
and both the dialog and the daemon refuse an edit until it is resent or
discarded. A claim whose turn is still running is not shown: that turn owns it.
A steer whose adapter write has not returned blocks the edit until it does,
even when its turn settles in that window: the text is not in the journal yet,
and cutting now would let it land after the cut. Once the write comes back the
text is in the turn once, with no uncertain row beside it.

A boundary belongs to the provider session that recorded it. h4rn stores that
session id beside the boundary, and only uses it while the thread still resumes
that same session. When a fallback starts a fresh session, the kept prompts'
old boundaries stay recorded but are no longer native: a later rewind plans a
handoff instead of pairing an old message id with a session that never saw it.

The provider boundary is applied at the **next** session start. The old
session is shut down at rewind time so it can never be sent another prompt.
If the adapter cannot keep its native promise — the provider no longer has the
named turn, the CLI refuses the message id, or no recorded boundary exists —
the daemon falls back to the handoff, clears the refused boundary, journals a
**fell back** event and tells the person, so a plan that named a native rewind
never turns into a silent handoff. Every fallback path journals that event: the
retry that starts a fresh session, a failed startup that has to turn the pending
boundary into a handoff, and a switch to another harness, whose first session
already carries the kept history as a handoff and cannot use the old provider's
boundary at all.

## Files

With **Rewind files with Edit from here** turned on in Settings → Labs, there
are two choices:

- **Revert and keep changes** leaves the workspace exactly as it is.
- **Revert files too** puts the thread's worktree back to the turn snapshot
  taken before the chosen message.

The file choice is only offered while the Labs switch is on, for a thread
running in **its own linked git worktree**, and only when every removed turn has
a complete pre-turn checkpoint. A thread in the project's main checkout rewinds
its conversation only, because restoring a shared checkout would change files
other work sees.

A complete checkpoint is the git tree of the whole tracked worktree captured
just before the turn's prompt reaches the agent. It covers files a shell command
changed even when no tool call named them. Checkpoints are only captured while
the Labs switch is on, so a turn sent while it was off has none; turning the
switch on does not invent one for work that already ran. Turns recorded without
one — an older thread, or a harness that never recorded it — refuse **Revert
files too** with a clear message rather than restoring a partial set.

Restoring is refused when another thread or agent session uses that directory,
a folder inside it, or a folder that contains it. Completed subagents of the
same thread do not block the restore only when they are terminal and have no
queued or active work, no pending approval, question or guest request, and no
background task such as a PR watch.

The dialog warns that **Revert files too also undoes edits you made yourself**
after the chosen message. The restore wants the whole tree at the checkpoint.
A symlink the turn added is removed itself, never its target, and a restore
never writes through a symlinked parent.

The restore writes a durable journal first: the intended changes and a blob of
the bytes each path held before. Then it applies the files, commits the cut and
the journal changes together, and only then publishes the new in-memory state.
If the commit fails, the files are put back from the journal and the
conversation is unchanged. If the daemon stops between the apply and the
commit, the next startup rolls the restore back; a committed cut is kept and
its journal removed. A missing snapshot blob, a file too large to capture, a
path that now escapes the worktree, a target that is now a folder or a failed
write refuses or rolls back the whole rewind.

The worktree must still be the registration the thread was created with: the
same git dir and common dir. A moved worktree whose old path now holds another
checkout is refused. The workspace is reserved from the first file write
through the commit, so a thread or agent session that would start there waits
or is refused.

The switch stays off by default because the file restore still has known gaps.
A later checkpoint can override the one the message names; a rename, a symlink
and a file mode are not carried; a failed capture can be read as a deletion; and
a restore writes bytes only, so ownership and extended attributes are left as
they are. The conversation rewind never depends on any of that, which is why it
is available without the Lab.

## Safety

A rewind is refused while any of these is true:

- a turn is running;
- a message is queued or a prompt is parked behind a pause;
- a message is still being delivered — a steer's write has not returned — so
  the person is asked to try again in a moment;
- a message's delivery is uncertain — a queued row that may already have
  reached the agent, or a promoted prompt whose write is still in flight — so
  the person is asked to resend or discard it first;
- an approval is pending;
- an automatic resume is armed after a usage limit (a cancelled resume does
  not block it, even if the thread's last state still says rate limited);
- the harness session is still starting.

The daemon enforces every refusal, even when a client's summary is stale. The
UI disables the action with the same reasons and shows them in the dialog.
