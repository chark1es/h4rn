# Reviewing diffs

The IDE's diff pane answers three different review questions. The picker in the path bar chooses
which one; the daemon reads every answer from git in the thread's working tree. Nothing is cached
from a previous mode and no list is invented.

| Source | What it compares |
|---|---|
| **Uncommitted** | One file against its `HEAD` blob. |
| **Turn** | One file against the snapshot the daemon took when the agent turn first touched it. |
| **Branch** | The working tree against the merge base of a base branch and `HEAD`. |
| **Commits** | One commit against its first parent; a root commit against the empty tree. |

## Branch review

`Branch` shows the work on this branch, not the base's newer commits. The daemon resolves the base
in this order:

1. the upstream's default branch (`refs/remotes/<remote>/HEAD`);
2. that remote's `main`, then `master`;
3. a local `main`, then a local `master`.

With no base at all — no remote, no `main` or `master`, or an unborn `HEAD` — the pane says why.
Choose another branch from the base picker, or switch to **Uncommitted**.

The file list comes from `git diff` against the merge base plus untracked files. It shows the git
status letter, added and removed line counts, and the old path for a rename. Like every list in
IDE mode it stops at 2,000 files and says when it did.

## Commits

`Commits` lists the newest 100 commits in `merge-base..HEAD`, newest first. Selecting one shows its
changed files and their diffs against its first parent. A merge commit is never compared with both
parents: work that came from the second parent belongs to the commit that authored it. A root
commit compares against the empty tree, so every file reads as added.

Commit ids are validated before git sees them: full or abbreviated hex that resolves to a commit in
this repository. A tag, tree or option-looking string is refused with a clear message.

## Review comments

Select a line in any diff mode. Shift-click or Shift+Arrow extends the range. Press `Enter` (or
`c`) to write a comment; `Escape` closes it. Added lines are commented on the new side, removed
lines on the old side, and a mixed range reports the side where it has line numbers.

Pending comments sit under the line they belong to. Edit or delete one in place. The pane header
counts them for the whole thread, so switching files or modes never hides one. The count and the
comments stay in memory for the daemon session; the diff contents themselves are always re-read
from git.

**Send to agent** appends one plain-text context block per comment to the thread's composer draft:

```
Review comment · src/auth/session.ts · new side, lines 12–13 · branch vs main
> 12 + const now = clock.now();
> 13 + expect(now).toBe(fixed);
Comment: Use the injected clock here.
```

It writes the draft and nothing else. The composer shows the text, and nothing reaches the agent
until you press Send. The comments then leave the pane.

## Reading the pane

- A tree row is a keyboard target. `Tab` enters the diff, `ArrowUp`/`ArrowDown` move, `Enter`
  comments, and `Escape` clears the selection.
- The sign column is mandatory: `+` for added lines, `−` for removed ones.
- Binary files and files over the 2 MiB read limit say so instead of showing mojibake. The file
  list marks a binary file as binary.
- Every failure is the daemon's own message: no repository, no base, no common history, or a
  commit that does not exist.
