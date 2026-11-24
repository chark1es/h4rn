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

function cue(input: { id: string; turnId: string; imported?: boolean }): StreamTurn {
  const turn: StreamTurn = {
    id: `cue:user:${input.id}`,
    startedAt: "2026-01-01T00:00:00Z",
    status: "completed",
    items: [
      {
        kind: "message",
        itemId: input.id,
        role: "user",
        text: "a prompt",
        attachments: [],
        streaming: false,
      },
    ],
    usage: null,
    changes: null,
    forkTurnId: input.turnId,
  };
  if (input.imported) turn.imported = true;
  return turn;
}

describe("forkableTurn", () => {
  it("names a finished daemon turn", () => {
    const row = turn({ id: "turn_1" });
    expect(forkableTurn(row, [row])).toBe("turn_1");
  });

  it("refuses a running turn", () => {
    const row = turn({ id: "turn_1", status: "running" });
    expect(forkableTurn(row, [row])).toBeNull();
  });

  it("uses the real turn id behind a copied user cue", () => {
    const row = cue({ id: "item_1", turnId: "turn_imported", imported: true });
    expect(forkableTurn(row, [row])).toBe("turn_imported");
  });

  it("does not offer a live cue whose turn is still running", () => {
    const row = cue({ id: "item_1", turnId: "turn_1" });
    const daemon = turn({ id: "turn_1", status: "running" });
    expect(forkableTurn(row, [row, daemon])).toBeNull();
  });

  it("offers a prompt-only turn once its daemon turn settled", () => {
    // The agent answered nothing, so the turn has no items of its own; the cue
    // still names it, and its settled state is what enables the fork.
    const row = cue({ id: "item_1", turnId: "turn_1" });
    const daemon = turn({ id: "turn_1", items: [] });
    expect(forkableTurn(row, [row, daemon])).toBe("turn_1");
  });

  it("does not offer a cue whose turn has not started", () => {
    const row = cue({ id: "item_1", turnId: "turn_queued" });
    expect(forkableTurn(row, [row])).toBeNull();
  });

  it("does not offer a turn without content", () => {
    const row = turn({ id: "turn_1", items: [] });
    expect(forkableTurn(row, [row])).toBeNull();
  });
});

describe("latestForkableTurn", () => {
  it("is null while the newest turn is unsettled", () => {
    const log = [turn({ id: "turn_1" }), turn({ id: "turn_2", status: "running" })];
    expect(latestForkableTurn(log)).toBeNull();
  });

  it("names the newest turn once it settles", () => {
    const log = [turn({ id: "turn_1" }), turn({ id: "turn_2" })];
    expect(latestForkableTurn(log)).toBe("turn_2");
  });

  it("returns null when nothing is finished", () => {
    expect(latestForkableTurn([turn({ id: "turn_1", status: "running" })])).toBeNull();
    expect(latestForkableTurn([])).toBeNull();
  });

  it("resolves a prompt-only turn through its cue", () => {
    const log = [cue({ id: "item_1", turnId: "turn_1" }), turn({ id: "turn_1", items: [] })];
    expect(latestForkableTurn(log)).toBe("turn_1");
  });

  it("stops at a later prompt still waiting behind a promoted turn", () => {
    // B was promoted and its daemon row lands after C's cue, but C was sent
    // later and is still waiting; the newest logical turn is C.
    const settledA = turn({ id: "turn_a" });
    const cueB = cue({ id: "item_b", turnId: "turn_b" });
    const cueC = cue({ id: "item_c", turnId: "turn_c" });
    const daemonB = turn({ id: "turn_b" });
    expect(latestForkableTurn([settledA, cueB, cueC, daemonB])).toBeNull();
    // Once C runs and settles, it is the newest turn again.
    const daemonC = turn({ id: "turn_c" });
    expect(latestForkableTurn([settledA, cueB, cueC, daemonB, daemonC])).toBe("turn_c");
  });
});
