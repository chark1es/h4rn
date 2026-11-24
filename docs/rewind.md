# Editing an earlier prompt

**Edit from here** sits beneath every sent message. Choosing it rewinds the
thread to just before that message: the message and everything after it leave
the active conversation and the harness's own history. The prompt and its
attachments come back to the composer, ready to edit and resend.

This is a conversation edit, not a time machine. It does not undo external
actions the agent took elsewhere — a push, a deploy, a message to another
system. Only the thread, the harness history and (optionally) the worktree go
back.

## What happens to the history

- The message you chose is removed, together with every later turn.
- The kept conversation stays readable and is what the next turn continues
  from.
- The daemon journals one **rewound** event. A client that reloads later folds
  the same cut, so every device sees the same thread. The prompt itself is
  returned only to the device that asked, in the answer, so a replay never
  reopens a composer someone had already closed.
- If a daemon restarts before the next prompt, the recorded provider boundary
  survives: the next session still starts from the edited history instead of
  silently resuming the old one.

### Harness history

The thread row advertises how its harness drops the removed turns, and the
dialog says which one will run.

| Harness | Method | What it does |
|---|---|---|
| Codex | Native | On the next session start, resumes the thread and calls `thread/revert` with the first removed provider turn. |
| Claude Code | Native | The next session resumes with `--resume-session-at <message id>`. h4rn tags each prompt with the turn's UUID, and it is that id the CLI records. The flag is hidden from `claude --help` but exists in print mode, which every h4rn session uses. |
| OpenCode, Antigravity, configured ACP agents | Handoff | These harnesses cannot truncate a live conversation through the protocol h4rn speaks. h4rn starts a fresh session and gives it a bounded handoff of the kept history, using the same builder as imported threads. |

When no provider session exists yet (a thread whose first turn never started a
session), a rewind is always a handoff: there is no provider history to
truncate.

The provider boundary is applied at the **next** session start. The old
session is shut down at rewind time so it can never be sent another prompt.
If the adapter cannot keep its native promise (for example the provider no
longer has the named turn), the daemon falls back to the handoff and reports
`handoff` in the answer.

## Files

There are two choices:

- **Revert and keep changes** leaves the workspace exactly as it is.
- **Revert files too** puts the thread's worktree back to the turn snapshot
  taken before the chosen message.

The file choice is only offered for a thread running in **its own linked git
worktree**. A thread in the project's main checkout rewinds its conversation
only, because restoring a shared checkout would change files other work sees.

Restoring is refused when another thread or agent session uses that directory,
a folder inside it, or a folder that contains it. Completed subagents of the
same thread do not block the restore as long as they have no queued or active
work, pending requests or background tasks.

The dialog warns that **Revert files too also undoes edits you made yourself**
after the chosen message. The snapshot is the pre-turn state of every file the
removed turns touched; it is not a commit and does not include files those
turns never reported.

The restore happens before the journal write. A missing snapshot blob, a file
too large to capture, a path that now escapes the worktree or a failed write
refuses the whole rewind and leaves the conversation unchanged. Current bytes
are kept in memory while the restore runs and are put back if a later file
fails, so a failed restore does not leave a half-reverted tree.

## Safety

A rewind is refused while any of these is true:

- a turn is running;
- a message is queued or a prompt is parked behind a pause;
- an approval is pending;
- an automatic resume is armed after a usage limit;
- the harness session is still starting.

The daemon enforces every refusal, even when a client's summary is stale. The
UI disables the action with the same reasons and shows them in the dialog.
