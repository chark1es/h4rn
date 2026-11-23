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

export type ScheduleKind = ScheduleSpec["kind"];
export type ScheduleField = "name" | "workspace" | "model" | "prompt" | "timezone" | "schedule";
export type ScheduleFormErrors = Partial<Record<ScheduleField, string>>;

/** The schedule kinds in the order the editor offers them. */
export const SCHEDULE_KINDS: Array<{ kind: ScheduleKind; label: string }> = [
  { kind: "once", label: "Once" },
  { kind: "every", label: "Every…" },
  { kind: "daily", label: "Daily" },
  { kind: "weekly", label: "Weekly" },
  { kind: "cron", label: "Cron" },
  { kind: "webhook", label: "On webhook" },
];

/** Where a run goes, in the editor's words. */
export const DESTINATIONS: Array<{ value: ScheduleDestination; label: string; hint: string }> = [
  {
    value: "new_thread",
    label: "New thread each run",
    hint: "Every run is a separate thread.",
  },
  {
    value: "continue_thread",
    label: "One thread, continuing",
    hint: "Later runs post into the same conversation.",
  },
  {
    value: "fresh_session",
    label: "One thread, fresh session",
    hint: "The thread stays readable; each run starts without the old context.",
  },
];

/** Weekdays as the editor numbers them (Monday = 1). */
export const WEEKDAYS: Array<{ day: number; short: string; long: string }> = [
  { day: 1, short: "Mon", long: "Monday" },
  { day: 2, short: "Tue", long: "Tuesday" },
  { day: 3, short: "Wed", long: "Wednesday" },
  { day: 4, short: "Thu", long: "Thursday" },
  { day: 5, short: "Fri", long: "Friday" },
  { day: 6, short: "Sat", long: "Saturday" },
  { day: 7, short: "Sun", long: "Sunday" },
];

/** The webhook placeholders, with one example each. */
export const WEBHOOK_PLACEHOLDERS = [
  { token: "{{body.path}}", hint: "A JSON or form field, such as {{body.pull_request.html_url}}" },
  { token: "{{headers.name}}", hint: "A request header, such as {{headers.x-github-event}}" },
  { token: "{{query.name}}", hint: "A query value, such as {{query.ref}}" },
  { token: "{{body}}", hint: "The raw body" },
  { token: "{{request}}", hint: "The method, path, query, headers and body" },
];

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
  if (!draft.name.trim()) errors.name = "Give the task a name.";
  else if (draft.name.trim().length > 120) errors.name = "Keep the name under 120 characters.";
  if (!draft.workspace_path.trim()) errors.workspace = "Choose a workspace.";
  else if (!isAbsolute(draft.workspace_path)) {
    errors.workspace = "Use an absolute path on this host.";
  }
  if (!draft.model.trim()) errors.model = "Choose a model.";
  if (!draft.prompt.trim()) errors.prompt = "Write the prompt to send.";
  if (!knownTimezone(draft.timezone)) errors.timezone = "Choose a known IANA time zone.";

  const schedule = draft.schedule;
  if (schedule.kind === "once") {
    if (!realDate(schedule.date) || !TIME.test(schedule.time)) {
      errors.schedule = "Choose a real date and time.";
    }
  } else if (schedule.kind === "every") {
    if (!Number.isInteger(schedule.count) || schedule.count < 1 || schedule.count > 10_080) {
      errors.schedule = "Choose between 1 and 10080 units.";
    }
  } else if (schedule.kind === "daily") {
    if (!TIME.test(schedule.time)) errors.schedule = "Choose a real time of day.";
  } else if (schedule.kind === "weekly") {
    if (schedule.days.length === 0) errors.schedule = "Choose at least one weekday.";
    else if (!TIME.test(schedule.time)) errors.schedule = "Choose a real time of day.";
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
  const fields = expression.trim().split(/\s+/).filter(Boolean);
  if (fields.length !== 5) {
    return "Use five fields: minute hour day-of-month month day-of-week.";
  }
  const bounds: Array<[string, number, number]> = [
    ["minute", 0, 59],
    ["hour", 0, 23],
    ["day of month", 1, 31],
    ["month", 1, 12],
    ["day of week", 0, 7],
  ];
  for (let index = 0; index < fields.length; index += 1) {
    const [name, minimum, maximum] = bounds[index] as [string, number, number];
    const field = fields[index] ?? "";
    for (const part of field.split(",")) {
      const [range = "", step] = part.split("/");
      if (part.includes("/") && (!step || !/^\d+$/.test(step) || Number(step) === 0)) {
        return `The ${name} step in \`${part}\` is not a positive number.`;
      }
      if (range === "*") continue;
      const boundsPart = range?.split("-") ?? [];
      if (boundsPart.length > 2) return `The ${name} field \`${part}\` is not valid.`;
      for (const number of boundsPart) {
        if (!/^\d+$/.test(number)) return `The ${name} field \`${part}\` is not numeric.`;
        const value = Number(number);
        if (value < minimum || value > maximum) {
          return `The ${name} ${value} must be between ${minimum} and ${maximum}.`;
        }
      }
      if (boundsPart.length === 2 && Number(boundsPart[0]) > Number(boundsPart[1])) {
        return `The ${name} range \`${part}\` runs backwards.`;
      }
    }
  }
  return null;
}

