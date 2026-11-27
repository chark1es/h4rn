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
 * (and the row around it). Call the result once the action has finished: if
 * the trigger is gone and focus went with it, focus moves to the next control
 * after the row, or else the one before.
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
    if (trigger.isConnected || !focusLost()) return;
    [...after, ...before].find((el) => el.isConnected)?.focus();
  };
}
