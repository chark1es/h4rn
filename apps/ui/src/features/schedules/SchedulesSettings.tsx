import {
  CopyIcon,
  HistoryIcon,
  PencilIcon,
  PlusIcon,
  RotateCwIcon,
  Trash2Icon,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { ChipSelect } from "@/components/common/ChipSelect";
import { ConfirmDialog } from "@/components/common/ConfirmDialog";
import { SuggestInput } from "@/components/ui/autocomplete";
import { Switch } from "@/components/ui/switch";
import {
  loadAcpAgents,
  loadHarnessVersions,
  useHarnesses,
} from "@/features/harnesses/harnessStore";
import { useProjects } from "@/features/projects/store";
import type { HarnessKind, ScheduleDraft, ScheduleRun, ScheduleTask, ScheduleTaskId } from "@/gen";
import { onScheduleFrame } from "@/lib/client/frames";
import { HARNESS_LABELS, harnessLabel } from "@/lib/entities";
import { i18n } from "@/lib/i18n";
import { HARNESS_MODELS } from "@/lib/models";
import { openThread, pushToast, useApp } from "@/lib/store/appStore";
import { MODE_CHOICES } from "@/lib/threadControls";
import {
  applyScheduleEvent,
  createSchedule,
  DESTINATIONS,
  deleteSchedule,
  describeDestination,
  describeRun,
  describeSchedule,
  destinationHint,
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
  placeholderHint,
  rotateScheduleWebhook,
  runScheduleNow,
  SCHEDULE_KINDS,
  type ScheduleFormErrors,
  type ScheduleKind,
  scheduleKindLabel,
  setScheduleEnabled,
  unitLabel,
  updateSchedule,
  useSchedules,
  validateDraft,
  WEBHOOK_PLACEHOLDERS,
  WEEKDAYS,
  weekdayName,
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
  const { t } = useTranslation(["settings", "common"]);
  const schedule = draft.schedule;
  return (
    <>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("schedules.editor.when")}</div>
          <FieldError message={error} />
        </div>
        <div className="ctl">
          <ChipSelect<ScheduleKind>
            label={t("schedules.editor.schedule")}
            value={schedule.kind}
            className="w-37.5"
            options={SCHEDULE_KINDS.map((kind) => ({
              value: kind,
              label: scheduleKindLabel(kind),
            }))}
            onChange={(kind) => onChange(withKind(draft, kind).schedule)}
          />
        </div>
      </div>

      {schedule.kind === "once" ? (
        <div className="q-field">
          <div className="lab">
            <div className="t">{t("schedules.editor.dateTime")}</div>
          </div>
          <div className="ctl">
            <input
              className="q-input"
              type="date"
              aria-label={t("schedules.editor.date")}
              value={schedule.date}
              onChange={(event) => onChange({ ...schedule, date: event.target.value })}
            />
            <input
              className="q-input"
              type="time"
              aria-label={t("schedules.editor.time")}
              value={schedule.time}
              onChange={(event) => onChange({ ...schedule, time: event.target.value })}
            />
          </div>
        </div>
      ) : null}

      {schedule.kind === "every" ? (
        <div className="q-field">
          <div className="lab">
            <div className="t">{t("schedules.editor.interval")}</div>
          </div>
          <div className="ctl">
            <input
              className="q-input"
              type="number"
              min={1}
              max={10080}
              aria-label={t("schedules.editor.intervalCount")}
              value={schedule.count}
              onChange={(event) =>
                onChange({ ...schedule, count: Number(event.target.value) || 1 })
              }
              style={{ width: 80 }}
            />
            <ChipSelect<"minutes" | "hours">
              label={t("schedules.editor.intervalUnit")}
              value={schedule.unit}
              className="w-27.5"
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
            <div className="t">{t("schedules.editor.time")}</div>
          </div>
          <div className="ctl">
            <input
              className="q-input"
              type="time"
              aria-label={t("schedules.editor.dailyTime")}
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
              <div className="t">{t("schedules.editor.days")}</div>
            </div>
            <div className="ctl" style={{ flexWrap: "wrap", justifyContent: "flex-end" }}>
              {WEEKDAYS.map((day) => {
                const on = schedule.days.includes(day);
                return (
                  <button
                    key={day}
                    type="button"
                    className={`q-btn ${on ? "secondary" : "ghost"}`}
                    aria-pressed={on}
                    aria-label={weekdayName(day, "long")}
                    style={{ minWidth: 42, height: 26 }}
                    onClick={() =>
                      onChange({
                        ...schedule,
                        days: on
                          ? schedule.days.filter((value) => value !== day)
                          : [...schedule.days, day].sort((left, right) => left - right),
                      })
                    }
                  >
                    {weekdayName(day, "short")}
                  </button>
                );
              })}
            </div>
          </div>
          <div className="q-field">
            <div className="lab">
              <div className="t">{t("schedules.editor.time")}</div>
            </div>
            <div className="ctl">
              <input
                className="q-input"
                type="time"
                aria-label={t("schedules.editor.weeklyTime")}
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
            <div className="t">{t("schedules.editor.cron")}</div>
            <div className="s">{t("schedules.editor.cronHint")}</div>
          </div>
          <div className="ctl">
            <input
              className="q-input q-mono"
              aria-label={t("schedules.editor.cron")}
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
  const { t } = useTranslation(["settings", "common"]);
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
      pushToast({ title: i18n.t("settings:schedules.webhook.copied"), variant: "success" });
    } catch {
      pushToast({ title: i18n.t("settings:schedules.webhook.copyFailed"), variant: "error" });
    }
  };
  return (
    <div className="q-group">
      <div className="q-group-head">
        <h2>{t("schedules.webhook.title")}</h2>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("schedules.webhook.path")}</div>
          <div className="s">
            {!live
              ? t("schedules.webhook.saveFirst")
              : hasPath
                ? t("schedules.webhook.secret")
                : t("schedules.webhook.none")}
          </div>
        </div>
        <div className="ctl">
          <span className="q-mono q-muted" title={path ?? undefined} style={{ maxWidth: 360 }}>
            {path ??
              (hasPath && loading
                ? t("common:loading")
                : revealError
                  ? t("schedules.webhook.unavailable")
                  : hasPath
                    ? t("schedules.webhook.hidden")
                    : "—")}
          </span>
          <button
            type="button"
            className="q-btn ghost"
            disabled={!path}
            aria-label={t("schedules.webhook.copyLabel")}
            onClick={() => void copy()}
          >
            <CopyIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            {t("common:copy")}
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
            {rotating ? t("schedules.webhook.rotating") : t("schedules.webhook.rotate")}
          </button>
        </div>
      </div>
      {revealError ? (
        <p className="q-muted" role="alert">
          {revealError}
        </p>
      ) : null}
      <div className="q-field q-field-block">
        <div className="lab">
          <div className="t">{t("schedules.webhook.placeholders")}</div>
          <div className="s">{t("schedules.webhook.placeholdersHint")}</div>
        </div>
        <div className="ctl" style={{ flexDirection: "column", alignItems: "stretch", gap: 4 }}>
          {WEBHOOK_PLACEHOLDERS.map((entry) => (
            <div key={entry.token} style={{ display: "flex", gap: 10 }}>
              <span className="q-mono" style={{ minWidth: 132 }}>
                {entry.token}
              </span>
              <span className="q-muted">{placeholderHint(entry)}</span>
            </div>
          ))}
        </div>
      </div>
      <p className="q-muted">{t("schedules.webhook.tunnel")}</p>
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
  const { t } = useTranslation(["settings", "common"]);
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
    <section
      className="q-group"
      aria-label={task ? t("schedules.editor.editLabel") : t("schedules.editor.newLabel")}
    >
      <div className="q-group-head">
        <h2>{task ? t("schedules.editor.editTitle") : t("schedules.newTask")}</h2>
        <button type="button" className="q-btn ghost" onClick={onClose}>
          {t("common:close")}
        </button>
      </div>

      <div className="q-field">
        <div className="lab">
          <div className="t">{t("textGeneration.name")}</div>
          <FieldError message={errors.name} />
        </div>
        <div className="ctl">
          <input
            className="q-input"
            aria-label={t("schedules.editor.nameLabel")}
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
          <div className="t">{t("schedules.editor.timezone")}</div>
          <div className="s">{t("schedules.editor.timezoneHint")}</div>
          <FieldError message={errors.timezone} />
        </div>
        <div className="ctl">
          <SuggestInput
            aria-label={t("schedules.editor.timezone")}
            value={draft.timezone}
            onValueChange={(next) => patch({ timezone: next })}
            suggestions={timezoneOptions(timezone)}
            showLabel={t("common:showSuggestions")}
            mono
            className="w-55"
          />
        </div>
      </div>

      <div className="q-field">
        <div className="lab">
          <div className="t">{t("schedules.editor.destination")}</div>
          <div className="s">{destinationHint(draft.destination)}</div>
        </div>
        <div className="ctl">
          <ChipSelect<ScheduleDraft["destination"]>
            label={t("schedules.editor.destination")}
            value={draft.destination}
            className="w-55"
            options={DESTINATIONS.map((value) => ({
              value,
              label: describeDestination(value),
            }))}
            onChange={(destination) => patch({ destination })}
          />
        </div>
      </div>

      <div className="q-field">
        <div className="lab">
          <div className="t">{t("schedules.editor.workspace")}</div>
          <FieldError message={errors.workspace} />
        </div>
        <div className="ctl">
          <SuggestInput
            aria-label={t("schedules.editor.workspaceLabel")}
            value={draft.workspace_path}
            onValueChange={(next) => patch({ workspace_path: next })}
            suggestions={projects.map((project) => project.path)}
            showLabel={t("common:showSuggestions")}
            mono
            className="w-80"
          />
        </div>
      </div>

      <div className="q-field">
        <div className="lab">
          <div className="t">{t("schedules.editor.harness")}</div>
        </div>
        <div className="ctl">
          <ChipSelect<string>
            label={t("schedules.editor.harness")}
            value={draft.harness === "acp" ? `acp:${draft.agent_id ?? ""}` : draft.harness}
            className="w-50"
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
          <div className="t">{t("sourceControl.model")}</div>
          <FieldError message={errors.model} />
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            aria-label={t("sourceControl.model")}
            value={draft.model}
            onChange={(event) => patch({ model: event.target.value })}
            style={{ width: 240 }}
          />
        </div>
      </div>

      <div className="q-field">
        <div className="lab">
          <div className="t">{t("threads.permissions")}</div>
        </div>
        <div className="ctl">
          <ChipSelect<ScheduleDraft["runtime_mode"]>
            label={t("threads.permissions")}
            value={draft.runtime_mode}
            className="w-47.5"
            options={MODE_CHOICES.map((choice) => ({
              value: choice.value,
              label: t(`common:permissionModes.${choice.value}`),
            }))}
            onChange={(runtime_mode) => patch({ runtime_mode })}
          />
        </div>
      </div>

      <div className="q-field q-field-block">
        <div className="lab">
          <div className="t">{t("schedules.editor.prompt")}</div>
          <FieldError message={errors.prompt} />
        </div>
        <div className="ctl" style={{ flexDirection: "column", alignItems: "stretch" }}>
          <textarea
            className="q-input"
            aria-label={t("schedules.editor.prompt")}
            rows={6}
            value={draft.prompt}
            onChange={(event) => patch({ prompt: event.target.value })}
          />
        </div>
      </div>

      {draft.schedule.kind === "webhook" ? <WebhookPanel task={task} /> : null}

      <div className="q-field">
        <div className="lab">
          <div className="t">{t("schedules.editor.enabled")}</div>
          <div className="s">{t("schedules.editor.enabledHint")}</div>
        </div>
        <div className="ctl">
          <Switch
            checked={draft.enabled}
            aria-label={t("schedules.editor.enabledLabel")}
            onCheckedChange={() => patch({ enabled: !draft.enabled })}
          />
        </div>
      </div>

      {Object.keys(errors).length > 0 ? (
        <p className="q-muted" role="alert">
          {t("schedules.editor.fixFields")}
        </p>
      ) : null}
      {saveError ? (
        <p role="alert" style={{ color: "var(--danger)" }}>
          {saveError}
        </p>
      ) : null}
      <div style={{ display: "flex", justifyContent: "flex-end", gap: 8, paddingTop: 4 }}>
        <button type="button" className="q-btn ghost" onClick={onClose}>
          {t("common:cancel")}
        </button>
        <button
          type="button"
          className="q-btn primary"
          disabled={saving}
          onClick={() => void save()}
        >
          {saving ? t("common:saving") : t("schedules.editor.save")}
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
  const { t } = useTranslation(["settings", "common"]);
  const at = new Date(run.started_at);
  return (
    <div className="q-field">
      <div className="lab" style={{ width: "auto", flex: 1, minWidth: 0 }}>
        <div className="t">{outcomeWord(run)}</div>
        <div className="s">
          {Number.isNaN(at.getTime()) ? "" : formatDateTime(at)}
          {run.scheduled_for
            ? ` · ${t("schedules.run.scheduled", { when: formatDateTime(new Date(run.scheduled_for)) })}`
            : ""}
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
            {t("schedules.run.openThread")}
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
  const { t } = useTranslation(["settings", "common"]);
  const busy = useSchedules((state) => state.saving);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const toggle = async () => {
    setError(null);
    const ok = await setScheduleEnabled(task.task_id, !task.enabled);
    if (!ok) setError(getSchedulesState().saveError ?? t("schedules.task.toggleFailed"));
  };
  const run = async () => {
    setRunning(true);
    setError(null);
    const ok = await runScheduleNow(task.task_id);
    if (!ok) setError(getSchedulesState().saveError ?? t("schedules.task.runFailed"));
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
          {` · ${describeRun(task.last_run)}`}
        </div>
        {error ? (
          <div className="s" role="alert" style={{ color: "var(--danger)" }}>
            {error}
          </div>
        ) : null}
      </div>
      <div className="ctl" style={{ flex: "0 1 auto", justifyContent: "flex-end" }}>
        <Switch
          checked={task.enabled}
          aria-label={
            task.enabled
              ? t("schedules.task.pause", { name: task.name })
              : t("schedules.task.resume", { name: task.name })
          }
          disabled={busy}
          onCheckedChange={() => void toggle()}
        />
        {task.schedule.kind !== "webhook" ? (
          <button
            type="button"
            className="q-btn secondary"
            disabled={running || busy}
            aria-label={t("schedules.task.runLabel", { name: task.name })}
            onClick={() => void run()}
          >
            {running ? t("forges.device.starting") : t("schedules.task.run")}
          </button>
        ) : null}
        <button
          type="button"
          className="q-btn ghost"
          aria-label={t("schedules.task.historyLabel", { name: task.name })}
          onClick={onHistory}
        >
          <HistoryIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          {t("schedules.task.history")}
        </button>
        <button
          type="button"
          className="q-btn ghost"
          aria-label={t("harnesses.acp.editLabel", { name: task.name })}
          onClick={onEdit}
        >
          <PencilIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          {t("common:edit")}
        </button>
        <button
          type="button"
          className="q-btn ghost danger"
          aria-label={t("schedules.task.deleteLabel", { name: task.name })}
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
  const { t } = useTranslation(["settings", "common"]);
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
          <h1>{t("schedules.title")}</h1>
          <p className="lead">{t("schedules.lead")}</p>
        </div>
        <div style={{ display: "flex", gap: 8 }}>
          <button
            type="button"
            className="q-btn ghost"
            disabled={loading || !online}
            onClick={() => void loadSchedules()}
          >
            {t("common:refresh")}
          </button>
          <button
            type="button"
            className="q-btn primary"
            disabled={!online || !host}
            onClick={() => setEditing("new")}
          >
            <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            {t("schedules.newTask")}
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
          <h2>{t("schedules.tasks")}</h2>
        </div>
        {!loaded && online ? (
          <div className="q-skel" style={{ height: 120 }} />
        ) : !online ? (
          <p className="q-muted">{t("worktrees.offline")}</p>
        ) : tasks.length === 0 ? (
          <p className="q-muted">{t("schedules.empty")}</p>
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
            <h2>{t("schedules.historyTitle", { name: historyTask.name })}</h2>
            <button type="button" className="q-btn ghost" onClick={() => setHistoryFor(null)}>
              {t("common:close")}
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
            <p className="q-muted">{t("schedules.noRuns")}</p>
          ) : (
            runs[historyTask.task_id]?.map((run) => <RunRow key={run.run_id} run={run} />)
          )}
        </div>
      ) : null}

      <ConfirmDialog
        open={removing !== null}
        title={t("schedules.delete.title")}
        description={removing ? t("schedules.delete.description", { name: removing.name }) : ""}
        confirmLabel={t("common:delete")}
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
