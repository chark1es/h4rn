/**
 * The native bridge for Android device alerts (ADR-049).
 *
 * Only an Android Tauri shell has the plugin. Every function is inert
 * elsewhere, so the shared UI can call it unconditionally; Settings hides the
 * device row when {@link deviceAlertsShell} is false.
 */

type TauriInternals = {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
};

export type DevicePushPermission = "granted" | "denied" | "default" | "unknown";

export type NativePushStatus = {
  /** This build has a Firebase project configured. */
  available: boolean;
  permission: DevicePushPermission;
  /** Cached FCM token, when the plugin has one. */
  token: string | null;
  /** The notification channel exists (created by the plugin). */
  channel: boolean;
  /** The shell's own version, sent with registration. */
  appVersion: string;
};

function internals(): TauriInternals | null {
  const candidate = (window as unknown as { __TAURI_INTERNALS__?: TauriInternals })
    .__TAURI_INTERNALS__;
  return typeof candidate?.invoke === "function" ? candidate : null;
}

/** Whether this is an Android shell that can carry alerts. */
export function deviceAlertsShell(): boolean {
  return internals() !== null && /Android/i.test(window.navigator.userAgent);
}

function describe(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "object" && error !== null && "message" in error) {
    return String((error as { message: unknown }).message);
  }
  return String(error);
}

export async function nativePushStatus(): Promise<NativePushStatus | null> {
  const api = internals();
  if (!api || !deviceAlertsShell()) return null;
  try {
    const status = await api.invoke<NativePushStatus>("push_status");
    return {
      available: status.available === true,
      permission: status.permission ?? "unknown",
      token: status.token ?? null,
      channel: status.channel === true,
      appVersion: status.appVersion ?? "",
    };
  } catch {
    return null;
  }
}

/** Asks the OS for notification permission and waits for the system dialog. */
export async function nativeRequestPushPermission(): Promise<void> {
  const api = internals();
  if (!api || !deviceAlertsShell()) return;
  await api.invoke<null>("push_request_permission");
}

/** Reads the current FCM token, failing with a readable message. */
export async function nativePushToken(): Promise<string> {
  const api = internals();
  if (!api || !deviceAlertsShell()) {
    throw new Error("Device alerts are only available in the Android app.");
  }
  try {
    return await api.invoke<string>("push_token");
  } catch (error) {
    throw new Error(describe(error));
  }
}

/**
 * Names the thread currently on screen, or null.
 *
 * The notification service suppresses an alert only when this thread matches
 * and the app is in front; every other thread still alerts.
 */
export async function setForegroundThread(threadId: string | null): Promise<void> {
  const api = internals();
  if (!api || !deviceAlertsShell()) return;
  try {
    await api.invoke<null>("push_set_foreground_thread", { threadId });
  } catch {
    // A shell without the plugin is the same as no alerts; nothing to report.
  }
}
