import { describe, expect, it } from "vitest";

import { leftAloneSummary } from "./left-alone";

describe("leftAloneSummary", () => {
  it("says nothing when a restore leaves nothing alone", () => {
    expect(leftAloneSummary([])).toBeNull();
  });

  it("names the paths, and counts the rest", () => {
    expect(leftAloneSummary([".env"])).toBe(".env");
    expect(leftAloneSummary([".env", "secret.log"])).toBe(".env, secret.log");
    expect(leftAloneSummary(["a", "b", "c", "d", "e"])).toBe("a, b, c, d and 1 more");
  });
});
