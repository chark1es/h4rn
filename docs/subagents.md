# Cross-harness subagents

A subagent is a **child thread**: an ordinary h4rn conversation with a
`parent_thread_id`. It gets its own harness session, journal, usage accounting,
approvals and resume id, and can run on any harness this daemon can launch —
Claude Code, Codex, OpenCode or Antigravity — regardless of the parent's
harness. The design is recorded in
[ADR-038](decisions.md#adr-038-a-subagent-is-a-child-thread-not-a-second-session-type)
and
[ADR-039](decisions.md#adr-039-delegation-policy-is-enforced-by-the-daemon-not-the-parent).

The child always runs on the same **host** as the parent. Cross-host delegation
is not implemented yet: a request naming another host fails with `Unsupported`
before any thread is created, and the Delegate dialog lists other online hosts
but disables them with that reason.

## Using it

### From a parent agent

Every harness session is injected with the `h4rn` MCP server. A model calls the
five delegation tools under that namespace, for example `h4rn__spawn_subagent`:

| Tool | Purpose |
| --- | --- |
| `spawn_subagent` | Create the child, start its first turn, return its id immediately. |
| `get_subagent_status` | Read state, budget counters and the current activity. |
| `get_subagent_result` | Wait up to `wait_secs` (default 30, max 180) and return the handoff. |
| `cancel_subagent` | Interrupt the child and clean up its worktree. |
| `list_subagents` | List this thread's children, newest first. |

Spawn arguments:

```json
{
  "task": "Add the retry loop and run the tests",
  "harness": "codex",
  "model": "gpt-5-codex",
  "isolation": "worktree",
  "context": {"kind": "files", "paths": ["src/retry.rs"], "summary": "See the failing test"},
  "budget": {"max_turns": 12, "max_wall_time_secs": 600, "max_tokens": 20000}
}
```

`harness` defaults to the parent's harness; `model` defaults to the parent's
model; `isolation` is `shared` (default) or `worktree`. There is no host
argument: the child runs on the parent's host. Context is one of:

- `{"kind":"none"}` — only the task text;
- `{"kind":"summary","summary":"..."}` — the task plus a caller-written handoff;
- `{"kind":"files","paths":[...],"summary":"..."}` — the task plus selected
  workspace files (128 KiB total, paths may not escape the workspace).

The child does **not** see the parent conversation. The parent must put
everything the child needs into `task` or `context`.

### From the app

The subagents panel on a thread uses the same daemon operations through the
protocol envelope `ClientRequest::Subagent`:

- `ListSubagents { parent_thread, include_finished }`
- `GetSubagent { subagent_id }`
- `CancelSubagent { subagent_id }`
- `SpawnSubagent { request }` — the Delegate button

Live progress arrives as `SubagentUpdated`; the parent's journal receives a
durable `Delegated` item when the child is created and `SubagentFinished` when
it settles. Child threads are hidden from the top-level thread list and inbox
unless a client asks for `include_children` (or opens the parent). The parent
stays in **Working** while its own turn is blocked in a delegation tool, which
is what "waiting on a child" means.

## Identity and visibility

- `SubagentId` is stable (`sa_` + UUID v7) and stored on the child thread's
  metadata, so the child thread and its journal survive a daemon restart.
- The supervisor's record (budget counters, current activity, result) is
  persisted to `subagents.db` in the daemon data directory on every mutation
  and rehydrated before the watcher starts, so a restart rebuilds the
  `Delegated` card, statuses and results. A child that was running when the
  daemon stopped is restored as `failed` with an explicit reason; terminal
  records are restored verbatim.
- The child thread id is pre-allocated so early harness events can be matched to
  the record even before `create_thread` returns.
- `ThreadSummary.parent_thread_id` is persisted in the event journal's thread
  metadata; `ThreadFilter.include_children` is the only way a top-level listing
  sees child rows.

## Isolation

- **shared** — the child runs in the parent's workspace. Edits are visible to
  both immediately. Use this for read-only investigation and for work the
  parent will review as it happens.
- **worktree** — the child gets `git worktree` at
  `<data_dir>/subagents/<subagent id>/worktree` on branch `h4rn/sa-<short id>`.
  On success the worktree is removed and the branch is kept so the parent can
  merge or inspect it. On cancel or a budget overrun the branch is deleted too.

There is no automatic merge. A parent that wants the work uses the branch name
shown in the subagent view.

## Policy

The daemon, not the parent model, enforces:

- **Harness availability** — the harness must have an installed adapter on this
  daemon's host. Failures list the available harnesses.
- **Nesting depth** — `DEFAULT_MAX_DEPTH` is 2: a top-level thread may spawn
  children and grandchildren, not great-grandchildren. The depth is derived
  from persisted `parent_thread_id` links.
- **Budgets** — every child has `max_turns`, `max_wall_time_secs` and
  `max_tokens`. Omitted fields use daemon defaults (20 turns, 900 s, 50k
  tokens); `0` is rejected. A run that exceeds any budget is interrupted and
  marked `BudgetExceeded` with a redacted reason in its result.
- **Hosts** — only the local host is supported in this phase. A non-local host
  returns a clear `not_implemented`-style unsupported error with a TODO for the
  remote path.
- **Approvals** — the child inherits the parent's runtime mode. A child's
  approval is mirrored onto the parent thread as `sa:<approval id>` with a
  `[Subagent]` prompt prefix and the child's `SubagentId`. Answering either the
  child's id or the parent's namespaced id reaches the same harness session and
  resolves both. When a child settles, any still-pending mirrored hand is
  cleared so the parent inbox cannot get stuck.
- **Credentials** — the child reuses the parent session's credential and model
  account. It never receives the parent's MCP bearer token; the daemon mints a
  token for the child session because the child is a normal thread.

## Budget accounting

`SubagentProgress` carries turns started/completed, cumulative tokens and
elapsed milliseconds. Turns are counted from the child's `TurnStarted`; tokens
are read from the metered ledger (`ThreadManager::metered_tokens`), which is the
same source of truth as budgeting elsewhere (ADR-024), so a budget is enforced
against metered usage rather than the thread projection's last report. The
wall-clock is refreshed by the supervisor tick and excludes time spent paused
while the child is `scheduled`. When the child settles, its last assistant
message, usage and worktree file changes are folded into `SubagentResult`.

Child turns are recorded in the ledger with their `parent_thread_id`. A
`GetUsage`/`ReportQuery` for the parent can pass `include_children = true` to
roll child usage into the parent's totals; the default reports only the
parent's own turns.

## Failure modes

| State | Meaning |
| --- | --- |
| `starting` | The thread row exists; the harness session has not started. |
| `running` | The child is working. |
| `awaiting_approval` | The child is blocked on a hand shown on the parent. |
| `scheduled` | The child hit a usage limit; auto-resume is armed and the parent sees the wait instead of a failure. |
| `completed` | The child finished; `result` is available. |
| `failed` | The child's turn failed or the harness errored. |
| `cancelled` | The user or parent cancelled it. |
| `budget_exceeded` | A turn, token or wall-time cap was hit. |

`spawn_subagent` validates the task, harness, budget and isolation before it
creates anything. A worktree spawn on a non-repository workspace fails with a
hint instead of silently running shared.

## Tracking and clearing worktrees

h4rn keeps track of the git worktrees it makes and the ones its threads use, so
Settings → Worktrees can show what is in use and clear the leftovers.

A tracked worktree has one of three origins:

- `subagent`: h4rn created it for a worktree-isolated child. h4rn owns the
  directory and its `h4rn/sa-*` branch.
- `thread`: a thread's (or subagent child's) workspace is a linked worktree h4rn
  did not create, for example one made by an editor.
- `harness`: a linked worktree of a thread's repository that the agent or its
  harness-native subagents created or touched during a turn (Claude Code's
  `.claude/worktrees/agent-*`, or a plain `git worktree add`).

Its status is `active` while a subagent is not finished or a thread that used
it is busy: a turn is in flight (from the moment it begins until it completes,
whatever the agent state says, because a failed or denied tool call reports
`Error` while the turn keeps running), pinned or never-run (the Working tab
with an idle agent), or its agent state is queued, scheduled, thinking,
searching, reading, editing, running a tool, awaiting approval or rate-limited.
Done, failed and unreachable threads are not busy on their own, even inside the
Working tab's exit grace.

`settled` means nothing running uses it, and `orphaned` means an owned
worktree's subagent is finished but its directory is still on disk (cleanup
failed, or the daemon restarted mid-run). An entry is not listed when its
directory is gone, its branch (for owned entries) is gone and no registration of
its own remains. While its exact registration remains it is still listed with
`exists: false`, so a clear can remove that registration instead of reporting
the path as not tracked; an owned entry in that state whose branch is gone is
listed with no `branch`, so nothing promises a branch that is gone. A row is
never hidden because git could not answer: an owned
entry whose directory is missing is dropped only when a successful branch
lookup shows its branch gone and a successful snapshot shows no registration.
If either lookup fails (git errors, or `for-each-ref` warns on stderr about a
ref it skipped), the row stays with its recorded branch. The same holds when
the owned entry's repository cannot be found at all (its directory is gone and
neither the recorded repository, the parent's workspace nor
`rev-parse --git-common-dir` leads to one): the row stays with its recorded
branch and an empty `repo`, and clearing it is skipped as
`its repository could not be found`.

