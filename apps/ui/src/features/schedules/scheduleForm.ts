/**
 * The Scheduled tasks editor's pure rules.
 *
 * The daemon is the authority on saving (it validates again and computes the
 * next real run). These helpers keep the form quiet and immediate: a field
 * error appears before a request is sent, and the list describes each task in
 * the same words the editor uses.
 */
import type {
  IntervalUnit,
  ScheduleDestination,
  ScheduleDraft,
  ScheduleRun,
  ScheduleSpec,
  ScheduleTask,
} from "@/gen";
import { activeLocale, i18n } from "@/lib/i18n";

export type ScheduleKind = ScheduleSpec["kind"];
export type ScheduleField = "name" | "workspace" | "model" | "prompt" | "timezone" | "schedule";
export type ScheduleFormErrors = Partial<Record<ScheduleField, string>>;
type CronField = "minute" | "hour" | "dayOfMonth" | "month" | "dayOfWeek";

/** The schedule kinds in the order the editor offers them. */
export const SCHEDULE_KINDS: ScheduleKind[] = [
  "once",
  "every",
  "daily",
  "weekly",
  "cron",
  "webhook",
];

/** A schedule kind in the editor's words. */
export function scheduleKindLabel(kind: ScheduleKind): string {
  return i18n.t(`settings:schedules.kinds.${kind}`);
}

/** Where a run goes, in the order the editor offers them. */
export const DESTINATIONS: ScheduleDestination[] = [
  "new_thread",
  "continue_thread",
  "fresh_session",
];

/** What a destination means, under the picker. */
export function destinationHint(destination: ScheduleDestination): string {
  return i18n.t(`settings:schedules.destinations.${destination}.hint`);
}

/** Weekdays as the editor numbers them (Monday = 1). */
export const WEEKDAYS: number[] = [1, 2, 3, 4, 5, 6, 7];

/**
 * A weekday's name in the active language, from Intl rather than a list of
 * words. 2024-01-01 was a Monday, so day `n` is 2024-01-0n.
 */
export function weekdayName(day: number, width: "short" | "long"): string {
  const date = new Date(Date.UTC(2024, 0, day, 12));
  return new Intl.DateTimeFormat(activeLocale(), { weekday: width, timeZone: "UTC" }).format(date);
}

/** The webhook placeholders, with one example each. */
export const WEBHOOK_PLACEHOLDERS = [
  { token: "{{body.path}}", id: "jsonField", example: "{{body.pull_request.html_url}}" },
  { token: "{{headers.name}}", id: "header", example: "{{headers.x-github-event}}" },
  { token: "{{query.name}}", id: "query", example: "{{query.ref}}" },
  { token: "{{body}}", id: "body", example: null },
  { token: "{{request}}", id: "request", example: null },
] as const;

/** What a webhook placeholder stands for. The example is never interpolated. */
export function placeholderHint(entry: (typeof WEBHOOK_PLACEHOLDERS)[number]): string {
  return entry.example
    ? i18n.t(`settings:schedules.placeholders.${entry.id}`, { example: entry.example })
    : i18n.t(`settings:schedules.placeholders.${entry.id}`);
}

/** A blank draft for a new task. */
export function emptyDraft(host: string, timezone: string): ScheduleDraft {
  return {
    name: "",
    host,
    workspace_path: "",
    harness: "claude_code",
    agent_id: null,
    model: "",
    runtime_mode: "approval_required",
    prompt: "",
    schedule: { kind: "daily", time: "09:00" },
    timezone,
    destination: "new_thread",
    enabled: true,
  };
}

/** The editable draft of a stored task. */
export function draftOf(task: ScheduleTask): ScheduleDraft {
  return {
    name: task.name,
    host: task.host,
    workspace_path: task.workspace_path,
    harness: task.harness,
    agent_id: task.agent_id,
    model: task.model,
    runtime_mode: task.runtime_mode,
    prompt: task.prompt,
    schedule: task.schedule,
    timezone: task.timezone,
    destination: task.destination,
    enabled: task.enabled,
  };
}

/**
 * Switches the schedule kind, keeping the fields that still make sense.
 *
 * A time carries from daily/weekly/once. A webhook prompt keeps its text and
 * the new webhook path is created by the daemon on save.
 */
