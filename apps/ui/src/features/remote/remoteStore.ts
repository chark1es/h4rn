/**
 * Remote access state: the connect account, machines, devices and pairings.
 *
 * Transport selection is a Rust concern; this module decides *which* machine
 * and *which* account a person chose, persists the non-secret descriptor and
 * rebuilds the protocol client when that choice changes.
 */

import { useSyncExternalStore } from "react";

import type { ConnectDevice, ConnectUser, MachineSummary, PairingInfo, PairScope } from "@/gen";
import { MOCK_ALLOWED, readBootConfig } from "@/lib/client/config";
import { takeConnectPair, takeOidcCallback } from "@/lib/platform/pending";
import {
  readSessionState,
  removeSessionState,
  writeSessionState,
} from "@/lib/platform/sessionState";
import { pushToast, restartConnection } from "@/lib/store/appStore";
import {
  type ActiveRemote,
  clearActiveRemote,
  pathsFor,
  readActiveRemote,
  writeActiveRemote,
} from "./active";
import {
  base64UrlToHex,
  ConnectClient,
  type ConnectError,
  completeLogin,
  deviceRegistration,
  normalizeTokens,
  pollDeviceGrant,
  type RemoteCredentials,
  readCredentials,
  startDeviceGrant,
  startLogin,
  writeCredentials,
} from "./connect";
import { mockOffer, offerFromCreate, type PairingOffer } from "./pairingOffer";
import { DEFAULT_PAIR_SCOPES, isPairScope, orderedScopes } from "./pairingScopes";
import { loadConnectClient, resetConnectSession, setConnectClient } from "./session";

export type RemotePhase = "loading" | "signed_out" | "signing_in" | "signed_in" | "error";

export type DeviceCodeLogin = {
  grantId: string;
  userCode: string;
  verificationUri: string;
  verificationUriComplete: string | null;
  expiresAt: number;
};

export type { PairingOffer } from "./pairingOffer";

export type RemoteState = {
  phase: RemotePhase;
  connectUrl: string;
  user: ConnectUser | null;
  device: ConnectDevice | null;
  devices: ConnectDevice[];
  machines: MachineSummary[];
  pairings: PairingInfo[];
  activeMachineId: string | null;
  active: ActiveRemote | null;
  login: DeviceCodeLogin | null;
  pairing: PairingOffer | null;
  /** A typed-code pairing awaiting the person's check of the code on the host. */
  sasConfirm: SasConfirm | null;
  busy: string | null;
  error: string | null;
  lastSyncAt: number | null;
};

/**
 * What the phone shows after claiming a typed code and before it trusts the
 * daemon key connect handed over. The string is derived in Rust (wasm) from
 * this device's key and that daemon key; the host shows its own derivation.
 */
export type SasConfirm = {
  sas: string;
  machineName: string;
  active: ActiveRemote;
};

const PENDING_LOGIN_KEY = "h4rn.remote.pending-login";
const LOOPBACK_PORT = 47812;
export const DEFAULT_CONNECT_URL = "http://127.0.0.1:8787";

function shellKind(): "desktop" | "mobile" | "web" {
  if (typeof window === "undefined") return "web";
  const tauri = (window as unknown as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
  if (!tauri) return "web";
  return /Android|iPhone|iPad|iPod/i.test(window.navigator.userAgent) ? "mobile" : "desktop";
}

function tauriInvoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  const internals = (
    window as unknown as {
      __TAURI_INTERNALS__?: {
        invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
      };
    }
  ).__TAURI_INTERNALS__;
  if (!internals) throw new Error("This shell cannot open a sign-in window.");
  return internals.invoke<T>(command, args);
}

function initialConnectUrl(): string {
  return (
    readBootConfig().connectUrl ??
    readActiveRemote()?.connectUrl ??
    (readBootConfig().mock ? DEFAULT_CONNECT_URL : DEFAULT_CONNECT_URL)
  );
}

function createInitialState(): RemoteState {
  const active = readActiveRemote();
  return {
    phase: "loading",
    connectUrl: initialConnectUrl(),
    user: null,
    device: null,
    devices: [],
    machines: [],
    pairings: [],
    activeMachineId: active?.machineId ?? null,
    active,
    login: null,
    pairing: null,
    sasConfirm: null,
    busy: null,
    error: null,
    lastSyncAt: null,
  };
}

