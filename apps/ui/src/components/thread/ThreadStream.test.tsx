// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { QueuedMessage, ThreadSummary } from "@/gen";
import { QueuedTurn } from "./ThreadStream";

const mock = vi.hoisted(() => ({
  sendQueuedNow: vi.fn(async () => true),
  moveQueuedMessage: vi.fn(async () => true),
  removeQueuedMessage: vi.fn(async () => true),
}));

vi.mock("@/lib/store/appStore", () => ({
  useApp: () => null,
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

const message: QueuedMessage = {
  message_id: "qm_one",
  text: "a queued thought",
  attachments: [],
  queued_at: "2026-01-01T00:00:00.000Z",
};

function summary(overrides: Partial<ThreadSummary> = {}): ThreadSummary {
  return {
    thread_id: "th_one",
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
    queued: [message],
    steer: true,
    ...overrides,
  };
}

function renderQueued(
  options: { editing?: boolean; index?: number; total?: number; steer?: boolean } = {},
) {
  const threadSummary = summary({
    steer: options.steer ?? true,
    queued: Array.from({ length: options.total ?? 2 }, (_, index) => ({
      ...message,
      message_id: `qm_${index}`,
      text: `message ${index}`,
    })),
  });
  return render(
    <QueuedTurn
      threadId="th_one"
      summary={threadSummary}
      message={threadSummary.queued[options.index ?? 0] as QueuedMessage}
      index={options.index ?? 0}
      editing={options.editing ?? false}
    />,
  );
}

describe("QueuedTurn", () => {
  beforeEach(() => {
    mock.sendQueuedNow.mockClear();
    mock.removeQueuedMessage.mockClear();
    mock.moveQueuedMessage.mockClear();
  });

  afterEach(() => cleanup());

  it("shows the message as not yet sent", () => {
    renderQueued();
    expect(screen.getByText("Queued")).toBeTruthy();
    expect(screen.getByText("message 0")).toBeTruthy();
    expect(screen.getByText("Send now steers it into this turn")).toBeTruthy();
  });

  it("offers Send now, Edit and Remove", () => {
    renderQueued();
    fireEvent.click(screen.getByRole("button", { name: "Send now" }));
    expect(mock.sendQueuedNow).toHaveBeenCalledWith("th_one", "qm_0");

    const heard: CustomEvent[] = [];
    const listener = (event: Event) => heard.push(event as CustomEvent);
    window.addEventListener("h4rn:edit-queued", listener);
    fireEvent.click(screen.getByRole("button", { name: "Edit" }));
    window.removeEventListener("h4rn:edit-queued", listener);
    expect(heard[0]?.detail).toMatchObject({ threadId: "th_one", message: { message_id: "qm_0" } });

    fireEvent.click(screen.getByRole("button", { name: "Remove" }));
    expect(mock.removeQueuedMessage).toHaveBeenCalledWith("th_one", "qm_0");
  });

  it("reorders and disables the edges", () => {
    renderQueued({ index: 1, total: 3 });
    fireEvent.click(screen.getByRole("button", { name: "Move up" }));
    expect(mock.moveQueuedMessage).toHaveBeenCalledWith("th_one", "qm_1", "up");
    fireEvent.click(screen.getByRole("button", { name: "Move down" }));
    expect(mock.moveQueuedMessage).toHaveBeenLastCalledWith("th_one", "qm_1", "down");

    cleanup();
    renderQueued({ index: 0, total: 2 });
    expect((screen.getByRole("button", { name: "Move up" }) as HTMLButtonElement).disabled).toBe(
      true,
    );
    cleanup();
    renderQueued({ index: 1, total: 2 });
    expect((screen.getByRole("button", { name: "Move down" }) as HTMLButtonElement).disabled).toBe(
      true,
    );
  });

  it("marks the message open for editing", () => {
    const { container } = renderQueued({ editing: true });
    expect(container.querySelector(".q-queued")?.getAttribute("data-editing")).toBe("true");
  });
});
