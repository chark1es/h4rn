import { SecureTransport } from "@/lib/client/secure";
import type { TransportFactory } from "@/lib/client/types";

import { pathsFor, readActiveRemote } from "./active";
import { mintRelayUrlFor } from "./session";

/**
 * Builds the secure transport for the machine selected in Settings → Hosts.
 *
 * The direct/tailnet addresses are static and come from `/v1/machines`; the
 * relay URL is minted fresh for every reconnect because its access token and
 * DPoP proof are short-lived.
 */
export function createRemoteTransportFactory(): TransportFactory {
  return (context) => {
    const active = readActiveRemote();
    return new SecureTransport(context, {
      remote: active
        ? {
            daemonPublic: active.daemonNoiseKey,
            machineId: active.machineId,
            paths: pathsFor(active),
            sessionKey: `h4rn.secure.session.${active.machineId}`,
          }
        : null,
      mintRelayUrl: async () => {
        const current = readActiveRemote();
        if (!current?.relay) return null;
        try {
          return await mintRelayUrlFor(current.machineId);
        } catch {
          // A direct path may still be reachable while the relay is down.
          return null;
        }
      },
    });
  };
}
