/**
 * The machine this device is currently connected to.
 *
 * This record is deliberately small and contains no token: it is the
 * non-secret descriptor boot needs synchronously to choose a transport before
 * the async secret store has answered. Tokens stay in `secretStore`.
 */

import { kindForEndpoint, normalizeSecureUrl, type PathConfig } from "@/lib/client/secure";

const ACTIVE_KEY = "h4rn.remote.active";

export type ActiveRemote = {
  connectUrl: string;
  machineId: string;
  machineName: string;
  /** Daemon X25519 static public key, hex. */
  daemonNoiseKey: string;
  /** Advertised candidates, most preferred first. */
  addresses: string[];
  /** Whether the machine is online according to connect. */
  online: boolean;
  /** Whether a relay path should be attempted. */
  relay: boolean;
};

function storage(): Storage | null {
  try {
    return typeof window === "undefined" ? null : window.localStorage;
  } catch {
    return null;
  }
}

export function readActiveRemote(): ActiveRemote | null {
  const raw = storage()?.getItem(ACTIVE_KEY);
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw) as Partial<ActiveRemote>;
    if (!parsed.machineId || !parsed.daemonNoiseKey || !parsed.connectUrl) return null;
    return {
      connectUrl: parsed.connectUrl,
      machineId: parsed.machineId,
      machineName: parsed.machineName ?? parsed.machineId,
      daemonNoiseKey: parsed.daemonNoiseKey,
      addresses: Array.isArray(parsed.addresses) ? parsed.addresses : [],
      online: parsed.online ?? true,
      relay: parsed.relay ?? true,
    };
  } catch {
    return null;
  }
}

export function writeActiveRemote(active: ActiveRemote): void {
  try {
    storage()?.setItem(ACTIVE_KEY, JSON.stringify(active));
  } catch {
    // A locked-down webview still connects for this page load.
  }
}

export function clearActiveRemote(): void {
  try {
    storage()?.removeItem(ACTIVE_KEY);
  } catch {
    // Ignore.
  }
}

/** Candidate transport paths for a machine, relay last as the fallback. */
export function pathsFor(active: ActiveRemote): PathConfig[] {
  const paths: PathConfig[] = [];
  const seen = new Set<string>();
  const relay = active.relay || active.addresses.some((address) => address.startsWith("relay:"));
  for (const address of active.addresses) {
    // `relay:<connect base>` is a marker, not a dialable endpoint: the
    // authenticated URL is minted per attempt.
    if (!address.trim() || address.startsWith("relay:")) continue;
    let endpoint: string;
    try {
      endpoint = normalizeSecureUrl(address);
    } catch {
      continue;
    }
    if (seen.has(endpoint)) continue;
    seen.add(endpoint);
    paths.push({ kind: kindForEndpoint(endpoint), endpoint });
  }
  if (relay) {
    // The endpoint is a label; `SecureTransport` replaces it with a freshly
    // minted authenticated URL before every attempt.
    paths.push({ kind: "relay", endpoint: "relay" });
  }
  return paths;
}
