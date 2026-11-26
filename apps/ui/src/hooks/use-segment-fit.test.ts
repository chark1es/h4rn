import { describe, expect, it } from "vitest";

import { type SegmentItem, segmentOverflows } from "./use-segment-fit";

const tab = (label: number, count: number): SegmentItem => ({
  padding: 12,
  gap: 6,
  parts: [label, count],
});

describe("segmentOverflows", () => {
  it("fits when the tabs' natural widths fit the row", () => {
    // English at the narrowest sidebar: Working 6, Done 3, Settled 14.
    const items = [tab(52, 8), tab(33, 8), tab(46, 15)];
    expect(segmentOverflows(219, 2, items)).toBe(false);
  });

  it("overflows when longer labels need more than the row has", () => {
    // Spanish at the same width: En curso 6, Terminado 3, Resuelto 14.
    const items = [tab(56, 8), tab(68, 8), tab(58, 15)];
    expect(segmentOverflows(219, 2, items)).toBe(true);
  });

  it("allows a pixel of rounding before calling it an overflow", () => {
    const items = [tab(50, 10)];
    // 12 padding + 50 + 6 gap + 10 = 78.
    expect(segmentOverflows(78, 2, items)).toBe(false);
    expect(segmentOverflows(77, 2, items)).toBe(false);
    expect(segmentOverflows(76, 2, items)).toBe(true);
  });
});
