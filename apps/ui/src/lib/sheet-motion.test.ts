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
  el.setAttribute("data-open", "");
  const slide = { finish: vi.fn() };
  const ownTransition = { finish: vi.fn(), transitionProperty: "opacity" };
  el.getAnimations = () => [slide, ownTransition] as unknown as Animation[];
  return { el, slide, ownTransition };
}

function snapMarker(top: number) {
  const marker = document.createElement("div");
  marker.style.cssText = `position:absolute;top:${top}px;scroll-snap-align:start`;
  return marker;
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

  it("finishes the slide again when a kept-mounted sheet reopens", async () => {
    stubMotion(true);
    const { el, slide } = sheetWithSlide();
    const cleanup = skipReducedSheetMotion(el);
    await frames(4);
    el.removeAttribute("data-open");
    el.setAttribute("data-closed", "");
    await frames(2);
    slide.finish.mockClear();
    el.removeAttribute("data-closed");
    el.setAttribute("data-open", "");
    await frames(5);
    expect(slide.finish).toHaveBeenCalled();
    cleanup?.();
  });

  it("puts a sheet left at its closed snap position at its open snap point", async () => {
    stubMotion(true);
    const host = document.createElement("div");
    const { el } = sheetWithSlide();
    host.append(el, snapMarker(0), snapMarker(357), snapMarker(900));
    host.scrollTop = 0;
    const cleanup = skipReducedSheetMotion(el);
    await frames(6);
    expect(host.scrollTop).toBe(357);
    cleanup?.();
  });

  it("does not move a sheet that opened on screen", async () => {
    stubMotion(true);
    const host = document.createElement("div");
    const { el } = sheetWithSlide();
    host.append(el, snapMarker(357));
    host.scrollTop = 200;
    const cleanup = skipReducedSheetMotion(el);
    await frames(6);
    expect(host.scrollTop).toBe(200);
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
