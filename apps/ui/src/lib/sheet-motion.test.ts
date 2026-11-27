// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { skipReducedSheetMotion } from "./sheet-motion";

function stubMotion(reduce: boolean) {
  vi.stubGlobal("matchMedia", (query: string) => ({
    matches: query.includes("prefers-reduced-motion: reduce") ? reduce : false,
    media: query,
  }));
}

function sheetWithSlide() {
  const el = document.createElement("div");
  el.setAttribute("data-driver", "scroll-snap");
  const slide = { finish: vi.fn() };
  const ownTransition = { finish: vi.fn(), transitionProperty: "opacity" };
  el.getAnimations = () => [slide, ownTransition] as unknown as Animation[];
  return { el, slide, ownTransition };
}

async function frames(count: number) {
  for (let i = 0; i < count; i++) await new Promise((resolve) => requestAnimationFrame(resolve));
}

describe("skipReducedSheetMotion", () => {
  beforeEach(() => vi.unstubAllGlobals());
  afterEach(() => vi.unstubAllGlobals());

  it("finishes the SDK's open slide when reduced motion is on", async () => {
    stubMotion(true);
    const { el, slide, ownTransition } = sheetWithSlide();
    const cleanup = skipReducedSheetMotion(el);
    await frames(4);
    expect(slide.finish).toHaveBeenCalled();
    expect(ownTransition.finish).not.toHaveBeenCalled();
    cleanup?.();
  });

  it("finishes the close slide as soon as the sheet starts closing", async () => {
    stubMotion(true);
    const { el, slide } = sheetWithSlide();
    const cleanup = skipReducedSheetMotion(el);
    await frames(4);
    slide.finish.mockClear();
    el.setAttribute("data-ending-style", "");
    await Promise.resolve();
    expect(slide.finish).toHaveBeenCalled();
    cleanup?.();
  });

  it("leaves the slide alone with normal motion", async () => {
    stubMotion(false);
    const { el, slide } = sheetWithSlide();
    const cleanup = skipReducedSheetMotion(el);
    await frames(4);
    expect(slide.finish).not.toHaveBeenCalled();
    cleanup?.();
  });

  it("leaves popover presentations alone", async () => {
    stubMotion(true);
    const { el, slide } = sheetWithSlide();
    el.removeAttribute("data-driver");
    const cleanup = skipReducedSheetMotion(el);
    await frames(4);
    expect(slide.finish).not.toHaveBeenCalled();
    cleanup?.();
  });
});
