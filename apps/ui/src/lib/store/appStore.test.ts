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
import { createThread, getState, loadThreads, store } from "./appStore";

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

describe("createThread draft handoff", () => {
  beforeEach(() => {
    fake.request.mockClear();
    fake.response.current = null;
    seedHosts(["handoff_host"]);
  });

  it("moves the temporary thread's draft to the id the daemon returns", async () => {
    const realId = "thread_after_create";
    fake.response.current = {
      type: "thread_created",
      thread_id: realId,
      summary: threadSummary({ threadId: realId, host: "handoff_host" }),
    };

    const created = createThread({
      host: "handoff_host",
      harness: "claude_code",
      workspacePath: "~/code",
      model: "claude-sonnet-4-5",
      prompt: "start this",
    });
    const tempId = getState().pendingCreates[0]?.tempId ?? "";
    expect(tempId.startsWith("local-thread-")).toBe(true);

    // The optimistic thread is visible while the daemon starts it, so the box
    // can be typed in; that text must follow the thread to its real id.
    drafts.writeThreadDraft("handoff_host", tempId, "typed while starting");
    expect(await created).toBe(true);

    expect(readThreadDraft("handoff_host", tempId)).toBe("");
    expect(readThreadDraft("handoff_host", realId)).toBe("typed while starting");
  });
});