Paths are compared after normalisation: the path is canonicalised exactly as
written before any `..` is folded, so a workspace spelled through a symlink
still resolves to the checkout the filesystem says it is. Only a missing path
canonicalises its longest existing ancestor and folds the unresolved suffix
lexically. Two places deliberately do not follow a final symlink. A stored
observation is listed under exactly its stored path, never re-canonicalised,
and registration snapshot paths keep their own last component when it has
become a symlink: a registered checkout replaced by a symlink to some other
worktree keeps its own name rather than turning into the other worktree's. And
`exists` is true only for a real directory: a symlink at a listed path (to
anything) is listed with `exists: false`.

**What is stored.** Only observations that cannot be derived live go to
`<data_dir>/worktrees.db`: `harness` worktrees and which threads used them.
Subagent worktrees come from the supervisor and thread workspaces from the
thread list, and the list is recomputed on every request. An observed worktree
whose directory is missing is still listed, with `exists: false`, while its own
registration remains in the repository's snapshot: its registration is exactly
what a clear removes. It is forgotten only when neither the directory nor the
registration exists, and only after a successful snapshot proves the
registration absent: a failed snapshot (a broken repository, a killed git)
keeps the row with `exists: false` rather than guessing. A stored row whose
repository directory itself no longer exists can never be cleared through git,
so it is forgotten from the store (nothing on disk is touched). Inside the
daemon, registration snapshots (the `-z` parser and the metadata fallback alike)
keep every path byte-exact, valid UTF-8 or not, and containment, the
exact-registration lookup and branch occupancy all use that raw snapshot: a
branch checked out in a worktree whose path is not UTF-8 is still seen as
checked out and never deleted. Only at the protocol edge are paths filtered:
the protocol carries paths as strings, so a row sent to a client, or an
observation stored for one, whose path is not valid UTF-8 is skipped, never
offered lossily and never stored.

