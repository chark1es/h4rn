import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import type { ThreadSummary, Usage } from "@/gen";
import type { StreamTurn } from "@/lib/store/projection";
import { nextTurnIndex, ThreadReceipt } from "./ThreadStream";
import { TurnBlock } from "./TurnBlock";

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
  harness: "claude_code",
  title: "Imported",
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
  limit: null,
  queued_count: 0,
  queue_revision: 0,
  steer: true,
};

/** One assistant turn, marked as imported history only when asked. */
function turn(imported: boolean, count = usage): StreamTurn {
  const result: StreamTurn = {
    id: imported ? "turn_imported" : "turn_live",
    startedAt: "2026-11-18T09:00:00Z",
    status: "completed",
    items: [
      {
        kind: "message",
        itemId: imported ? "item_imported" : "item_live",
        role: "assistant",
        text: imported ? "an imported reply" : "a reply from here",
        attachments: [],
        streaming: false,
      },
    ],
    usage: imported ? null : count,
    changes: null,
  };
  if (imported) result.imported = true;
  return result;
}

describe("ThreadReceipt", () => {
  it("names an imported thread instead of inventing a conclusion for it", () => {
    const markup = renderToStaticMarkup(<ThreadReceipt summary={summary} turns={[turn(true)]} />);
    expect(markup).toContain("Imported from Claude Code");
    expect(markup).not.toContain("tokens");
    expect(markup).not.toContain("Finished");
  });

  it("keeps the import's receipt when no agent turn was written yet", () => {
    // An imported conversation the person has not continued has no assistant
    // turn: only their own message, which the stream shows as a cue row.
    const cue: StreamTurn = {
      id: "cue:user:item_1",
      startedAt: "2026-11-18T09:00:00Z",
      status: "completed",
      items: [
        {
          kind: "message",
          itemId: "item_1",
          role: "user",
          text: "an imported request",
          attachments: [],
          streaming: false,
        },
      ],
      usage: null,
      changes: null,
    };
    const markup = renderToStaticMarkup(
      <ThreadReceipt
        summary={{ ...summary, harness: "codex", imported_from: "claude_code" }}
        turns={[cue]}
      />,
    );
    expect(markup).toContain("Imported from Claude Code");
    expect(markup).not.toContain("Finished");
    expect(markup).not.toContain("tokens");
  });

  it("keeps the ordinary receipt for a thread that ran here", () => {
    const markup = renderToStaticMarkup(
      <ThreadReceipt
        summary={{
          ...summary,
          usage: { ...usage, input_tokens: 10, output_tokens: 5, duration_ms: 1_500 },
        }}
        turns={[turn(true), turn(false)]}
      />,
    );
    expect(markup).toContain("Finished");
    expect(markup).toContain("15 tokens");
    expect(markup).not.toContain("Imported from");
  });

  it("stays out of a fork while its stream is only copied history", () => {
    const markup = renderToStaticMarkup(
      <ThreadReceipt
        summary={{
          ...summary,
          forked_from: { thread_id: "thread_source", turn_id: "turn_source" },
        }}
        turns={[turn(true)]}
      />,
    );
    expect(markup).toBe("");
  });

  it("shows a fork's own receipt once it has run a turn here", () => {
    const markup = renderToStaticMarkup(
      <ThreadReceipt
        summary={{
          ...summary,
          forked_from: { thread_id: "thread_source", turn_id: "turn_source" },
          usage: { ...usage, input_tokens: 10, output_tokens: 5, duration_ms: 1_500 },
        }}
        turns={[turn(true), turn(false)]}
      />,
    );
    expect(markup).toContain("Finished");
    expect(markup).toContain("15 tokens");
    expect(markup).not.toContain("Imported from");
  });

  it("does not read a thread that switched harness as imported", () => {
    const divider: StreamTurn = {
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
    const markup = renderToStaticMarkup(
      <ThreadReceipt summary={summary} turns={[turn(false), divider]} />,
    );
    expect(markup).not.toContain("Imported from");
    expect(markup).toContain("Finished");
  });

  it("names the import source after the thread switched harness", () => {
    const divider: StreamTurn = {
      id: "switch:codex:claude_code",
      startedAt: "2026-11-18T09:10:00Z",
      status: "completed",
      items: [
        {
          kind: "harness_switch",
          itemId: "switch:codex:claude_code",
          from: "codex",
          to: "claude_code",
        },
      ],
      usage: null,
      changes: null,
      divider: true,
    };
    const markup = renderToStaticMarkup(
      <ThreadReceipt
        summary={{ ...summary, harness: "codex", imported_from: "claude_code" }}
        turns={[turn(true), divider]}
      />,
    );
    expect(markup).toContain("Imported from Claude Code");
    expect(markup).not.toContain("Imported from Codex");
  });

  it("stays out of a thread that is not done", () => {
    expect(
      renderToStaticMarkup(
        <ThreadReceipt
          summary={{ ...summary, tab: "working", agent_state: "thinking" }}
          turns={[turn(true)]}
        />,
      ),
    ).toBe("");
  });
});

describe("Harness switch divider", () => {
  it("names both harnesses in the transcript", () => {
    const divider: StreamTurn = {
      id: "switch:claude_code:codex",
      startedAt: "2026-01-01T00:00:00.000Z",
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
    const markup = renderToStaticMarkup(
      <TurnBlock
        turn={divider}
        turns={[divider]}
        threadId="thread_1"
        summary={summary}
        onReply={() => undefined}
      />,
    );
    expect(markup).toContain("Switched from Claude Code to Codex");
    expect(markup).not.toContain("q-ag-head");
  });
});

describe("turn navigation", () => {
  // Three real turns with a switch divider and a user cue between them.
  const real = [0, 2, 4];

  it("steps between real turns instead of stopping on a divider or a cue", () => {
    expect(nextTurnIndex(real, 2, 1)).toBe(4);
    expect(nextTurnIndex(real, 0, 1)).toBe(2);
    expect(nextTurnIndex(real, 4, -1)).toBe(2);
    expect(nextTurnIndex(real, 2, -1)).toBe(0);
  });

  it("skips a divider the tail follow landed on", () => {
    // The newest row is the divider (index 5); Previous Turn must reach the
    // turn before it, and Next Turn must not stay on the divider either.
    expect(nextTurnIndex(real, 5, -1)).toBe(4);
    expect(nextTurnIndex(real, 5, 1)).toBe(4);
    expect(nextTurnIndex(real, 3, -1)).toBe(2);
  });

  it("stays on the end turns rather than walking onto a synthetic row", () => {
    expect(nextTurnIndex(real, 0, -1)).toBe(0);
    expect(nextTurnIndex(real, 4, 1)).toBe(4);
    expect(nextTurnIndex([], 3, 1)).toBe(3);
  });
});