export function withKind(draft: ScheduleDraft, kind: ScheduleKind): ScheduleDraft {
  if (draft.schedule.kind === kind) return draft;
  const time =
    draft.schedule.kind === "once" ||
    draft.schedule.kind === "daily" ||
    draft.schedule.kind === "weekly"
      ? draft.schedule.time
      : "09:00";
  const schedule: ScheduleSpec =
    kind === "once"
      ? { kind: "once", date: today(), time }
      : kind === "every"
        ? { kind: "every", count: 1, unit: "hours" }
        : kind === "daily"
          ? { kind: "daily", time }
          : kind === "weekly"
            ? { kind: "weekly", days: [1, 2, 3, 4, 5], time }
            : kind === "cron"
              ? { kind: "cron", expression: "0 9 * * *" }
              : { kind: "webhook" };
  return { ...draft, schedule };
}

/** Today's local date, for a new once schedule. */
export function today(now = new Date()): string {
  const year = now.getFullYear();
  const month = String(now.getMonth() + 1).padStart(2, "0");
  const day = String(now.getDate()).padStart(2, "0");
  return `${year}-${month}-${day}`;
}

/** The browser's IANA zone, or UTC when it cannot name one. */
export function localTimezone(): string {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
  } catch {
    return "UTC";
  }
}

/** Whether a string names an IANA zone the browser knows. */
export function knownTimezone(name: string): boolean {
  if (!name.trim()) return false;
  try {
    new Intl.DateTimeFormat(undefined, { timeZone: name.trim() });
    return true;
  } catch {
    return false;
  }
}

const TIME = /^([01]\d|2[0-3]):[0-5]\d$/;
const DATE = /^\d{4}-\d{2}-\d{2}$/;

/** Whether a `YYYY-MM-DD` string is a real calendar date. */
export function realDate(value: string): boolean {
  if (!DATE.test(value)) return false;
  const parsed = new Date(`${value}T00:00:00.000Z`);
  return !Number.isNaN(parsed.getTime()) && parsed.toISOString().slice(0, 10) === value;
}

/** Client-side validation; the daemon validates again before saving. */
export function validateDraft(draft: ScheduleDraft): ScheduleFormErrors {
  const errors: ScheduleFormErrors = {};
  const t = i18n.getFixedT(null, "settings", "schedules.validate");
  if (!draft.name.trim()) errors.name = t("nameRequired");
  else if (draft.name.trim().length > 120) errors.name = t("nameTooLong");
  if (!draft.workspace_path.trim()) errors.workspace = t("workspaceRequired");
  else if (!isAbsolute(draft.workspace_path)) {
    errors.workspace = t("workspaceAbsolute");
  }
  if (!draft.model.trim()) errors.model = t("modelRequired");
  if (!draft.prompt.trim()) errors.prompt = t("promptRequired");
  if (!knownTimezone(draft.timezone)) errors.timezone = t("timezone");

  const schedule = draft.schedule;
  if (schedule.kind === "once") {
    if (!realDate(schedule.date) || !TIME.test(schedule.time)) {
      errors.schedule = t("dateTime");
    }
  } else if (schedule.kind === "every") {
    if (!Number.isInteger(schedule.count) || schedule.count < 1 || schedule.count > 10_080) {
      errors.schedule = t("intervalRange");
    }
  } else if (schedule.kind === "daily") {
    if (!TIME.test(schedule.time)) errors.schedule = t("time");
  } else if (schedule.kind === "weekly") {
    if (schedule.days.length === 0) errors.schedule = t("weekday");
    else if (!TIME.test(schedule.time)) errors.schedule = t("time");
  } else if (schedule.kind === "cron") {
    const problem = cronProblem(schedule.expression);
    if (problem) errors.schedule = problem;
  }
  return errors;
}

/** Whether a path is absolute on any supported platform. */
export function isAbsolute(path: string): boolean {
  const value = path.trim();
  return value.startsWith("/") || /^[A-Za-z]:[\\/]/.test(value) || value.startsWith("\\\\");
}

/**
 * A field-level check for the five-field cron form.
 *
 * The daemon owns the real parser (ranges, steps, day-of-month/day-of-week
 * either semantics and a next-run search). This only catches the obvious
 * mistakes early so a save is not sent for them.
 */