**How harness worktrees are observed.** The daemon snapshots
`git worktree list --porcelain -z` for a thread's repository when a turn starts
and again when it completes; any
linked worktree that appeared in between is recorded against that thread (and its
subagent id, for a child). Two turns on the same repository can overlap, though
(for example a parent and its child), and then a worktree that appeared belongs
to neither turn alone: both snapshots are marked shared and the newcomer is not
attributed. Tool-call inputs are also scanned for absolute paths (`cwd`,
`file_path`, shell commands and so on); a path inside a linked worktree of the
thread's repository records an access, which is evidence a turn diff cannot
fake. The per-repository worktree list is cached and refreshed at most every two
seconds, so tool calls do not shell out to git one by one. Every git call the
watcher makes — resolving a thread's repository, its checkout top level, and
listing a repository's worktrees — is bounded to ten seconds, so one wedged
repository cannot stall observation for every thread; a timeout is logged and
that event's work is dropped. The NUL-delimited form requires git 2.36+. Only
a git that clearly rejects `-z` triggers the fallback: an `unknown switch` or
`unknown option` error naming `z`, or exit status 129 (git's usage exit) with
`worktree` in the usage text; a bare `usage:` from anything else, a timeout or a
repository error is reported as a failure. On such a git the daemon reads git's
registration metadata directly instead of parsing output, which is byte-exact on
every version: `<common>/worktrees/*/gitdir` holds the path of the linked
worktree's `.git` file (only its final newline is stripped; the worktree is its
parent), `<admin>/HEAD` is `ref: refs/heads/<b>` or a detached sha,
`<admin>/locked` marks a lock, and a registration is prunable when its checkout
path is not a real directory, checked without following symlinks (a symlink
there is not a live checkout, whatever it points at). The main worktree is the
parent of `<common>` for a non-bare repository; for a bare repository the main
registration is `<common>` itself with an unknown `HEAD`, exactly as `-z` lists
it (`bare`, no branch). Each `gitdir` and `HEAD` file is read only when it is a
regular file (checked without following symlinks, before it is opened) and with
a 4 KiB bound, as git only ever writes one short line there: a FIFO, device,
directory or symlink is never opened (a FIFO would block the read until some
writer appeared), so such a `gitdir`, or an oversized one, skips that
registration, and such a `HEAD`, or an oversized one, is unknown. Which form
works is remembered for the rest of the process, so the failed probe happens
once. The NUL-delimited form is parsed as
records, so a path containing a newline (or bytes that are not UTF-8 on Unix)
stays one path; the metadata fallback keeps such paths byte-exact too. Only an
observation of a path that is not valid UTF-8 is then skipped, because the
protocol carries paths as strings and must never offer a lossy one. Every other
path git prints (`rev-parse --show-toplevel`, `--git-dir`, `--git-common-dir`)
is read as bytes with only git's single trailing newline stripped, never other
whitespace, so sibling checkouts named `checkout` and `checkout ` stay distinct;
output that is not valid UTF-8 is skipped, never decoded lossily.
h4rn's own
`<data_dir>/subagents/*/worktree` paths and the main worktree are never recorded
as `harness`. Write failures are logged and never interrupt the watcher.

