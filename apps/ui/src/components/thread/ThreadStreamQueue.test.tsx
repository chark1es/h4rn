// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { QueuedMessage, ThreadSummary } from "@/gen";

const mock = vi.hoisted(() => {
  const summary = {
    thread_id: "thread_queue",
    parent_thread_id: null,
    host: "host_ui",
    harness: "claude_code",
    title: "A thread",
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
    agent_state: "thinking",
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
    queued_count: 2,
    queue_revision: 3,
    steer: true,
  };
  const queued: QueuedMessage[] = [
    {
      message_id: "qm_one",
      text: "first queued",
      attachments: [],
      queued_at: "2026-01-01T00:00:00.000Z",
      steerable: true,
      delivery_unknown: false,
    },
    {
      message_id: "qm_two",
      text: "second queued",
      attachments: [],
      queued_at: "2026-01-01T00:00:01.000Z",
      steerable: true,
      delivery_unknown: false,
    },
  ];
  return {
    summary,
    state: {
      ui: { queuedEdit: null },
      queues: {
        thread_queue: { queued, total: 2, revision: 3 },
      },
    },
    moveQueuedMessage: vi.fn(async () => true),
    removeQueuedMessage: vi.fn(async () => true),
    sendQueuedNow: vi.fn(async () => true),
  };
});

vi.mock("@/lib/store/appStore", () => ({
  useApp: (selector: (state: unknown) => unknown) => selector(mock.state),
  moveQueuedMessage: mock.moveQueuedMessage,
  removeQueuedMessage: mock.removeQueuedMessage,
  sendQueuedNow: mock.sendQueuedNow,
}));

vi.mock("@/hooks/use-media-query", () => ({
  useIsPhone: () => false,
}));

vi.mock("@/features/attachments/MessageAttachments", () => ({
  MessageAttachments: () => null,
}));

// The stream is virtualised in the app; the test needs every row rendered.
vi.mock("@tanstack/react-virtual", () => ({
  useVirtualizer: ({ count }: { count: number }) => ({
    getVirtualItems: () => Array.from({ length: count }, (_, index) => ({ index, key: index })),
    getTotalSize: () => count * 160,
    measureElement: () => undefined,
    scrollToIndex: () => undefined,
  }),
}));

import { ThreadStream } from "./ThreadStream";

describe("ThreadStream queue rows", () => {
  afterEach(() => cleanup());

  it("disables Move down on the last queued row even when other rows follow it", () => {
    // The stream's rows are a pending message plus two queued ones: the last
    // queued row is not the last row, but the queue ends there.
    render(
      <ThreadStream
        threadId="thread_queue"
        summary={mock.summary as ThreadSummary}
        turns={[]}
        pending={[
          {
            id: "local-1",
            text: "sending now",
            attachments: [],
            at: "2026-01-01T00:00:02.000Z",
            sending: true,
          },
        ]}
        onReply={() => undefined}
      />,
    );

    const moveDown = screen.getAllByRole("button", { name: "Move down" });
    expect(moveDown).toHaveLength(2);
    expect((moveDown[0] as HTMLButtonElement).disabled).toBe(false);
    expect((moveDown[1] as HTMLButtonElement).disabled).toBe(true);
  });
});
