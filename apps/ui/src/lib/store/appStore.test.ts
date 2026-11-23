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
import { applyEvent, createThread, getState, loadThreads, setThreadTab, store } from "./appStore";

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
    runtime_mode: "approval_required",
    reasoning_efforts: [],
    models: [],
    fast: false,
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

describe("a create reply and a generated title in either order", () => {
  const created = { ...threadSummary({ threadId: "th_race", host: "host_race" }) };
  created.title = "Fix the build";

  function generatedUpsert() {
    return {
      seq: 9,
      event: {
        type: "thread_upserted" as const,
        ...threadSummary({ threadId: "th_race", host: "host_race" }),
        title: "Generated title",
      },
    };
  }

  function createInput() {
    return {
      host: "host_race" as never,
      harness: "claude_code" as const,
      workspacePath: "~/code",
      model: "claude-sonnet-4-5",
      prompt: "Fix the build",
    };
  }

  beforeEach(() => {
    fake.response.current = null;
    fake.request.mockClear();
    seedHosts(["host_race"]);
  });

  it("keeps the upsert when the generated title was projected first", async () => {
    // The event wins the race and is projected before the reply arrives.
    store.setState(applyEvent(getState(), generatedUpsert()));
    fake.response.current = {
      type: "thread_created",
      thread_id: "th_race",
      turn_id: "turn_race",
      summary: created,
    };

    await createThread(createInput());

    expect(getState().threads.th_race?.title).toBe("Generated title");
  });

  it("lets the upsert replace the create reply when it arrives second", async () => {
    fake.response.current = {
      type: "thread_created",
      thread_id: "th_race",
      turn_id: "turn_race",
      summary: created,
    };

    await createThread(createInput());
    expect(getState().threads.th_race?.title).toBe("Fix the build");

    store.setState(applyEvent(getState(), generatedUpsert()));
    expect(getState().threads.th_race?.title).toBe("Generated title");
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

  it("drops the temporary thread's draft when the create fails", async () => {
    fake.request.mockRejectedValueOnce(new Error("the host is unreachable"));

    const created = createThread({
      host: "handoff_host",
      harness: "claude_code",
      workspacePath: "~/code",
      model: "claude-sonnet-4-5",
      prompt: "start this",
    });
    const tempId = getState().pendingCreates[0]?.tempId ?? "";
    expect(tempId.startsWith("local-thread-")).toBe(true);

    // The optimistic thread is on screen, so the box can be typed in.
    drafts.writeThreadDraft("handoff_host", tempId, "typed while starting");
    expect(await created).toBe(false);

    // Nothing will ever reconcile this id, and another session could reuse it:
    // its memory-only draft must not outlive the failed create.
    expect(readThreadDraft("handoff_host", tempId)).toBe("");
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
