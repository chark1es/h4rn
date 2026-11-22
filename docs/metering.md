# Metering and auto-resume

Phase 5 adds a durable usage ledger, provider-limit tracking and automatic
resume after a rate limit (ADR-024). The ledger is local first: every turn is
metered from the harness event stream, stored in SQLite next to the daemon's
other data, and only then synced to h4rn-connect. Proxy reports are enrichment,
never the source of truth.

## What is measured

| Metric | Definition |
| --- | --- |
| Input tokens | Prompt tokens reported by the harness for the turn, including cached input. |
| Output tokens | Completion tokens for the turn. |
| Cached input (read) | Prompt tokens served from the provider's cache. |
| Cached creation | Prompt tokens written to the provider's cache. |
| TTFT | Turn start to the first `TextDelta`, in milliseconds. |
| Generation time | First token to turn end, minus the union of tool-call intervals. |
| TPS | Output tokens divided by generation time. Tool time never counts. |
| Wall time | Turn start to the turn's `TurnCompleted`. |
| Active | Sum of turn wall time for the queried threads. |
| Idle | Runtime minus active: the gap between the first turn start and the last turn completion. |
| Runtime | First turn start to last turn completion across the queried threads. |
| Cost | See [Cost provenance](#cost-provenance). |
| Turns | Counted ledger rows, after dedupe. |

TTFT and TPS are zero until the first token arrives. A turn that never emits a
token still contributes tokens, cost and wall time but has no TTFT/TPS mean. A
turn that completes while a tool is still running counts the tool time up to
that moment.

The same arithmetic applies before a row exists: `UsageTick` frames are
throttled to about one per second per running turn and carry the live counters,
TTFT, TPS, elapsed and tool time. Ticks are ephemeral and carry no sequence
number; the durable row is written when the turn completes.

## Where the numbers come from

1. **Harness stream (always).** `Usage` events within a turn are merged;
   `TurnCompleted.usage` replaces the accumulated counters when it carries
   accounting. The adapter's own turn report fills gaps when the completion
   event has no usage. This is the self-metered row and it always counts.
2. **CLIProxyAPI performance fork (optional).** When the profile's capabilities
   include `/v8/management/performance`, the daemon polls the last hour every
   60 seconds and stores one `proxy_reported` row per call with a stable
   `proxy_<profile>_<request-id>` row id. Retries of the same call update
   nothing.
3. **Harness cost.** If the turn reports `cost_micros`, that value wins and the
   row's provenance is `harness_reported`.

### Dedupe rule

Self-metered rows always count. A proxy-reported row counts only when it is
**not** already covered by a self-metered row, matched in this order:

1. the same `(thread_id, run_id)`, or
2. the same `correlation_id` (the proxy request id when the fork echoes one),
   or
3. the same `provider` and `model` within ±120 seconds of the self-metered
   turn completion.

Matched proxy rows are stored with `deduped = true` for reconciliation and are
excluded from every report and from the synced total. This is what prevents a
turn from being counted twice when the harness and the proxy both observe it.

## Cost provenance

Each row records a `cost_source`:

- `harness_reported` — the adapter supplied a cost. It wins over any estimate.
- `price_table` — estimated from the built-in price table; `price_table`
  records the table version used.
- `proxy_reported` — supplied by the proxy.
- `unknown` — no price is known. The cost is zero, not free; the UI shows the
  provenance so an unpriced model is visible.

Prices are per million tokens and split plain input, output, cached read and
cached creation. Unknown models are deliberately unpriced rather than silently
assigned a default.

## Limits and resets

A harness limit becomes a `LimitHit` with the harness, provider/account, model,
`reset_at` (when the provider says), any consumption numbers and the provider's
message. The daemon records it, keeps the thread in Working and sets
`AgentState::Scheduled` when auto-resume is armed. The adapter is the only
judge of whether a failure is a limit: a write that fails before its prompt
reaches the agent carries a typed limit signal when the adapter parsed one, and
the daemon never infers a limit from another harness's error text (an HTTP 413
"payload size limit exceeded", for example, is an ordinary failed turn). A
limit is only recorded while the failed write still owns the turn and its
session, so a shutdown that interrupts the turn while the write is pending
leaves no schedule behind. `GetLimits` returns the history plus the current
policy, and `ListResets` returns schedules and history.

