import { GitForkIcon } from "lucide-react";

import type { ThreadSummary } from "@/gen";
import { openThread, useApp } from "@/lib/store/appStore";

/**
 * The quiet lineage line under a forked thread's title.
 *
 * The origin is resolved from the thread list so a rename shows here without
 * rewriting the fork. A source that is gone keeps the fact but loses the link.
 */
export function ThreadForkLine({ summary }: { summary: ThreadSummary }) {
  const forkedFrom = summary.forked_from;
  const originId = forkedFrom?.thread_id ?? null;
  const origin = useApp((state) => (originId ? state.threads[originId] : undefined));

  if (!forkedFrom) return null;
  return (
    <div className="q-fork-line">
      <GitForkIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
      {origin ? (
        <button
          type="button"
          className="q-fork-link"
          onClick={() => openThread(origin.thread_id)}
          title={`Open ${origin.title}`}
        >
          Forked from <span>{origin.title}</span>
        </button>
      ) : (
        <span>Forked from a thread that is no longer in your list</span>
      )}
    </div>
  );
}
