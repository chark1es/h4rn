import { describe, expect, it } from "vitest";

import { isPlacementMove, noteManualPlacement, onManualPlacement } from "./placement";

describe("manual placements", () => {
  it("treats moves into Done and Settled as placements", () => {
    expect(isPlacementMove("done")).toBe(true);
    expect(isPlacementMove("settled")).toBe(true);
  });

  it("does not treat a pin toggle's move through Working as a placement", () => {
    expect(isPlacementMove("working")).toBe(false);
  });

  it("announces a placement until the listener unsubscribes", () => {
    const seen: string[] = [];
    const stop = onManualPlacement((threadId) => seen.push(threadId));

    noteManualPlacement("thr_placed");
    stop();
    noteManualPlacement("thr_ignored");

    expect(seen).toEqual(["thr_placed"]);
  });
});
