import { describe, expect, it } from "vitest";

import type { StreamTurn } from "@/lib/store/projection";
import { stoppedNotice, turnPrompt } from "./turn";

function turn(items: StreamTurn["items"]): StreamTurn {
  return {
    id: "turn_1",
    startedAt: "2026-01-01T00:00:00Z",
    status: "token_limit",
    items,
    usage: null,
    changes: null,
  };
}

describe("stoppedNotice", () => {
  it("names the limit that stopped the turn", () => {
    expect(stoppedNotice("token_limit")).toBe("Stopped: the agent hit its token limit.");
    expect(stoppedNotice("turn_limit")).toBe("Stopped: the agent hit its turn-request limit.");
  });

  it("stays quiet for every other status", () => {
    expect(stoppedNotice("completed")).toBeNull();
    expect(stoppedNotice("failed")).toBeNull();
    expect(stoppedNotice("interrupted")).toBeNull();
    expect(stoppedNotice("running")).toBeNull();
  });
});

describe("turnPrompt", () => {
  it("returns the prompt that started the turn, for a resend", () => {
    const prompt = turnPrompt(
      turn([
        {
          kind: "message",
          itemId: "item_1",
          role: "user",
          text: "ship it",
          attachments: [],
          streaming: false,
        },
        {
          kind: "message",
          itemId: "item_2",
          role: "assistant",
          text: "on it",
          streaming: false,
          attachments: [],
        },
      ]),
    );
    expect(prompt).toEqual({ text: "ship it", attachments: [] });
  });

  it("returns null when the turn has no user prompt to repeat", () => {
    expect(
      turnPrompt(
        turn([
          {
            kind: "message",
            itemId: "item_1",
            role: "assistant",
            text: "on it",
            attachments: [],
            streaming: false,
          },
        ]),
      ),
    ).toBeNull();
  });
});
