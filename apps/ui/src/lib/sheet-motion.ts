const reducedMotion = () =>
  typeof matchMedia === "function" && matchMedia("(prefers-reduced-motion: reduce)").matches;

function finishSheetMotion(el: HTMLElement) {
  if (!el.hasAttribute("data-driver") || !el.getAnimations || !reducedMotion()) return;
  for (const animation of el.getAnimations()) {
    // Our own CSS transitions already honour reduced motion; only the SDK's script animations.
    if ("transitionProperty" in animation || "animationName" in animation) continue;
    animation.finish();
  }
}

/**
 * A ref for any UI SDK sheet surface. On touch screens the SDK's scroll-snap
 * driver slides sheets in and out even with reduced motion on (see
 * docs/ui-audit/upstream-ui-sdk.md), so this jumps its open and close to the
 * end. Popover presentations have no `data-driver` and are left alone.
 */
function skipReducedSheetMotion(el: HTMLElement | null) {
  if (!el || typeof requestAnimationFrame !== "function") return;
  // The SDK lays an opening sheet out over its first frames (the slide hides
  // that), so the open finishes after them; a close finishes at once.
  let frame = requestAnimationFrame(() => {
    frame = requestAnimationFrame(() => {
      frame = requestAnimationFrame(() => finishSheetMotion(el));
    });
  });
  const observer = new MutationObserver(() => {
    if (!el.hasAttribute("data-ending-style") && !el.hasAttribute("data-closed")) return;
    finishSheetMotion(el);
    cancelAnimationFrame(frame);
    frame = requestAnimationFrame(() => finishSheetMotion(el));
  });
  observer.observe(el, { attributes: true, attributeFilter: ["data-closed", "data-ending-style"] });
  return () => {
    cancelAnimationFrame(frame);
    observer.disconnect();
  };
}

export { skipReducedSheetMotion };
