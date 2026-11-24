# Thread worktrees

A new thread can run in its own git checkout instead of the project's main
folder. The branch and checkout are separate, so two threads can build and test
at once without fighting over one working tree, and the main checkout is never
written to. The design is recorded in
[ADR-022](decisions.md#adr-022-checkpoints-are-git-objects-worktrees-isolate-threads).

## Starting one

In the New thread dialog, choose **Run in a new worktree**. The option is
offered for a registered git project on the thread's host. Leave the branch
field empty and the daemon generates a name; type one to use your own.

The daemon creates the worktree from the project's current `HEAD` before the
first session starts, then makes that checkout the thread's workspace. Files,
Changes, terminals and every later turn use it. The thread header and sidebar
row show the branch.

A worktree request fails with a clear message when:

- the folder is not a git repository (or git is not installed);
- a named branch is already checked out in another worktree, including the main
  checkout;
- the named branch is not a valid git branch name.

The main checkout is only read: h4rn runs `git worktree add` against it and never
changes its index or working tree.

## Where they go

By default: `<data_dir>/worktrees/<project>/<branch>`. `data_dir` is h4rn's data
folder on that machine. A `/` in the branch name becomes `-` in the folder
name, so `feat/login` lives at `.../worktrees/<project>/feat-login`; each
branch still gets one folder of its own.

**Settings → Worktrees → New worktrees → Location** sets another directory:

- an absolute path, such as `/mnt/worktrees`, or
- a path starting with `~/`, such as `~/worktrees`.

The setting is per machine, because it is a path on that host. It applies to
new worktrees only; existing checkouts stay where they are. Deleting or
archiving a thread never moves or removes its checkout.

## Branch names

A generated name is the configured prefix plus a short slug from the first
prompt, for example `h4rn/fix-the-login-loop`. The prefix defaults to `h4rn/`.

- Invalid characters become hyphens.
- The trailing slash is optional; `feat` and `feat/` both produce `feat/...`.
- An empty prefix adds nothing.
- If the name is taken, a short numeric suffix is added (`...-2`, `...-3`).

A branch you name is used as written. An existing branch is attached rather
than recreated, so its history is the one you asked for. Only a branch h4rn
created is deleted if thread creation fails after the checkout was made.

Repositories with submodules get them initialised recursively, so the agent
does not work in an empty-looking folder. A repository whose `.gitmodules`
names local paths is allowed to use git's `file` transport for that one
command; repositories with network remotes keep git's stricter default.

## Tracking and clearing

New thread worktrees appear in **Settings → Worktrees**, owned by their thread
(origin `thread`), with the branch and path. The safe-clear rules are the same
as for every tracked worktree:

- an active session or a thread working in it prevents removal;
- uncommitted changes prevent a plain clear;
- a thread workspace is only removed when named explicitly, never by **Clear
  all**.

Deleting or archiving a thread leaves its checkout alone, and the row stays in
Settings so you can clear it there afterwards. Clear it when its work is done.
The branch is kept, so commits stay reachable.

## Remote hosts

A thread's worktree is created by the daemon on the thread's host, using that
machine's data folder and Settings → Worktrees values. A daemon only creates
threads for its own host today; when a client addresses another host, that
host's daemon is the one that creates the checkout.
