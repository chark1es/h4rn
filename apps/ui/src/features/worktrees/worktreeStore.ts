import { useEffect, useSyncExternalStore } from "react";

import type { ProtoError, SkippedWorktree, TrackedWorktree } from "@/gen";
import { toProtoError } from "@/lib/client/errors";
import { getClient } from "@/lib/store/appStore";

/** What the last clear did; the page words it under the Settled group. */
export type ClearOutcome = { removed: number; skipped: SkippedWorktree[] };

export type WorktreeState = {
  rows: TrackedWorktree[];
  loaded: boolean;
  loading: boolean;
  clearing: boolean;
  error: ProtoError | null;
  outcome: ClearOutcome | null;
  /** Set when a clear request itself failed, as opposed to skipping entries. */
  clearError: string | null;
};

const initialState: WorktreeState = {
  rows: [],
  loaded: false,
  loading: false,
  clearing: false,
  error: null,
  outcome: null,
  clearError: null,
};

type Listener = () => void;

let state = initialState;
const listeners = new Set<Listener>();

function patch(next: Partial<WorktreeState>): void {
  state = { ...state, ...next };
  for (const listener of listeners) listener();
}

export function getWorktreeState(): WorktreeState {
  return state;
}

export function useWorktrees<T>(selector: (value: WorktreeState) => T): T {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => selector(state),
    () => selector(state),
  );
}

/** Worktrees something is using right now. */
export function inUseWorktrees(rows: TrackedWorktree[]): TrackedWorktree[] {
  return rows.filter((row) => row.status === "active");
}

/** Worktrees with nothing running in them: settled, or left behind. */
export function settledWorktrees(rows: TrackedWorktree[]): TrackedWorktree[] {
  return rows.filter((row) => row.status !== "active");
}

/**
 * What "Clear all" removes. Thread workspaces belong to whoever made them and
 * their thread may resume there, so the daemon only removes one by path.
 */
export function clearableWorktrees(rows: TrackedWorktree[]): TrackedWorktree[] {
  return settledWorktrees(rows).filter((row) => row.origin !== "thread");
}

export async function loadWorktrees(): Promise<void> {
  patch({ loading: true, error: null });
  try {
    const response = await getClient().request({ type: "list_worktrees" });
    if (response.type !== "worktrees") {
      patch({ loaded: true, loading: false });
      return;
    }
    patch({ rows: response.worktrees, loaded: true, loading: false, error: null });
  } catch (error) {
    patch({ loaded: true, loading: false, error: toProtoError(error) });
  }
}

/**
 * Clears `paths` (every settled entry when null). Entries the daemon leaves in
 * place come back in `outcome.skipped` so the page can offer a forced retry.
 */
export async function clearWorktrees(paths: string[] | null, force = false): Promise<void> {
  if (state.clearing) return;
  patch({ clearing: true, clearError: null });
  try {
    const response = await getClient().request({ type: "clear_worktrees", paths, force });
    if (response.type !== "worktrees_cleared") {
      patch({ clearing: false });
      return;
    }
    patch({
      rows: response.worktrees,
      loaded: true,
      clearing: false,
      outcome: { removed: response.removed.length, skipped: response.skipped },
    });
  } catch (error) {
    const proto = toProtoError(error);
    patch({
      clearing: false,
      clearError: proto.hint ? `${proto.message} ${proto.hint}` : proto.message,
    });
  }
}

export function dismissWorktreeOutcome(): void {
  patch({ outcome: null, clearError: null });
}

/** Reads the list on mount and again each time the connection returns. */
export function useWorktreeList(online: boolean): void {
  useEffect(() => {
    if (online) void loadWorktrees();
  }, [online]);
}

export function resetWorktreeStoreForTests(): void {
  state = initialState;
  for (const listener of listeners) listener();
}
