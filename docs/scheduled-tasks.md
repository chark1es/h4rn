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
(`TZ`, then the system's IANA zone: `/etc/localtime` on Unix, or the Windows
zone mapped to an IANA name). The daemon reports that zone in its handshake, so
the editor's default follows the selected daemon rather than the phone or
laptop you happen to be typing on. The editor offers a searchable list of IANA
names. Intervals are absolute durations, so "every 6 hours" stays six hours
across a daylight-saving change; a daily or weekly time follows the wall clock,
so 09:30 stays 09:30. A time the spring-forward gap skips runs at the first
valid minute after the gap. A **Daily** or **Weekly** time in the repeated
fall-back hour runs at the first occurrence only, so 02:30 in Berlin runs once
on the fall-back day; the same clock time a week later is the next run. A cron
expression with several jobs in that hour returns the earliest UTC instant
among them, so 02:00 and 02:30 jobs in Berlin both run in the first pass. A
**Once** slot is taken when it is admitted, before the harness is asked to
start: it never matches again in the repeated hour, and neither an edit nor a
pause during the run followed by a resume re-arms the same date and time.
Naming a different date or time in an edit does arm a new slot.

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

Changing a one-thread task's workspace or harness starts a new thread on its
next run, because the old conversation belongs to a different checkout or
agent. Switching to **New thread each run** drops the task's link to its old
thread; the thread itself is not deleted. Before each run into an existing
thread, the daemon reconciles the thread's permission mode and model with the
task, so a run uses the settings the task was saved with rather than whatever
that thread last switched to.

A thread with an enabled one-thread task keeps its worktree through a clear.
Settings → Worktrees reports it as skipped with **has an enabled scheduled
task**; pausing or deleting the task releases it.

## Missed runs and overlap

If the daemon was down at a due time, the task runs once when it starts, not
once for every missed slot. Its next run is then computed from the current
time, so a long outage does not produce a burst.

A task whose previous run is still working, or whose slot is already being
started, skips its slot and records the skip in the history. Admission is
atomic: it rechecks that the task is still enabled, still due and not already
in flight before any harness is asked to start, so a pause or an edit that
lands first wins. A manual **Run now** on a busy task is refused with a reason
instead of queuing a second prompt. **Run now** also works while a task is
paused: the person asked for that run, and it leaves the schedule and the
enabled state alone. Enabling a paused task computes its next run from now; it
does not catch up on slots from while it was paused.

If a run's journal write fails, only that write is retried, on its own
backoff deadline (30 seconds, doubling up to an hour). The task's real next
slot is kept, the failed slot is never admitted again, and a webhook or manual
run does not acquire a clock time. If the stored schedule itself cannot
produce an instant (an invalid time zone, for example), the task backs off 30
seconds, doubling up to an hour, so the runner does not retry every instant. A
**Once** task whose run started is spent even when its write is still
retrying.

The history keeps the newest 100 runs per task, and a run and the task's
advanced clock are committed in one journal transaction. Each entry records the time, the
slot it was due for, the thread it used, the outcome (**Started**, **Skipped** or
**Failed**) and the failure or skip reason. A thread a run started is one click
away under **History**.

## Webhooks

Choose **On webhook** as the schedule. Saving gives the task a random 256-bit
path under `/webhooks/`, which **anyone holding it** can use to start a run. The
path is a secret: a listed task and a change event carry only a "has a URL"
flag, and the editor reads the path through an admin-only request when its panel
opens. The editor's **Copy** and **Rotate** controls work on that value. Rotating replaces
the path immediately and the old one answers 404. Every request is a POST;
other methods receive 405. The path is checked and a delivery slot taken before
the body is read, parsed or copied, so a wrong path is cheap. The body is capped
at 64 KiB (413 when larger) and one task accepts 30 deliveries in a rolling
minute (429 after that). An accepted request answers 202 immediately and starts
the run on its own task, so a slow harness never delays the caller. Two
deliveries that arrive while one is starting start one run; the other is
recorded as a skip.

The prompt decides what the agent sees. These placeholders are filled from the
request:

| Placeholder | Value |
|---|---|
| `{{body.path}}` | A JSON field by dotted path, such as `{{body.pull_request.html_url}}`; array indexes work too. |
| `{{headers.name}}` | A header, case-insensitively. |
| `{{query.name}}` | A query-string value. |
| `{{body}}` | The raw body. |
| `{{request}}` | The method, path, query, headers and body, with the secret path segment and credentials removed. |

A missing value becomes empty. At most 64 placeholders may appear in one
prompt, and a rendered prompt is capped at 512 KiB. Each value is wrapped in a
`<webhook-data name="…">` element, and the prompt opens with a sentence saying
that text is untrusted request data. The secret path segment is replaced with
`[redacted]`, credential headers (`Authorization`, `Cookie`, `X-…-Token`,
`X-Hub-Signature…`) lose their values, and the forge token patterns are removed
from the body, query and header values, so a delivery cannot echo a credential
into the transcript. A value cannot close its own element: `&` and `<` are
escaped, so an entity-encoded closing tag is inert too. This helps an agent tell a
delivery from the person's instructions; it does not make the body safe. Anyone
with the URL can start a run with any content.

Which listener answers a webhook is explicit:

| Listener | Webhooks? | Who can reach it |
|---|---|---|
| The primary bind (`--bind`, default `127.0.0.1`) | Yes | This machine only while the default is loopback; a non-loopback `--bind` serves webhooks on that address instead. |
| An extra bind you name (`--extra-bind`) | Yes | Whatever that address exposes. |
| The tailnet address Tailscale auto mode discovers | No | Paired devices for `/secure`; plain HTTP webhooks are absent. |
| The Settings → Network access LAN listener | No | Paired devices for the Noise routes only. |
| The Connect relay | No | It carries the typed WebSocket, not plain HTTP. |

Exposure follows the address you configured: the default loopback bind is
reachable only from this machine, while a `--bind` on a LAN or tunnel address
serves its webhook paths to whoever can reach that address. A public URL
therefore needs a tunnel in front of the bind port, such as Tailscale Funnel,
Cloudflare Tunnel, ngrok or a reverse proxy, unless the bind itself already
names a reachable address. An explicit extra bind is the way to serve webhooks
on a LAN address without a tunnel. The editor shows the path and not a host because the daemon cannot know
which public name you use.

## Protocol

The requests are `ListSchedules`, `CreateSchedule`, `UpdateSchedule`,
`DeleteSchedule`, `SetScheduleEnabled`, `RunScheduleNow`, `ListScheduleRuns`,
`RotateScheduleWebhook` and `GetScheduleWebhookUrl`. Reads need the `view`
scope; every write and the reveal need `admin`, because a task can start work
and a webhook path is a secret. A listed task never carries the path, and
`ScheduleEvent` never does either: `RotateScheduleWebhook` and
`GetScheduleWebhookUrl` answer with a `ScheduleWebhookUrl` body instead. `ScheduleEvent` is a transient frame that carries the current task after
a create, edit, run, enable/disable or rotation; a reconnecting client re-reads
the list instead of replaying it. Tasks, secrets and history live in the daemon
journal (`daemon_schedules`, `daemon_schedule_runs`), so they survive a restart.

A webhook task cannot be run with **Run now**; call its URL instead.