type Store = {
  getState: () => RemoteState;
  setState: (next: RemoteState) => void;
  subscribe: (listener: () => void) => () => void;
};

function createStore(initial: RemoteState): Store {
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

export const remoteStore = createStore(createInitialState());

export function useRemote<T>(selector: (state: RemoteState) => T): T {
  return useSyncExternalStore(
    remoteStore.subscribe,
    () => selector(remoteStore.getState()),
    () => selector(remoteStore.getState()),
  );
}

export function getRemoteState(): RemoteState {
  return remoteStore.getState();
}

function patch(next: Partial<RemoteState>): void {
  remoteStore.setState({ ...remoteStore.getState(), ...next });
}

function describe(error: unknown): string {
  const connect = error as ConnectError;
  return connect?.hint ?? connect?.message ?? String(error);
}

function defaultDeviceName(): string {
  const platform = shellKind();
  const ua = typeof navigator === "undefined" ? "" : navigator.userAgent;
  const browser = /Firefox\//.test(ua)
    ? "Firefox"
    : /Edg\//.test(ua)
      ? "Edge"
      : /Chrome\//.test(ua)
        ? "Chrome"
        : /Safari\//.test(ua)
          ? "Safari"
          : platform === "desktop"
            ? "Desktop"
            : "Browser";
  return `h4rn · ${browser}`;
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

let booted = false;
let mockFixtures = false;

const MOCK_MACHINE_ID = "mock-machine-atlas";
const MOCK_CONNECT_URL = DEFAULT_CONNECT_URL;

function mockMachine(
  id: string,
  name: string,
  online: boolean,
  hosts: MachineSummary["hosts"],
  addresses: string[],
): MachineSummary {
  return {
    id,
    name,
    online,
    last_seen: new Date(Date.now() - (online ? 1_000 : 3 * 60 * 60_000)).toISOString(),
    daemon_device_id: `mock-daemon-${id}`,
    hosts,
    addresses,
  };
}

function mockMachines(scenario = "default"): MachineSummary[] {
  const all = [
    mockMachine(
      MOCK_MACHINE_ID,
      "atlas",
      true,
      [
        { id: "mock-host-atlas", name: "atlas", kind: "host", distro: null },
        { id: "mock-host-wsl", name: "Ubuntu-24.04", kind: "wsl", distro: "Ubuntu-24.04" },
      ],
      ["192.168.1.24:47800", `relay:${MOCK_CONNECT_URL}`],
    ),
    mockMachine(
      "mock-machine-orion",
      "orion",
      true,
      [{ id: "mock-host-orion", name: "orion", kind: "host", distro: null }],
      [`relay:${MOCK_CONNECT_URL}`],
    ),
    mockMachine(
      "mock-machine-buildbox",
      "buildbox",
      false,
      [{ id: "mock-host-buildbox", name: "buildbox", kind: "host", distro: null }],
      [],
    ),
  ];
  // A fresh install knows one machine; the others come with use.
  return scenario === "empty"
    ? all.slice(0, 1).map((machine) => ({ ...machine, hosts: machine.hosts.slice(0, 1) }))
    : all;
}

function mockDevice(id: string, name: string, kind: ConnectDevice["kind"]): ConnectDevice {
  return {
    id,
    kind,
    name,
    machine_id: kind === "daemon" ? MOCK_MACHINE_ID : null,
    proof_public_key: "mock",
    noise_static_public_key: kind === "daemon" ? "mock-noise" : null,
    platform: "linux",
    created_at: new Date().toISOString(),
    last_seen: new Date().toISOString(),
    revoked_at: null,
    scopes: kind === "mobile" ? ["view", "approve"] : [],
  };
}

/**
 * Loads saved credentials and starts the connect session.
 *
 * Safe to call twice (React StrictMode runs effects twice); only the first call
 * performs work.
 */
export function initializeRemote(): void {
  if (booted) return;
  booted = true;

  const config = readBootConfig();
  if (MOCK_ALLOWED && config.mock) {
    mockFixtures = true;
    const user: ConnectUser = {
      id: "mock-user",
      email: "dev@example.com",
      display_name: "Dev",
      avatar_url: null,
    };
    remoteStore.setState({
      ...remoteStore.getState(),
      phase: "signed_in",
      connectUrl: MOCK_CONNECT_URL,
      user,
      device: mockDevice("mock-web", "h4rn web", "web"),
      devices: [
        mockDevice("mock-daemon", "atlas", "daemon"),
        mockDevice("mock-web", "h4rn web", "web"),
        mockDevice("mock-phone", "iPhone 16 Pro", "mobile"),
      ],
      machines: mockMachines(config.scenario),
      pairings: [],
      activeMachineId: readActiveRemote()?.machineId ?? MOCK_MACHINE_ID,
      lastSyncAt: Date.now(),
    });
    // Mock mode mirrors a phone that just opened a pairing link, so the
    // confirmation screen (scopes, expiry, claim) is reachable in a browser.
    pendingPairingRaw = `h4rn://pair?c=${MOCK_CONNECT_URL}&m=mock-machine-orion&k=bW9jay1ub2lzZQ&p=view,approve&e=${Math.floor(
      (Date.now() + 10 * 60_000) / 1_000,
    )}`;
    return;
  }

  // The OIDC provider redirects to /auth/callback with a code; finish it before
  // any other boot work so a reload lands back on the thread route.
  void finishRedirectCallback();

  void (async () => {
    try {
      const credentials = await readCredentials();
      if (!credentials) {
        patch({ phase: "signed_out", connectUrl: initialConnectUrl() });
        return;
      }
      setConnectClient(new ConnectClient(credentials));
      patch({
        phase: "signed_in",
        connectUrl: credentials.connectUrl,
        user: credentials.user,
        device: credentials.device,
      });
      await refresh();
    } catch (error) {
      patch({ phase: "error", error: describe(error) });
    } finally {
      await consumePendingDeepLinks();
    }
  })();

  if (typeof window !== "undefined") {
    window.addEventListener("h4rn-connect-pair", (event) => {
      const detail = (event as CustomEvent<unknown>).detail;
      if (typeof detail === "string") void pairMachine(detail);
    });
    window.addEventListener("h4rn-oidc-callback", (event) => {
      const detail = (event as CustomEvent<unknown>).detail;
      if (detail && typeof detail === "object") {
        void finishCallback(new URLSearchParams(detail as Record<string, string>));
      }
    });
  }
}

async function finishRedirectCallback(): Promise<void> {
  if (typeof window === "undefined") return;
  if (window.location.pathname !== "/auth/callback") return;
  const params = new URLSearchParams(window.location.search);
  const returnTo = await finishCallback(params);
  try {
    if (returnTo && returnTo !== "/auth/callback") {
      // Reloading is deliberate: the app booted on the callback route and its
      // router already read that path. A fresh load returns to the route the
      // person left, with the new credentials already in the secret store.
      window.location.replace(returnTo);
      return;
    }
    window.history.replaceState({}, "", "/");
  } catch {
    // A webview that refuses history edits still has a working app.
  }
}

type PendingLogin = {
  provider: string;
  state: string;
  redirectUri: string;
  connectUrl: string;
  /** Where to return after the provider redirects back (path plus query). */
  returnTo: string;
};

function readPendingLogin(): PendingLogin | null {
  const raw = readSessionState(PENDING_LOGIN_KEY);
  if (!raw) return null;
  try {
    return JSON.parse(raw) as PendingLogin;
  } catch {
    return null;
  }
}

function writePendingLogin(pending: PendingLogin): void {
  // OIDC state and the return path are short-lived and non-secret; the code
  // itself never reaches storage, and tokens go to the platform secret store.
  writeSessionState(PENDING_LOGIN_KEY, JSON.stringify(pending));
}

function clearPendingLogin(): void {
  removeSessionState(PENDING_LOGIN_KEY);
}

/** Replays a cold-start deep link that arrived before React mounted. */
async function consumePendingDeepLinks(): Promise<void> {
  const pair = takeConnectPair();
  const oidc = takeOidcCallback();
  if (oidc) await finishCallback(new URLSearchParams(oidc));
  if (pair) await pairMachine(pair);
}

/**
 * Completes a sign-in callback.
 *
 * Returns the path to return to on success (used by the full-page redirect
 * flow); deep-link and loopback callers ignore it because their view never
 * navigated away.
 */
async function finishCallback(params: URLSearchParams): Promise<string | null> {
  const error = params.get("error");
  if (error) {
    patch({ phase: "error", error: params.get("error_description") ?? error });
    clearPendingLogin();
    return null;
  }
  const code = params.get("code");
  const state = params.get("state");
  const pending = readPendingLogin();
  if (!code || !state || !pending || pending.state !== state) {
    patch({ phase: "error", error: "The sign-in callback is missing its state." });
    return null;
  }
  patch({ phase: "signing_in", busy: "Completing sign-in" });
  try {
    const result = await completeLogin(
      pending.connectUrl,
      pending.provider,
      state,
      code,
      pending.redirectUri,
    );
    await establish(pending.connectUrl, result.tokens, result.user, result.device);
    clearPendingLogin();
    return pending.returnTo;
  } catch (failure) {
    patch({ phase: "error", error: describe(failure) });
    return null;
  } finally {
    patch({ busy: null });
  }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

async function establish(
  connectUrl: string,
  tokens: {
    access_token: string;
    refresh_token: string;
    expires_in: number | bigint;
    token_type: string;
  },
  user: ConnectUser,
  device: ConnectDevice,
): Promise<void> {
  const credentials: RemoteCredentials = {
    connectUrl,
    user,
    device,
    tokens: normalizeTokens(tokens),
    expires_at: Date.now() + Math.max(0, Number(tokens.expires_in) - 30) * 1_000,
  };
  await writeCredentials(credentials);
  setConnectClient(new ConnectClient(credentials));
  remoteStore.setState({
    ...remoteStore.getState(),
    phase: "signed_in",
    connectUrl,
    user,
    device,
    login: null,
    error: null,
  });
  await refresh();
}

/** Reloads the account, machine list and pairings. */
export async function refresh(): Promise<void> {
  if (mockFixtures) return;
  const client = await loadConnectClient();
  if (!client) {
    patch({ phase: "signed_out" });
    return;
  }
  try {
    const [me, machines, pairings] = await Promise.all([
      client.me(),
      client.machines(),
      client.listPairings().catch(() => [] as PairingInfo[]),
    ]);
    const device =
      me.devices.find((candidate) => candidate.id === client.device.id) ?? client.device;
    patch({
      phase: "signed_in",
      user: me.user,
      device,
      devices: me.devices,
      machines,
      pairings,
      activeMachineId: remoteStore.getState().activeMachineId,
      lastSyncAt: Date.now(),
      error: null,
    });
  } catch (error) {
    patch({ error: describe(error) });
  }
}

/** Saves the account credentials and keeps the account menu honest. */
async function persistAccount(user: ConnectUser, device: ConnectDevice): Promise<void> {
  const client = await loadConnectClient();
  if (!client) return;
  const credentials = await client.credentialsSnapshot();
  await writeCredentials({ ...credentials, user, device });
}

// ---------------------------------------------------------------------------
// Sign-in
// ---------------------------------------------------------------------------

function redirectUriForShell(): string {
  const shell = shellKind();
  if (shell === "mobile") return "h4rn://callback";
  if (shell === "desktop") return `http://127.0.0.1:${LOOPBACK_PORT}/auth/callback`;
  return `${window.location.origin}/auth/callback`;
}

/**
 * Starts an OIDC sign-in.
 *
 * Browsers and mobile redirect; the desktop shell opens the OS browser and
 * waits for a loopback callback so the provider never sees a custom scheme.
 */
export async function signIn(provider: string): Promise<void> {
  const state = remoteStore.getState();
  const shell = shellKind();
  patch({ phase: "signing_in", busy: "Opening sign-in", error: null });
  try {
    const device = await deviceRegistration(
      shell === "web" ? "web" : shell === "mobile" ? "mobile" : "desktop",
      defaultDeviceName(),
    );
    const redirectUri = redirectUriForShell();
    const start = await startLogin(state.connectUrl, provider, redirectUri, device);
    writePendingLogin({
      provider,
      state: start.state,
      redirectUri,
      connectUrl: state.connectUrl,
      returnTo:
        typeof window === "undefined"
          ? "/"
          : `${window.location.pathname}${window.location.search}`,
    });

    if (shell === "desktop") {
      // The Rust command binds the loopback listener, opens the browser and
      // resolves with the callback's query string.
      const query = await tauriInvoke<string>("auth_loopback", {
        url: start.authorize_url,
        path: "/auth/callback",
        port: LOOPBACK_PORT,
      });
      await finishCallback(new URLSearchParams(query));
      return;
    }

    if (shell === "mobile") {
      // Open the provider in the system browser so this webview keeps the app
      // bundle. The provider redirects to `h4rn://callback`, which the OS
      // hands back to the deep-link adapter.
      try {
        await tauriInvoke("plugin:opener|open_url", { url: start.authorize_url });
        return;
      } catch {
        // Fall back to an in-webview navigation; the deep link still lands.
      }
    }

    // Browsers navigate and come back through `/auth/callback`.
    window.location.assign(start.authorize_url);
  } catch (error) {
    patch({ phase: "error", busy: null, error: describe(error) });
  }
}

/**
 * Starts a device-authorization grant and polls until approved.
 *
 * This is the headless flow: it also gives the e2e suite a way to sign in
 * without a browser round trip.
 */
export async function signInWithDeviceCode(provider: string): Promise<DeviceCodeLogin | null> {
  const state = remoteStore.getState();
  const shell = shellKind();
  patch({ phase: "signing_in", busy: "Requesting a code", error: null });
  try {
    const device = await deviceRegistration(
      shell === "web" ? "web" : shell === "mobile" ? "mobile" : "desktop",
      defaultDeviceName(),
    );
    const start = await startDeviceGrant(state.connectUrl, provider, device);
    const login: DeviceCodeLogin = {
      grantId: start.grant_id,
      userCode: start.user_code,
      verificationUri: start.verification_uri,
      verificationUriComplete: start.verification_uri_complete,
      expiresAt: Date.now() + Number(start.expires_in) * 1_000,
    };
    patch({ login, busy: "Waiting for approval" });
    void pollLogin(login, Number(start.interval));
    return login;
  } catch (error) {
    patch({ phase: "error", busy: null, error: describe(error) });
    return null;
  }
}

async function pollLogin(login: DeviceCodeLogin, intervalSecs: number): Promise<void> {
  let interval = Math.max(1, intervalSecs);
  const connectUrl = remoteStore.getState().connectUrl;
  while (Date.now() < login.expiresAt) {
    await delay(interval * 1_000);
    let result: Awaited<ReturnType<typeof pollDeviceGrant>>;
    try {
      result = await pollDeviceGrant(connectUrl, login.grantId);
    } catch (error) {
      patch({ phase: "error", busy: null, error: describe(error) });
      return;
    }
    switch (result.status) {
      case "approved":
        if (result.tokens && result.user && result.device) {
          await establish(connectUrl, result.tokens, result.user, result.device);
        }
        return;
      case "denied":
        patch({ phase: "signed_out", login: null, busy: null, error: "Sign-in was denied." });
        return;
      case "expired":
        patch({ phase: "signed_out", login: null, busy: null, error: "That code expired." });
        return;
      case "slow_down":
        interval = Math.max(1, Number(result.retry_after ?? interval + 1));
        break;
      default:
        break;
    }
  }
  patch({ phase: "signed_out", login: null, busy: null, error: "That code expired." });
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Targets a different connect deployment before sign-in. */
export function setConnectUrl(url: string): void {
  const trimmed = url.replace(/\/+$/, "");
  patch({
    connectUrl: trimmed || DEFAULT_CONNECT_URL,
    phase: remoteStore.getState().user ? remoteStore.getState().phase : "signed_out",
    error: null,
  });
}

/** Abandons a pending device-authorization grant. */
export function cancelLogin(): void {
  patch({ login: null, busy: null, phase: "signed_out" });
}

/** Signs out, forgets the active machine and returns to a local session. */
export async function signOut(): Promise<void> {
  const client = await loadConnectClient();
  try {
    await client?.logout();
  } catch {
    // Local cleanup is what the person asked for.
  }
  resetConnectSession();
  clearActiveRemote();
  remoteStore.setState({
    ...createInitialState(),
    phase: "signed_out",
    connectUrl: remoteStore.getState().connectUrl,
  });
  restartConnection();
}

// ---------------------------------------------------------------------------
// Machines
// ---------------------------------------------------------------------------

function daemonKeyFor(machine: MachineSummary, devices: ConnectDevice[]): string {
  const daemon = devices.find(
    (device) => device.kind === "daemon" && device.machine_id === machine.id,
  );
  const key = daemon?.noise_static_public_key;
  if (!key) {
    throw new Error(`${machine.name} has not published a Noise key yet. Re-run h4rnd login.`);
  }
  return base64UrlToHex(key);
}

/** Builds and persists the descriptor for one machine. */
function activeFor(
  connectUrl: string,
  machine: MachineSummary,
  devices: ConnectDevice[],
): ActiveRemote {
  return {
    connectUrl,
    machineId: machine.id,
    machineName: machine.name,
    daemonNoiseKey: daemonKeyFor(machine, devices),
    addresses: machine.addresses,
    online: machine.online,
    relay: true,
  };
}

/** Connects to a machine discovered through `/v1/machines`. */
export async function activateMachine(machineId: string): Promise<boolean> {
  if (mockFixtures) {
    patch({ activeMachineId: machineId });
    return true;
  }
  const client = await loadConnectClient();
  const machine = remoteStore.getState().machines.find((candidate) => candidate.id === machineId);
  if (!client || !machine) {
    pushToast({ variant: "error", title: "That machine is no longer available." });
    return false;
  }
  patch({ busy: `Connecting to ${machine.name}` });
  try {
    const me = await client.me();
    const active = activeFor(client.connectUrl, machine, me.devices);
    writeActiveRemote(active);
    patch({ active, activeMachineId: machineId, busy: null, error: null });
    restartConnection();
    return true;
  } catch (error) {
    patch({ busy: null, error: describe(error) });
    pushToast({
      variant: "error",
      title: "Could not select that machine.",
      description: describe(error),
    });
    return false;
  }
}

/** Stops using the remote machine and returns to whatever local session exists. */
export function forgetMachine(): void {
  clearActiveRemote();
  patch({ active: null, activeMachineId: null });
  restartConnection();
}

// ---------------------------------------------------------------------------
// Pairing
// ---------------------------------------------------------------------------

export type ConnectPairingPayload = {
  connectUrl: string;
  machineId: string;
  daemonNoiseKey: string;
  expiresAt: number;
  /** What the host says it will grant. Display only; connect enforces. */
  scopes: PairScope[];
  raw: string;
};

let pendingPairingRaw: string | null = null;

/** The pairing link this device scanned, for the pairing screen to confirm. */
export function pendingPairing(): { raw: string; payload: ConnectPairingPayload } | null {
  if (!pendingPairingRaw) return null;
  const payload = parseConnectPairing(pendingPairingRaw);
  return payload ? { raw: pendingPairingRaw, payload } : null;
}

/** Parses `h4rn://pair?c=...&m=...&k=...&s=...&e=...`. */
export function parseConnectPairing(raw: string): ConnectPairingPayload | null {
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    return null;
  }
  if (url.protocol !== "h4rn:" || url.hostname !== "pair") return null;
  const connectUrl = url.searchParams.get("c");
  const machineId = url.searchParams.get("m");
  const key = url.searchParams.get("k");
  const expires = Number(url.searchParams.get("e") ?? "0");
  if (!connectUrl || !machineId || !key) return null;
  return {
    connectUrl,
    machineId,
    daemonNoiseKey: base64UrlToHex(key),
    expiresAt: expires * 1_000,
    scopes: orderedScopes((url.searchParams.get("p") ?? "").split(",").filter(isPairScope)),
    raw,
  };
}

/**
 * Claims a pairing QR and connects to its machine.
 *
 * If the QR points at a different connect deployment the account is not signed
 * in against, the payload is kept and the connect URL is switched so the sign-in
 * screen targets the right relay.
 */
export async function pairMachine(raw: string): Promise<boolean> {
  pendingPairingRaw = raw;
  const payload = parseConnectPairing(raw);
  if (!payload) {
    patch({ error: "That QR code is not an h4rn pairing link." });
    return false;
  }
  if (payload.expiresAt > 0 && payload.expiresAt < Date.now()) {
    patch({ error: "That pairing code has expired. Generate a new one on the host." });
    return false;
  }
  if (mockFixtures) {
    patch({ activeMachineId: payload.machineId });
    return true;
  }

  const state = remoteStore.getState();
  if (!state.user) {
    patch({
      connectUrl: payload.connectUrl,
      phase: "signed_out",
      error: "Sign in to h4rn-connect first, then scan the pairing code again.",
    });
    return false;
  }
  if (payload.connectUrl.replace(/\/+$/, "") !== state.connectUrl.replace(/\/+$/, "")) {
    patch({
      error: `This code belongs to ${payload.connectUrl}. Sign out and sign in there first.`,
    });
    return false;
  }

  const client = await loadConnectClient();
  if (!client) return false;
  patch({ busy: "Claiming pairing", error: null });
  try {
    const claimed = await client.claimPairing(raw, defaultDeviceName());
    const machine = claimed.machine;
    // The daemon's key in the QR is authoritative for the first handshake.
    const active: ActiveRemote = {
      connectUrl: client.connectUrl,
      machineId: machine.id,
      machineName: machine.name,
      daemonNoiseKey: payload.daemonNoiseKey,
      addresses: machine.addresses,
      online: machine.online,
      relay: true,
    };
    writeActiveRemote(active);
    patch({ active, activeMachineId: machine.id, busy: null });
    await refresh();
    restartConnection();
    pushToast({ variant: "success", title: `Paired with ${machine.name}.` });
    return true;
  } catch (error) {
    patch({ busy: null, error: describe(error) });
    pushToast({
      variant: "error",
      title: "Could not claim that pairing.",
      description: describe(error),
    });
    return false;
  }
}

/**
 * Creates a QR and short code for pairing a new device with a machine.
 *
 * `scopes` is what the paired device will hold. Replacing a live pairing
 * revokes the old one first, so changing a checkbox never leaves a second
 * working code behind.
 */
export async function createPairing(
  machineId: string,
  scopes: readonly PairScope[] = DEFAULT_PAIR_SCOPES,
  ttlMs = 5 * 60_000,
): Promise<void> {
  if (mockFixtures) {
    patch({ pairing: mockOffer(machineId, MOCK_CONNECT_URL, scopes, ttlMs) });
    return;
  }
  const client = await loadConnectClient();
  if (!client) return;
  const previous = getRemoteState().pairing;
  patch({ busy: "Generating a code" });
  try {
    const created = await client.createPairing(machineId, orderedScopes(scopes));
    patch({ pairing: offerFromCreate(created, ttlMs), busy: null });
    if (previous) void client.revokePairing(previous.id).catch(() => undefined);
  } catch (error) {
    patch({ busy: null, error: describe(error) });
    pushToast({
      variant: "error",
      title: "Could not create a pairing.",
      description: describe(error),
    });
  }
}

/**
 * Claims a pairing by the 8-character code typed from the host.
 *
 * A typed code carries no daemon key, so connect supplies it and a malicious
 * relay could substitute its own. The claim therefore stops at a short
 * authentication string: nothing is pinned or connected until the person
 * confirms it matches the host's screen (`confirmPairingSas`). Resolves true
 * only when the pairing is complete, which for a typed code is never the case
 * here; the page watches `sasConfirm`.
 */
export async function pairByCode(code: string): Promise<boolean> {
  if (mockFixtures) {
    pushToast({ variant: "success", title: "Paired with atlas." });
    return true;
  }
  const client = await loadConnectClient();
  if (!client) {
    patch({ error: "Sign in to h4rn-connect first, then enter the code again." });
    return false;
  }
  patch({ busy: "Claiming pairing", error: null });
  try {
    const claimed = await client.claimPairingCode(code, defaultDeviceName());
    const machine = claimed.machine;
    const daemonNoiseKey = base64UrlToHex(claimed.daemon_noise_public_key);
    const active: ActiveRemote = {
      connectUrl: client.connectUrl,
      machineId: machine.id,
      machineName: machine.name,
      daemonNoiseKey,
      addresses: machine.addresses,
      online: machine.online,
      relay: true,
    };
    const sas = await client.pairingSas(daemonNoiseKey);
    patch({ sasConfirm: { sas, machineName: machine.name, active }, busy: null });
    return false;
  } catch (error) {
    patch({ busy: null, error: describe(error) });
    return false;
  }
}

/** The person confirmed the code matches: pin the daemon key and connect. */
export async function confirmPairingSas(): Promise<boolean> {
  const pending = remoteStore.getState().sasConfirm;
  if (!pending) return false;
  writeActiveRemote(pending.active);
  patch({ active: pending.active, activeMachineId: pending.active.machineId, sasConfirm: null });
  await refresh();
  restartConnection();
  pushToast({ variant: "success", title: `Paired with ${pending.machineName}.` });
  return true;
}

/**
 * The codes differ, so something between this device and the host changed a
 * key. Nothing was pinned. Revoke this device's registration so the pairing
 * cannot be used, and sign out.
 */
export async function rejectPairingSas(): Promise<void> {
  const client = await loadConnectClient();
  patch({ sasConfirm: null });
  try {
    if (client) await client.revokeDevice(client.device.id);
  } catch {
    // Signing out below still removes the local credentials.
  }
  await signOut();
  patch({
    error:
      "The codes did not match, so this device was revoked. Pair again from a network you trust.",
  });
}

export function dismissPairing(): void {
  patch({ pairing: null });
}

export async function revokePairing(id: string): Promise<void> {
  if (mockFixtures) {
    patch({ pairing: null });
    return;
  }
  const client = await loadConnectClient();
  if (!client) return;
  try {
    await client.revokePairing(id);
    patch({ pairing: null, pairings: getRemoteState().pairings.filter((row) => row.id !== id) });
  } catch (error) {
    patch({ error: describe(error) });
  }
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

/** Renames a device optimistically and rolls back on failure. */
export async function renameDevice(id: string, name: string): Promise<boolean> {
  const trimmed = name.trim();
  if (!trimmed) return false;
  const before = getRemoteState().devices;
  patch({
    devices: before.map((device) => (device.id === id ? { ...device, name: trimmed } : device)),
  });
  if (mockFixtures) return true;
  const client = await loadConnectClient();
  if (!client) return false;
  try {
    const updated = await client.renameDevice(id, trimmed);
    patch({
      devices: getRemoteState().devices.map((device) => (device.id === id ? updated : device)),
      device: getRemoteState().device?.id === id ? updated : getRemoteState().device,
    });
    await persistAccount(
      getRemoteState().user ?? client.user,
      getRemoteState().device ?? client.device,
    );
    return true;
  } catch (error) {
    patch({ devices: before, error: describe(error) });
    pushToast({
      variant: "error",
      title: "Could not rename the device.",
      description: describe(error),
    });
    return false;
  }
}

/** Revokes a device optimistically; rollback restores it to the table. */
export async function revokeDevice(id: string): Promise<boolean> {
  const before = getRemoteState().devices;
  patch({ devices: before.filter((device) => device.id !== id) });
  if (mockFixtures) return true;
  const client = await loadConnectClient();
  if (!client) return false;
  try {
    await client.revokeDevice(id);
    const revokedSelf = getRemoteState().device?.id === id;
    pushToast({ variant: "success", title: "Device revoked. Its next request fails." });
    if (revokedSelf) {
      // These credentials are the device that was just revoked. Keeping them
      // would only produce endless 401s, so drop them and return to a local
      // session; the relay already ended the live session.
      await signOut();
    }
    return true;
  } catch (error) {
    patch({ devices: before, error: describe(error) });
    pushToast({
      variant: "error",
      title: "Could not revoke the device.",
      description: describe(error),
    });
    return false;
  }
}

export { pathsFor };