An agent may also answer `end_turn` and only then exit, with the provider's
reset on the exit report. The completion sees a closed transport, waits
briefly for that report, and applies its limit before the turn settles, so the
schedule is armed and the message queued behind the turn stays paused. The
reset a turn already applied is kept on the turn, not only on the live
schedule, so an exit report that repeats it cannot arm a new schedule after the
person cancelled the resume during the wait; a newer reset still arms one. The
same applies when the death is reported only by the exit, with no EOF first: the
live session carries the reset it applied into the dying entry. When several
deaths are waiting for their exit reports, each schedule remembers the
generation that armed it, and an older generation's later, earlier reset keeps
its own row but cannot replace the newer generation's pause. A reset applied by
one generation is never recorded on another, and a requeued write's pause names
the generation that claimed the write. That per-generation rule also covers the
turn's own marker: only the generation that owns the active turn records a reset
on it, so an older generation's delayed exit report cannot make a newer turn's
own completion arm the pause the person already cancelled. When a newer
generation confirms the same reset, the pause keeps its attempt and jitter and
transfers to that generation, so an older generation's delayed report cannot
replace it later.

A death between turns publishes one diagnostic row; it does not raise a failure,
so a thread whose auto-resume just armed stays Scheduled (and a thread with no
limit is not turned into an Error asking for attention by the agent's exit
alone). Each death between turns is tracked by its session generation, so a
message that replaced the dead session and then died itself before the first
exit report arrived still leaves both deaths with their own row and reset. When
several reports describe one death, the exit/status report wins
over the generic stdout-close notice even when its text is shorter; only
reports of the same kind are compared by detail.

### Scheduling

- **Known reset inside `max_wait`**: resume exactly at the reset plus
  deterministic jitter.
- **Known reset beyond `max_wait`**: retry at the cap and say why, instead of
  abandoning the thread.
- **Unknown reset**: capped exponential backoff
  `min(30 s × 2^attempt, 15 min, max_wait)`; the reason says the provider did
  not report a reset. h4rn never invents a provider reset it did not observe.

Schedules are persisted in SQLite and replayed after a daemon restart. While a
schedule is armed the thread stays in Working, queued input is held, and the
schedule can be cancelled (`CancelResume`) or fired immediately (`ResumeNow`).
Cancelling the pause, or sending a message while a requeued turn is parked
behind it, releases that turn instead of leaving it pending with nothing to
deliver it; a Send's own message queues behind the parked turn. A release never
waits on a storage write: if the ledger or the journal cannot record it, the
error is logged and the prompt is still delivered, because the schedule it was
waiting for is already gone. A released thread is projected as running, not as
rate limited: it leaves the paused phase, drops the limit that explained it and
shows no resume time, without counting a second turn. A pause records the one
dispatch it parks, so a pause armed by an old agent's exit report does not hold a
replacement prompt that a Send has just started. The parked prompt is stored on
the thread row, so a restart restores it and the schedule delivers the person's
message rather than the policy's prompt; a graceful shutdown leaves that parked
turn and its saved prompt untouched for the same reason, and keeps it while the
startup resync is unresolved too — a ledger read that failed does not turn the
prompt into a settled turn that the next boot would replace with the policy's
prompt. A schedule that did not survive the restart releases the prompt
instead, recording the release and dropping the limit exactly as a cancellation
does. The restart restores the
schedule ids from the ledger independently of the limit history: a failed limit
read only leaves the reset time unknown for now, and the ticker retries the
resync until both reads succeed, so a restored pause never stays unnamed. The
retry waits start at a second and double up to a minute, so a ledger that stays
unreadable does not rewrite every thread's summary on every tick. A retry never
overwrites what changed after it read the ledger: a newer limit that armed
another schedule in that window keeps its own reset and parked prompt, and only
a record whose pause is still unresolved takes the restored identity.

