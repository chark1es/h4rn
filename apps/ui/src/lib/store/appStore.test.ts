import { beforeEach, describe, expect, it, vi } from "vitest";
import type { HostSummary, ThreadSummary } from "@/gen";

const fake = vi.hoisted(() => {
  const response: { current: unknown } = { current: null };
  const request = vi.fn(async (_request: unknown) => response.current);
  return { response, request, client: { request } };
});

vi.mock("@/lib/client/config", () => ({
  createClientFromEnv: () => fake.client,
  readBootConfig: () => ({}),
}));

import { onManualPlacement } from "@/features/notifications/placement";
import { drafts, flushDrafts, readThreadDraft } from "@/lib/drafts";
import { getState, loadThreads, setThreadTab, store } from "./appStore";

function hostSummary(hostId: string): HostSummary {
  return {
    host_id: hostId,
    name: hostId,
    platform: "linux",
    online: true,
    last_seen: "2026-01-01T00:00:00.000Z",
    harnesses: ["claude_code"],
    latency_ms: null,
    path: null,
  };
}

function threadSummary(input: { threadId: string; host: string }): ThreadSummary {
  return {
    thread_id: input.threadId,
    parent_thread_id: null,
    host: input.host,
    harness: "claude_code",
    title: input.threadId,
    workspace_path: "~/code",
    model: "claude-sonnet-4-5",
    tab: "working",
    placed_by_user: false,
    pinned: false,
    snoozed_until: null,
    last_activity: "2026-01-01T00:00:00.000Z",
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
    ttft_ms: null,
    context_window: null,
    context_fill_percent: null,
    needs_attention: false,
    unread: false,
    resume_at: null,
    limit: null,
  };
}

function seedHosts(hostIds: string[]): void {
  const state = getState();
  store.setState({
    ...state,
    hosts: Object.fromEntries(hostIds.map((hostId) => [hostId, hostSummary(hostId)])),
    threads: {},
  });
}

describe("loadThreads", () => {
  beforeEach(() => {
    fake.response.current = null;
    fake.request.mockClear();
    seedHosts([]);
  });

  it("lists the daemon's root threads and asks for no children", async () => {
    seedHosts(["load_host_a"]);
    fake.response.current = {
      type: "threads",
      threads: [threadSummary({ threadId: "load_thread", host: "load_host_a" })],
    };

    await loadThreads();

    const request = fake.request.mock.calls[0]?.[0] as
      | { type: string; filter: { include_children: boolean } }
      | undefined;
    expect(request?.type).toBe("list_threads");
    expect(request?.filter.include_children).toBe(false);
    expect(Object.keys(getState().threads)).toEqual(["load_thread"]);
  });

  it("never deletes drafts while loading the thread list", async () => {
    seedHosts(["load_host_b"]);
    drafts.writeThreadDraft("load_host_b", "not_in_the_list", "still unsent");
    drafts.writeThreadDraft("load_host_other", "another_machine", "keep for later");
    flushDrafts();
    fake.response.current = { type: "threads", threads: [] };

    await loadThreads();

    expect(readThreadDraft("load_host_b", "not_in_the_list")).toBe("still unsent");
    expect(readThreadDraft("load_host_other", "another_machine")).toBe("keep for later");
  });
});

describe("manual placements", () => {
  it("notes a placement after the daemon accepts the move", async () => {
    fake.response.current = { type: "ack" };
    const seen: string[] = [];
    const unsubscribe = onManualPlacement((threadId) => seen.push(threadId));

    await setThreadTab("thread_placed", "done");
    unsubscribe();

    expect(seen).toEqual(["thread_placed"]);
  });

  it("leaves no placement note when the move fails", async () => {
    fake.response.current = null;
    fake.request.mockRejectedValueOnce(new Error("the host is unreachable"));
    const seen: string[] = [];
    const unsubscribe = onManualPlacement((threadId) => seen.push(threadId));

    await setThreadTab("thread_failed", "done");
    unsubscribe();

    expect(seen).toEqual([]);
  });
});
