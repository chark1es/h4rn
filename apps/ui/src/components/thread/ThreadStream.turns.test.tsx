// @vitest-environment jsdom

import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { ThreadSummary, Usage } from "@/gen";
import type { StreamTurn } from "@/lib/store/projection";

const scrollToIndex = vi.hoisted(() => vi.fn());

vi.mock("@/lib/store/appStore", () => ({
  useApp: (selector: (state: unknown) => unknown) =>
    selector({ ui: { queuedEdit: null }, queues: {} }),
}));

vi.mock("@/hooks/use-media-query", () => ({
  useIsPhone: () => false,
  usePrefersReducedMotion: () => true,
}));

vi.mock("@/features/attachments/MessageAttachments", () => ({
  MessageAttachments: () => null,
}));

// The stream is virtualised in the app; the test needs every row rendered and
// the turn shortcuts' scroll target observable.
vi.mock("@tanstack/react-virtual", () => ({
  useVirtualizer: ({ count }: { count: number }) => ({
    getVirtualItems: () => Array.from({ length: count }, (_, index) => ({ index, key: index })),
    getTotalSize: () => count * 160,
    measureElement: () => undefined,
    scrollToIndex,
  }),
}));

import { ThreadStream } from "./ThreadStream";

const usage: Usage = {
  input_tokens: 0,
  output_tokens: 0,
  cached_input_tokens: 0,
  cost_micros: 0,
  duration_ms: 0,
  generation_ms: 0,
  tps: 0,
};

const summary: ThreadSummary = {
  thread_id: "thread_1",
  parent_thread_id: null,
  host: "host_1",
  harness: "codex",
  title: "A switched thread",
  workspace_path: "/work",
  model: "gpt-5.1-codex",
  runtime_mode: "approval_required",
  reasoning_efforts: [],
  models: [],
  fast: false,
  tab: "working",
  placed_by_user: false,
  pinned: false,
  snoozed_until: null,
  last_activity: "2026-11-18T09:00:00Z",
  agent_state: "done",
  usage,
  ttft_ms: null,
  context_window: null,
  context_fill_percent: null,
  needs_attention: false,
  pending_guest_requests: 0,
  unread: false,
  resume_at: null,
  rewind: "handoff",
  file_rewind: false,
  limit: null,
  queued_count: 0,
  queue_revision: 0,
  steer: true,
};

/** One agent turn. */
function turn(id: string, text: string): StreamTurn {
  return {
    id,
    startedAt: "2026-11-18T09:00:00Z",
    status: "completed",
    items: [
      {
        kind: "message",
        itemId: `${id}_item`,
        role: "assistant",
        text,
        attachments: [],
        streaming: false,
      },
    ],
    usage,
    changes: null,
  };
}

/** The person's prompt, shown as its own row rather than a turn. */
function cue(id: string, text: string): StreamTurn {
  return {
    id: `cue:user:${id}`,
    startedAt: "2026-11-18T09:00:00Z",
    status: "completed",
    items: [
      {
        kind: "message",
        itemId: id,
        role: "user",
        text,
        attachments: [],
        streaming: false,
      },
    ],
    usage: null,
    changes: null,
  };
}

/** The transcript's quiet switch notice. */
const notice: StreamTurn = {
  id: "switch:claude_code:codex",
  startedAt: "2026-11-18T09:10:00Z",
  status: "completed",
  items: [
    {
      kind: "harness_switch",
      itemId: "switch:claude_code:codex",
      from: "claude_code",
      to: "codex",
    },
  ],
  usage: null,
  changes: null,
  divider: true,
};

// Real turns at 0, 2 and 4; a cue at 1 and the switch notice at 3.
const rows: StreamTurn[] = [
  turn("turn_a", "before the switch"),
  cue("item_1", "a prompt"),
  turn("turn_b", "right after the switch"),
  notice,
  turn("turn_c", "later"),
];

/** Runs the keyboard shortcut the app turns into this event. */
function jump(direction: number) {
  act(() => {
    window.dispatchEvent(new CustomEvent("h4rn:turn", { detail: { direction } }));
  });
}

describe("turn navigation across a harness switch", () => {
  afterEach(() => {
    cleanup();
    scrollToIndex.mockClear();
  });

  it("steps over the switch notice to the real turns around it", async () => {
    render(
      <ThreadStream
        threadId="thread_1"
        summary={summary}
        turns={rows}
        pending={[]}
        onReply={() => undefined}
      />,
    );
    // The transcript shows the switch as a divider, not as a turn.
    expect(screen.getByText(/Switched from Claude Code to Codex/)).toBeTruthy();
    // The tail follow lands on the newest row (index 4) first.
    await waitFor(() => expect(scrollToIndex).toHaveBeenCalledWith(4, { align: "end" }));
    scrollToIndex.mockClear();

    // Previous Turn from the newest turn reaches index 2, not the notice at 3.
    jump(-1);
    expect(scrollToIndex).toHaveBeenCalledWith(2, { align: "start" });
    // And again past the cue at 1, to the first real turn.
    scrollToIndex.mockClear();
    jump(-1);
    expect(scrollToIndex).toHaveBeenCalledWith(0, { align: "start" });

    // Next Turn from the first turn lands past the cue, on index 2.
    scrollToIndex.mockClear();
    jump(1);
    expect(scrollToIndex).toHaveBeenCalledWith(2, { align: "start" });
  });
});
