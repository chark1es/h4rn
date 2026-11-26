/**
 * Recognition of `h4rn://` links, without touching the DOM.
 *
 * Three link shapes share the scheme:
 *
 * - `h4rn://connect?daemon=…&token=…` — a local daemon plus its access token;
 * - `h4rn://pair?c=…&m=…` — an h4rn-connect pairing through the relay;
 * - `h4rn://pair?e=…&d=…&p=…` — a direct LAN/tailnet pairing (`CreateDirectPairing`).
 *
 * The pure classifier lives here so the parser can be tested in Node and so
 * the platform adapter stays the only place that stores secrets or dispatches
 * events.
 */

import { i18n } from "@/lib/i18n";

export type PairingTarget =
  | { kind: "connect"; raw: string }
  | { kind: "direct"; raw: string }
  | { kind: "oidc"; detail: Record<string, string> }
  | { kind: "daemon"; daemon: string; token: string };

const TARGETS = ["connect", "pair", "oidc", "callback"];

function targetOf(url: URL): string {
  return (url.hostname || url.pathname.replace(/^\/+/, "")).toLowerCase();
}

/** `e` (endpoint), `d` (daemon key) and `p` (one-time PSK) make a direct QR. */
export function isDirectPairingParams(params: URLSearchParams): boolean {
  return Boolean(params.get("e") && params.get("d") && params.get("p"));
}

/** Whether a whole link is a direct QR, so a paste field can route it. */
export function isDirectPairingUrl(raw: string): boolean {
  try {
    const url = new URL(raw);
    return (
      url.protocol === "h4rn:" &&
      targetOf(url) === "pair" &&
      isDirectPairingParams(url.searchParams)
    );
  } catch {
    return false;
  }
}

/**
 * Classifies one link.
 *
 * Returns `null` for URLs that are not h4rn links, and throws when a link
 * claims to carry a daemon token but does not. The caller decides how to
 * consume each kind.
 */
export function parsePairingUrl(raw: string): PairingTarget | null {
  const url = new URL(raw);
  if (url.protocol !== "h4rn:") return null;
  const target = targetOf(url);
  if (!TARGETS.includes(target)) return null;

  if (target === "oidc" || target === "callback") {
    // The platform has no auth screen of its own; hand the callback to the UI
    // without touching the daemon connection.
    return { kind: "oidc", detail: Object.fromEntries(url.searchParams.entries()) };
  }

  // A connect pairing QR names the relay, machine and daemon Noise key; it
  // carries no daemon token, so handle it before the local pairing shapes.
  if (target === "pair" && url.searchParams.get("c") && url.searchParams.get("m")) {
    return { kind: "connect", raw };
  }

  if (target === "pair" && isDirectPairingParams(url.searchParams)) {
    return { kind: "direct", raw };
  }

  const daemon = url.searchParams.get("daemon");
  const token = url.searchParams.get("token");
  if (!daemon || !token) {
    throw new Error(i18n.t("pairing:errors.missingDaemonOrToken"));
  }
  return { kind: "daemon", daemon, token };
}
