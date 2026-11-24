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
import {
  applyEvent,
  createThread,
  editQueuedMessage,
  getState,
  loadThread,
  loadThreadQueue,
  loadThreads,
  moveQueuedMessage,
  sendQueuedNow,
  setThreadTab,
  steerOldestQueued,
  store,
  syncQueueWithSummary,
} from "./appStore";

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
    pending_guest_requests: 0,
    unread: false,
    resume_at: null,
    limit: null,
    queued_count: 0,
    queue_revision: 0,
    steer: true,
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

describe("loadThread", () => {
  beforeEach(() => {
    fake.response.current = null;
    fake.request.mockReset();
    seedHosts([]);
  });

  it("follows the daemon's replay pages until there is no more", async () => {
    const summary = threadSummary({ threadId: "page_thread", host: "page_host" });
    const message = (seq: number, text: string) => ({
      seq,
      event: {
        type: "item_completed",
        thread_id: "page_thread",
        turn_id: `turn-${seq}`,
        item: {
          kind: "message",
          item_id: `item-${seq}`,
          role: "user",
          text,
          attachments: [],
        },
      },
    });
    fake.request
      .mockResolvedValueOnce({
        type: "thread",
        summary,
        events: [message(1, "first page")],
        next_seq: 1,
        more: true,
      })
      .mockResolvedValueOnce({
        type: "thread",
        summary,
        events: [message(2, "second page")],
        next_seq: 2,
        more: false,
      });

    await loadThread("page_thread", 0);

    expect(fake.request).toHaveBeenCalledTimes(2);
    const second = fake.request.mock.calls[1]?.[0] as { since_seq: number } | undefined;
    expect(second?.since_seq).toBe(1);
    const log = getState().logs.page_thread;
    expect(log?.nextSeq).toBe(2);
    expect(log?.loaded).toBe(true);
    const texts = (log?.turns ?? [])
      .flatMap((turn) => turn.items)
      .flatMap((item) => (item.kind === "message" ? [item.text] : []));
    expect(texts).toEqual(["first page", "second page"]);
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

describe("queued messages", () => {
  const queued = (id: string, text: string) => ({
    message_id: id,
    text,
    attachments: [],
    queued_at: "2026-01-01T00:00:00.000Z",
  });

  /**
   * Seeds a running thread with rows in the queue cache and a summary that
   * counts them, the way the daemon publishes them.
   */
  const seedQueue = (overrides: Partial<ThreadSummary> = {}) => {
    const thread = {
      ...threadSummary({ threadId: "queue_thread", host: "queue_host" }),
      agent_state: "thinking" as const,
      queued_count: 2,
      queue_revision: 4,
      steer: true,
      ...overrides,
    };
    store.setState({
      ...getState(),
      threads: { queue_thread: thread },
      threadSeqs: { ...getState().threadSeqs, queue_thread: 40 },
      queues: {
        queue_thread: {
          queued: [queued("qm_one", "first"), queued("qm_two", "second")],
          total: 2,
          revision: 4,
        },
      },
    });
  };

  const queueIds = (threadId: string) =>
    getState().queues[threadId]?.queued.map((message) => message.message_id) ?? [];

  beforeEach(() => {
    fake.request.mockClear();
    seedQueue();
  });

  it("steers the oldest queued message and leaves the rest", () => {
    fake.response.current = {
      type: "queue_updated",
      thread_id: "queue_thread",
      queued: [queued("qm_two", "second")],
      total: 1,
      revision: 5,
      seq: 41,
    };

    expect(steerOldestQueued("queue_thread")).toBe(true);
    const body = fake.request.mock.calls[0]?.[0] as {
      type: string;
      message_id: string;
      action: { type: string };
    };
    expect(body.type).toBe("queue_message");
    expect(body.message_id).toBe("qm_one");
    expect(body.action.type).toBe("steer");
  });

  it("does not steer when nothing is queued", () => {
    seedQueue({ queued_count: 0 });
    store.setState({ ...getState(), queues: {} });
    expect(steerOldestQueued("queue_thread")).toBe(false);
    expect(fake.request).not.toHaveBeenCalled();
  });

  it("does not steer when no turn is running", () => {
    seedQueue({ agent_state: "rate_limited" });
    expect(steerOldestQueued("queue_thread")).toBe(false);
    expect(fake.request).not.toHaveBeenCalled();
  });

  it("steers a turn a limit paused but the daemon still reports as active", () => {
    seedQueue({ agent_state: "rate_limited", active_turn_id: "turn_running" });
    fake.response.current = {
      type: "queue_updated",
      thread_id: "queue_thread",
      queued: [queued("qm_two", "second")],
      total: 1,
      revision: 5,
      seq: 41,
    };
    expect(steerOldestQueued("queue_thread")).toBe(true);
    expect(fake.request).toHaveBeenCalled();
  });

  it("does not steer a harness without mid-turn input", () => {
    seedQueue({ steer: false });
    expect(steerOldestQueued("queue_thread")).toBe(false);
    expect(fake.request).not.toHaveBeenCalled();
  });

  it("reorders from the daemon's answer", async () => {
    fake.response.current = {
      type: "queue_updated",
      thread_id: "queue_thread",
      queued: [queued("qm_two", "second"), queued("qm_one", "first")],
      total: 2,
      revision: 5,
      seq: 41,
    };

    await moveQueuedMessage("queue_thread", "qm_two", "up");

    const body = fake.request.mock.calls[0]?.[0] as {
      type: string;
      action: { type: string; direction: string };
    };
    expect(body.type).toBe("queue_message");
    expect(body.action).toEqual({ type: "move", direction: "up" });
    expect(queueIds("queue_thread")).toEqual(["qm_two", "qm_one"]);
  });

  it("asks the daemon to move a message and keeps its error off the queue", async () => {
    fake.request.mockRejectedValueOnce(new Error("no"));
    expect(await moveQueuedMessage("queue_thread", "qm_one", "down")).toBe(false);
    expect(queueIds("queue_thread")).toEqual(["qm_one", "qm_two"]);
  });

  it("ignores a queue reply older than the summary this client already has", async () => {
    // A newer summary moved the queue on; the reply describes the old one.
    const current = getState().threads.queue_thread;
    if (!current) throw new Error("the queue test thread is missing");
    store.setState({
      ...getState(),
      threads: {
        queue_thread: { ...current, queue_revision: 6 },
      },
      queues: {
        queue_thread: { queued: [queued("qm_two", "second")], total: 1, revision: 6 },
      },
      threadSeqs: { queue_thread: 42 },
    });
    fake.response.current = {
      type: "queue_updated",
      thread_id: "queue_thread",
      queued: [queued("qm_one", "first"), queued("qm_two", "second")],
      total: 2,
      revision: 5,
      seq: 41,
    };

    await moveQueuedMessage("queue_thread", "qm_one", "down");

    expect(queueIds("queue_thread")).toEqual(["qm_two"]);
  });

  it("keeps newer rows when a no-op reply arrives", async () => {
    fake.response.current = {
      type: "queue_updated",
      thread_id: "queue_thread",
      queued: [],
      total: 0,
      revision: 0,
      seq: 0,
    };

    await moveQueuedMessage("queue_thread", "qm_one", "up");

    expect(queueIds("queue_thread")).toEqual(["qm_one", "qm_two"]);
    expect(getState().queues.queue_thread?.total).toBe(2);
  });

  it("fetches the queue in pages, separately from the summary", async () => {
    const current = getState().threads.queue_thread;
    if (!current) throw new Error("the queue test thread is missing");
    store.setState({
      ...getState(),
      queues: {},
      threads: {
        queue_thread: { ...current, queued_count: 2, queue_revision: 7 },
      },
    });
    fake.request
      .mockImplementationOnce(async () => ({
        type: "thread_queue",
        thread_id: "queue_thread",
        queued: [queued("qm_one", "first")],
        total: 2,
        next_offset: 1,
        revision: 7,
      }))
      .mockImplementationOnce(async () => ({
        type: "thread_queue",
        thread_id: "queue_thread",
        queued: [queued("qm_two", "second")],
        total: 2,
        next_offset: 2,
        revision: 7,
      }));

    await loadThreadQueue("queue_thread");

    const requests = fake.request.mock.calls.map((call) => (call[0] as { offset: number }).offset);
    expect(requests).toEqual([0, 1]);
    expect(queueIds("queue_thread")).toEqual(["qm_one", "qm_two"]);
    expect(getState().queues.queue_thread?.total).toBe(2);
  });
  it("names the turn it saw when a queued message is sent now", async () => {
    seedQueue({ active_turn_id: "turn_seen" });
    fake.response.current = {
      type: "queue_updated",
      thread_id: "queue_thread",
      queued: [queued("qm_two", "second")],
      total: 1,
      revision: 5,
      seq: 41,
    };

    await sendQueuedNow("queue_thread", "qm_one");

    const body = fake.request.mock.calls[0]?.[0] as {
      action: { type: string; turn_id: string | null };
    };
    expect(body.action).toEqual({ type: "send_now", turn_id: "turn_seen" });
  });

  it("restarts paging when the queue changes between two pages", async () => {
    const current = getState().threads.queue_thread;
    if (!current) throw new Error("the queue test thread is missing");
    store.setState({
      ...getState(),
      queues: {},
      threads: { queue_thread: { ...current, queued_count: 2, queue_revision: 7 } },
    });
    // A promotion lands between the pages: the second page belongs to a new
    // queue, so joining them would show rows that never stood next to each
    // other. The read starts over instead.
    fake.request
      .mockImplementationOnce(async () => ({
        type: "thread_queue",
        thread_id: "queue_thread",
        queued: [queued("qm_one", "first")],
        total: 2,
        next_offset: 1,
        revision: 7,
      }))
      .mockImplementationOnce(async () => ({
        type: "thread_queue",
        thread_id: "queue_thread",
        queued: [queued("qm_two", "second")],
        total: 1,
        next_offset: 1,
        revision: 8,
      }))
      .mockImplementationOnce(async () => ({
        type: "thread_queue",
        thread_id: "queue_thread",
        queued: [queued("qm_two", "second")],
        total: 1,
        next_offset: 1,
        revision: 8,
      }));

    await loadThreadQueue("queue_thread");

    const requests = fake.request.mock.calls.map((call) => (call[0] as { offset: number }).offset);
    expect(requests).toEqual([0, 1, 0]);
    expect(queueIds("queue_thread")).toEqual(["qm_two"]);
    expect(getState().queues.queue_thread?.revision).toBe(8);
  });

  it("refetches every page when a reply carries only the first one", async () => {
    const current = getState().threads.queue_thread;
    if (!current) throw new Error("the queue test thread is missing");
    store.setState({
      ...getState(),
      queues: {},
      threadSeqs: { queue_thread: 40 },
      threads: { queue_thread: { ...current, queued_count: 150, queue_revision: 5 } },
    });
    const rows = Array.from({ length: 150 }, (_, index) =>
      queued(`qm_${index}`, `message ${index}`),
    );
    // The daemon's pages carry the edit that just landed.
    const afterEdit = rows.map((message, index) =>
      index === 0 ? { ...message, text: "edited words" } : message,
    );
    fake.response.current = null;
    fake.request
      .mockResolvedValueOnce({
        type: "queue_updated",
        thread_id: "queue_thread",
        // A bounded reply: the first page of a much longer queue, with the
        // edited row in it.
        queued: rows.slice(0, 100),
        total: 150,
        revision: 5,
        seq: 41,
      })
      .mockImplementationOnce(async () => ({
        type: "thread_queue",
        thread_id: "queue_thread",
        queued: afterEdit.slice(0, 100),
        total: 150,
        next_offset: 100,
        revision: 5,
      }))
      .mockImplementationOnce(async () => ({
        type: "thread_queue",
        thread_id: "queue_thread",
        queued: afterEdit.slice(100),
        total: 150,
        next_offset: 150,
        revision: 5,
      }));

    await editQueuedMessage("queue_thread", "qm_0", "edited words", []);

    await vi.waitFor(() => expect(getState().queues.queue_thread?.total).toBe(150));
    expect(getState().queues.queue_thread?.queued).toHaveLength(150);
    const edited = getState().queues.queue_thread?.queued.find(
      (message) => message.message_id === "qm_0",
    );
    expect(edited?.text).toBe("edited words");
  });

  it("drops a queue reply that arrives after the summary says nothing waits", async () => {
    const current = getState().threads.queue_thread;
    if (!current) throw new Error("the queue test thread is missing");
    store.setState({
      ...getState(),
      queues: {
        queue_thread: {
          queued: [queued("qm_one", "first"), queued("qm_two", "second")],
          total: 2,
          revision: 4,
        },
      },
      threads: { queue_thread: { ...current, queued_count: 2, queue_revision: 4 } },
    });
    // A fetch for the two-message queue is in flight when the message starts:
    // the summary then says nothing waits, at a newer revision.
    fake.request.mockImplementationOnce(async () => ({
      type: "thread_queue",
      thread_id: "queue_thread",
      queued: [queued("qm_one", "first"), queued("qm_two", "second")],
      total: 2,
      next_offset: 2,
      revision: 4,
    }));
    const inflight = loadThreadQueue("queue_thread");
    syncQueueWithSummary({ ...current, queued_count: 0, queue_revision: 5 });

    await inflight;

    expect(queueIds("queue_thread")).toEqual([]);
    expect(getState().queues.queue_thread?.revision).toBe(5);
  });

  it("loads the queue when the thread it opens still has messages waiting", async () => {
    store.setState({ ...getState(), queues: {} });
    const summary = {
      ...threadSummary({ threadId: "queue_thread", host: "load_host_q" }),
      queued_count: 1,
      queue_revision: 9,
    };
    fake.request
      .mockResolvedValueOnce({
        type: "thread",
        summary,
        events: [],
        next_seq: 0,
        more: false,
      })
      .mockImplementationOnce(async () => ({
        type: "thread_queue",
        thread_id: "queue_thread",
        queued: [queued("qm_one", "first")],
        total: 1,
        next_offset: 1,
        revision: 9,
      }));

    await loadThread("queue_thread", 0);

    await vi.waitFor(() => expect(queueIds("queue_thread")).toEqual(["qm_one"]));
  });

  it("loads the queue for a thread list that still has messages waiting", async () => {
    seedHosts(["load_host_q"]);
    store.setState({ ...getState(), queues: {} });
    fake.request
      .mockImplementationOnce(async () => ({
        type: "threads",
        threads: [
          {
            ...threadSummary({ threadId: "queue_thread", host: "load_host_q" }),
            queued_count: 1,
            queue_revision: 9,
          },
        ],
      }))
      .mockImplementationOnce(async () => ({
        type: "thread_queue",
        thread_id: "queue_thread",
        queued: [queued("qm_one", "first")],
        total: 1,
        next_offset: 1,
        revision: 9,
      }));

    await loadThreads();

    await vi.waitFor(() => expect(queueIds("queue_thread")).toEqual(["qm_one"]));
  });
});
