import { beforeAll, describe, expect, it, vi } from "vitest";

import type { SubagentView } from "@/gen";

const harness = vi.hoisted(() => ({
  listeners: [] as Array<(payload: { event: unknown }) => void>,
  request: vi.fn(),
  toast: vi.fn(),
}));

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => ({ request: harness.request }),
  onAppEvent: (listener: (payload: { event: unknown }) => void) => {
    harness.listeners.push(listener);
    return () => undefined;
  },
  pushToast: harness.toast,
}));

import {
  cancelSubagent,
  getSubagentEntry,
  initializeSubagents,
  loadSubagents,
  spawnSubagent,
  subagentStore,
} from "./subagentStore";

function view(overrides: Partial<SubagentView> = {}): SubagentView {
  return {
    subagent_id: "sa_1",
    parent_thread_id: "thr_parent",
    child_thread_id: "thr_child",
    task: "Trace the flaky retry timer",
    harness: "codex",
    host: "atlas",
    model: "gpt-5.1-codex",
    isolation: "shared",
    workspace_path: "/home/you/code/h4rn",
    branch: null,
    depth: 1,
    state: "running",
    agent_state: "running_tool",
    progress: {
      turns_started: 2,
      turns_completed: 1,
      tokens: 2_400,
      elapsed_ms: 30_000,
      current: "Reading retry.test.ts",
    },
    budget: { max_turns: 20, max_wall_time_secs: 900, max_tokens: 50_000 },
    context: { kind: "none" },
    credential: { label: "CLIProxyAPI: work", shared_with_parent: true },
    created_at: new Date().toISOString(),
    started_at: new Date().toISOString(),
    finished_at: null,
    result: null,
    ...overrides,
  };
}

beforeAll(() => {
  initializeSubagents();
});

describe("live subagent store", () => {
  it("subscribes to the event feed once", () => {
    initializeSubagents();
    expect(harness.listeners).toHaveLength(1);
  });

  it("list_subagents fills the tree and clears loading", async () => {
    harness.request.mockResolvedValueOnce({
      type: "subagents",
      subagents: [view({ subagent_id: "sa_load" })],
    });

    const pending = loadSubagents("thr_load");
    expect(getSubagentEntry("thr_load").loading).toBe(true);
    await pending;

    const entry = getSubagentEntry("thr_load");
    expect(entry.loading).toBe(false);
    expect(entry.loaded).toBe(true);
    expect(entry.error).toBeNull();
    expect(entry.views.sa_load?.progress.tokens).toBe(2_400);
    expect(harness.request).toHaveBeenLastCalledWith({
      type: "subagent",
      request: {
        type: "list_subagents",
        parent_thread: "thr_load",
        include_finished: true,
      },
    });
  });

  it("surfaces a load failure without dropping the cached tree", async () => {
    harness.request.mockRejectedValueOnce(new Error("daemon went away"));
    await loadSubagents("thr_fail");
    expect(getSubagentEntry("thr_fail").error).toBe("daemon went away");

    harness.request.mockResolvedValueOnce({
      type: "subagents",
      subagents: [view({ subagent_id: "sa_cached" })],
    });
    await loadSubagents("thr_fail", true);
    harness.request.mockRejectedValueOnce(new Error("still down"));
    await loadSubagents("thr_fail", true);
    expect(getSubagentEntry("thr_fail").views.sa_cached).toBeDefined();
    expect(getSubagentEntry("thr_fail").error).toBe("still down");
  });

  it("patches views from spawned and finished events", () => {
    const listener = harness.listeners[0];
    expect(listener).toBeDefined();

    listener?.({
      event: {
        type: "subagent_spawned",
        parent_thread_id: "thr_events",
        subagent: view({ subagent_id: "sa_spawned", state: "starting" }),
      },
    });
    expect(getSubagentEntry("thr_events").views.sa_spawned?.state).toBe("starting");

    listener?.({
      event: {
        type: "subagent_finished",
        parent_thread_id: "thr_events",
        subagent: view({ subagent_id: "sa_spawned", state: "completed" }),
      },
    });
    expect(getSubagentEntry("thr_events").views.sa_spawned?.state).toBe("completed");
  });

  it("ignores unrelated events", () => {
    const listener = harness.listeners[0];
    listener?.({
      event: { type: "turn_started", thread_id: "thr_events", turn_id: "turn_1" },
    });
    expect(getSubagentEntry("thr_events").views).toHaveProperty("sa_spawned");
  });

  it("spawn_subagent sends the daemon defaults and selects the child", async () => {
    harness.request.mockResolvedValueOnce({
      type: "subagent",
      subagent: view({ subagent_id: "sa_new", parent_thread_id: "thr_spawn", state: "starting" }),
    });

    await expect(
      spawnSubagent({
        parentThread: "thr_spawn",
        task: "  Audit the session store  ",
        harness: "claude_code",
        host: "atlas",
        model: "  ",
        isolation: "worktree",
        maxTurns: 20,
        maxWallTimeSecs: 900,
        maxTokens: null,
        contextSummary: " Parent found a double write. ",
      }),
    ).resolves.toBe(true);

    expect(harness.request).toHaveBeenLastCalledWith({
      type: "subagent",
      request: {
        type: "spawn_subagent",
        request: {
          parent_thread: "thr_spawn",
          task: "Audit the session store",
          harness: "claude_code",
          host: "atlas",
          model: null,
          isolation: "worktree",
          context: { kind: "summary", summary: "Parent found a double write." },
          budget: { max_turns: 20, max_wall_time_secs: 900, max_tokens: null },
        },
      },
    });
    expect(getSubagentEntry("thr_spawn").views.sa_new).toBeDefined();
    expect(subagentStore.getState().selected).toBe("sa_new");
    expect(harness.toast).toHaveBeenLastCalledWith(expect.objectContaining({ variant: "success" }));
  });

  it("refuses to spawn an empty task", async () => {
    harness.request.mockClear();
    await expect(
      spawnSubagent({
        parentThread: "thr_empty",
        task: "   ",
        harness: null,
        host: null,
        model: null,
        isolation: "shared",
        maxTurns: null,
        maxWallTimeSecs: null,
        maxTokens: null,
        contextSummary: null,
      }),
    ).resolves.toBe(false);
    expect(harness.request).not.toHaveBeenCalled();
  });

  it("cancel_subagent updates the row from the daemon answer", async () => {
    harness.request.mockResolvedValueOnce({
      type: "subagent",
      subagent: view({
        subagent_id: "sa_cancel",
        parent_thread_id: "thr_cancel",
        state: "cancelled",
        finished_at: new Date().toISOString(),
      }),
    });
    await cancelSubagent(view({ subagent_id: "sa_cancel", parent_thread_id: "thr_cancel" }));
    expect(getSubagentEntry("thr_cancel").views.sa_cancel?.state).toBe("cancelled");
    expect(subagentStore.getState().cancelling).toBeNull();
  });

  it("keeps a cancel failure visible and clears the busy flag", async () => {
    harness.request.mockRejectedValueOnce(new Error("not cancellable"));
    await cancelSubagent(view({ subagent_id: "sa_stuck", parent_thread_id: "thr_stuck" }));
    expect(subagentStore.getState().error).toBe("not cancellable");
    expect(subagentStore.getState().cancelling).toBeNull();
    expect(harness.toast).toHaveBeenLastCalledWith(expect.objectContaining({ variant: "error" }));
  });
});
