# Reviewing diffs

The IDE's diff pane answers four different review questions. The picker in the path bar chooses
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

A `refs/remotes/<remote>/HEAD` that no longer resolves to a commit (its branch was deleted or never
fetched) is skipped, and the search continues with that remote's `main` or `master`. With no base at
all — no remote, no `main` or `master`, or an unborn `HEAD` — the pane says why and offers **Compare
with HEAD**, which switches to Uncommitted. Choose another branch from the base picker, too.

The file list comes from `git diff` against the merge base plus untracked files. It shows the git
status letter, added and removed line counts, and the old path for a rename. Like every list in
IDE mode it stops at 2,000 files and says when it did; untracked files stop at the same cap. An
untracked file has no git numstat, so the daemon reads it to count lines, within a 2 MiB budget for
the whole untracked pass. Files past that budget are still listed with `no counts`; a binary or
unreadable file is marked `binary`. Neither is shown as an empty added file.

## Commits

`Commits` lists the newest 100 commits in `merge-base..HEAD`, newest first. Selecting one shows its
changed files and their diffs against its first parent. A merge commit is never compared with both
parents: work that came from the second parent belongs to the commit that authored it. A root
commit compares against the empty tree, so every file reads as added. A rename is followed to its
old path in the parent, so moving a file reads as a rename with a real diff, not as a deletion plus
an addition. A commit review reads the commit's two blobs; a large working copy does not affect it.

In a shallow clone the oldest commit has no parent object. Selecting it says that the parent is not
in the clone instead of pretending every file was added; fetch more history and try again.

Commit ids are validated before git sees them: full or abbreviated hex that resolves to a commit in
this repository. A tag, tree or option-looking string is refused with a clear message.

Both wider scopes re-read their lists when you enter them, and the **Refresh** button in the diff bar
reads the branch and commit lists again. A commit the agent made a moment ago appears without a
reload.

## Review comments

Select a line in any diff mode. Shift-click or Shift+Arrow extends the range. Press `Enter` (or
`c`) to write a comment; `Escape` closes it. Added lines are commented on the new side, removed
lines on the old side, and a mixed range reports the side where it has line numbers.

A comment sits inline under its line only while the pane shows the source it was written in.
Comments for the same file but another source (or on lines hidden by collapsed context) are listed
below the diff with the source each was written in, so nothing is silently moved to a line it never
described. Edit or delete one in place. The pane header counts the thread's pending comments, so
switching files or modes never hides one. Comments live in page memory for the thread; a reload
clears them, while diff contents are always re-read from git.

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
- Binary files and files over the 2 MiB read limit say so instead of showing mojibake, in the diff
  body only: the file list, source bar and comment actions stay usable, so another file or source
  is one click away. Both sides of `HEAD` and commit diffs are size-checked before they are read.
- The file list marks a binary or unreadable file as `binary`, and a file the daemon did not read
  for line counts as `no counts`.
- Every failure is the daemon's own message: no repository, no base, no common history, a commit
  that does not exist, or a shallow clone whose parent is missing.
