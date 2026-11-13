import type { ThreadId, ThreadSummary } from "@/gen";

/**
 * The thread whose project the IDE and the right panel follow.
 *
 * A thread route sets the selection. The /ide deep link has no thread of its
 * own, so it falls back to the first thread that is actually working — that is
 * the project the editor should open.
 */
export function activeThread(
  threads: Record<ThreadId, ThreadSummary>,
  selectedThreadId: ThreadId | null,
): ThreadSummary | null {
  if (selectedThreadId) {
    const selected = threads[selectedThreadId];
    if (selected) return selected;
  }
  const working = Object.values(threads)
    .filter((thread) => thread.tab === "working")
    .sort((a, b) => b.last_activity.localeCompare(a.last_activity));
  return working[0] ?? null;
}
