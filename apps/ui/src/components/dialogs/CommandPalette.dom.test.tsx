// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";

import type { ThreadSummary } from "@/gen";
import type { StreamTurn } from "@/lib/store/projection";
import { createThreadLog } from "@/lib/store/projection";

vi.mock("@/lib/store/appStore", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/store/appStore")>();
  return { ...actual, forkThread: vi.fn(async () => true) };
});

import { forkThread, getState, store } from "@/lib/store/appStore";
import { CommandPalette } from "./CommandPalette";

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
  rewind: "handoff",
  file_rewind: false,
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

function renderPalette(turns: StreamTurn[]) {
  store.setState({
    ...getState(),
    threads: { thread_1: summary },
    logs: { thread_1: { ...createThreadLog(), loaded: true, turns } },
    ui: {
      ...getState().ui,
      route: { kind: "thread", threadId: "thread_1" },
      paletteOpen: true,
    },
  });
  render(<CommandPalette />);
}

function forkItem(): HTMLElement {
  const label = screen.getByText("Fork from latest turn");
  const item = label.closest('[data-slot="command-item"]');
  if (!(item instanceof HTMLElement)) throw new Error("the fork row is not a command item");
  return item;
}

beforeAll(() => {
  Element.prototype.hasPointerCapture ??= () => false;
  Element.prototype.setPointerCapture ??= () => undefined;
  Element.prototype.releasePointerCapture ??= () => undefined;
  Element.prototype.scrollIntoView ??= () => undefined;
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
  class ResizeObserverStub {
    observe() {}
    unobserve() {}
    disconnect() {}
  }
  Object.defineProperty(window, "ResizeObserver", {
    writable: true,
    value: ResizeObserverStub,
  });
  Object.defineProperty(globalThis, "ResizeObserver", {
    writable: true,
    value: ResizeObserverStub,
  });
});

describe("CommandPalette fork from latest turn", () => {
  afterEach(() => {
    cleanup();
    vi.mocked(forkThread).mockClear();
  });

  it("is disabled while the newest turn is running", async () => {
    renderPalette([turn({ status: "running" })]);
    const item = await waitFor(() => forkItem());
    expect(item.hasAttribute("data-disabled")).toBe(true);
    fireEvent.click(item);
    expect(forkThread).not.toHaveBeenCalled();
  });

  it("forks the latest settled turn", async () => {
    renderPalette([turn()]);
    const item = await waitFor(() => forkItem());
    expect(item.hasAttribute("data-disabled")).toBe(false);
    fireEvent.click(item);
    await waitFor(() => expect(forkThread).toHaveBeenCalledWith("thread_1", "turn_1"));
  });
});

describe("CommandPalette keyboard", () => {
  afterEach(() => {
    cleanup();
    vi.mocked(forkThread).mockClear();
  });

  it("highlights the best match as you type, runs it with Enter and moves with the arrows", async () => {
    renderPalette([turn()]);
    const input = screen.getByRole("combobox");
    await act(async () => {
      fireEvent.change(input, { target: { value: "fork" } });
    });
    const highlighted = () =>
      document.querySelector('[data-slot="command-item"][data-highlighted]')?.textContent;
    expect(highlighted()).toContain("Fork from latest turn");
    await act(async () => {
      fireEvent.keyDown(input, { key: "Enter" });
    });
    await waitFor(() => expect(forkThread).toHaveBeenCalledWith("thread_1", "turn_1"));
  });

  it("moves between rows with the arrow keys and stops at the ends, as cmdk did", async () => {
    renderPalette([turn()]);
    const input = screen.getByRole("combobox");
    const items = () => [...document.querySelectorAll('[data-slot="command-item"]')];
    const highlightedIndex = () =>
      items().findIndex((item) => item.hasAttribute("data-highlighted"));
    await act(async () => {
      input.focus();
    });
    expect(highlightedIndex()).toBe(0);
    await act(async () => {
      fireEvent.keyDown(input, { key: "ArrowDown" });
    });
    expect(highlightedIndex()).toBe(1);
    await act(async () => {
      fireEvent.keyDown(input, { key: "ArrowUp" });
      fireEvent.keyDown(input, { key: "ArrowUp" });
    });
    expect(highlightedIndex()).toBe(0);
  });

  it("closes on Escape", async () => {
    renderPalette([turn()]);
    await act(async () => {
      fireEvent.keyDown(screen.getByRole("combobox"), { key: "Escape" });
    });
    await waitFor(() => expect(getState().ui.paletteOpen).toBe(false));
  });
});
