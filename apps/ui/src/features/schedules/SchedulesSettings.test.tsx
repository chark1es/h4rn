// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ResponseBody, ScheduleRun, ScheduleTask } from "@/gen";
import { getState, store } from "@/lib/store/appStore";

import { SchedulesSettings } from "./SchedulesSettings";
import { resetSchedulesStore, setSchedulesRequesterForTests } from "./store";

function task(patch: Partial<ScheduleTask> = {}): ScheduleTask {
  return {
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
    webhook_path_set: false,
    once_consumed: false,
    next_run_at: "2026-11-24T08:00:00.000Z",
    last_run: null,
    created_at: "2026-11-20T10:00:00.000Z",
    updated_at: "2026-11-20T10:00:00.000Z",
    ...patch,
  };
}

function run(patch: Partial<ScheduleRun> = {}): ScheduleRun {
  return {
    run_id: 1,
    task_id: "sch_1",
    scheduled_for: "2026-11-23T08:00:00.000Z",
    started_at: "2026-11-23T08:00:01.000Z",
    thread_id: "th_1",
    outcome: "started",
    detail: null,
    ...patch,
  };
}

function ok(response: ResponseBody): Promise<ResponseBody> {
  return Promise.resolve(response);
}

beforeEach(() => {
  resetSchedulesStore();
  store.setState({
    ...getState(),
    localHostId: "host_1",
    daemonTimezone: "Asia/Tokyo",
    connection: {
      status: "online",
      attempt: 0,
      transport: "direct",
      lastConnectedAt: Date.now(),
      lastEventAt: Date.now(),
      latencyMs: 1,
      error: null,
    },
  });
});

afterEach(() => {
  cleanup();
  setSchedulesRequesterForTests(null);
  vi.restoreAllMocks();
});

describe("scheduled tasks settings", () => {
  it("lists tasks with their schedule, next run and last outcome", async () => {
    setSchedulesRequesterForTests((request) =>
      request.type === "list_schedules"
        ? ok({ type: "schedules", tasks: [task()] })
        : ok({ type: "ack" }),
    );
    render(<SchedulesSettings />);

    expect(await screen.findByText("Nightly review")).toBeTruthy();
    expect(screen.getByText(/Daily at 09:00/)).toBeTruthy();
    expect(screen.getByText(/Next /)).toBeTruthy();
    expect(screen.getByText(/Never run/)).toBeTruthy();
  });

  it("runs a task now and shows the fresh outcome", async () => {
    const requests: string[] = [];
    setSchedulesRequesterForTests((request) => {
      requests.push(request.type);
      if (request.type === "list_schedules") {
        return ok({ type: "schedules", tasks: [task()] });
      }
      if (request.type === "run_schedule_now") {
        return ok({ type: "schedule_task", task: task({ last_run: run() }) });
      }
      return ok({ type: "ack" });
    });
    render(<SchedulesSettings />);

    await screen.findByText("Nightly review");
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Run Nightly review now" }));
    });

    expect(requests).toContain("run_schedule_now");
    expect(await screen.findByText(/Ran /)).toBeTruthy();
  });

  it("loads history and links to the thread a run started", async () => {
    setSchedulesRequesterForTests((request) => {
      if (request.type === "list_schedules") {
        return ok({ type: "schedules", tasks: [task({ last_run: run() })] });
      }
      if (request.type === "list_schedule_runs") {
        return ok({ type: "schedule_runs", runs: [run()] });
      }
      return ok({ type: "ack" });
    });
    render(<SchedulesSettings />);

    await screen.findByText("Nightly review");
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "History for Nightly review" }));
    });

    expect(await screen.findByText("History · Nightly review")).toBeTruthy();
    expect(screen.getByText("Started")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Open thread" })).toBeTruthy();
  });

  it("shows field errors instead of creating an empty task", async () => {
    const requests: string[] = [];
    setSchedulesRequesterForTests((request) => {
      requests.push(request.type);
      if (request.type === "list_schedules") {
        return ok({ type: "schedules", tasks: [] });
      }
      return ok({ type: "ack" });
    });
    render(<SchedulesSettings />);

    await screen.findByText(/No scheduled tasks yet/);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "New task" }));
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Save task" }));
    });

    expect(screen.getByText("Give the task a name.")).toBeTruthy();
    expect(screen.getByText("Write the prompt to send.")).toBeTruthy();
    expect(requests).not.toContain("create_schedule");
  });

  it("defaults a new task to the daemon's time zone, not this device's", async () => {
    setSchedulesRequesterForTests((request) =>
      request.type === "list_schedules"
        ? ok({ type: "schedules", tasks: [] })
        : ok({ type: "ack" }),
    );
    render(<SchedulesSettings />);

    await screen.findByText(/No scheduled tasks yet/);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "New task" }));
    });
    const zone = screen.getByLabelText("Time zone") as HTMLInputElement;
    expect(zone.value).toBe("Asia/Tokyo");
  });

  it("asks for a webhook path instead of reading it from the task", async () => {
    const requests: string[] = [];
    setSchedulesRequesterForTests((request) => {
      requests.push(request.type);
      if (request.type === "list_schedules") {
        return ok({
          type: "schedules",
          tasks: [
            task({
              schedule: { kind: "webhook" },
              webhook_path_set: true,
              next_run_at: null,
            }),
          ],
        });
      }
      if (request.type === "get_schedule_webhook_url") {
        return ok({
          type: "schedule_webhook_url",
          task_id: "sch_1",
          path: "/webhooks/abc",
        });
      }
      return ok({ type: "ack" });
    });
    render(<SchedulesSettings />);

    await screen.findByText("Nightly review");
    expect(screen.queryByText("/webhooks/abc")).toBeNull();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Edit Nightly review" }));
    });

    expect(await screen.findByText("/webhooks/abc")).toBeTruthy();
    expect(requests).toContain("get_schedule_webhook_url");
  });

  it("pauses a task and asks for history only when opened", async () => {
    const requests: string[] = [];
    setSchedulesRequesterForTests((request) => {
      requests.push(request.type);
      if (request.type === "list_schedules") {
        return ok({ type: "schedules", tasks: [task()] });
      }
      if (request.type === "set_schedule_enabled") {
        return ok({ type: "schedule_task", task: task({ enabled: false }) });
      }
      return ok({ type: "ack" });
    });
    render(<SchedulesSettings />);

    await screen.findByText("Nightly review");
    expect(requests).not.toContain("list_schedule_runs");
    await act(async () => {
      fireEvent.click(screen.getByRole("switch", { name: "Pause Nightly review" }));
    });
    expect(requests).toContain("set_schedule_enabled");
    expect(await screen.findByText(/Paused/)).toBeTruthy();
  });
});