When a schedule fires, the daemon starts a new turn with the policy prompt and
marks the schedule `Fired`. The fire first checks that the thread's record names
the schedule it is about to claim, by its ledger id, so a restarted daemon that
has not yet read the ledger cannot consume a due attempt it does not know
about; the daemon attaches metering and resyncs the record before the ticker
starts for the same reason. If a failed write had put its prompt back for a
retry when the limit armed, that requeued turn is what the schedule delivers,
instead of the policy prompt, so the person's message is not replaced. A newer
limit that replaces the parked pause keeps that prompt named, so a pump-end
advance cannot deliver it into the paused provider. The fire re-checks under
the record lock that the pause on the thread is the schedule it claimed, by its
ledger id: a missing pause, a different pause, or a newer limit that replaced it
while the attempt was being claimed leaves the newer pause alone and starts
nothing (and does not change the model for a failover). A fire only releases the
pending prompt its own pause parks: a turn a Send started is left alone and no
attempt is spent on it. When a claimed fire then finds the record no longer
names its schedule, it records the attempt as `Cancelled` with
`superseded before delivery` instead of leaving the row looking `Fired`; a
newer row that replaced the attempt is never touched. A retried write only
waits for a pause that names its own dispatch, the same predicate the queue
advance uses, so a pause armed by another session generation does not hold a
prompt it does not own. An unknown reset on that requeued-write path waits the
backoff above rather than retrying immediately. A release whose journal append
fails is still projected on the next tick instead of being hidden by a cached
summary.

With `failover = true` and an active proxy profile, it asks the proxy for another
model/account that is not cooling down, records
that choice on the schedule (and emits it in `ScheduleChanged`), and uses the
model for the resumed session. A failed resume moves to `Waiting` with
`last_error` instead of silently losing the thread.

### Failing over now

Instead of waiting for a reset, a limit-hit thread can resume immediately on
another model or account. `ListFailoverOptions { thread_id }` returns the
daemon's default plus every eligible alternative (`h4rn_proxy::eligible_failovers`,
best first: the head is what `pick_failover` returns); `FailOver { thread_id,
model, account }` fires the schedule now. With no `model` the default is used; a
named model must be one of the options. The choice is recorded on the schedule
and the session restarts on the new model. Without a proxy profile for the
thread's harness there are no options and the reply carries `unavailable` with
the reason; the card's "Fail over…" button is then disabled with it as a tooltip.

## Requests and events

Client requests: `GetUsage`, `GetLimits`, `ListResets`, `CancelResume`,
`ResumeNow`, `ListFailoverOptions`, `FailOver`, `SetAutoResumePolicy`.

Tool rows carry `ToolCall.stats`: `diff {added, removed}` for edits and
`matches {count, unit}` for searches, derived in `h4rn_harness::stats` from each
adapter's own result (Claude `structuredPatch`/`Grep` results, Codex `fileChange`
diffs, OpenCode `metadata`, ACP diff content). Tools that report neither get none.

Ephemeral server frames (`ServerFrame::UsageEvent`): `UsageTick`,
`LimitStatus`, `ScheduleChanged`. Durable lifecycle changes
(`usage_limit_hit`, `thread_upserted`) stay in the sequenced event log.

`GetUsage` supports thread, workspace, machine and global scopes, grouping by
thread/workspace/machine/harness/model, and optional hour or day buckets.
Ledger rows carry `parent_thread_id` for subagent child threads; a thread-scoped
query with `include_children = true` rolls child turns into the parent's totals,
while the default counts only the parent's own turns.

## Syncing to connect

The daemon pushes unsynced rows in batches of up to 200 every 30 seconds when
signed in. `row_id` is the idempotency key: the connect schema enforces one row
per `(user_id, row_id)` and a retried batch reports duplicates instead of
storing twice. Rows are marked synced only after a successful response.

`POST /v1/usage` ingests a batch; `GET /v1/usage/aggregate` returns the same
report shape as `GetUsage` and excludes `deduped` rows. The connect migration
adds the Phase 5 columns and the unique row index to the existing
`usage_events` table, plus indexes for per-user series and per-workspace
queries. Hour and day buckets come from Postgres `date_trunc`, so global
aggregates use the same definitions as the local ledger.

## Files and configuration

- `h4rn-meter` — pure turn math, price table, scheduler, ledger, hub.
- `h4rn-protocol/src/usage.rs` — wire types for reports, limits, schedules and
  sync.
- Daemon `meter.db` (under the data directory) — ledger, limits and schedules;
  separate from `events.db`.
- `Config::meter_db_path()` — override point for the ledger location.
