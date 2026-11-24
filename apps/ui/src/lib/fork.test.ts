import { describe, expect, it } from "vitest";

import type { StreamTurn } from "@/lib/store/projection";
import { forkableTurn, latestForkableTurn } from "./fork";

function turn(input: Partial<StreamTurn> & { id: string }): StreamTurn {
  return {
    startedAt: "2026-01-01T00:00:00Z",
    status: "completed",
    items: [
      {
        kind: "message",
        itemId: `${input.id}:item`,
        role: "assistant",
        text: "done",
        attachments: [],
        streaming: false,
      },
    ],
    usage: null,
    changes: null,
    ...input,
  };
}

describe("forkableTurn", () => {
  it("names a finished daemon turn", () => {
    expect(forkableTurn(turn({ id: "turn_1" }))).toBe("turn_1");
  });

  it("refuses a running turn", () => {
    expect(forkableTurn(turn({ id: "turn_1", status: "running" }))).toBeNull();
  });

  it("uses the real turn id behind an imported user cue", () => {
    expect(
      forkableTurn(turn({ id: "cue:user:item_1", imported: true, forkTurnId: "turn_imported" })),
    ).toBe("turn_imported");
  });

  it("does not offer a live user cue", () => {
    expect(forkableTurn(turn({ id: "cue:user:item_1" }))).toBeNull();
  });

  it("does not offer a turn without content", () => {
    expect(forkableTurn(turn({ id: "turn_1", items: [] }))).toBeNull();
  });
});

describe("latestForkableTurn", () => {
  it("walks back past a running newest turn", () => {
    const turns = [turn({ id: "turn_1" }), turn({ id: "turn_2", status: "running" })];
    expect(latestForkableTurn(turns)).toBe("turn_1");
  });

  it("returns null when nothing is finished", () => {
    expect(latestForkableTurn([turn({ id: "turn_1", status: "running" })])).toBeNull();
    expect(latestForkableTurn([])).toBeNull();
  });
});
