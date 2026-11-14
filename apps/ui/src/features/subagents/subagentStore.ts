/**
 * Live subagent state.
 *
 * The thread projection only keeps a summary item, so the delegation panel
 * reads `ListSubagents` once and then patches the views from
 * `SubagentSpawned` / `SubagentUpdated` / `SubagentFinished`. Every action goes
 * through the daemon; nothing here invents progress.
 */

import { useSyncExternalStore } from "react";

import type {
  Event,
  HarnessKind,
  HostId,
  SpawnSubagentRequest,
  SubagentId,
  SubagentIsolation,
  SubagentView,
  ThreadId,
} from "@/gen";
import { getClient, onAppEvent, pushToast } from "@/lib/store/appStore";

export type SubagentEntry = {
  views: Record<SubagentId, SubagentView>;
  loading: boolean;
  loaded: boolean;
  error: string | null;
};

export type SubagentStoreState = {
  byThread: Record<ThreadId, SubagentEntry>;
  selected: SubagentId | null;
  spawning: boolean;
  cancelling: SubagentId | null;
  delegateOpen: boolean;
  error: string | null;
};

function emptyEntry(): SubagentEntry {
  return { views: {}, loading: false, loaded: false, error: null };
}

type Store = {
  getState: () => SubagentStoreState;
  setState: (next: SubagentStoreState) => void;
  subscribe: (listener: () => void) => () => void;
};

