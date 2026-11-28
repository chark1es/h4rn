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

/** The scroll offset of the first open snap point the scroll-snap driver laid out. */
function openSnapTop(host: HTMLElement): number | undefined {
  let open: number | undefined;
  for (const child of Array.from(host.children)) {
    if (!(child instanceof HTMLElement) || child.style.scrollSnapAlign !== "start") continue;
    const top = Number.parseFloat(child.style.top);
    if (top > 1 && (open === undefined || top < open)) open = top;
  }
  return open;
}

/**
 * Puts a sheet that opened but stayed at the closed snap position (scrolled
 * off the bottom of the screen) at its first open snap point, without motion.
 */
function revealStuckSheet(el: HTMLElement) {
  const host = el.parentElement;
  if (el.getAttribute("data-driver") !== "scroll-snap" || !host || !reducedMotion()) return;
  if (!el.hasAttribute("data-open") || el.hasAttribute("data-ending-style")) return;
  if (host.scrollTop > 1) return;
  const top = openSnapTop(host);
  if (top !== undefined) host.scrollTop = top;
}

// Frames after the open in which the driver's relayouts can still strand the sheet.
const REVEAL_FRAMES = 30;

/**
 * A ref for any UI SDK sheet surface. On touch screens the SDK's scroll-snap
 * driver slides sheets in and out even with reduced motion on, and a sheet
 * whose content resizes while it opens can stay scrolled off screen (see
 * docs/ui-audit/upstream-ui-sdk.md). On every open and close this jumps the
 * motion to the end and puts a stranded sheet at its open snap point. Popover
 * presentations have no `data-driver` and are left alone.
 */
function skipReducedSheetMotion(el: HTMLElement | null) {
  if (!el || typeof requestAnimationFrame !== "function") return;
  let frame = 0;
  const after = (count: number, run: () => void) => {
    frame = requestAnimationFrame(() => (count > 1 ? after(count - 1, run) : run()));
  };
  const reveal = (left: number) => {
    revealStuckSheet(el);
    if (left > 1) frame = requestAnimationFrame(() => reveal(left - 1));
  };
  const opened = () => {
    cancelAnimationFrame(frame);
    // The SDK lays an opening sheet out over its first frames (the slide hides
    // that), so the open finishes after them.
    after(3, () => {
      finishSheetMotion(el);
      reveal(REVEAL_FRAMES);
    });
  };
  const closed = () => {
    finishSheetMotion(el);
    cancelAnimationFrame(frame);
    after(1, () => finishSheetMotion(el));
  };
  let wasOpen = el.hasAttribute("data-open");
  if (wasOpen) opened();
  const observer = new MutationObserver(() => {
    const open = el.hasAttribute("data-open");
    if (el.hasAttribute("data-ending-style") || el.hasAttribute("data-closed")) closed();
    else if (open && !wasOpen) opened();
    wasOpen = open;
  });
  observer.observe(el, {
    attributes: true,
    attributeFilter: ["data-open", "data-closed", "data-ending-style"],
  });
  return () => {
    cancelAnimationFrame(frame);
    observer.disconnect();
  };
}

export { skipReducedSheetMotion };
