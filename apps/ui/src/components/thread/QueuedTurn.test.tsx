// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { QueuedMessage, ThreadSummary } from "@/gen";
import { setLocalePreference } from "@/lib/i18n";
import { QueuedTurn, sendNowHint } from "./QueuedTurn";

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
  steerable: true,
  delivery_unknown: false,
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
    rewind: "handoff",
    file_rewind: false,
    queued_count: 1,
    queue_revision: 1,
    steer: true,
    ...overrides,
  };
}

function renderQueued(
  options: {
    editing?: boolean;
    index?: number;
    total?: number;
    steer?: boolean;
    running?: boolean;
    steerable?: boolean;
    deliveryUnknown?: boolean;
  } = {},
) {
  const total = options.total ?? 2;
  const rows = Array.from({ length: total }, (_, index) => ({
    ...message,
    message_id: `qm_${index}`,
    text: `message ${index}`,
    steerable: options.steerable ?? true,
    delivery_unknown: options.deliveryUnknown ?? false,
  }));
  const threadSummary = summary({
    steer: options.steer ?? true,
    queued_count: total,
    ...(options.running === false ? { agent_state: "rate_limited" } : {}),
  });
  return render(
    <QueuedTurn
      threadId="th_one"
      summary={threadSummary}
      message={rows[options.index ?? 0] as QueuedMessage}
      index={options.index ?? 0}
      total={total}
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

  it("does not promise a steer for a row the daemon will move to the front", () => {
    renderQueued({ steerable: false });
    expect(screen.getByText("Waits for this turn to finish")).toBeTruthy();
    expect(screen.queryByText("Send now steers it into this turn")).toBeNull();
  });

  it("shows a row that may already have been delivered as resend or discard", () => {
    renderQueued({ deliveryUnknown: true });
    expect(screen.getByText("May have been delivered")).toBeTruthy();
    expect(
      screen.getByText("May have been delivered; sending again adds a second copy"),
    ).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Send again" }));
    expect(mock.sendQueuedNow).toHaveBeenCalledWith("th_one", "qm_0");
    fireEvent.click(screen.getByRole("button", { name: "Discard" }));
    expect(mock.removeQueuedMessage).toHaveBeenCalledWith("th_one", "qm_0");
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

  it("only promises steering while a turn is really running", () => {
    const running = renderQueued();
    expect(screen.getByText("Send now steers it into this turn")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Send now" }).getAttribute("title") ?? "").toContain(
      "Steer",
    );
    running.unmount();

    // A paused thread has no running turn, so Send now starts the message next.
    renderQueued({ running: false });
    expect(screen.getByText("Send now starts it next")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Send now" }).getAttribute("title") ?? "").toContain(
      "front of the queue",
    );
  });
});

describe("QueuedTurn in another language", () => {
  afterEach(async () => {
    cleanup();
    await setLocalePreference("en");
  });

  it("words the row, its hint and its actions in the active language", async () => {
    await setLocalePreference("es");
    render(
      <QueuedTurn
        threadId="th_one"
        summary={summary()}
        message={message}
        index={0}
        total={2}
        editing={false}
      />,
    );
    expect(screen.getByText("En cola")).toBeTruthy();
    expect(screen.getByText("«Enviar ahora» lo dirige a este turno")).toBeTruthy();
    expect(screen.getByRole("button", { name: /Enviar ahora/ })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Bajar" })).toBeTruthy();
    expect(sendNowHint(summary(), { ...message, delivery_unknown: true })).toBe(
      "Puede que se haya entregado; enviarlo de nuevo añade una segunda copia",
    );
  });
});
