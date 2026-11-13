import { describe, expect, it } from "vitest";

import { collapseDiff, computeDiff, diffStats } from "./diff";

describe("IDE unified diff", () => {
  it("keeps shared lines as context and marks the change", () => {
    const lines = computeDiff("a\nb\nc\n", "a\nB\nc\n");
    expect(lines.map((line) => line.kind)).toEqual(["context", "removed", "added", "context"]);
    expect(diffStats(lines)).toEqual({ added: 1, removed: 1 });
    const added = lines.find((line) => line.kind === "added");
    expect(added?.text).toBe("B");
    expect(added?.newLine).toBe(2);
  });

  it("treats a missing side as empty rather than a blank line", () => {
    expect(diffStats(computeDiff("", "one\ntwo\n"))).toEqual({ added: 2, removed: 0 });
    expect(diffStats(computeDiff("one\ntwo\n", ""))).toEqual({ added: 0, removed: 2 });
  });

  it("collapses runs outside the change context", () => {
    const before = Array.from({ length: 30 }, (_, index) => `line ${index}`).join("\n");
    const after = before.replace("line 15", "line fifteen");
    const rows = collapseDiff(computeDiff(`${before}\n`, `${after}\n`), 2);
    expect(rows[0]).toEqual({ kind: "skip", count: 13 });
    expect(rows.some((row) => row.kind === "added")).toBe(true);
    expect(rows.at(-1)).toEqual({ kind: "skip", count: 12 });
  });

  it("returns no rows when both sides are empty", () => {
    expect(computeDiff("", "")).toEqual([]);
  });
});
