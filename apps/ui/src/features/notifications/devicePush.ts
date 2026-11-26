/**
 * Phone alerts: registration with Connect and the foreground route
 * (ADR-051, ADR-052).
 *
 * The daemon publishes transitions; Connect decides and sends them; this module
 * only makes the phone reachable. The chosen state is this device's own, so it
 * lives in `localStorage`; the authoritative registration is the relay row it
 * creates. The shell asks APNs or FCM for a fresh token on every launch, and
 * this module re-registers with that token on launch and whenever the app
 * returns to the foreground, so a token either provider replaced reaches the
 * relay instead of a cached one staying there forever.
 */

import { remoteStore } from "@/features/remote/remoteStore";
import { currentConnectClient } from "@/features/remote/session";
import { i18n } from "@/lib/i18n";
import {
  type DevicePushPermission,
  deviceAlertsPlatform,
  deviceAlertsShell,
  nativeCancelStoppedLiveThreads,
  nativePushStatus,
  nativePushToken,
  nativeRequestPushPermission,
  nativeSetLiveUpdates,
  nativeSetPushEnabled,
  setForegroundThread,
} from "@/lib/platform/push";
import { store } from "@/lib/store/appStore";

const ENABLED_KEY = "h4rn.push.deviceAlerts";
const LIVE_UPDATES_KEY = "h4rn.push.liveUpdates";
const DEREGISTER_PENDING_KEY = "h4rn.push.deregisterPending";
const PACKAGE_NAME = "dev.h4rn.app";

export type DeviceAlertsState = {
  /** This is a phone shell, so the row belongs on Settings. */
  supported: boolean;
  /** The build can receive alerts; `null` when the shell did not answer. */
  available: boolean | null;
  permission: DevicePushPermission;
  /** This device's switch; only meaningful while signed in. */
  enabled: boolean;
  /** A successful registration is known to exist for this install. */
  registered: boolean;
  /** The signed-in relay has a sender for this phone's platform. */
  configured: boolean | null;
  token: string | null;
  error: string | null;
};

let started = false;
let lastRegistered = false;

/** One sentence about the phone's real notification state. */
export function deviceStatusLine(device: DeviceAlertsState | null, signedIn: boolean): string {
  if (!signedIn) return i18n.t("settings:notifications.device.signIn");
  if (device === null) return i18n.t("common:checking");
  if (device.available === false) return i18n.t("settings:notifications.device.unavailable");
  if (device.available === null) return i18n.t("settings:notifications.device.checkFailed");
  if (device.permission === "denied") return i18n.t("settings:notifications.device.blocked");
  if (device.permission === "default") return i18n.t("settings:notifications.device.willAsk");
  if (device.enabled && device.registered)
    return i18n.t("settings:notifications.device.registered");
  if (device.enabled) return i18n.t("settings:notifications.device.unconfirmed");
  return i18n.t("settings:notifications.device.off");
}

/**
 * What this Connect server's push sender state means, or `null` when it is
 * fine or not yet known.
 *
 * A failed status call is not evidence that the server has no sender; saying
 * so would send the person to their operator for the wrong reason.
 */
export function deviceAvailabilityNote(device: DeviceAlertsState | null): string | null {
  if (device === null) return null;
  if (device.configured === false) {
    return i18n.t("settings:notifications.device.noSender");
  }
  if (device.configured === null && device.error !== null) {
    return i18n.t("settings:notifications.device.senderUnknown");
  }
  return null;
}

/**
 * Whether the device switch can be flipped right now.
 *
 * An unknown answer (`available === null`) is not a build without push, so
 * the switch stays usable and the person can try again.
 */
export function deviceToggleUsable(
  device: DeviceAlertsState | null,
  signedIn: boolean,
  busy: boolean,
): boolean {
  if (!signedIn || device === null || busy) return false;
  return device.available !== false && device.permission !== "denied";
}

export function devicePushEnabled(): boolean {
  try {
    return window.localStorage.getItem(ENABLED_KEY) === "1";
  } catch {
    return false;
  }
}

/** Whether this phone can show an ongoing Live Update (Android only). */
export function deviceLiveUpdatesSupported(): boolean {
  return deviceAlertsPlatform() === "android";
}

