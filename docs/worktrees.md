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
- the named branch is not a valid git branch name;
- the thread's ACP agent is configured with a fixed directory, so it would
  ignore a new checkout.

The main checkout is only read: h4rn runs `git worktree add` against it and never
changes its index or working tree.

## Where they go

By default: `<data_dir>/worktrees/<project>/<branch>`. `data_dir` is h4rn's data
folder on that machine. A `/` in the branch name becomes `-` in the folder
name, so `feat/login` lives at `.../worktrees/<project>/feat-login`; each
branch still gets one folder of its own. A name that maps to a folder which
already exists is refused: a branch you name reports that the folder is taken,
while a generated name moves to the next suffix (`...-2`). h4rn never reuses an
existing folder, because it may hold another thread's checkout.

The location must be outside the project's own checkout and its main checkout,
and so must the folder h4rn puts a project's checkouts in. Checkouts go to
`<location>/<project>/<branch>`, so an inside path (including the project folder
itself) is refused, and so is the main checkout's parent folder (`~/code` for
the project `~/code/api`): that would put the new checkout in the main checkout.
The same holds when the thread starts in a linked worktree, in a worktree of a
`--separate-git-dir` repository or of a submodule, or in a worktree of a bare
repository. The project folder is named after the main worktree (or the bare
repository's git directory), wherever that is: h4rn reads the repository's own
worktree registrations, so the main checkout is found even when its git
directory is not named `.git`. The destination is refused inside the workspace,
the main checkout, or any other linked worktree of the same repository, because
a checkout there would make that working tree dirty. All of these are refused
when the setting is saved and when a worktree is created, because a person
should learn about it while they can still change the location.

**Settings → Worktrees → New worktrees → Location** sets another directory:

- an absolute path, such as `/mnt/worktrees`, or
- a path starting with `~/`, such as `~/worktrees`.

The setting is per machine, because it is a path on that host. It applies to
new worktrees only; existing checkouts stay where they are. Deleting or
archiving a thread never moves or removes its checkout. h4rn keeps a hidden
`.claims` folder beside the checkouts, with one small lock file per destination,
so two creates for the same folder take turns instead of racing; the files are
tiny and safe to leave alone.

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
Cleanup after a failed request only touches the checkout and branch that
request made: it is tied to the git registration the request created, so a
newer thread's checkout at the same folder is never removed. The destination
folder's lock is held through that cleanup, and a create re-reads the branch
once it holds the lock, so a retry of the same branch either finishes before
the cleanup or recreates the branch after it; it never loses a branch the
cleanup removed while it was waiting.

Repositories with submodules get them initialised recursively, so the agent
does not work in an empty-looking folder. A repository whose `.gitmodules`
names local paths is allowed to use git's `file` transport for that one
command; repositories with network remotes keep git's stricter default.

While a create is unfinished, h4rn keeps a small recovery record beside its own
data. If the daemon shuts down, it waits for the failed create's rollback
instead of leaving it half done. If the daemon is killed first, the next startup
finishes the rollback: it removes only a checkout whose git registration still
carries the owner marker h4rn wrote when it made that checkout, and only when no
thread uses it. A worktree you made yourself, and one a thread works in, are
never touched by that sweep.

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