Every git command the tracker runs, for observation, listing or clearing, has
`GIT_TRACE`, `GIT_TRACE2`, `GIT_TRACE2_EVENT`, `GIT_TRACE2_PERF`,
`GIT_TRACE_PACKET`, `GIT_TRACE_PERFORMANCE`, `GIT_TRACE_SETUP`,
`GIT_TRACE_REFS`, `GIT_TRACE_FSMONITOR`, `GIT_TRACE_SHALLOW` and
`GIT_CURL_VERBOSE` removed from its environment. Several lookups treat any
stderr output from a successful command as inconclusive (a broken-ref warning),
so trace output inherited from the daemon's environment must never reach them.
Spawning git is retried a few times, with a short sleep, when it fails with
`ETXTBSY` (the executable is briefly held open for writing by a process that
forked while it was being written, as with a git upgraded in place).

A worktree-isolated child records the parent repository's main worktree at
spawn, not the workspace it happened to start in. That matters for nested
children: a grandchild's parent workspace is the child's own worktree, which is
deleted when the child settles, so the branch would otherwise become unfindable.
If only an old record remains (one written before this), listing follows the
recorded repositories through nested subagent worktrees, several hops deep.

**Requests.** `list_worktrees` (scope `view`) returns the list, active first,
then orphaned, then settled, newest first within each. `clear_worktrees` (scope
`admin`, because it deletes from disk) takes optional `paths` (default: every
non-active entry except thread workspaces), `force` and `bulk`, and returns the
removed paths, the skipped ones with reasons, and the fresh list. Settings
always sends the explicit paths of the rows the user saw, for Clear all as well
as for one row, so a clear can only ever touch what was on screen. Clear all
(and the forced retry of the paths Clear all skipped) sets `bulk`, which applies
the Clear all exclusions to those paths: a row that became a thread's workspace
while the dialog was open is left out, exactly as with `paths` omitted. A
per-row Remove sends `bulk: false` and may remove a thread workspace it names. Removals run one at a
time and are awaited: the response never reports completion while a delete is
still running, and the fresh list always reflects what actually happened.