/**
 * Whether the Live Updates switch does anything on this phone right now.
 *
 * A Live Update is one of this phone's own alerts, so it needs Device
 * notifications on and allowed; until then the switch is shown but inert.
 */
export function deviceLiveUpdatesUsable(device: DeviceAlertsState | null): boolean {
  return device?.enabled === true && device.permission === "granted";
}

/** The separate Live Updates switch; off by default. */
export function deviceLiveUpdatesEnabled(): boolean {
  try {
    return window.localStorage.getItem(LIVE_UPDATES_KEY) === "1";
  } catch {
    return false;
  }
}

/**
 * Saves the Live Updates switch and mirrors it to the notification service.
 *
 * The service runs without the WebView, so it reads the persisted flag before
 * showing a running turn; turning it off also cancels what is showing.
 */
export async function setDeviceLiveUpdates(enabled: boolean): Promise<void> {
  try {
    if (enabled) window.localStorage.setItem(LIVE_UPDATES_KEY, "1");
    else window.localStorage.removeItem(LIVE_UPDATES_KEY);
  } catch {
    // Without storage the setting applies to this session only.
  }
  await nativeSetLiveUpdates(enabled);
}

/**
 * The threads one machine's snapshot lists with no turn running, sorted.
 *
 * A Live Update for one of these is stale: the turn ended by a Stop, a grouped
 * finish that names no thread, or while the app was closed. A queued turn that
 * keeps the thread Working is not listed. Neither is any thread this machine
 * does not know: it may be another machine's agent, still running, and its
 * `working` frame comes once per turn, so cancelling it would lose it for good.
 */
export function stoppedThreads(threads: Record<string, ThreadTurnState>): string[] {
  return Object.entries(threads)
    .filter(([, thread]) => thread.tab !== "working")
    .map(([threadId]) => threadId)
    .sort();
}

type ThreadTurnState = { tab: string };

/** Whether this install is signed in to Connect right now. */
function signedIn(): boolean {
  return remoteStore.getState().phase === "signed_in";
}

/**
 * Mirrors "signed in and switched on" to the phone's notification service.
 *
 * The service runs without the WebView, so it reads this persisted flag before
 * showing a data message. Signing out must drop it even when the switch is
 * still on: the row may outlive the credentials, and the old account's titles
 * must never reach the lock screen.
 */
function mirrorNativeFlag(): void {
  void nativeSetPushEnabled(devicePushEnabled() && signedIn());
}

function rememberEnabled(enabled: boolean): void {
  try {
    if (enabled) window.localStorage.setItem(ENABLED_KEY, "1");
    else window.localStorage.removeItem(ENABLED_KEY);
  } catch {
    // A locked-down webview still keeps the in-memory registration.
  }
  mirrorNativeFlag();
}

/**
 * A removal the relay has not confirmed yet.
 *
 * Turning the switch off while the phone is offline, or a relay error, must not
 * leave a row that keeps alerting. The marker survives a restart and is retried
 * whenever this app is signed in until Connect answers.
 */
function deregistrationPending(): boolean {
  try {
    return window.localStorage.getItem(DEREGISTER_PENDING_KEY) === "1";
  } catch {
    return false;
  }
}

function rememberDeregistrationPending(pending: boolean): void {
  try {
    if (pending) window.localStorage.setItem(DEREGISTER_PENDING_KEY, "1");
    else window.localStorage.removeItem(DEREGISTER_PENDING_KEY);
  } catch {
    // Without storage, the next signed-in call still retries once per session.
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
      configured: null,
      token: null,
      error: null,
    };
  }
  const result = await nativePushStatus();
  const status = result.kind === "checked" ? result.status : null;
  // A failed command says nothing about whether this build can push.
  const available =
    result.kind === "checked" ? result.status.available : result.kind === "failed" ? null : false;
  let error = result.kind === "failed" ? result.error : null;
  let configured: boolean | null = null;
  let registered = lastRegistered;
  const client = currentConnectClient();
  if (client) {
    try {
      const relay = await client.pushStatus();
      // A relay with only one platform's sender configured must not tell the
      // other platform that alerts will arrive. Older relays have no per-
      // platform answer, so their account-wide flag is the best there is.
      configured = relay.platform_configured ?? relay.configured;
      // The relay answers about this device's own row. The account's other
      // phones say nothing about whether this one can receive an alert.
      registered = relay.registered === true;
      lastRegistered = registered;
    } catch (caught) {
      // Unknown, not "no sender": the next read can succeed.
      configured = null;
      error = describe(caught);
    }
  }
  return {
    supported,
    available,
    permission: status?.permission ?? "unknown",
    enabled: devicePushEnabled(),
    registered,
    configured,
    token: status?.token ?? null,
    error,
  };
}

