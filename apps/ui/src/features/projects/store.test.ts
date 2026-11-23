import { describe, expect, it } from "vitest";

import type { ThreadSummary } from "@/gen";

import { sidebarProjects } from "./store";

function thread(
  overrides: Partial<ThreadSummary> & Pick<ThreadSummary, "thread_id" | "workspace_path">,
): ThreadSummary {
  const base = {
    thread_id: overrides.thread_id,
    parent_thread_id: null,
    host: "host_local",
    harness: "claude_code",
    title: overrides.thread_id,
    workspace_path: overrides.workspace_path,
    model: "claude-sonnet-5",
    runtime_mode: "approval_required",
    reasoning_efforts: [],
    models: [],
    fast: false,
    tab: "done",
    placed_by_user: false,
    pinned: false,
    snoozed_until: null,
    last_activity: "2026-11-20T10:00:00Z",
    agent_state: "done",
    usage: {
      input_tokens: 0,
      output_tokens: 0,
      cached_input_tokens: 0,
      cost_micros: 0,
      duration_ms: 0,
      generation_ms: 0,
      tps: 0,
    },
    needs_attention: false,
    unread: false,
    ttft_ms: null,
    context_window: null,
    context_fill_percent: null,
    resume_at: null,
    limit: null,
  } as ThreadSummary;
  return { ...base, ...overrides } as ThreadSummary;
}

describe("sidebar projects", () => {
  it("puts a thread imported from a linked worktree under the main checkout", () => {
    const main = thread({ thread_id: "th_main", workspace_path: "/code/api" });
    const worktree = thread({
      thread_id: "th_wt",
      workspace_path: "/code/api-wt",
      project_path: "/code/api",
    });

    const groups = sidebarProjects({
      visible: [main, worktree],
      all: [main, worktree],
      projects: [],
      prefs: { projectSort: "activity", threadSort: "activity", order: [] },
    });

    expect(groups).toHaveLength(1);
    expect(groups[0]?.key).toBe("host_local\u0000/code/api");
    expect(groups[0]?.name).toBe("api");
    expect(groups[0]?.threads.map((row) => row.thread_id)).toEqual(["th_main", "th_wt"]);
  });

  it("keeps an ordinary thread in its own working folder", () => {
    const one = thread({ thread_id: "th_one", workspace_path: "/code/api" });
    const two = thread({ thread_id: "th_two", workspace_path: "/code/docs" });

    const groups = sidebarProjects({
      visible: [one, two],
      all: [one, two],
      projects: [],
      prefs: { projectSort: "activity", threadSort: "activity", order: [] },
    });

    expect(groups.map((group) => group.name).sort()).toEqual(["api", "docs"]);
  });
});