A clear lists twice: once to pick its targets, and once at the end for the
response. The selection listing skips `git status` for every entry because each
removal checks dirtiness itself in preflight (with git's own refusal as the
backstop); the response listing asks for it, each entry's `git status` bounded
to ten seconds like the watcher's calls (a timeout reports `dirty` as unknown).
Requested paths are matched to listed rows exactly: the strings must be
byte-equal once trailing separators are trimmed. Nothing is normalised, so a
request spelled through a symlink or with `..` never reaches a row, and
selection does no filesystem work at all.

**Clearing rules.**

- Active worktrees are never removed, even with `force`. Each target is handled
  in a fixed order: a blocking preflight (repository and main-worktree checks, a
  nested worktree, dirtiness, whether the checkout is still on its listed
  branch, detached HEAD, unmerged owned branch); then a blocking prepare that
  takes its own registration snapshot and re-runs the checks that can go stale
  (the listed branch and detached reachability, containment against that
  snapshot, the nested-checkout scan, files hidden from `git status`, and the
  registration's `HEAD` when the directory is already gone); then the async live activity check; and
  finally, immediately before the destructive `git worktree remove`, one more
  verification against a registration snapshot taken right there (then the
  directory is checked, so both readings postdate the activity check): for an
  existing checkout prepare also saw, containment against that snapshot, the
  listed branch, detached reachability and files hidden from `git status`; for
  a missing checkout, whether it was already missing at prepare or just
  vanished, its registration's `HEAD` from that snapshot (unknown, or detached
  and reachable from no ref, is skipped without force), so a temporary branch
  that reached the detached commit during prepare and was deleted since does
  not let the registration go; if the directory appeared or vanished since
  prepare, every check for the state that exists now (the existing-checkout
  checks including the nested scan, or the missing registration's `HEAD`). An
  owned entry with no branch left whose selected row said `exists: false`, and
  whose directory exists at prepare or at this final verification, is refused
  outright as `the checkout was recreated during the clear` (not forceable):
  the row the user chose had nothing on disk to protect, so the directory there
  now is a new checkout that is not this entry's. A checkout switched to
  another branch, an assume-unchanged file edited, or a nested worktree
  registered, in that window is therefore caught and the checkouts are kept.
  A missing checkout with no registration in that snapshot
  is reported removed only because both readings show nothing remains (no
  directory, no registration); anything that is there is handled as above.
  Two things are deliberately not changed. The live activity check stays
  between prepare and this final verification (preflight → prepare → live
  activity check → re-verify and destroy), as settled in earlier rounds: moving
  it after the verification would put an async wait in front of the destructive
  call. And the nested-checkout scan is not re-run here for a checkout whose
  state did not flip: a whole tree walk would widen the window between the
  activity check and the removal; it runs in prepare and on a state flip only. The live re-check collects every candidate —
  the accessors the listing named, the accesses recorded for the path in the
  store, and every current thread (children and snoozed included) whose
  workspace is the entry or inside it — and then checks activity once, at the
  end, over the whole set. A thread found by one source while it was idle is
  therefore not dropped before another source would have seen it busy, and a
  busy thread keeps the entry and is named in the reason.
- A `thread` workspace is only removed when its path is named. It belongs to
  whoever created it, and its thread may be resumed there, so clearing
  everything leaves it alone. That holds by path, not only by origin: when
  `paths` is omitted, or for the given paths when `bulk` is set, any row (a
  `harness` observation, say) whose path is a thread's workspace in the fresh
  listing the clear takes, or the top level of that workspace's checkout, is
  left out too; it is dropped from the request rather than reported. A subagent child's workspace is its subagent's own worktree and
  does not count.
- The destructive key is the path exactly as listed. Git resolves every path it
  is given through symlinks, so a selected path that is a symlink (or a file),
  or that only resolves to itself through a symlinked parent, is never handed to
  git and is skipped as `not a registered worktree` (not forceable), whether
  or not a registration still names it. An existing checkout must also have a
  registration at exactly its path in the registration snapshot, in preflight,
  prepare and the final verification before removal; otherwise it is skipped
  with the same reason. Every row is listed under the path that was recorded
  for it, never re-canonicalised through its own final component: a stored
  observation under its stored path, and an owned subagent row under the
  subagent's recorded `workspace_path` with only its parent normalised (so a
  data directory reached through a symlink stays clearable, matching how git
  records registration paths). So a tracked worktree (owned or not) that was
  moved away and whose old path now is a symlink to another worktree of the
  same repository is listed with `exists: false` and is never cleared through
  that symlink, by an ordinary, forced or bulk clear: the other worktree is
  untouched.
