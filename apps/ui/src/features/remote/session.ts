/**
 * The process-wide connect session.
 *
 * The transport factory is synchronous, but reading keychain credentials is
 * async. This module bridges the two: it lazily loads the saved credentials
 * and caches the resulting `ConnectClient` so token refreshes and the relay URL
 * mint see the same session.
 */

import { ConnectClient, readCredentials } from "./connect";

let cached: ConnectClient | null = null;
let loading: Promise<ConnectClient | null> | null = null;

/** Loads the signed-in client, if this device has credentials. */
export function loadConnectClient(): Promise<ConnectClient | null> {
  if (cached) return Promise.resolve(cached);
  loading ??= (async () => {
    const credentials = await readCredentials();
    if (!credentials) return null;
    cached = new ConnectClient(credentials);
    return cached;
  })();
  return loading;
}

/** Returns the already-loaded client without touching storage. */
export function currentConnectClient(): ConnectClient | null {
  return cached;
}

export function setConnectClient(client: ConnectClient | null): void {
  cached = client;
  loading = client ? Promise.resolve(client) : null;
}

/** Mints a relay URL for the active machine, or `null` when signed out. */
export async function mintRelayUrlFor(machineId: string): Promise<string | null> {
  const client = await loadConnectClient();
  if (!client) return null;
  return client.mintRelayUrl(machineId);
}

export function resetConnectSession(): void {
  cached = null;
  loading = null;
}