/** One sentence for the list. */
export function describeSchedule(spec: ScheduleSpec): string {
  switch (spec.kind) {
    case "once":
      return `Once on ${spec.date} at ${spec.time}`;
    case "every":
      return `Every ${spec.count} ${spec.unit}`;
    case "daily":
      return `Daily at ${spec.time}`;
    case "weekly": {
      const names = spec.days
        .map((day) => WEEKDAYS.find((entry) => entry.day === day)?.short)
        .filter(Boolean)
        .join(", ");
      return `Weekly on ${names} at ${spec.time}`;
    }
    case "cron":
      return `Cron ${spec.expression}`;
    case "webhook":
      return "On webhook";
  }
}

/** The destination in the list's words. */
export function describeDestination(destination: ScheduleDestination): string {
  return DESTINATIONS.find((entry) => entry.value === destination)?.label ?? destination;
}

/** A local, readable next-run line, or what the task is waiting for. */
export function nextRunText(task: ScheduleTask, now = new Date()): string {
  if (task.schedule.kind === "webhook") return "Runs when its URL is called";
  if (!task.enabled) return "Paused";
  if (!task.next_run_at) return "No further run";
  const at = new Date(task.next_run_at);
  if (Number.isNaN(at.getTime())) return "";
  const prefix = at.getTime() <= now.getTime() ? "Due now" : "Next";
  return `${prefix} ${formatDateTime(at)}`;
}

/** A short local date and time. */
export function formatDateTime(at: Date): string {
  try {
    return new Intl.DateTimeFormat(undefined, {
      dateStyle: "medium",
      timeStyle: "short",
    }).format(at);
  } catch {
    return at.toISOString();
  }
}

/** The outcome of the last run in the list, with its detail. */
export function describeRun(run: ScheduleRun | null): string {
  if (!run) return "Never run";
  const at = new Date(run.started_at);
  const when = Number.isNaN(at.getTime()) ? "" : formatDateTime(at);
  switch (run.outcome) {
    case "started":
      return `Ran ${when}`;
    case "skipped":
      return `Skipped ${when}${run.detail ? ` · ${run.detail}` : ""}`;
    case "failed":
      return `Failed ${when}${run.detail ? ` · ${run.detail}` : ""}`;
  }
}

/** The outcome in one word, for the history table. */
export function outcomeWord(run: ScheduleRun): string {
  switch (run.outcome) {
    case "started":
      return "Started";
    case "skipped":
      return "Skipped";
    case "failed":
      return "Failed";
  }
}

/** The interval unit in the editor's words. */
export function unitLabel(unit: IntervalUnit): string {
  return unit === "minutes" ? "minutes" : "hours";
}

/** Whether the task's prompt can use webhook placeholders. */
export function isWebhook(spec: ScheduleSpec): boolean {
  return spec.kind === "webhook";
}
