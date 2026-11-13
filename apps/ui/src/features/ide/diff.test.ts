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

  it("collapses runs outside the change context behind hunk headers", () => {
    const before = Array.from({ length: 30 }, (_, index) => `line ${index}`).join("\n");
    const after = before.replace("line 15", "line fifteen");
    const rows = collapseDiff(computeDiff(`${before}\n`, `${after}\n`), 2);
    const hunks = rows.filter((row) => row.kind === "hunk");
    expect(hunks).toHaveLength(1);
    expect(hunks[0]).toMatchObject({
      oldStart: 14,
      oldCount: 5,
      newStart: 14,
      newCount: 5,
      context: "line 12",
    });
    expect(rows[0]).toEqual(hunks[0]);
    expect(rows.some((row) => row.kind === "added")).toBe(true);
  });

  it("opens a second hunk after a long collapsed gap", () => {
    const before = Array.from({ length: 40 }, (_, index) => `line ${index}`).join("\n");
    const after = before.replace("line 5", "line five").replace("line 30", "line thirty");
    const rows = collapseDiff(computeDiff(`${before}\n`, `${after}\n`), 2);
    const hunks = rows.filter((row) => row.kind === "hunk");
    expect(hunks).toHaveLength(2);
    expect(hunks[1]).toMatchObject({ oldStart: 29, newStart: 29 });
  });

  it("returns no rows when both sides are empty", () => {
    expect(computeDiff("", "")).toEqual([]);
  });
});
