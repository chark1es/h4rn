import {
  CopyIcon,
  HistoryIcon,
  PencilIcon,
  PlusIcon,
  RotateCwIcon,
  Trash2Icon,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";

import { ChipSelect } from "@/components/common/ChipSelect";
import { ConfirmDialog } from "@/components/common/ConfirmDialog";
import {
  loadAcpAgents,
  loadHarnessVersions,
  useHarnesses,
} from "@/features/harnesses/harnessStore";
import { useProjects } from "@/features/projects/store";
import type { HarnessKind, ScheduleDraft, ScheduleRun, ScheduleTask, ScheduleTaskId } from "@/gen";
import { onScheduleFrame } from "@/lib/client/frames";
import { HARNESS_LABELS, harnessLabel } from "@/lib/entities";
import { HARNESS_MODELS } from "@/lib/models";
import { openThread, useApp } from "@/lib/store/appStore";
import { MODE_CHOICES } from "@/lib/threadControls";

import {
  applyScheduleEvent,
  createSchedule,
  DESTINATIONS,
  deleteSchedule,
  describeDestination,
  describeRun,
  describeSchedule,
  draftOf,
  emptyDraft,
  formatDateTime,
  getSchedulesState,
  loadScheduleRuns,
  loadSchedules,
  loadScheduleWebhookUrl,
  localTimezone,
  nextRunText,
  outcomeWord,
  rotateScheduleWebhook,
  runScheduleNow,
  SCHEDULE_KINDS,
  type ScheduleFormErrors,
  type ScheduleKind,
  setScheduleEnabled,
  unitLabel,
  updateSchedule,
  useSchedules,
  validateDraft,
  WEBHOOK_PLACEHOLDERS,
  WEEKDAYS,
  withKind,
} from "./index";

const BUILTIN_HARNESSES: HarnessKind[] = ["claude_code", "codex", "opencode", "antigravity"];

/** A row's error line under a field. */
function FieldError({ message }: { message?: string | undefined }) {
  if (!message) return null;
  return (
    <div className="s" role="alert" style={{ color: "var(--danger)" }}>
      {message}
    </div>
  );
}

/** The schedule-specific fields of the editor. */
function ScheduleFields({
  draft,
  error,
  onChange,
}: {
  draft: ScheduleDraft;
  error?: string | undefined;
  onChange: (schedule: ScheduleDraft["schedule"]) => void;
}) {
  const schedule = draft.schedule;
  return (
    <>
      <div className="q-field">
        <div className="lab">
          <div className="t">When</div>
          <FieldError message={error} />
        </div>
        <div className="ctl">
          <ChipSelect<ScheduleKind>
            label="Schedule"
            value={schedule.kind}
            width={150}
            options={SCHEDULE_KINDS.map((entry) => ({ value: entry.kind, label: entry.label }))}
            onChange={(kind) => onChange(withKind(draft, kind).schedule)}
          />
        </div>
      </div>

      {schedule.kind === "once" ? (
        <div className="q-field">
          <div className="lab">
            <div className="t">Date and time</div>
          </div>
          <div className="ctl">
            <input
              className="q-input"
              type="date"
              aria-label="Date"
              value={schedule.date}
              onChange={(event) => onChange({ ...schedule, date: event.target.value })}
            />
            <input
              className="q-input"
              type="time"
              aria-label="Time"
              value={schedule.time}
              onChange={(event) => onChange({ ...schedule, time: event.target.value })}
            />
          </div>
        </div>
      ) : null}

      {schedule.kind === "every" ? (
        <div className="q-field">
          <div className="lab">
            <div className="t">Interval</div>
          </div>
          <div className="ctl">
            <input
              className="q-input"
              type="number"
              min={1}
              max={10080}
              aria-label="Interval count"
              value={schedule.count}
              onChange={(event) =>
                onChange({ ...schedule, count: Number(event.target.value) || 1 })
              }
              style={{ width: 80 }}
            />
            <ChipSelect<"minutes" | "hours">
              label="Interval unit"
              value={schedule.unit}
              width={110}
              options={[
                { value: "minutes", label: unitLabel("minutes") },
                { value: "hours", label: unitLabel("hours") },
              ]}
              onChange={(unit) => onChange({ ...schedule, unit })}
            />
          </div>
        </div>
      ) : null}

      {schedule.kind === "daily" ? (
        <div className="q-field">
          <div className="lab">
            <div className="t">Time</div>
          </div>
          <div className="ctl">
            <input
              className="q-input"
              type="time"
              aria-label="Daily time"
              value={schedule.time}
              onChange={(event) => onChange({ ...schedule, time: event.target.value })}
            />
          </div>
        </div>
      ) : null}

      {schedule.kind === "weekly" ? (
        <>
          <div className="q-field">
            <div className="lab">
              <div className="t">Days</div>
            </div>
            <div className="ctl" style={{ flexWrap: "wrap", justifyContent: "flex-end" }}>
              {WEEKDAYS.map((day) => {
                const on = schedule.days.includes(day.day);
                return (
                  <button
                    key={day.day}
                    type="button"
                    className={`q-btn ${on ? "secondary" : "ghost"}`}
                    aria-pressed={on}
                    aria-label={day.long}
                    style={{ minWidth: 42, height: 26 }}
                    onClick={() =>
                      onChange({
                        ...schedule,
                        days: on
                          ? schedule.days.filter((value) => value !== day.day)
                          : [...schedule.days, day.day].sort((left, right) => left - right),
                      })
                    }
                  >
                    {day.short}
                  </button>
                );
              })}
            </div>
          </div>
          <div className="q-field">
            <div className="lab">
              <div className="t">Time</div>
            </div>
            <div className="ctl">
              <input
                className="q-input"
                type="time"
                aria-label="Weekly time"
                value={schedule.time}
                onChange={(event) => onChange({ ...schedule, time: event.target.value })}
              />
            </div>
          </div>
        </>
      ) : null}

      {schedule.kind === "cron" ? (
        <div className="q-field">
          <div className="lab">
            <div className="t">Cron expression</div>
            <div className="s">minute hour day-of-month month day-of-week</div>
          </div>
          <div className="ctl">
            <input
              className="q-input q-mono"
              aria-label="Cron expression"
              placeholder="0 9 * * *"
              value={schedule.expression}
              onChange={(event) => onChange({ ...schedule, expression: event.target.value })}
              style={{ width: 210 }}
            />
          </div>
        </div>
      ) : null}
    </>
  );
}

/** The webhook URL, copy and rotate controls. */
function WebhookPanel({ task }: { task: ScheduleTask | null }) {
  const live = useSchedules((state) =>
    task ? (state.tasks.find((entry) => entry.task_id === task.task_id) ?? task) : null,
  );
  const path = useSchedules((state) => (task ? (state.webhookPaths[task.task_id] ?? null) : null));
  const loading = useSchedules((state) =>
    task ? state.webhookPathLoading === task.task_id : false,
  );
  const revealError = useSchedules((state) =>
    task && state.webhookPathError?.task_id === task.task_id
      ? state.webhookPathError.message
      : null,
  );
  const [copied, setCopied] = useState<string | null>(null);
  const [rotating, setRotating] = useState(false);
  const hasPath = live?.webhook_path_set ?? false;

  // The list never carries the secret; ask for it when the panel opens.
  const taskId = task?.task_id ?? null;
  useEffect(() => {
    if (!taskId) return;
    void loadScheduleWebhookUrl(taskId);
  }, [taskId]);

  const copy = async () => {
    if (!path) return;
    try {
      await navigator.clipboard.writeText(path);
      setCopied("Path copied");
    } catch {
      setCopied("Copy failed");
    }
  };
  return (
    <div className="q-group">
      <div className="q-group-head">
        <h2>Webhook</h2>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">URL path</div>
          <div className="s">
            {!live
              ? "Save the task to create its URL."
              : hasPath
                ? "Anyone with this path can start a run. Rotate it if it leaks."
                : "No URL yet."}
          </div>
        </div>
        <div className="ctl">
          <span className="q-mono q-muted" title={path ?? undefined} style={{ maxWidth: 360 }}>
            {path ??
              (hasPath && loading
                ? "Loading…"
                : revealError
                  ? "Unavailable"
                  : hasPath
                    ? "Hidden"
                    : "—")}
          </span>
          <button
            type="button"
            className="q-btn ghost"
            disabled={!path}
            aria-label="Copy webhook path"
            onClick={() => void copy()}
          >
            <CopyIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            Copy
          </button>
          <button
            type="button"
            className="q-btn secondary"
            disabled={!live || rotating}
            onClick={() => {
              if (!live) return;
              setRotating(true);
              void rotateScheduleWebhook(live.task_id).finally(() => setRotating(false));
            }}
          >
            <RotateCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            {rotating ? "Rotating…" : "Rotate"}
          </button>
        </div>
      </div>
      {revealError ? (
        <p className="q-muted" role="alert">
          {revealError}
        </p>
      ) : null}
      {copied ? (
        <p className="q-muted" role="status" aria-live="polite">
          {copied}
        </p>
      ) : null}
      <div className="q-field q-field-block">
        <div className="lab">
          <div className="t">Prompt placeholders</div>
          <div className="s">
            No value is left empty. A value is wrapped as untrusted request data.
          </div>
        </div>
        <div className="ctl" style={{ flexDirection: "column", alignItems: "stretch", gap: 4 }}>
          {WEBHOOK_PLACEHOLDERS.map((entry) => (
            <div key={entry.token} style={{ display: "flex", gap: 10 }}>
              <span className="q-mono" style={{ minWidth: 132 }}>
                {entry.token}
              </span>
              <span className="q-muted">{entry.hint}</span>
            </div>
          ))}
        </div>
      </div>
      <p className="q-muted">
        A public URL needs a tunnel to this daemon (the Connect relay carries the typed socket, not
        plain HTTP). Point Tailscale Funnel, Cloudflare Tunnel or another reverse proxy at this
        daemon's loopback port and keep the path.
      </p>
    </div>
  );
}

/** The editor for a new or existing task. */
function ScheduleEditor({
  task,
  host,
  timezone,
  onClose,
}: {
  task: ScheduleTask | null;
  host: string;
  timezone: string;
  onClose: () => void;
}) {
  const [draft, setDraft] = useState<ScheduleDraft>(() =>
    task ? draftOf(task) : emptyDraft(host, timezone),
  );
  const [errors, setErrors] = useState<ScheduleFormErrors>({});
  const saving = useSchedules((state) => state.saving);
  const saveError = useSchedules((state) => state.saveError);
  const harnessRows = useHarnesses((state) => state.rows);
  const agents = useHarnesses((state) => state.agents);
  const projects = useProjects((state) => state.projects);
  const harnesses = useMemo(() => {
    const installed = harnessRows
      .filter((row) => row.installed !== null)
      .map((row) => row.harness)
      .filter((kind): kind is HarnessKind => kind !== "acp");
    const list = installed.length > 0 ? [...installed] : [...BUILTIN_HARNESSES];
    if (!list.includes(draft.harness) && draft.harness !== "acp") {
      list.push(draft.harness);
    }
    return list;
  }, [harnessRows, draft.harness]);

  useEffect(() => {
    void loadHarnessVersions();
    void loadAcpAgents();
  }, []);

  const patch = (next: Partial<ScheduleDraft>) => setDraft({ ...draft, ...next });
  const save = async () => {
    const found = validateDraft(draft);
    setErrors(found);
    if (Object.keys(found).length > 0) return;
    const saved = task ? await updateSchedule(task.task_id, draft) : await createSchedule(draft);
    if (saved) onClose();
  };

  return (
    <section className="q-group" aria-label={task ? "Edit scheduled task" : "New scheduled task"}>
      <div className="q-group-head">
        <h2>{task ? "Edit task" : "New task"}</h2>
        <button type="button" className="q-btn ghost" onClick={onClose}>
          Close
        </button>
      </div>

      <div className="q-field">
        <div className="lab">
          <div className="t">Name</div>
          <FieldError message={errors.name} />
        </div>
        <div className="ctl">
          <input
            className="q-input"
            aria-label="Task name"
            aria-invalid={Boolean(errors.name)}
            value={draft.name}
            onChange={(event) => patch({ name: event.target.value })}
            style={{ width: 280 }}
          />
        </div>
      </div>

      <ScheduleFields
        draft={draft}
        error={errors.schedule}
        onChange={(schedule) => patch({ schedule })}
      />

      <div className="q-field">
        <div className="lab">
          <div className="t">Time zone</div>
          <div className="s">Wall-clock schedules use this zone</div>
          <FieldError message={errors.timezone} />
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            aria-label="Time zone"
            list="schedule-timezones"
            value={draft.timezone}
            onChange={(event) => patch({ timezone: event.target.value })}
            style={{ width: 220 }}
          />
          <datalist id="schedule-timezones">
            {timezoneOptions(timezone).map((name) => (
              <option key={name} value={name} />
            ))}
          </datalist>
        </div>
      </div>

      <div className="q-field">
        <div className="lab">
          <div className="t">Destination</div>
          <div className="s">
            {DESTINATIONS.find((entry) => entry.value === draft.destination)?.hint}
          </div>
        </div>
        <div className="ctl">
          <ChipSelect<ScheduleDraft["destination"]>
            label="Destination"
            value={draft.destination}
            width={220}
            options={DESTINATIONS.map((entry) => ({ value: entry.value, label: entry.label }))}
            onChange={(destination) => patch({ destination })}
          />
        </div>
      </div>

      <div className="q-field">
        <div className="lab">
          <div className="t">Workspace</div>
          <FieldError message={errors.workspace} />
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            aria-label="Workspace path"
            list="schedule-workspaces"
            value={draft.workspace_path}
            onChange={(event) => patch({ workspace_path: event.target.value })}
            style={{ width: 320 }}
          />
          <datalist id="schedule-workspaces">
            {projects.map((project) => (
              <option key={project.id} value={project.path}>
                {project.name}
              </option>
            ))}
          </datalist>
        </div>
      </div>

      <div className="q-field">
        <div className="lab">
          <div className="t">Harness</div>
        </div>
        <div className="ctl">
          <ChipSelect<string>
            label="Harness"
            value={draft.harness === "acp" ? `acp:${draft.agent_id ?? ""}` : draft.harness}
            width={200}
            options={[
              ...harnesses
                .filter((kind) => kind !== "acp")
                .map((kind) => ({ value: kind, label: harnessLabel(kind) })),
              ...agents.map((agent) => ({
                value: `acp:${agent.id}`,
                label: `${HARNESS_LABELS.acp} · ${agent.name}`,
              })),
              ...(draft.harness === "acp" &&
              draft.agent_id &&
              !agents.some((agent) => agent.id === draft.agent_id)
                ? [
                    {
                      value: `acp:${draft.agent_id}`,
                      label: `${HARNESS_LABELS.acp} · ${draft.agent_id}`,
                    },
                  ]
                : []),
            ]}
            onChange={(value) => {
              if (value.startsWith("acp:")) {
                const agent = value.slice(4) || (agents[0]?.id ?? null);
                patch({
                  harness: "acp",
                  agent_id: agent,
                  model: HARNESS_MODELS.acp.default,
                });
                return;
              }
              const harness = value as HarnessKind;
              patch({
                harness,
                agent_id: null,
                model: HARNESS_MODELS[harness].default,
              });
            }}
          />
        </div>
      </div>

      <div className="q-field">
        <div className="lab">
          <div className="t">Model</div>
          <FieldError message={errors.model} />
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            aria-label="Model"
            value={draft.model}
            onChange={(event) => patch({ model: event.target.value })}
            style={{ width: 240 }}
          />
        </div>
      </div>

      <div className="q-field">
        <div className="lab">
          <div className="t">Permissions</div>
        </div>
        <div className="ctl">
          <ChipSelect<ScheduleDraft["runtime_mode"]>
            label="Permissions"
            value={draft.runtime_mode}
            width={190}
            options={MODE_CHOICES.map((choice) => ({ value: choice.value, label: choice.label }))}
            onChange={(runtime_mode) => patch({ runtime_mode })}
          />
        </div>
      </div>

      <div className="q-field q-field-block">
        <div className="lab">
          <div className="t">Prompt</div>
          <FieldError message={errors.prompt} />
        </div>
        <div className="ctl" style={{ flexDirection: "column", alignItems: "stretch" }}>
          <textarea
            className="q-input"
            aria-label="Prompt"
            rows={6}
            value={draft.prompt}
            onChange={(event) => patch({ prompt: event.target.value })}
          />
        </div>
      </div>

      {draft.schedule.kind === "webhook" ? <WebhookPanel task={task} /> : null}

      <div className="q-field">
        <div className="lab">
          <div className="t">Enabled</div>
          <div className="s">A paused task keeps its history and URL.</div>
        </div>
        <div className="ctl">
          <button
            type="button"
            className="q-switch"
            role="switch"
            aria-checked={draft.enabled}
            aria-label="Task enabled"
            onClick={() => patch({ enabled: !draft.enabled })}
          />
        </div>
      </div>

      {Object.keys(errors).length > 0 ? (
        <p className="q-muted" role="alert">
          Fix the highlighted fields to save.
        </p>
      ) : null}
      {saveError ? (
        <p role="alert" style={{ color: "var(--danger)" }}>
          {saveError}
        </p>
      ) : null}
      <div style={{ display: "flex", justifyContent: "flex-end", gap: 8, paddingTop: 4 }}>
        <button type="button" className="q-btn ghost" onClick={onClose}>
          Cancel
        </button>
        <button
          type="button"
          className="q-btn primary"
          disabled={saving}
          onClick={() => void save()}
        >
          {saving ? "Saving…" : "Save task"}
        </button>
      </div>
    </section>
  );
}

function timezoneOptions(preferred: string): string[] {
  try {
    return Intl.supportedValuesOf("timeZone");
  } catch {
    return [preferred];
  }
}

/** One run in the history list. */
function RunRow({ run }: { run: ScheduleRun }) {
  const at = new Date(run.started_at);
  return (
    <div className="q-field">
      <div className="lab" style={{ width: "auto", flex: 1, minWidth: 0 }}>
        <div className="t">{outcomeWord(run)}</div>
        <div className="s">
          {Number.isNaN(at.getTime()) ? "" : formatDateTime(at)}
          {run.scheduled_for ? ` · scheduled ${formatDateTime(new Date(run.scheduled_for))}` : ""}
          {run.detail ? ` · ${run.detail}` : ""}
        </div>
      </div>
      <div className="ctl">
        {run.thread_id ? (
          <button
            type="button"
            className="q-btn ghost"
            onClick={() => run.thread_id && openThread(run.thread_id)}
          >
            Open thread
          </button>
        ) : null}
      </div>
    </div>
  );
}

/** One task in the list. */
function TaskRow({
  task,
  onEdit,
  onHistory,
  onDelete,
}: {
  task: ScheduleTask;
  onEdit: () => void;
  onHistory: () => void;
  onDelete: () => void;
}) {
  const busy = useSchedules((state) => state.saving);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const toggle = async () => {
    setError(null);
    const ok = await setScheduleEnabled(task.task_id, !task.enabled);
    if (!ok) setError(getSchedulesState().saveError ?? "The task did not change.");
  };
  const run = async () => {
    setRunning(true);
    setError(null);
    const ok = await runScheduleNow(task.task_id);
    if (!ok) setError(getSchedulesState().saveError ?? "The run did not start.");
    setRunning(false);
  };
  return (
    <div className="q-field">
      <div className="lab" style={{ width: "auto", flex: 1, minWidth: 0 }}>
        <div className="t">{task.name}</div>
        <div className="s">
          {describeSchedule(task.schedule)} · {describeDestination(task.destination)} ·{" "}
          {task.timezone}
        </div>
        <div className="s">
          {nextRunText(task)}
          {task.last_run ? ` · ${describeRun(task.last_run)}` : " · Never run"}
        </div>
        {error ? (
          <div className="s" role="alert" style={{ color: "var(--danger)" }}>
            {error}
          </div>
        ) : null}
      </div>
      <div className="ctl" style={{ flex: "0 1 auto", justifyContent: "flex-end" }}>
        <button
          type="button"
          className="q-switch"
          role="switch"
          aria-checked={task.enabled}
          aria-label={`${task.enabled ? "Pause" : "Resume"} ${task.name}`}
          disabled={busy}
          onClick={() => void toggle()}
        />
        {task.schedule.kind !== "webhook" ? (
          <button
            type="button"
            className="q-btn secondary"
            disabled={running || busy}
            aria-label={`Run ${task.name} now`}
            onClick={() => void run()}
          >
            {running ? "Starting…" : "Run now"}
          </button>
        ) : null}
        <button
          type="button"
          className="q-btn ghost"
          aria-label={`History for ${task.name}`}
          onClick={onHistory}
        >
          <HistoryIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          History
        </button>
        <button
          type="button"
          className="q-btn ghost"
          aria-label={`Edit ${task.name}`}
          onClick={onEdit}
        >
          <PencilIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          Edit
        </button>
        <button
          type="button"
          className="q-btn ghost danger"
          aria-label={`Delete ${task.name}`}
          onClick={onDelete}
        >
          <Trash2Icon className="q-icon s14" strokeWidth={1.5} aria-hidden />
        </button>
      </div>
    </div>
  );
}

/** Settings → Scheduled tasks. */
export function SchedulesSettings() {
  const online = useApp((state) => state.connection.status) === "online";
  const host = useApp((state) => state.localHostId);
  const daemonTimezone = useApp((state) => state.daemonTimezone);
  const timezone = daemonTimezone || localTimezone();
  const tasks = useSchedules((state) => state.tasks);
  const loaded = useSchedules((state) => state.loaded);
  const loading = useSchedules((state) => state.loading);
  const error = useSchedules((state) => state.error);
  const saveError = useSchedules((state) => state.saveError);
  const runsLoading = useSchedules((state) => state.runsLoading);
  const runsError = useSchedules((state) => state.runsError);
  const runs = useSchedules((state) => state.runs);

  const [editing, setEditing] = useState<ScheduleTask | "new" | null>(null);
  const [historyFor, setHistoryFor] = useState<ScheduleTaskId | null>(null);
  const [removing, setRemoving] = useState<ScheduleTask | null>(null);

  useEffect(() => {
    if (online) void loadSchedules();
  }, [online]);
  useEffect(() => onScheduleFrame(applyScheduleEvent), []);

  const historyTask = historyFor
    ? (tasks.find((task) => task.task_id === historyFor) ?? null)
    : null;
  const openHistory = (taskId: ScheduleTaskId) => {
    setHistoryFor(taskId);
    void loadScheduleRuns(taskId);
  };

  return (
    <div className="q-form">
      <div className="q-form-head">
        <div>
          <h1>Scheduled tasks</h1>
          <p className="lead">
            Run a prompt on a schedule or when a webhook is called. The daemon owns the timer, so
            closing this app does not stop a task. A run that is still working makes the next slot
            skip, and a missed slot runs once when the daemon comes back.
          </p>
        </div>
        <div style={{ display: "flex", gap: 8 }}>
          <button
            type="button"
            className="q-btn ghost"
            disabled={loading || !online}
            onClick={() => void loadSchedules()}
          >
            Refresh
          </button>
          <button
            type="button"
            className="q-btn primary"
            disabled={!online || !host}
            onClick={() => setEditing("new")}
          >
            <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            New task
          </button>
        </div>
      </div>

      {error ? (
        <p className="q-muted" role="alert">
          {error.message}
        </p>
      ) : null}
      {saveError ? (
        <p role="alert" style={{ color: "var(--danger)" }}>
          {saveError}
        </p>
      ) : null}

      {editing ? (
        <ScheduleEditor
          key={editing === "new" ? "new" : editing.task_id}
          task={editing === "new" ? null : editing}
          host={host ?? ""}
          timezone={timezone}
          onClose={() => setEditing(null)}
        />
      ) : null}

      <div className="q-group">
        <div className="q-group-head">
          <h2>Tasks</h2>
        </div>
        {!loaded && online ? (
          <div className="q-skel" style={{ height: 120 }} />
        ) : !online ? (
          <p className="q-muted">Offline</p>
        ) : tasks.length === 0 ? (
          <p className="q-muted">
            No scheduled tasks yet. Start one for a recurring review, a nightly check, or a webhook
            from another service.
          </p>
        ) : (
          tasks.map((task) => (
            <TaskRow
              key={task.task_id}
              task={task}
              onEdit={() => setEditing(task)}
              onHistory={() => openHistory(task.task_id)}
              onDelete={() => setRemoving(task)}
            />
          ))
        )}
      </div>

      {historyTask ? (
        <div className="q-group">
          <div className="q-group-head">
            <h2>History · {historyTask.name}</h2>
            <button type="button" className="q-btn ghost" onClick={() => setHistoryFor(null)}>
              Close
            </button>
          </div>
          {runsError ? (
            <p role="alert" style={{ color: "var(--danger)" }}>
              {runsError}
            </p>
          ) : null}
          {runsLoading === historyTask.task_id ? (
            <div className="q-skel" style={{ height: 80 }} />
          ) : (runs[historyTask.task_id]?.length ?? 0) === 0 ? (
            <p className="q-muted">No runs yet.</p>
          ) : (
            runs[historyTask.task_id]?.map((run) => <RunRow key={run.run_id} run={run} />)
          )}
        </div>
      ) : null}

      <ConfirmDialog
        open={removing !== null}
        title="Delete this scheduled task?"
        description={
          removing
            ? `"${removing.name}" and its run history are deleted. Threads it already started stay.`
            : ""
        }
        confirmLabel="Delete"
        onOpenChange={(open) => {
          if (!open) setRemoving(null);
        }}
        onConfirm={() => {
          if (removing) void deleteSchedule(removing.task_id);
          setRemoving(null);
        }}
      />
    </div>
  );
}
