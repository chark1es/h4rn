import { useSyncExternalStore } from "react";

function subscribeResize(callback: () => void): () => void {
  window.addEventListener("resize", callback);
  return () => window.removeEventListener("resize", callback);
}

/** The window's inner width, kept current on resize. */
export function useViewportWidth(): number {
  return useSyncExternalStore(
    subscribeResize,
    () => window.innerWidth,
    () => 1440,
  );
}
