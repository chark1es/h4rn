import { H4rnClient } from "./client";
import { createMockTransport } from "./mock";
import { createSecureTransportFactory, hasStoredSecurity } from "./secure";
import type { ClientOptions, TransportFactory } from "./types";

export const DEFAULT_WS_URL = "ws://127.0.0.1:47800/ws";
const TOKEN_KEY = "h4rn.token";

export type BootConfig = {
  mock: boolean;
  scenario: string;
  token: string | null;
  daemonUrl: string | null;
  theme: "light" | "dark" | "system" | null;
  /** Use the Noise/multipath transport instead of plaintext `/ws`. */
  secure: boolean;
  /** Raw `h4rn://pair?...` URI from `?pair=`. */
  pairing: string | null;
  /** Optional relay standby endpoint from `?relay=`. */
  relay: string | null;
};

export function readBootConfig(): BootConfig {
  if (typeof window === "undefined") {
    return {
      mock: false,
      scenario: "default",
      token: null,
      daemonUrl: null,
      theme: null,
      secure: false,
      pairing: null,
      relay: null,
    };
  }

  const params = new URLSearchParams(window.location.search);
  const queryTheme = params.get("theme");
  const pairing = params.get("pair");
  return {
    mock: params.get("mock") === "1",
    scenario: params.get("scenario") ?? "default",
    token: params.get("token") ?? readStoredToken(),
    daemonUrl: readDaemonUrl(params.get("daemon")),
    theme:
      queryTheme === "light" || queryTheme === "dark" || queryTheme === "system"
        ? queryTheme
        : null,
    // A pairing link or a previously pinned daemon selects Noise. Plain
    // `?daemon=` stays on the loopback WebSocket for backwards compatibility.
    secure: Boolean(pairing) || params.get("secure") === "1" || hasStoredSecurity(),
    pairing,
    relay: params.get("relay"),
  };
}

/**
 * Resolves a `?daemon=` override.
 *
 * Only absolute `ws://` and `wss://` URLs are accepted so a malformed query
 * cannot silently point the app at a different scheme.
 */
function readDaemonUrl(value: string | null): string | null {
  if (!value) return null;
  try {
    const url = new URL(value);
    if (url.protocol === "ws:" || url.protocol === "wss:") return url.toString();
  } catch {
    // Fall through to the default daemon URL.
  }
  return null;
}

function readStoredToken(): string | null {
  try {
    return window.localStorage.getItem(TOKEN_KEY);
  } catch {
    return null;
  }
}

export function createClientFromEnv(): H4rnClient {
  const config = readBootConfig();

  if (config.mock) {
    const transportFactory: TransportFactory = () => createMockTransport(config.scenario);
    const options: ClientOptions = {
      url: config.daemonUrl ?? DEFAULT_WS_URL,
      token: "mock",
      transportLabel: "mock",
      transportFactory,
      requestTimeoutMs: 10_000,
      reconnect: { baseMs: 300, maxMs: 4_000, jitter: 0.2, maxAttempts: 8 },
    };
    return new H4rnClient(options);
  }

  if (config.secure) {
    return new H4rnClient({
      url: config.daemonUrl ?? DEFAULT_WS_URL,
      token: null,
      transportLabel: "encrypted",
      transportFactory: createSecureTransportFactory({
        pairing: config.pairing,
        relay: config.relay,
      }),
      requestTimeoutMs: 10_000,
      reconnect: { baseMs: 400, maxMs: 8_000, jitter: 0.2, maxAttempts: 10 },
    });
  }

  return new H4rnClient({
    url: config.daemonUrl ?? DEFAULT_WS_URL,
    token: config.token,
    transportLabel: "direct",
  });
}
