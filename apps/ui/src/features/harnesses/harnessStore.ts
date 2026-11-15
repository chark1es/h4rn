import { useEffect, useSyncExternalStore } from "react";

import type { HarnessKind, HarnessUpdateEvent, HarnessVersionInfo, ProtoError } from "@/gen";
import { toProtoError } from "@/lib/client/errors";
import { onHarnessFrame } from "@/lib/client/frames";
import { getClient } from "@/lib/store/appStore";

/** Output kept per harness; an update prints a handful of lines, not thousands. */
const MAX_OUTPUT_LINES = 200;

export type HarnessResult = { ok: boolean; message: string };

export type HarnessState = {
  rows: HarnessVersionInfo[];
  loaded: boolean;
  loading: boolean;
  error: ProtoError | null;
  updating: Partial<Record<HarnessKind, boolean>>;
  output: Partial<Record<HarnessKind, string[]>>;
  results: Partial<Record<HarnessKind, HarnessResult>>;
};

const initialState: HarnessState = {
  rows: [],
  loaded: false,
  loading: false,
  error: null,
  updating: {},
  output: {},
  results: {},
};

type Listener = () => void;

let state = initialState;
const listeners = new Set<Listener>();
let unsubscribeFrames: (() => void) | null = null;

function patch(next: Partial<HarnessState>): void {
  state = { ...state, ...next };
  for (const listener of listeners) listener();
}

export function getHarnessState(): HarnessState {
  return state;
}

export function useHarnesses<T>(selector: (value: HarnessState) => T): T {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => selector(state),
    () => selector(state),
  );
}

/** Harnesses whose installed copy is older than the registry's. */
export function harnessUpdateCount(value: HarnessState): number {
  return value.rows.filter((row) => row.update_available).length;
}

/** Applies one streamed update event. */
export function applyHarnessEvent(event: HarnessUpdateEvent): void {
  if (event.type === "output") {
    const lines = [...(state.output[event.harness] ?? []), event.line].slice(-MAX_OUTPUT_LINES);
    patch({ output: { ...state.output, [event.harness]: lines } });
    return;
  }
  patch({
    updating: { ...state.updating, [event.harness]: false },
    results: { ...state.results, [event.harness]: { ok: event.ok, message: event.message } },
  });
  // The daemon re-detected the version and dropped its cache; read it back.
  void loadHarnessVersions();
}

/** Subscribes to update output once, however many screens ask. */
export function ensureHarnessEvents(): void {
  if (unsubscribeFrames) return;
  unsubscribeFrames = onHarnessFrame(applyHarnessEvent);
}

export async function loadHarnessVersions(): Promise<void> {
  patch({ loading: true, error: null });
  try {
    const response = await getClient().request({ type: "list_harness_versions" });
    if (response.type !== "harness_versions") return;
    patch({ rows: response.harnesses, loaded: true, loading: false, error: null });
  } catch (error) {
    patch({ loaded: true, loading: false, error: toProtoError(error) });
  }
}

/** Loads the list once per mount; the Settings nav and the panel both call it. */
export function useHarnessVersions(): void {
  useEffect(() => {
    ensureHarnessEvents();
    if (!state.loaded && !state.loading) void loadHarnessVersions();
  }, []);
}

export async function updateHarness(harness: HarnessKind): Promise<void> {
  ensureHarnessEvents();
  const { [harness]: _result, ...results } = state.results;
  patch({
    updating: { ...state.updating, [harness]: true },
    output: { ...state.output, [harness]: [] },
    results,
  });
  try {
    await getClient().request({ type: "update_harness", harness });
  } catch (error) {
    const proto = toProtoError(error);
    patch({
      updating: { ...state.updating, [harness]: false },
      results: {
        ...state.results,
        [harness]: {
          ok: false,
          message: proto.hint ? `${proto.message} ${proto.hint}` : proto.message,
        },
      },
    });
  }
}

export function resetHarnessStoreForTests(): void {
  unsubscribeFrames?.();
  unsubscribeFrames = null;
  state = initialState;
  for (const listener of listeners) listener();
}
