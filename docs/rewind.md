# Editing an earlier prompt

**Edit from here** sits beneath every sent message. Choosing it rewinds the
thread to just before that message: the message and everything after it leave
the active conversation and the harness's own history. The prompt and its
attachments come back to the composer, ready to edit and resend.

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
  after it, and never resurrects the prompt the earlier rewind removed.
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
| Codex | Native | On the next session start, resumes the thread and calls `thread/revert` before the first **provider turn h4rn really delivered** for a removed prompt. |
| Claude Code | Native | The next session resumes with `--resume-session-at <message id>`. Every prompt is sent through the tagged frame, and h4rn records the uuid of the assistant reply **before** the removed prompt. That flag is inclusive, so the boundary is the last kept message, never the removed prompt. |
| OpenCode, Antigravity, configured ACP agents | Handoff | These harnesses cannot truncate a live conversation through the protocol h4rn speaks. h4rn starts a fresh session and gives it a bounded handoff of the kept history, using the same builder as imported threads. |

When no provider session exists yet (a thread whose first turn never started a
session), a rewind is always a handoff: there is no provider history to
truncate. A queued message is journaled under the turn that finally runs it, so
its provider boundary is its own, not the turn that happened to be running when
it arrived.

The provider boundary is applied at the **next** session start. The old
session is shut down at rewind time so it can never be sent another prompt.
If the adapter cannot keep its native promise — the provider no longer has the
named turn, the CLI refuses the message id, or no recorded boundary exists —
the daemon falls back to the handoff, clears the refused boundary, journals a
**fell back** event and tells the person, so a plan that named a native rewind
never turns into a silent handoff.

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
changed even when no tool call named them. Turns recorded without one — an
older thread, or a harness that never recorded it — refuse **Revert files too**
with a clear message rather than restoring a partial set.

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
- an approval is pending;
- an automatic resume is armed after a usage limit (a cancelled resume does
  not block it, even if the thread's last state still says rate limited);
- the harness session is still starting.

The daemon enforces every refusal, even when a client's summary is stale. The
UI disables the action with the same reasons and shows them in the dialog.
