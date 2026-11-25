// @vitest-environment jsdom

import { describe, expect, it } from "vitest";

import type { ThreadSummary } from "@/gen";
import { rewindRefusal } from "./store/appStore";
import { createThreadLog } from "./store/projection";

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
