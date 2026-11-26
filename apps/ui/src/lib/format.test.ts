import { afterEach, describe, expect, it } from "vitest";

import { formatCost, toolStatsLabel } from "./format";
import { setLocalePreference } from "./i18n";

afterEach(async () => {
  await setLocalePreference("en");
});

describe("formatCost", () => {
  it("writes the narrow dollar sign in every language, as the chart axis does", async () => {
    expect(formatCost(1_230_000)).toBe("$1.23");
    expect(formatCost(0)).toBe("—");
    await setLocalePreference("zh-CN");
    expect(formatCost(1_230_000)).toBe("$1.23");
    await setLocalePreference("es");
    expect(formatCost(1_230_000)).toBe("1,23\u00a0$");
    await setLocalePreference("ja");
    expect(formatCost(1_230_000)).toBe("$1.23");
  });
});

describe("toolStatsLabel", () => {
  it("passes a diffstat through for the +N −M pair", () => {
    expect(toolStatsLabel({ kind: "diff", added: 12, removed: 3 })).toEqual({
      kind: "diff",
      added: 12,
      removed: 3,
    });
  });

  it("words match counts and agrees in number", () => {
    expect(toolStatsLabel({ kind: "matches", count: 3, unit: "matches" })).toEqual({
      kind: "text",
      text: "3 matches",
    });
    expect(toolStatsLabel({ kind: "matches", count: 1, unit: "matches" })).toEqual({
      kind: "text",
      text: "1 match",
    });
    expect(toolStatsLabel({ kind: "matches", count: 2, unit: "files" })).toEqual({
      kind: "text",
      text: "2 files",
    });
    expect(toolStatsLabel({ kind: "matches", count: 0, unit: "matches" })).toEqual({
      kind: "text",
      text: "0 matches",
    });
  });

  it("says nothing without stats", () => {
    expect(toolStatsLabel(undefined)).toBeNull();
    expect(toolStatsLabel(null)).toBeNull();
  });
});
