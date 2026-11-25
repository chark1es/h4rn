import { describe, expect, it } from "vitest";

import type { ThreadSummary } from "@/gen";
import {
  compareThreads,
  isSnoozed,
  matchesFilter,
  matchesQuery,
  needsAttention,
  snoozedThreads,
  tabCounts,
  threadsForTab,
} from "./inbox";
import type { AppState, ThreadFilterState } from "./store/appStore";

const NOW = Date.parse("2026-01-01T12:00:00.000Z");

const FILTER: ThreadFilterState = {
  host: null,
  harness: null,
  needsAttention: false,
  unread: false,
};

function thread(overrides: Partial<ThreadSummary> = {}): ThreadSummary {
  return {
    thread_id: "t1",
    parent_thread_id: null,
    host: "host_a",
    harness: "claude_code",
    title: "Refactor the daemon",
    workspace_path: "/work/h4rn",
    model: "opus",
    runtime_mode: "approval_required",
    reasoning_efforts: [],
    models: [],
    fast: false,
    tab: "working",
    placed_by_user: false,
    pinned: false,
    snoozed_until: null,
    last_activity: "2026-01-01T10:00:00.000Z",
    agent_state: "idle",
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
    pending_guest_requests: 0,
    unread: false,
    resume_at: null,
    rewind: "handoff",
    file_rewind: false,
    limit: null,
    ttft_ms: null,
    context_window: null,
    context_fill_percent: null,
    queued_count: 0,
    queue_revision: 0,
    steer: true,
    ...overrides,
  };
}

function state(threads: ThreadSummary[], overrides: Partial<AppState> = {}): AppState {
  return {
    threads: Object.fromEntries(threads.map((item) => [item.thread_id, item])),
    hosts: {
      host_a: {
        host_id: "host_a",
        name: "Build box",
        platform: "linux",
        online: true,
        harnesses: ["claude_code"],
      },
    },
    ui: { query: "", filter: FILTER },
    ...overrides,
  } as unknown as AppState;
}

describe("needsAttention", () => {
  it("treats approvals, errors and unpinned rate limits as attention", () => {
    expect(needsAttention(thread({ agent_state: "awaiting_approval" }), NOW)).toBe(true);
    expect(needsAttention(thread({ agent_state: "error" }), NOW)).toBe(true);
    expect(needsAttention(thread({ agent_state: "rate_limited" }), NOW)).toBe(true);
    expect(needsAttention(thread({ agent_state: "rate_limited", pinned: true }), NOW)).toBe(false);
  });

  it("does not raise attention for a snoozed thread", () => {
    expect(
      needsAttention(
        thread({ needs_attention: true, snoozed_until: "2026-01-01T13:00:00.000Z" }),
        NOW,
      ),
    ).toBe(false);
  });
});

describe("isSnoozed", () => {
  it("is true only while the wake time is in the future", () => {
    expect(isSnoozed(thread({ snoozed_until: "2026-01-01T13:00:00.000Z" }), NOW)).toBe(true);
    expect(isSnoozed(thread({ snoozed_until: "2026-01-01T11:00:00.000Z" }), NOW)).toBe(false);
    expect(isSnoozed(thread({ snoozed_until: "not a date" }), NOW)).toBe(false);
  });
});

describe("compareThreads", () => {
  it("orders attention, then pinned, then recency", () => {
    const attention = thread({ thread_id: "a", agent_state: "error", pinned: false });
    const pinned = thread({ thread_id: "b", pinned: true });
    const recent = thread({ thread_id: "c", last_activity: "2026-01-01T11:00:00.000Z" });

    const sorted = [recent, pinned, attention].sort((a, b) => compareThreads(a, b, NOW));
    expect(sorted.map((item) => item.thread_id)).toEqual(["a", "b", "c"]);
  });
});

describe("threadsForTab", () => {
  it("returns only that tab, excluding snoozed threads", () => {
    const app = state([
      thread({ thread_id: "a", tab: "working" }),
      thread({ thread_id: "b", tab: "working", snoozed_until: "2026-01-01T13:00:00.000Z" }),
      thread({ thread_id: "c", tab: "done" }),
    ]);

    expect(threadsForTab(app, "working", NOW).map((item) => item.thread_id)).toEqual(["a"]);
    expect(threadsForTab(app, "done", NOW).map((item) => item.thread_id)).toEqual(["c"]);
  });

  it("applies the search query across title, host and model", () => {
    const app = state([
      thread({ thread_id: "a", title: "Fix the parser" }),
      thread({ thread_id: "b", title: "Unrelated", model: "parser-v2" }),
      thread({ thread_id: "c", title: "Nothing here" }),
    ]);
    app.ui.query = "parser";

    expect(threadsForTab(app, "working", NOW).map((item) => item.thread_id)).toEqual(["a", "b"]);
  });
});

describe("matchesFilter", () => {
  it("filters by host, harness, attention and unread", () => {
    expect(matchesFilter(thread(), { ...FILTER, host: "host_b" }, NOW, "Build box")).toBe(false);
    expect(matchesFilter(thread(), { ...FILTER, host: "host_a" }, NOW, "Build box")).toBe(true);
    expect(matchesFilter(thread(), { ...FILTER, harness: "codex" }, NOW, "Build box")).toBe(false);
    expect(
      matchesFilter(
        thread({ needs_attention: true }),
        { ...FILTER, needsAttention: true },
        NOW,
        "B",
      ),
    ).toBe(true);
    expect(matchesFilter(thread({ unread: true }), { ...FILTER, unread: true }, NOW, "B")).toBe(
      true,
    );
  });
});

describe("matchesQuery", () => {
  it("matches harness labels as well as fields", () => {
    expect(matchesQuery(thread(), "claude")).toBe(true);
    expect(matchesQuery(thread(), "nothing")).toBe(false);
    expect(matchesQuery(thread(), "   ")).toBe(true);
  });
});

describe("snoozedThreads", () => {
  it("returns snoozed threads ordered by wake time", () => {
    const app = state([
      thread({ thread_id: "a", snoozed_until: "2026-01-01T15:00:00.000Z" }),
      thread({ thread_id: "b", snoozed_until: "2026-01-01T13:00:00.000Z" }),
      thread({ thread_id: "c" }),
    ]);

    expect(snoozedThreads(app, NOW).map((item) => item.thread_id)).toEqual(["b", "a"]);
  });
});

describe("tabCounts", () => {
  it("counts each tab without snoozed threads", () => {
    const app = state([
      thread({ thread_id: "a", tab: "working" }),
      thread({ thread_id: "b", tab: "working", snoozed_until: "2026-01-01T13:00:00.000Z" }),
      thread({ thread_id: "c", tab: "done" }),
      thread({ thread_id: "d", tab: "settled" }),
    ]);

    expect(tabCounts(app, NOW)).toEqual({ working: 1, done: 1, settled: 1 });
  });
});