function createStore(initial: SubagentStoreState): Store {
  let state = initial;
  const listeners = new Set<() => void>();
  return {
    getState: () => state,
    setState: (next) => {
      state = next;
      for (const listener of listeners) listener();
    },
    subscribe: (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}

export const subagentStore = createStore({
  byThread: {},
  selected: null,
  spawning: false,
  cancelling: null,
  delegateOpen: false,
  error: null,
});

export function useSubagentStore<T>(selector: (state: SubagentStoreState) => T): T {
  return useSyncExternalStore(
    subagentStore.subscribe,
    () => selector(subagentStore.getState()),
    () => selector(subagentStore.getState()),
  );
}

function patch(next: Partial<SubagentStoreState>): void {
  subagentStore.setState({ ...subagentStore.getState(), ...next });
}

function entryFor(state: SubagentStoreState, threadId: ThreadId): SubagentEntry {
  return state.byThread[threadId] ?? emptyEntry();
}

function upsert(
  threadId: ThreadId,
  view: SubagentView,
  options: { selected?: boolean } = {},
): void {
  const state = subagentStore.getState();
  const entry = entryFor(state, threadId);
  const views = { ...entry.views, [view.subagent_id]: view };
  patch({
    byThread: {
      ...state.byThread,
      [threadId]: { ...entry, views, loaded: true, error: null },
    },
    ...(options.selected ? { selected: view.subagent_id } : {}),
  });
}

function remove(threadId: ThreadId, id: SubagentId): void {
  const state = subagentStore.getState();
  const entry = entryFor(state, threadId);
  const views = { ...entry.views };
  delete views[id];
  patch({
    byThread: { ...state.byThread, [threadId]: { ...entry, views } },
    ...(state.selected === id ? { selected: null } : {}),
  });
}

let started = false;

/** Starts the raw-event feed. Safe to call twice. */
export function initializeSubagents(): void {
  if (started) return;
  started = true;
  onAppEvent(({ event }: { event: Event }) => {
    if (
      event.type !== "subagent_spawned" &&
      event.type !== "subagent_updated" &&
      event.type !== "subagent_finished"
    ) {
      return;
    }
    upsert(event.parent_thread_id, event.subagent);
  });
}

export function getSubagentEntry(threadId: ThreadId): SubagentEntry {
  return entryFor(subagentStore.getState(), threadId);
}

export function subagentCount(threadId: ThreadId): number {
  return Object.keys(getSubagentEntry(threadId).views).length;
}

export function waitingSubagentCount(threadId: ThreadId): number {
  return Object.values(getSubagentEntry(threadId).views).filter(
    (view) => view.state === "awaiting_approval",
  ).length;
}

export function useSubagentViews(threadId: ThreadId): SubagentView[] {
  const views = useSubagentStore((state) => state.byThread[threadId]?.views);
  return Object.values(views ?? {}).sort(
    (a, b) =>
      a.depth - b.depth ||
      Date.parse(a.created_at) - Date.parse(b.created_at) ||
      a.subagent_id.localeCompare(b.subagent_id),
  );
}

/** Header badge shared by the desktop panel and the phone sheet. */
export function useSubagentCount(threadId: ThreadId): {
  total: number;
  waiting: number;
  loading: boolean;
} {
  const total = useSubagentStore(
    (state) => Object.keys(state.byThread[threadId]?.views ?? {}).length,
  );
  const waiting = useSubagentStore(
    (state) =>
      Object.values(state.byThread[threadId]?.views ?? {}).filter(
        (view) => view.state === "awaiting_approval",
      ).length,
  );
  const loading = useSubagentStore((state) => state.byThread[threadId]?.loading ?? false);
  return { total, waiting, loading };
}

export function useSubagentLoading(threadId: ThreadId): boolean {
  return useSubagentStore((state) => state.byThread[threadId]?.loading ?? false);
}

export function useSubagentError(threadId: ThreadId): string | null {
  return useSubagentStore((state) => state.byThread[threadId]?.error ?? null);
}

export function selectSubagent(id: SubagentId | null): void {
  patch({ selected: id });
}

export function openDelegate(): void {
  patch({ delegateOpen: true });
}

export function closeDelegate(): void {
  patch({ delegateOpen: false });
}

export function useSelectedSubagent(): SubagentView | null {
  return useSubagentStore((state) => {
    if (!state.selected) return null;
    for (const entry of Object.values(state.byThread)) {
      const view = entry.views[state.selected];
      if (view) return view;
    }
    return null;
  });
}

/** Loads the children of one parent thread. */
export async function loadSubagents(threadId: ThreadId, force = false): Promise<void> {
  const state = subagentStore.getState();
  const entry = entryFor(state, threadId);
  if (!force && (entry.loading || entry.loaded)) return;
  patch({
    byThread: {
      ...state.byThread,
      [threadId]: { ...entry, loading: true, error: null },
    },
  });
  try {
    const response = await getClient().request({
      type: "subagent",
      request: {
        type: "list_subagents",
        parent_thread: threadId,
        include_finished: true,
      },
    });
    if (response.type !== "subagents") return;
    const current = subagentStore.getState();
    const views = Object.fromEntries(
      response.subagents.map((view) => [view.subagent_id, view]),
    ) as Record<SubagentId, SubagentView>;
    patch({
      byThread: {
        ...current.byThread,
        [threadId]: { views, loading: false, loaded: true, error: null },
      },
    });
  } catch (error) {
    const current = subagentStore.getState();
    const message = error instanceof Error ? error.message : String(error);
    patch({
      byThread: {
        ...current.byThread,
        [threadId]: { ...entryFor(current, threadId), loading: false, error: message },
      },
    });
  }
}

export type SpawnInput = {
  parentThread: ThreadId;
  task: string;
  harness: HarnessKind | null;
  host: HostId | null;
  model: string | null;
  isolation: SubagentIsolation;
  maxTurns: number | null;
  maxWallTimeSecs: number | null;
  maxTokens: number | null;
  contextSummary: string | null;
};

/** Spawns a child and folds the returned view into the panel. */
export async function spawnSubagent(input: SpawnInput): Promise<boolean> {
  const task = input.task.trim();
  if (!task) return false;
  patch({ spawning: true, error: null });
  const request: SpawnSubagentRequest = {
    parent_thread: input.parentThread,
    task,
    harness: input.harness,
    host: input.host,
    model: input.model?.trim() ? input.model.trim() : null,
    isolation: input.isolation,
    context: input.contextSummary?.trim()
      ? { kind: "summary", summary: input.contextSummary.trim() }
      : { kind: "none" },
    budget: {
      max_turns: input.maxTurns,
      max_wall_time_secs: input.maxWallTimeSecs,
      max_tokens: input.maxTokens,
    },
  };
  try {
    const response = await getClient().request({
      type: "subagent",
      request: { type: "spawn_subagent", request },
    });
    if (response.type !== "subagent") return false;
    upsert(input.parentThread, response.subagent, { selected: true });
    pushToast({ variant: "success", title: `Delegated to ${response.subagent.harness}.` });
    return true;
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    patch({ error: message });
    pushToast({ variant: "error", title: "Could not delegate.", description: message });
    return false;
  } finally {
    patch({ spawning: false });
  }
}

/** Cancels a child and updates its row from the daemon's answer. */
export async function cancelSubagent(view: SubagentView): Promise<void> {
  patch({ cancelling: view.subagent_id, error: null });
  try {
    const response = await getClient().request({
      type: "subagent",
      request: { type: "cancel_subagent", subagent_id: view.subagent_id },
    });
    if (response.type === "subagent") {
      upsert(response.subagent.parent_thread_id, response.subagent);
    }
    pushToast({ variant: "success", title: "Subagent cancelled." });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    patch({ error: message });
    pushToast({ variant: "error", title: "Could not cancel.", description: message });
  } finally {
    patch({ cancelling: null });
  }
}

/** Forgets a finished row locally; the daemon keeps its record. */
export function dismissSubagent(threadId: ThreadId, id: SubagentId): void {
  remove(threadId, id);
}
