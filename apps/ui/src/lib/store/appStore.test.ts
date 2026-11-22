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

import { drafts, flushDrafts, readThreadDraft } from "@/lib/drafts";
import { getState, loadThreads, store } from "./appStore";

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

function threadSummary(input: {
  threadId: string;
  host: string;
  parent?: string | null;
}): ThreadSummary {
  return {
    thread_id: input.threadId,
    parent_thread_id: input.parent ?? null,
    host: input.host,
    harness: "claude_code",
    title: input.threadId,
    workspace_path: "~/code",
    model: "claude-sonnet-4-5",
    tab: "working",
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

function threadsResponse(threads: ThreadSummary[]) {
  return { type: "threads", threads };
}

describe("loadThreads draft pruning", () => {
  beforeEach(() => {
    fake.response.current = null;
    fake.request.mockClear();
    seedHosts([]);
  });

  it("asks for the full inventory, children included, but lists only root threads", async () => {
    const parent = threadSummary({ threadId: "prune_parent", host: "prune_host_a" });
    const child = threadSummary({
      threadId: "prune_child",
      host: "prune_host_a",
      parent: "prune_parent",
    });
    seedHosts(["prune_host_a"]);
    fake.response.current = threadsResponse([parent, child]);

    await loadThreads();

    const request = fake.request.mock.calls[0]?.[0] as
      | { type: string; filter: { include_children: boolean } }
      | undefined;
    expect(request?.type).toBe("list_threads");
    expect(request?.filter.include_children).toBe(true);
    expect(Object.keys(getState().threads)).toEqual(["prune_parent"]);
  });

  it("keeps a subagent thread's draft across a restart", async () => {
    seedHosts(["prune_host_child"]);
    drafts.writeThreadDraft("prune_host_child", "prune_parent_thread", "root draft");
    drafts.writeThreadDraft("prune_host_child", "prune_child_thread", "child draft");
    flushDrafts();
    fake.response.current = threadsResponse([
      threadSummary({ threadId: "prune_parent_thread", host: "prune_host_child" }),
      threadSummary({
        threadId: "prune_child_thread",
        host: "prune_host_child",
        parent: "prune_parent_thread",
      }),
    ]);

    await loadThreads();

    expect(readThreadDraft("prune_host_child", "prune_parent_thread")).toBe("root draft");
    expect(readThreadDraft("prune_host_child", "prune_child_thread")).toBe("child draft");
  });

  it("leaves another machine's drafts alone when a different daemon loads its threads", async () => {
    seedHosts(["prune_machine_b"]);
    drafts.writeThreadDraft("prune_machine_a", "gone_on_a", "machine A draft");
    drafts.writeThreadDraft("prune_machine_b", "gone_on_b", "machine B draft");
    flushDrafts();
    fake.response.current = threadsResponse([]);

    await loadThreads();

    expect(readThreadDraft("prune_machine_a", "gone_on_a")).toBe("machine A draft");
    // The daemon did report machine B, so a thread of its that is gone loses
    // its draft.
    expect(readThreadDraft("prune_machine_b", "gone_on_b")).toBe("");
  });
});
