# Importing Claude Code and Codex conversations

h4rn can read the conversations Claude Code and Codex already saved on this
machine and turn them into threads. An imported thread keeps its transcript,
title, harness, model and working folder; its first new message starts a real
session so you can carry on where you left off.

Nothing here writes to the harnesses' folders. h4rn only reads their transcripts,
and removing an imported thread never touches them.

## Where conversations come from

- **Claude Code** reads `<CLAUDE_CONFIG_DIR>/projects/*/*.jsonl`, defaulting to
  `~/.claude/projects`. `CLAUDE_CONFIG_DIR` is resolved the way Claude Code
  resolves it (a `~` is expanded; a relative value is taken from the process's
  working directory) and an override that cannot be resolved is reported in the
  import list instead of being ignored.
- **Codex** reads `<CODEX_HOME>/sessions/YYYY/MM/DD/rollout-*.jsonl`, defaulting
  to `~/.codex/sessions`. `CODEX_HOME` is resolved the same way.

Conversations are grouped by the folder they ran in. A git checkout groups all of
its conversations, including those in linked worktrees, under the main checkout;
a GitHub `origin` names the group `owner/name`. Folders that are not git
repositories are listed under "Other folders". Conversations under
`~/Downloads` are not offered, including when that folder is a symlink or the
path is spelled with a Windows extended-length prefix. The working folder the
full parse finds is checked against that rule too, not just the one its first
record named.

An imported conversation from a linked worktree keeps the worktree as its
working folder, because that is where a native resume has to run. It is listed
under the main checkout's project: the association is saved with the thread, so
it survives a restart.

The import list is built from what was actually found. An empty list means no
transcripts are there — h4rn never invents placeholders.

## What is read from each conversation

Import is best effort and keeps the useful conversation without tool noise:

- the first user request, plus the newest visible user and assistant messages,
  200 messages in total;
- each kept message's timestamp;
- the conversation's title and working folder, and its model when the transcript
  records one.

Tool calls, attachments, reasoning and harness setup context are left out. For
Codex, a turn's canonical user event wins: when its text matches a response copy
that names a valid turn id, every response copy with that id is dropped — the
generated environment block and the project's private AGENTS.md instructions
included — and a copy that exactly repeats the canonical text is dropped too. A
turn with no canonical event keeps its context, and a copy whose turn id names
another turn is kept, so a repeated prompt after an aborted turn loses neither
turn's text. The canonical prompt itself is kept exactly as written, its leading
indentation and trailing newline included; only the comparison used for dedupe
is trimmed. A rewound Codex rollout declares a byte prefix of the rollout it
replaced, and that prefix streams before the rollout's own records, so the
original request and reply stay in the imported conversation; when the prefix
cannot be read the rollout is imported on its own. For Claude Code, a whole
harness element is recognised by its
content block — a command or notification must be complete, so a prompt that
merely begins with `<task-notification` is still a prompt. Slash commands and
`!` shell commands are kept exactly as typed, while their output, background
task notices and interruption markers are not. A conversation that only ran
commands is skipped; one whose person asked something is kept even when no reply
arrived yet.

Malformed records are ignored, and unreadable or empty conversations are skipped.
Records are streamed one at a time: a record larger than the text worth keeping
is read past its attachment or tool output, so a short prompt is never lost to a
screenshot beside it. Discovery reads the same streamed records, so a prompt
sharing a record with a multi-megabyte attachment is still offered. A conversation is skipped with a warning when its
transcript is over 4 GiB, when the kept text and metadata need more than 32 MiB,
when any single record is longer than 48 MiB — the person's text, a tool result
or an attachment alike, not only user text — or when it has more than 100,000
records. Retention stops the moment that limit is crossed: the kept window and a
turn's pending text are released at once, so one oversized conversation cannot
grow memory toward the size of its file. Each import attempt reads at most 100
conversations and 4 GiB per project, newest first, and only counts conversations
that are not already imported and whose head showed a visible message. Once a
project's quota is full, the rest of that project's Claude Code transcripts are
not read at all; one already imported is still recognised. That shortcut is per
Claude Code folder, because its folder names the working directory. Codex
rollouts live in date folders shared by every project, so each rollout is still
peeked at its head to learn which project it belongs to, and its full transcript
is read only while that project's quota allows. The list says when a limit was
reached.

