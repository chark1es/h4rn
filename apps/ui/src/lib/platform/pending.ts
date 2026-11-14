/**
 * Deep links that arrive before React mounts its listeners.
 *
 * A cold-start `h4rn://` intent is consumed by the platform adapter at module
 * import, which is earlier than `initializeRemote()` runs. Keeping the payload
 * in memory (never `sessionStorage`) means an OIDC authorization code or a
 * connect pairing URI cannot be read back by another tab or an origin script.
 */

let pendingOidc: Record<string, string> | null = null;
let pendingConnectPair: string | null = null;

export function rememberOidcCallback(detail: Record<string, string>): void {
  pendingOidc = detail;
}

export function takeOidcCallback(): Record<string, string> | null {
  const detail = pendingOidc;
  pendingOidc = null;
  return detail;
}

export function rememberConnectPair(raw: string): void {
  pendingConnectPair = raw;
}

export function takeConnectPair(): string | null {
  const raw = pendingConnectPair;
  pendingConnectPair = null;
  return raw;
}
