/**
 * New-thread defaults, per device.
 *
 * Like the terminal theme and notification switches, the choice lives in this
 * device's `localStorage`: it is a preference of the person at this client, not
 * a daemon setting. The new-thread dialog and the thread controls read the same
 * value through `useSyncExternalStore`.
 */

import { useSyncExternalStore } from "react";

import type { RuntimeMode } from "@/gen";

export type ThreadSettings = {
  /** Permission mode a new thread starts on. */
  defaultPermissionMode: RuntimeMode;
};

export const THREAD_SETTINGS_DEFAULTS: ThreadSettings = {
  defaultPermissionMode: "approval_required",
};

const STORAGE_KEY = "h4rn.threads";

const MODES: RuntimeMode[] = ["approval_required", "auto_accept_edits", "plan", "full_access"];

function coerce(value: unknown): ThreadSettings {
  if (typeof value !== "object" || value === null) return { ...THREAD_SETTINGS_DEFAULTS };
  const candidate = value as Partial<Record<keyof ThreadSettings, unknown>>;
  return {
    defaultPermissionMode: MODES.includes(candidate.defaultPermissionMode as RuntimeMode)
      ? (candidate.defaultPermissionMode as RuntimeMode)
      : THREAD_SETTINGS_DEFAULTS.defaultPermissionMode,
  };
}

function read(): ThreadSettings {
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (!raw) return { ...THREAD_SETTINGS_DEFAULTS };
    return coerce(JSON.parse(raw) as unknown);
  } catch {
    return { ...THREAD_SETTINGS_DEFAULTS };
  }
}

let current: ThreadSettings =
  typeof window === "undefined" ? { ...THREAD_SETTINGS_DEFAULTS } : read();

const listeners = new Set<() => void>();

export function getThreadSettings(): ThreadSettings {
  return current;
}

export function setDefaultPermissionMode(mode: RuntimeMode): void {
  current = { ...current, defaultPermissionMode: mode };
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(current));
  } catch {
    // Non-fatal: the choice still applies for this session.
  }
  for (const listener of listeners) listener();
}

export function useThreadSettings(): ThreadSettings {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => current,
    () => current,
  );
}