/** Registers this install with Connect, rotating any old token. */
export async function registerDeviceAlerts(): Promise<void> {
  const client = currentConnectClient();
  if (!client) throw new Error(i18n.t("settings:notifications.device.signIn"));
  const result = await nativePushStatus();
  if (result.kind === "failed") throw new Error(result.error);
  if (result.kind !== "checked" || !result.status.available) {
    throw new Error(i18n.t("settings:notifications.buildCannotReceive"));
  }
  const token = await nativePushToken();
  const platform = deviceAlertsPlatform();
  await client.registerPushDevice({
    deviceId: client.device.id,
    token,
    platform: platform ?? "android",
    appVersion: result.status.appVersion,
    // Only Android has a package name; iOS delivery is addressed by the APNs
    // topic on the relay.
    ...(platform === "ios" ? {} : { packageName: PACKAGE_NAME }),
  });
  // The switch, or the whole session, may have been turned off while the PUT
  // was in flight. A fresh registration must not revive a phone the person
  // already silenced: keep the removal marker and finish the removal instead.
  if (!devicePushEnabled() || !signedIn()) {
    rememberDeregistrationPending(true);
    await reconcileDeviceDeregistration();
    return;
  }
  // A fresh registration supersedes any removal that was still pending.
  rememberDeregistrationPending(false);
  lastRegistered = true;
}

/** Turns the switch on, asking for permission and registering. */
export async function enableDeviceAlerts(): Promise<DeviceAlertsState> {
  try {
    await nativeRequestPushPermission();
  } catch (caught) {
    // A refused dialog, or a shell that could not ask, still leaves the switch
    // where it was; tell the person why instead of freezing it.
    rememberEnabled(false);
    const state = await readDeviceAlerts();
    return { ...state, enabled: false, error: describe(caught) };
  }
  const result = await nativePushStatus();
  if (result.kind === "failed") {
    rememberEnabled(false);
    const state = await readDeviceAlerts();
    return { ...state, enabled: false, error: result.error };
  }
  if (result.kind !== "checked" || result.status.permission !== "granted") {
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
    // Mark the removal before trying it: a failed request must leave a note
    // that the next signed-in visit retries, not a phone that keeps alerting
    // while its switch reads Off.
    rememberDeregistrationPending(true);
    try {
      await client.deregisterPushDevice(client.device.id);
      rememberDeregistrationPending(false);
    } catch {
      // Connect did not confirm; `reconcileDeviceDeregistration` retries.
    }
  }
  lastRegistered = false;
  return readDeviceAlerts();
}

/**
 * Retries a removal Connect has not confirmed.
 *
 * Called on launch, when the app becomes visible and when the account syncs,
 * so an offline switch-off is finished as soon as the relay is reachable. A
 * successful delete, or a relay that no longer has the row, clears the marker.
 */
export async function reconcileDeviceDeregistration(): Promise<void> {
  if (!deregistrationPending()) return;
  const client = currentConnectClient();
  if (!client) return;
  try {
    await client.deregisterPushDevice(client.device.id);
    rememberDeregistrationPending(false);
    lastRegistered = false;
  } catch {
    // Still unreachable: the next launch or visibility change tries again.
  }
}

/**
 * Best-effort deregistration before credentials are forgotten at sign-out.
 *
 * Also performed by `POST /v1/auth/logout`; this keeps the removal immediate
 * when the sign-out starts from this phone. The native flag goes off first and
 * unconditionally, because it is the one guarantee that survives a relay that
 * cannot be reached: the notification service stops showing data messages even
 * while the row is still on Connect. A removal that does not answer in time is
 * recorded and retried at the next signed-in launch.
 */
