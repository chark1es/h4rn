import { describe, expect, it } from "vitest";

import type { MessageRole } from "@/gen";
import type { StreamTurn, ThreadLog, TurnItem } from "@/lib/store/projection";
import { caretOnFirstLine, sentPrompts, stepPromptHistory } from "./composerHistory";

function message(role: MessageRole, text: string): TurnItem {
  return {
    kind: "message",
    itemId: `${role}:${text}`,
    role,
    text,
    attachments: [],
    streaming: false,
  };
}

function turn(id: string, items: TurnItem[]): StreamTurn {
  return {
    id,
    startedAt: "2026-01-01T12:00:00.000Z",
    status: "completed",
    items,
    usage: null,
    changes: null,
  };
}

function log(turns: StreamTurn[], pendingText: string[] = []): ThreadLog {
  return {
    turns,
    nextSeq: 0,
    loaded: true,
    loading: false,
    approvals: {},
    guestRequests: [],
    pending: pendingText.map((text, index) => ({
      id: `pending-${index}`,
      text,
      attachments: [],
      at: "2026-01-01T12:00:00.000Z",
      sending: true,
    })),
    cutTurns: [],
  };
}

describe("sentPrompts", () => {
  it("reads user messages in order and skips assistant text and blanks", () => {
    const thread = log([
      turn("1", [message("user", "first prompt"), message("assistant", "answer")]),
      turn("2", [message("user", "  "), message("user", "second prompt")]),
    ]);

    expect(sentPrompts(thread)).toEqual(["first prompt", "second prompt"]);
  });

  it("includes messages still pending and collapses immediate repeats", () => {
    const thread = log(
      [turn("1", [message("user", "deploy")]), turn("2", [message("user", "deploy")])],
      ["deploy", "and verify"],
    );

    expect(sentPrompts(thread)).toEqual(["deploy", "and verify"]);
  });

  it("returns nothing without a log", () => {
    expect(sentPrompts(undefined)).toEqual([]);
    expect(sentPrompts(null)).toEqual([]);
  });
});

describe("stepPromptHistory", () => {
  const prompts = ["one", "two", "three"];

  it("saves the in-progress text on the first step back", () => {
    const step = stepPromptHistory({
      direction: "backward",
      prompts,
      state: { index: -1, draft: "" },
      current: "half-typed",
    });

    expect(step).toEqual({ index: 2, draft: "half-typed", text: "three" });
  });

  it("walks back to the oldest prompt and stops there", () => {
    const first = stepPromptHistory({
      direction: "backward",
      prompts,
      state: { index: 2, draft: "half-typed" },
      current: "three",
    });
    expect(first).toEqual({ index: 1, draft: "half-typed", text: "two" });
    if (!first) throw new Error("expected a prompt");

    const second = stepPromptHistory({
      direction: "backward",
      prompts,
      state: { index: first.index, draft: first.draft },
      current: "two",
    });
    expect(second).toEqual({ index: 0, draft: "half-typed", text: "one" });
    if (!second) throw new Error("expected a prompt");

    expect(
      stepPromptHistory({
        direction: "backward",
        prompts,
        state: { index: second.index, draft: second.draft },
        current: "one",
      }),
    ).toBeNull();
  });

  it("walks forward and finally restores the in-progress text", () => {
    let step = stepPromptHistory({
      direction: "forward",
      prompts,
      state: { index: 0, draft: "half-typed" },
      current: "one",
    });
    expect(step).toEqual({ index: 1, draft: "half-typed", text: "two" });

    step = stepPromptHistory({
      direction: "forward",
      prompts,
      state: { index: 2, draft: "half-typed" },
      current: "three",
    });
    expect(step).toEqual({ index: -1, draft: "", text: "half-typed" });
  });

  it("does nothing without prompts or outside history mode", () => {
    expect(
      stepPromptHistory({
        direction: "backward",
        prompts: [],
        state: { index: -1, draft: "" },
        current: "text",
      }),
    ).toBeNull();
    expect(
      stepPromptHistory({
        direction: "forward",
        prompts,
        state: { index: -1, draft: "" },
        current: "text",
      }),
    ).toBeNull();
  });
});

describe("caretOnFirstLine", () => {
  it("accepts a caret on the first line with no selection", () => {
    expect(caretOnFirstLine("hello", 0, 0)).toBe(true);
    expect(caretOnFirstLine("hello", 3, 3)).toBe(true);
    expect(caretOnFirstLine("hello\nworld", 4, 4)).toBe(true);
    expect(caretOnFirstLine("", 0, 0)).toBe(true);
  });

  it("rejects a caret past the first newline", () => {
    expect(caretOnFirstLine("hello\nworld", 5, 5)).toBe(true);
    expect(caretOnFirstLine("hello\nworld", 6, 6)).toBe(false);
    expect(caretOnFirstLine("hello\nworld", 8, 8)).toBe(false);
    expect(caretOnFirstLine("\n", 1, 1)).toBe(false);
  });

  it("rejects any selection", () => {
    expect(caretOnFirstLine("hello", 1, 3)).toBe(false);
  });
});