- Without `force`, a worktree with uncommitted changes, or an owned branch not
  merged into the repository's `HEAD`, is skipped. Dirtiness is asked of git
  again in preflight rather than trusted from the listing, and untracked files
  always count, even when `status.showUntrackedFiles` says otherwise; a failed
  check on an existing directory is a skip, not a removal. Anything written
  after preflight is caught by git's own refusal inside `git worktree remove`,
  which every invocation runs with `-c status.showUntrackedFiles=all
  -c diff.ignoreSubmodules=none`, so that backstop counts untracked files and
  submodules whatever the user's config says.
- Every git call that decides whether work would be lost (the dirty check, the
  hidden-files `git ls-files -v`, and `git worktree remove` with the `git
  status` it runs) also runs with `-c core.fsmonitor=false
  -c core.untrackedCache=false`. A filesystem monitor hook that reports nothing
  changed, or a stale untracked cache, would otherwise let git trust its index
  and miss an edit or a new file.
- Without `force`, an existing checkout must still be on the branch the listing
  gave it. If the current branch differs, the entry is skipped as
  `checkout is no longer on <branch>`; force removes the replacement checkout
  anyway (and branch deletion still applies only to an owned `h4rn/` branch).
- A detached HEAD is skipped as `detached HEAD has commits on no branch` unless
  its commit is reachable from some branch, remote-tracking branch or tag;
  otherwise removing the checkout would leave that work unreachable. When the
  checkout's directory is already gone, the same check uses the detached
  registration's `HEAD` commit from the registration snapshot (`-z` output, or
  git's metadata on an older git).
- The registration's `HEAD` is one of attached (a branch), detached (a sha) or
  unknown. It is unknown when the metadata fallback cannot read or parse the
  `HEAD` file, or when a `-z` record has neither `branch` nor `detached` (or a
  `detached` record lacks its sha). Without `force`, a missing checkout whose
  registration `HEAD` is unknown is skipped as
  `could not read the registration's HEAD` (forceable): unknown is never read
  as "nothing to protect".
- The prepare step before the live activity check re-runs the detached-HEAD
  reachability check, the "still on its listed branch" check and containment,
  against its own fresh snapshot (the removal itself re-checks containment
  against the snapshot it takes immediately before `git worktree remove`). A checkout detached, replaced
  or given a new commit after preflight is therefore skipped with the same
  reason it would have been skipped at preflight, and its registration and
  commit stay. A nested worktree created after preflight is likewise still
  detected, and both checkouts are kept.
- A worktree git reports as locked is skipped as `is locked; unlock it first`
  (not forceable); the checkout and its registration stay until it is unlocked.
  The same wording applies when the checkout's directory is already gone but
  its registration is locked.
- An existing checkout that contains another registered worktree is never
  removed, with or without force: it is skipped as
  `contains another worktree at <path>` (not forceable). Git would otherwise
  remove the parent and leave the nested registration pointing at a directory
  that no longer exists. The check applies only while the selected directory
  exists: a registration whose checkout is already gone has no nested working
  tree to protect, and removing that registration must leave the nested
  registration alone.