export function cronProblem(expression: string): string | null {
  const t = i18n.getFixedT(null, "settings", "schedules.cron");
  const fields = expression.trim().split(/\s+/).filter(Boolean);
  if (fields.length !== 5) {
    return t("fiveFields");
  }
  const bounds: Array<[CronField, number, number]> = [
    ["minute", 0, 59],
    ["hour", 0, 23],
    ["dayOfMonth", 1, 31],
    ["month", 1, 12],
    ["dayOfWeek", 0, 7],
  ];
  for (let index = 0; index < fields.length; index += 1) {
    const [fieldName, minimum, maximum] = bounds[index] as [CronField, number, number];
    const name = t(`fields.${fieldName}`);
    const field = fields[index] ?? "";
    for (const part of field.split(",")) {
      const [range = "", step] = part.split("/");
      if (part.includes("/") && (!step || !/^\d+$/.test(step) || Number(step) === 0)) {
        return t("step", { name, part });
      }
      if (range === "*") continue;
      const boundsPart = range?.split("-") ?? [];
      if (boundsPart.length > 2) return t("invalid", { name, part });
      for (const number of boundsPart) {
        if (!/^\d+$/.test(number)) return t("notNumeric", { name, part });
        const value = Number(number);
        if (value < minimum || value > maximum) {
          return t("range", { name, value, minimum, maximum });
        }
      }
      if (boundsPart.length === 2 && Number(boundsPart[0]) > Number(boundsPart[1])) {
        return t("backwards", { name, part });
      }
    }
  }
  return null;
}

/** One sentence for the list. */
export function describeSchedule(spec: ScheduleSpec): string {
  const t = i18n.getFixedT(null, "settings", "schedules.describe");
  switch (spec.kind) {
    case "once":
      return t("once", { date: spec.date, time: spec.time });
    case "every":
      return spec.unit === "minutes"
        ? t("everyMinutes", { count: spec.count })
        : t("everyHours", { count: spec.count });
    case "daily":
      return t("daily", { time: spec.time });
    case "weekly": {
      const names = spec.days
        .filter((day) => WEEKDAYS.includes(day))
        .map((day) => weekdayName(day, "short"));
      const days = new Intl.ListFormat(activeLocale(), {
        style: "narrow",
        type: "conjunction",
      }).format(names);
      return t("weekly", { days, time: spec.time });
    }
    case "cron":
      return t("cron", { expression: spec.expression });
    case "webhook":
      return t("webhook");
  }
}

/** The destination in the list's words. */
export function describeDestination(destination: ScheduleDestination): string {
  return DESTINATIONS.includes(destination)
    ? i18n.t(`settings:schedules.destinations.${destination}.label`)
    : destination;
}

/** A local, readable next-run line, or what the task is waiting for. */
export function nextRunText(task: ScheduleTask, now = new Date()): string {
  const t = i18n.getFixedT(null, "settings", "schedules.next");
  if (task.schedule.kind === "webhook") return t("webhook");
  if (!task.enabled) return t("paused");
  if (!task.next_run_at) return t("none");
  const at = new Date(task.next_run_at);
  if (Number.isNaN(at.getTime())) return "";
  const when = formatDateTime(at);
  return at.getTime() <= now.getTime() ? t("due", { when }) : t("next", { when });
}

/** A short local date and time. */
export function formatDateTime(at: Date): string {
  try {
    return new Intl.DateTimeFormat(activeLocale(), {
      dateStyle: "medium",
      timeStyle: "short",
    }).format(at);
  } catch {
    return at.toISOString();
  }
}

/** The outcome of the last run in the list, with its detail. */
export function describeRun(run: ScheduleRun | null): string {
  const t = i18n.getFixedT(null, "settings", "schedules.run");
  if (!run) return t("never");
  const at = new Date(run.started_at);
  const when = Number.isNaN(at.getTime()) ? "" : formatDateTime(at);
  const detail = run.detail ? ` · ${run.detail}` : "";
  switch (run.outcome) {
    case "started":
      return t("ran", { when });
    case "skipped":
      return `${t("skipped", { when })}${detail}`;
    case "failed":
      return `${t("failed", { when })}${detail}`;
  }
}

/** The outcome in one word, for the history table. */
export function outcomeWord(run: ScheduleRun): string {
  return i18n.t(`settings:schedules.outcome.${run.outcome}`);
}

/** The interval unit in the editor's words. */
export function unitLabel(unit: IntervalUnit): string {
  return i18n.t(`settings:schedules.units.${unit}`);
}

/** Whether the task's prompt can use webhook placeholders. */
export function isWebhook(spec: ScheduleSpec): boolean {
  return spec.kind === "webhook";
}
