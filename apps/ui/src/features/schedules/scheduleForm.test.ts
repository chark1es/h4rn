import { describe, expect, it } from "vitest";
import type { ScheduleTask } from "@/gen";
import {
  cronProblem,
  describeDestination,
  describeSchedule,
  draftOf,
  emptyDraft,
  nextRunText,
  validateDraft,
  withKind,
} from "./scheduleForm";

const task: ScheduleTask = {
  task_id: "sch_1",
  name: "Nightly review",
  host: "host_1",
  workspace_path: "/home/atlas/code/api",
  harness: "claude_code",
  agent_id: null,
  model: "claude-sonnet",
  runtime_mode: "approval_required",
  prompt: "Review the diff.",
  schedule: { kind: "daily", time: "09:00" },
  timezone: "Europe/Berlin",
  destination: "new_thread",
  enabled: true,
  thread_id: null,
  webhook_path: null,
  next_run_at: "2026-11-24T08:00:00.000Z",
  last_run: null,
  created_at: "2026-11-20T10:00:00.000Z",
  updated_at: "2026-11-20T10:00:00.000Z",
};

describe("scheduled task form", () => {
  it("requires a name, workspace, model, prompt and a real time zone", () => {
    const errors = validateDraft(emptyDraft("host_1", "Not/AZone"));
    expect(errors.name).toBeTruthy();
    expect(errors.workspace).toBeTruthy();
    expect(errors.model).toBeTruthy();
    expect(errors.prompt).toBeTruthy();
    expect(errors.timezone).toBeTruthy();
  });

  it("accepts a filled draft and rejects a past-like once shape", () => {
    const draft = {
      ...emptyDraft("host_1", "Europe/Berlin"),
      workspace_path: "/home/atlas/code/api",
      model: "claude-sonnet",
      prompt: "Review the diff.",
      name: "Nightly review",
    };
    expect(validateDraft(draft)).toEqual({});

    const once = {
      ...draft,
      schedule: { kind: "once" as const, date: "2026-13-01", time: "09:00" },
    };
    expect(validateDraft(once).schedule).toBeTruthy();
  });

  it("checks cron fields before the daemon sees them", () => {
    expect(cronProblem("0 9 * * *")).toBeNull();
    expect(cronProblem("*/15 9-17 * * 1-5")).toBeNull();
    expect(cronProblem("0 9 * *")).toMatch(/five fields/);
    expect(cronProblem("60 9 * * *")).toMatch(/minute/);
    expect(cronProblem("0 9 * * 8")).toMatch(/day of week/);
    expect(cronProblem("*/0 9 * * *")).toMatch(/step/);
  });

  it("switches schedule kinds without losing the time", () => {
    const draft = draftOf(task);
    const weekly = withKind(draft, "weekly");
    expect(weekly.schedule).toEqual({ kind: "weekly", days: [1, 2, 3, 4, 5], time: "09:00" });
    const once = withKind(weekly, "once");
    expect(once.schedule).toMatchObject({ kind: "once", time: "09:00" });
  });

  it("describes the schedule and destination in plain words", () => {
    expect(describeSchedule({ kind: "every", count: 6, unit: "hours" })).toBe("Every 6 hours");
    expect(describeSchedule({ kind: "weekly", days: [1, 3], time: "08:00" })).toBe(
      "Weekly on Mon, Wed at 08:00",
    );
    expect(describeDestination("fresh_session")).toBe("One thread, fresh session");
  });

  it("shows the real next run and pauses a disabled task", () => {
    expect(nextRunText(task, new Date("2026-11-23T00:00:00Z"))).toContain("Next");
    expect(nextRunText({ ...task, enabled: false })).toBe("Paused");
    expect(
      nextRunText({ ...task, schedule: { kind: "webhook" }, webhook_path: "/webhooks/x" }),
    ).toBe("Runs when its URL is called");
  });
});
