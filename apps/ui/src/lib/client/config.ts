import { readActiveRemote } from "@/features/remote/active";
import { createRemoteTransportFactory } from "@/features/remote/remoteTransport";
import { peekSecret } from "@/lib/platform/secrets";

import { H4rnClient } from "./client";
import { createMockTransport } from "./mock";
import { createSecureTransportFactory, hasStoredSecurity } from "./secure";
import type { ClientOptions, TransportFactory } from "./types";

export const DEFAULT_WS_URL = "ws://127.0.0.1:47800/ws";
const TOKEN_KEY = "h4rn.token";
const PAIRING_URI_KEY = "h4rn.secure.pairing-uri";

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
  /** A connect deployment to sign in against, from `?connect=`. */
  connectUrl: string | null;
  /** Whether a machine selected through connect is the active transport. */
  remote: boolean;
};

/**
 * The mock daemon (`?mock=1`) exists for `bun run dev`, tests and screenshots.
 * Both operands are build-time constants, so a production build folds this to
 * `false` and drops the mock modules unless it opts in with
 * `VITE_H4RN_MOCK=1`.
 */
export const MOCK_ALLOWED = import.meta.env.DEV || import.meta.env.VITE_H4RN_MOCK === "1";

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
      connectUrl: null,
      remote: false,
    };
  }

  const params = new URLSearchParams(window.location.search);
  const queryTheme = params.get("theme");
  const pairing = params.get("pair") ?? peekSecret(PAIRING_URI_KEY);
  const active = readActiveRemote();
  return {
    mock: MOCK_ALLOWED && params.get("mock") === "1",
    scenario: params.get("scenario") ?? "default",
    // `preparePlatform()` moves a `?token=` bootstrap value into the platform
    // secret store and strips it from the address bar before this runs. The
    // query fallback keeps non-browser callers and tests working.
    token: peekSecret(TOKEN_KEY) ?? params.get("token"),
    daemonUrl: readDaemonUrl(params.get("daemon")),
    theme:
      queryTheme === "light" || queryTheme === "dark" || queryTheme === "system"
        ? queryTheme
        : null,
    // A pairing link or a previously pinned daemon selects Noise. Plain
    // `?daemon=` stays on the loopback WebSocket for backwards compatibility.
    secure:
      Boolean(pairing) || params.get("secure") === "1" || hasStoredSecurity() || active !== null,
    pairing,
    relay: params.get("relay"),
    connectUrl: params.get("connect"),
    remote: active !== null,
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

export function createClientFromEnv(): H4rnClient {
  const config = readBootConfig();

  if (MOCK_ALLOWED && config.mock) {
    const transportFactory: TransportFactory = (context) =>
      createMockTransport(config.scenario, context.attempt);
    const options: ClientOptions = {
      url: config.daemonUrl ?? DEFAULT_WS_URL,
      token: "mock",
      transportLabel: "mock",
      transportFactory,
      requestTimeoutMs: 10_000,
      // The visual regression scenarios need a deterministic stop: `offline`
      // drops the socket and gives up, `error` reconnects once and fails the
      // reload so the partial-failure banner is rendered.
      reconnect:
        config.scenario === "offline"
          ? { baseMs: 100, maxMs: 100, jitter: 0, maxAttempts: 0 }
          : config.scenario === "error"
            ? { baseMs: 120, maxMs: 120, jitter: 0, maxAttempts: 8 }
            : { baseMs: 300, maxMs: 4_000, jitter: 0.2, maxAttempts: 8 },
    };
    return new H4rnClient(options);
  }

  if (config.remote) {
    return new H4rnClient({
      url: config.daemonUrl ?? DEFAULT_WS_URL,
      token: null,
      transportLabel: "encrypted",
      transportFactory: createRemoteTransportFactory(),
      requestTimeoutMs: 10_000,
      reconnect: { baseMs: 400, maxMs: 8_000, jitter: 0.2, maxAttempts: 10 },
    });
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
