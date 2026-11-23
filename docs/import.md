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
Codex, the persisted turn boundaries end each turn: a generated copy of a user
turn is dropped only when it names the same valid turn id as the canonical user
event with the same text. A repeated prompt after an aborted turn therefore
keeps both turns' text, and ambiguous context stays so no user text is lost. For
Claude Code, a whole harness element is recognised by its content block — a
command or notification must be complete, so a prompt that merely begins with
`<task-notification` is still a prompt. Slash commands and `!` shell commands are
kept exactly as typed, while their output, background task notices and
interruption markers are not. A conversation that only ran commands is skipped;
one whose person asked something is kept even when no reply arrived yet.

Malformed records are ignored, and unreadable or empty conversations are skipped.
Records are streamed one at a time: a record larger than the text worth keeping
is read past its attachment or tool output, so a short prompt is never lost to a
screenshot beside it. A conversation is skipped when its transcript is over
4 GiB, when the kept text and metadata need more than 32 MiB, or when it has
more than 100,000 records; the pending text of one turn is bounded by the same
32 MiB. Each import attempt reads at most 100 conversations and 4 GiB per
project, newest first, and only counts conversations that are not already
imported and whose head showed a visible message. The list says when a limit was
reached.

## Importing

**Add project → Import** lists what was found. Git repositories active in the last
30 days with at least three conversations are checked already; **Review all**
shows everything, with **Select all** and **Select none**. A project that is
already in h4rn is not checked again.

Starting an import adds any project that is new and creates one thread per
conversation. Each source file's identity (its session id, or its path with size
and content fingerprint when it names none) is recorded, so running the import
again only adds conversations h4rn has not seen. A source is claimed and its
thread written in one transaction: two copies of a conversation in one run, two
runs at once, or a crash between the claim and the write cannot produce a second
thread. Progress appears in the dialog, with any limit a scan hit, and the import
runs in the daemon, so you can close the dialog or leave the view; reopening it
shows the current counts. Imported threads land in Done without raising a hand,
with each message's original time, and stay there after a restart.

## Continuing an imported thread

An imported thread has no live session. Its first new message starts one, in the
conversation's original working folder:

1. **Native resume first.** When the transcript records a session id the harness
   can resume, h4rn asks the harness to resume it (`--resume` for Claude Code,
   `thread/resume` for Codex).
2. **Handoff when that fails.** If the harness cannot find or resume the saved
   session, h4rn retries the same turn with a bounded handoff of the saved
   history followed by your exact request. Your new message is never shortened.
   A harness that reports success and then exits before it reports a session —
   Claude Code's real behaviour for a bad `--resume` — is treated the same way:
   the dead id is cleared and the turn is retried once with the handoff.

The handoff keeps the original request and the newest messages, in order, with
their roles. It is bounded in UTF-8 bytes: 16,000 by default, clamped to
1,024–64,000. Set `H4RN_HANDOFF_BYTES` on the daemon to choose a different size.
The budget covers the whole handoff — the header, every message as it is
serialised, the lead-in and your new request — so the saved history never pushes
your request past it. The journal keeps the conversation as it was imported
(only the hard 32 MiB kept-text limit applies there); the handoff applies the
64,000-byte ceiling when it builds the continuation. Once the retry opens a
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