## Importing

**Add project → Import** lists what was found. Git repositories active in the last
30 days with at least three conversations are checked already; **Review all**
shows everything, with **Select all** and **Select none**. A project that is
already in h4rn is not checked again.

Starting an import adds any project that is new and creates one thread per
conversation. Each source file's identity (its session id, or its path with size
and content fingerprint when it names none) is recorded, so running the import
again only adds conversations h4rn has not seen. The id used for the claim comes
from the full parse, not the scan's head, so two differently named copies whose
session id sits in a later record still import once. The scan also checks the
identity stored for each transcript, not only the one its head revealed, so an
already imported conversation is not listed again. A transcript that named no
id keeps its path, size and content fingerprint as its identity, so one that
keeps growing is offered again as a new conversation. A source is claimed and its
thread written in one transaction: two copies of a conversation in one run, two
runs at once, or a crash between the claim and the write cannot produce a second
thread. Progress appears in the dialog, with any limit a scan hit, and the import
runs in the daemon, so you can close the dialog or leave the view; reopening it
shows the current counts. Imported threads land in Done without raising a hand,
with each message's original time, and a restart restores that finished state
and sorts the thread by its newest imported message rather than the moment h4rn
wrote it.

A long replay crosses a paired connection without shortening anything: the
daemon sends a thread's history in bounded pages, and a frame too large for the
secure transport is split into chunks the client joins before reading it, so a
600 KiB prompt stays whole. The journal decides where each page ends from its
byte budget, so asking for the next page reads one page rather than the rest of
the thread.

## Continuing an imported thread

An imported thread has no live session. Its first new message starts one, in the
conversation's original working folder:

1. **Native resume first.** When the transcript records a session id the harness
   can resume, h4rn asks the harness to resume it (`--resume` for Claude Code,
   `thread/resume` for Codex). That resume is spent once the session starts:
   after that the thread continues on its own live session, so a later startup
   that dies before reporting a session fails that turn as any other thread
   would instead of replaying the imported history over the turns made here.
2. **Handoff when that fails.** If the harness cannot find or resume the saved
   session, h4rn retries the same turn with a bounded handoff of the saved
   history followed by your exact request. Your new message is never shortened.
   A harness that reports success and then exits before it reports a session —
   Claude Code's real behaviour for a bad `--resume` — is treated the same way:
   the dead id is cleared and the turn is retried once with the handoff. A Stop
   that lands while the retry is deciding ends the turn interrupted instead of
   starting anything.

The handoff keeps the original request and the newest messages, in order, with
their roles. The turn's own request is never part of that history block, even
though it is already in the journal by then: it reaches the agent once, as the
request that follows the history. The handoff is bounded in UTF-8 bytes: 16,000
by default, clamped to 1,024–64,000. Set `H4RN_HANDOFF_BYTES` on the daemon to
choose a different size.
The budget covers the whole handoff — the header, every message as it is
serialised, the lead-in and your new request — so the saved history never pushes
your request past it. The newest messages fill the budget from the end, and once
one does not fit the handoff stops there rather than skipping it for an older
one, so the continuation never has a hole where the omitted turns were. The
history is read from the thread's journaled messages, the one copy h4rn keeps
(the imported origin holds only the source facts), so the handoff still works
after a restart without a second transcript being stored. Once the retry opens a
session, later messages resume that session normally.

The handoff builder lives in `crates/h4rn-daemon/src/handoff.rs` so provider
switching and forking can reuse it.

## Privacy

- Transcripts are opened read-only; h4rn never modifies, moves or deletes them.
- Message text is never logged. Import warnings name folders or say that a
  transcript could not be read, not what it said.
- Token-shaped strings in kept messages and titles are redacted with h4rn's
  existing redactor before anything reaches the journal.
- Import requests need the `approve` scope; scanning needs `view`.
