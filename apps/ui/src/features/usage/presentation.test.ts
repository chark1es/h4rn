import { describe, expect, it } from "vitest";

import type { UsageBucketPoint, UsageReport, UsageTotals } from "@/gen";
import {
  bucketLabel,
  bucketTotal,
  countdownPrecise,
  firstToken,
  percent,
  seriesColor,
} from "./presentation";
import { bucketFor, rangeFor, seriesTotals, totalsValue } from "./usageStore";

const DAY = 24 * 60 * 60 * 1000;

function totals(overrides: Partial<UsageTotals> = {}): UsageTotals {
  return {
    input_tokens: 120,
    output_tokens: 30,
    cached_input_tokens: 40,
    cached_creation_tokens: 0,
    cost_micros: 1_500_000,
    ttft_ms: 640,
    tps: 42,
    wall_ms: 8_000,
    generation_ms: 5_000,
    active_ms: 6_000,
    idle_ms: 2_000,
    runtime_ms: 10_000,
    turns: 1,
    ...overrides,
  };
}

function point(start: string, overrides: Partial<UsageTotals> = {}): UsageBucketPoint {
  return {
    start,
    end: new Date(Date.parse(start) + DAY).toISOString(),
    totals: totals(overrides),
  };
}

describe("usage range and bucket selection", () => {
  const now = Date.parse("2026-11-13T15:30:00.000Z");

  it("maps today to local midnight and leaves the end open", () => {
    const range = rangeFor("today", now);
    const start = new Date(now);
    start.setHours(0, 0, 0, 0);
    expect(range).toEqual({ since: start.toISOString(), until: null });
  });

  it("maps 14 and 30 days to rolling windows", () => {
    expect(rangeFor("14d", now)).toEqual({
      since: new Date(now - 14 * DAY).toISOString(),
      until: null,
    });
    expect(rangeFor("30d", now)).toEqual({
      since: new Date(now - 30 * DAY).toISOString(),
      until: null,
    });
  });

  it("keeps all unbounded", () => {
    expect(rangeFor("all", now)).toEqual({ since: null, until: null });
  });

  it("buckets today hourly and longer ranges daily", () => {
    expect(bucketFor("today")).toBe("hour");
    expect(bucketFor("14d")).toBe("day");
    expect(bucketFor("30d")).toBe("day");
    expect(bucketFor("all")).toBe("day");
  });
});

describe("usage series helpers", () => {
  const report: UsageReport = {
    scope: "global",
    scope_id: null,
    range: { since: null, until: null },
    group_by: "model",
    bucket: "day",
    series_group_by: "harness",
    totals: totals(),
    buckets: [
      point("2026-11-12T00:00:00.000Z"),
      point("2026-11-13T00:00:00.000Z", { input_tokens: 0, output_tokens: 0 }),
    ],
    series: [
      {
        key: "claude_code",
        points: [
          point("2026-11-12T00:00:00.000Z"),
          point("2026-11-13T00:00:00.000Z", { input_tokens: 0, output_tokens: 0 }),
        ],
      },
      {
        key: "codex",
        points: [
          point("2026-11-12T00:00:00.000Z", { input_tokens: 10, output_tokens: 5 }),
          point("2026-11-13T00:00:00.000Z", { input_tokens: 20, output_tokens: 7 }),
        ],
      },
    ],
    groups: [],
  };

  it("totals every point in a stacked series", () => {
    expect(seriesTotals(report)).toEqual([
      { key: "claude_code", total: 150 },
      { key: "codex", total: 42 },
    ]);
  });

  it("returns no series for a missing report", () => {
    expect(seriesTotals(null)).toEqual([]);
  });

  it("picks the metric value from totals", () => {
    expect(totalsValue(totals(), "tokens")).toBe(150);
    expect(totalsValue(totals(), "cost")).toBeCloseTo(1.5);
  });

  it("uses the same token and cost bases for a chart bar", () => {
    const bucket = point("2026-11-12T00:00:00.000Z");
    expect(bucketTotal(bucket, "tokens")).toBe(150);
    expect(bucketTotal(bucket, "cost")).toBeCloseTo(1.5);
  });
});

describe("usage presentation formatting", () => {
  const now = Date.parse("2026-11-13T15:30:00.000Z");

  it("counts down to the daemon's reset instant", () => {
    expect(countdownPrecise(new Date(now + 3_600_000).toISOString(), now)).toBe("1h 00m");
    expect(countdownPrecise(new Date(now + 125_000).toISOString(), now)).toBe("2m 05s");
    expect(countdownPrecise(new Date(now + 9_000).toISOString(), now)).toBe("9s");
    expect(countdownPrecise(new Date(now - 1).toISOString(), now)).toBe("now");
    expect(countdownPrecise(null, now)).toBe("unknown");
    expect(countdownPrecise("not-a-date", now)).toBe("unknown");
  });

  it("formats first token in the unit the number deserves", () => {
    expect(firstToken(0)).toBe("—");
    expect(firstToken(420)).toBe("420ms");
    expect(firstToken(1_700)).toBe("1.7s");
    expect(firstToken(999.6)).toBe("1.0s");
  });

  it("clamps percentages into a renderable range", () => {
    expect(percent(6_000, 8_000)).toBe(75);
    expect(percent(0, 0)).toBe(0);
    expect(percent(-1, 10)).toBe(0);
    expect(percent(11, 10)).toBe(100);
  });

  it("labels day buckets without a time component", () => {
    const localNoon = new Date(2026, 10, 12, 12).toISOString();
    const label = bucketLabel(point(localNoon), "day");
    expect(label).toContain("12");
    expect(bucketLabel({ start: "bad", end: "bad", totals: totals() }, "day")).toBe("—");
  });

  it("keeps a harness colour across scopes", () => {
    expect(seriesColor("claude_code", 0)).toBe("var(--s1)");
    expect(seriesColor("codex", 1)).toBe("var(--s2)");
    expect(seriesColor("opencode", 2)).toBe("var(--s3)");
    expect(seriesColor("antigravity", 3)).toBe("var(--s4)");
    expect(seriesColor("unknown", 4)).toBe("var(--s1)");
  });
});
