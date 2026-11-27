import { type RefObject, useLayoutEffect, useRef } from "react";

function popupDuration(): number {
  const value = getComputedStyle(document.documentElement).getPropertyValue("--duration-popup");
  const ms = Number.parseFloat(value);
  return Number.isFinite(ms) ? ms : 180;
}

/**
 * Fades the container's first child in whenever `view` changes: a short
 * opacity-only crossfade between views (thread and IDE, settings sections,
 * inbox and thread). Nothing remounts and nothing moves; reduced motion skips it.
 */
export function useViewFade(ref: RefObject<HTMLElement | null>, view: string) {
  const first = useRef(true);
  // biome-ignore lint/correctness/useExhaustiveDependencies: the fade runs on each view change.
  useLayoutEffect(() => {
    if (first.current) {
      first.current = false;
      return;
    }
    const node = ref.current?.firstElementChild;
    if (!(node instanceof HTMLElement) || typeof node.animate !== "function") return;
    if (window.matchMedia?.("(prefers-reduced-motion: reduce)").matches) return;
    node.animate([{ opacity: 0 }, { opacity: 1 }], {
      duration: popupDuration(),
      easing: "cubic-bezier(0.2, 0, 0, 1)",
    });
  }, [view]);
}

/** The view a route shows, coarse enough that in-page selections don't fade. */
export function viewKey(
  route: { kind: string; threadId?: string; surface?: string; path?: string },
  mode: string,
): string {
  if (route.kind === "thread") return `${mode}:thread:${route.threadId}`;
  if (route.kind !== "placeholder") return `${mode}:${route.kind}`;
  const depth = route.surface === "settings" || route.surface === "library" ? 3 : 2;
  return `${mode}:${(route.path ?? "").split("/").slice(0, depth).join("/")}`;
}
