/**
 * Discovery and import of conversations Claude Code and Codex already saved.
 *
 * The daemon scans real transcripts; this store only shows what it found and
 * relays the person's selection. Progress arrives as transient frames, so a
 * run keeps going if the dialog is closed and a reopening view asks the daemon
 * for the current state.
 */
import { useSyncExternalStore } from "react";

import type { ClientRequest, ImportProgress, ImportScan, ResponseBody } from "@/gen";
import { toProtoError } from "@/lib/client/errors";
import { getClient, getState, pushToast } from "@/lib/store/appStore";
import { loadProjects } from "./store";

export type ImportState = {
  scan: ImportScan | null;
  loading: boolean;
  error: string | null;
  /** Keys the person checked. */
  selected: string[];
  /** Whether every found project is listed, not only the recommended ones. */
  reviewAll: boolean;
  /** The current or most recent run. */
  progress: ImportProgress | null;
  /** A start that the daemon refused. */
  starting: boolean;
};

let state: ImportState = {
  scan: null,
  loading: false,
  error: null,
  selected: [],
  reviewAll: false,
  progress: null,
  starting: false,
};

const listeners = new Set<() => void>();

function set(patch: Partial<ImportState>): void {
  state = { ...state, ...patch };
  for (const listener of listeners) listener();
}

export function useImportState<T>(selector: (value: ImportState) => T): T {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => selector(state),
    () => selector(state),
  );
}

export function getImportState(): ImportState {
  return state;
}

/** Resets the module-level store for a unit test. */
export function resetImportStoreForTests(patch: Partial<ImportState> = {}): void {
  state = {
    scan: null,
    loading: false,
    error: null,
    selected: [],
    reviewAll: false,
    progress: null,
    starting: false,
    ...patch,
  };
  wasRunning = false;
  for (const listener of listeners) listener();
}

/** Projects that are shown for the current review mode. */
export function visibleProjects(scan: ImportScan | null, reviewAll: boolean) {
  if (!scan) return [];
  return reviewAll ? scan.projects : scan.projects.filter((project) => project.recommended);
}

/** Loads the scan and checks the projects the daemon recommends. */
export async function loadImportScan(): Promise<void> {
  const host = getState().localHostId;
  if (!host) {
    set({ error: "The daemon has not reported its host yet." });
    return;
  }
  set({ loading: true, error: null });
  try {
    const response = (await getClient().request({
      type: "scan_session_imports",
      host,
    } as ClientRequest)) as ResponseBody;
    if (response.type !== "session_imports") {
      throw new Error(`Unexpected answer \`${response.type}\`.`);
    }
    const recommended = response.scan.projects
      .filter((project) => project.recommended && project.importable > 0)
      .map((project) => project.key);
    const known = new Set(state.selected);
    set({
      scan: response.scan,
      loading: false,
      error: null,
      selected: state.reviewAll
        ? response.scan.projects
            .filter((project) => known.has(project.key) && project.importable > 0)
            .map((project) => project.key)
        : recommended,
    });
    // A reopened dialog may land mid-run; the daemon keeps the current state.
    void refreshStatus();
  } catch (error) {
    const proto = toProtoError(error);
    set({
      loading: false,
      error: proto.hint ? `${proto.message}. ${proto.hint}` : proto.message,
    });
  }
}

async function refreshStatus(): Promise<void> {
  const host = getState().localHostId;
  if (!host) return;
  try {
    const response = (await getClient().request({
      type: "session_import_status",
      host,
    } as ClientRequest)) as ResponseBody;
    if (response.type === "session_import_status" && response.progress) {
      set({ progress: response.progress });
    }
  } catch {
    // Progress is informational; a failed read leaves the last frame in place.
  }
}

export function setReviewAll(reviewAll: boolean): void {
  const shown = visibleProjects(state.scan, reviewAll).map((project) => project.key);
  const selected = reviewAll ? state.selected : state.selected.filter((key) => shown.includes(key));
  set({ reviewAll, selected });
}

export function toggleImportProject(key: string): void {
  const selected = state.selected.includes(key)
    ? state.selected.filter((entry) => entry !== key)
    : [...state.selected, key];
  set({ selected: selected.filter((entry) => shownKeys().includes(entry)) });
}

export function selectAllImportProjects(): void {
  set({ selected: shownKeys().filter((key) => importableByKey(key) > 0) });
}

export function selectNoImportProjects(): void {
  set({ selected: [] });
}

function shownKeys(): string[] {
  return visibleProjects(state.scan, state.reviewAll).map((project) => project.key);
}

function importableByKey(key: string): number {
  return state.scan?.projects.find((project) => project.key === key)?.importable ?? 0;
}

/** Starts importing the checked projects. */
export async function startImport(): Promise<void> {
  const host = getState().localHostId;
  if (!host) {
    pushToast({
      title: "Couldn't start the import.",
      description: "The daemon has not reported its host yet.",
      variant: "error",
    });
    return;
  }
  if (state.selected.length === 0) {
    pushToast({
      title: "Choose a project first.",
      description: "Check at least one project to import.",
      variant: "info",
    });
    return;
  }
  set({ starting: true });
  try {
    const response = (await getClient().request({
      type: "import_sessions",
      host,
      projects: state.selected,
    } as ClientRequest)) as ResponseBody;
    if (response.type !== "session_import_status" || !response.progress) {
      throw new Error(`Unexpected answer \`${response.type}\`.`);
    }
    set({ progress: response.progress, starting: false });
  } catch (error) {
    const proto = toProtoError(error);
    set({ starting: false });
    pushToast({
      title: "Couldn't start the import.",
      description: proto.hint ? `${proto.message}. ${proto.hint}` : proto.message,
      variant: "error",
    });
  }
}

let wasRunning = false;

/** Applies one transient progress frame. */
export function applyImportEvent(progress: ImportProgress): void {
  const finished = wasRunning && !progress.running;
  wasRunning = progress.running;
  set({ progress });
  if (finished) {
    // The imported threads arrived as ordinary journal events; refresh what
    // the dialog shows and register the newly added projects.
    void loadProjects();
    void loadImportScan();
  }
}
