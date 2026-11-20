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
failed, or the daemon restarted mid-run). An entry with nothing left to clear
(directory gone and, for owned entries, branch gone) is not listed.

Paths are compared after normalisation: the path is canonicalised exactly as
written before any `..` is folded, so a workspace spelled through a symlink
still resolves to the checkout the filesystem says it is. Only a missing path
canonicalises its longest existing ancestor and folds the unresolved suffix
lexically.

**What is stored.** Only observations that cannot be derived live go to
`<data_dir>/worktrees.db`: `harness` worktrees and which threads used them.
Subagent worktrees come from the supervisor and thread workspaces from the
thread list, and the list is recomputed on every request.

**How harness worktrees are observed.** The daemon snapshots `git worktree list`
for a thread's repository when a turn starts and again when it completes; any
linked worktree that appeared in between is recorded against that thread (and its
subagent id, for a child). Two turns on the same repository can overlap, though
(for example a parent and its child), and then a worktree that appeared belongs
to neither turn alone: both snapshots are marked shared and the newcomer is not
attributed. Tool-call inputs are also scanned for absolute paths (`cwd`,
`file_path`, shell commands and so on); a path inside a linked worktree of the
thread's repository records an access, which is evidence a turn diff cannot
fake. The per-repository worktree list is cached and refreshed at most every two
seconds, so tool calls do not shell out to git one by one. h4rn's own
`<data_dir>/subagents/*/worktree` paths and the main worktree are never recorded
as `harness`. Write failures are logged and never interrupt the watcher.

A worktree-isolated child records the parent repository's main worktree at
spawn, not the workspace it happened to start in. That matters for nested
children: a grandchild's parent workspace is the child's own worktree, which is
deleted when the child settles, so the branch would otherwise become unfindable.
If only an old record remains (one written before this), listing follows the
recorded repositories through nested subagent worktrees, several hops deep.

**Requests.** `list_worktrees` (scope `view`) returns the list, active first,
then orphaned, then settled, newest first within each. `clear_worktrees` (scope
`admin`, because it deletes from disk) takes optional `paths` (default: every
non-active entry except thread workspaces) and `force`, and returns the removed
paths, the skipped ones with reasons, and the fresh list. Settings always sends
the explicit paths of the rows the user saw, for Clear all as well as for one
row, so a clear can only ever touch what was on screen. Removals run one at a
time and are awaited: the response never reports completion while a delete is
still running, and the fresh list always reflects what actually happened.

A clear lists twice: once to pick its targets, and once at the end for the
response. The selection listing skips `git status` for every entry because each
removal checks dirtiness itself at the last moment; the response listing asks
for it.

**Clearing rules.**

- Active worktrees are never removed, even with `force`. Each target is handled
  in two blocking phases: a preflight (repository and main-worktree checks,
  dirtiness, whether the checkout is still on its listed branch, detached HEAD,
  unmerged owned branch) and the removal itself. The live re-check runs after
  preflight and immediately before removal, so a thread that becomes busy in
  that window still keeps the entry and its checkout stays on disk. That
  re-check first collects every candidate — the accessors the listing named, the
  accesses recorded for the path in the store, and every current thread
  (children and snoozed included) whose workspace is the entry or inside it —
  and then checks activity once, at the end, over the whole set. A thread found
  by one source while it was idle is therefore not dropped before another source
  would have seen it busy, and a busy thread keeps the entry and is named in the
  reason.
- A `thread` workspace is only removed when its path is named. It belongs to
  whoever created it, and its thread may be resumed there, so clearing
  everything leaves it alone.
- Without `force`, a worktree with uncommitted changes, or an owned branch not
  merged into the repository's `HEAD`, is skipped. Dirtiness is asked of git
  again at the last moment rather than trusted from the listing, and untracked
  files always count, even when `status.showUntrackedFiles` says otherwise; a
  failed check on an existing directory is a skip, not a removal.
- Without `force`, an existing checkout must still be on the branch the listing
  gave it. If the current branch differs, the entry is skipped as
  `checkout is no longer on <branch>` (not forceable): a replacement checkout
  recreated at the same path is not the entry that was listed.
- A detached HEAD is skipped as `detached HEAD has commits on no branch` unless
  its commit is reachable from some branch, remote-tracking branch or tag;
  otherwise removing the checkout would leave that work unreachable.
- Removal is plain `git worktree remove`, with `--force` only when the request
  asked for it, never merely because h4rn owns the checkout. Git's own contract
  applies: a removable checkout takes ignored files (build output, caches) with
  it, which the confirmation dialog says. There is no repository-wide
  `git worktree prune`: `git worktree remove` drops the selected worktree's own
  registration, and a repository-wide prune would also forget an unrelated
  worktree whose directory is only temporarily away. When the selected
  checkout's directory is already gone, only its own registration is looked up
  by path in `git worktree list --porcelain` and removed (with `--force`, or
  plainly, whichever this git accepts); if neither works the registration is
  left in place rather than deleting another worktree's metadata.
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
  did not create is never deleted. Without `force`, the preflight records the
  branch's commit when it verifies the merge, and removal re-reads it right
  before deleting: if it moved, the branch is kept and the path is skipped as
  `branch <b> moved during the clear` (forceable), because the tip that was
  verified is no longer the tip that would go. Deletion itself is
  `git update-ref -d refs/heads/<b> <verified commit>`, an atomic
  compare-and-delete. Git's checked-out-branch protection still applies first:
  if `git worktree list --porcelain` shows the branch checked out anywhere, the
  path is reported as
  `checkout removed, but branch <b> could not be deleted: it is checked out in <path>`.
  With `force`, plain `git branch -D` is used. If the checkout was removed but
  the branch could not be deleted for any reason, the path is reported as
  skipped with
  `checkout removed, but branch <b> could not be deleted: <git's message>` so
  the leftover branch is not mistaken for a clean removal.
- The repository's main worktree is never touched.
- If git refuses, the entry is skipped. When `git worktree remove` reports
  `contains modified or untracked files`, that is reported as the standard
  `has uncommitted changes` reason, so a forced retry is offered exactly as for
  our own last-moment dirty check.

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
