/**
 * Notification choices, per device.
 *
 * The same daemon can be open on a laptop, a browser and a phone; each device
 * decides for itself what should interrupt. Like the terminal theme, the
 * choice lives in this device's `localStorage` and is applied through a
 * `useSyncExternalStore` hook so the settings page and the notifier read the
 * same value.
 */

import { useSyncExternalStore } from "react";

export type NotificationSettingKey = "approvals" | "finishedTurns" | "hostOffline";

export type NotificationSettings = Record<NotificationSettingKey, boolean>;

export const NOTIFICATION_DEFAULTS: NotificationSettings = {
  approvals: true,
  finishedTurns: true,
  hostOffline: false,
};

const STORAGE_KEY = "h4rn.notifications";

function coerce(value: unknown): NotificationSettings {
  if (typeof value !== "object" || value === null) return { ...NOTIFICATION_DEFAULTS };
  const candidate = value as Partial<Record<NotificationSettingKey, unknown>>;
  return {
    approvals:
      typeof candidate.approvals === "boolean"
        ? candidate.approvals
        : NOTIFICATION_DEFAULTS.approvals,
    finishedTurns:
      typeof candidate.finishedTurns === "boolean"
        ? candidate.finishedTurns
        : NOTIFICATION_DEFAULTS.finishedTurns,
    hostOffline:
      typeof candidate.hostOffline === "boolean"
        ? candidate.hostOffline
        : NOTIFICATION_DEFAULTS.hostOffline,
  };
}

function read(): NotificationSettings {
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (!raw) return { ...NOTIFICATION_DEFAULTS };
    return coerce(JSON.parse(raw) as unknown);
  } catch {
    return { ...NOTIFICATION_DEFAULTS };
  }
}

let current: NotificationSettings =
  typeof window === "undefined" ? { ...NOTIFICATION_DEFAULTS } : read();

const listeners = new Set<() => void>();

export function getNotificationSettings(): NotificationSettings {
  return current;
}

export function setNotificationSetting(key: NotificationSettingKey, enabled: boolean): void {
  current = { ...current, [key]: enabled };
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(current));
  } catch {
    // Non-fatal: the choice still applies for this session.
  }
  for (const listener of listeners) listener();
}

export function useNotificationSettings(): NotificationSettings {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => current,
    () => current,
  );
}

/**
 * What a notification switch shows for the current OS permission.
 *
 * A `default` permission means the OS has not been asked yet, so every switch
 * reads off even when this device's stored preference is on: showing it on
 * would promise a notification the OS will not deliver. The preference is
 * kept, and turning a switch on is what asks. `denied` has no switch at all.
 */
export function displayedNotificationSetting(
  permission: "granted" | "denied" | "default" | "unknown",
  stored: boolean,
): boolean {
  if (permission === "default" || permission === "denied") return false;
  return stored;
}

/** What the Host offline row can say about this device's relay presence. */
export type HostOfflineVisibility =
  | { kind: "hidden" }
  | { kind: "available" }
  | { kind: "no-links" }
  | { kind: "relay-too-old" };

/**
 * Whether the Host offline row belongs on Settings.
 *
 * Nothing is known before the account's first answer, so the row and its
 * explanation both stay away: a flash of "no linked host" would claim the
 * device has no links when the fetch simply has not finished. A relay that
 * ignores `?linked=true` cannot say which machines are linked, so it gets its
 * own note instead of pretending the whole account belongs here.
 */
export function hostOfflineVisibility(remote: {
  phase: string;
  signedIn: boolean;
  linkedSupported: boolean;
  linkedMachineCount: number;
}): HostOfflineVisibility {
  if (remote.phase === "loading") return { kind: "hidden" };
  if (remote.signedIn && remote.linkedSupported && remote.linkedMachineCount > 0) {
    return { kind: "available" };
  }
  if (remote.signedIn && !remote.linkedSupported) return { kind: "relay-too-old" };
  return { kind: "no-links" };
}
