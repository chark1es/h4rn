/**
 * h4rn-connect credentials in the platform secret store.
 *
 * This is a thin, feature-specific view over `lib/platform/secrets.ts`: the
 * one store every credential goes through (ADR-038). Session and refresh tokens
 * must never sit in `localStorage` or `sessionStorage`; on the desktop they go
 * to the OS keychain, on mobile to the Keystore/Keychain, and on the web to
 * AES-GCM envelopes in IndexedDB.
 */

import {
  deleteSecret as deletePlatformSecret,
  getSecret as getPlatformSecret,
  setSecret as setPlatformSecret,
} from "@/lib/platform/secrets";

const PREFIX = "h4rn.remote.";

/** Reads one connect secret, or `null` when it was never stored. */
export function getSecret(key: string): Promise<string | null> {
  return getPlatformSecret(PREFIX + key);
}

/** Stores one connect secret. */
export function setSecret(key: string, value: string): Promise<void> {
  return setPlatformSecret(PREFIX + key, value);
}

/** Removes one connect secret; a missing entry is not an error. */
export function deleteSecret(key: string): Promise<void> {
  return deletePlatformSecret(PREFIX + key);
}
