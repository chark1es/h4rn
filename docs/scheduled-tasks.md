# Scheduled tasks and webhooks

A scheduled task runs a prompt on the daemon without a client attached. It can
run on a calendar schedule, or whenever an HTTP caller posts to its own secret
URL. The daemon owns the timer, the journal and the webhook path, so closing the
app does not stop a task and a restart does not forget it.

Open **Settings → Scheduled tasks** to create one. The list shows the schedule,
the next real run, the last outcome and whether the task is enabled.

## Schedules

A task runs in one of these ways:

| Schedule | Meaning |
|---|---|
| **Once** | One local date and time, then never again. |
| **Every…** | An interval of minutes or hours, counted from when the task was created. |
| **Daily** | A local wall-clock time every day. |
| **Weekly** | One or more weekdays at a local wall-clock time. |
| **Cron** | A five-field expression: minute, hour, day-of-month, month, day-of-week. |
| **On webhook** | Only when its URL receives a POST. |

Calendar schedules use the task's time zone, which defaults to the daemon's own
(`TZ`, or the system's IANA zone). The editor offers a searchable list of IANA
names. Intervals are absolute durations, so "every 6 hours" stays six hours
across a daylight-saving change; a daily or weekly time follows the wall clock,
so 09:30 stays 09:30. A time the spring-forward gap skips runs at the first
valid minute after the gap. A time in the repeated fall-back hour runs at the
first occurrence, or the second if the first has already passed.

Cron accepts `*`, numbers, ranges (`1-5`), steps (`*/15`, `1-30/2`) and comma
lists. Weekdays are 0 (Sunday) through 6, with 7 also accepted as Sunday. When
both day-of-month and day-of-week are restricted, either may match, as in
standard cron. An expression that can never match a real date is refused when
saved.

## Destinations

Each run starts a thread or posts into one:

- **New thread each run** keeps every run separate. The task remembers the
  thread it started last, for the overlap check, but it is not the task's
  permanent thread.
- **One thread, continuing** starts a thread on the first run and posts later
  runs into the same conversation.
- **One thread, fresh session** keeps the same thread readable but clears the
  harness resume id before each run, so the agent does not carry the previous
  conversation's context.

Changing a one-thread task's workspace starts a new thread there on its next
run. Switching to **New thread each run** drops the task's link to its old
thread; the thread itself is not deleted.

A thread with an enabled one-thread task keeps its worktree through a clear.
Settings → Worktrees reports it as skipped with **has an enabled scheduled
task**; pausing or deleting the task releases it.

## Missed runs and overlap

If the daemon was down at a due time, the task runs once when it starts, not
once for every missed slot. Its next run is then computed from the current
time, so a long outage does not produce a burst.

A task whose previous run is still working skips its slot and records the skip
in the history. A manual **Run now** on a busy task is refused with a reason
instead of queuing a second prompt. Enabling a paused task computes its next
run from now; it does not catch up on slots from while it was paused.

The history keeps the newest 100 runs per task. Each entry records the time, the
slot it was due for, the thread it used, the outcome (**Started**, **Skipped** or
**Failed**) and the failure or skip reason. A thread a run started is one click
away under **History**.

## Webhooks

Choose **On webhook** as the schedule. Saving gives the task a random 256-bit
path under `/webhooks/`, shown in the editor with **Copy** and **Rotate**.
Rotating replaces the path immediately and the old one answers 404. Every
request is a POST; other methods receive 405. The body is capped at 64 KiB (413
when larger) and one task accepts 30 deliveries in a rolling minute (429 after
that). An accepted request answers 202 immediately and starts the run on its own
task, so a slow harness never delays the caller.

The prompt decides what the agent sees. These placeholders are filled from the
request:

| Placeholder | Value |
|---|---|
| `{{body.path}}` | A JSON field by dotted path, such as `{{body.pull_request.html_url}}`; array indexes work too. |
| `{{headers.name}}` | A header, case-insensitively. |
| `{{query.name}}` | A query-string value. |
| `{{body}}` | The raw body. |
| `{{request}}` | The method, path, query, headers and body. |

A missing value becomes empty. At most 64 placeholders may appear in one
prompt. Each value is wrapped in a `<webhook-data name="…">` element, and the
prompt opens with a sentence saying that text is untrusted request data. A value
cannot close its own element: `<` is escaped. This helps an agent tell a
delivery from the person's instructions; it does not make the body safe. Anyone
with the URL can start a run with any content.

The daemon's HTTP server listens on the loopback bind by default. A public URL
needs a tunnel in front of that port, such as Tailscale Funnel, Cloudflare
Tunnel, ngrok or a reverse proxy. The Connect relay carries the typed WebSocket,
not plain HTTP, so it does not publish webhook URLs by itself. The editor shows
the path and not a host because the daemon cannot know which public name you
use.

## Protocol

The requests are `ListSchedules`, `CreateSchedule`, `UpdateSchedule`,
`DeleteSchedule`, `SetScheduleEnabled`, `RunScheduleNow`,
`ListScheduleRuns` and `RotateScheduleWebhook`. Reads need the `view` scope;
every write needs `admin`, because a task can start work and a webhook path is a
secret. `ScheduleEvent` is a transient frame that carries the current task after
a create, edit, run, enable/disable or rotation; a reconnecting client re-reads
the list instead of replaying it. Tasks, secrets and history live in the daemon
journal (`daemon_schedules`, `daemon_schedule_runs`), so they survive a restart.

A webhook task cannot be run with **Run now**; call its URL instead.
