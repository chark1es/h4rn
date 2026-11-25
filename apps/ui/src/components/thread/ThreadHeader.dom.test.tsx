// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";

import type { ThreadSummary } from "@/gen";
import type { StreamTurn } from "@/lib/store/projection";
import { createThreadLog } from "@/lib/store/projection";

vi.mock("@/lib/store/appStore", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/store/appStore")>();
  return { ...actual, forkThread: vi.fn(async () => true) };
});

import { forkThread, getState, store } from "@/lib/store/appStore";
import { ThreadHeader } from "./ThreadHeader";

const summary: ThreadSummary = {
  thread_id: "thread_1",
  parent_thread_id: null,
  host: "host_1",
  harness: "claude_code",
  title: "Fix the parser",
  workspace_path: "/work",
  model: "claude-sonnet",
  runtime_mode: "approval_required",
  reasoning_efforts: [],
  models: [],
  fast: false,
  tab: "done",
  placed_by_user: false,
  pinned: false,
  snoozed_until: null,
  last_activity: "2026-01-01T00:00:00Z",
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
  pending_guest_requests: 0,
  unread: false,
  queued_count: 0,
  queue_revision: 0,
  steer: false,
  resume_at: null,
  limit: null,
  ttft_ms: null,
  context_window: null,
  context_fill_percent: null,
};

function turn(overrides: Partial<StreamTurn> = {}): StreamTurn {
  return {
    id: "turn_1",
    startedAt: "2026-01-01T00:00:00Z",
    status: "completed",
    items: [
      {
        kind: "message",
        itemId: "item_1",
        role: "assistant",
        text: "Fixed the parser.",
        attachments: [],
        streaming: false,
      },
    ],
    usage: null,
    changes: null,
    ...overrides,
  };
}

function renderHeader(turns: StreamTurn[]) {
  store.setState({
    ...getState(),
    threads: { thread_1: summary },
    logs: { thread_1: { ...createThreadLog(), loaded: true, turns } },
    ui: {
      ...getState().ui,
      route: { kind: "thread", threadId: "thread_1" },
      paletteOpen: false,
    },
  });
  render(<ThreadHeader summary={summary} host={undefined} onBack={() => {}} />);
}

function openMenu() {
  const trigger = screen.getByRole("button", { name: "Thread actions" });
  fireEvent.pointerDown(trigger, {
    button: 0,
    ctrlKey: false,
    pointerType: "mouse",
  });
  fireEvent.click(trigger);
}

beforeAll(() => {
  Element.prototype.hasPointerCapture ??= () => false;
  Element.prototype.setPointerCapture ??= () => undefined;
  Element.prototype.releasePointerCapture ??= () => undefined;
  Object.defineProperty(window, "matchMedia", {
    writable: true,
    value: (query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addListener: () => undefined,
      removeListener: () => undefined,
      addEventListener: () => undefined,
      removeEventListener: () => undefined,
      dispatchEvent: () => false,
    }),
  });
});

describe("ThreadHeader fork from latest turn", () => {
  afterEach(() => {
    cleanup();
    vi.mocked(forkThread).mockClear();
  });

  it("is disabled while the newest turn is running", async () => {
    renderHeader([turn({ status: "running" })]);
    openMenu();
    const item = await screen.findByRole("menuitem", { name: "Fork from latest turn" });
    expect(item.getAttribute("data-disabled")).not.toBeNull();
    fireEvent.click(item);
    expect(forkThread).not.toHaveBeenCalled();
  });

  it("forks the latest settled turn", async () => {
    renderHeader([turn()]);
    openMenu();
    const item = await screen.findByRole("menuitem", { name: "Fork from latest turn" });
    expect(item.getAttribute("data-disabled")).toBeNull();
    fireEvent.click(item);
    await waitFor(() => expect(forkThread).toHaveBeenCalledWith("thread_1", "turn_1"));
  });
});
