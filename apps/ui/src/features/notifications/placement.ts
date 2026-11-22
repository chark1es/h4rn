/**
 * Manual tab placements, as the store announces them.
 *
 * A "Finished" notification is only ever the daemon's own lifecycle
 * transition. When a person moves a thread between tabs, that is a placement,
 * not a completion; the notifier must stop attributing a remembered live
 * completion to whatever Done update follows. The store calls this after the
 * daemon accepts the move, so a rejected move leaves no placement note. The
 * notifier drops what it was holding for that thread.
 *
 * This tiny module exists so the store does not have to import the notifier
 * (which reads the store) and create a cycle.
 */

import type { ThreadId, ThreadTab } from "@/gen";

const listeners = new Set<(threadId: ThreadId) => void>();

/**
 * Whether a tab move is a manual placement the notifier should hear about.
 *
 * Done and Settled are where a person puts a thread. Working is reached by a
 * pin toggle on the way to nothing, and by the daemon starting a turn, so it
 * is not treated as a placement.
 */
export function isPlacementMove(tab: ThreadTab): boolean {
  return tab === "done" || tab === "settled";
}

/** Subscribes to manual placements; returns the unsubscribe. */
export function onManualPlacement(listener: (threadId: ThreadId) => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Called by the store when the person moves a thread by hand. */
export function noteManualPlacement(threadId: ThreadId): void {
  for (const listener of listeners) listener(threadId);
}