export function deregisterDeviceAlertsOnSignOut(): void {
  // The local switch is this device's preference for the next sign-in; the
  // native flag is "signed in and switched on", so it can never stay on here.
  void nativeSetPushEnabled(false);
  lastRegistered = false;
  if (!devicePushEnabled()) return;
  rememberDeregistrationPending(true);
  const client = currentConnectClient();
  if (!client) return;
  void client
    .deregisterPushDevice(client.device.id)
    .then(() => rememberDeregistrationPending(false))
    .catch(() => undefined);
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
  const foregroundThread = (): string | null => {
    const route = store.getState().ui.route;
    return route.kind === "thread" ? route.threadId : null;
  };
  const syncRoute = (): void => {
    const thread = foregroundThread();
    if (thread === currentThread) return;
    currentThread = thread;
    void setForegroundThread(thread);
  };
  syncRoute();
  store.subscribe(syncRoute);

  // The daemon publishes the finish, failure or limit itself, and that alert
  // replaces a thread's ongoing entry. A turn with no alert (the person's own
  // Stop, a grouped finish that names no thread, or one that ended while the
  // app was closed) leaves its entry behind, so every thread list the daemon
  // sends, the first one included, tells the phone which of its turns have
  // ended and it cancels those. Entries for threads the list does not name
  // stay: they may belong to another machine.
  let lastThreads: unknown = null;
  let lastSync: number | null = null;
  let lastReconciled: string | null = null;
  const syncTurns = (): void => {
    if (deviceAlertsPlatform() !== "android") return;
    const { threads, lastSyncAt, connection } = store.getState();
    // Only a thread change matters; the store also notifies for route, theme
    // and every other UI state.
    if (threads === lastThreads && lastSyncAt === lastSync) return;
    // Before the first list arrives, or while the daemon is out of reach, the
    // snapshot says nothing about which turns run: a stale list could name a
    // thread as stopped that has started a new turn since.
    if (lastSyncAt === null || connection.status !== "online") return;
    lastThreads = threads;
    lastSync = lastSyncAt;
    const stopped = stoppedThreads(threads);
    const key = `${lastSyncAt}\n${stopped.join("\n")}`;
    if (key === lastReconciled) return;
    lastReconciled = key;
    void nativeCancelStoppedLiveThreads(stopped);
  };
  syncTurns();
  store.subscribe(syncTurns);

  let lastPhase = "";
  const syncAccount = (): void => {
    const { phase } = remoteStore.getState();
    if (phase === lastPhase) return;
    lastPhase = phase;
    // Still reading the stored session: leave the native flag as the last
    // signed-in or signed-out moment set it. The phone may be woken for an
    // alert while the webview is still starting.
    if (phase === "loading") return;
    if (phase !== "signed_in") {
      // "Signed in and switched on": never re-arm while signed out, and drop
      // the flag the moment the session ends, however it ended.
      void nativeSetPushEnabled(false);
      return;
    }
    // A launch reaches "signed_in" only after the stored credentials load, so
    // the flag the launch turned off must come back on here for a phone whose
    // switch is on; otherwise its alerts would stay silenced until the switch
    // was flipped again.
    mirrorNativeFlag();
    void (async () => {
      await reconcileDeviceDeregistration();
      if (!devicePushEnabled()) return;
      const result = await nativePushStatus();
      if (result.kind !== "checked" || result.status.permission !== "granted") return;
      await registerDeviceAlerts();
    })().catch(() => undefined);
  };
  syncAccount();
  remoteStore.subscribe(syncAccount);

  // A switch-off while offline waits for a signed-in visit to finish removing
  // the row, and a token can rotate while the app is in the background.
  // Reconcile and re-register when the app comes back.
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState !== "visible") return;
    // A Live Update for the thread on screen may have been posted while the
    // app was in the background. The route did not change, so name the thread
    // again; the shell drops that thread's ongoing entry.
    currentThread = foregroundThread();
    void setForegroundThread(currentThread);
    if (remoteStore.getState().phase !== "signed_in") return;
    void (async () => {
      await reconcileDeviceDeregistration();
      if (!devicePushEnabled()) return;
      await registerDeviceAlerts();
    })().catch(() => undefined);
  });

  // Launch is one of the signed-in moments a pending removal is retried:
  // `syncAccount` above runs once here when the account is already signed in.
  // While signed out the flag stays off even if the switch was left on; while
  // the session is still loading, `syncAccount` decides once it resolves.
  if (remoteStore.getState().phase !== "loading") mirrorNativeFlag();
}
