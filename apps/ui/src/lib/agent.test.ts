import { describe, expect, it } from "vitest";

import type { ThreadSummary } from "@/gen";
import { isTurnRunning } from "./agent";

function summary(overrides: Partial<ThreadSummary>): ThreadSummary {
  return {
    thread_id: "th_one",
    parent_thread_id: null,
    host: "host_ui",
    harness: "codex",
    title: "A thread",
    workspace_path: "~/code",
    model: "gpt-5.1-codex",
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
    queued_count: 0,
    queue_revision: 0,
    steer: true,
    ...overrides,
  };
}

describe("isTurnRunning", () => {
  it("follows the daemon's active turn over the agent state", () => {
    expect(isTurnRunning(summary({ agent_state: "running_tool" }))).toBe(true);
    expect(isTurnRunning(summary({ agent_state: "idle" }))).toBe(false);
  });

  it("counts a rate-limited turn the daemon still reports as active", () => {
    // A limit can pause a turn while its agent is mid-tool: the state says
    // rate_limited, but the turn is still the one a steer belongs to.
    expect(isTurnRunning(summary({ agent_state: "rate_limited" }))).toBe(false);
    expect(
      isTurnRunning(summary({ agent_state: "rate_limited", active_turn_id: "turn_running" })),
    ).toBe(true);
  });
});