- Before an existing checkout is removed (force or not), its tree is scanned,
  without following symlinks, for a nested `.git` file or directory other than
  the checkout's own top-level one, so an ignored clone of any repository (not
  only a registered worktree) is found. One is skipped as
  `contains another git checkout at <path>` (never forceable). Only a real
  submodule checkout of this repository is exempt, and only when both hold: its
  directory, relative to its superproject, is a gitlink (mode `160000`) in that
  superproject's index (`git ls-files -s -z`, run once per superproject the first
  time a nested `.git` file is met under it, bounded by the time left; the
  superproject is the checkout itself, or the nearest submodule already found
  exempt, so a submodule of a submodule is judged by its own parent), and it is a `.git`
  *file* whose `gitdir:` target (resolved against the file's directory when
  relative) is under `<common>/modules/` or `<common>/worktrees/<id>/modules/`
  with no `worktrees` path component after `modules` (such a component would
  make it a linked worktree *of* the submodule, a separate checkout). Git's own
  non-forced `git worktree remove` then refuses a checkout holding submodules,
  reported as `contains submodules` (forceable); with `force` git removes it.
  Any other nested `.git` is the never-forceable reason above: a linked
  worktree of a submodule placed inside the checkout, a gitfile at a path that
  is not a gitlink, a gitfile pointing elsewhere, one that cannot be read, is
  not a regular file, or is larger than 4 KiB (only a regular `.git` file is
  ever opened, and only that much of it is read), or any
  nested checkout when the gitlink lookup itself fails. The scan is bounded by the
  30-second per-command deadline, checked for every directory entry and once
  more before answering "nothing found"; running out of time, or a directory
  that cannot be read, is skipped as `could not check for nested checkouts`
  (forceable), never read as clean.
- Without `force`, prepare, and the final verification immediately before
  removal, also run `git ls-files -v` in an existing checkout, bounded like
  every other call. An
  entry git tags as assume-unchanged (a lowercase tag), whether or not its file
  is on disk, or skip-worktree (`S`) whose file exists on disk is skipped as
  `has files hidden from git status` (forceable): an edit to such a file, and
  for assume-unchanged even its deletion, never shows in `git status`, so
  neither the dirty check nor git's removal backstop would see it. Only
  skip-worktree files absent on disk (a sparse checkout) do not count.
- Removal is plain `git worktree remove`, with `--force` only when the request
  asked for it, never merely because h4rn owns the checkout. Git's own contract
  applies: a removable checkout takes ignored files (build output, caches) with
  it, which the confirmation dialog says. There is no repository-wide
  `git worktree prune`: `git worktree remove` drops the selected worktree's own
  registration, and a repository-wide prune would also forget an unrelated
  worktree whose directory is only temporarily away. When the selected
  checkout's directory is already gone, only its own registration is looked up
  by path in the registration snapshot and removed: an ordinary clear runs the
  plain `git worktree remove <path>`, and only `force` may try `--force`, then
  the plain form. If it fails the path is skipped as
  `registration left in place: <git's message>` (not forceable) rather than
  reported as removed, and the owned branch is only touched when the
  registration was removed.
- Every git command in a clear is bounded to 30 seconds; a command that outlives
  its deadline is killed. Collecting a command's output is bounded too: if a
  descendant process left behind by a git alias keeps stdout or stderr open,
  the readers are given the rest of the deadline plus one second and then
  detached, and the call reports `git timed out after Ns` instead of hanging.
  A timeout while checking or removing the checkout skips the entry as
  `git timed out after 30s`. After the checkout is gone, a timeout deleting the
  branch is reported as the branch failure.
