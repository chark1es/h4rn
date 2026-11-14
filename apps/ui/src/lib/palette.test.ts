import { describe, expect, it } from "vitest";

import { paletteScore } from "./palette";

describe("paletteScore", () => {
  it("ranks a prefix above a word start above a substring", () => {
    const prefix = paletteScore("Deny pending command", "den");
    const wordStart = paletteScore("Open the IDE", "ide");
    const inside = paletteScore("Provide context", "vid");
    expect(prefix).toBeGreaterThan(wordStart);
    expect(wordStart).toBeGreaterThan(inside);
    expect(inside).toBeGreaterThan(0);
  });

  it("keeps a thread that only contains the letters in order below the real command", () => {
    const real = paletteScore("deny pending action", "deny");
    const loose = paletteScore("Audit dependency tree", "deny");
    expect(real).toBeGreaterThan(loose);
  });

  it("does not match one or two stray letters across a long title", () => {
    expect(paletteScore("Ship relay retry with backoff", "xz")).toBe(0);
    expect(paletteScore("Migrate session store to Redis", "ide")).toBeLessThan(0.5);
  });

  it("treats an empty search as a match for everything", () => {
    expect(paletteScore("anything", "  ")).toBe(1);
  });
});
