/**
 * Manual tab placements, as the store announces them.
 *
 * A "Finished" notification is only ever the daemon's own lifecycle
 * transition. When a person moves a thread between tabs, that is a placement,
 * not a completion; the notifier must stop attributing a remembered live
 * completion to whatever Done update follows. The store calls this before it
 * sends the move, and the notifier drops what it was holding for that thread.
 *
 * This tiny module exists so the store does not have to import the notifier
 * (which reads the store) and create a cycle.
 */

import type { ThreadId } from "@/gen";

const listeners = new Set<(threadId: ThreadId) => void>();

/** Subscribes to manual placements; returns the unsubscribe. */
export function onManualPlacement(listener: (threadId: ThreadId) => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Called by the store when the person moves a thread by hand. */
export function noteManualPlacement(threadId: ThreadId): void {
  for (const listener of listeners) listener(threadId);
}
