/** The pairing a Settings → Devices screen shows: QR, typed code, scopes, expiry. */

import type { PairingCreateResponse, PairScope } from "@/gen";

import { orderedScopes } from "./pairingScopes";

export type PairingOffer = {
  id: string;
  qr: string;
  /** Eight Crockford base32 characters, no separator. */
  shortCode: string;
  scopes: PairScope[];
  expiresAt: number;
};

/** Reads connect's create response into what the screen needs. */
export function offerFromCreate(created: PairingCreateResponse, ttlMs: number): PairingOffer {
  const expires = Date.parse(created.pairing.expires_at);
  return {
    id: created.pairing.id,
    qr: created.qr,
    shortCode: created.short_code,
    scopes: orderedScopes(created.pairing.scopes),
    expiresAt: Number.isFinite(expires) ? expires : Date.now() + ttlMs,
  };
}

/** Mock mode: a pairing that behaves like the real one without a relay. */
export function mockOffer(
  machineId: string,
  connectUrl: string,
  scopes: readonly PairScope[],
  ttlMs: number,
  now = Date.now(),
): PairingOffer {
  const ordered = orderedScopes(scopes);
  const expiresAt = now + ttlMs;
  // A deterministic code per (machine, scopes) keeps screenshots stable.
  const seed = `${machineId}:${ordered.join(",")}`;
  const alphabet = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
  let hash = 2166136261;
  for (let index = 0; index < seed.length; index += 1) {
    hash = Math.imul(hash ^ seed.charCodeAt(index), 16777619) >>> 0;
  }
  let code = "";
  while (code.length < 8) {
    hash = Math.imul(hash ^ (hash >>> 15), 2246822519) >>> 0;
    code += alphabet[(hash >>> 7) % alphabet.length];
  }
  return {
    id: `mock-pairing-${seed}`,
    qr: `h4rn://pair?c=${connectUrl}&m=${machineId}&k=mock&e=${Math.floor(expiresAt / 1_000)}&p=${ordered.join(",")}`,
    shortCode: code,
    scopes: ordered,
    expiresAt,
  };
}
