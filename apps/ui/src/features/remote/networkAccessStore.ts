/**
 * The daemon's direct network listener and one-time direct pairings.
 *
 * The daemon owns the socket and the pairing secret; this module only mirrors
 * its answers for Settings. Every request is admin-scoped, so it is also the
 * place that reports a bind failure without pretending the listener is up.
 */

import { useSyncExternalStore } from "react";

import type { DirectDevice, PairScope } from "@/gen";
import { toProtoError } from "@/lib/client/errors";
import { getClient } from "@/lib/store/appStore";

/** What another device on this network would dial. */
export type NetworkAccessView = {
  enabled: boolean;
  listening: boolean;
  port: number;
  addresses: string[];
  error: string | null;
};

/** A `DirectPairingOffer` with the expiry as wall-clock milliseconds. */
export type DirectPairingView = {
  uri: string;
  expiresAt: number;
  addresses: string[];
  scopes: PairScope[];
};

export type NetworkAccessState = {
  access: NetworkAccessView | null;
  loading: boolean;
  saving: boolean;
  error: string | null;
  offer: DirectPairingView | null;
  creating: boolean;
  offerError: string | null;
  /** Devices paired directly with this computer; `null` until loaded. */
  devices: DirectDevice[] | null;
  /** Key of the device whose revoke is in flight. */
  revoking: string | null;
  devicesError: string | null;
};

const initialState: NetworkAccessState = {
  access: null,
  loading: false,
  saving: false,
  error: null,
  offer: null,
  creating: false,
  offerError: null,
  devices: null,
  revoking: null,
  devicesError: null,
};

let state: NetworkAccessState = initialState;
const listeners = new Set<() => void>();

function update(patch: Partial<NetworkAccessState>): void {
  state = { ...state, ...patch };
  for (const listener of listeners) listener();
}

function describe(error: unknown): string {
  return toProtoError(error).message;
}

export function getNetworkAccessState(): NetworkAccessState {
  return state;
}

export function subscribeNetworkAccess(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Re-renders the caller when any part of the listener state changes. */
export function useNetworkAccess(): NetworkAccessState {
  return useSyncExternalStore(subscribeNetworkAccess, getNetworkAccessState, getNetworkAccessState);
}

/** Clears the store between tests. */
export function resetNetworkAccessStoreForTests(): void {
  state = initialState;
  listeners.clear();
  inFlight = null;
}

let inFlight: Promise<void> | null = null;

/** Reads the setting and its current addresses once per mount. */
export function loadNetworkAccess(): Promise<void> {
  if (inFlight) return inFlight;
  inFlight = (async () => {
    update({ loading: true, error: null });
    try {
      const response = await getClient().request({ type: "get_network_access" });
      if (response.type === "network_access") {
        update({
          loading: false,
          access: {
            enabled: response.enabled,
            listening: response.listening,
            port: response.port,
            addresses: response.addresses,
            error: response.error,
          },
        });
      } else {
        update({ loading: false });
      }
    } catch (error) {
      update({ loading: false, error: describe(error) });
    } finally {
      inFlight = null;
    }
  })();
  return inFlight;
}

/** Turns the dedicated network listener on or off. */
export async function setNetworkAccess(enabled: boolean): Promise<boolean> {
  update({ saving: true, error: null });
  try {
    const response = await getClient().request({ type: "set_network_access", enabled });
    if (response.type !== "network_access") {
      update({ saving: false });
      return false;
    }
    const patch: Partial<NetworkAccessState> = {
      saving: false,
      access: {
        enabled: response.enabled,
        listening: response.listening,
        port: response.port,
        addresses: response.addresses,
        error: response.error,
      },
    };
    // Turning the listener off also closes the daemon's pairing window.
    if (!response.enabled) patch.offer = null;
    update(patch);
    return true;
  } catch (error) {
    update({ saving: false, error: describe(error) });
    return false;
  }
}

/**
 * Opens a one-time QR for another device.
 *
 * The daemon grants `view` and `approve` when no scopes are asked for. Every
 * new offer replaces the previous one, because only one secret can be armed at
 * a time.
 */
export async function createDirectPairing(scopes: PairScope[]): Promise<DirectPairingView | null> {
  update({ creating: true, offerError: null });
  try {
    const response = await getClient().request({
      type: "create_direct_pairing",
      scopes,
      label: null,
    });
    if (response.type !== "direct_pairing_offer") {
      update({ creating: false });
      return null;
    }
    const offer: DirectPairingView = {
      uri: response.uri,
      expiresAt: response.expires_at * 1_000,
      addresses: response.addresses,
      scopes: response.scopes,
    };
    update({ creating: false, offer });
    return offer;
  } catch (error) {
    update({ creating: false, offerError: describe(error) });
    return null;
  }
}

/** Reads the devices paired directly with this computer. */
export async function loadDirectDevices(): Promise<void> {
  try {
    const response = await getClient().request({ type: "list_direct_devices" });
    if (response.type === "direct_devices") {
      update({ devices: response.devices, devicesError: null });
    }
  } catch (error) {
    update({ devicesError: describe(error) });
  }
}

/**
 * Removes a directly paired device. The daemon drops its key and ends its
 * live sessions, so it has to pair again to come back.
 */
export async function revokeDirectDevice(key: string): Promise<boolean> {
  update({ revoking: key, devicesError: null });
  try {
    const response = await getClient().request({ type: "revoke_direct_device", key });
    update({
      revoking: null,
      devices:
        response.type === "direct_devices"
          ? response.devices
          : (state.devices ?? []).filter((device) => device.key !== key),
    });
    return true;
  } catch (error) {
    update({ revoking: null, devicesError: describe(error) });
    return false;
  }
}

/** Closes the open pairing window and hides its QR. */
export async function cancelDirectPairing(): Promise<void> {
  update({ offer: null, offerError: null });
  try {
    await getClient().request({ type: "cancel_direct_pairing" });
  } catch (error) {
    update({ offerError: describe(error) });
  }
}
