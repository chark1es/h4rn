import { describe, expect, it } from "vitest";

import type { ToolCall, Usage } from "@/gen";

import {
  createThreadLog,
  foldEvent,
  isRealTurn,
  latestRealTurn,
  type StreamTurn,
  totalTokens,
} from "./projection";

const USAGE: Usage = {
  input_tokens: 100,
  output_tokens: 50,
  cached_input_tokens: 10,
  cost_micros: 1200,
  duration_ms: 900,
  generation_ms: 900,
  tps: 0,
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
  it("keeps a guest message out of the transcript until the owner sends it", () => {
    let log = createThreadLog();
    log = foldEvent(log, {
      type: "guest_requested",
      thread_id: "t1",
      request: {
        request_id: "greq_1",
        thread_id: "t1",
        text: "look at this",
        attachments: [{ name: "notes.txt", mime: "text/plain", size: 12 }],
        created_at: "2026-01-01T00:00:00.000Z",
      },
    });
    expect(log.turns).toHaveLength(0);
    expect(log.guestRequests).toHaveLength(1);
    expect(log.guestRequests[0]?.attachments[0]?.name).toBe("notes.txt");

    log = foldEvent(log, {
      type: "guest_request_resolved",
      thread_id: "t1",
      request_id: "greq_1",
      sent: true,
    });
    expect(log.guestRequests).toHaveLength(0);
  });

  it("removes only the guest card whose request resolved", () => {
    let log = createThreadLog();
    for (const id of ["greq_a", "greq_b"]) {
      log = foldEvent(log, {
        type: "guest_requested",
        thread_id: "t1",
        request: {
          request_id: id,
          thread_id: "t1",
          text: id,
          attachments: [],
          created_at: "2026-01-01T00:00:00.000Z",
        },
      });
    }
    log = foldEvent(log, {
      type: "guest_request_resolved",
      thread_id: "t1",
      request_id: "greq_b",
      sent: false,
    });
    expect(log.guestRequests.map((request) => request.request_id)).toEqual(["greq_a"]);
  });

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

  it("puts a harness switch between the turns as a quiet divider row", () => {
    let log = createThreadLog();
    log = foldEvent(log, {
      type: "turn_started",
      thread_id: "t1",
      turn_id: "turn_1",
      started_at: "2026-01-01T00:00:00.000Z",
      harness: "claude_code",
    });
    log = foldEvent(log, {
      type: "harness_switched",
      thread_id: "t1",
      from: "claude_code",
      to: "codex",
    });
    log = foldEvent(log, {
      type: "turn_started",
      thread_id: "t1",
      turn_id: "turn_2",
      started_at: "2026-01-01T00:01:00.000Z",
      harness: "codex",
    });
    expect(log.turns[0]?.id).toBe("turn_1");
    const divider = log.turns.find((turn) => turn.divider);
    expect(divider?.items).toMatchObject([
      { kind: "harness_switch", from: "claude_code", to: "codex" },
    ]);
    expect(log.turns.find((turn) => turn.id === "turn_1")?.harness).toBe("claude_code");
    expect(log.turns.find((turn) => turn.id === "turn_2")?.harness).toBe("codex");
  });

  it("keeps an older turn's real harness when its own event predates the field", () => {
    // The daemon's normal order: a turn journaled before the upgrade carries
    // no harness on its `turn_started`; the switch that follows names where the
    // thread came from.
    let log = createThreadLog();
    log = foldEvent(log, {
      type: "turn_started",
      thread_id: "t1",
      turn_id: "turn_1",
      started_at: "2026-01-01T00:00:00.000Z",
    });
    log = foldEvent(log, {
      type: "turn_completed",
      thread_id: "t1",
      turn_id: "turn_1",
      status: "completed",
      usage: {
        input_tokens: 0,
        output_tokens: 0,
        cached_input_tokens: 0,
        cost_micros: 0,
        duration_ms: 0,
        generation_ms: 0,
        tps: 0,
      },
    });
    log = foldEvent(log, {
      type: "harness_switched",
      thread_id: "t1",
      from: "claude_code",
      to: "codex",
    });
    log = foldEvent(log, {
      type: "turn_started",
      thread_id: "t1",
      turn_id: "turn_2",
      started_at: "2026-01-01T00:02:00.000Z",
      harness: "codex",
    });
    expect(log.turns.find((turn) => turn.id === "turn_1")?.harness).toBe("claude_code");
    expect(log.turns.find((turn) => turn.id === "turn_2")?.harness).toBe("codex");
  });

  it("does not mark the switch divider as an imported turn", () => {
    let log = createThreadLog();
    log = foldEvent(log, {
      type: "harness_switched",
      thread_id: "t1",
      from: "claude_code",
      to: "codex",
    });
    const divider = log.turns.find((turn) => turn.divider);
    expect(divider?.imported).toBeUndefined();
  });

  it("keeps one plan item per turn, updated in place", () => {
    let log = createThreadLog();
    log = foldEvent(log, {
      type: "plan",
      thread_id: "t1",
      turn_id: "turn_1",
      entries: [
        { content: "Inspect", status: "completed", priority: "high" },
        { content: "Implement", status: "in_progress", priority: "medium" },
      ],
    });
    log = foldEvent(log, {
      type: "plan",
      thread_id: "t1",
      turn_id: "turn_1",
      entries: [{ content: "Inspect", status: "completed", priority: "high" }],
    });

    const items = log.turns[0]?.items ?? [];
    expect(items).toHaveLength(1);
    expect(items[0]).toMatchObject({
      kind: "plan",
      entries: [{ content: "Inspect", status: "completed" }],
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
        attachments: [],
      },
    });

    expect(log.turns[0]?.items[0]).toMatchObject({
      kind: "message",
      text: "final text",
      streaming: false,
    });
  });

  it("keeps an imported message's own time from the event", () => {
    const log = foldEvent(createThreadLog(), {
      type: "item_completed",
      thread_id: "t1",
      turn_id: "turn_imported",
      at: "2026-11-18T09:00:00Z",
      item: {
        kind: "message",
        item_id: "item_imported",
        role: "user",
        text: "an imported prompt",
        attachments: [],
      },
    });

    expect(log.turns[0]?.startedAt).toBe("2026-11-18T09:00:00Z");
    expect(log.turns[0]?.imported).toBe(true);
  });

  it("settles an imported assistant turn instead of leaving it live", () => {
    const log = foldEvent(createThreadLog(), {
      type: "item_completed",
      thread_id: "t1",
      turn_id: "turn_imported",
      at: "2026-11-18T09:01:00Z",
      item: {
        kind: "message",
        item_id: "item_imported",
        role: "assistant",
        text: "the imported reply",
        attachments: [],
      },
    });

    expect(log.turns[0]).toMatchObject({
      id: "turn_imported",
      status: "completed",
      imported: true,
      startedAt: "2026-11-18T09:01:00Z",
      items: [{ kind: "message", role: "assistant", streaming: false }],
    });
  });

  it("leaves a live message's turn running without an imported mark", () => {
    const log = foldEvent(createThreadLog(), {
      type: "item_completed",
      thread_id: "t1",
      turn_id: "turn_live",
      item: {
        kind: "message",
        item_id: "item_live",
        role: "assistant",
        text: "a live reply",
        attachments: [],
      },
    });

    expect(log.turns[0]?.status).toBe("running");
    expect(log.turns[0]?.imported).toBeUndefined();
  });

  it("replaces a matching pending message with the server's user message", () => {
    const log = createThreadLog();
    log.pending.push({
      id: "local_1",
      text: "ship it",
      attachments: [],
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
        attachments: [],
      },
    });

    expect(next.pending).toHaveLength(0);
    expect(next.turns.at(-1)).toMatchObject({
      id: "cue:user:server_1",
      // The cue names the daemon turn it belongs to, so a fork can wait for
      // that turn to settle.
      forkTurnId: "turn_1",
      items: [{ kind: "message", role: "user", text: "ship it" }],
    });
  });

  it("attaches a replayed error row to the turn it followed", () => {
    let log = createThreadLog();
    log = foldEvent(log, {
      type: "item_completed",
      thread_id: "t1",
      turn_id: "turn_1",
      item: {
        kind: "message",
        item_id: "server_1",
        role: "assistant",
        text: "done",
        attachments: [],
      },
      at: "2026-01-01T00:00:00.000Z",
    });
    log = foldEvent(log, {
      type: "error",
      thread_id: "t1",
      message: "the transport closed",
      hint: null,
      retryable: true,
    });

    const turn = log.turns.find((candidate) => candidate.id === "turn_1");
    expect(turn?.items.at(-1)).toMatchObject({
      kind: "error",
      message: "the transport closed",
    });
  });

  it("attaches a replayed limit row to the turn it followed", () => {
    let log = createThreadLog();
    log = foldEvent(log, {
      type: "item_completed",
      thread_id: "t1",
      turn_id: "turn_1",
      item: {
        kind: "message",
        item_id: "server_1",
        role: "assistant",
        text: "done",
        attachments: [],
      },
      at: "2026-01-01T00:00:00.000Z",
    });
    log = foldEvent(log, {
      type: "usage_limit_hit",
      thread_id: "t1",
      reset_at: "2026-01-01T01:00:00.000Z",
      auto_resume: false,
    });

    const turn = log.turns.find((candidate) => candidate.id === "turn_1");
    expect(turn?.items.at(-1)).toMatchObject({
      kind: "limit",
      resetAt: "2026-01-01T01:00:00.000Z",
    });
  });

  it("folds a fork's wire so a prompt sits above its reply and a limit stays in its turn", () => {
    // The exact order a fork journals: the prompt before `TurnStarted` (as a
    // live send does), the reply, then the settlement row, then completion.
    const at = "2026-01-01T00:00:00.000Z";
    let log = createThreadLog();
    log = foldEvent(log, {
      type: "item_completed",
      thread_id: "t1",
      turn_id: "turn_1",
      item: {
        kind: "message",
        item_id: "item_user",
        role: "user",
        text: "the prompt",
        attachments: [],
      },
      at,
    });
    log = foldEvent(log, {
      type: "turn_started",
      thread_id: "t1",
      turn_id: "turn_1",
      started_at: at,
    });
    log = foldEvent(log, {
      type: "item_completed",
      thread_id: "t1",
      turn_id: "turn_1",
      item: {
        kind: "message",
        item_id: "item_reply",
        role: "assistant",
        text: "the reply",
        attachments: [],
      },
      at,
    });
    log = foldEvent(log, {
      type: "usage_limit_hit",
      thread_id: "t1",
      reset_at: null,
      auto_resume: false,
    });
    log = foldEvent(log, {
      type: "turn_completed",
      thread_id: "t1",
      turn_id: "turn_1",
      usage: USAGE,
      status: "completed",
    });

    expect(log.turns.map((turn) => turn.id)).toEqual(["cue:user:item_user", "turn_1"]);
    expect(log.turns[0]?.items.map((item) => item.kind)).toEqual(["message"]);
    expect(log.turns[1]?.items.map((item) => item.kind)).toEqual(["message", "limit"]);
    expect(log.turns[1]?.items.at(-1)).toMatchObject({ kind: "limit" });
    expect(log.turns[1]?.imported).toBe(true);
  });

  it("keeps a replayed error off a prompt cue after the turn settled", () => {
    // A queued prompt's cue can be the newest row while the turn that already
    // settled is the one an error belongs to. The cue must not take the row.
    const at = "2026-01-01T00:00:00.000Z";
    let log = createThreadLog();
    log = foldEvent(log, {
      type: "item_completed",
      thread_id: "t1",
      turn_id: "turn_1",
      item: {
        kind: "message",
        item_id: "item_user",
        role: "user",
        text: "the prompt",
        attachments: [],
      },
    });
    log = foldEvent(log, {
      type: "turn_started",
      thread_id: "t1",
      turn_id: "turn_1",
      started_at: at,
    });
    log = foldEvent(log, {
      type: "item_completed",
      thread_id: "t1",
      turn_id: "turn_1",
      item: {
        kind: "message",
        item_id: "item_reply",
        role: "assistant",
        text: "the reply",
        attachments: [],
      },
    });
    log = foldEvent(log, {
      type: "turn_completed",
      thread_id: "t1",
      turn_id: "turn_1",
      usage: USAGE,
      status: "completed",
    });
    // The person queued another prompt; its cue is now the newest row, but its
    // turn has not started.
    log = foldEvent(log, {
      type: "item_completed",
      thread_id: "t1",
      turn_id: "turn_2",
      item: {
        kind: "message",
        item_id: "item_queued",
        role: "user",
        text: "the queued prompt",
        attachments: [],
      },
    });
    log = foldEvent(log, {
      type: "error",
      thread_id: "t1",
      message: "the transport closed",
      hint: null,
      retryable: true,
    });

    const cue = log.turns.find((turn) => turn.id === "cue:user:item_queued");
    expect(cue?.items.every((item) => item.kind === "message")).toBe(true);
    const daemon = log.turns.find((turn) => turn.id === "turn_1");
    expect(daemon?.items.at(-1)).toMatchObject({
      kind: "error",
      message: "the transport closed",
    });
  });

  it("marks the daemon row of a copied prompt-only turn imported", () => {
    // A copied turn with no reply has no item to carry the copied marker, so
    // its empty daemon row must learn it from the cue. Otherwise the fork
    // claims a zero-token "Finished" receipt.
    const at = "2026-01-01T00:00:00.000Z";
    let log = createThreadLog();
    log = foldEvent(log, {
      type: "item_completed",
      thread_id: "t1",
      turn_id: "turn_1",
      item: {
        kind: "message",
        item_id: "item_user",
        role: "user",
        text: "just this",
        attachments: [],
      },
      at,
    });
    log = foldEvent(log, {
      type: "turn_started",
      thread_id: "t1",
      turn_id: "turn_1",
      started_at: at,
    });
    log = foldEvent(log, {
      type: "turn_completed",
      thread_id: "t1",
      turn_id: "turn_1",
      usage: USAGE,
      status: "completed",
    });

    const daemon = log.turns.find((turn) => turn.id === "turn_1");
    expect(daemon?.items).toEqual([]);
    expect(daemon?.imported).toBe(true);
    expect(log.turns.every((turn) => turn.imported)).toBe(true);
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
        attachments: [],
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

  it("keeps a call's stats when a later update leaves them out", () => {
    let log = createThreadLog();
    log = foldEvent(log, {
      type: "tool_call",
      thread_id: "t1",
      turn_id: "turn_1",
      tool_call: {
        ...TOOL,
        name: "edit",
        status: "running",
        stats: { kind: "diff", added: 12, removed: 3 },
      },
    });
    log = foldEvent(log, {
      type: "tool_call",
      thread_id: "t1",
      turn_id: "turn_1",
      tool_call: { ...TOOL, name: "edit", status: "completed" },
    });
    expect(log.turns[0]?.items[0]).toMatchObject({
      kind: "tool",
      toolCall: { status: "completed", stats: { kind: "diff", added: 12, removed: 3 } },
    });
  });

  it("lets a newer count replace an older one", () => {
    let log = createThreadLog();
    for (const count of [2, 5]) {
      log = foldEvent(log, {
        type: "tool_call",
        thread_id: "t1",
        turn_id: "turn_1",
        tool_call: { ...TOOL, name: "Grep", stats: { kind: "matches", count, unit: "matches" } },
      });
    }
    expect(log.turns[0]?.items[0]).toMatchObject({
      toolCall: { stats: { kind: "matches", count: 5 } },
    });
  });

  it("records a delegated item and follows its subagent updates", () => {
    let log = foldEvent(createThreadLog(), {
      type: "item_completed",
      thread_id: "t1",
      turn_id: "turn_1",
      item: {
        kind: "delegated",
        item_id: "delegated_sa_1",
        subagent_id: "sa_1",
        thread_id: null,
        task: "Reply with exactly: pong",
        harness: "codex",
        host: "host_a",
        model: "gpt-5",
        state: "starting",
        agent_state: "thinking",
      },
    });
    log = foldEvent(log, {
      type: "subagent_updated",
      parent_thread_id: "t1",
      subagent: {
        subagent_id: "sa_1",
        parent_thread_id: "t1",
        child_thread_id: "thr_child",
        task: "Reply with exactly: pong",
        harness: "codex",
        host: "host_a",
        model: "gpt-5",
        isolation: "shared",
        workspace_path: "/work/h4rn",
        branch: null,
        depth: 1,
        state: "running",
        agent_state: "thinking",
        progress: {
          turns_started: 1,
          turns_completed: 0,
          tokens: 0,
          elapsed_ms: 12,
          current: null,
        },
        budget: { max_turns: 20, max_wall_time_secs: 900, max_tokens: 50000 },
        context: { kind: "none" },
        credential: { label: "shared", shared_with_parent: true },
        created_at: "2026-01-01T00:00:00.000Z",
        started_at: "2026-01-01T00:00:01.000Z",
        finished_at: null,
        result: null,
      },
    });

    expect(log.turns[0]?.items).toHaveLength(1);
    expect(log.turns[0]?.items[0]).toMatchObject({
      kind: "delegated",
      subagentId: "sa_1",
      threadId: "thr_child",
      state: "running",
    });
  });

  it("records an approval request and its pending state", () => {
    const log = foldEvent(createThreadLog(), {
      type: "approval_requested",
      thread_id: "t1",
      turn_id: "turn_1",
      approval_id: "appr_1",
      subagent_id: null,
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
      subagent_id: null,
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

  it("refuses to name an owner for an older journal's queued prompt", () => {
    // An older journal filed B's prompt under A's turn while A was running.
    // B's real turn is the next one that starts, which the event does not name,
    // so the row offers no fork instead of pointing at A.
    let log = foldEvent(createThreadLog(), {
      type: "item_completed",
      thread_id: "t1",
      turn_id: "turn_a",
      item: {
        kind: "message",
        item_id: "a_prompt",
        role: "user",
        text: "A prompt",
        attachments: [],
      },
    });
    log = foldEvent(log, {
      type: "turn_started",
      thread_id: "t1",
      turn_id: "turn_a",
      started_at: "2026-01-01T00:00:00.000Z",
    });
    log = foldEvent(log, {
      type: "item_completed",
      thread_id: "t1",
      turn_id: "turn_a",
      item: {
        kind: "message",
        item_id: "b_prompt",
        role: "user",
        text: "B lost",
        attachments: [],
      },
    });

    const bCue = log.turns.find((turn) => turn.id === "cue:user:b_prompt");
    expect(bCue?.forkTurnId).toBeUndefined();
    const aCue = log.turns.find((turn) => turn.id === "cue:user:a_prompt");
    expect(aCue?.forkTurnId).toBe("turn_a");
  });

  it("keeps a fork's promoted prompts above the earlier turn's reply", () => {
    // The fork's own wire sequence for A running with B and C queued behind it.
    // The source showed C's prompt before B's reply, and the rows have to keep
    // that order.
    const wire = [
      { turn: "turn_a", kind: "prompt", text: "A prompt" },
      { turn: "turn_a", kind: "started" },
      { turn: "turn_b", kind: "prompt", text: "B prompt" },
      { turn: "turn_c", kind: "prompt", text: "C prompt" },
      { turn: "turn_a", kind: "reply", text: "echo: A prompt" },
      { turn: "turn_a", kind: "completed" },
      { turn: "turn_b", kind: "started" },
      { turn: "turn_b", kind: "reply", text: "echo: B prompt" },
      { turn: "turn_b", kind: "completed" },
      { turn: "turn_c", kind: "started" },
      { turn: "turn_c", kind: "reply", text: "echo: C prompt" },
      { turn: "turn_c", kind: "completed" },
    ] as const;
    let log = createThreadLog();
    for (const row of wire) {
      if (row.kind === "prompt") {
        log = foldEvent(log, {
          type: "item_completed",
          thread_id: "t1",
          turn_id: row.turn,
          at: "2026-01-01T00:00:00.000Z",
          item: {
            kind: "message",
            item_id: `prompt_${row.turn}`,
            role: "user",
            text: row.text,
            attachments: [],
          },
        });
      } else if (row.kind === "reply") {
        log = foldEvent(log, {
          type: "item_completed",
          thread_id: "t1",
          turn_id: row.turn,
          at: "2026-01-01T00:00:01.000Z",
          item: {
            kind: "message",
            item_id: `reply_${row.turn}`,
            role: "assistant",
            text: row.text,
            attachments: [],
          },
        });
      } else if (row.kind === "started") {
        log = foldEvent(log, {
          type: "turn_started",
          thread_id: "t1",
          turn_id: row.turn,
          started_at: "2026-01-01T00:00:00.000Z",
        });
      } else {
        log = foldEvent(log, {
          type: "turn_completed",
          thread_id: "t1",
          turn_id: row.turn,
          status: "completed",
          usage: USAGE,
        });
      }
    }

    const ids = log.turns.map((turn) => turn.id);
    expect(ids).toEqual([
      "cue:user:prompt_turn_a",
      "turn_a",
      "cue:user:prompt_turn_b",
      "cue:user:prompt_turn_c",
      "turn_b",
      "turn_c",
    ]);
    expect(
      log.turns[1]?.items.map((item) => (item.kind === "message" ? item.text : item.kind)),
    ).toEqual(["echo: A prompt"]);
    expect(
      log.turns[4]?.items.map((item) => (item.kind === "message" ? item.text : item.kind)),
    ).toEqual(["echo: B prompt"]);
  });
});

describe("latestRealTurn", () => {
  it("skips a switch divider and a user cue after the last agent turn", () => {
    const turn = (id: string, extra: Partial<StreamTurn> = {}): StreamTurn => ({
      id,
      startedAt: "2026-01-01T00:00:00.000Z",
      status: "completed",
      items: [],
      usage: null,
      changes: null,
      ...extra,
    });
    const turns = [
      turn("turn_a"),
      turn("switch:claude_code:codex", { divider: true }),
      turn("cue:user:item_1"),
    ];

    expect(latestRealTurn(turns)?.id).toBe("turn_a");
    expect(isRealTurn(turns[1] as StreamTurn)).toBe(false);
    expect(isRealTurn(turns[2] as StreamTurn)).toBe(false);
  });
});

describe("totalTokens", () => {
  it("sums prompt and completion tokens", () => {
    expect(totalTokens(USAGE)).toBe(150);
  });
});
describe("thread_rewound", () => {
  it("drops the edited message and every later turn but keeps earlier ones", () => {
    let log = createThreadLog();
    for (const [index, text] of ["one", "two", "three"].entries()) {
      log = foldEvent(log, {
        type: "item_completed",
        thread_id: "t1",
        turn_id: `turn_${index}`,
        item: {
          kind: "message",
          item_id: `item_user_${index}`,
          role: "user",
          text,
          attachments: [],
        },
      });
      log = foldEvent(log, {
        type: "item_completed",
        thread_id: "t1",
        turn_id: `turn_${index}`,
        item: {
          kind: "message",
          item_id: `assistant_${index}`,
          role: "assistant",
          text: `answer ${index}`,
          attachments: [],
        },
      });
    }
    expect(log.turns).toHaveLength(6);

    log = foldEvent(log, {
      type: "thread_rewound",
      thread_id: "t1",
      before_item_id: "item_user_1",
      files_restored: false,
      at: "2026-01-01T00:00:01.000Z",
    });

    const items = log.turns.flatMap((turn) => turn.items.map((item) => item.itemId));
    expect(items).toEqual(["item_user_0", "assistant_0"]);
  });

  it("leaves a log that never held the message alone", () => {
    const log = foldEvent(createThreadLog(), {
      type: "thread_rewound",
      thread_id: "t1",
      before_item_id: "missing",
      files_restored: true,
      at: "2026-01-01T00:00:00.000Z",
    });
    expect(log.turns).toHaveLength(0);
  });
});
