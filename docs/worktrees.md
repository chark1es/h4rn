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
repository. The project folder is named after the real main worktree (or the
bare repository's git directory), wherever that is: h4rn resolves the checkout
from `core.worktree`, from the directory whose `.git` file points at the common
git directory, or from a nearby directory or registration whose own `.git` file
proves it is that checkout. A folder that git reports as the main worktree only
because its git directory is named `.git` counts only when it really holds the
checkout: the workspace's own git directory is that git directory, or a file
the repository tracks is there as the folder's own file. The workspace merely
sitting inside the folder is not enough on its own, because a linked worktree
can sit inside the parent of a metadata directory named `.git`. A tracked name
counts only when it resolves inside the folder and outside the workspace and
every registered worktree below the folder: with the metadata directory at
`meta/.git` and a linked worktree at `meta/docs`, the tracked `docs/README.md`
is that worktree's own file, not a file `meta` holds. A linked worktree nested
inside its real main checkout (`repo/.worktrees/one`) still passes, because that
checkout holds the tracked files itself, and so does a main checkout moved
inside one of its own linked worktrees. As a last check no folder on the destination's way up
may belong to the same repository. Metadata directories such as `api.git` or an
admin directory are never mistaken for a working tree, and a linked worktree is
never accepted as the main one. When none of those can prove where the main
checkout is, h4rn refuses the location instead of guessing; the refusal says the
main checkout could not be found and never names the metadata directory. One
reading of the repository decides both the project folder's name and the
destination that is checked and created, so a location that passes the check is
exactly the one a checkout goes into. The destination is refused inside the
workspace, the main checkout, or any other linked worktree of the same
repository, because a checkout there would make that working tree dirty. The
check uses one complete worktree snapshot; if git cannot read an admin entry,
the location is refused instead of guessing that the missing worktree is absent.
All of these are refused when the setting is saved and when a worktree is
created, because a person should learn about it while they can still change the
location.

A repository whose `HEAD` tree has no files (only `--allow-empty` commits)
cannot prove its main checkout from a linked worktree: no tracked file exists to
show which folder holds the working tree, and a metadata directory's parent has
the same `.git` shape as a checkout. Until the repository tracks a file (or
names its main checkout with `core.worktree`), h4rn refuses every location for
that project rather than guess. Commit one file and the same location works.

**Settings → Worktrees → New worktrees → Location** sets another directory:

- an absolute path, such as `/mnt/worktrees`, or
- a path starting with `~/`, such as `~/worktrees`.

The setting is per machine, because it is a path on that host. It applies to
new worktrees only; existing checkouts stay where they are. Deleting or
archiving a thread never moves or removes its checkout. h4rn keeps a hidden
`.claims` folder beside the checkouts, with one small lock file per destination,
so two creates for the same folder take turns instead of racing; each file also
names the create that last used the folder, so recovery can prove ownership
after a crash. The files are tiny and safe to leave alone.

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

Only one daemon may use a data folder at a time. It takes an operating-system
lock when it starts, before restoring threads or finishing any worktree
recovery, and holds it until shutdown. A second daemon started on the same data
folder exits without adopting, clearing or restoring anything.

Before h4rn changes a repository, it writes a small recovery intent beside its
own data. The file is written atomically (a synced temporary file renamed into
place), so a crash leaves a complete record or the previous one, never half a
JSON file. The record names the checkout, branch and a random owner token. h4rn
then registers the worktree without checking it out, writes that token into the
registration's admin dir, and only then populates the working tree. A crash in
the checkout hook therefore leaves both the token and the record.

If the daemon shuts down, it closes create admission, waits for every running
create to adopt its checkout or start its rollback, and then waits for those
rollbacks. If the daemon is killed first, the next startup finishes each record
only with positive proof. While the checkout is still there, the proof is the
checkout's `.git` naming the recorded admin dir, that admin dir's `gitdir`
pointing back at the checkout, its `HEAD` being the recorded branch, and the
owner token matching. The same token is written into the destination folder's
claim file before `git worktree add` runs, so a kill between the add and the
marker in the admin dir is still proved by it. A checkout folder that is gone
while its admin dir and marker remain — deleted by hand, or a kill inside
`git worktree remove` — is finished from the admin dir alone: its `gitdir` still
names the path and its `HEAD` the branch, and git removes a registration whose
folder no longer exists. A record the claim proves, with no admin dir at all and
a destination that is gone or still empty, is finished the same way: the
destination folder is removed if it is empty, the branch h4rn made is deleted
under the usual guards, and the record is retired. Anything in the destination
is left alone and the record waits. Before `git worktree remove` runs, the
record is marked as removing, so a kill between the removal and the write that
records it still finishes the branch step instead of retiring the record and
leaving the branch behind. Once the checkout has been removed and that step was
recorded, the record itself is the proof, because it says this create removed
its own checkout; the startup then deletes the branch h4rn made only if that
branch is not registered, is not checked out anywhere, and still points at the
commit the create started from, and retires the record. A record that cannot be
proved is kept and left alone. A record whose path a thread now uses is retired
without touching the checkout; a thread working in a parent folder does not
count as using the checkout, so an unused one is still cleaned up. Recovery
never runs a repository-wide `git worktree prune`: a moved worktree you made
yourself keeps its registration. Cleanup is retried until the checkout, its
registration and the branch h4rn made are actually gone; a locked worktree, or
a branch deletion git refuses, keeps its record until a later start succeeds. A
busy folder is deferred rather than stalling startup, and only a start that
really removed a checkout or branch reports it as swept.

The identity of a checkout is checked again immediately before h4rn asks git to
remove it, and the destination folder's claim is held for the checks, the
removal and the branch step. One window remains: a separate git process could
remove the checkout and put another one at the same path in the moments between
that last check and `git worktree remove`. h4rn cannot hold that process off.
It never acts on a path it has not checked in this way, and it never prunes
registrations it did not make.

The claim token has a limit of its own. It says which create last used the
folder, but it cannot tell a checkout a person later made on the same branch at
the same path apart from the interrupted create's checkout: both leave the same
branch at the same path while the claim still names that create. Recovery uses
the token when the registration carries no owner marker (and then also requires
the branch and path to match the record), or when there is no registration at
all and the destination is gone or empty; in every case a branch is deleted only
when it is unregistered, not checked out, and still at the commit the create
started from. That combination is still indistinguishable from a person's own
checkout on the same branch. The confusion lasts only while the folder's claim still names
that create; the next create for the folder rewrites it.

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
