// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";

import type { ThreadSummary } from "@/gen";
import type { StreamTurn } from "@/lib/store/projection";
import { TurnBlock } from "./TurnBlock";

vi.mock("@/lib/store/appStore", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/store/appStore")>();
  return { ...actual, forkThread: vi.fn(async () => true) };
});

import { forkThread } from "@/lib/store/appStore";

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
  resume_at: null,
  limit: null,
  ttft_ms: null,
  context_window: null,
  context_fill_percent: null,
};

function finishedTurn(overrides: Partial<StreamTurn> = {}): StreamTurn {
  return {
    id: "turn_7",
    startedAt: "2026-01-01T00:00:00Z",
    status: "completed",
    items: [
      {
        kind: "message",
        itemId: "item_7",
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

function renderTurn(turn: StreamTurn) {
  return render(<TurnBlock turn={turn} threadId="thread_1" summary={summary} onReply={() => {}} />);
}

beforeAll(() => {
  // Radix's dropdown and the media query hook expect APIs jsdom lacks.
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

function openMenu(trigger: HTMLElement) {
  fireEvent.pointerDown(trigger, {
    button: 0,
    ctrlKey: false,
    pointerType: "mouse",
  });
  fireEvent.click(trigger);
}

describe("fork from a turn", () => {
  afterEach(() => {
    cleanup();
    vi.mocked(forkThread).mockClear();
  });

  it("offers Fork from here in a finished turn's menu", async () => {
    renderTurn(finishedTurn());
    openMenu(screen.getByRole("button", { name: "Turn actions" }));
    const item = await screen.findByRole("menuitem", { name: "Fork from here" });
    fireEvent.click(item);
    await waitFor(() => expect(forkThread).toHaveBeenCalledWith("thread_1", "turn_7"));
  });

  it("does not offer a menu on a running turn", () => {
    renderTurn(finishedTurn({ status: "running" }));
    expect(screen.queryByRole("button", { name: "Turn actions" })).toBeNull();
  });

  it("uses the real turn behind an imported user cue", async () => {
    renderTurn(
      finishedTurn({
        id: "cue:user:item_8",
        imported: true,
        forkTurnId: "turn_8",
        items: [
          {
            kind: "message",
            itemId: "item_8",
            role: "user",
            text: "an imported prompt",
            attachments: [],
            streaming: false,
          },
        ],
      }),
    );
    openMenu(screen.getByRole("button", { name: "Fork from this turn" }));
    const item = await screen.findByRole("menuitem", { name: "Fork from here" });
    fireEvent.click(item);
    await waitFor(() => expect(forkThread).toHaveBeenCalledWith("thread_1", "turn_8"));
  });
});
