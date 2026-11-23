import { useCallback, useEffect, useState } from "react";

import type { ThreadIssue } from "@/gen";
import { getClient } from "@/lib/store/appStore";

import { IssueApi } from "./store";

const listeners = new Map<string, Set<() => void>>();

/** Tells every mounted linked-issue surface for a thread to re-read. */
export function notifyThreadIssueLinks(threadId: string): void {
  for (const listener of listeners.get(threadId) ?? []) listener();
}

function subscribe(threadId: string, listener: () => void): () => void {
  const set = listeners.get(threadId) ?? new Set();
  set.add(listener);
  listeners.set(threadId, set);
  return () => {
    set.delete(listener);
    if (set.size === 0) listeners.delete(threadId);
  };
}

const defaultApi = () => new IssueApi((request) => getClient().request(request));

export type ThreadIssueState = {
  issues: ThreadIssue[];
  loading: boolean;
  error: string | null;
  reload: () => void;
};

/**
 * The linked issues of one thread, with their latest tracker state.
 *
 * Nothing is cached beyond the component: the daemon reads the tracker for
 * each link, so a stale row is never presented as current.
 */
export function useThreadIssues(
  threadId: string | null,
  api: () => IssueApi = defaultApi,
): ThreadIssueState {
  const [issues, setIssues] = useState<ThreadIssue[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [nonce, setNonce] = useState(0);

  const reload = useCallback(() => setNonce((value) => value + 1), []);

  useEffect(() => {
    if (!threadId) return;
    return subscribe(threadId, reload);
  }, [threadId, reload]);

  useEffect(() => {
    void nonce;
    if (!threadId) {
      setIssues([]);
      setError(null);
      setLoading(false);
      return;
    }
    let cancelled = false;
    setLoading(true);
    api()
      .threadIssues(threadId)
      .then((next) => {
        if (cancelled) return;
        setIssues(next);
        setError(null);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setError(cause instanceof Error ? cause.message : String(cause));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [threadId, nonce, api]);

  return { issues, loading, error, reload };
}
