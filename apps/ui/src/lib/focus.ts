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

const DIALOG = "[role='dialog'], [role='alertdialog']";
let stopWatching: (() => void) | undefined;

/**
 * Notes the first element focus lands on outside a dialog from now on: where
 * a closing dialog put it when its trigger could not take focus back. Only
 * the latest call watches.
 */
function watchFallback() {
  stopWatching?.();
  let fallback: Element | null = null;
  const onFocusIn = (event: FocusEvent) => {
    const target = event.target;
    if (fallback || !(target instanceof Element) || target.closest(DIALOG)) return;
    fallback = target;
  };
  document.addEventListener("focusin", onFocusIn, true);
  const stop = () => {
    document.removeEventListener("focusin", onFocusIn, true);
    if (stopWatching === stop) stopWatching = undefined;
  };
  stopWatching = stop;
  return {
    /** Focus is nowhere, or still where the dialog's fallback put it. */
    unclaimed: () => focusLost() || document.activeElement === fallback,
    stop,
  };
}

/**
 * Remembers where `trigger` sits in tab order before an action that may remove
 * it (and the row around it). Call the result once the action has finished.
 * If the trigger is still there (the action failed or kept the row), focus
 * goes back to it: it was disabled while the action ran, so the dialog closing
 * put focus somewhere else. If the trigger is gone and focus went with it (or
 * only to where the dialog fell back to), focus moves to the control that now follows the row in tab order, or else
 * the one before. The neighbours are looked up again after the update, since
 * it may have replaced them.
 */
export function rememberNeighbours(trigger: HTMLElement): () => Promise<void> {
  const row = trigger.closest<HTMLElement>("tr, li, [role='row']") ?? trigger;
  const all = tabbables(document.body);
  const at = all.indexOf(trigger);
  const outsideRow = (el: HTMLElement) => !row.contains(el);
  const after = at === -1 ? [] : all.slice(at + 1).filter(outsideRow);
  const before = at === -1 ? [] : all.slice(0, at).filter(outsideRow).reverse();
  const focus = watchFallback();
  return async () => {
    await new Promise((resolve) => requestAnimationFrame(resolve));
    focus.stop();
    if (trigger.isConnected) {
      // A busy flag can clear a render after the action settles; wait a few frames for it.
      for (let frame = 0; frame < 5 && trigger.matches(":disabled"); frame++) {
        await new Promise((resolve) => requestAnimationFrame(resolve));
      }
      if (trigger.isConnected && !trigger.matches(":disabled")) trigger.focus();
      return;
    }
    if (!focus.unclaimed()) return;
    nextAfterRow(before, after)?.focus();
  };
}

/**
 * The control now following the removed row: the first tabbable after the
 * nearest earlier control that survived, else an original later control that
 * survived, else that earlier control itself.
 */
function nextAfterRow(before: HTMLElement[], after: HTMLElement[]): HTMLElement | undefined {
  const now = tabbables(document.body);
  const anchor = before.find((el) => now.includes(el));
  const next = anchor ? now[now.indexOf(anchor) + 1] : undefined;
  return next ?? after.find((el) => now.includes(el)) ?? anchor;
}
