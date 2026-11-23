import { useCallback, useEffect, useState } from "react";

import type { ThreadPullRequest } from "@/gen";
import { getClient, onAppEvent } from "@/lib/store/appStore";

import { ForgeApi } from "./store";

const listeners = new Map<string, Set<() => void>>();

/** Tells every mounted linked-PR surface for a thread to re-read the daemon. */
export function notifyThreadLinks(threadId: string): void {
  for (const listener of listeners.get(threadId) ?? []) listener();
}

/** Listens for link changes made anywhere in the app (badge, palette, PR page). */
export function subscribeThreadLinks(threadId: string, listener: () => void): () => void {
  const set = listeners.get(threadId) ?? new Set();
  set.add(listener);
  listeners.set(threadId, set);
  return () => {
    set.delete(listener);
    if (set.size === 0) listeners.delete(threadId);
  };
}

const defaultApi = () => new ForgeApi((request) => getClient().request(request));

export type ThreadPullState = {
  pulls: ThreadPullRequest[];
  loading: boolean;
  error: string | null;
  reload: () => void;
};

/**
 * The linked PRs of one thread, with their latest state from the daemon.
 *
 * Nothing is cached beyond the component: the daemon reads the forge for the
 * state, so a stale row is never presented as current.
 */
export function useThreadPullRequests(
  threadId: string | null,
  api: () => ForgeApi = defaultApi,
): ThreadPullState {
  const [pulls, setPulls] = useState<ThreadPullRequest[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [nonce, setNonce] = useState(0);

  const reload = useCallback(() => setNonce((value) => value + 1), []);

  useEffect(() => {
    if (!threadId) return;
    return subscribeThreadLinks(threadId, reload);
  }, [threadId, reload]);

  // A watch wake arrives as a user message appended to the thread. It may
  // also be the last thing the watch ever says (a merge or close ends it), so
  // re-read rather than keep showing the pre-wake state and a stale
  // "Stop watching".
  useEffect(() => {
    if (!threadId) return;
    return onAppEvent(({ event }) => {
      if (event.type !== "item_completed") return;
      if (event.thread_id !== threadId) return;
      if (event.item.kind !== "message" || event.item.role !== "user") return;
      reload();
    });
  }, [threadId, reload]);

  useEffect(() => {
    // Reading the nonce is what makes a reload re-run this effect.
    void nonce;
    if (!threadId) {
      setPulls([]);
      setError(null);
      setLoading(false);
      return;
    }
    let cancelled = false;
    setLoading(true);
    api()
      .threadPullRequests(threadId)
      .then((next) => {
        if (cancelled) return;
        setPulls(next);
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

  return { pulls, loading, error, reload };
}
