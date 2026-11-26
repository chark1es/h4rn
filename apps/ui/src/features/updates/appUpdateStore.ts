import { useSyncExternalStore } from "react";

import { i18n } from "@/lib/i18n";

import {
  type AppUpdateBridge,
  type AppUpdateProgress,
  type AppUpdateStatus,
  appUpdateBridge,
} from "@/lib/platform/appUpdate";

const AUTO_KEY = "h4rn.update.auto";

export type AppUpdateState = {
  /** The shell can update itself (desktop only). */
  supported: boolean;
  currentVersion: string | null;
  availableVersion: string | null;
  checkedAtMs: number | null;
  autoCheck: boolean;
  checking: boolean;
  /** "Later" was pressed; stays true until the next check reports back. */
  snoozed: boolean;
  progress: AppUpdateProgress | null;
  /** Why the last check failed; `appUpdateErrorText` words it. */
  error: AppUpdateError | null;
};

/** The shell's own sentence, shown as sent, or the client's kind. */
export type AppUpdateError = { kind: "sent"; text: string } | { kind: "unreachable" };

/** The check failure in the active language; the shell's text is shown as sent. */
export function appUpdateErrorText(error: AppUpdateError): string {
  return error.kind === "sent" ? error.text : i18n.t("shell:update.unreachable");
}

const initialState: AppUpdateState = {
  supported: false,
  currentVersion: null,
  availableVersion: null,
  checkedAtMs: null,
  autoCheck: true,
  checking: false,
  snoozed: false,
  progress: null,
  error: null,
};

type Listener = () => void;

let state = initialState;
let bridge: AppUpdateBridge | null = null;
let started = false;
const listeners = new Set<Listener>();

function patch(next: Partial<AppUpdateState>): void {
  state = { ...state, ...next };
  for (const listener of listeners) listener();
}

export function getAppUpdateState(): AppUpdateState {
  return state;
}

export function useAppUpdate<T>(selector: (value: AppUpdateState) => T): T {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => selector(state),
    () => selector(state),
  );
}

/** The notice shows for an offered release the person has not snoozed. */
export function noticeVisible(value: AppUpdateState): boolean {
  return value.supported && value.availableVersion !== null && !value.snoozed;
}

/** Applies a status from the shell. A fresh report ends any snooze. */
export function applyStatus(status: AppUpdateStatus): void {
  patch({
    currentVersion: status.current_version,
    availableVersion: status.available?.version ?? null,
    checkedAtMs: status.checked_at_ms,
    autoCheck: status.auto_check,
    error: status.error ? { kind: "sent", text: status.error } : null,
    checking: false,
    snoozed: false,
  });
}

function readStoredAuto(): boolean {
  try {
    return window.localStorage.getItem(AUTO_KEY) !== "off";
  } catch {
    return true;
  }
}

/** Connects the store to the shell once. Does nothing on web and mobile. */
export async function startAppUpdates(): Promise<void> {
  if (started) return;
  started = true;
  bridge = appUpdateBridge();
  if (!bridge) return;
  const active = bridge;
  try {
    const auto = readStoredAuto();
    // The saved choice wins over the shell's default before its first check.
    const status = await active.setAuto(auto);
    patch({ supported: true });
    applyStatus(status);
    await active.onChecked(applyStatus);
    await active.onProgress((progress) => patch({ progress }));
  } catch {
    // A shell without the updater commands behaves like a web build.
    bridge = null;
    patch({ supported: false });
  }
}

export async function checkForAppUpdates(): Promise<void> {
  if (!bridge) return;
  patch({ checking: true });
  try {
    applyStatus(await bridge.check());
  } catch {
    patch({ checking: false, error: { kind: "unreachable" } });
  }
}

export async function setAutoCheck(enabled: boolean): Promise<void> {
  patch({ autoCheck: enabled });
  try {
    window.localStorage.setItem(AUTO_KEY, enabled ? "on" : "off");
  } catch {
    // Storage can be blocked; the shell still honours this session's choice.
  }
  try {
    await bridge?.setAuto(enabled);
  } catch {
    // The toggle is advisory; the next launch re-sends it.
  }
}

/** "Later": hide the notice until the next check reports back. */
export function snoozeAppUpdate(): void {
  patch({ snoozed: true });
}

export async function installAppUpdate(): Promise<void> {
  if (!bridge || state.progress?.phase === "downloading") return;
  patch({ progress: { phase: "downloading", downloaded: 0, total: null } });
  try {
    await bridge.install();
  } catch {
    // The shell also emits a `failed` progress with the sentence to show.
    if (state.progress?.phase !== "failed") {
      patch({
        // The shell writes its own sentence in the same field; this one is
        // worded when it is raised, like the shell's.
        progress: { phase: "failed", message: i18n.t("shell:update.installFailed") },
      });
    }
  }
}

export function resetAppUpdateStoreForTests(): void {
  state = initialState;
  bridge = null;
  started = false;
  for (const listener of listeners) listener();
}