- For owned entries the branch is deleted, but only when it starts with `h4rn/`,
  and the empty `<data_dir>/subagents/<id>` directory is removed. A branch h4rn
  did not create is never deleted. Preflight resolves the branch to a sha first
  (after checking it is not a symbolic ref) and refuses an unmerged tip, so the
  early skip reads `branch <b> has unmerged commits` (forceable). After the
  checkout is gone, an ordinary clear re-checks everything immediately before
  deleting: it takes a fresh registration snapshot (so a worktree that took the
  branch over after the checkout went is seen), re-runs the rebase/bisect
  checks (`rebase-merge/head-name`, `rebase-merge/update-refs`,
  `rebase-apply/head-name` and `BISECT_START` in every worktree's git dir; each
  is read only when it is a regular file of at most 4 KiB, and anything else
  there — a FIFO, a symlink, an oversized or unreadable file — counts as a
  rebase or bisect in progress, so the branch is left), and re-runs `git merge-base --is-ancestor <verified-sha> HEAD`; a tip
  that is no longer an ancestor is the same forceable
  `branch <b> has unmerged commits`. It then deletes exactly that verified tip
  with `git update-ref --no-deref -d refs/heads/<b> <verified-sha>`. If
  `update-ref` fails, the ref is re-read: missing means the branch is already
  gone and the path is reported as removed; a new tip already merged into
  `HEAD` is the forceable `branch <b> changed during the clear`; a new tip that
  is not merged is the forceable unmerged reason; anything else is reported as
  `checkout removed, but branch <b> could not be deleted: <git's message>`.
  Whether the branch exists is asked with
  `git for-each-ref --format='%(objectname) %(refname)' refs/heads/<b>`, where
  no exact `refs/heads/<b>` line means absent and a git failure is an error.
  `for-each-ref` skips a broken ref with only a warning, so no matching line
  plus anything on stderr is inconclusive and also an error, never "absent";
  in preflight that is `branch <b> could not be checked: <error>` (not
  forceable). If that lookup fails after the checkout was removed, the path is
  reported as `checkout removed, but branch <b> could not be checked: <error>`
  (not forceable), never as a silent removal. If
  the branch did not exist at preflight but exists now, it is left alone and
  reported as `checkout removed, but branch <b> was left in place` (not
  forceable). Before deleting, git's protections are applied explicitly: the
  branch is not deleted when any registered worktree has it checked out, when
  any worktree's git directory (the main `<common>` or `<common>/worktrees/*`)
  has `rebase-merge/head-name`, `rebase-apply/head-name` or
  `rebase-merge/update-refs` naming `refs/heads/<b>`, or when `BISECT_START`
  names the branch. Those files are matched by whole whitespace-trimmed line
  (and by whole whitespace-separated token for `update-refs`), never as
  substrings, so a rebase on `h4rn/sa-abc2` does not protect `h4rn/sa-abc`.
  Those refusals are reported as
  `checkout removed, but branch <b> could not be deleted: <why>`. A symbolic
  owned ref is never deleted: the checkout still goes, and the path is skipped
  as `branch <b> is a symbolic ref` with the symref and its target intact. A
  forced clear uses git's own `git branch -D <b>` (git still refuses a branch
  checked out in any worktree). Any other refusal after the checkout is gone is
  reported as
  `checkout removed, but branch <b> could not be deleted: <git's message>` so
  the leftover branch is not mistaken for a clean removal.
- The repository's main worktree is never touched.
- If git refuses, the entry is skipped. When `git worktree remove` reports
  `contains modified or untracked files`, that is reported as the standard
  `has uncommitted changes` reason, so a forced retry is offered exactly as for
  our own preflight dirty check.

An orphaned directory left by a restart shows up as `orphaned` and can be
cleared; `restore` itself still folds a running child into `Failed` and does not
touch the directory.

## Tests

`crates/h4rn-daemon/tests/e2e_subagents.rs` drives the real `/mcp` endpoint with
a token minted for the parent thread and covers spawn, status, result, cancel,
approval mirroring/cleanup, worktree create/remove and wall-time enforcement
using `FakeHarness`. The ignored
`real_claude_delegates_to_codex` test runs a real Claude Code parent delegating
"Reply with exactly: pong" to a real Codex child.

`crates/h4rn-daemon/tests/e2e_worktrees.rs` lists and clears a settled subagent
worktree over the wire, and the unit tests in `crates/h4rn-daemon/src/worktrees/`
use real temporary git repositories.

```text
cargo test -p h4rn-daemon --test e2e_subagents
cargo test -p h4rn-daemon --test e2e_worktrees
cargo test -p h4rn-daemon --test e2e_subagents -- --ignored --nocapture
```
