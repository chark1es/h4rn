/**
 * Android device alerts: registration with Connect and the foreground route
 * (ADR-049).
 *
 * The daemon publishes transitions; Connect decides and sends them; this module
 * only makes the phone reachable. The chosen state is this device's own, so it
 * lives in `localStorage`; the authoritative registration is the relay row it
 * creates, and the FCM token is rotated by re-registering on every launch.
 */

import { remoteStore } from "@/features/remote/remoteStore";
import { currentConnectClient } from "@/features/remote/session";
import {
  type DevicePushPermission,
  deviceAlertsShell,
  nativePushStatus,
  nativePushToken,
  nativeRequestPushPermission,
  setForegroundThread,
} from "@/lib/platform/push";
import { store } from "@/lib/store/appStore";

const ENABLED_KEY = "h4rn.push.deviceAlerts";
const PACKAGE_NAME = "dev.h4rn.app";

export type DeviceAlertsState = {
  /** This is the Android shell, so the row belongs on Settings. */
  supported: boolean;
  /** The build has a Firebase project. */
  available: boolean;
  permission: DevicePushPermission;
  /** This device's switch; only meaningful while signed in. */
  enabled: boolean;
  /** A successful registration is known to exist for this install. */
  registered: boolean;
  /** The signed-in relay has an FCM service account configured. */
  configured: boolean;
  token: string | null;
  error: string | null;
};

let started = false;
let lastRegistered = false;

/** One sentence about the phone's real notification state. */
export function deviceStatusLine(device: DeviceAlertsState | null, signedIn: boolean): string {
  if (!signedIn) return "Sign in to Connect first.";
  if (device === null) return "Checking…";
  if (!device.available) return "Unavailable in this build.";
  if (device.permission === "denied") return "Blocked in system settings.";
  if (device.permission === "default")
    return "Android will ask for permission when you turn this on.";
  if (device.enabled && device.registered) return "Registered with this phone.";
  if (device.enabled) return "Registered, but Connect has not confirmed this phone yet.";
  return "Off.";
}

export function devicePushEnabled(): boolean {
  try {
    return window.localStorage.getItem(ENABLED_KEY) === "1";
  } catch {
    return false;
  }
}

function rememberEnabled(enabled: boolean): void {
  try {
    if (enabled) window.localStorage.setItem(ENABLED_KEY, "1");
    else window.localStorage.removeItem(ENABLED_KEY);
  } catch {
    // A locked-down webview still keeps the in-memory registration.
  }
}

function describe(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "object" && error !== null && "message" in error) {
    return String((error as { message: unknown }).message);
  }
  return String(error);
}

/** Reads the true state from the shell and, when signed in, the relay. */
export async function readDeviceAlerts(): Promise<DeviceAlertsState> {
  const supported = deviceAlertsShell();
  if (!supported) {
    return {
      supported: false,
      available: false,
      permission: "unknown",
      enabled: devicePushEnabled(),
      registered: false,
      configured: false,
      token: null,
      error: null,
    };
  }
  const status = await nativePushStatus();
  let configured = false;
  let registered = lastRegistered;
  const client = currentConnectClient();
  if (client) {
    try {
      const relay = await client.pushStatus();
      configured = relay.configured;
      registered = devicePushEnabled() && relay.devices > 0;
      lastRegistered = registered;
    } catch (error) {
      return {
        supported,
        available: status?.available ?? false,
        permission: status?.permission ?? "unknown",
        enabled: devicePushEnabled(),
        registered,
        configured: false,
        token: status?.token ?? null,
        error: describe(error),
      };
    }
  }
  return {
    supported,
    available: status?.available ?? false,
    permission: status?.permission ?? "unknown",
    enabled: devicePushEnabled(),
    registered,
    configured,
    token: status?.token ?? null,
    error: null,
  };
}

/** Registers this install with Connect, rotating any old token. */
export async function registerDeviceAlerts(): Promise<void> {
  const client = currentConnectClient();
  if (!client) throw new Error("Sign in to Connect first.");
  const status = await nativePushStatus();
  if (!status?.available) {
    throw new Error("This build has no Firebase configuration, so alerts cannot be delivered.");
  }
  const token = await nativePushToken();
  await client.registerPushDevice({
    deviceId: client.device.id,
    token,
    packageName: PACKAGE_NAME,
    appVersion: status.appVersion,
  });
  lastRegistered = true;
}

/** Turns the switch on, asking for permission and registering. */
export async function enableDeviceAlerts(): Promise<DeviceAlertsState> {
  await nativeRequestPushPermission();
  const status = await nativePushStatus();
  if (status?.permission !== "granted") {
    return readDeviceAlerts();
  }
  rememberEnabled(true);
  try {
    await registerDeviceAlerts();
  } catch (error) {
    rememberEnabled(false);
    const state = await readDeviceAlerts();
    return { ...state, enabled: false, error: describe(error) };
  }
  return readDeviceAlerts();
}

/** Turns the switch off and removes the relay registration. */
export async function disableDeviceAlerts(): Promise<DeviceAlertsState> {
  rememberEnabled(false);
  const client = currentConnectClient();
  if (client) {
    try {
      await client.deregisterPushDevice(client.device.id);
    } catch {
      // Signing out removes it server-side too; the local switch still wins.
    }
  }
  lastRegistered = false;
  return readDeviceAlerts();
}

/**
 * Best-effort deregistration before credentials are forgotten at sign-out.
 *
 * Also performed by `POST /v1/auth/logout`; this keeps the removal immediate
 * when the sign-out starts from this phone.
 */
export function deregisterDeviceAlertsOnSignOut(): void {
  const client = currentConnectClient();
  if (!client || !devicePushEnabled()) return;
  lastRegistered = false;
  void client.deregisterPushDevice(client.device.id).catch(() => undefined);
}

/**
 * Starts the foreground route watcher and re-registers after sign-in.
 *
 * Called once from the app shell. Re-registering on launch is what rotates an
 * FCM token that Google replaced while the app was closed.
 */
export function startDevicePush(): void {
  if (started || !deviceAlertsShell()) return;
  started = true;

  let currentThread: string | null = null;
  const syncRoute = (): void => {
    const route = store.getState().ui.route;
    const thread = route.kind === "thread" ? route.threadId : null;
    if (thread === currentThread) return;
    currentThread = thread;
    void setForegroundThread(thread);
  };
  syncRoute();
  store.subscribe(syncRoute);

  let lastPhase = "";
  const syncAccount = (): void => {
    const { phase } = remoteStore.getState();
    if (phase === lastPhase) return;
    lastPhase = phase;
    if (phase !== "signed_in" || !devicePushEnabled()) return;
    void (async () => {
      const status = await nativePushStatus();
      if (status?.permission !== "granted") return;
      await registerDeviceAlerts();
    })().catch(() => undefined);
  };
  syncAccount();
  remoteStore.subscribe(syncAccount);

  // An FCM token can rotate while the app is in the background. Re-register
  // when the app comes back, so the relay stops sending to the dead token.
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState !== "visible") return;
    if (!devicePushEnabled() || remoteStore.getState().phase !== "signed_in") return;
    void registerDeviceAlerts().catch(() => undefined);
  });
}
