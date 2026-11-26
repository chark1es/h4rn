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

A complete checkpoint is the git tree of the whole worktree captured just
before the turn's prompt reaches the agent: every tracked file and every
untracked file git does not ignore. It covers files a shell command changed even
when no tool call named them. The tree keeps git's own modes, so an executable
bit and a symlink's target come back, and a rename becomes the old path restored
plus the new path removed — a staged rename included, because the plan never
asks git to pair the two paths into one. A checkpoint recorded before
checkpoints covered the whole worktree (a tracked-only tree) is not complete.
A file that is there but cannot be read when the checkpoint is taken (a
`private.key` at mode 000, say) makes the checkpoint incomplete, and **Revert
files too** refuses with that file named. Leaving it out would record it as
absent, and the restore would then delete it once the turn made it readable. A
file that disappeared while the checkpoint was being taken is left out, as git
would record it removed. Each path keeps its own bytes even when it ends in a
carriage return next to a path without one (`a.txt` and `a.txt\r`).
Checkpoints are only captured while the Labs switch is on, so a turn sent while
it was off has none; turning the switch on does not invent one for work that
already ran. Turns recorded without a complete checkpoint — an older thread, or
a harness that never recorded it — refuse **Revert files too** with a clear
message rather than restoring a partial set. When the daemon says the choice is
not available, the dialog names the likely reason from the removed turns'
own checkpoint reports — the selected message's turn and every turn after it —
so a worktree that cannot be restored right now is not described as a message
with no checkpoint.

The selected message's own checkpoint decides the restore. A later removed
turn's checkpoint never overrides it: rewinding to before the chosen message
puts the whole removed range back to that one tree. A path absent from a
complete checkpoint is removed, including an untracked file a shell command
created, even when a tool later named it and a capture holds its bytes: the
tree is the earlier, whole-worktree evidence, and a capture is never the base
for restored bytes.

### Ignored files are out of scope

**Revert files too** restores only what git could see at the checkpoint:
tracked files and untracked files git does not ignore. It never writes,
recreates or removes a path that was ignored then, nor anything inside an
ignored folder. Build output, caches and secret `.env` files are not the turn's
work to rewind, and their pre-turn bytes cannot be proved from timestamps: `cp
-p`, `rsync -t`, `tar x` and `mv` keep old modification times, clocks skew, and
a file deleted before a tool named it looks absent. Restoring one of them from a
mid-turn capture would silently invent a state that never existed.

The checkpoint still records which ignored paths (or ignored folders) were
present when it was taken. That listing is how the restore recognises a path
that was ignored then and is visible now — the turn emptied `.gitignore` or ran
`git add -f` — and leaves it exactly as the turn left it. A recorded folder
entry covers everything beneath it. The listing cannot say which of those files
already existed, and the turn may remove the rule and create its own file
there, so nothing under a recorded folder is written, recreated or removed, and
every path the turn touched there is named. Leaving a file the turn created is
safer than deleting one that was already there. Ignoring more than 10,000 paths
makes the listing incomplete, so the checkpoint is incomplete and **Revert
files too** refuses with that reason.

The dialog and the answer name the ignored paths the turn touched (from the
tool calls and captures that named them, and from recorded ignored files that
are gone now) and were left as they are, so the person knows those were not
rewound. This is a plain note, not an error.

### Exact bytes

A checkpoint is taken without conversion: every path is hashed with
`git hash-object --no-filters` and written to a fresh scratch index with
`update-index --index-info`, so no attribute source can convert or filter what
is stored — not `.gitattributes`, not `.git/info/attributes`, and not the global
`core.attributesFile` — and `core.autocrlf` never runs. Restore reads the raw
blob back, so a CRLF file comes back with its CRLF and a filtered file comes
back with its working-tree bytes, whatever the rules say now.

The plan never reads the worktree through git's conversion rules either. It
builds a second tree of the worktree as it is now with the same builder, and
compares the two trees with `git diff-tree -r --no-renames`. A turn that only
changed a file's line endings (`a\nb\n` to `a\r\nb\r\n` under a
`*.txt text eol=crlf` rule in `.git/info/attributes` or a global attributes
file) is a real change, and it is put back. No particular git version is
needed for this.

Regular files are hashed in one `hash-object --stdin-paths` process, so a
checkpoint of a few thousand files takes well under a second instead of one
process per file (about 62 seconds for 3,000 files before). A path the line
protocol cannot carry (one with a newline, or starting with a quote) and every
symlink target are hashed on their own.

