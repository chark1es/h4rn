import { describe, expect, it } from "vitest";

import type { ToolCall, Usage } from "@/gen";

import { createThreadLog, foldEvent, totalTokens } from "./projection";

const USAGE: Usage = {
  input_tokens: 100,
  output_tokens: 50,
  cached_input_tokens: 10,
  cost_micros: 1200,
  duration_ms: 900,
};

const TOOL: ToolCall = {
  tool_call_id: "call_1",
  name: "Bash",
  status: "running",
  summary: "bun test",
  input: null,
  output: null,
  started_at: "2026-01-01T00:00:00.000Z",
  completed_at: null,
};

describe("foldEvent", () => {
  it("accumulates message deltas into a running turn", () => {
    let log = createThreadLog();
    log = foldEvent(log, {
      type: "message_delta",
      thread_id: "t1",
      turn_id: "turn_1",
      item_id: "item_1",
      text: "Hel",
    });
    log = foldEvent(log, {
      type: "message_delta",
      thread_id: "t1",
      turn_id: "turn_1",
      item_id: "item_1",
      text: "lo",
    });

    expect(log.turns).toHaveLength(1);
    expect(log.turns[0]).toMatchObject({
      id: "turn_1",
      status: "running",
      items: [{ kind: "message", itemId: "item_1", text: "Hello", streaming: true }],
    });
  });

  it("ignores a duplicate turn_started and keeps one row", () => {
    const start = {
      type: "turn_started" as const,
      thread_id: "t1" as const,
      turn_id: "turn_1" as const,
      started_at: "2026-01-01T00:00:00.000Z",
    };
    let log = foldEvent(createThreadLog(), start);
    log = foldEvent(log, start);

    expect(log.turns).toHaveLength(1);
  });

  it("finalizes a streamed assistant message on item_completed", () => {
    let log = createThreadLog();
    log = foldEvent(log, {
      type: "message_delta",
      thread_id: "t1",
      turn_id: "turn_1",
      item_id: "item_1",
      text: "partial",
    });
    log = foldEvent(log, {
      type: "item_completed",
      thread_id: "t1",
      turn_id: "turn_1",
      item: {
        kind: "message",
        item_id: "item_1",
        role: "assistant",
        text: "final text",
      },
    });

    expect(log.turns[0]?.items[0]).toMatchObject({
      kind: "message",
      text: "final text",
      streaming: false,
    });
  });

  it("replaces a matching pending message with the server's user message", () => {
    const log = createThreadLog();
    log.pending.push({
      id: "local_1",
      text: "ship it",
      at: "2026-01-01T00:00:00.000Z",
      sending: true,
    });

    const next = foldEvent(log, {
      type: "item_completed",
      thread_id: "t1",
      turn_id: "turn_1",
      item: {
        kind: "message",
        item_id: "server_1",
        role: "user",
        text: "ship it",
      },
    });

    expect(next.pending).toHaveLength(0);
    expect(next.turns.at(-1)).toMatchObject({
      id: "cue:user:server_1",
      items: [{ kind: "message", role: "user", text: "ship it" }],
    });
  });

  it("ignores a duplicate completed user message", () => {
    const event = {
      type: "item_completed" as const,
      thread_id: "t1" as const,
      turn_id: "turn_1" as const,
      item: {
        kind: "message" as const,
        item_id: "server_1",
        role: "user" as const,
        text: "ship it",
      },
    };
    let log = foldEvent(createThreadLog(), event);
    log = foldEvent(log, event);

    expect(log.turns).toHaveLength(1);
  });

  it("upserts tool calls in place as their status changes", () => {
    let log = createThreadLog();
    log = foldEvent(log, {
      type: "tool_call",
      thread_id: "t1",
      turn_id: "turn_1",
      tool_call: TOOL,
    });
    log = foldEvent(log, {
      type: "tool_call",
      thread_id: "t1",
      turn_id: "turn_1",
      tool_call: {
        ...TOOL,
        status: "completed",
        output: "ok",
        completed_at: "2026-01-01T00:00:01.000Z",
      },
    });

    expect(log.turns[0]?.items).toHaveLength(1);
    expect(log.turns[0]?.items[0]).toMatchObject({
      kind: "tool",
      toolCall: { status: "completed", output: "ok" },
    });
  });

  it("records an approval request and its pending state", () => {
    const log = foldEvent(createThreadLog(), {
      type: "approval_requested",
      thread_id: "t1",
      turn_id: "turn_1",
      approval_id: "appr_1",
      kind: "command",
      prompt: "Run the tests?",
      options: [{ id: "yes", label: "Go" }],
    });

    expect(log.approvals.appr_1).toEqual({ status: "pending" });
    expect(log.turns[0]?.items[0]).toMatchObject({
      kind: "approval",
      approvalId: "appr_1",
      prompt: "Run the tests?",
    });
  });

  it("resolves an approval from the event stream", () => {
    let log = foldEvent(createThreadLog(), {
      type: "approval_requested",
      thread_id: "t1",
      turn_id: "turn_1",
      approval_id: "appr_1",
      kind: "question",
      prompt: "Which branch?",
      options: [
        { id: "main", label: "main" },
        { id: "dev", label: "dev" },
      ],
    });
    log = foldEvent(log, {
      type: "approval_resolved",
      thread_id: "t1",
      approval_id: "appr_1",
      option_id: "dev",
      by_device: "device_1",
    });

    expect(log.approvals.appr_1).toEqual({ status: "resolved" });
  });

  it("closes the turn and stops streaming on turn_completed", () => {
    let log = createThreadLog();
    log = foldEvent(log, {
      type: "message_delta",
      thread_id: "t1",
      turn_id: "turn_1",
      item_id: "item_1",
      text: "done",
    });
    log = foldEvent(log, {
      type: "turn_completed",
      thread_id: "t1",
      turn_id: "turn_1",
      status: "completed",
      usage: USAGE,
    });

    expect(log.turns[0]).toMatchObject({ status: "completed", usage: USAGE });
    expect(log.turns[0]?.items[0]).toMatchObject({ streaming: false });
  });

  it("adds a usage-limit item to the running turn", () => {
    let log = createThreadLog();
    log = foldEvent(log, {
      type: "turn_started",
      thread_id: "t1",
      turn_id: "turn_1",
      started_at: "2026-01-01T00:00:00.000Z",
    });
    log = foldEvent(log, {
      type: "usage_limit_hit",
      thread_id: "t1",
      reset_at: "2026-01-01T01:00:00.000Z",
      auto_resume: true,
    });

    expect(log.turns[0]?.items[0]).toMatchObject({
      kind: "limit",
      resetAt: "2026-01-01T01:00:00.000Z",
      autoResume: true,
    });
  });

  it("ignores thread_upserted and unknown event kinds", () => {
    const log = createThreadLog();
    expect(foldEvent(log, { type: "thread_upserted" } as never)).toBe(log);
    expect(foldEvent(log, { type: "future_event" } as never)).toBe(log);
  });

  it("does not mutate the source log", () => {
    const source = createThreadLog();
    foldEvent(source, {
      type: "message_delta",
      thread_id: "t1",
      turn_id: "turn_1",
      item_id: "item_1",
      text: "hi",
    });

    expect(source.turns).toHaveLength(0);
  });
});

describe("totalTokens", () => {
  it("sums prompt and completion tokens", () => {
    expect(totalTokens(USAGE)).toBe(150);
  });
});
