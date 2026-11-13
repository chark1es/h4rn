import { useEffect, useRef, useState } from "react";

import { usePrefersReducedMotion } from "./use-media-query";

/**
 * Reveal streamed text incrementally instead of snapping to every delta.
 *
 * The store still holds the authoritative text; this hook only paces what is
 * painted. Reduced motion shows the full text immediately. Catch-up speed is
 * proportional to the backlog so a burst never lags behind for long.
 */
export function useSmoothText(text: string, enabled = true): string {
  const reduced = usePrefersReducedMotion();
  const [display, setDisplay] = useState(() => (reduced || !enabled ? text : ""));
  const frame = useRef<number | null>(null);

  // biome-ignore lint/correctness/useExhaustiveDependencies: display is the reveal cursor, not a trigger
  useEffect(() => {
    if (reduced || !enabled) {
      setDisplay(text);
      return;
    }

    let shown = display.startsWith(text) || text.startsWith(display) ? display.length : 0;
    if (shown > text.length || !text.startsWith(display)) {
      shown = 0;
      setDisplay("");
    }
    if (shown >= text.length) {
      if (display !== text) setDisplay(text);
      return;
    }

    let cancelled = false;
    const tick = () => {
      if (cancelled) return;
      const backlog = text.length - shown;
      if (backlog <= 0) {
        setDisplay(text);
        return;
      }
      const step = Math.max(2, Math.ceil(backlog * 0.34));
      shown = Math.min(text.length, shown + step);
      setDisplay(text.slice(0, shown));
      if (shown < text.length) {
        frame.current = requestAnimationFrame(tick);
      }
    };
    frame.current = requestAnimationFrame(tick);

    return () => {
      cancelled = true;
      if (frame.current !== null) cancelAnimationFrame(frame.current);
      frame.current = null;
    };
    // `display` is read as the current reveal position; depending on it would
    // restart the loop on every painted frame.
  }, [text, enabled, reduced]);

  return display;
}
