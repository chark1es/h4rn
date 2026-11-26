/**
 * The native bridge for phone alerts (ADR-051, ADR-052).
 *
 * Only a mobile Tauri shell has the plugin: FCM on Android, APNs on iOS. Every
 * function is inert elsewhere, so the shared UI can call it unconditionally;
 * Settings hides the device row when {@link deviceAlertsShell} is false.
 * Android and iOS expose the same commands and response shapes, so nothing
 * here branches on the platform beyond detecting it.
 */

type TauriInternals = {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
};

export type DevicePushPermission = "granted" | "denied" | "default" | "unknown";

export type NativePushStatus = {
  /** This build can receive alerts (Firebase configured on Android). */
  available: boolean;
  permission: DevicePushPermission;
  /** The push token the shell cached: FCM on Android, APNs on iOS. */
  token: string | null;
  /** The notification channel exists (Android; always true on iOS). */
  channel: boolean;
  /** The shell's own version, sent with registration. */
  appVersion: string;
};

/**
 * What the shell said about this phone.
 *
 * A failed command is its own answer: it says nothing about whether the build
 * can carry alerts, so the UI reports "unknown" and lets the person try again
 * instead of claiming the build cannot carry them.
 */
export type NativePushStatusResult =
  | { kind: "checked"; status: NativePushStatus }
  | { kind: "failed"; error: string }
  | { kind: "unsupported" };

function internals(): TauriInternals | null {
  const candidate = (window as unknown as { __TAURI_INTERNALS__?: TauriInternals })
    .__TAURI_INTERNALS__;
  return typeof candidate?.invoke === "function" ? candidate : null;
}

/** The phone platform this shell runs on, or null when it cannot carry alerts. */
export function deviceAlertsPlatform(): "android" | "ios" | null {
  if (internals() === null) return null;
  const ua = window.navigator.userAgent;
  if (/Android/i.test(ua)) return "android";
  if (/iPhone|iPad|iPod/i.test(ua)) return "ios";
  return null;
}

/** Whether this is a phone shell that can carry alerts. */
export function deviceAlertsShell(): boolean {
  return deviceAlertsPlatform() !== null;
}

function describe(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "object" && error !== null && "message" in error) {
    return String((error as { message: unknown }).message);
  }
  return String(error);
}

export async function nativePushStatus(): Promise<NativePushStatusResult> {
  const api = internals();
  if (!api || !deviceAlertsShell()) return { kind: "unsupported" };
  try {
    const status = await api.invoke<NativePushStatus>("push_status");
    return {
      kind: "checked",
      status: {
        available: status.available === true,
        permission: status.permission ?? "unknown",
        token: status.token ?? null,
        channel: status.channel === true,
        appVersion: status.appVersion ?? "",
      },
    };
  } catch (error) {
    return { kind: "failed", error: describe(error) };
  }
}

/** Asks the OS for notification permission and waits for the system dialog. */
export async function nativeRequestPushPermission(): Promise<void> {
  const api = internals();
  if (!api || !deviceAlertsShell()) return;
  await api.invoke<null>("push_request_permission");
}

/** Reads the current push token (FCM on Android, APNs on iOS). */
export async function nativePushToken(): Promise<string> {
  const api = internals();
  if (!api || !deviceAlertsShell()) {
    throw new Error("Device alerts are only available in the h4rn phone app.");
  }
  try {
    return await api.invoke<string>("push_token");
  } catch (error) {
    throw new Error(describe(error));
  }
}

/**
 * Reads the thread link from a notification tap that cold-started the app.
 *
 * iOS has no page to dispatch `h4rn-deep-link` to while the app is launching,
 * so its delegate holds the tap until the shared UI asks here, once, after the
 * deep-link listener exists. Resolves null for an ordinary launch, a warm tap
 * (which is dispatched as an event) and a shell without the plugin.
 */
export async function nativeTakePendingPushLink(): Promise<string | null> {
  const api = internals();
  if (!api || !deviceAlertsShell()) return null;
  try {
    const payload = await api.invoke<{ url?: string | null }>("push_take_pending_link");
    const url = payload?.url;
    return typeof url === "string" && url.length > 0 ? url : null;
  } catch {
    // An older shell has no buffered tap; the deep-link plugin still handles
    // links that arrive while the page is running.
    return null;
  }
}

/**
 * Mirrors the local switch to the Android notification service.
 *
 * The service runs without the WebView, so it reads this persisted flag before
 * showing a data message: an Off switch keeps the phone quiet even when its
 * relay row could not be removed.
 */
export async function nativeSetPushEnabled(enabled: boolean): Promise<void> {
  const api = internals();
  if (!api || !deviceAlertsShell()) return;
  try {
    await api.invoke<null>("push_set_enabled", { enabled });
  } catch {
    // An older shell without the command still registers and deregisters;
    // the relay row remains the first line of defence.
  }
}

/**
 * Names the thread currently on screen, or null.
 *
 * The notification service suppresses an alert only when this thread matches
 * and the app is in front; every other thread still alerts, and a grouped
 * alert is never suppressed.
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
