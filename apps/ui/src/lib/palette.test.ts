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

  it("ignores accents on Latin letters", () => {
    expect(paletteScore("Bifurcar desde el último turno", "ultimo")).toBe(0.8);
    expect(paletteScore("Configuración", "configuracion")).toBe(1);
    expect(paletteScore("Fork", "fórk")).toBe(1);
  });

  it("finds word starts in any script", () => {
    // «» and “” separate words like spaces do.
    expect(paletteScore("Mostrar «En curso»", "curso")).toBe(0.8);
    expect(paletteScore("显示“进行中”", "进行")).toBe(0.8);
    expect(paletteScore("スレッドを整理", "スレッド")).toBe(1);
  });

  it("keeps Japanese voiced kana distinct from their plain form", () => {
    expect(paletteScore("ガイド", "カイド")).toBe(0);
    expect(paletteScore("ガイド", "ガイ")).toBe(1);
  });

  it("matches letters in order across characters outside the BMP", () => {
    // 𠀀 and 𠀁 are one code point each but two UTF-16 units; the last-resort
    // match counts both sides by code point.
    expect(paletteScore("a𠀀b𠀁c", "𠀀𠀁c")).toBe(0.1);
  });

  it("searches the keywords as well as the shown name", () => {
    expect(
      paletteScore("Resolver el hilo", "settle", ["Settle thread", "settle now archive"]),
    ).toBe(0.8);
  });
});
