/**
 * Short-lived, non-secret state for one browser tab.
 *
 * `sessionStorage` is reserved for OIDC `state`, pending redirect metadata and
 * similar values that are useless to an attacker after the callback. Tokens,
 * refresh tokens, PSKs, pairing secrets and device keys never go here; they use
 * `lib/platform/secrets.ts` instead (ADR-038).
 */

export function readSessionState(key: string): string | null {
  if (typeof window === "undefined") return null;
  try {
    return window.sessionStorage.getItem(key);
  } catch {
    return null;
  }
}

export function writeSessionState(key: string, value: string): void {
  if (typeof window === "undefined") return;
  try {
    window.sessionStorage.setItem(key, value);
  } catch {
    // Private mode can reject writes; the in-memory flow still works.
  }
}

export function removeSessionState(key: string): void {
  if (typeof window === "undefined") return;
  try {
    window.sessionStorage.removeItem(key);
  } catch {
    // Ignore.
  }
}
