const TABBABLE =
  'a[href], button, input, select, textarea, summary, [contenteditable=""], [contenteditable="true"], [tabindex]';

/** The elements Tab visits inside `root`, in document order. */
export function tabbables(root: ParentNode): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>(TABBABLE)).filter(
    (el) =>
      el.tabIndex >= 0 &&
      !el.matches(":disabled") &&
      !el.closest("[inert], [hidden]") &&
      (typeof el.checkVisibility !== "function" || el.checkVisibility()),
  );
}

function focusLost(): boolean {
  const active = document.activeElement;
  return !active || active === document.body || !active.isConnected;
}

/**
 * Remembers the controls around `trigger` before an action that may remove it
 * (and the row around it). Call the result once the action has finished. If
 * the trigger is still there (the action failed or kept the row), focus goes
 * back to it: it was disabled while the action ran, so the dialog closing put
 * focus somewhere else. If the trigger is gone and focus went with it, focus
 * moves to the next control after the row, or else the one before.
 */
export function rememberNeighbours(trigger: HTMLElement): () => Promise<void> {
  const row = trigger.closest<HTMLElement>("tr, li, [role='row']") ?? trigger;
  const all = tabbables(document.body);
  const at = all.indexOf(trigger);
  const outsideRow = (el: HTMLElement) => !row.contains(el);
  const after = at === -1 ? [] : all.slice(at + 1).filter(outsideRow);
  const before = at === -1 ? [] : all.slice(0, at).filter(outsideRow).reverse();
  return async () => {
    await new Promise((resolve) => requestAnimationFrame(resolve));
    if (trigger.isConnected) {
      // A busy flag can clear a render after the action settles; wait a few frames for it.
      for (let frame = 0; frame < 5 && trigger.matches(":disabled"); frame++) {
        await new Promise((resolve) => requestAnimationFrame(resolve));
      }
      if (trigger.isConnected && !trigger.matches(":disabled")) trigger.focus();
      return;
    }
    if (!focusLost()) return;
    [...after, ...before].find((el) => el.isConnected)?.focus();
  };
}
