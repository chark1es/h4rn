/**
 * Coalesces a replay burst into one reload.
 *
 * A reconnect replays the history the client missed as one synchronous burst
 * of events. The first replayed event schedules the reload and the rest of
 * the burst join it; a later burst in another task is a separate read.
 *
 * React would also batch these updates, but the coalescer makes the one-read
 * guarantee independent of that: nothing is scheduled per event.
 */
export function createBurstReload(reload: () => void): () => void {
  let scheduled = false;
  return () => {
    if (scheduled) return;
    scheduled = true;
    queueMicrotask(() => {
      scheduled = false;
      reload();
    });
  };
}