The permission bits on disk are kept — only the executable bit is set or
cleared, and it comes from the checkpoint. A file counts as executable by git's
own rule, the owner's execute bit: a file that only its group or others can
run is recorded as `100644`, as git records it. A file the restore has to recreate
gets the bits a capture recorded before the turn, so a 0600 file is never
recreated as 0644; with no recorded bits it gets the conventional 0644 or 0755.
A rollback puts back the exact bits each file had, and never writes through a
link: the bytes go to a temporary file and are renamed into place, so a path
that is now a symlink is replaced rather than followed.

An untracked nested git repository is left out of the plan: git records it as a
gitlink the real index does not have, and it is not a file the restore can put
back. A submodule the real index does name still refuses the restore when its
pointer changed. A folder counts as its own repository only when git run inside
it names that folder as its top level; an uninitialised submodule is an empty
folder, where git would walk up to the parent repository, so the checkpoint
records the real index's pointer for it and it never blocks a restore.

Git paths are relative to the repository root. A thread created in a
subdirectory of the repository resolves them there, and a change outside that
thread's own worktree refuses the restore instead of writing somewhere else.

Restoring is refused when another thread or agent session uses that directory,
a folder inside it, or a folder that contains it. Completed subagents of the
same thread do not block the restore only when they are terminal and have no
queued or active work, no pending approval, question or guest request, and no
background task such as a PR watch.

The workspace is reserved before that check runs, and stays reserved through
the commit. A turn that began before the check is active when the check reads
its thread, so the restore refuses. One that begins after it is refused when its
prompt is claimed — even on a session that is already live, such as an idle
subagent's — or when its session starts, so no prompt reaches an agent in the
tree while the restore writes it. A turn refused this way fails with a note to
send the message again once the rewind finishes. A known trade-off: the
reservation comes first, so a turn on an overlapping checkout that starts in the
moment before the check is refused even when the check then refuses the restore
too. Sending it again works.

The dialog warns that **Revert files too also undoes edits you made yourself**
after the chosen message. The restore wants the whole tree at the checkpoint.
A symlink the turn added is removed itself, never its target, and a restore
never writes through a symlinked parent.

The restore writes a durable journal first: the intended changes and a blob of
the bytes each path held before, with its permission bits. The backup blobs, the
journal and every restored file and folder are flushed to disk before the
journal says the restore is committed; a folder chain the restore creates is
flushed one level at a time in its parent. Then it applies the files, commits
the cut and the journal changes together, and only then publishes the new
in-memory state. If the commit fails, the files are put back from the journal
and the conversation is unchanged; the journal is removed only once that
rollback is on disk, so a failed one is retried at the next startup. If the
daemon stops between the apply and the commit, the next startup rolls the
restore back; a committed cut is kept and its journal removed. Both a live
rollback and startup recovery walk every recorded parent without following a
link and prove the nearest real folder is still inside the journal's root; a
folder that has become a symlink refuses the rollback, which then keeps its
journal. The root itself is checked the same way: a recorded root that is now a
symlink refuses the rollback rather than writing into whatever it points at. A missing blob, a
file too large to capture, a failed `git` read, a path that now escapes the
worktree, a target that is now a folder, a failed unlink and a failed write
refuse or roll back the whole rewind. A failed `git` read is never read as a
deletion.

The answer reports what the restore really did: it says files were restored only
when it wrote or removed at least one path. A path that already holds the
checkpoint's bytes and mode is neither rewritten nor counted, so a turn that
changed nothing leaves the worktree as it was and says so.

The worktree must still be the registration the thread was created with: the
same git dir and common dir. A moved worktree whose old path now holds another
checkout is refused. Startup recovery checks the same identity, and that the
journal's saved root is still the workspace's own repository root, before it
touches anything a journal recorded; it leaves the files and the journal alone
when they do not match. A journal that cannot be read is not treated as an
uncommitted restore: recovery refuses to roll anything back, keeps the journal,
shows the error on its thread (the journal's name carries the thread id, so it
is still known) and carries on booting, so one bad journal never keeps the whole
daemon down. The workspace is reserved from the first file write through the
commit, so a thread or agent session that would start there waits or is
refused.

The switch stays off by default while the file restore is still being hardened.
A restore keeps the file's own permission bits and git's executable bit only, so
ownership and extended attributes are left as they are; ignored files are never
touched; and the checkpoint and the restore both use raw bytes, so conversion
rules and filters do not alter what comes back. A changed submodule pointer
refuses the restore, while an untracked nested repository is left alone. The
conversation rewind never depends on any of that, which is why it is available
without the Lab.

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
