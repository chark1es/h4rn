import { type RefObject, useLayoutEffect, useState } from "react";

/** One segment button's natural width: padding plus its parts side by side. */
export type SegmentItem = { padding: number; gap: number; parts: number[] };

/**
 * Whether a row of segment buttons fits at their natural widths.
 *
 * Each button needs its padding, its parts (the label and the count) and the
 * gaps between them; the row adds its own gaps. This is what the segment
 * needs before anything is truncated, so it does not change once the
 * segment switches to fitting.
 */
export function segmentOverflows(available: number, gap: number, items: SegmentItem[]): boolean {
  let need = gap * Math.max(0, items.length - 1);
  for (const item of items) {
    need += item.padding;
    need += item.gap * Math.max(0, item.parts.length - 1);
    for (const part of item.parts) need += part;
  }
  // scrollWidth rounds to whole pixels; allow for that.
  return need > available + 1;
}

function px(value: string): number {
  const number = Number.parseFloat(value);
  return Number.isFinite(number) ? number : 0;
}

function measure(segment: HTMLElement): boolean {
  const style = getComputedStyle(segment);
  const available = segment.clientWidth - px(style.paddingLeft) - px(style.paddingRight);
  const items: SegmentItem[] = [];
  for (const button of Array.from(segment.children) as HTMLElement[]) {
    const own = getComputedStyle(button);
    items.push({
      padding: px(own.paddingLeft) + px(own.paddingRight),
      gap: px(own.columnGap),
      // A truncated label still reports its full text width as scrollWidth.
      parts: (Array.from(button.children) as HTMLElement[]).map((part) => part.scrollWidth),
    });
  }
  return segmentOverflows(available, px(style.columnGap), items);
}

/**
 * True while the segment's labels are too long for its width, so the caller
 * lets the labels shrink with an ellipsis (`.q-seg.fit`) instead of pushing
 * the last tab out of view. Short labels keep the usual equal tabs.
 *
 * `key` changes whenever the text does (the language, a count).
 */
export function useSegmentFit(ref: RefObject<HTMLElement | null>, key: string): boolean {
  const [fit, setFit] = useState(false);
  useLayoutEffect(() => {
    const segment = ref.current;
    if (!segment) return;
    void key;
    const update = () => setFit(measure(segment));
    update();
    const observer = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(update);
    observer?.observe(segment);
    // Widths change once the web font arrives.
    let live = true;
    void document.fonts?.ready.then(() => {
      if (live) update();
    });
    return () => {
      live = false;
      observer?.disconnect();
    };
  }, [ref, key]);
  return fit;
}
