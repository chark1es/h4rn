// @vitest-environment jsdom

import { describe, expect, it } from "vitest";

import type { QueuedMessage, ThreadSummary } from "@/gen";
import { filesRewindAvailable, rewindRefusal } from "./store/appStore";
import { createThreadLog, foldEvent, type ThreadLog } from "./store/projection";

function summary(overrides: Partial<ThreadSummary> = {}): ThreadSummary {
  return {
    thread_id: "t1",
    parent_thread_id: null,
    host: "host_a",
    harness: "claude_code",
    title: "Thread",
    workspace_path: "/work",
    model: "sonnet",
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
    steer: false,
    rewind: "handoff",
    file_rewind: true,
    ...overrides,
  };
}

describe("rewindRefusal", () => {
  it("allows a finished thread", () => {
    expect(rewindRefusal(summary(), createThreadLog())).toBeNull();
  });

  it("refuses a running, queued or approval-bound thread", () => {
    expect(rewindRefusal(summary({ agent_state: "thinking" }), undefined)).toContain(
      "current turn",
    );
    expect(rewindRefusal(summary({ agent_state: "queued" }), undefined)).toContain("waiting");
    expect(rewindRefusal(summary({ agent_state: "awaiting_approval" }), undefined)).toContain(
      "approval",
    );
  });

  it("refuses an armed resume and a harness that cannot rewind", () => {
    expect(rewindRefusal(summary({ agent_state: "scheduled" }), undefined)).toContain(
      "automatic resume",
    );
    expect(rewindRefusal(summary({ rewind: "unsupported" }), undefined)).toContain(
      "cannot drop turns",
    );
  });

  it("names an uncertain delivery as something to resend or discard", () => {
    const queued: QueuedMessage[] = [
      {
        message_id: "qm_uncertain",
        text: "maybe delivered",
        attachments: [],
        queued_at: "2026-01-01T00:00:00Z",
        steerable: false,
        delivery_unknown: true,
      },
    ];
    expect(rewindRefusal(summary(), createThreadLog(), queued)).toContain(
      "Resend or discard it first",
    );
  });

  it("finds a pending approval in the transcript even before the summary catches up", () => {
    const log = createThreadLog();
    log.turns.push({
      id: "turn_1",
      startedAt: "2026-01-01T00:00:00Z",
      status: "running",
      items: [
        {
          kind: "approval",
          itemId: "approval_1",
          approvalId: "approval_1",
          approvalKind: "command",
          prompt: "run it",
          options: [],
        },
      ],
      usage: null,
      changes: null,
    });
    expect(rewindRefusal(summary(), log)).toContain("approval");
  });
});

const usage = {
  input_tokens: 0,
  output_tokens: 0,
  cached_input_tokens: 0,
  cost_micros: 0,
  duration_ms: 0,
  generation_ms: 0,
  tps: 0,
};

/** One daemon turn folded the way the live stream delivers it. */
function foldTurn(log: ThreadLog, turn: string, item: string, complete: boolean | null) {
  let next = foldEvent(log, {
    type: "turn_started",
    thread_id: "t1",
    turn_id: turn,
    started_at: "2026-01-01T00:00:00.000Z",
  });
  next = foldEvent(next, {
    type: "item_completed",
    thread_id: "t1",
    turn_id: turn,
    item: { kind: "message", item_id: item, role: "user", text: `prompt ${turn}`, attachments: [] },
  });
  return foldEvent(next, {
    type: "turn_completed",
    thread_id: "t1",
    turn_id: turn,
    status: "completed",
    usage,
    ...(complete === null
      ? {}
      : { changes: { thread_id: "t1", turn_id: turn, files: [], complete, note: null } }),
  });
}

describe("filesRewindAvailable", () => {
  it("reads the removed daemon turns, not the message's own cue row", () => {
    let log = createThreadLog();
    log = foldTurn(log, "turn_1", "item_user_1", true);
    log = foldTurn(log, "turn_2", "item_user_2", true);
    // The row that holds the message is a cue with `changes: null`; it must
    // not decide whether the turns behind it have a checkpoint.
    expect(log.turns.find((turn) => turn.id === "cue:user:item_user_1")?.changes).toBeNull();
    expect(filesRewindAvailable(log, "item_user_1")).toBe(true);
    expect(filesRewindAvailable(log, "item_user_2")).toBe(true);
  });

  it("is false when the selected message's own turn has no complete checkpoint", () => {
    let log = createThreadLog();
    log = foldTurn(log, "turn_1", "item_user_1", false);
    log = foldTurn(log, "turn_2", "item_user_2", true);
    expect(filesRewindAvailable(log, "item_user_1")).toBe(false);
    expect(filesRewindAvailable(log, "item_user_2")).toBe(true);
  });

  it("is false when a later removed turn reported no checkpoint at all", () => {
    let log = createThreadLog();
    log = foldTurn(log, "turn_1", "item_user_1", true);
    log = foldTurn(log, "turn_2", "item_user_2", null);
    expect(filesRewindAvailable(log, "item_user_1")).toBe(false);
  });
});
